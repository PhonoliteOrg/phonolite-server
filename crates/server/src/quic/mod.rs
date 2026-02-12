use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use bytes::Bytes;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;

use crate::config::{resolve_path, ServerConfig};
use crate::state::AppState;
use crate::streaming::{
    build_raw_opus_meta, parse_frame_ms, parse_transcode_mode, parse_transcode_quality,
    transcode_mode_label, transcode_quality_label,
};
use crate::transcode::{BitrateSelector, TranscodeMode, TranscodeQuality};
use common::join_relpath;

const ALPN_QUIC: &[&[u8]] = &[b"phonolite-quic"];
const SERVER_CONN_ID_LEN: usize = 16;
const MAX_UDP_SIZE: usize = 65535;
const MAX_QUIC_DATAGRAM: usize = 1350;
const CONTROL_STREAM_MAX_LINE: usize = 64 * 1024;
const MAX_STREAM_BUFFER_BYTES: usize = 6 * 1024 * 1024;
const SEEK_RESET_MARKER: u16 = 0xFFFF;

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
    #[serde(rename = "queue")]
    Queue { track_ids: Vec<String> },
    #[serde(rename = "advance")]
    Advance,
    #[serde(rename = "buffer")]
    Buffer { buffer_ms: u32, target_ms: Option<u32> },
    #[serde(rename = "seek")]
    Seek { track_id: String, position_ms: u32 },
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
    track_id: String,
    role: StreamRole,
    frame_ms: u32,
    mode: TranscodeMode,
    quality: TranscodeQuality,
    rx: tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>,
    pending: VecDeque<Bytes>,
    offset: usize,
    finished: bool,
    buffered_bytes: usize,
    sent_bytes: u64,
    last_send: Instant,
    last_drain: Instant,
    last_send_log: Instant,
    last_send_err: Option<String>,
}

impl OutgoingStream {
    fn new(
        stream_id: u64,
        track_id: String,
        role: StreamRole,
        frame_ms: u32,
        mode: TranscodeMode,
        quality: TranscodeQuality,
        rx: tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>,
    ) -> Self {
        let now = Instant::now();
        Self {
            stream_id,
            track_id,
            role,
            frame_ms,
            mode,
            quality,
            rx,
            pending: VecDeque::new(),
            offset: 0,
            finished: false,
            buffered_bytes: 0,
            sent_bytes: 0,
            last_send: now,
            last_drain: now,
            last_send_log: now,
            last_send_err: None,
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
                Ok(Err(_)) => {
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
    active_track: Option<String>,
    queue: VecDeque<String>,
    outgoing: HashMap<u64, OutgoingStream>,
    track_streams: HashMap<String, u64>,
    buffer_target_ms: u32,
    client_buffer_ms: u32,
    last_debug: Instant,
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
            active_track: None,
            queue: VecDeque::new(),
            outgoing: HashMap::new(),
            track_streams: HashMap::new(),
            buffer_target_ms: 8000,
            client_buffer_ms: 0,
            last_debug: Instant::now(),
        }
    }

    fn next_server_uni_stream(&mut self) -> u64 {
        let id = self.next_uni_stream_id;
        self.next_uni_stream_id = self.next_uni_stream_id.saturating_add(4);
        id
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

    let mut tick = tokio::time::interval(Duration::from_millis(25));

    loop {
        tokio::select! {
            result = socket.recv_from(&mut recv_buf) => {
                let (len, from) = match result {
                    Ok(value) => value,
                    Err(err) => {
                        tracing::error!("quic recv error: {}", err);
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

                flush_conn(&mut client.conn, &socket, &mut send_buf);
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
                        closed.push(id.clone());
                        continue;
                    }
                    refresh_conn_ids(&mut conn_id_map, &mut client.conn, id);
                    handle_readable(&state, client);
                    flush_control(&mut client.session, &mut client.conn);
                    flush_streams(&mut client.session, &mut client.conn);
                    flush_conn(&mut client.conn, &socket, &mut send_buf);
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

fn resolve_quic_bind_addr(config: &ServerConfig) -> Result<SocketAddr, String> {
    let host = config
        .bind_addr
        .as_deref()
        .and_then(|value| value.split(':').next())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("0.0.0.0");
    let addr = format!("{}:{}", host, config.quic_port);
    addr.parse::<SocketAddr>()
        .map_err(|err| format!("invalid quic bind addr: {}", err))
}

fn ensure_quic_certs(state: &AppState, config: &ServerConfig) -> Result<(PathBuf, PathBuf), String> {
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
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("cert dir error: {}", err))?;
        }
    }
    if let Some(parent) = key_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("key dir error: {}", err))?;
        }
    }
    std::fs::write(&cert_path, cert_pem)
        .map_err(|err| format!("cert write error: {}", err))?;
    std::fs::write(&key_path, key_pem)
        .map_err(|err| format!("key write error: {}", err))?;

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
    config.set_max_idle_timeout(30_000);
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
            client.session.active_track = Some(track_id.clone());
            if let Some(queue) = queue {
                client.session.queue = queue.into();
            } else if client.session.queue.is_empty() {
                client.session.queue.push_back(track_id.clone());
            }
            ensure_active_in_queue(&mut client.session);
            promote_existing_stream(&mut client.session, &track_id, StreamRole::Active);
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
                    send_control(
                        client,
                        ControlResponse::Error {
                            message: &err,
                        },
                    );
                    return;
                }
            }
            send_control(
                client,
                ControlResponse::OpenOk {
                    track_id: &track_id,
                },
            );
            prebuffer_next_two(state, client, mode.as_deref(), quality.as_deref(), frame_ms);
        }
        ControlMessage::Queue { track_ids } => {
            client.session.queue = track_ids.into();
            ensure_active_in_queue(&mut client.session);
            prune_streams(&mut client.session, &mut client.conn);
            let frame_ms = active_frame_ms(&client.session);
            prebuffer_next_two(state, client, None, None, frame_ms);
        }
        ControlMessage::Advance => {
            if let Some(next) = next_track_in_queue(&client.session) {
                let frame_ms = active_frame_ms(&client.session);
                client.session.active_track = Some(next.clone());
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
                prebuffer_next_two(state, client, None, None, frame_ms);
            }
        }
        ControlMessage::Buffer { buffer_ms, target_ms } => {
            client.session.client_buffer_ms = buffer_ms;
            if let Some(target) = target_ms {
                client.session.buffer_target_ms = target;
            }
        }
        ControlMessage::Seek { track_id, position_ms } => {
            tracing::info!("QUIC seek track={} position_ms={}", track_id, position_ms);
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
            client.session.active_track = Some(track_id.clone());
            ensure_active_in_queue(&mut client.session);
            let mut frame_ms = active_frame_ms(&client.session);
            let mut mode_label: Option<&str> = None;
            let mut quality_label: Option<&str> = None;
            if let Some(stream_id) = client.session.track_streams.get(&track_id).cloned() {
                if let Some(outgoing) = client.session.outgoing.get(&stream_id) {
                    frame_ms = outgoing.frame_ms;
                    mode_label = Some(transcode_mode_label(outgoing.mode));
                    quality_label = Some(transcode_quality_label(outgoing.quality));
                }
                tracing::info!(
                    "QUIC seek switching streams track={} stream_id={}",
                    track_id,
                    stream_id
                );
                client.session.track_streams.remove(&track_id);
                client.session.outgoing.remove(&stream_id);
                let _ = client
                    .conn
                    .stream_shutdown(stream_id, quiche::Shutdown::Write, 0);
            }
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
    let Some(active) = session.active_track.as_ref() else { return };
    if session.queue.iter().any(|id| id == active) {
        return;
    }
    session.queue.push_front(active.clone());
}

fn promote_existing_stream(session: &mut SessionState, track_id: &str, role: StreamRole) {
    if let Some(stream_id) = session.track_streams.get(track_id).cloned() {
        if let Some(outgoing) = session.outgoing.get_mut(&stream_id) {
            outgoing.role = role;
        }
    }
}

fn prune_streams(session: &mut SessionState, conn: &mut quiche::Connection) {
    let mut allowed: HashSet<String> = HashSet::new();
    if let Some(active) = session.active_track.as_ref() {
        allowed.insert(active.clone());
    }
    for id in session.queue.iter() {
        allowed.insert(id.clone());
    }
    let mut remove_ids = Vec::new();
    for (stream_id, outgoing) in session.outgoing.iter() {
        if !allowed.contains(&outgoing.track_id) {
            remove_ids.push((*stream_id, outgoing.track_id.clone()));
        }
    }
    for (stream_id, track_id) in remove_ids {
        let _ = conn.stream_shutdown(stream_id, quiche::Shutdown::Write, 0);
        session.outgoing.remove(&stream_id);
        session.track_streams.remove(&track_id);
    }
}

fn prebuffer_next_two(
    state: &AppState,
    client: &mut ClientConn,
    mode: Option<&str>,
    quality: Option<&str>,
    frame_ms: u32,
) {
    let active = match client.session.active_track.as_ref() {
        Some(value) => value.clone(),
        None => return,
    };
    let mut remaining = Vec::new();
    let mut seen_active = false;
    for id in client.session.queue.iter() {
        if !seen_active {
            if id == &active {
                seen_active = true;
            }
            continue;
        }
        remaining.push(id.clone());
        if remaining.len() >= 2 {
            break;
        }
    }
    for track_id in remaining {
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
) -> Result<tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>, String> {
    let library_guard = state.library_state.read();
    let library = library_guard
        .library
        .clone()
        .ok_or_else(|| "library not ready".to_string())?;
    let track = library
        .get_track(track_id)
        .map_err(|err| format!("library error: {}", err))?
        .ok_or_else(|| "track not found".to_string())?;
    let root = library.root().to_path_buf();
    let path = join_relpath(&root, &track.file_relpath);
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
        adaptive_bitrate_bps: session.as_ref().map(|s| std::sync::Arc::clone(&s.target_bitrate_bps)),
    };

    let meta = build_raw_opus_meta(&library, &track);
    let start_ms = start_ms.min(meta.duration_ms);
    let (tx, rx) =
        tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(256);
    let path_clone = path.clone();
    let track_id_clone = track_id.to_string();
    tokio::task::spawn_blocking(move || {
        let result = crate::transcode::transcode_to_raw_opus(
            &path_clone,
            selector,
            frame_ms,
            meta,
            start_ms,
            &tx,
        );
        if let Err(err) = result {
            tracing::warn!("QUIC transcode failed track={} err={}", track_id_clone, err);
            let _ = tx.blocking_send(Err(std::io::Error::new(std::io::ErrorKind::Other, err)));
        }
    });

    Ok(rx)
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
    let rx = spawn_track_transcode(state, &track_id, frame_ms, mode, quality, start_ms)?;

    let stream_id = client.session.next_server_uni_stream();
    client
        .session
        .track_streams
        .insert(track_id.clone(), stream_id);
    client.session.outgoing.insert(
        stream_id,
        OutgoingStream::new(stream_id, track_id.clone(), role, frame_ms, mode, quality, rx),
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
    track_id: &str,
    position_ms: u32,
) -> Result<(), String> {
    let frame_ms = outgoing.frame_ms;
    let mode = outgoing.mode;
    let quality = outgoing.quality;
    let rx = spawn_track_transcode(state, track_id, frame_ms, mode, quality, position_ms)?;

    outgoing.rx = rx;
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

    let marker = SEEK_RESET_MARKER.to_le_bytes().to_vec();
    outgoing.pending.push_back(Bytes::from(marker));
    outgoing.buffered_bytes = outgoing.buffered_bytes.saturating_add(2);

    Ok(())
}

fn send_control(client: &mut ClientConn, message: ControlResponse<'_>) {
    let payload = match serde_json::to_string(&message) {
        Ok(value) => value,
        Err(_) => return,
    };
    let mut line = payload;
    line.push('\n');
    client.session.control_outbox.enqueue(Bytes::from(line));
}

fn flush_control(session: &mut SessionState, conn: &mut quiche::Connection) {
    let stream_id = match session.control_stream {
        Some(value) => value,
        None => return,
    };
    loop {
        let Some(front) = session.control_outbox.pending.front() else { break };
        let data = &front[session.control_outbox.offset..];
        match conn.stream_send(stream_id, data, false) {
            Ok(sent) => {
                if sent == data.len() {
                    session.control_outbox.pending.pop_front();
                    session.control_outbox.offset = 0;
                } else {
                    session.control_outbox.offset = session.control_outbox.offset.saturating_add(sent);
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
    let mut active_ids = Vec::new();
    let mut prefetch_ids = Vec::new();
    for (stream_id, outgoing) in session.outgoing.iter() {
        if outgoing.role == StreamRole::Active {
            active_ids.push(*stream_id);
        } else {
            prefetch_ids.push(*stream_id);
        }
    }
    for stream_id in active_ids.into_iter().chain(prefetch_ids.into_iter()) {
        let Some(outgoing) = session.outgoing.get_mut(&stream_id) else { continue };
        let force_send = outgoing
            .pending
            .front()
            .map(|front| front.len() == 2 && front[0] == 0xFF && front[1] == 0xFF)
            .unwrap_or(false);
        if !force_send
            && outgoing.role == StreamRole::Active
            && session.buffer_target_ms > 0
            && session.client_buffer_ms >= session.buffer_target_ms
        {
            continue;
        }
        outgoing.drain_incoming();
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
                    if sent == data_len {
                        outgoing.pending.pop_front();
                        outgoing.offset = 0;
                        outgoing.buffered_bytes =
                            outgoing.buffered_bytes.saturating_sub(data_len);
                    } else {
                        outgoing.offset = outgoing.offset.saturating_add(sent);
                        break;
                    }
                    if sent > 0 {
                        outgoing.sent_bytes = outgoing.sent_bytes.saturating_add(sent as u64);
                        outgoing.last_send = Instant::now();
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
            let _ = conn.stream_shutdown(stream_id, quiche::Shutdown::Write, 0);
            finished.push((stream_id, outgoing.track_id.clone()));
        }
    }
    for (stream_id, track_id) in finished {
        session.outgoing.remove(&stream_id);
        session.track_streams.remove(&track_id);
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

fn flush_conn(
    conn: &mut quiche::Connection,
    socket: &UdpSocket,
    out: &mut [u8],
) {
    loop {
        match conn.send(out) {
            Ok((len, send_info)) => {
                let _ = socket.try_send_to(&out[..len], send_info.to);
            }
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
