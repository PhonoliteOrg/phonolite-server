//! Direct TLS termination is required: the OPAQUE context uses this connection's exporter.
mod rotation;
pub(crate) use rotation::{activate_pending_rotation, prepare_rotation};

use crate::{
    admin::access::AdminAccess,
    config::{resolve_path, ServerConfig},
    http::request_authority,
    security::TlsConnection,
    security_protocol::EXPORTER_LABEL,
};
use axum::{
    body::Body,
    http::{header, Method, Request, Response, StatusCode},
    Extension, Router,
};
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use rustls::pki_types::PrivateKeyDer;
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    net::IpAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio_rustls::TlsAcceptor;

#[derive(Clone, Copy)]
pub enum Endpoint {
    Api,
    Admin,
}

#[derive(Clone, Copy)]
struct RedirectPolicy {
    port: u16,
    bind_ip: IpAddr,
    endpoint: Endpoint,
}

fn redirect_location<B>(request: &Request<B>, policy: RedirectPolicy) -> Option<String> {
    // Only origin-form navigation is accepted; never trust forwarding headers
    // or use an absolute request target as a redirect destination.
    if request.uri().scheme().is_some()
        || request.uri().authority().is_some()
        || !request.uri().path().starts_with('/')
    {
        return None;
    }
    let (host, port) = request_authority(request, policy.port)?;
    if matches!(policy.endpoint, Endpoint::Admin)
        && !AdminAccess::new(std::net::SocketAddr::new(policy.bind_ip, policy.port))
            .allows(&host, port)
    {
        return None;
    }
    let path = request.uri().path_and_query()?.as_str();
    Some(format!("https://{host}:{port}{path}"))
}

fn http_redirect<B>(request: Request<B>, policy: RedirectPolicy) -> Response<Body> {
    let mut response = Response::new(Body::empty());
    response
        .headers_mut()
        .insert(header::CONNECTION, "close".parse().unwrap());
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    // A redirect cannot undo credentials already sent in plaintext. Do not read
    // bodies, authenticate, or automatically replay writes over another scheme.
    if request.method() != Method::GET && request.method() != Method::HEAD {
        *response.status_mut() = StatusCode::UPGRADE_REQUIRED;
        *response.body_mut() =
            Body::from("HTTPS is required. Send this request directly over HTTPS.\n");
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            "text/plain; charset=utf-8".parse().unwrap(),
        );
        return response;
    }
    match redirect_location(&request, policy).and_then(|location| location.parse().ok()) {
        Some(location) => {
            *response.status_mut() = StatusCode::PERMANENT_REDIRECT;
            response.headers_mut().insert(header::LOCATION, location);
        }
        None => *response.status_mut() = StatusCode::BAD_REQUEST,
    }
    response
}

pub fn identity(
    config: &ServerConfig,
    path: &Path,
    enrolled: bool,
) -> Result<(PathBuf, PathBuf), Box<dyn std::error::Error>> {
    let cert = resolve_path(path, &config.quic_cert_path);
    let key = resolve_path(path, &config.quic_key_path);
    if cert.exists() != key.exists() {
        return Err("incomplete server TLS identity; restore its certificate and key".into());
    }
    if !cert.exists() {
        if enrolled || !config.quic_self_signed {
            return Err("server TLS identity missing; restore security backup".into());
        }
        let pair = rcgen::generate_simple_self_signed(vec![
            "phonolite".into(),
            "localhost".into(),
            "127.0.0.1".into(),
            "::1".into(),
        ])?;
        write_secret(&key, pair.key_pair.serialize_pem().as_bytes())?;
        write_secret(&cert, pair.cert.pem().as_bytes())?;
    }
    harden_file(&key)?;
    Ok((cert, key))
}

pub fn renew(config: &ServerConfig, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let cert = resolve_path(path, &config.quic_cert_path);
    let key = resolve_path(path, &config.quic_key_path);
    let pair = rcgen::KeyPair::from_pem(&fs::read_to_string(&key)?)?;
    let params = rcgen::CertificateParams::new(vec![
        "phonolite".into(),
        "localhost".into(),
        "127.0.0.1".into(),
        "::1".into(),
    ])?;
    let renewed = params.self_signed(&pair)?;
    let replacement = cert.with_extension(format!("{}.renewed.pem", uuid::Uuid::new_v4()));
    write_secret(&replacement, renewed.pem().as_bytes())?;
    // Preserve the old certificate until the replacement is completely written.
    replace_file(&replacement, &cert)?;
    Ok(())
}

pub(super) fn replace_file(replacement: &Path, target: &Path) -> io::Result<()> {
    #[cfg(not(windows))]
    fs::rename(replacement, target)?;
    #[cfg(windows)]
    unsafe {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };
        let source: Vec<u16> = replacement
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
        if MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub fn write_secret(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    harden_file(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

pub fn harden_file(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(windows)]
    unsafe {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::{
            Foundation::LocalFree,
            Security::{
                Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SetFileSecurityW, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
            },
        };
        let sddl: Vec<u16> = "D:P(A;;FA;;;OW)(A;;FA;;;SY)\0".encode_utf16().collect();
        let mut descriptor = std::ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let result = SetFileSecurityW(
            name.as_ptr(),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor,
        );
        let error = io::Error::last_os_error();
        LocalFree(descriptor);
        if result == 0 {
            return Err(error);
        }
    }
    Ok(())
}

pub async fn serve(
    listener: tokio::net::TcpListener,
    app: Router,
    cert: &Path,
    key: &Path,
    endpoint: Endpoint,
) -> Result<(), Box<dyn std::error::Error>> {
    let address = listener.local_addr()?;
    let policy = RedirectPolicy {
        port: address.port(),
        bind_ip: address.ip(),
        endpoint,
    };
    let certificates = rustls_pemfile::certs(&mut io::BufReader::new(fs::File::open(cert)?))
        .collect::<Result<Vec<_>, _>>()?;
    let private_key: PrivateKeyDer =
        rustls_pemfile::private_key(&mut io::BufReader::new(fs::File::open(key)?))?
            .ok_or("missing TLS key")?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(certificates, private_key)?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    config.max_early_data_size = 0;
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let slots = Arc::new(tokio::sync::Semaphore::new(512));
    let shutdown = crate::process_control::shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        let (socket, peer) = tokio::select! { _ = &mut shutdown => break, accepted = listener.accept() => accepted? };
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            continue;
        };
        let acceptor = acceptor.clone();
        let app = app.clone();
        tokio::spawn(async move {
            let _permit = permit;
            serve_connection(socket, acceptor, app, peer.ip(), policy).await;
        });
    }
    Ok(())
}

async fn serve_connection<T>(
    socket: T,
    acceptor: TlsAcceptor,
    app: Router,
    peer: IpAddr,
    policy: RedirectPolicy,
) where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Buffer without consuming the prefix so the TLS ClientHello stays intact.
    // A plain HTTP request gets a standalone redirect service, never the app.
    let mut socket = BufReader::new(socket);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let plain_http = match tokio::time::timeout_at(deadline, socket.fill_buf()).await {
        Ok(Ok(bytes)) if !bytes.is_empty() => bytes[0].is_ascii_alphabetic(),
        _ => return,
    };
    if plain_http {
        let service = hyper::service::service_fn(move |request| async move {
            Ok::<_, std::convert::Infallible>(http_redirect(request, policy))
        });
        let _ = tokio::time::timeout(
            Duration::from_secs(10),
            hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(10))
                .max_headers(32)
                .max_buf_size(8192)
                .keep_alive(false)
                .serve_connection(TokioIo::new(socket), service),
        )
        .await;
        return;
    }
    let stream = match tokio::time::timeout_at(deadline, acceptor.accept(socket)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            tracing::warn!(
                %peer,
                port = policy.port,
                error = %error,
                "TLS handshake failed"
            );
            return;
        }
        Err(_) => {
            tracing::warn!(%peer, port = policy.port, "TLS handshake timed out");
            return;
        }
    };
    let mut binding = [0u8; 32];
    if stream
        .get_ref()
        .1
        .export_keying_material(&mut binding, EXPORTER_LABEL, Some(&[]))
        .is_err()
    {
        return;
    }
    let service = app.layer(Extension(TlsConnection { binding, peer }));
    let _ = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(10))
        .max_buf_size(32768)
        .serve_connection(TokioIo::new(stream), TowerToHyperService::new(service))
        .with_upgrades()
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn policy(endpoint: Endpoint, port: u16) -> RedirectPolicy {
        RedirectPolicy {
            endpoint,
            port,
            bind_ip: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        }
    }

    fn request(method: Method, host: Option<&str>, path: &str) -> Request<()> {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(host) = host {
            builder = builder.header(header::HOST, host);
        }
        builder.body(()).unwrap()
    }

    #[test]
    fn http_navigation_redirects_to_https_preserving_address_path_and_query() {
        for (endpoint, port, host, expected) in [
            (
                Endpoint::Admin,
                3002,
                "localhost:3002",
                "https://localhost:3002/settings?tab=users&next=%2F",
            ),
            (
                Endpoint::Admin,
                3002,
                "127.0.0.1:3002",
                "https://127.0.0.1:3002/settings?tab=users&next=%2F",
            ),
            (
                Endpoint::Admin,
                3002,
                "localhost",
                "https://localhost:3002/settings?tab=users&next=%2F",
            ),
            (
                Endpoint::Api,
                3000,
                "192.168.1.10:3000",
                "https://192.168.1.10:3000/settings?tab=users&next=%2F",
            ),
            (
                Endpoint::Api,
                3000,
                "[::1]:3000",
                "https://[::1]:3000/settings?tab=users&next=%2F",
            ),
            (
                Endpoint::Api,
                3000,
                "[::1]",
                "https://[::1]:3000/settings?tab=users&next=%2F",
            ),
            (
                Endpoint::Api,
                3000,
                "Music.Example:8080",
                "https://music.example:8080/settings?tab=users&next=%2F",
            ),
            (
                Endpoint::Api,
                443,
                "music.example",
                "https://music.example:443/settings?tab=users&next=%2F",
            ),
        ] {
            for method in [Method::GET, Method::HEAD] {
                let response = http_redirect(
                    request(method, Some(host), "/settings?tab=users&next=%2F"),
                    policy(endpoint, port),
                );
                assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT, "{host}");
                assert_eq!(response.headers()[header::LOCATION], expected);
                assert_eq!(response.headers()[header::CONNECTION], "close");
                assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            }
        }
    }

    #[test]
    fn malformed_or_ambiguous_http_destinations_are_rejected() {
        for host in [
            None,
            Some(""),
            Some("user@localhost:3000"),
            Some("localhost:0"),
            Some("localhost:65536"),
            Some("localhost:abc"),
            Some("localhost:"),
            Some("localhost:+3000"),
            Some("localhost:3000:3001"),
            Some("localhost/path"),
            Some("localhost\\path"),
            Some("localhost,evil.example"),
            Some("-bad.example"),
            Some("bad-.example"),
            Some("example.."),
            Some("[not-an-ipv6]:3000"),
        ] {
            let response =
                http_redirect(request(Method::GET, host, "/"), policy(Endpoint::Api, 3000));
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{host:?}");
            assert!(!response.headers().contains_key(header::LOCATION));
        }
        let mut duplicate = request(Method::GET, Some("localhost:3000"), "/");
        duplicate
            .headers_mut()
            .append(header::HOST, "evil.example".parse().unwrap());
        assert_eq!(
            http_redirect(duplicate, policy(Endpoint::Api, 3000)).status(),
            StatusCode::BAD_REQUEST
        );
        for target in ["http://evil.example/", "https://evil.example/", "*"] {
            assert_eq!(
                http_redirect(
                    request(Method::GET, Some("localhost:3000"), target),
                    policy(Endpoint::Api, 3000)
                )
                .status(),
                StatusCode::BAD_REQUEST
            );
        }
    }

    #[test]
    fn admin_redirects_only_target_the_configured_loopback_origin() {
        for host in [
            "evil.example:3002",
            "192.168.1.10:3002",
            "localhost:3000",
            "127.0.0.1:80",
            "[::1]:3002",
        ] {
            let response = http_redirect(
                request(Method::GET, Some(host), "/login"),
                policy(Endpoint::Admin, 3002),
            );
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{host}");
            assert!(!response.headers().contains_key(header::LOCATION));
        }
    }

    #[test]
    fn admin_redirects_follow_the_selected_interface_or_wildcard_binding() {
        for (bind_ip, host, allowed) in [
            ("192.168.1.10", "192.168.1.10:4002", true),
            ("192.168.1.10", "192.168.1.11:4002", false),
            ("192.168.1.10", "localhost:4002", false),
            ("0.0.0.0", "192.168.1.10:4002", true),
            ("0.0.0.0", "127.0.0.1:4002", true),
            ("0.0.0.0", "evil.example:4002", false),
            ("0.0.0.0", "0.0.0.0:4002", false),
            ("fd00::10", "[fd00::10]:4002", true),
            ("fd00::10", "[fd00::11]:4002", false),
            ("::", "[fd00::10]:4002", true),
            ("::", "192.168.1.10:4002", true),
        ] {
            let mut policy = policy(Endpoint::Admin, 4002);
            policy.bind_ip = bind_ip.parse().unwrap();
            let response = http_redirect(
                request(Method::GET, Some(host), "/settings?tab=server"),
                policy,
            );
            assert_eq!(
                response.status(),
                if allowed {
                    StatusCode::PERMANENT_REDIRECT
                } else {
                    StatusCode::BAD_REQUEST
                },
                "bind={bind_ip} host={host}"
            );
            if allowed {
                assert_eq!(
                    response.headers()[header::LOCATION],
                    format!("https://{host}/settings?tab=server")
                );
            }
        }
    }

    #[test]
    fn plaintext_writes_require_https_without_redirecting() {
        for method in [
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
            Method::TRACE,
        ] {
            let response = http_redirect(
                request(method, Some("localhost:3002"), "/login"),
                policy(Endpoint::Admin, 3002),
            );
            assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED);
            assert!(!response.headers().contains_key(header::LOCATION));
        }
    }

    // These transport tests use memory buffers only; they never bind a listener.
    fn memory_tls() -> (TlsAcceptor, tokio_rustls::TlsConnector) {
        let pair = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert = pair.cert.der().clone();
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(pair.key_pair.serialize_der());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut server = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone()], key.into())
            .unwrap();
        server.alpn_protocols = vec![b"http/1.1".to_vec()];
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert).unwrap();
        let mut client = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        client.alpn_protocols = vec![b"http/1.1".to_vec()];
        (
            TlsAcceptor::from(Arc::new(server)),
            tokio_rustls::TlsConnector::from(Arc::new(client)),
        )
    }

    async fn memory_http(raw: &[u8]) -> (String, usize) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (socket, mut client) = tokio::io::duplex(8192);
        let handled = Arc::new(AtomicUsize::new(0));
        let count = handled.clone();
        let app = Router::new().fallback(move || async move {
            count.fetch_add(1, Ordering::SeqCst);
            "private application response"
        });
        let task = tokio::spawn(serve_connection(
            socket,
            memory_tls().0,
            app,
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            policy(Endpoint::Admin, 3002),
        ));
        client.write_all(raw).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        (
            String::from_utf8(response).unwrap(),
            handled.load(Ordering::SeqCst),
        )
    }

    #[tokio::test]
    async fn plaintext_navigation_never_reaches_application_handlers() {
        let (response, handled) =
            memory_http(b"GET /settings?tab=users HTTP/1.1\r\nHost: localhost:3002\r\n\r\n").await;
        assert!(response.starts_with("HTTP/1.1 308"), "{response}");
        assert!(
            response.contains("location: https://localhost:3002/settings?tab=users\r\n"),
            "{response}"
        );
        assert_eq!(handled, 0);
        assert!(!response.contains("private application response"));
    }

    #[tokio::test]
    async fn plaintext_login_is_rejected_without_waiting_for_its_body() {
        let (response, handled) = memory_http(
            b"POST /login HTTP/1.1\r\nHost: localhost:3002\r\nContent-Length: 1000000\r\n\r\n",
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 426"), "{response}");
        assert!(!response.contains("location:"));
        assert_eq!(handled, 0);
    }

    #[tokio::test]
    async fn tls_client_hello_and_exporter_binding_survive_protocol_detection() {
        let (socket, client) = tokio::io::duplex(8192);
        let (acceptor, connector) = memory_tls();
        let app = Router::new().route(
            "/secure",
            axum::routing::get(
                |Extension(connection): Extension<TlsConnection>| async move {
                    assert_eq!(connection.peer, IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
                    format!("secure:{:?}", connection.binding)
                },
            ),
        );
        let task = tokio::spawn(serve_connection(
            socket,
            acceptor,
            app,
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            policy(Endpoint::Api, 3000),
        ));
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let mut stream =
            tokio::time::timeout(Duration::from_secs(2), connector.connect(name, client))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(
            stream.get_ref().1.protocol_version(),
            Some(rustls::ProtocolVersion::TLSv1_3)
        );
        let mut binding = [0u8; 32];
        stream
            .get_ref()
            .1
            .export_keying_material(&mut binding, EXPORTER_LABEL, Some(&[]))
            .unwrap();
        let expected = format!("secure:{binding:?}");
        stream
            .write_all(b"GET /secure HTTP/1.1\r\nHost: localhost:3000\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut response = BufReader::new(stream);
            let mut line = String::new();
            response.read_line(&mut line).await.unwrap();
            assert!(line.starts_with("HTTP/1.1 200"), "{line}");
            let mut length = None;
            loop {
                line.clear();
                assert_ne!(response.read_line(&mut line).await.unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = Some(value.trim().parse::<usize>().unwrap());
                }
            }
            assert_eq!(length, Some(expected.len()));
            let mut body = vec![0; expected.len()];
            response.read_exact(&mut body).await.unwrap();
            assert_eq!(body, expected.as_bytes());
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
    }

    struct TemporaryDirectory(PathBuf);
    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn config() -> (ServerConfig, PathBuf, TemporaryDirectory) {
        let root =
            std::env::temp_dir().join(format!("phonolite-tls-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("config.yaml");
        (ServerConfig::default(), path, TemporaryDirectory(root))
    }
    #[test]
    fn identity_is_persistent_and_enrolled_servers_refuse_missing_material() {
        let (config, path, _directory) = config();
        assert!(identity(&config, &path, true).is_err());
        let (cert, key) = identity(&config, &path, false).unwrap();
        let original = fs::read(&key).unwrap();
        identity(&config, &path, true).unwrap();
        assert_eq!(original, fs::read(&key).unwrap());
        fs::remove_file(cert).unwrap();
        assert!(identity(&config, &path, false).is_err());
        assert_eq!(original, fs::read(&key).unwrap());
    }
    #[test]
    fn renewal_replaces_certificate_without_replacing_private_key() {
        let (config, path, _directory) = config();
        let (cert, key) = identity(&config, &path, false).unwrap();
        let original = fs::read(&key).unwrap();
        renew(&config, &path).unwrap();
        assert_eq!(original, fs::read(&key).unwrap());
        assert!(fs::read_to_string(cert)
            .unwrap()
            .starts_with("-----BEGIN CERTIFICATE-----"));
    }
}
