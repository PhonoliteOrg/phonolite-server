//! Ephemeral playback liveness, independent of listening statistics and consent.
//! Only a session digest and client identity are retained; no audio or history.
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub(crate) const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(15);
// Retain stopped/stale sequences so delayed HTTP requests cannot resurrect them.
const SEQUENCE_RETENTION: Duration = Duration::from_secs(15 * 60);
const MAX_DEVICES: usize = 4096;
const MAX_DEVICES_PER_SESSION: usize = 32;

#[derive(Default)]
pub(crate) struct PlaybackPresence {
    devices: HashMap<(String, String), Device>,
}

struct Device {
    user_id: String,
    sequence: u64,
    playing: bool,
    last_seen: Instant,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PresenceError {
    InvalidClientId,
    Capacity,
}

impl PlaybackPresence {
    pub(crate) fn report(
        &mut self,
        user_id: &str,
        session_digest: &str,
        client_id: &str,
        sequence: u64,
        playing: bool,
        now: Instant,
    ) -> Result<bool, PresenceError> {
        if client_id.is_empty()
            || client_id.len() > 128
            || client_id
                .chars()
                .any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(PresenceError::InvalidClientId);
        }
        self.prune(now);
        let key = (session_digest.to_owned(), client_id.to_owned());
        if let Some(device) = self.devices.get_mut(&key) {
            if sequence <= device.sequence {
                return Ok(false);
            }
            device.sequence = sequence;
            device.playing = playing;
            device.last_seen = now;
            return Ok(true);
        }
        if self.devices.len() >= MAX_DEVICES
            || self
                .devices
                .keys()
                .filter(|(session, _)| session == session_digest)
                .count()
                >= MAX_DEVICES_PER_SESSION
        {
            return Err(PresenceError::Capacity);
        }
        self.devices.insert(
            key,
            Device {
                user_id: user_id.to_owned(),
                sequence,
                playing,
                last_seen: now,
            },
        );
        Ok(true)
    }

    pub(crate) fn playing_devices(&mut self, now: Instant) -> u64 {
        self.prune(now);
        self.devices
            .values()
            .filter(|device| {
                device.playing
                    && now.saturating_duration_since(device.last_seen) < HEARTBEAT_TIMEOUT
            })
            .count() as u64
    }

    pub(crate) fn clear_session(&mut self, digest: &str) {
        self.devices.retain(|(session, _), _| session != digest);
    }

    pub(crate) fn clear_user_except(&mut self, user: &str, except_digest: Option<&str>) {
        self.devices.retain(|(session, _), device| {
            device.user_id != user || Some(session.as_str()) == except_digest
        });
    }

    fn prune(&mut self, now: Instant) {
        self.devices.retain(|_, device| {
            now.saturating_duration_since(device.last_seen) < SEQUENCE_RETENTION
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_devices_under_the_same_account_and_session_independently() {
        let mut presence = PlaybackPresence::default();
        let now = Instant::now();
        for (session, client) in [
            ("token-a", "desktop"),
            ("token-a", "phone"),
            ("token-b", "desktop"),
        ] {
            assert_eq!(
                presence.report("alice", session, client, 1, true, now),
                Ok(true)
            );
        }
        assert_eq!(presence.playing_devices(now), 3);
        presence
            .report("alice", "token-a", "desktop", 2, false, now)
            .unwrap();
        assert_eq!(presence.playing_devices(now), 2);
        presence.clear_session("token-a");
        assert_eq!(presence.playing_devices(now), 1);
    }

    #[test]
    fn heartbeat_expires_without_a_task_and_duplicate_or_delayed_reports_do_not_refresh_it() {
        let mut presence = PlaybackPresence::default();
        let now = Instant::now();
        presence
            .report("alice", "token", "desktop", 1, true, now)
            .unwrap();
        assert_eq!(
            presence.playing_devices(now + HEARTBEAT_TIMEOUT - Duration::from_millis(1)),
            1
        );
        assert_eq!(
            presence.report(
                "alice",
                "token",
                "desktop",
                1,
                true,
                now + Duration::from_secs(10)
            ),
            Ok(false)
        );
        assert_eq!(presence.playing_devices(now + HEARTBEAT_TIMEOUT), 0);
        presence
            .report(
                "alice",
                "token",
                "desktop",
                3,
                false,
                now + Duration::from_secs(16),
            )
            .unwrap();
        assert_eq!(
            presence.report(
                "alice",
                "token",
                "desktop",
                2,
                true,
                now + Duration::from_secs(30)
            ),
            Ok(false)
        );
        assert_eq!(presence.playing_devices(now + Duration::from_secs(30)), 0);
        presence
            .report(
                "alice",
                "token",
                "desktop",
                4,
                true,
                now + Duration::from_secs(31),
            )
            .unwrap();
        assert_eq!(presence.playing_devices(now + Duration::from_secs(31)), 1);
    }

    #[test]
    fn revoking_others_preserves_only_the_current_session_and_other_accounts() {
        let mut presence = PlaybackPresence::default();
        let now = Instant::now();
        for (user, session) in [("alice", "current"), ("alice", "other"), ("bob", "bob")] {
            presence
                .report(user, session, "client", 1, true, now)
                .unwrap();
        }
        presence.clear_user_except("alice", Some("current"));
        assert_eq!(presence.playing_devices(now), 2);
        presence.clear_user_except("alice", None);
        assert_eq!(presence.playing_devices(now), 1);
    }

    #[test]
    fn bounds_client_identity_and_retained_state() {
        let mut presence = PlaybackPresence::default();
        let now = Instant::now();
        for client in ["", " ", "with space", "control\n", &"x".repeat(129)] {
            assert_eq!(
                presence.report("alice", "token", client, 1, true, now),
                Err(PresenceError::InvalidClientId)
            );
        }
        for client in 0..MAX_DEVICES_PER_SESSION {
            presence
                .report("alice", "token", &client.to_string(), 1, false, now)
                .unwrap();
        }
        assert_eq!(
            presence.report("alice", "token", "extra", 1, true, now),
            Err(PresenceError::Capacity)
        );
        assert_eq!(
            presence.report("alice", "token", "extra", 1, true, now + SEQUENCE_RETENTION),
            Ok(true)
        );
    }
}
