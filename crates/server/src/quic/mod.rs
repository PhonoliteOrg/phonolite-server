use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;

use crate::config::{bind_target, resolve_path, ServerConfig};
use crate::state::AppState;
use crate::stream_cache::{CacheGuard, CacheKey};
use crate::stream_sessions::StreamSessionHandle;
use crate::streaming::{
    build_raw_opus_meta, parse_frame_ms, parse_transcode_mode, parse_transcode_quality,
    transcode_mode_label, transcode_quality_label,
};
use crate::transcode::{BitrateSelector, TranscodeCommand, TranscodeMode, TranscodeQuality};

const ALPN_QUIC: &[&[u8]] = &[b"phonolite-quic"];
const SERVER_CONN_ID_LEN: usize = 16;
const MAX_UDP_SIZE: usize = 65535;
const MAX_QUIC_DATAGRAM: usize = 1350;
const CONTROL_STREAM_MAX_LINE: usize = 64 * 1024;
const MAX_STREAM_BUFFER_BYTES: usize = 6 * 1024 * 1024;
const SEEK_RESET_MARKER: u16 = 0xFFFF;
const SEEK_RECOVERY_TARGET_MS: u32 = 400;
const PREFETCH_TRACK_LIMIT: usize = 1;
const STATS_FLUSH_INTERVAL: Duration = Duration::from_secs(5);
const STATS_MAX_PLAYBACK_DELTA_MS: u64 = 15_000;

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ControlMessage {
    #[serde(rename = "auth")]
    Auth { token: String },
    #[serde(rename = "open")]
    Open {
        track_id: String,
        mode: Option<String>,
        quality: Option<String>,
        frame_ms: Option<u32>,
        queue: Option<Vec<String>>,
    },
    #[serde(rename = "advance")]
    Advance,
    #[serde(rename = "buffer")]
    Buffer {
        buffer_ms: u32,
        target_ms: Option<u32>,
    },
    #[serde(rename = "seek")]
    Seek {
        track_id: String,
        position_ms: u32,
        #[serde(default)]
        seek_id: u32,
    },
    #[serde(rename = "playback")]
    Playback {
        track_id: String,
        position_ms: u32,
        playing: Option<bool>,
    },
    #[serde(rename = "ping")]
    Ping { ts: Option<i64> },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum ControlResponse<'a> {
    #[serde(rename = "auth_ok")]
    AuthOk,
    #[serde(rename = "error")]
    Error { message: &'a str },
    #[serde(rename = "pong")]
    Pong { ts: Option<i64> },
    #[serde(rename = "stream")]
    Stream {
        track_id: &'a str,
        stream_id: u64,
        role: &'a str,
        frame_ms: u32,
    },
    #[serde(rename = "open_ok")]
    OpenOk { track_id: &'a str },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamRole {
    Active,
    Prefetch,
}

struct ControlParser {
    buffer: Vec<u8>,
}

impl ControlParser {
    fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    fn push(&mut self, data: &[u8]) -> Vec<ControlMessage> {
        self.buffer.extend_from_slice(data);
        if self.buffer.len() > CONTROL_STREAM_MAX_LINE {
            self.buffer.clear();
            return Vec::new();
        }
        let mut out = Vec::new();
        loop {
            let newline = self.buffer.iter().position(|b| *b == b'\n');
            let Some(pos) = newline else { break };
            let mut line = self.buffer.drain(..=pos).collect::<Vec<u8>>();
            if let Some(b'\n') = line.last() {
                line.pop();
            }
            if line.is_empty() {
                continue;
            }
            if line.len() > CONTROL_STREAM_MAX_LINE {
                self.buffer.clear();
                break;
            }
            if let Ok(text) = std::str::from_utf8(&line) {
                if let Ok(msg) = serde_json::from_str::<ControlMessage>(text) {
                    out.push(msg);
                }
            }
        }
        out
    }
}

struct ControlOutbox {
    pending: VecDeque<Bytes>,
    offset: usize,
}

impl ControlOutbox {
    fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            offset: 0,
        }
    }

    fn enqueue(&mut self, payload: Bytes) {
        self.pending.push_back(payload);
    }
}

struct OutgoingStream {
    stream_id: u64,
    generation: u64,
    track_id: String,
    role: StreamRole,
    frame_ms: u32,
    mode: TranscodeMode,
    quality: TranscodeQuality,
    adaptive_session: Option<StreamSessionHandle>,
    rx: tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>,
    cmd_tx: Option<mpsc::Sender<TranscodeCommand>>,
    cache_guard: Option<CacheGuard>,
    pending: VecDeque<Bytes>,
    offset: usize,
    finished: bool,
    buffered_bytes: usize,
    sent_bytes: u64,
    last_send: Instant,
    last_drain: Instant,
    last_send_log: Instant,
    last_send_err: Option<String>,
    producer_failure: Option<String>,
}

impl OutgoingStream {
    fn new(
        stream_id: u64,
        generation: u64,
        track_id: String,
        role: StreamRole,
        frame_ms: u32,
        mode: TranscodeMode,
        quality: TranscodeQuality,
        adaptive_session: Option<StreamSessionHandle>,
        rx: tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>,
        cmd_tx: Option<mpsc::Sender<TranscodeCommand>>,
        cache_guard: Option<CacheGuard>,
    ) -> Self {
        let now = Instant::now();
        Self {
            stream_id,
            generation,
            track_id,
            role,
            frame_ms,
            mode,
            quality,
            adaptive_session,
            rx,
            cmd_tx,
            cache_guard,
            pending: VecDeque::new(),
            offset: 0,
            finished: false,
            buffered_bytes: 0,
            sent_bytes: 0,
            last_send: now,
            last_drain: now,
            last_send_log: now,
            last_send_err: None,
            producer_failure: None,
        }
    }

    fn stop_worker(&self) {
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.send(TranscodeCommand::Stop);
        }
    }

    fn drain_incoming(&mut self) {
        if self.finished {
            return;
        }
        while self.buffered_bytes < MAX_STREAM_BUFFER_BYTES {
            match self.rx.try_recv() {
                Ok(Ok(bytes)) => {
                    self.buffered_bytes = self.buffered_bytes.saturating_add(bytes.len());
                    self.pending.push_back(bytes);
                    self.last_drain = Instant::now();
                }
                Ok(Err(err)) => {
                    let message = err.to_string();
                    tracing::warn!(
                        "QUIC stream producer failed track={} role={:?} stream_id={} err={}",
                        self.track_id,
                        self.role,
                        self.stream_id,
                        message
                    );
                    self.producer_failure = Some(message);
                    self.finished = true;
                    break;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    self.finished = true;
                    break;
                }
            }
        }
    }
}

struct SessionState {
    authed: bool,
    user_id: Option<String>,
    control_stream: Option<u64>,
    control_outbox: ControlOutbox,
    control_parser: ControlParser,
    next_uni_stream_id: u64,
    stream_generation: u64,
    active_track: Option<String>,
    queue: VecDeque<String>,
    outgoing: HashMap<u64, OutgoingStream>,
    track_streams: HashMap<String, u64>,
    buffer_target_ms: u32,
    client_buffer_ms: u32,
    last_debug: Instant,
    stats_pending_ms: u64,
    stats_pending_plays: u64,
    stats_track_id: Option<String>,
    stats_artist_id: Option<String>,
    stats_genres: Vec<String>,
    stats_track_duration_ms: Option<u64>,
    stats_play_counted: bool,
    stats_last_position_ms: Option<u64>,
}

impl SessionState {
    fn new() -> Self {
        Self {
            authed: false,
            user_id: None,
            control_stream: None,
            control_outbox: ControlOutbox::new(),
            control_parser: ControlParser::new(),
            next_uni_stream_id: 3,
            stream_generation: 0,
            active_track: None,
            queue: VecDeque::new(),
            outgoing: HashMap::new(),
            track_streams: HashMap::new(),
            buffer_target_ms: 8000,
            client_buffer_ms: 0,
            last_debug: Instant::now(),
            stats_pending_ms: 0,
            stats_pending_plays: 0,
            stats_track_id: None,
            stats_artist_id: None,
            stats_genres: Vec::new(),
            stats_track_duration_ms: None,
            stats_play_counted: false,
            stats_last_position_ms: None,
        }
    }

    fn next_server_uni_stream(&mut self) -> u64 {
        let id = self.next_uni_stream_id;
        self.next_uni_stream_id = self.next_uni_stream_id.saturating_add(4);
        id
    }

    fn bump_stream_generation(&mut self) -> u64 {
        self.stream_generation = self.stream_generation.saturating_add(1);
        self.stream_generation
    }
}

struct ClientConn {
    conn: quiche::Connection,
    session: SessionState,
    timeout_at: Option<Instant>,
}

pub async fn run(state: AppState) -> Result<(), String> {
    let config = state.config.read().clone();
    if !config.quic_enabled {
        tracing::info!("QUIC disabled in config.");
        return Ok(());
    }

    let (cert_path, key_path) = ensure_quic_certs(&state, &config)?;
    let bind_addr = resolve_quic_bind_addr(&config)?;

    let mut quic_config = build_quic_config(&cert_path, &key_path)?;
    let socket = UdpSocket::bind(bind_addr)
        .await
        .map_err(|err| format!("quic bind error: {}", err))?;
    let local_addr = socket
        .local_addr()
        .map_err(|err| format!("quic local addr error: {}", err))?;

    tracing::info!("QUIC listening on {}", local_addr);

    let mut connections: HashMap<Vec<u8>, ClientConn> = HashMap::new();
    let mut conn_id_map: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let mut recv_buf = vec![0u8; MAX_UDP_SIZE];
    let mut send_buf = vec![0u8; MAX_UDP_SIZE];
    let mut pending_udp: VecDeque<(Vec<u8>, std::net::SocketAddr)> = VecDeque::new();

    let mut tick = tokio::time::interval(Duration::from_millis(25));

    loop {
        tokio::select! {
            result = socket.recv_from(&mut recv_buf) => {
                let (len, from) = match result {
                    Ok(value) => value,
                    Err(err) => {
                        if err.kind() == std::io::ErrorKind::ConnectionReset
                            || err.raw_os_error() == Some(10054)
                        {
                            tracing::debug!("quic recv reset by peer: {}", err);
                        } else {
                            tracing::error!("quic recv error: {}", err);
                        }
                        continue;
                    }
                };
                let packet = &mut recv_buf[..len];
                let hdr = match quiche::Header::from_slice(packet, SERVER_CONN_ID_LEN) {
                    Ok(hdr) => hdr,
                    Err(err) => {
                        tracing::debug!("quic header parse failed: {:?}", err);
                        continue;
                    }
                };
                let conn_id = hdr.dcid.to_vec();
                let mut lookup_id = conn_id_map
                    .get(&conn_id)
                    .cloned()
                    .unwrap_or_else(|| conn_id.clone());
                if !connections.contains_key(&lookup_id) {
                    if !quiche::version_is_supported(hdr.version) {
                        match quiche::negotiate_version(&hdr.scid, &hdr.dcid, &mut send_buf) {
                            Ok(len) => {
                                let _ = socket.send_to(&send_buf[..len], from).await;
                            }
                            Err(err) => {
                                tracing::debug!("quic version negotiation failed: {:?}", err);
                            }
                        }
                        continue;
                    }
                    let scid = generate_cid();
                    let scid_conn_id = quiche::ConnectionId::from_ref(&scid);
                    let conn = quiche::accept(
                        &scid_conn_id,
                        Some(&hdr.dcid),
                        local_addr,
                        from,
                        &mut quic_config,
                    )
                        .map_err(|err| format!("quic accept error: {:?}", err))?;
                    let timeout_at = conn.timeout().map(|t| Instant::now() + t);
                    let primary_id = scid.to_vec();
                    connections.insert(
                        primary_id.clone(),
                        ClientConn {
                            conn,
                            session: SessionState::new(),
                            timeout_at,
                        },
                    );
                    conn_id_map.insert(primary_id.clone(), primary_id.clone());
                    conn_id_map.insert(conn_id.clone(), primary_id.clone());
                    lookup_id = primary_id;
                }

                let client = match connections.get_mut(&lookup_id) {
                    Some(client) => client,
                    None => continue,
                };

                let recv_info = quiche::RecvInfo {
                    from,
                    to: local_addr,
                };
                if let Err(err) = client.conn.recv(packet, recv_info) {
                    if err != quiche::Error::Done {
                        tracing::debug!("quic recv failed: {:?}", err);
                    }
                    continue;
                }
                refresh_conn_ids(&mut conn_id_map, &mut client.conn, &lookup_id);
                handle_readable(&state, client);
                flush_control(&mut client.session, &mut client.conn);
                flush_streams(&mut client.session, &mut client.conn);

                flush_conn(&mut client.conn, &socket, &mut send_buf, &mut pending_udp);
                client.timeout_at = client.conn.timeout().map(|t| Instant::now() + t);
            }
            _ = tick.tick() => {
                let now = Instant::now();
                let mut closed = Vec::new();
                for (id, client) in connections.iter_mut() {
                    if let Some(deadline) = client.timeout_at {
                        if now >= deadline {
                            client.conn.on_timeout();
                        }
                    }
                    client.timeout_at = client.conn.timeout().map(|t| Instant::now() + t);
                    if client.conn.is_closed() {
                        if let Some(err) = client.conn.peer_error() {
                            tracing::warn!(
                                "QUIC closed by peer: code={} app={} reason={}",
                                err.error_code,
                                err.is_app,
                                String::from_utf8_lossy(&err.reason),
                            );
                        }
                        if let Some(err) = client.conn.local_error() {
                            tracing::warn!(
                                "QUIC closed locally: code={} app={} reason={}",
                                err.error_code,
                                err.is_app,
                                String::from_utf8_lossy(&err.reason),
                            );
                        }
                        if client.conn.is_timed_out() {
                            tracing::warn!("QUIC closed: idle timeout");
                        }
                        finalize_listen_stats(&state, &mut client.session);
                        closed.push(id.clone());
                        continue;
                    }
                    refresh_conn_ids(&mut conn_id_map, &mut client.conn, id);
                    handle_readable(&state, client);
                    flush_control(&mut client.session, &mut client.conn);
                    flush_streams(&mut client.session, &mut client.conn);
                    flush_conn(&mut client.conn, &socket, &mut send_buf, &mut pending_udp);
                    client.timeout_at = client.conn.timeout().map(|t| Instant::now() + t);
                    maybe_log_streams(&mut client.session, &client.conn);
                }
                for id in closed {
                    if let Some(client) = connections.get(&id) {
                        for scid in client.conn.source_ids() {
                            conn_id_map.remove(scid.as_ref());
                        }
                    }
                    connections.remove(&id);
                    conn_id_map.remove(&id);
                }
            }
        }
    }
}

fn refresh_conn_ids(
    conn_id_map: &mut HashMap<Vec<u8>, Vec<u8>>,
    conn: &mut quiche::Connection,
    primary_id: &Vec<u8>,
) {
    for scid in conn.source_ids() {
        conn_id_map
            .entry(scid.as_ref().to_vec())
            .or_insert_with(|| primary_id.clone());
    }
    while let Some(retired) = conn.retired_scid_next() {
        conn_id_map.remove(retired.as_ref());
    }
}

fn resolve_quic_bind_addr(config: &ServerConfig) -> Result<String, String> {
    Ok(bind_target(config.bind_addr.as_deref(), config.quic_port))
}

fn ensure_quic_certs(
    state: &AppState,
    config: &ServerConfig,
) -> Result<(PathBuf, PathBuf), String> {
    let cert_path = resolve_path(&state.config_path, &config.quic_cert_path);
    let key_path = resolve_path(&state.config_path, &config.quic_key_path);

    if cert_path.exists() && key_path.exists() {
        return Ok((cert_path, key_path));
    }
    if !config.quic_self_signed {
        return Err("missing QUIC cert/key and self-signed disabled".to_string());
    }

    let subject_alt_names = vec![
        "localhost".to_string(),
        "phonolite".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    let cert_key = rcgen::generate_simple_self_signed(subject_alt_names)
        .map_err(|err| format!("cert generation error: {}", err))?;
    let cert_pem = cert_key.cert.pem();
    let key_pem = cert_key.key_pair.serialize_pem();

    if let Some(parent) = cert_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|err| format!("cert dir error: {}", err))?;
        }
    }
    if let Some(parent) = key_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|err| format!("key dir error: {}", err))?;
        }
    }
    std::fs::write(&cert_path, cert_pem).map_err(|err| format!("cert write error: {}", err))?;
    std::fs::write(&key_path, key_pem).map_err(|err| format!("key write error: {}", err))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&key_path, permissions)
            .map_err(|err| format!("key permission error: {}", err))?;
    }

    Ok((cert_path, key_path))
}

fn build_quic_config(cert_path: &PathBuf, key_path: &PathBuf) -> Result<quiche::Config, String> {
    let mut config =
        quiche::Config::new(quiche::PROTOCOL_VERSION).map_err(|e| format!("{:?}", e))?;
    config
        .set_application_protos(ALPN_QUIC)
        .map_err(|err| format!("alpn error: {:?}", err))?;
    config
        .load_cert_chain_from_pem_file(
            cert_path
                .to_str()
                .ok_or_else(|| "invalid cert path".to_string())?,
        )
        .map_err(|err| format!("cert load error: {:?}", err))?;
    config
        .load_priv_key_from_pem_file(
            key_path
                .to_str()
                .ok_or_else(|| "invalid key path".to_string())?,
        )
        .map_err(|err| format!("key load error: {:?}", err))?;
    config.verify_peer(false);
    config.set_max_idle_timeout(90_000);
    config.set_max_recv_udp_payload_size(MAX_QUIC_DATAGRAM);
    config.set_max_send_udp_payload_size(MAX_QUIC_DATAGRAM);
    config.set_initial_max_data(20_000_000);
    config.set_initial_max_stream_data_bidi_local(10_000_000);
    config.set_initial_max_stream_data_bidi_remote(10_000_000);
    config.set_initial_max_stream_data_uni(10_000_000);
    config.set_initial_max_streams_bidi(16);
    config.set_initial_max_streams_uni(32);
    config.set_disable_active_migration(true);
    Ok(config)
}

fn generate_cid() -> Vec<u8> {
    let mut scid = [0u8; 16];
    rand::rng().fill_bytes(&mut scid);
    scid.to_vec()
}

fn handle_readable(state: &AppState, client: &mut ClientConn) {
    let mut buf = [0u8; 65535];
    let readable: Vec<u64> = client.conn.readable().collect();
    for stream_id in readable {
        loop {
            match client.conn.stream_recv(stream_id, &mut buf) {
                Ok((len, _fin)) => {
                    if client.session.control_stream.is_none() && is_bidi_stream(stream_id) {
                        client.session.control_stream = Some(stream_id);
                    }
                    if client.session.control_stream == Some(stream_id) {
                        let messages = client.session.control_parser.push(&buf[..len]);
                        for msg in messages {
                            handle_control_message(state, client, msg);
                        }
                    }
                }
                Err(quiche::Error::Done) => break,
                Err(err) => {
                    tracing::debug!("stream recv error: {:?}", err);
                    break;
                }
            }
        }
    }
}

fn handle_control_message(state: &AppState, client: &mut ClientConn, msg: ControlMessage) {
    match msg {
        ControlMessage::Auth { token } => {
            tracing::info!("QUIC auth attempt");
            if !state.auth.has_any_user().unwrap_or(false) {
                tracing::warn!("QUIC auth failed: server not initialized");
                send_control(
                    client,
                    ControlResponse::Error {
                        message: "server not initialized",
                    },
                );
                return;
            }
            match state.auth.user_from_token(&token) {
                Ok(Some(user)) => {
                    client.session.authed = true;
                    client.session.user_id = Some(user.id);
                    tracing::info!("QUIC auth ok");
                    if let Some(tp) = client.conn.peer_transport_params() {
                        tracing::info!(
                            "QUIC peer transport params: max_idle_timeout={} max_udp_payload_size={} initial_max_data={} initial_max_stream_data_uni={} initial_max_stream_data_bidi_local={} initial_max_stream_data_bidi_remote={} initial_max_streams_uni={} initial_max_streams_bidi={}",
                            tp.max_idle_timeout,
                            tp.max_udp_payload_size,
                            tp.initial_max_data,
                            tp.initial_max_stream_data_uni,
                            tp.initial_max_stream_data_bidi_local,
                            tp.initial_max_stream_data_bidi_remote,
                            tp.initial_max_streams_uni,
                            tp.initial_max_streams_bidi,
                        );
                    } else {
                        tracing::info!("QUIC peer transport params not available yet");
                    }
                    send_control(client, ControlResponse::AuthOk);
                }
                Ok(None) => {
                    tracing::warn!("QUIC auth failed: unauthorized");
                    send_control(
                        client,
                        ControlResponse::Error {
                            message: "unauthorized",
                        },
                    );
                }
                Err(_) => {
                    tracing::warn!("QUIC auth failed: auth error");
                    send_control(
                        client,
                        ControlResponse::Error {
                            message: "auth error",
                        },
                    );
                }
            }
        }
        ControlMessage::Open {
            track_id,
            mode,
            quality,
            frame_ms,
            queue,
        } => {
            tracing::info!(
                "QUIC open track={} mode={:?} quality={:?} frame_ms={:?}",
                track_id,
                mode,
                quality,
                frame_ms
            );
            if !client.session.authed {
                tracing::warn!("QUIC open rejected: unauthorized");
                send_control(
                    client,
                    ControlResponse::Error {
                        message: "unauthorized",
                    },
                );
                return;
            }
            let generation = client.session.bump_stream_generation();
            client.session.active_track = Some(track_id.clone());
            if let Some(queue) = queue {
                client.session.queue = queue.into();
            } else if client.session.queue.is_empty() {
                client.session.queue.push_back(track_id.clone());
            }
            ensure_active_in_queue(&mut client.session);
            promote_existing_stream(
                &mut client.session,
                &track_id,
                StreamRole::Active,
                generation,
            );
            prune_streams(&mut client.session, &mut client.conn);
            let frame_ms = frame_ms.unwrap_or(20);
            if !client.session.track_streams.contains_key(&track_id) {
                if let Err(err) = start_track_stream(
                    state,
                    client,
                    track_id.clone(),
                    StreamRole::Active,
                    frame_ms,
                    0,
                    mode.as_deref(),
                    quality.as_deref(),
                ) {
                    tracing::warn!("QUIC open failed: {}", err);
                    send_control(client, ControlResponse::Error { message: &err });
                    return;
                }
            }
            send_control(
                client,
                ControlResponse::OpenOk {
                    track_id: &track_id,
                },
            );
            prebuffer_next_tracks(state, client, mode.as_deref(), quality.as_deref(), frame_ms);
        }
        ControlMessage::Advance => {
            if let Some(next) = next_track_in_queue(&client.session) {
                let generation = client.session.bump_stream_generation();
                let frame_ms = active_frame_ms(&client.session);
                client.session.active_track = Some(next.clone());
                promote_existing_stream(&mut client.session, &next, StreamRole::Active, generation);
                prune_streams(&mut client.session, &mut client.conn);
                if !client.session.track_streams.contains_key(&next) {
                    let _ = start_track_stream(
                        state,
                        client,
                        next,
                        StreamRole::Active,
                        frame_ms,
                        0,
                        None,
                        None,
                    );
                }
                prebuffer_next_tracks(state, client, None, None, frame_ms);
            }
        }
        ControlMessage::Buffer {
            buffer_ms,
            target_ms,
        } => {
            client.session.client_buffer_ms = buffer_ms;
            if let Some(target) = target_ms {
                client.session.buffer_target_ms = target;
            }
            report_active_buffer(state, &client.session);
            prune_streams(&mut client.session, &mut client.conn);
            let frame_ms = active_frame_ms(&client.session);
            prebuffer_next_tracks(state, client, None, None, frame_ms);
        }
        ControlMessage::Seek {
            track_id,
            position_ms,
            seek_id,
        } => {
            tracing::info!(
                "QUIC seek track={} position_ms={} seek_id={}",
                track_id,
                position_ms,
                seek_id
            );
            if !client.session.authed {
                tracing::warn!("QUIC seek rejected: unauthorized");
                send_control(
                    client,
                    ControlResponse::Error {
                        message: "unauthorized",
                    },
                );
                return;
            }
            let generation = client.session.bump_stream_generation();
            client.session.client_buffer_ms = 0;
            client.session.buffer_target_ms = SEEK_RECOVERY_TARGET_MS;
            client.session.active_track = Some(track_id.clone());
            ensure_active_in_queue(&mut client.session);
            let mut frame_ms = active_frame_ms(&client.session);
            let mut mode_label: Option<&str> = None;
            let mut quality_label: Option<&str> = None;
            let mut reused = false;
            if let Some(stream_id) = client.session.track_streams.get(&track_id).cloned() {
                if let Some(outgoing) = client.session.outgoing.get_mut(&stream_id) {
                    frame_ms = outgoing.frame_ms;
                    mode_label = Some(transcode_mode_label(outgoing.mode));
                    quality_label = Some(transcode_quality_label(outgoing.quality));
                    outgoing.role = StreamRole::Active;
                    match seek_track_stream(
                        state,
                        outgoing,
                        generation,
                        &track_id,
                        position_ms,
                        seek_id,
                    ) {
                        Ok(()) => {
                            tracing::info!(
                                "QUIC seek reusing stream track={} stream_id={} seek_id={}",
                                track_id,
                                stream_id,
                                seek_id
                            );
                            reused = true;
                        }
                        Err(err) => {
                            tracing::warn!("QUIC seek reuse failed: {}", err);
                            send_control(client, ControlResponse::Error { message: &err });
                            return;
                        }
                    }
                } else {
                    client.session.track_streams.remove(&track_id);
                }
            }
            if !reused {
                if let Err(err) = start_track_stream(
                    state,
                    client,
                    track_id,
                    StreamRole::Active,
                    frame_ms,
                    position_ms,
                    mode_label,
                    quality_label,
                ) {
                    tracing::warn!("QUIC seek failed: {}", err);
                    send_control(client, ControlResponse::Error { message: &err });
                }
            }
            prune_streams(&mut client.session, &mut client.conn);
        }
        ControlMessage::Playback {
            track_id,
            position_ms,
            playing,
        } => {
            if !client.session.authed {
                return;
            }
            update_listen_stats_from_playback(
                state,
                &mut client.session,
                &track_id,
                position_ms,
                playing.unwrap_or(true),
            );
        }
        ControlMessage::Ping { ts } => {
            send_control(client, ControlResponse::Pong { ts });
        }
    }
}

fn next_track_in_queue(session: &SessionState) -> Option<String> {
    let mut iter = session.queue.iter();
    let active = session.active_track.as_ref()?;
    while let Some(track) = iter.next() {
        if track == active {
            return iter.next().cloned();
        }
    }
    session.queue.front().cloned()
}

fn active_frame_ms(session: &SessionState) -> u32 {
    let Some(track_id) = session.active_track.as_ref() else {
        return 20;
    };
    let Some(stream_id) = session.track_streams.get(track_id) else {
        return 20;
    };
    session
        .outgoing
        .get(stream_id)
        .map(|outgoing| outgoing.frame_ms)
        .unwrap_or(20)
}

fn ensure_active_in_queue(session: &mut SessionState) {
    let Some(active) = session.active_track.as_ref() else {
        return;
    };
    if session.queue.iter().any(|id| id == active) {
        return;
    }
    session.queue.push_front(active.clone());
}

fn promote_existing_stream(
    session: &mut SessionState,
    track_id: &str,
    role: StreamRole,
    generation: u64,
) {
    if let Some(stream_id) = session.track_streams.get(track_id).cloned() {
        if let Some(outgoing) = session.outgoing.get_mut(&stream_id) {
            outgoing.role = role;
            outgoing.generation = generation;
        }
    }
}

fn allowed_prefetch_tracks(session: &SessionState) -> Vec<String> {
    let active = match session.active_track.as_ref() {
        Some(value) => value,
        None => return Vec::new(),
    };
    let mut remaining = Vec::new();
    let mut seen_active = false;
    for id in session.queue.iter() {
        if !seen_active {
            if id == active {
                seen_active = true;
            }
            continue;
        }
        remaining.push(id.clone());
        if remaining.len() >= PREFETCH_TRACK_LIMIT {
            break;
        }
    }
    remaining
}

fn should_prefetch(session: &SessionState) -> bool {
    session.buffer_target_ms > 0 && session.client_buffer_ms >= session.buffer_target_ms
}

fn report_active_buffer(state: &AppState, session: &SessionState) {
    let Some(active) = session.active_track.as_ref() else {
        return;
    };
    let Some(stream_id) = session.track_streams.get(active) else {
        return;
    };
    let Some(outgoing) = session.outgoing.get(stream_id) else {
        return;
    };
    let Some(adaptive_session) = outgoing.adaptive_session.as_ref() else {
        return;
    };
    state
        .stream_sessions
        .report_buffer(adaptive_session.id, session.client_buffer_ms as u64);
}

fn prune_streams(session: &mut SessionState, conn: &mut quiche::Connection) {
    let mut allowed: HashSet<String> = HashSet::new();
    if let Some(active) = session.active_track.as_ref() {
        allowed.insert(active.clone());
    }
    if should_prefetch(session) {
        for id in allowed_prefetch_tracks(session) {
            allowed.insert(id.clone());
        }
    }
    let mut remove_ids = Vec::new();
    for (stream_id, outgoing) in session.outgoing.iter() {
        if outgoing.generation != session.stream_generation || !allowed.contains(&outgoing.track_id)
        {
            remove_ids.push((*stream_id, outgoing.track_id.clone()));
        }
    }
    for (stream_id, track_id) in remove_ids {
        let _ = conn.stream_shutdown(stream_id, quiche::Shutdown::Write, 0);
        if let Some(outgoing) = session.outgoing.remove(&stream_id) {
            outgoing.stop_worker();
        }
        session.track_streams.remove(&track_id);
    }
}

fn prebuffer_next_tracks(
    state: &AppState,
    client: &mut ClientConn,
    mode: Option<&str>,
    quality: Option<&str>,
    frame_ms: u32,
) {
    if !should_prefetch(&client.session) {
        return;
    }
    for track_id in allowed_prefetch_tracks(&client.session) {
        let _ = start_track_stream(
            state,
            client,
            track_id,
            StreamRole::Prefetch,
            frame_ms,
            0,
            mode,
            quality,
        );
    }
}

fn spawn_track_transcode(
    state: &AppState,
    track_id: &str,
    frame_ms: u32,
    mode: TranscodeMode,
    quality: TranscodeQuality,
    start_ms: u32,
) -> Result<
    (
        tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>,
        Option<mpsc::Sender<TranscodeCommand>>,
        Option<CacheGuard>,
        Option<StreamSessionHandle>,
    ),
    String,
> {
    let library_guard = state.library_state.read();
    let library = library_guard
        .library
        .clone()
        .ok_or_else(|| "library not ready".to_string())?;
    let track = library
        .get_track(track_id)
        .map_err(|err| format!("library error: {}", err))?
        .ok_or_else(|| "track not found".to_string())?;
    let path = library
        .resolve_relpath(&track.file_relpath)
        .ok_or_else(|| "music root not configured".to_string())?;
    if !path.exists() {
        tracing::warn!("QUIC track file missing: {}", path.display());
        return Err("file not found".to_string());
    }

    let fixed_bitrate_bps = None;
    let session = if mode == TranscodeMode::Auto {
        Some(state.stream_sessions.create_session(quality))
    } else {
        None
    };
    let selector = BitrateSelector {
        mode,
        quality,
        fixed_bitrate_bps,
        adaptive_bitrate_bps: session
            .as_ref()
            .map(|s| std::sync::Arc::clone(&s.target_bitrate_bps)),
    };

    let meta = build_raw_opus_meta(&library, &track);
    let start_ms = start_ms.min(meta.duration_ms);
    let cacheable = mode == TranscodeMode::Fixed;
    let cache_key = CacheKey::new(track_id, frame_ms, mode, quality);
    let mut cache_guard = None;
    let mut cache_writer = None;
    if cacheable {
        if let Ok(Some(reader)) = state.stream_cache.reader(&cache_key) {
            if reader.is_complete() && reader.can_start(start_ms) {
                let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(64);
                tokio::task::spawn_blocking(move || {
                    if let Err(err) = reader.stream_to(start_ms, &tx) {
                        let _ = tx.blocking_send(Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            err,
                        )));
                    }
                });
                return Ok((rx, None, cache_guard, None));
            }
        }
        if let Ok(Some(writer)) = state.stream_cache.writer(cache_key.clone()) {
            cache_guard = Some(writer.guard());
            cache_writer = Some(writer);
        }
    }
    let (rx, cmd_tx) = crate::transcode::spawn_raw_opus_worker(
        path.clone(),
        selector,
        frame_ms,
        meta,
        start_ms,
        cache_writer,
    )?;
    Ok((rx, Some(cmd_tx), cache_guard, session))
}

fn start_track_stream(
    state: &AppState,
    client: &mut ClientConn,
    track_id: String,
    role: StreamRole,
    frame_ms: u32,
    start_ms: u32,
    mode: Option<&str>,
    quality: Option<&str>,
) -> Result<(), String> {
    tracing::info!(
        "QUIC start stream track={} role={:?} frame_ms={} mode={:?} quality={:?}",
        track_id,
        role,
        frame_ms,
        mode,
        quality
    );
    if client.session.track_streams.contains_key(&track_id) {
        return Ok(());
    }

    let mode = parse_transcode_mode(mode).unwrap_or(TranscodeMode::Auto);
    let quality = parse_transcode_quality(quality).unwrap_or(TranscodeQuality::High);
    let frame_ms = parse_frame_ms(Some(frame_ms)).unwrap_or(20);
    let generation = client.session.stream_generation;
    let (rx, cmd_tx, cache_guard, adaptive_session) =
        spawn_track_transcode(state, &track_id, frame_ms, mode, quality, start_ms)?;

    let stream_id = client.session.next_server_uni_stream();
    client
        .session
        .track_streams
        .insert(track_id.clone(), stream_id);
    client.session.outgoing.insert(
        stream_id,
        OutgoingStream::new(
            stream_id,
            generation,
            track_id.clone(),
            role,
            frame_ms,
            mode,
            quality,
            adaptive_session,
            rx,
            cmd_tx,
            cache_guard,
        ),
    );

    let role_label = match role {
        StreamRole::Active => "active",
        StreamRole::Prefetch => "prefetch",
    };
    send_control(
        client,
        ControlResponse::Stream {
            track_id: &track_id,
            stream_id,
            role: role_label,
            frame_ms,
        },
    );

    Ok(())
}

fn seek_track_stream(
    state: &AppState,
    outgoing: &mut OutgoingStream,
    generation: u64,
    track_id: &str,
    position_ms: u32,
    seek_id: u32,
) -> Result<(), String> {
    outgoing.generation = generation;
    let frame_ms = outgoing.frame_ms;
    let mode = outgoing.mode;
    let quality = outgoing.quality;
    if mode == TranscodeMode::Fixed {
        let cache_key = CacheKey::new(track_id, frame_ms, mode, quality);
        if let Ok(Some(reader)) = state.stream_cache.reader(&cache_key) {
            // Reusing a partial cache for seek can strand the stream after the
            // already-cached frames are drained, so only seek through complete entries.
            if reader.can_start(position_ms) && reader.is_complete() {
                let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(64);
                tokio::task::spawn_blocking(move || {
                    if let Err(err) = reader.stream_to(position_ms, &tx) {
                        let _ = tx.blocking_send(Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            err,
                        )));
                    }
                });
                if outgoing.cmd_tx.is_some() {
                    outgoing.stop_worker();
                }
                outgoing.cmd_tx = None;
                outgoing.cache_guard = None;
                outgoing.adaptive_session = None;
                outgoing.rx = rx;
                reset_outgoing_seek_state(outgoing);

                let marker = seek_reset_frame(seek_id);
                outgoing.buffered_bytes = outgoing.buffered_bytes.saturating_add(marker.len());
                outgoing.pending.push_back(marker);
                return Ok(());
            }
        }
    }
    if outgoing.cmd_tx.is_some() {
        outgoing.stop_worker();
    }
    outgoing.cmd_tx = None;
    let (rx, cmd_tx, cache_guard, adaptive_session) =
        spawn_track_transcode(state, track_id, frame_ms, mode, quality, position_ms)?;
    outgoing.rx = rx;
    outgoing.cmd_tx = cmd_tx;
    outgoing.cache_guard = cache_guard;
    outgoing.adaptive_session = adaptive_session;
    reset_outgoing_seek_state(outgoing);

    let marker = seek_reset_frame(seek_id);
    outgoing.buffered_bytes = outgoing.buffered_bytes.saturating_add(marker.len());
    outgoing.pending.push_back(marker);

    Ok(())
}

fn seek_reset_frame(seek_id: u32) -> Bytes {
    let mut marker = Vec::with_capacity(6);
    marker.extend_from_slice(&SEEK_RESET_MARKER.to_le_bytes());
    marker.extend_from_slice(&seek_id.to_le_bytes());
    Bytes::from(marker)
}

fn reset_outgoing_seek_state(outgoing: &mut OutgoingStream) {
    outgoing.pending.clear();
    outgoing.offset = 0;
    outgoing.finished = false;
    outgoing.buffered_bytes = 0;
    outgoing.sent_bytes = 0;
    let now = Instant::now();
    outgoing.last_send = now;
    outgoing.last_drain = now;
    outgoing.last_send_log = now;
    outgoing.last_send_err = None;
    outgoing.producer_failure = None;
}

fn enqueue_control(outbox: &mut ControlOutbox, message: ControlResponse<'_>) {
    let payload = match serde_json::to_string(&message) {
        Ok(value) => value,
        Err(_) => return,
    };
    let mut line = payload;
    line.push('\n');
    outbox.enqueue(Bytes::from(line));
}

fn send_control(client: &mut ClientConn, message: ControlResponse<'_>) {
    enqueue_control(&mut client.session.control_outbox, message);
}

fn flush_control(session: &mut SessionState, conn: &mut quiche::Connection) {
    let stream_id = match session.control_stream {
        Some(value) => value,
        None => return,
    };
    loop {
        let Some(front) = session.control_outbox.pending.front() else {
            break;
        };
        let data = &front[session.control_outbox.offset..];
        match conn.stream_send(stream_id, data, false) {
            Ok(sent) => {
                if sent == data.len() {
                    session.control_outbox.pending.pop_front();
                    session.control_outbox.offset = 0;
                } else {
                    session.control_outbox.offset =
                        session.control_outbox.offset.saturating_add(sent);
                    break;
                }
            }
            Err(quiche::Error::Done) => break,
            Err(_) => {
                session.control_outbox.pending.pop_front();
                session.control_outbox.offset = 0;
            }
        }
    }
}

fn flush_streams(session: &mut SessionState, conn: &mut quiche::Connection) {
    let mut finished = Vec::new();
    let mut active_failures = Vec::new();
    let mut active_ids = Vec::new();
    let mut prefetch_ids = Vec::new();
    let prefetch_allowed = should_prefetch(session);
    let active_track = session.active_track.clone();
    for (stream_id, outgoing) in session.outgoing.iter() {
        if outgoing.role == StreamRole::Active {
            active_ids.push(*stream_id);
        } else {
            prefetch_ids.push(*stream_id);
        }
    }
    for stream_id in active_ids.into_iter().chain(prefetch_ids.into_iter()) {
        let Some(outgoing) = session.outgoing.get_mut(&stream_id) else {
            continue;
        };
        let force_send = outgoing
            .pending
            .front()
            .map(|front| front.len() >= 2 && front[0] == 0xFF && front[1] == 0xFF)
            .unwrap_or(false);
        if !force_send && outgoing.role == StreamRole::Prefetch && !prefetch_allowed {
            continue;
        }
        if !force_send
            && outgoing.role == StreamRole::Active
            && session.buffer_target_ms > 0
            && session.client_buffer_ms >= session.buffer_target_ms
        {
            continue;
        }
        outgoing.drain_incoming();
        if let Some(failure) = outgoing.producer_failure.take() {
            if outgoing.role == StreamRole::Active
                && active_track.as_deref() == Some(outgoing.track_id.as_str())
            {
                active_failures.push((outgoing.track_id.clone(), failure));
            }
        }
        loop {
            let (send_result, data_len) = match outgoing.pending.front() {
                Some(front) => {
                    let data = &front[outgoing.offset..];
                    (conn.stream_send(stream_id, data, false), data.len())
                }
                None => break,
            };
            match send_result {
                Ok(sent) => {
                    if sent > 0 {
                        outgoing.sent_bytes = outgoing.sent_bytes.saturating_add(sent as u64);
                        outgoing.buffered_bytes = outgoing.buffered_bytes.saturating_sub(sent);
                        outgoing.last_send = Instant::now();
                    }
                    if sent == data_len {
                        outgoing.pending.pop_front();
                        outgoing.offset = 0;
                    } else {
                        outgoing.offset = outgoing.offset.saturating_add(sent);
                        break;
                    }
                }
                Err(quiche::Error::Done) => break,
                Err(err) => {
                    let now = Instant::now();
                    if now.duration_since(outgoing.last_send_log) >= Duration::from_secs(5) {
                        let cap = conn.stream_capacity(stream_id).ok();
                        let err_text = format!("{:?}", err);
                        if outgoing.last_send_err.as_deref() != Some(err_text.as_str()) {
                            tracing::warn!(
                                "QUIC stream send error track={} role={:?} stream_id={} err={} capacity={:?} established={} stats_sent={} stats_recv={}",
                                outgoing.track_id,
                                outgoing.role,
                                stream_id,
                                err_text,
                                cap,
                                conn.is_established(),
                                conn.stats().sent_bytes,
                                conn.stats().recv_bytes,
                            );
                            outgoing.last_send_err = Some(err_text);
                        }
                        outgoing.last_send_log = now;
                    }
                    outgoing.buffered_bytes = outgoing.buffered_bytes.saturating_sub(data_len);
                    outgoing.pending.pop_front();
                    outgoing.offset = 0;
                }
            }
        }
        if outgoing.finished && outgoing.pending.is_empty() {
            let keep_open = outgoing.role == StreamRole::Active
                && session
                    .active_track
                    .as_deref()
                    .map(|id| id == outgoing.track_id)
                    .unwrap_or(false);
            if keep_open {
                // Keep the current active stream open so seeks reuse it.
                continue;
            }
            outgoing.stop_worker();
            let _ = conn.stream_shutdown(stream_id, quiche::Shutdown::Write, 0);
            finished.push((stream_id, outgoing.track_id.clone()));
        }
    }
    for (stream_id, track_id) in finished {
        session.outgoing.remove(&stream_id);
        session.track_streams.remove(&track_id);
    }
    for (track_id, failure) in active_failures {
        let message = format!("stream failed for {}: {}", track_id, failure);
        enqueue_control(
            &mut session.control_outbox,
            ControlResponse::Error { message: &message },
        );
    }
}

fn maybe_log_streams(session: &mut SessionState, conn: &quiche::Connection) {
    let now = Instant::now();
    if now.duration_since(session.last_debug) < Duration::from_secs(5) {
        return;
    }
    session.last_debug = now;
    let path = conn.path_stats().next();
    for outgoing in session.outgoing.values() {
        let capacity = conn.stream_capacity(outgoing.stream_id).ok();
        let paused = outgoing.role == StreamRole::Active
            && session.buffer_target_ms > 0
            && session.client_buffer_ms >= session.buffer_target_ms;
        tracing::info!(
            "QUIC stream debug track={} role={:?} pending_chunks={} buffered_bytes={} finished={} sent_bytes={} since_last_send_ms={} since_last_drain_ms={} client_buffer_ms={} target_ms={} paused={} capacity={:?} established={} stats_sent={} stats_recv={} path={:?}",
            outgoing.track_id,
            outgoing.role,
            outgoing.pending.len(),
            outgoing.buffered_bytes,
            outgoing.finished,
            outgoing.sent_bytes,
            now.duration_since(outgoing.last_send).as_millis(),
            now.duration_since(outgoing.last_drain).as_millis(),
            session.client_buffer_ms,
            session.buffer_target_ms,
            paused,
            capacity,
            conn.is_established(),
            conn.stats().sent_bytes,
            conn.stats().recv_bytes,
            path,
        );
    }
}

fn finalize_listen_stats(state: &AppState, session: &mut SessionState) {
    if !state.config.read().stats_collection_enabled {
        return;
    }
    let user_id = match session.user_id.clone() {
        Some(value) => value,
        None => return,
    };
    flush_pending_stats(state, session, &user_id);
}

fn flush_pending_stats(state: &AppState, session: &mut SessionState, user_id: &str) {
    if session.stats_pending_ms == 0 && session.stats_pending_plays == 0 {
        return;
    }
    let Some(track_id) = session.stats_track_id.as_deref() else {
        session.stats_pending_ms = 0;
        session.stats_pending_plays = 0;
        return;
    };
    let Some(artist_id) = session.stats_artist_id.as_deref() else {
        session.stats_pending_ms = 0;
        session.stats_pending_plays = 0;
        return;
    };
    if let Err(err) = state.stats.record_listen(
        user_id,
        track_id,
        artist_id,
        &session.stats_genres,
        session.stats_pending_ms,
        session.stats_pending_plays,
    ) {
        tracing::warn!("QUIC stats record failed: {}", err);
    }
    session.stats_pending_ms = 0;
    session.stats_pending_plays = 0;
}

fn clear_stats_track(session: &mut SessionState) {
    session.stats_track_id = None;
    session.stats_artist_id = None;
    session.stats_genres.clear();
    session.stats_track_duration_ms = None;
    session.stats_play_counted = false;
    session.stats_last_position_ms = None;
}

fn load_stats_metadata(state: &AppState, session: &mut SessionState, track_id: &str) -> bool {
    let library_guard = state.library_state.read();
    let Some(library) = library_guard.library.clone() else {
        return false;
    };
    let track = match library.get_track(track_id) {
        Ok(Some(value)) => value,
        Ok(None) => return false,
        Err(err) => {
            tracing::warn!("QUIC stats track lookup failed: {}", err);
            return false;
        }
    };
    session.stats_track_id = Some(track.id);
    session.stats_artist_id = Some(track.artist_id);
    session.stats_genres = track.genres;
    session.stats_track_duration_ms = Some(track.duration_ms as u64);
    session.stats_play_counted = false;
    session.stats_last_position_ms = None;
    true
}

fn update_listen_stats_from_playback(
    state: &AppState,
    session: &mut SessionState,
    track_id: &str,
    position_ms: u32,
    playing: bool,
) {
    if !state.config.read().stats_collection_enabled {
        session.stats_pending_ms = 0;
        clear_stats_track(session);
        return;
    }

    let user_id = match session.user_id.clone() {
        Some(value) => value,
        None => return,
    };

    if session.stats_track_id.as_deref() != Some(track_id) {
        flush_pending_stats(state, session, &user_id);
        if !load_stats_metadata(state, session, track_id) {
            return;
        }
    }

    let position_ms = position_ms as u64;
    if let Some(last_pos) = session.stats_last_position_ms {
        if playing {
            let delta = position_ms.saturating_sub(last_pos);
            if delta > 0 && delta <= STATS_MAX_PLAYBACK_DELTA_MS {
                session.stats_pending_ms = session.stats_pending_ms.saturating_add(delta);
            }
        }
    }
    session.stats_last_position_ms = Some(position_ms);

    if playing && !session.stats_play_counted {
        if let Some(duration_ms) = session.stats_track_duration_ms {
            if duration_ms > 0 && position_ms >= duration_ms / 2 {
                session.stats_pending_plays = session.stats_pending_plays.saturating_add(1);
                session.stats_play_counted = true;
            }
        }
    }

    if !playing {
        flush_pending_stats(state, session, &user_id);
        return;
    }

    if session.stats_pending_ms >= STATS_FLUSH_INTERVAL.as_millis() as u64 {
        flush_pending_stats(state, session, &user_id);
    }
}

fn flush_udp_queue(socket: &UdpSocket, pending: &mut VecDeque<(Vec<u8>, std::net::SocketAddr)>) {
    while let Some((packet, addr)) = pending.pop_front() {
        match socket.try_send_to(&packet, addr) {
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                pending.push_front((packet, addr));
                tracing::debug!("quic send stalled with {} queued datagrams", pending.len());
                break;
            }
            Err(err) => {
                tracing::debug!("quic pending send error: {:?}", err);
            }
        }
    }
}

fn flush_conn(
    conn: &mut quiche::Connection,
    socket: &UdpSocket,
    out: &mut [u8],
    pending: &mut VecDeque<(Vec<u8>, std::net::SocketAddr)>,
) {
    flush_udp_queue(socket, pending);
    if !pending.is_empty() {
        return;
    }

    loop {
        match conn.send(out) {
            Ok((len, send_info)) => match socket.try_send_to(&out[..len], send_info.to) {
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    pending.push_back((out[..len].to_vec(), send_info.to));
                    tracing::debug!("quic send backpressure queued {} datagrams", pending.len());
                    break;
                }
                Err(err) => {
                    tracing::debug!("quic send error: {:?}", err);
                    break;
                }
            },
            Err(quiche::Error::Done) => break,
            Err(err) => {
                tracing::debug!("quic send error: {:?}", err);
                break;
            }
        }
    }
}

fn is_bidi_stream(stream_id: u64) -> bool {
    stream_id % 4 == 0 || stream_id % 4 == 1
}
