//! Recent quality belongs to an authenticated listener/path, not a track.
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use crate::transcode::TranscodeQuality;

const LIFETIME: Duration = Duration::from_secs(300);
const MAX_ENTRIES: usize = 1024;

#[derive(Clone, Eq, Hash, PartialEq)]
pub(super) struct ListenerPath {
    user: String,
    session: [u8; 32],
    address: IpAddr,
}

impl ListenerPath {
    pub fn new(user: &str, token: &str, address: IpAddr) -> Self {
        Self {
            user: user.to_owned(),
            session: *blake3::hash(token.as_bytes()).as_bytes(),
            address,
        }
    }
}

#[derive(Default)]
pub(super) struct QualityHistory {
    entries: HashMap<ListenerPath, (TranscodeQuality, Instant)>,
}

impl QualityHistory {
    pub fn quality(&mut self, key: &ListenerPath, now: Instant) -> Option<TranscodeQuality> {
        let (quality, updated) = *self.entries.get(key)?;
        if now.saturating_duration_since(updated) >= LIFETIME {
            self.entries.remove(key);
            return None;
        }
        Some(quality)
    }

    pub fn remember(&mut self, key: ListenerPath, quality: TranscodeQuality, now: Instant) {
        self.entries
            .retain(|_, (_, updated)| now.saturating_duration_since(*updated) < LIFETIME);
        if !self.entries.contains_key(&key) && self.entries.len() >= MAX_ENTRIES {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, updated))| *updated)
                .map(|(key, _)| key.clone())
            {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(key, (quality, now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_and_new_connections_share_quality_only_for_the_same_listener_path() {
        let now = Instant::now();
        let mut history = QualityHistory::default();
        let key = ListenerPath::new("alice", "device-a", "192.0.2.1".parse().unwrap());
        history.remember(key.clone(), TranscodeQuality::High, now);
        assert_eq!(history.quality(&key, now), Some(TranscodeQuality::High));
        for other in [
            ListenerPath::new("bob", "device-a", "192.0.2.1".parse().unwrap()),
            ListenerPath::new("alice", "device-b", "192.0.2.1".parse().unwrap()),
            ListenerPath::new("alice", "device-a", "192.0.2.2".parse().unwrap()),
        ] {
            assert!(history.quality(&other, now).is_none());
        }
        history.remember(
            key.clone(),
            TranscodeQuality::Low,
            now + Duration::from_secs(10),
        );
        assert_eq!(
            history.quality(&key, now + Duration::from_secs(11)),
            Some(TranscodeQuality::Low)
        );
        assert!(history
            .quality(&key, now + Duration::from_secs(310))
            .is_none());
    }

    #[test]
    fn history_is_bounded_and_evicts_oldest_observation() {
        let now = Instant::now();
        let mut history = QualityHistory::default();
        let address = "192.0.2.1".parse().unwrap();
        for index in 0..=MAX_ENTRIES {
            history.remember(
                ListenerPath::new(&index.to_string(), "device", address),
                TranscodeQuality::Medium,
                now + Duration::from_millis(index as u64),
            );
        }
        assert_eq!(history.entries.len(), MAX_ENTRIES);
        assert!(history
            .quality(&ListenerPath::new("0", "device", address), now)
            .is_none());
    }
}
