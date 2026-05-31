use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MetadataUpdateEvent {
    pub revision: u64,
    pub kind: String,
    pub id: String,
    pub album_id: Option<String>,
    pub artist_id: Option<String>,
    pub updated_at: u64,
}

#[derive(Clone)]
pub struct MetadataEventBus {
    tx: broadcast::Sender<MetadataUpdateEvent>,
    revision: Arc<AtomicU64>,
}

impl MetadataEventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self {
            tx,
            revision: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<MetadataUpdateEvent> {
        self.tx.subscribe()
    }

    pub fn emit_artist(&self, artist_id: impl Into<String>) -> MetadataUpdateEvent {
        let artist_id = artist_id.into();
        self.emit("artist", artist_id.clone(), None, Some(artist_id))
    }

    pub fn emit_album(
        &self,
        album_id: impl Into<String>,
        artist_id: Option<String>,
    ) -> MetadataUpdateEvent {
        let album_id = album_id.into();
        self.emit("album", album_id.clone(), Some(album_id), artist_id)
    }

    pub fn emit_album_artists(
        &self,
        album_id: impl Into<String>,
        artist_id: Option<String>,
    ) -> MetadataUpdateEvent {
        let album_id = album_id.into();
        self.emit("album_artists", album_id.clone(), Some(album_id), artist_id)
    }

    fn emit(
        &self,
        kind: &str,
        id: String,
        album_id: Option<String>,
        artist_id: Option<String>,
    ) -> MetadataUpdateEvent {
        let event = MetadataUpdateEvent {
            revision: self.revision.fetch_add(1, Ordering::Relaxed) + 1,
            kind: kind.to_string(),
            id,
            album_id,
            artist_id,
            updated_at: now_secs(),
        };
        let _ = self.tx.send(event.clone());
        event
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::MetadataEventBus;

    #[test]
    fn emits_incrementing_artist_events() {
        let bus = MetadataEventBus::new(8);

        let first = bus.emit_artist("artist-1");
        let second = bus.emit_album("album-1", Some("artist-1".to_string()));

        assert_eq!(first.revision, 1);
        assert_eq!(first.kind, "artist");
        assert_eq!(first.id, "artist-1");
        assert_eq!(first.artist_id.as_deref(), Some("artist-1"));
        assert_eq!(first.album_id, None);
        assert_eq!(second.revision, 2);
        assert_eq!(second.kind, "album");
        assert_eq!(second.album_id.as_deref(), Some("album-1"));
    }
}
