use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;

use crate::config::{bind_target, ServerConfig};
use crate::performance::{self, OperationTiming, StreamProfile};
use crate::state::AppState;
use crate::stream_cache::{CacheGuard, CacheKey, CACHE_RANGE_MS, MEMORY_CACHE_MAX_BYTES};
use crate::streaming::{
    build_raw_opus_meta, parse_frame_ms, parse_transcode_mode, parse_transcode_quality,
    transcode_mode_label, transcode_quality_label,
};
use crate::transcode::{BitrateSelector, TranscodeMode, TranscodeQuality};

const ALPN_QUIC: &[&[u8]] = &[b"phonolite-quic"];
const SERVER_CONN_ID_LEN: usize = 16;
const MAX_UDP_SIZE: usize = 65535;
const MAX_QUIC_DATAGRAM: usize = 1350;
const CONTROL_STREAM_MAX_LINE: usize = 64 * 1024;
const MAX_STREAM_BUFFER_BYTES: usize = 64 * 1024;
const STREAM_QUEUE_MS: u32 = 400;
const MAX_RANGE_PRODUCERS: usize = 4;
const SEEK_RESET_MARKER: u16 = 0xFFFF;
const SEEK_RECOVERY_TARGET_MS: u32 = 400;
const PREFETCH_TRACK_LIMIT: usize = 1;
// Current clients discard inactive streams. Enable only with negotiated support.
const PREFETCH_ENABLED: bool = false;
const BUFFER_REPORT_STALE_AFTER: Duration = Duration::from_secs(2);

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ControlMessage {
    #[serde(rename = "auth")]
    Auth { token: String },
    #[serde(rename = "open")]
    Open {
        request_id: Option<u64>,
        track_id: String,
        mode: Option<String>,
        quality: Option<String>,
        frame_ms: Option<u32>,
        queue: Option<Vec<String>>,
        start_ms: Option<u32>,
    },
    #[serde(rename = "advance")]
    Advance,
    #[serde(rename = "buffer")]
    Buffer {
        request_id: Option<u64>,
        buffer_ms: u32,
        target_ms: Option<u32>,
    },
    #[serde(rename = "seek")]
    Seek {
        request_id: Option<u64>,
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
    Error {
        message: &'a str,
        request_id: Option<u64>,
    },
    #[serde(rename = "pong")]
    Pong { ts: Option<i64> },
    #[serde(rename = "stream")]
    Stream {
        request_id: Option<u64>,
        track_id: &'a str,
        stream_id: u64,
        role: &'a str,
        frame_ms: u32,
        seek_id: Option<u32>,
        position_ms: u32,
        range_end_ms: u32,
        end_of_track: bool,
        source_version: &'a str,
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
    request_id: Option<u64>,
    track_id: String,
    role: StreamRole,
    frame_ms: u32,
    mode: TranscodeMode,
    quality: TranscodeQuality,
    rx: tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>,
    _cache_guard: Option<CacheGuard>,
    pending: VecDeque<Bytes>,
    offset: usize,
    finished: bool,
    buffered_bytes: usize,
    sent_bytes: u64,
    last_send: Instant,
    last_drain: Instant,
    producer_failure: Option<String>,
    profile: Option<StreamProfile>,
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
        rx: tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>,
        _cache_guard: Option<CacheGuard>,
    ) -> Self {
        let now = Instant::now();
        Self {
            stream_id,
            generation,
            request_id: None,
            track_id,
            role,
            frame_ms,
            mode,
            quality,
            rx,
            _cache_guard,
            pending: VecDeque::new(),
            offset: 0,
            finished: false,
            buffered_bytes: 0,
            sent_bytes: 0,
            last_send: now,
            last_drain: now,
            producer_failure: None,
            profile: None,
        }
    }

    fn drain_incoming(&mut self) {
        if self.finished {
            return;
        }
        while self.buffered_bytes < MAX_STREAM_BUFFER_BYTES
            && self.pending.len() < (STREAM_QUEUE_MS / self.frame_ms).max(1) as usize
        {
            match self.rx.try_recv() {
                Ok(Ok(bytes)) => {
                    if let Some(profile) = &mut self.profile {
                        profile.observed(&bytes);
                    }
                    self.buffered_bytes = self.buffered_bytes.saturating_add(bytes.len());
                    self.pending.push_back(bytes);
                    self.last_drain = Instant::now();
                }
                Ok(Err(err)) => {
                    if let Some(profile) = &self.profile {
                        profile.timing.stage("producer_failed");
                    }
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
    profile_id: u64,
    pending_operation: Option<OperationTiming>,
    authed: bool,
    auth_token: Option<zeroize::Zeroizing<String>>,
    auth_checked: Instant,
    user_id: Option<String>,
    control_stream: Option<u64>,
    control_outbox: ControlOutbox,
    control_parser: ControlParser,
    next_uni_stream_id: u64,
    stream_generation: u64,
    open_request_id: Option<u64>,
    pending_seek_id: Option<u32>,
    active_track: Option<String>,
    queue: VecDeque<String>,
    outgoing: HashMap<u64, OutgoingStream>,
    track_streams: HashMap<String, u64>,
    playback_frame_ms: u32,
    playback_quality: TranscodeQuality,
    buffer_target_ms: u32,
    client_buffer_ms: u32,
    last_buffer_report: Instant,
    last_debug: Instant,
}

impl SessionState {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            profile_id: if performance::enabled() {
                performance::next_id()
            } else {
                0
            },
            pending_operation: None,
            authed: false,
            auth_token: None,
            auth_checked: now,
            user_id: None,
            control_stream: None,
            control_outbox: ControlOutbox::new(),
            control_parser: ControlParser::new(),
            next_uni_stream_id: 3,
            stream_generation: 0,
            open_request_id: None,
            pending_seek_id: None,
            active_track: None,
            queue: VecDeque::new(),
            outgoing: HashMap::new(),
            track_streams: HashMap::new(),
            playback_frame_ms: 20,
            playback_quality: TranscodeQuality::High,
            buffer_target_ms: 8000,
            client_buffer_ms: 0,
            last_buffer_report: now,
            last_debug: now,
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
                    if client.session.authed && client.session.auth_checked.elapsed() >= Duration::from_secs(1) {
                        client.session.auth_checked = now;
                        let valid = client.session.auth_token.as_ref().is_some_and(|token| state.auth.user_from_token(token).is_ok_and(|user| user.is_some()));
                        if !valid {
                            client.session.authed = false;
                            client.session.auth_token = None;
                            client.session.outgoing.clear();
                            let _ = client.conn.close(true, 401, b"session expired or revoked");
                        }
                    }
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
    // HTTPS and QUIC must share the authenticated identity. Never create an
    // independent identity or replace a missing key after accounts are enrolled.
    crate::tls::identity(config, &state.config_path, true).map_err(|error| error.to_string())
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
                        request_id: None,
                    },
                );
                return;
            }
            match state.auth.user_from_token(&token) {
                Ok(Some(user)) => {
                    client.session.authed = true;
                    client.session.auth_token = Some(zeroize::Zeroizing::new(token));
                    client.session.auth_checked = Instant::now();
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
                            request_id: client.session.open_request_id,
                        },
                    );
                }
                Err(_) => {
                    tracing::warn!("QUIC auth failed: auth error");
                    send_control(
                        client,
                        ControlResponse::Error {
                            message: "auth error",
                            request_id: None,
                        },
                    );
                }
            }
        }
        ControlMessage::Open {
            request_id,
            track_id,
            mode,
            quality,
            frame_ms,
            queue,
            start_ms,
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
                        request_id,
                    },
                );
                return;
            }
            client.session.open_request_id = request_id;
            client.session.pending_seek_id = None;
            client.session.pending_operation =
                OperationTiming::new(client.session.profile_id, "open", request_id, None);
            client.session.bump_stream_generation();
            client.session.client_buffer_ms = 0;
            client.session.buffer_target_ms = 8000;
            client.session.active_track = Some(track_id.clone());
            if let Some(queue) = queue {
                client.session.queue = queue.into();
            } else if client.session.queue.is_empty() {
                client.session.queue.push_back(track_id.clone());
            }
            ensure_active_in_queue(&mut client.session);
            remove_track_stream(&mut client.session, &mut client.conn, &track_id);
            prune_streams(&mut client.session, &mut client.conn);
            let frame_ms = frame_ms.unwrap_or(20);
            if let Err(err) = start_track_stream(
                state,
                client,
                track_id.clone(),
                StreamRole::Active,
                frame_ms,
                start_ms.unwrap_or(0),
                mode.as_deref(),
                quality.as_deref(),
            ) {
                send_control(
                    client,
                    ControlResponse::Error {
                        message: &err,
                        request_id,
                    },
                );
                return;
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
                client.session.pending_seek_id = None;
                client.session.pending_operation = OperationTiming::new(
                    client.session.profile_id,
                    "advance",
                    client.session.open_request_id,
                    None,
                );
                client.session.bump_stream_generation();
                let frame_ms = client.session.playback_frame_ms;
                let quality = client.session.playback_quality;
                client.session.client_buffer_ms = 0;
                client.session.buffer_target_ms = 8000;
                client.session.last_buffer_report = Instant::now();
                client.session.active_track = Some(next.clone());
                remove_track_stream(&mut client.session, &mut client.conn, &next);
                prune_streams(&mut client.session, &mut client.conn);
                if let Err(error) = start_track_stream(
                    state,
                    client,
                    next,
                    StreamRole::Active,
                    frame_ms,
                    0,
                    Some("fixed"),
                    Some(transcode_quality_label(quality)),
                ) {
                    send_control(
                        client,
                        ControlResponse::Error {
                            message: &error,
                            request_id: client.session.open_request_id,
                        },
                    );
                }
            }
        }
        ControlMessage::Buffer {
            request_id,
            buffer_ms,
            target_ms,
        } => {
            if request_id.is_some() && request_id != client.session.open_request_id {
                return;
            }
            client.session.client_buffer_ms = buffer_ms;
            client.session.last_buffer_report = Instant::now();
            if let Some(target) = target_ms {
                client.session.buffer_target_ms = target;
            }
            prune_streams(&mut client.session, &mut client.conn);
            let frame_ms = active_frame_ms(&client.session);
            prebuffer_next_tracks(state, client, None, None, frame_ms);
        }
        ControlMessage::Seek {
            request_id,
            track_id,
            position_ms,
            seek_id,
        } => {
            if !client.session.authed {
                send_control(
                    client,
                    ControlResponse::Error {
                        message: "unauthorized",
                        request_id,
                    },
                );
                return;
            }
            client.session.open_request_id = request_id;
            client.session.pending_seek_id = Some(seek_id);
            client.session.pending_operation =
                OperationTiming::new(client.session.profile_id, "seek", request_id, Some(seek_id));
            client.session.bump_stream_generation();
            client.session.client_buffer_ms = 0;
            client.session.buffer_target_ms = SEEK_RECOVERY_TARGET_MS;
            client.session.last_buffer_report = Instant::now();
            let (frame_ms, mode, quality) = client
                .session
                .track_streams
                .get(&track_id)
                .and_then(|stream_id| client.session.outgoing.get(stream_id))
                .map(|stream| (stream.frame_ms, stream.mode, stream.quality))
                .unwrap_or((
                    client.session.playback_frame_ms,
                    TranscodeMode::Fixed,
                    client.session.playback_quality,
                ));
            remove_track_stream(&mut client.session, &mut client.conn, &track_id);
            client.session.active_track = Some(track_id.clone());
            ensure_active_in_queue(&mut client.session);
            prune_streams(&mut client.session, &mut client.conn);
            if let Err(err) = start_track_stream(
                state,
                client,
                track_id,
                StreamRole::Active,
                frame_ms,
                position_ms,
                Some(transcode_mode_label(mode)),
                Some(transcode_quality_label(quality)),
            ) {
                send_control(
                    client,
                    ControlResponse::Error {
                        message: &err,
                        request_id,
                    },
                );
            }
        }
        // Legacy playback reports remain accepted for transport compatibility.
        // Only durable, consent-gated /stats/events ingestion records listening.
        ControlMessage::Playback { .. } => {}
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
        return session.playback_frame_ms;
    };
    let Some(stream_id) = session.track_streams.get(track_id) else {
        return session.playback_frame_ms;
    };
    session
        .outgoing
        .get(stream_id)
        .map(|outgoing| outgoing.frame_ms)
        .unwrap_or(session.playback_frame_ms)
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
    PREFETCH_ENABLED
        && session.buffer_target_ms > 0
        && session.client_buffer_ms >= session.buffer_target_ms
}

fn active_backpressure_paused(session: &SessionState, now: Instant) -> bool {
    session.buffer_target_ms > 0
        && session.client_buffer_ms >= session.buffer_target_ms
        && now.duration_since(session.last_buffer_report) <= BUFFER_REPORT_STALE_AFTER
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
        session.outgoing.remove(&stream_id);
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

struct PreparedRangeStream {
    rx: tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>,
    guard: CacheGuard,
    position_ms: u32,
    range_end_ms: u32,
    end_of_track: bool,
    source_version: String,
}

fn spawn_track_transcode(
    state: &AppState,
    track_id: &str,
    frame_ms: u32,
    _mode: TranscodeMode,
    quality: TranscodeQuality,
    start_ms: u32,
    operation: Option<OperationTiming>,
) -> Result<PreparedRangeStream, String> {
    let library = state
        .library_state
        .read()
        .library
        .clone()
        .ok_or("library not ready")?;
    let track = library
        .get_track(track_id)
        .map_err(|error| error.to_string())?
        .ok_or("track not found")?;
    let path = library
        .resolve_relpath(&track.file_relpath)
        .ok_or("music root not configured")?;
    let meta = build_raw_opus_meta(&library, &track);
    let position_ms = clamp_position(start_ms, meta.duration_ms)?;
    let (range_start_ms, range_end_ms) = range_bounds(position_ms, meta.duration_ms);
    let key = CacheKey::new(track_id, frame_ms, TranscodeMode::Fixed, quality)
        .with_range(range_start_ms)
        .with_source(&path)
        .map_err(|error| error.to_string())?;
    let source_version = key.source_version().to_owned();
    let end_of_track = range_end_ms >= meta.duration_ms;
    debug_assert_eq!(MEMORY_CACHE_MAX_BYTES, 256 * 1024 * 1024);
    let shared = state.stream_cache.enabled();
    let writer = state
        .stream_cache
        .writer(key.clone())
        .map_err(|error| error.to_string())?;
    let reader = state
        .stream_cache
        .reader(&key)
        .map_err(|error| error.to_string())?;
    let (writer, reader) = match reader {
        Some(reader) => (writer, reader),
        None if !shared => {
            let (writer, reader) = state
                .stream_cache
                .transient(&key)
                .map_err(|error| error.to_string())?;
            (Some(writer), reader)
        }
        None => return Err("stream cache capacity exhausted; retry playback".into()),
    };
    if let Some(timing) = operation {
        timing.stage(if reader.can_start(position_ms) {
            "range_cache_hit"
        } else if writer.is_some() {
            "range_producer_reserved"
        } else {
            "range_producer_shared"
        });
    }
    let guard = reader.guard();
    if let Some(mut writer) = writer {
        static PRODUCERS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
        let permits = Arc::clone(
            PRODUCERS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(MAX_RANGE_PRODUCERS))),
        );
        tokio::spawn(async move {
            let Ok(permit) = permits.acquire_owned().await else {
                writer.fail("range producer unavailable");
                return;
            };
            if !writer.has_readers() {
                writer.fail("range request superseded before production");
                return;
            }
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let selector = BitrateSelector {
                    quality,
                    fixed_bitrate_bps: None,
                };
                match crate::transcode::transcode_raw_opus_range_cache(
                    &path,
                    selector,
                    frame_ms,
                    meta,
                    range_start_ms,
                    range_end_ms,
                    &mut writer,
                ) {
                    Ok(()) => {
                        let _ = writer.finalize();
                    }
                    Err(error) => {
                        tracing::warn!("range transcode failed: {}", error);
                        writer.fail(error);
                    }
                }
            })
            .await
            .ok();
        });
    }
    let queue_frames = (STREAM_QUEUE_MS / frame_ms).max(1) as usize;
    let (tx, rx) = tokio::sync::mpsc::channel(queue_frames);
    tokio::task::spawn_blocking(move || {
        if let Some(timing) = operation {
            timing.stage("source_prepared");
        }
        if let Err(error) = reader.stream_to(position_ms, &tx) {
            if !tx.is_closed() {
                let _ = tx.blocking_send(Err(std::io::Error::other(error)));
            }
        }
    });
    Ok(PreparedRangeStream {
        rx,
        guard,
        position_ms,
        range_end_ms,
        end_of_track,
        source_version,
    })
}

fn clamp_position(position_ms: u32, duration_ms: u32) -> Result<u32, String> {
    if duration_ms == 0 {
        return Err("track has no playable duration".into());
    }
    Ok(position_ms.min(duration_ms.saturating_sub(1)))
}

fn range_bounds(position_ms: u32, duration_ms: u32) -> (u32, u32) {
    let position = position_ms.min(duration_ms.saturating_sub(1));
    let start = position / CACHE_RANGE_MS * CACHE_RANGE_MS;
    (start, start.saturating_add(CACHE_RANGE_MS).min(duration_ms))
}

fn remove_track_stream(session: &mut SessionState, conn: &mut quiche::Connection, track_id: &str) {
    if let Some(stream_id) = session.track_streams.remove(track_id) {
        session.outgoing.remove(&stream_id);
        let _ = conn.stream_shutdown(stream_id, quiche::Shutdown::Write, 0);
    }
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

    let _requested_mode = parse_transcode_mode(mode).unwrap_or(TranscodeMode::Auto);
    let mode = TranscodeMode::Fixed;
    let quality = parse_transcode_quality(quality).unwrap_or(TranscodeQuality::High);
    let frame_ms = parse_frame_ms(Some(frame_ms)).unwrap_or(20);
    if role == StreamRole::Active {
        client.session.playback_frame_ms = frame_ms;
        client.session.playback_quality = quality;
    }
    let generation = client.session.stream_generation;
    let operation = if role == StreamRole::Active {
        client.session.pending_operation.take()
    } else {
        None
    };
    let prepared = spawn_track_transcode(
        state, &track_id, frame_ms, mode, quality, start_ms, operation,
    )?;

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
            prepared.rx,
            Some(prepared.guard),
        ),
    );
    if let Some(outgoing) = client.session.outgoing.get_mut(&stream_id) {
        outgoing.request_id = client.session.open_request_id;
        if let Some(seek_id) = client.session.pending_seek_id {
            let marker = seek_reset_frame(seek_id);
            outgoing.buffered_bytes += marker.len();
            outgoing.pending.push_back(marker);
        }
    }
    if let Some(timing) = operation {
        timing.stage("stream_registered");
        if let Some(outgoing) = client.session.outgoing.get_mut(&stream_id) {
            outgoing.profile = Some(StreamProfile::new(timing));
        }
    }

    let role_label = match role {
        StreamRole::Active => "active",
        StreamRole::Prefetch => "prefetch",
    };
    send_control(
        client,
        ControlResponse::Stream {
            request_id: client.session.open_request_id,
            track_id: &track_id,
            stream_id,
            role: role_label,
            frame_ms,
            seek_id: client.session.pending_seek_id,
            position_ms: prepared.position_ms,
            range_end_ms: prepared.range_end_ms,
            end_of_track: prepared.end_of_track,
            source_version: &prepared.source_version,
        },
    );

    Ok(())
}

fn seek_reset_frame(seek_id: u32) -> Bytes {
    let mut marker = Vec::with_capacity(6);
    marker.extend_from_slice(&SEEK_RESET_MARKER.to_le_bytes());
    marker.extend_from_slice(&seek_id.to_le_bytes());
    Bytes::from(marker)
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
    let now = Instant::now();
    let mut finished = Vec::new();
    let mut active_failures = Vec::new();
    let mut active_ids = Vec::new();
    let mut prefetch_ids = Vec::new();
    let prefetch_allowed = should_prefetch(session);
    let active_paused = active_backpressure_paused(session, now);
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
        // Check before playback backpressure: a paused/cached client can cancel
        // this stream without ever sending another Open or Seek.
        let capacity = conn.stream_capacity(stream_id);
        if matches!(
            capacity,
            Err(quiche::Error::StreamStopped(_) | quiche::Error::StreamReset(_))
        ) || (outgoing.sent_bytes > 0
            && matches!(capacity, Err(quiche::Error::InvalidStreamState(_))))
        {
            finished.push((stream_id, outgoing.track_id.clone()));
            continue;
        }
        let force_send = outgoing
            .pending
            .front()
            .map(|front| front.len() >= 2 && front[0] == 0xFF && front[1] == 0xFF)
            .unwrap_or(false);
        if !force_send && outgoing.role == StreamRole::Prefetch && !prefetch_allowed {
            continue;
        }
        if !force_send && outgoing.role == StreamRole::Active && active_paused {
            continue;
        }
        outgoing.drain_incoming();
        if let Some(failure) = outgoing.producer_failure.take() {
            if outgoing.role == StreamRole::Active
                && active_track.as_deref() == Some(outgoing.track_id.as_str())
            {
                active_failures.push((outgoing.track_id.clone(), failure, outgoing.request_id));
            }
        }
        let mut terminal_error = false;
        loop {
            let (send_result, data_len, audio_frame) = match outgoing.pending.front() {
                Some(front) => {
                    let data = &front[outgoing.offset..];
                    (
                        conn.stream_send(stream_id, data, false),
                        data.len(),
                        outgoing.profile.is_some() && performance::is_audio_frame(front),
                    )
                }
                None => break,
            };
            match send_result {
                Ok(sent) => {
                    if sent == 0 && data_len > 0 {
                        if let Some(profile) = &mut outgoing.profile {
                            profile.blocked();
                        }
                    }
                    if sent > 0 {
                        if let Some(profile) = &mut outgoing.profile {
                            profile.submitted(sent, audio_frame && sent == data_len);
                        }
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
                Err(quiche::Error::Done) => {
                    if let Some(profile) = &mut outgoing.profile {
                        profile.blocked();
                    }
                    break;
                }
                Err(err) => {
                    terminal_error = true;
                    if !peer_cancelled_stream(&err) {
                        tracing::warn!(
                            "QUIC stream send failed track={} stream_id={} err={:?}",
                            outgoing.track_id,
                            stream_id,
                            err
                        );
                        if outgoing.role == StreamRole::Active {
                            active_failures.push((
                                outgoing.track_id.clone(),
                                err.to_string(),
                                outgoing.request_id,
                            ));
                        }
                    }
                    break;
                }
            }
        }
        if terminal_error {
            finished.push((stream_id, outgoing.track_id.clone()));
            continue;
        }
        if outgoing.finished && outgoing.pending.is_empty() {
            match conn.stream_send(stream_id, &[], true) {
                Ok(_) => {}
                Err(quiche::Error::Done) => continue,
                Err(error) => {
                    if !peer_cancelled_stream(&error) && outgoing.role == StreamRole::Active {
                        active_failures.push((
                            outgoing.track_id.clone(),
                            error.to_string(),
                            outgoing.request_id,
                        ));
                    }
                }
            }
            finished.push((stream_id, outgoing.track_id.clone()));
        }
    }
    remove_finished_streams(session, finished);
    for (track_id, failure, request_id) in active_failures {
        let message = format!("stream failed for {}: {}", track_id, failure);
        enqueue_control(
            &mut session.control_outbox,
            ControlResponse::Error {
                message: &message,
                request_id,
            },
        );
    }
}

fn peer_cancelled_stream(error: &quiche::Error) -> bool {
    matches!(
        error,
        quiche::Error::StreamStopped(_) | quiche::Error::StreamReset(_)
    )
}

fn remove_finished_streams(session: &mut SessionState, streams: Vec<(u64, String)>) {
    for (stream_id, track_id) in streams {
        // Drop the receiver and cache guard immediately; a stopped peer cannot
        // consume queued data or FIN and must not pin a range reservation.
        session.outgoing.remove(&stream_id);
        if session.track_streams.get(&track_id) == Some(&stream_id) {
            session.track_streams.remove(&track_id);
        }
    }
}

fn maybe_log_streams(session: &mut SessionState, conn: &quiche::Connection) {
    let now = Instant::now();
    if now.duration_since(session.last_debug) < Duration::from_secs(5) {
        return;
    }
    session.last_debug = now;
    let path = conn.path_stats().next();
    let buffer_report_age_ms = now.duration_since(session.last_buffer_report).as_millis();
    let active_paused = active_backpressure_paused(session, now);
    if performance::enabled() {
        let stats = conn.stats();
        let active_path = conn.path_stats().find(|path| path.active);
        tracing::info!(
            target: "performance",
            event = "quic_transport_sample",
            session_id = session.profile_id,
            transport_sent_bytes = stats.sent_bytes,
            transport_received_bytes = stats.recv_bytes,
            transport_lost_bytes = stats.lost_bytes,
            retransmitted_stream_bytes = stats.stream_retrans_bytes,
            sent_packets = stats.sent,
            lost_packets = stats.lost,
            rtt_ms = active_path.as_ref().map(|path| performance::milliseconds(path.rtt)),
            congestion_window_bytes = active_path.as_ref().map(|path| path.cwnd),
        );
    }
    for outgoing in session.outgoing.values_mut() {
        let capacity = conn.stream_capacity(outgoing.stream_id).ok();
        let paused = outgoing.role == StreamRole::Active && active_paused;
        if let Some(profile) = &mut outgoing.profile {
            profile.sample(
                outgoing.stream_id,
                outgoing.buffered_bytes,
                session.client_buffer_ms,
                paused,
            );
        }
        tracing::info!(
            "QUIC stream debug track={} role={:?} pending_chunks={} buffered_bytes={} finished={} sent_bytes={} since_last_send_ms={} since_last_drain_ms={} client_buffer_ms={} target_ms={} buffer_report_age_ms={} paused={} capacity={:?} established={} stats_sent={} stats_recv={} path={:?}",
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
            buffer_report_age_ms,
            paused,
            capacity,
            conn.is_established(),
            conn.stats().sent_bytes,
            conn.stats().recv_bytes,
            path,
        );
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

#[cfg(test)]
mod streaming_tests {
    use super::*;
    #[test]
    fn request_generation_and_seek_range_are_announced_out_of_band() {
        let response = ControlResponse::Stream {
            request_id: Some(42),
            track_id: "track",
            stream_id: 11,
            role: "active",
            frame_ms: 20,
            seek_id: Some(9),
            position_ms: 35005,
            range_end_ms: 60000,
            end_of_track: false,
            source_version: "version",
        };
        let json = serde_json::to_value(response).unwrap();
        assert_eq!(json["request_id"], 42);
        assert_eq!(json["seek_id"], 9);
        assert_eq!(json["position_ms"], 35005);
        assert_eq!(json["range_end_ms"], 60000);
        assert_eq!(json["end_of_track"], false);
        assert_eq!(json["source_version"], "version");
        let error = serde_json::to_value(ControlResponse::Error {
            message: "failed",
            request_id: Some(42),
        })
        .unwrap();
        assert_eq!(error["request_id"], 42);
    }
    #[test]
    fn reconnect_open_target_and_seek_generation_are_parsed() {
        let open: ControlMessage = serde_json::from_str(
            r#"{"type":"open","request_id":7,"track_id":"track","start_ms":35005}"#,
        )
        .unwrap();
        assert!(matches!(
            open,
            ControlMessage::Open {
                request_id: Some(7),
                start_ms: Some(35005),
                ..
            }
        ));
        let seek: ControlMessage = serde_json::from_str(
            r#"{"type":"seek","request_id":8,"track_id":"track","position_ms":200,"seek_id":9}"#,
        )
        .unwrap();
        assert!(matches!(
            seek,
            ControlMessage::Seek {
                request_id: Some(8),
                seek_id: 9,
                ..
            }
        ));
        let legacy: ControlMessage =
            serde_json::from_str(r#"{"type":"seek","track_id":"track","position_ms":200}"#)
                .unwrap();
        assert!(matches!(
            legacy,
            ControlMessage::Seek {
                request_id: None,
                seek_id: 0,
                ..
            }
        ));
    }
    #[test]
    fn range_bounds_handle_boundaries_clamping_and_track_end() {
        assert_eq!(range_bounds(35005, 95000), (35000, 40000));
        assert_eq!(range_bounds(60000, 95000), (60000, 65000));
        assert_eq!(range_bounds(100000, 95000), (90000, 95000));
        assert_eq!(range_bounds(60000, 60000), (55000, 60000));
        assert_eq!(range_bounds(0, 0), (0, 0));
    }
    #[test]
    fn exact_end_and_past_end_targets_clamp_to_a_playable_position() {
        assert_eq!(clamp_position(30000, 30000).unwrap(), 29999);
        assert_eq!(clamp_position(u32::MAX, 30000).unwrap(), 29999);
        assert_eq!(clamp_position(0, 1).unwrap(), 0);
        assert!(clamp_position(0, 0).is_err());
        assert_eq!(
            range_bounds(clamp_position(30000, 30000).unwrap(), 30000),
            (25000, 30000)
        );
    }
    #[test]
    fn outgoing_pcm_independent_staging_is_bounded_by_media_time() {
        let (tx, rx) = tokio::sync::mpsc::channel(40);
        for _ in 0..40 {
            tx.try_send(Ok(Bytes::from_static(&[1, 0, 2]))).unwrap();
        }
        let mut stream = OutgoingStream::new(
            3,
            1,
            "track".into(),
            StreamRole::Active,
            20,
            TranscodeMode::Fixed,
            TranscodeQuality::High,
            rx,
            None,
        );
        stream.drain_incoming();
        assert_eq!(stream.pending.len(), 20);
        assert_eq!(stream.rx.len(), 20);
        assert_eq!(
            stream.pending.len() as u32 * stream.frame_ms,
            STREAM_QUEUE_MS
        );
    }
    #[test]
    fn consecutive_seeks_allocate_distinct_stream_ids_and_marker_is_prefix() {
        let mut session = SessionState::new();
        assert_eq!(
            (
                session.next_server_uni_stream(),
                session.next_server_uni_stream()
            ),
            (3, 7)
        );
        assert_eq!(seek_reset_frame(9).as_ref(), &[255, 255, 9, 0, 0, 0]);
    }
    #[test]
    fn cancelled_stream_releases_paused_reader_and_preserves_current_replacement() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let mut session = SessionState::new();
        session.client_buffer_ms = 10000;
        session.track_streams.insert("track".into(), 7);
        session.outgoing.insert(
            3,
            OutgoingStream::new(
                3,
                1,
                "track".into(),
                StreamRole::Active,
                20,
                TranscodeMode::Fixed,
                TranscodeQuality::High,
                rx,
                None,
            ),
        );
        tx.try_send(Ok(Bytes::from_static(&[1, 0, 2]))).unwrap();
        assert!(peer_cancelled_stream(&quiche::Error::StreamStopped(0)));
        assert!(peer_cancelled_stream(&quiche::Error::StreamReset(0)));
        assert!(!peer_cancelled_stream(&quiche::Error::Done));
        remove_finished_streams(&mut session, vec![(3, "track".into())]);
        assert!(tx.is_closed());
        assert!(session.outgoing.is_empty());
        assert_eq!(session.track_streams.get("track"), Some(&7));
    }
}
