use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, CString};
use std::net::{ToSocketAddrs, UdpSocket};
use std::os::raw::{c_char, c_int};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use bytes::Bytes;
use rand::RngCore;
use serde::{Deserialize, Serialize};

const ALPN_QUIC: &[&[u8]] = &[b"phonolite-quic"];
const MAX_UDP_SIZE: usize = 65535;
const MAX_QUIC_DATAGRAM: usize = 1350;
const MAX_PREFETCH_BYTES: usize = 12 * 1024 * 1024;

#[repr(C)]
pub struct QuicHandle {
    inner: Arc<ClientHandle>,
}

struct ClientHandle {
    tx: mpsc::Sender<ControlCommand>,
    rx: Mutex<mpsc::Receiver<Vec<u8>>>,
    last_error: Arc<Mutex<Option<String>>>,
    last_stats: Arc<Mutex<Option<String>>>,
}

enum ControlCommand {
    Open {
        track_id: String,
        mode: Option<String>,
        quality: Option<String>,
        frame_ms: u32,
        queue: Vec<String>,
    },
    Buffer {
        buffer_ms: u32,
        target_ms: Option<u32>,
    },
    Advance,
    Close,
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum ClientMessage<'a> {
    #[serde(rename = "auth")]
    Auth { token: &'a str },
    #[serde(rename = "open")]
    Open {
        track_id: &'a str,
        mode: Option<&'a str>,
        quality: Option<&'a str>,
        frame_ms: u32,
        queue: Option<&'a [String]>,
    },
    #[serde(rename = "buffer")]
    Buffer { buffer_ms: u32, target_ms: Option<u32> },
    #[serde(rename = "advance")]
    Advance,
    #[serde(rename = "ping")]
    Ping { ts: Option<i64> },
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ServerMessage {
    #[serde(rename = "auth_ok")]
    AuthOk,
    #[serde(rename = "error")]
    Error { message: String },
    #[serde(rename = "stream")]
    Stream {
        track_id: String,
        stream_id: u64,
        role: String,
        frame_ms: u32,
    },
    #[serde(rename = "open_ok")]
    OpenOk { track_id: String },
    #[serde(rename = "pong")]
    Pong { ts: Option<i64> },
}

struct ClientState {
    control_stream_id: u64,
    pending_control: VecDeque<Bytes>,
    control_offset: usize,
    control_buf: Vec<u8>,
    active_track: Option<String>,
    active_stream: Option<u64>,
    track_streams: HashMap<String, u64>,
    prefetch_buffers: HashMap<String, VecDeque<Bytes>>,
    prefetch_bytes: HashMap<String, usize>,
    pending_streams: HashMap<u64, VecDeque<Bytes>>,
    pending_stream_bytes: HashMap<u64, usize>,
}

impl ClientState {
    fn new() -> Self {
        Self {
            control_stream_id: 0,
            pending_control: VecDeque::new(),
            control_offset: 0,
            control_buf: Vec::new(),
            active_track: None,
            active_stream: None,
            track_streams: HashMap::new(),
            prefetch_buffers: HashMap::new(),
            prefetch_bytes: HashMap::new(),
            pending_streams: HashMap::new(),
            pending_stream_bytes: HashMap::new(),
        }
    }
}

#[no_mangle]
pub extern "C" fn phonolite_quic_connect(
    host: *const c_char,
    port: u16,
    token: *const c_char,
) -> *mut QuicHandle {
    let host = unsafe { cstr_to_string(host) };
    let token = unsafe { cstr_to_string(token) };
    if host.is_empty() || token.is_empty() {
        return std::ptr::null_mut();
    }
    let addr = format!("{}:{}", host, port);

    let (tx_cmd, rx_cmd) = mpsc::channel::<ControlCommand>();
    let (tx_bytes, rx_bytes) = mpsc::channel::<Vec<u8>>();
    let last_error = Arc::new(Mutex::new(None));
    let last_stats = Arc::new(Mutex::new(None));
    let handle = Arc::new(ClientHandle {
        tx: tx_cmd.clone(),
        rx: Mutex::new(rx_bytes),
        last_error: Arc::clone(&last_error),
        last_stats: Arc::clone(&last_stats),
    });

    let thread_handle = handle.clone();
    thread::spawn(move || {
        if let Err(err) = run_client(addr, token, rx_cmd, tx_bytes, last_error, last_stats) {
            if let Ok(mut guard) = thread_handle.last_error.lock() {
                *guard = Some(err);
            }
        }
    });

    let boxed = Box::new(QuicHandle { inner: handle });
    Box::into_raw(boxed)
}

#[no_mangle]
pub extern "C" fn phonolite_quic_open_track(
    handle: *mut QuicHandle,
    track_id: *const c_char,
    mode: *const c_char,
    quality: *const c_char,
    frame_ms: u32,
    queue_json: *const c_char,
) -> c_int {
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return -1;
    };
    let track_id = unsafe { cstr_to_string(track_id) };
    if track_id.is_empty() {
        return -2;
    }
    let mode = unsafe { cstr_to_optional_string(mode) };
    let quality = unsafe { cstr_to_optional_string(quality) };
    let queue = unsafe { cstr_to_optional_string(queue_json) }
        .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
        .unwrap_or_default();

    if handle
        .inner
        .tx
        .send(ControlCommand::Open {
            track_id,
            mode,
            quality,
            frame_ms,
            queue,
        })
        .is_err()
    {
        return -3;
    }
    0
}

#[no_mangle]
pub extern "C" fn phonolite_quic_send_buffer(
    handle: *mut QuicHandle,
    buffer_ms: u32,
    target_ms: u32,
) -> c_int {
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return -1;
    };
    let target = if target_ms == 0 { None } else { Some(target_ms) };
    if handle
        .inner
        .tx
        .send(ControlCommand::Buffer { buffer_ms, target_ms: target })
        .is_err()
    {
        return -2;
    }
    0
}

#[no_mangle]
pub extern "C" fn phonolite_quic_advance(handle: *mut QuicHandle) -> c_int {
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return -1;
    };
    if handle.inner.tx.send(ControlCommand::Advance).is_err() {
        return -2;
    }
    0
}

#[no_mangle]
pub extern "C" fn phonolite_quic_read(
    handle: *mut QuicHandle,
    buffer: *mut u8,
    buffer_len: usize,
) -> c_int {
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return -1;
    };
    if buffer.is_null() || buffer_len == 0 {
        return -2;
    }
    let rx = match handle.inner.rx.lock() {
        Ok(value) => value,
        Err(_) => return -3,
    };
    match rx.try_recv() {
        Ok(chunk) => {
            if chunk.is_empty() {
                return 0;
            }
            let len = chunk.len().min(buffer_len);
            unsafe {
                std::ptr::copy_nonoverlapping(chunk.as_ptr(), buffer, len);
            }
            len as c_int
        }
        Err(mpsc::TryRecvError::Empty) => -5,
        Err(mpsc::TryRecvError::Disconnected) => -4,
    }
}

#[no_mangle]
pub extern "C" fn phonolite_quic_last_error(handle: *mut QuicHandle) -> *mut c_char {
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return std::ptr::null_mut();
    };
    let msg = match handle.inner.last_error.lock() {
        Ok(value) => value.clone().unwrap_or_default(),
        Err(_) => String::new(),
    };
    let cstring = CString::new(msg).unwrap_or_else(|_| CString::new("").unwrap());
    cstring.into_raw()
}

#[no_mangle]
pub extern "C" fn phonolite_quic_poll_stats(handle: *mut QuicHandle) -> *mut c_char {
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return std::ptr::null_mut();
    };
    let msg = match handle.inner.last_stats.lock() {
        Ok(mut value) => value.take().unwrap_or_default(),
        Err(_) => String::new(),
    };
    if msg.is_empty() {
        return std::ptr::null_mut();
    }
    let cstring = CString::new(msg).unwrap_or_else(|_| CString::new("").unwrap());
    cstring.into_raw()
}

#[no_mangle]
pub extern "C" fn phonolite_quic_free_string(ptr: *mut c_char) {
    if ptr.is_null() {
        return;
    }
    unsafe {
        let _ = CString::from_raw(ptr);
    }
}

#[no_mangle]
pub extern "C" fn phonolite_quic_close(handle: *mut QuicHandle) {
    if handle.is_null() {
        return;
    }
    unsafe {
        let boxed = Box::from_raw(handle);
        let _ = boxed.inner.tx.send(ControlCommand::Close);
    }
}

unsafe fn cstr_to_string(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    CStr::from_ptr(ptr).to_string_lossy().trim().to_string()
}

unsafe fn cstr_to_optional_string(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let value = CStr::from_ptr(ptr).to_string_lossy().trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn run_client(
    addr: String,
    token: String,
    rx_cmd: mpsc::Receiver<ControlCommand>,
    tx_bytes: mpsc::Sender<Vec<u8>>,
    last_error: Arc<Mutex<Option<String>>>,
    last_stats: Arc<Mutex<Option<String>>>,
) -> Result<(), String> {
    let mut addrs = addr
        .to_socket_addrs()
        .map_err(|err| format!("invalid server addr: {}", err))?
        .collect::<Vec<_>>();
    let server_addr = addrs
        .iter()
        .find(|addr| addr.is_ipv4())
        .copied()
        .or_else(|| addrs.first().copied())
        .ok_or_else(|| "invalid server addr: no resolved addresses".to_string())?;
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|err| format!("{}", err))?;
    socket
        .connect(server_addr)
        .map_err(|err| format!("socket connect: {}", err))?;
    socket
        .set_nonblocking(true)
        .map_err(|err| format!("socket nonblock: {}", err))?;
    let local_addr = socket.local_addr().map_err(|err| format!("{}", err))?;
    if let Ok(mut guard) = last_stats.lock() {
        *guard = Some(format!(
            "QUIC client socket local={} server={}",
            local_addr, server_addr
        ));
    }

    let mut config =
        quiche::Config::new(quiche::PROTOCOL_VERSION).map_err(|e| format!("{:?}", e))?;
    config
        .set_application_protos(ALPN_QUIC)
        .map_err(|err| format!("alpn error: {:?}", err))?;
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

    let scid = generate_cid();
    let scid_conn_id = quiche::ConnectionId::from_ref(&scid);
    let mut conn = quiche::connect(None, &scid_conn_id, local_addr, server_addr, &mut config)
        .map_err(|err| format!("connect error: {:?}", err))?;

    let mut state = ClientState::new();
    enqueue_control(&mut state, ClientMessage::Auth { token: &token });

    let mut recv_buf = vec![0u8; MAX_UDP_SIZE];
    let mut send_buf = vec![0u8; MAX_UDP_SIZE];
    let mut next_timeout = conn.timeout().map(|t| Instant::now() + t);
    let mut last_stats_log = Instant::now();
    let mut last_ping = Instant::now();
    let mut last_ack_elicit = Instant::now();

    loop {
        while let Ok(cmd) = rx_cmd.try_recv() {
            match cmd {
                ControlCommand::Open {
                    track_id,
                    mode,
                    quality,
                    frame_ms,
                    queue,
                } => {
                    state.active_track = Some(track_id.clone());
                    state.active_stream = None;
                    state.track_streams.clear();
                    state.prefetch_buffers.clear();
                    state.prefetch_bytes.clear();
                    state.pending_streams.clear();
                    state.pending_stream_bytes.clear();
                    let queue_opt = if queue.is_empty() { None } else { Some(queue.as_slice()) };
                    enqueue_control(
                        &mut state,
                        ClientMessage::Open {
                            track_id: &track_id,
                            mode: mode.as_deref(),
                            quality: quality.as_deref(),
                            frame_ms,
                            queue: queue_opt,
                        },
                    );
                    if let Some(stream_id) = state.track_streams.get(&track_id).cloned() {
                        state.active_stream = Some(stream_id);
                        flush_prefetch_to_output(&mut state, &track_id, &tx_bytes);
                    }
                }
                ControlCommand::Buffer { buffer_ms, target_ms } => {
                    enqueue_control(
                        &mut state,
                        ClientMessage::Buffer {
                            buffer_ms,
                            target_ms,
                        },
                    );
                }
                ControlCommand::Advance => {
                    enqueue_control(&mut state, ClientMessage::Advance);
                }
                ControlCommand::Close => {
                    let _ = tx_bytes.send(Vec::new());
                    return Ok(());
                }
            }
        }

        if last_ping.elapsed() >= Duration::from_millis(500) {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|v| v.as_millis() as i64);
            enqueue_control(&mut state, ClientMessage::Ping { ts });
            last_ping = Instant::now();
        }
        if last_ack_elicit.elapsed() >= Duration::from_millis(200) {
            let _ = conn.send_ack_eliciting();
            last_ack_elicit = Instant::now();
        }

        flush_control(&mut state, &mut conn);
        flush_conn(&mut conn, &socket, &mut send_buf, &last_error);

        loop {
            match socket.recv_from(&mut recv_buf) {
                Ok((len, from)) => {
                    let recv_info = quiche::RecvInfo { from, to: local_addr };
                    if let Err(err) = conn.recv(&mut recv_buf[..len], recv_info) {
                        if err != quiche::Error::Done {
                            set_last_error_if_empty(
                                &last_error,
                                format!("QUIC recv error: {:?}", err),
                            );
                        }
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => {
                    set_last_error_if_empty(&last_error, format!("udp recv error: {}", err));
                    return Err(format!("recv error: {}", err));
                }
            }
        }

        handle_readable(&mut state, &mut conn, &tx_bytes, &last_error)?;
        flush_control(&mut state, &mut conn);
        flush_conn(&mut conn, &socket, &mut send_buf, &last_error);

        let now = Instant::now();
        if let Some(deadline) = next_timeout {
            if now >= deadline {
                conn.on_timeout();
            }
        }
        next_timeout = conn.timeout().map(|t| Instant::now() + t);

        if conn.is_closed() {
            if let Some(detail) = connection_close_detail(&conn) {
                set_last_error(&last_error, detail);
            } else {
                set_last_error_if_empty(&last_error, "QUIC connection closed".to_string());
            }
            let _ = tx_bytes.send(Vec::new());
            return Ok(());
        }

        maybe_log_stats(&conn, &mut last_stats_log, &last_stats);

        let sleep_for = match next_timeout {
            Some(deadline) => deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(5)),
            None => Duration::from_millis(5),
        };
        thread::sleep(sleep_for);
    }
}

fn enqueue_control(state: &mut ClientState, message: ClientMessage<'_>) {
    if let Ok(json) = serde_json::to_string(&message) {
        let mut line = json;
        line.push('\n');
        state.pending_control.push_back(Bytes::from(line));
    }
}

fn flush_control(state: &mut ClientState, conn: &mut quiche::Connection) {
    loop {
        let Some(front) = state.pending_control.front() else { break };
        let data = &front[state.control_offset..];
        match conn.stream_send(state.control_stream_id, data, false) {
            Ok(sent) => {
                if sent == data.len() {
                    state.pending_control.pop_front();
                    state.control_offset = 0;
                } else {
                    state.control_offset += sent;
                    break;
                }
            }
            Err(quiche::Error::Done) => break,
            Err(_) => break,
        }
    }
}

fn flush_conn(
    conn: &mut quiche::Connection,
    socket: &UdpSocket,
    out: &mut [u8],
    last_error: &Arc<Mutex<Option<String>>>,
) {
    loop {
        match conn.send(out) {
            Ok((len, send_info)) => {
                if let Err(err) = socket.send_to(&out[..len], send_info.to) {
                    set_last_error_if_empty(last_error, format!("udp send error: {}", err));
                    break;
                }
            }
            Err(quiche::Error::Done) => break,
            Err(err) => {
                set_last_error_if_empty(last_error, format!("QUIC send error: {:?}", err));
                break;
            }
        }
    }
}

fn handle_readable(
    state: &mut ClientState,
    conn: &mut quiche::Connection,
    tx_bytes: &mpsc::Sender<Vec<u8>>,
    last_error: &Arc<Mutex<Option<String>>>,
) -> Result<(), String> {
    let mut buf = vec![0u8; MAX_UDP_SIZE];
    let readable: Vec<u64> = conn.readable().collect();
    for stream_id in readable {
        loop {
            match conn.stream_recv(stream_id, &mut buf) {
                Ok((len, _fin)) => {
                    if stream_id == state.control_stream_id {
                        handle_control_bytes(state, &buf[..len], tx_bytes, last_error);
                    } else {
                        handle_stream_bytes(state, stream_id, &buf[..len], tx_bytes);
                    }
                }
                Err(quiche::Error::Done) => break,
                Err(err) => {
                    set_last_error_if_empty(last_error, format!("QUIC stream recv error: {:?}", err));
                    break;
                }
            }
        }
    }
    Ok(())
}

fn handle_control_bytes(
    state: &mut ClientState,
    data: &[u8],
    tx: &mpsc::Sender<Vec<u8>>,
    last_error: &Arc<Mutex<Option<String>>>,
) {
    state.control_buf.extend_from_slice(data);
    loop {
        let pos = match state.control_buf.iter().position(|b| *b == b'\n') {
            Some(pos) => pos,
            None => break,
        };
        let line = state.control_buf.drain(..=pos).collect::<Vec<u8>>();
        if line.is_empty() {
            continue;
        }
        let text = match std::str::from_utf8(&line[..line.len().saturating_sub(1)]) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if let Ok(msg) = serde_json::from_str::<ServerMessage>(text) {
            match msg {
                ServerMessage::Stream {
                    track_id,
                    stream_id,
                    role: _,
                    frame_ms: _,
                } => {
                    state.track_streams.insert(track_id.clone(), stream_id);
                    let is_active = state.active_track.as_deref() == Some(track_id.as_str());
                    if is_active {
                        state.active_stream = Some(stream_id);
                    }
                    flush_pending_stream(state, stream_id, &track_id, tx, is_active);
                    if is_active {
                        flush_prefetch_to_output(state, &track_id, tx);
                    }
                }
                ServerMessage::Error { message } => {
                    set_last_error_if_empty(last_error, message);
                    let _ = tx.send(Vec::new());
                }
                _ => {}
            }
        }
    }
}

fn handle_stream_bytes(
    state: &mut ClientState,
    stream_id: u64,
    data: &[u8],
    tx: &mpsc::Sender<Vec<u8>>,
) {
    if Some(stream_id) == state.active_stream {
        let _ = tx.send(data.to_vec());
        return;
    }

    let track_id = match state
        .track_streams
        .iter()
        .find_map(|(k, v)| if *v == stream_id { Some(k.clone()) } else { None })
    {
        Some(value) => value,
        None => {
            buffer_pending_stream(state, stream_id, data);
            return;
        }
    };
    let buffer = state
        .prefetch_buffers
        .entry(track_id.clone())
        .or_insert_with(VecDeque::new);
    let size = state.prefetch_bytes.entry(track_id).or_insert(0);
    if *size > MAX_PREFETCH_BYTES {
        return;
    }
    buffer.push_back(Bytes::from(data.to_vec()));
    *size = size.saturating_add(data.len());
}

fn buffer_pending_stream(state: &mut ClientState, stream_id: u64, data: &[u8]) {
    let size = state.pending_stream_bytes.entry(stream_id).or_insert(0);
    if *size > MAX_PREFETCH_BYTES {
        return;
    }
    let buffer = state
        .pending_streams
        .entry(stream_id)
        .or_insert_with(VecDeque::new);
    buffer.push_back(Bytes::from(data.to_vec()));
    *size = size.saturating_add(data.len());
}

fn flush_pending_stream(
    state: &mut ClientState,
    stream_id: u64,
    track_id: &str,
    tx: &mpsc::Sender<Vec<u8>>,
    active: bool,
) {
    let Some(mut buffer) = state.pending_streams.remove(&stream_id) else {
        return;
    };
    state.pending_stream_bytes.remove(&stream_id);

    if active {
        while let Some(chunk) = buffer.pop_front() {
            let _ = tx.send(chunk.to_vec());
        }
        return;
    }

    let target = state
        .prefetch_buffers
        .entry(track_id.to_string())
        .or_insert_with(VecDeque::new);
    let size = state.prefetch_bytes.entry(track_id.to_string()).or_insert(0);
    while let Some(chunk) = buffer.pop_front() {
        if *size > MAX_PREFETCH_BYTES {
            break;
        }
        *size = size.saturating_add(chunk.len());
        target.push_back(chunk);
    }
}

fn flush_prefetch_to_output(
    state: &mut ClientState,
    track_id: &str,
    tx: &mpsc::Sender<Vec<u8>>,
) {
    if let Some(mut buffer) = state.prefetch_buffers.remove(track_id) {
        while let Some(chunk) = buffer.pop_front() {
            let _ = tx.send(chunk.to_vec());
        }
        state.prefetch_bytes.remove(track_id);
    }
}

fn set_last_error_if_empty(target: &Arc<Mutex<Option<String>>>, message: String) {
    if let Ok(mut guard) = target.lock() {
        if guard.is_none() {
            *guard = Some(message);
        }
    }
}

fn set_last_error(target: &Arc<Mutex<Option<String>>>, message: String) {
    if let Ok(mut guard) = target.lock() {
        *guard = Some(message);
    }
}

fn generate_cid() -> Vec<u8> {
    let mut scid = [0u8; 16];
    rand::rng().fill_bytes(&mut scid);
    scid.to_vec()
}

fn connection_close_detail(conn: &quiche::Connection) -> Option<String> {
    let mut parts = Vec::new();
    if conn.is_timed_out() {
        parts.push("timed_out=true".to_string());
    }
    if let Some(err) = conn.peer_error() {
        parts.push(format!(
            "peer_error(code={}, app={}, reason={})",
            err.error_code,
            err.is_app,
            String::from_utf8_lossy(&err.reason)
        ));
    }
    if let Some(err) = conn.local_error() {
        parts.push(format!(
            "local_error(code={}, app={}, reason={})",
            err.error_code,
            err.is_app,
            String::from_utf8_lossy(&err.reason)
        ));
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!("QUIC connection closed: {}", parts.join(", ")))
    }
}

fn maybe_log_stats(
    conn: &quiche::Connection,
    last_log: &mut Instant,
    last_stats: &Arc<Mutex<Option<String>>>,
) {
    let now = Instant::now();
    if now.duration_since(*last_log) < Duration::from_secs(5) {
        return;
    }
    *last_log = now;
    let stats = conn.stats();
    let path = conn.path_stats().next();
    let message = format!(
        "QUIC client stats: sent_pkts={} recv_pkts={} sent_bytes={} recv_bytes={} established={} timed_out={} path={:?}",
        stats.sent,
        stats.recv,
        stats.sent_bytes,
        stats.recv_bytes,
        conn.is_established(),
        conn.is_timed_out(),
        path,
    );
    if let Ok(mut guard) = last_stats.lock() {
        *guard = Some(message);
    }
}
