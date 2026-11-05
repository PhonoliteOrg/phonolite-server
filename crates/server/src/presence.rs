//! Live Phonolite client transports, separate from persistent login tokens and
//! console authentication. A client is online only while a transport is open
//! and it has been heard from recently.
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

pub(crate) const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Default)]
pub(crate) struct SessionPresence {
    sessions: Arc<Mutex<HashMap<String, LiveSession>>>,
}

struct LiveSession {
    connections: usize,
    last_seen: Instant,
}

pub(crate) struct SessionConnection {
    presence: SessionPresence,
    digest: String,
}

impl SessionPresence {
    pub(crate) fn connect(&self, digest: String) -> SessionConnection {
        let mut sessions = self.sessions.lock();
        let session = sessions.entry(digest.clone()).or_insert(LiveSession {
            connections: 0,
            last_seen: Instant::now(),
        });
        session.connections += 1;
        session.last_seen = Instant::now();
        SessionConnection {
            presence: self.clone(),
            digest,
        }
    }

    pub(crate) fn active_sessions(&self) -> Vec<String> {
        self.sessions
            .lock()
            .iter()
            .filter(|(_, session)| session.last_seen.elapsed() < HEARTBEAT_TIMEOUT)
            .map(|(digest, _)| digest.clone())
            .collect()
    }
}

impl SessionConnection {
    /// Call only on incoming client activity, never on server-side auth checks
    /// or outgoing SSE keepalives: those cannot prove the client is still there.
    pub(crate) fn touch(&self) {
        if let Some(session) = self.presence.sessions.lock().get_mut(&self.digest) {
            session.last_seen = Instant::now();
        }
    }
}

impl Drop for SessionConnection {
    fn drop(&mut self) {
        let mut sessions = self.presence.sessions.lock();
        if let Some(session) = sessions.get_mut(&self.digest) {
            session.connections -= 1;
            if session.connections == 0 {
                sessions.remove(&self.digest);
            }
        }
    }
}

/// Shared by requests on one TLS socket, released when that socket closes.
#[derive(Clone, Default)]
pub(crate) struct HttpConnection {
    session: Arc<Mutex<Option<SessionConnection>>>,
}

impl HttpConnection {
    pub(crate) fn observe(&self, presence: &SessionPresence, digest: String) {
        let mut current = self.session.lock();
        match current.as_ref() {
            Some(session) if session.digest == digest => session.touch(),
            _ => *current = Some(presence.connect(digest)),
        }
    }

    pub(crate) fn close(&self) {
        self.session.lock().take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiple_transports_count_once_until_the_last_connection_closes() {
        let presence = SessionPresence::default();
        let http = presence.connect("device-a".into());
        let quic = presence.connect("device-a".into());
        let other = presence.connect("device-b".into());
        assert_eq!(presence.active_sessions().len(), 2);
        drop(http);
        assert_eq!(presence.active_sessions().len(), 2);
        drop(quic);
        assert_eq!(presence.active_sessions(), ["device-b"]);
        drop(other);
        assert!(presence.active_sessions().is_empty());
        assert!(presence.sessions.lock().is_empty());
    }

    #[test]
    fn silent_connections_time_out_and_incoming_activity_restores_presence() {
        let presence = SessionPresence::default();
        let connection = presence.connect("device".into());
        presence
            .sessions
            .lock()
            .get_mut("device")
            .unwrap()
            .last_seen = Instant::now() - HEARTBEAT_TIMEOUT;
        assert!(presence.active_sessions().is_empty());
        connection.touch();
        assert_eq!(presence.active_sessions(), ["device"]);
    }

    #[test]
    fn http_keepalive_reuses_presence_and_account_switch_releases_previous_session() {
        let presence = SessionPresence::default();
        let socket = HttpConnection::default();
        let request = socket.clone();
        socket.observe(&presence, "first".into());
        request.observe(&presence, "first".into());
        assert_eq!(presence.sessions.lock()["first"].connections, 1);
        request.observe(&presence, "second".into());
        assert_eq!(presence.active_sessions(), ["second"]);
        socket.close();
        assert!(presence.active_sessions().is_empty());
    }
}
