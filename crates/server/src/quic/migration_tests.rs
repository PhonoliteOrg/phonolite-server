//! Real QUIC packets exchanged in memory: no sockets or background listeners.

use super::*;

struct Identity(PathBuf);

impl Identity {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("phonolite-migration-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        let identity = rcgen::generate_simple_self_signed(vec!["quic.tech".into()]).unwrap();
        std::fs::write(path.join("cert.pem"), identity.cert.pem()).unwrap();
        std::fs::write(path.join("key.pem"), identity.key_pair.serialize_pem()).unwrap();
        Self(path)
    }

    fn pipe(&self, allow_server_migration: bool) -> quiche::test_utils::Pipe {
        let mut client =
            build_quic_config(&self.0.join("cert.pem"), &self.0.join("key.pem"), true).unwrap();
        let mut server = build_quic_config(
            &self.0.join("cert.pem"),
            &self.0.join("key.pem"),
            allow_server_migration,
        )
        .unwrap();
        let mut pipe =
            quiche::test_utils::Pipe::with_client_and_server_config(&mut client, &mut server).unwrap();
        pipe.handshake().unwrap();
        pipe.advance().unwrap();
        pipe
    }
}

impl Drop for Identity {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Harness {
    client: quiche::Connection,
    server: ClientConn,
    routes: HashMap<Vec<u8>, Vec<u8>>,
    owner: Vec<u8>,
}

impl Harness {
    fn new(pipe: quiche::test_utils::Pipe) -> Self {
        let owner = pipe.server.source_id().as_ref().to_vec();
        let mut session = SessionState::new();
        session.authed = true;
        session.auth_token = Some(zeroize::Zeroizing::new("test-token".into()));
        session.active_track = Some("current-track".into());
        session.open_request_id = Some(41);
        session.queue.push_back("current-track".into());
        Self {
            client: pipe.client,
            server: ClientConn {
                conn: pipe.server,
                session,
                timeout_at: None,
                pending_datagram: None,
            },
            routes: HashMap::from([(owner.clone(), owner.clone())]),
            owner,
        }
    }

    fn pump(&mut self) {
        for _ in 0..64 {
            refresh_conn_ids(&mut self.routes, &mut self.server.conn, &self.owner, true);
            let mut changed = false;
            if let Ok(flight) = quiche::test_utils::emit_flight(&mut self.client) {
                for (mut packet, info) in flight {
                    changed = true;
                    let header =
                        quiche::Header::from_slice(&mut packet, SERVER_CONN_ID_LEN).unwrap();
                    assert_eq!(
                        self.routes.get(header.dcid.as_ref()),
                        Some(&self.owner),
                        "a newly announced CID must already dispatch to the existing ClientConn"
                    );
                    self.server
                        .conn
                        .recv(
                            &mut packet,
                            quiche::RecvInfo {
                                from: info.from,
                                to: info.to,
                            },
                        )
                        .unwrap();
                    refresh_conn_ids(&mut self.routes, &mut self.server.conn, &self.owner, true);
                }
            }
            if let Ok(flight) = quiche::test_utils::emit_flight(&mut self.server.conn) {
                for (mut packet, info) in flight {
                    changed = true;
                    self.client
                        .recv(
                            &mut packet,
                            quiche::RecvInfo {
                                from: info.from,
                                to: info.to,
                            },
                        )
                        .unwrap();
                }
            }
            if !changed {
                return;
            }
        }
        panic!("in-memory QUIC exchange failed to settle");
    }

    fn supply_client_ids(&mut self) {
        for _ in 0..self.client.scids_left() {
            self.client
                .new_scid(
                    &quiche::ConnectionId::from_ref(&generate_cid()),
                    rand::random(),
                    false,
                )
                .unwrap();
        }
    }
}

#[test]
fn migration_permission_is_disabled_by_default_and_old_clients_remain_compatible() {
    let identity = Identity::new();
    let mut pipe = identity.pipe(false);
    assert!(
        pipe.client
            .peer_transport_params()
            .unwrap()
            .disable_active_migration
    );
    let mut routes = HashMap::new();
    let owner = pipe.server.source_id().as_ref().to_vec();
    refresh_conn_ids(&mut routes, &mut pipe.server, &owner, false);
    assert_eq!(pipe.server.active_scids(), 1);
    pipe.server
        .stream_send(3, b"ordinary playback", true)
        .unwrap();
    pipe.advance().unwrap();
    let mut bytes = [0; 64];
    let (length, finished) = pipe.client.stream_recv(3, &mut bytes).unwrap();
    assert_eq!(&bytes[..length], b"ordinary playback");
    assert!(finished);
}

#[test]
fn migration_preserves_stream_auth_and_routing_through_cid_retirement() {
    let identity = Identity::new();
    let mut harness = Harness::new(identity.pipe(true));
    assert!(
        !harness
            .client
            .peer_transport_params()
            .unwrap()
            .disable_active_migration
    );
    harness.supply_client_ids();
    harness.pump();
    assert!(harness.client.available_dcids() > 0);
    harness
        .server
        .conn
        .stream_send(3, b"before", false)
        .unwrap();
    harness.pump();
    let mut bytes = [0; 64];
    assert_eq!(
        harness.client.stream_recv(3, &mut bytes).unwrap(),
        (6, false)
    );
    assert_eq!(&bytes[..6], b"before");

    let original_server_cid = harness.client.destination_id().as_ref().to_vec();
    let new_local = "127.0.0.2:1234".parse().unwrap();
    let peer = quiche::test_utils::Pipe::server_addr();
    harness.client.probe_path(new_local, peer).unwrap();
    harness.pump();
    assert!(harness.client.is_path_validated(new_local, peer).unwrap());
    harness.client.migrate_source(new_local).unwrap();
    harness.client.stream_send(2, b"confirmed", true).unwrap();
    harness.pump();
    assert!(harness
        .server
        .conn
        .path_stats()
        .any(|path| path.active && path.peer_addr == new_local));

    harness.server.conn.stream_send(3, b"after", true).unwrap();
    harness.pump();
    assert_eq!(
        harness.client.stream_recv(3, &mut bytes).unwrap(),
        (5, true)
    );
    assert_eq!(&bytes[..5], b"after");
    assert!(harness.server.session.authed);
    assert_eq!(
        harness.server.session.active_track.as_deref(),
        Some("current-track")
    );
    assert_eq!(harness.server.session.open_request_id, Some(41));
    assert_eq!(
        harness
            .server
            .session
            .auth_token
            .as_deref()
            .map(|token| token.as_str()),
        Some("test-token")
    );

    harness.client.retire_dcid(0).unwrap();
    harness.pump();
    assert!(!harness.routes.contains_key(&original_server_cid));
    assert!(harness.routes.values().all(|owner| owner == &harness.owner));
    assert_eq!(
        harness.server.conn.active_scids(),
        MIGRATION_CONNECTION_ID_LIMIT as usize
    );
}

#[test]
fn missing_peer_ids_prevents_migration_without_losing_the_existing_stream() {
    let identity = Identity::new();
    let mut harness = Harness::new(identity.pipe(true));
    harness.pump();
    // This old-style client has never supplied a spare local source CID.
    assert_eq!(
        harness.client.probe_path(
            "127.0.0.2:1234".parse().unwrap(),
            quiche::test_utils::Pipe::server_addr()
        ),
        Err(quiche::Error::OutOfIdentifiers)
    );
    harness
        .server
        .conn
        .stream_send(3, b"still playing", true)
        .unwrap();
    harness.pump();
    let mut bytes = [0; 64];
    assert_eq!(
        harness.client.stream_recv(3, &mut bytes).unwrap(),
        (13, true)
    );
    assert_eq!(&bytes[..13], b"still playing");
}
