use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use redb::{
    CommitError, Database, DatabaseError, ReadableTable, StorageError, TableDefinition, TableError,
    TransactionError,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::state::Playlist;

#[path = "account_data.rs"]
mod account_data;
pub use account_data::{LastTrack, ShufflePreset, ShufflePresets};
#[path = "playback_data.rs"]
mod playback_data;
pub use playback_data::{
    PlaybackCheckpoint, PlaybackDeckImport, PlaybackSnapshot, PlaybackSource, PlaybackStoreError,
    PlaybackWrite,
};

// The original, unscoped tables deliberately remain untouched. Their records do
// not identify an owner, so assigning them to any account would disclose data.
const PLAYLISTS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("user_playlists_v2");
const PLAYLIST_IMAGES_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("user_playlist_images_v2");
const LIKES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("user_likes_v2");
const PLAYBACK_SETTINGS_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("user_playback_settings_v2");
// Revisions live outside bincode playlist records so existing databases retain
// their exact serialization format across upgrades.
const COLLECTION_REVISIONS: TableDefinition<&str, u64> =
    TableDefinition::new("user_collection_revisions_v1");
const PLAYLIST_REVISIONS: TableDefinition<&str, u64> =
    TableDefinition::new("user_playlist_revisions_v1");

#[derive(Clone, Debug, Serialize)]
pub struct VersionedPlaylist {
    #[serde(flatten)]
    pub playlist: Playlist,
    pub revision: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct CollectionLikeState {
    pub track_id: String,
    pub liked: bool,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct CollectionSnapshot {
    pub user_id: String,
    pub revision: u64,
    pub likes: Vec<CollectionLikeState>,
    pub playlists: Vec<VersionedPlaylist>,
}

/// Owns a subscriber's channel lifetime. The store retains only weak channel
/// references, so disconnected accounts do not accumulate queues in memory.
pub struct CollectionSubscription {
    pub receiver: broadcast::Receiver<u64>,
    _sender: Arc<broadcast::Sender<u64>>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PlaybackSettings {
    pub repeat_mode: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LikeState {
    #[serde(default = "default_liked")]
    pub liked: bool,
    #[serde(default)]
    pub updated_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlaylistImage {
    pub content_type: String,
    pub bytes: Vec<u8>,
    pub updated_at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct LegacyPlaylist {
    id: String,
    name: String,
    #[serde(default)]
    track_ids: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PlaylistWithImageRefLegacy {
    id: String,
    name: String,
    #[serde(default)]
    track_ids: Vec<String>,
    #[serde(default)]
    image_ref: Option<String>,
}

impl LikeState {
    pub fn with_state(liked: bool) -> Self {
        Self {
            liked,
            updated_at: now_millis(),
        }
    }
}

#[derive(Clone)]
pub struct UserDataStore {
    db: Arc<Database>,
    subscribers: Arc<Mutex<HashMap<String, Weak<broadcast::Sender<u64>>>>>,
}

impl UserDataStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self {
            db,
            subscribers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn init_tables(&self) -> Result<(), UserDataError> {
        let write_txn = ::library::begin_write(&self.db)?;
        {
            let _ = write_txn.open_table(PLAYLISTS_TABLE)?;
            let _ = write_txn.open_table(PLAYLIST_IMAGES_TABLE)?;
            let _ = write_txn.open_table(LIKES_TABLE)?;
            let _ = write_txn.open_table(PLAYBACK_SETTINGS_TABLE)?;
            let _ = write_txn.open_table(COLLECTION_REVISIONS)?;
            let _ = write_txn.open_table(PLAYLIST_REVISIONS)?;
            account_data::init_tables(&write_txn)?;
            playback_data::init_tables(&write_txn)?;
        }
        write_txn.commit()?;
        let read_txn = self.db.begin_read()?;
        let mut legacy_rows = 0;
        for table_name in ["playlists", "playlist_images", "likes", "playback_settings"] {
            match read_txn.open_table(TableDefinition::<&str, &[u8]>::new(table_name)) {
                Ok(table) => legacy_rows += table.len()?,
                Err(TableError::TableDoesNotExist(_)) => {}
                Err(err) => return Err(err.into()),
            }
        }
        if legacy_rows > 0 {
            tracing::warn!(
                legacy_rows,
                "Legacy shared personalization data has no account owner; preserved unassigned and inaccessible to accounts. Ownership must be explicitly resolved before any migration."
            );
        }
        Ok(())
    }

    pub fn subscribe_collections(&self, user_id: &str) -> CollectionSubscription {
        let mut subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        subscribers.retain(|_, sender| sender.strong_count() > 0);
        let sender = subscribers
            .get(user_id)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let (sender, _) = broadcast::channel(64);
                let sender = Arc::new(sender);
                subscribers.insert(user_id.to_owned(), Arc::downgrade(&sender));
                sender
            });
        CollectionSubscription {
            receiver: sender.subscribe(),
            _sender: sender,
        }
    }

    pub fn collection_revision(&self, user_id: &str) -> Result<u64, UserDataError> {
        let txn = self.db.begin_read()?;
        let revision = match txn.open_table(COLLECTION_REVISIONS) {
            Ok(table) => Ok(table.get(user_id)?.map(|value| value.value()).unwrap_or(0)),
            Err(TableError::TableDoesNotExist(_)) => Ok(0),
            Err(err) => Err(err.into()),
        };
        revision
    }

    /// All personal state and its revision come from a single database snapshot.
    pub fn collection_snapshot(&self, user_id: &str) -> Result<CollectionSnapshot, UserDataError> {
        let txn = self.db.begin_read()?;
        let revision = txn
            .open_table(COLLECTION_REVISIONS)?
            .get(user_id)?
            .map(|value| value.value())
            .unwrap_or(0);
        let revisions = txn.open_table(PLAYLIST_REVISIONS)?;
        let playlists_table = txn.open_table(PLAYLISTS_TABLE)?;
        let likes_table = txn.open_table(LIKES_TABLE)?;
        let prefix = user_prefix(user_id);
        let mut playlists = Vec::new();
        let mut likes = Vec::new();
        for entry in playlists_table.range(prefix.as_str()..)? {
            let (key, value) = entry?;
            if !key.value().starts_with(&prefix) {
                break;
            }
            playlists.push(VersionedPlaylist {
                playlist: decode_playlist(value.value())?,
                revision: revisions
                    .get(key.value())?
                    .map(|value| value.value())
                    .unwrap_or(0),
            });
        }
        for entry in likes_table.range(prefix.as_str()..)? {
            let (key, value) = entry?;
            let Some(track_id) = key.value().strip_prefix(&prefix) else {
                break;
            };
            let state = decode_like_state(value.value());
            likes.push(CollectionLikeState {
                track_id: track_id.to_owned(),
                liked: state.liked,
                updated_at: state.updated_at,
            });
        }
        Ok(CollectionSnapshot {
            user_id: user_id.to_owned(),
            revision,
            likes,
            playlists,
        })
    }

    pub fn versioned_playlist(
        &self,
        user_id: &str,
        playlist_id: &str,
    ) -> Result<Option<VersionedPlaylist>, UserDataError> {
        let txn = self.db.begin_read()?;
        let key = user_key(user_id, playlist_id);
        let table = txn.open_table(PLAYLISTS_TABLE)?;
        let playlist = match table.get(key.as_str())? {
            Some(value) => decode_playlist(value.value())?,
            None => return Ok(None),
        };
        let revisions = txn.open_table(PLAYLIST_REVISIONS)?;
        let revision = revisions
            .get(key.as_str())?
            .map(|value| value.value())
            .unwrap_or(0);
        Ok(Some(VersionedPlaylist { playlist, revision }))
    }

    fn notify_account_change(&self, user_id: &str, revision: u64) {
        let sender = self
            .subscribers
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .get(user_id)
            .and_then(Weak::upgrade);
        if let Some(sender) = sender {
            let _ = sender.send(revision);
        }
    }

    fn commit_collections(
        &self,
        txn: ::library::AdmittedWriteTransaction<'_>,
        user_id: &str,
        playlist_id: Option<&str>,
    ) -> Result<(), UserDataError> {
        let revision = {
            let mut revisions = txn.open_table(COLLECTION_REVISIONS)?;
            let current = revisions
                .get(user_id)?
                .map(|value| value.value())
                .unwrap_or(0);
            let revision = current
                .checked_add(1)
                .ok_or(UserDataError::RevisionExhausted)?;
            revisions.insert(user_id, revision)?;
            revision
        };
        if let Some(playlist_id) = playlist_id {
            txn.open_table(PLAYLIST_REVISIONS)?
                .insert(user_key(user_id, playlist_id).as_str(), revision)?;
        }
        txn.commit()?;
        // Runs in the committing store operation, including when its requesting
        // HTTP future is dropped. SSE revision checks also repair notification loss.
        let sender = self
            .subscribers
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .get(user_id)
            .and_then(Weak::upgrade);
        if let Some(sender) = sender {
            let _ = sender.send(revision);
        }
        Ok(())
    }

    pub fn list_playlists(&self, user_id: &str) -> Result<Vec<Playlist>, UserDataError> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(PLAYLISTS_TABLE) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let mut items = Vec::new();
        let prefix = user_prefix(user_id);
        for entry in table.range(prefix.as_str()..)? {
            let entry = entry?;
            if !entry.0.value().starts_with(&prefix) {
                break;
            }
            let playlist = decode_playlist(entry.1.value())?;
            items.push(playlist);
        }
        Ok(items)
    }

    pub fn get_playlist(
        &self,
        user_id: &str,
        playlist_id: &str,
    ) -> Result<Option<Playlist>, UserDataError> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(PLAYLISTS_TABLE) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let key = user_key(user_id, playlist_id);
        let playlist = match table.get(key.as_str())? {
            Some(value) => Some(decode_playlist(value.value())?),
            None => None,
        };
        Ok(playlist)
    }

    pub fn create_playlist(
        &self,
        user_id: &str,
        name: String,
        description: Option<String>,
        track_ids: Vec<String>,
    ) -> Result<Playlist, UserDataError> {
        let playlist = Playlist {
            id: Uuid::new_v4().to_string(),
            name,
            track_ids,
            description: normalize_optional_text(description),
            image_ref: None,
        };
        let write_txn = ::library::begin_write(&self.db)?;
        {
            let mut table = write_txn.open_table(PLAYLISTS_TABLE)?;
            let bytes = encode_value(&playlist)?;
            let key = user_key(user_id, &playlist.id);
            table.insert(key.as_str(), bytes.as_slice())?;
        }
        self.commit_collections(write_txn, user_id, Some(&playlist.id))?;
        Ok(playlist)
    }

    pub fn update_playlist(
        &self,
        user_id: &str,
        playlist_id: &str,
        name: Option<String>,
        description: Option<String>,
        track_ids: Option<Vec<String>>,
    ) -> Result<Option<Playlist>, UserDataError> {
        self.update_playlist_inner(
            user_id,
            playlist_id,
            name,
            description,
            track_ids,
            None,
            false,
        )
    }

    pub fn update_playlist_checked(
        &self,
        user_id: &str,
        playlist_id: &str,
        name: Option<String>,
        description: Option<String>,
        track_ids: Option<Vec<String>>,
        expected_revision: Option<u64>,
    ) -> Result<Option<Playlist>, UserDataError> {
        self.update_playlist_inner(
            user_id,
            playlist_id,
            name,
            description,
            track_ids,
            expected_revision,
            true,
        )
    }

    fn update_playlist_inner(
        &self,
        user_id: &str,
        playlist_id: &str,
        name: Option<String>,
        description: Option<String>,
        track_ids: Option<Vec<String>>,
        expected_revision: Option<u64>,
        require_precondition: bool,
    ) -> Result<Option<Playlist>, UserDataError> {
        let write_txn = ::library::begin_write(&self.db)?;
        let updated = {
            let mut table = match write_txn.open_table(PLAYLISTS_TABLE) {
                Ok(table) => table,
                Err(TableError::TableDoesNotExist(_)) => return Ok(None),
                Err(err) => return Err(err.into()),
            };
            let key = user_key(user_id, playlist_id);
            let mut playlist = match table.get(key.as_str())? {
                Some(value) => decode_playlist(value.value())?,
                None => return Ok(None),
            };
            if require_precondition && track_ids.is_some() && expected_revision.is_none() {
                return Err(UserDataError::PreconditionRequired);
            }
            if let Some(expected) = expected_revision {
                let current = write_txn
                    .open_table(PLAYLIST_REVISIONS)?
                    .get(key.as_str())?
                    .map(|value| value.value())
                    .unwrap_or(0);
                if expected != current {
                    return Err(UserDataError::PlaylistConflict);
                }
            }
            let previous = encode_value(&playlist)?;
            if let Some(name) = name {
                playlist.name = name;
            }
            if let Some(description) = description {
                playlist.description = normalize_optional_text(Some(description));
            }
            if let Some(track_ids) = track_ids {
                playlist.track_ids = track_ids;
            }
            let bytes = encode_value(&playlist)?;
            if previous == bytes {
                return Ok(Some(playlist));
            }
            table.insert(key.as_str(), bytes.as_slice())?;
            playlist
        };
        self.commit_collections(write_txn, user_id, Some(playlist_id))?;
        Ok(Some(updated))
    }

    /// Adds/removes tracks against the current committed playlist, without
    /// replacing another client's edits. Repeating an operation is a no-op.
    pub fn change_playlist_tracks(
        &self,
        user_id: &str,
        playlist_id: &str,
        track_ids: Vec<String>,
        add: bool,
    ) -> Result<Option<Playlist>, UserDataError> {
        let txn = ::library::begin_write(&self.db)?;
        let key = user_key(user_id, playlist_id);
        let updated = {
            let mut table = txn.open_table(PLAYLISTS_TABLE)?;
            let mut playlist = match table.get(key.as_str())? {
                Some(value) => decode_playlist(value.value())?,
                None => return Ok(None),
            };
            let previous = playlist.track_ids.clone();
            if add {
                let mut existing: HashSet<String> = playlist.track_ids.iter().cloned().collect();
                for id in track_ids {
                    if !id.trim().is_empty() && existing.insert(id.clone()) {
                        playlist.track_ids.push(id);
                    }
                }
            } else {
                let removed: HashSet<String> = track_ids.into_iter().collect();
                playlist.track_ids.retain(|id| !removed.contains(id));
            }
            if previous == playlist.track_ids {
                return Ok(Some(playlist));
            }
            table.insert(key.as_str(), encode_value(&playlist)?.as_slice())?;
            playlist
        };
        self.commit_collections(txn, user_id, Some(playlist_id))?;
        Ok(Some(updated))
    }

    pub fn get_playlist_image(
        &self,
        user_id: &str,
        playlist_id: &str,
    ) -> Result<Option<PlaylistImage>, UserDataError> {
        let read_txn = self.db.begin_read()?;
        let key = user_key(user_id, playlist_id);
        let playlists = match read_txn.open_table(PLAYLISTS_TABLE) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        if playlists.get(key.as_str())?.is_none() {
            return Ok(None);
        }
        let table = match read_txn.open_table(PLAYLIST_IMAGES_TABLE) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let image = match table.get(key.as_str())? {
            Some(value) => Some(decode_value(value.value())?),
            None => None,
        };
        Ok(image)
    }

    pub fn set_playlist_image(
        &self,
        user_id: &str,
        playlist_id: &str,
        content_type: String,
        bytes: Vec<u8>,
    ) -> Result<Option<Playlist>, UserDataError> {
        let updated_at = now_millis();
        let image = PlaylistImage {
            content_type,
            bytes,
            updated_at,
        };
        let write_txn = ::library::begin_write(&self.db)?;
        let key = user_key(user_id, playlist_id);
        let updated = {
            let playlist = {
                let mut table = match write_txn.open_table(PLAYLISTS_TABLE) {
                    Ok(table) => table,
                    Err(TableError::TableDoesNotExist(_)) => return Ok(None),
                    Err(err) => return Err(err.into()),
                };
                let mut playlist = match table.get(key.as_str())? {
                    Some(value) => decode_playlist(value.value())?,
                    None => return Ok(None),
                };
                playlist.image_ref = Some(format!("{}-{}", updated_at, Uuid::new_v4()));
                let playlist_bytes = encode_value(&playlist)?;
                table.insert(key.as_str(), playlist_bytes.as_slice())?;
                playlist
            };
            {
                let mut table = write_txn.open_table(PLAYLIST_IMAGES_TABLE)?;
                let image_bytes = encode_value(&image)?;
                table.insert(key.as_str(), image_bytes.as_slice())?;
            }
            playlist
        };
        self.commit_collections(write_txn, user_id, Some(playlist_id))?;
        Ok(Some(updated))
    }

    pub fn clear_playlist_image(
        &self,
        user_id: &str,
        playlist_id: &str,
    ) -> Result<Option<Playlist>, UserDataError> {
        let write_txn = ::library::begin_write(&self.db)?;
        let key = user_key(user_id, playlist_id);
        let updated = {
            let playlist = {
                let mut table = match write_txn.open_table(PLAYLISTS_TABLE) {
                    Ok(table) => table,
                    Err(TableError::TableDoesNotExist(_)) => return Ok(None),
                    Err(err) => return Err(err.into()),
                };
                let mut playlist = match table.get(key.as_str())? {
                    Some(value) => decode_playlist(value.value())?,
                    None => return Ok(None),
                };
                if playlist.image_ref.is_none() {
                    return Ok(Some(playlist));
                }
                playlist.image_ref = None;
                let playlist_bytes = encode_value(&playlist)?;
                table.insert(key.as_str(), playlist_bytes.as_slice())?;
                playlist
            };
            {
                let mut table = write_txn.open_table(PLAYLIST_IMAGES_TABLE)?;
                let _ = table.remove(key.as_str())?;
            }
            playlist
        };
        self.commit_collections(write_txn, user_id, Some(playlist_id))?;
        Ok(Some(updated))
    }

    pub fn delete_playlist(&self, user_id: &str, playlist_id: &str) -> Result<bool, UserDataError> {
        let write_txn = ::library::begin_write(&self.db)?;
        let key = user_key(user_id, playlist_id);
        let deleted = {
            let removed = {
                let mut table = match write_txn.open_table(PLAYLISTS_TABLE) {
                    Ok(table) => table,
                    Err(TableError::TableDoesNotExist(_)) => return Ok(false),
                    Err(err) => return Err(err.into()),
                };
                let removed = table.remove(key.as_str())?.is_some();
                removed
            };
            if removed {
                let mut images = write_txn.open_table(PLAYLIST_IMAGES_TABLE)?;
                let _ = images.remove(key.as_str())?;
            }
            removed
        };
        if !deleted {
            return Ok(false);
        }
        self.commit_collections(write_txn, user_id, Some(playlist_id))?;
        Ok(deleted)
    }

    pub fn list_likes(&self, user_id: &str) -> Result<Vec<String>, UserDataError> {
        let mut liked_states: Vec<_> = self
            .list_like_states(user_id)?
            .into_iter()
            .filter(|(_, state)| state.liked)
            .collect();
        liked_states.sort_by(|left, right| {
            right
                .1
                .updated_at
                .cmp(&left.1.updated_at)
                .then_with(|| left.0.cmp(&right.0))
        });
        Ok(liked_states
            .into_iter()
            .map(|(track_id, _)| track_id)
            .collect())
    }

    pub fn list_like_states(
        &self,
        user_id: &str,
    ) -> Result<Vec<(String, LikeState)>, UserDataError> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(LIKES_TABLE) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let mut states = Vec::new();
        let prefix = user_prefix(user_id);
        for entry in table.range(prefix.as_str()..)? {
            let entry = entry?;
            let Some(track_id) = entry.0.value().strip_prefix(&prefix) else {
                break;
            };
            states.push((track_id.to_string(), decode_like_state(entry.1.value())));
        }
        Ok(states)
    }

    /// Personal flags for a small playback window. Read only the requested like
    /// keys and retain only requested playlist memberships, rather than copying
    /// an account's entire collection into every shuffle response.
    pub fn track_membership(
        &self,
        user_id: &str,
        track_ids: &[String],
    ) -> Result<(HashSet<String>, HashSet<String>), UserDataError> {
        let wanted: HashSet<&str> = track_ids.iter().map(String::as_str).collect();
        let mut liked = HashSet::new();
        let mut in_playlists = HashSet::new();
        if wanted.is_empty() {
            return Ok((liked, in_playlists));
        }
        let txn = self.db.begin_read()?;
        match txn.open_table(LIKES_TABLE) {
            Ok(table) => {
                for id in &wanted {
                    let key = user_key(user_id, id);
                    if let Some(value) = table.get(key.as_str())? {
                        if decode_like_state(value.value()).liked {
                            liked.insert((*id).to_string());
                        }
                    }
                }
            }
            Err(TableError::TableDoesNotExist(_)) => {}
            Err(err) => return Err(err.into()),
        }
        match txn.open_table(PLAYLISTS_TABLE) {
            Ok(table) => {
                let prefix = user_prefix(user_id);
                for entry in table.range(prefix.as_str()..)? {
                    let (key, value) = entry?;
                    if !key.value().starts_with(&prefix) {
                        break;
                    }
                    for id in decode_playlist(value.value())?.track_ids {
                        if wanted.contains(id.as_str()) {
                            in_playlists.insert(id);
                        }
                    }
                    if in_playlists.len() == wanted.len() {
                        break;
                    }
                }
            }
            Err(TableError::TableDoesNotExist(_)) => {}
            Err(err) => return Err(err.into()),
        }
        Ok((liked, in_playlists))
    }

    pub fn add_like(&self, user_id: &str, track_id: &str) -> Result<(), UserDataError> {
        self.set_like_state(user_id, track_id, true).map(|_| ())
    }

    pub fn remove_like(&self, user_id: &str, track_id: &str) -> Result<(), UserDataError> {
        self.set_like_state(user_id, track_id, false).map(|_| ())
    }

    pub fn set_like_state(
        &self,
        user_id: &str,
        track_id: &str,
        liked: bool,
    ) -> Result<LikeState, UserDataError> {
        self.set_like_state_with_updated_at(user_id, track_id, liked, None)
    }

    pub fn set_like_state_with_updated_at(
        &self,
        user_id: &str,
        track_id: &str,
        liked: bool,
        updated_at: Option<u64>,
    ) -> Result<LikeState, UserDataError> {
        let states =
            self.set_like_states(user_id, vec![(track_id.to_owned(), liked, updated_at)])?;
        Ok(states.into_iter().next().expect("one valid like update").1)
    }

    pub fn set_like_states(
        &self,
        user_id: &str,
        items: Vec<(String, bool, Option<u64>)>,
    ) -> Result<Vec<(String, LikeState)>, UserDataError> {
        let write_txn = ::library::begin_write(&self.db)?;
        let mut states = Vec::new();
        let mut changed = false;
        {
            let mut table = write_txn.open_table(LIKES_TABLE)?;
            for (track_id, liked, updated_at) in items {
                let key = user_key(user_id, &track_id);
                let previous = table
                    .get(key.as_str())?
                    .map(|value| decode_like_state(value.value()));
                if let Some(previous) = previous {
                    if previous.liked == liked
                        && updated_at
                            .filter(|value| *value > 0)
                            .is_none_or(|value| value == previous.updated_at)
                    {
                        states.push((track_id, previous));
                        continue;
                    }
                }
                let state = updated_at
                    .filter(|value| *value > 0)
                    .map(|updated_at| LikeState { liked, updated_at })
                    .unwrap_or_else(|| LikeState::with_state(liked));
                table.insert(key.as_str(), encode_value(&state)?.as_slice())?;
                states.push((track_id, state));
                changed = true;
            }
        }
        if changed {
            self.commit_collections(write_txn, user_id, None)?;
        }
        Ok(states)
    }

    pub fn get_playback_settings(
        &self,
        user_id: &str,
    ) -> Result<Option<PlaybackSettings>, UserDataError> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(PLAYBACK_SETTINGS_TABLE) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let settings = match table.get(user_id)? {
            Some(value) => Some(decode_value(value.value())?),
            None => None,
        };
        Ok(settings)
    }

    pub fn set_playback_settings(
        &self,
        user_id: &str,
        settings: PlaybackSettings,
    ) -> Result<(), UserDataError> {
        let write_txn = ::library::begin_write(&self.db)?;
        {
            let mut table = write_txn.open_table(PLAYBACK_SETTINGS_TABLE)?;
            let bytes = encode_value(&settings)?;
            table.insert(user_id, bytes.as_slice())?;
        }
        write_txn.commit()?;
        Ok(())
    }
}

pub fn open_or_create_db(path: &Path) -> Result<Database, UserDataError> {
    open_or_create_db_with_cache(path, 8 * 1024 * 1024)
}

pub fn open_or_create_db_with_cache(
    path: &Path,
    cache_bytes: usize,
) -> Result<Database, UserDataError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if path.exists() {
        Ok(Database::builder().set_cache_size(cache_bytes).open(path)?)
    } else {
        Ok(Database::builder()
            .set_cache_size(cache_bytes)
            .create(path)?)
    }
}

#[derive(Debug)]
pub enum UserDataError {
    PreconditionRequired,
    PlaylistConflict,
    RevisionExhausted,
    Invalid(String),
    PresetConflict,
    Io(std::io::Error),
    Redb(redb::Error),
    Database(DatabaseError),
    Table(TableError),
    Transaction(TransactionError),
    Storage(StorageError),
    Commit(CommitError),
    Bincode(Box<bincode::ErrorKind>),
}

impl From<std::io::Error> for UserDataError {
    fn from(err: std::io::Error) -> Self {
        UserDataError::Io(err)
    }
}

impl From<redb::Error> for UserDataError {
    fn from(err: redb::Error) -> Self {
        UserDataError::Redb(err)
    }
}

impl From<DatabaseError> for UserDataError {
    fn from(err: DatabaseError) -> Self {
        UserDataError::Database(err)
    }
}

impl From<TableError> for UserDataError {
    fn from(err: TableError) -> Self {
        UserDataError::Table(err)
    }
}

impl From<TransactionError> for UserDataError {
    fn from(err: TransactionError) -> Self {
        UserDataError::Transaction(err)
    }
}

impl From<StorageError> for UserDataError {
    fn from(err: StorageError) -> Self {
        UserDataError::Storage(err)
    }
}

impl From<CommitError> for UserDataError {
    fn from(err: CommitError) -> Self {
        UserDataError::Commit(err)
    }
}

impl From<Box<bincode::ErrorKind>> for UserDataError {
    fn from(err: Box<bincode::ErrorKind>) -> Self {
        UserDataError::Bincode(err)
    }
}

impl std::fmt::Display for UserDataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UserDataError::PreconditionRequired => write!(
                f,
                "expected_revision is required when replacing playlist tracks"
            ),
            UserDataError::PlaylistConflict => {
                write!(f, "playlist changed; refresh before replacing tracks")
            }
            UserDataError::RevisionExhausted => write!(f, "collection revision exhausted"),
            UserDataError::Invalid(message) => write!(f, "{message}"),
            UserDataError::PresetConflict => {
                write!(f, "shuffle preset changed; reload before saving")
            }
            UserDataError::Io(err) => write!(f, "io error: {}", err),
            UserDataError::Redb(err) => write!(f, "redb error: {}", err),
            UserDataError::Database(err) => write!(f, "redb database error: {}", err),
            UserDataError::Table(err) => write!(f, "redb table error: {}", err),
            UserDataError::Transaction(err) => write!(f, "redb transaction error: {}", err),
            UserDataError::Storage(err) => write!(f, "redb storage error: {}", err),
            UserDataError::Commit(err) => write!(f, "redb commit error: {}", err),
            UserDataError::Bincode(err) => write!(f, "bincode error: {}", err),
        }
    }
}

impl std::error::Error for UserDataError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            UserDataError::PreconditionRequired
            | UserDataError::PlaylistConflict
            | UserDataError::RevisionExhausted
            | UserDataError::Invalid(_)
            | UserDataError::PresetConflict => None,
            UserDataError::Io(err) => Some(err),
            UserDataError::Redb(err) => Some(err),
            UserDataError::Database(err) => Some(err),
            UserDataError::Table(err) => Some(err),
            UserDataError::Transaction(err) => Some(err),
            UserDataError::Storage(err) => Some(err),
            UserDataError::Commit(err) => Some(err),
            UserDataError::Bincode(err) => Some(err),
        }
    }
}

// A byte-length prefix makes this namespace unambiguous even when user IDs or
// item IDs contain separators or one user ID is a prefix of another.
fn user_prefix(user_id: &str) -> String {
    format!("{}:{}:", user_id.len(), user_id)
}

fn user_key(user_id: &str, item_id: &str) -> String {
    format!("{}{item_id}", user_prefix(user_id))
}

fn encode_value<T: Serialize>(value: &T) -> Result<Vec<u8>, UserDataError> {
    Ok(bincode::serialize(value)?)
}

fn decode_value<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, UserDataError> {
    Ok(bincode::deserialize(bytes)?)
}

fn decode_playlist(bytes: &[u8]) -> Result<Playlist, UserDataError> {
    match bincode::deserialize::<Playlist>(bytes) {
        Ok(playlist) => Ok(playlist),
        Err(_) => {
            if let Ok(legacy) = bincode::deserialize::<PlaylistWithImageRefLegacy>(bytes) {
                return Ok(Playlist {
                    id: legacy.id,
                    name: legacy.name,
                    track_ids: legacy.track_ids,
                    description: None,
                    image_ref: legacy.image_ref,
                });
            }
            let legacy: LegacyPlaylist = decode_value(bytes)?;
            Ok(Playlist {
                id: legacy.id,
                name: legacy.name,
                track_ids: legacy.track_ids,
                description: None,
                image_ref: None,
            })
        }
    }
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn decode_like_state(bytes: &[u8]) -> LikeState {
    match bincode::deserialize::<LikeState>(bytes) {
        Ok(state) => state,
        Err(_) if !bytes.is_empty() => LikeState {
            liked: true,
            updated_at: 0,
        },
        Err(_) => LikeState {
            liked: false,
            updated_at: 0,
        },
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn default_liked() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDb {
        path: std::path::PathBuf,
    }

    impl Drop for TestDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn test_store() -> (UserDataStore, TestDb) {
        let path =
            std::env::temp_dir().join(format!("phonolite_user_data_{}.redb", Uuid::new_v4()));
        let db = Arc::new(open_or_create_db(&path).unwrap());
        let store = UserDataStore::new(db);
        store.init_tables().unwrap();
        (store, TestDb { path })
    }

    #[test]
    fn collection_revisions_notify_all_owner_sessions_after_commit_and_coalesce_batches() {
        let (store, _db) = test_store();
        let mut first = store.subscribe_collections("alice");
        let mut second = store.subscribe_collections("alice");
        let mut other = store.subscribe_collections("bob");
        store.add_like("alice", "a").unwrap();
        assert_eq!(first.receiver.try_recv().unwrap(), 1);
        assert_eq!(second.receiver.try_recv().unwrap(), 1);
        assert!(matches!(
            other.receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert_eq!(
            store.collection_snapshot("alice").unwrap().likes[0].track_id,
            "a"
        );
        store.add_like("alice", "a").unwrap();
        assert_eq!(store.collection_revision("alice").unwrap(), 1);
        assert!(matches!(
            first.receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        store
            .set_like_states(
                "alice",
                vec![("a".into(), false, None), ("b".into(), true, None)],
            )
            .unwrap();
        assert_eq!(first.receiver.try_recv().unwrap(), 2);
        assert!(matches!(
            first.receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        let snapshot = store.collection_snapshot("alice").unwrap();
        assert_eq!(snapshot.revision, 2);
        assert!(
            !snapshot
                .likes
                .iter()
                .find(|state| state.track_id == "a")
                .unwrap()
                .liked
        );
        assert!(store.collection_snapshot("bob").unwrap().likes.is_empty());
    }

    #[test]
    fn concurrent_atomic_playlist_additions_preserve_both_clients_and_retry_is_a_noop() {
        let (store, _db) = test_store();
        let playlist = store
            .create_playlist("alice", "Mix".into(), None, vec!["original".into()])
            .unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let writers: Vec<_> = ["first", "second"]
            .into_iter()
            .map(|id| {
                let store = store.clone();
                let playlist_id = playlist.id.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store
                        .change_playlist_tracks("alice", &playlist_id, vec![id.into()], true)
                        .unwrap();
                })
            })
            .collect();
        barrier.wait();
        for writer in writers {
            writer.join().unwrap();
        }
        let tracks = store
            .get_playlist("alice", &playlist.id)
            .unwrap()
            .unwrap()
            .track_ids;
        assert_eq!(tracks.len(), 3);
        assert!(tracks.contains(&"first".to_owned()));
        assert!(tracks.contains(&"second".to_owned()));
        let revision = store.collection_revision("alice").unwrap();
        store
            .change_playlist_tracks(
                "alice",
                &playlist.id,
                vec!["first".into(), "second".into()],
                true,
            )
            .unwrap();
        assert_eq!(store.collection_revision("alice").unwrap(), revision);
        store
            .change_playlist_tracks("alice", &playlist.id, vec!["first".into()], false)
            .unwrap();
        store
            .change_playlist_tracks("alice", &playlist.id, vec!["first".into()], false)
            .unwrap();
        assert_eq!(store.collection_revision("alice").unwrap(), revision + 1);
    }

    #[test]
    fn checked_replacement_rejects_missing_and_stale_versions_without_partial_updates() {
        let (store, _db) = test_store();
        let playlist = store
            .create_playlist("alice", "Original".into(), None, vec!["a".into()])
            .unwrap();
        assert!(matches!(
            store.update_playlist_checked(
                "alice",
                &playlist.id,
                Some("Changed".into()),
                None,
                Some(vec![]),
                None
            ),
            Err(UserDataError::PreconditionRequired)
        ));
        store
            .change_playlist_tracks("alice", &playlist.id, vec!["b".into()], true)
            .unwrap();
        assert!(matches!(
            store.update_playlist_checked(
                "alice",
                &playlist.id,
                Some("Changed".into()),
                None,
                Some(vec![]),
                Some(1)
            ),
            Err(UserDataError::PlaylistConflict)
        ));
        let current = store
            .versioned_playlist("alice", &playlist.id)
            .unwrap()
            .unwrap();
        assert_eq!(current.revision, 2);
        assert_eq!(current.playlist.name, "Original");
        assert_eq!(current.playlist.track_ids, ["a", "b"]);
        store
            .update_playlist_checked(
                "alice",
                &playlist.id,
                None,
                None,
                Some(vec!["b".into(), "a".into()]),
                Some(2),
            )
            .unwrap();
        assert_eq!(store.collection_revision("alice").unwrap(), 3);
        assert!(store
            .update_playlist_checked("bob", &playlist.id, None, None, Some(vec![]), None)
            .unwrap()
            .is_none());
        assert_eq!(store.collection_revision("bob").unwrap(), 0);
    }

    #[test]
    fn snapshots_never_pair_a_partial_like_batch_with_its_revision() {
        let (store, _db) = test_store();
        let writer_store = store.clone();
        let writer = std::thread::spawn(move || {
            for index in 0..20 {
                writer_store
                    .set_like_states(
                        "alice",
                        vec![
                            (format!("{index}-a"), true, None),
                            (format!("{index}-b"), true, None),
                        ],
                    )
                    .unwrap();
            }
        });
        while !writer.is_finished() {
            let snapshot = store.collection_snapshot("alice").unwrap();
            assert_eq!(snapshot.likes.len() as u64, snapshot.revision * 2);
        }
        writer.join().unwrap();
        assert_eq!(store.collection_snapshot("alice").unwrap().likes.len(), 40);
    }

    #[test]
    fn collection_revisions_survive_reopen_and_legacy_playlist_bytes_stay_compatible() {
        let (store, db) = test_store();
        let legacy = LegacyPlaylist {
            id: "legacy".into(),
            name: "Existing".into(),
            track_ids: vec!["a".into()],
        };
        let bytes = encode_value(&legacy).unwrap();
        let txn = ::library::begin_write(&store.db).unwrap();
        txn.open_table(PLAYLISTS_TABLE)
            .unwrap()
            .insert(user_key("alice", "legacy").as_str(), bytes.as_slice())
            .unwrap();
        txn.commit().unwrap();
        let snapshot = store.collection_snapshot("alice").unwrap();
        assert_eq!(snapshot.revision, 0);
        assert_eq!(snapshot.playlists[0].revision, 0);
        assert_eq!(snapshot.playlists[0].playlist.name, "Existing");
        store
            .update_playlist_checked(
                "alice",
                "legacy",
                None,
                None,
                Some(vec!["b".into()]),
                Some(0),
            )
            .unwrap();
        store.add_like("alice", "b").unwrap();
        drop(store);
        let store = UserDataStore::new(Arc::new(open_or_create_db(&db.path).unwrap()));
        store.init_tables().unwrap();
        let snapshot = store.collection_snapshot("alice").unwrap();
        assert_eq!(snapshot.revision, 2);
        assert_eq!(snapshot.playlists[0].revision, 1);
        assert_eq!(snapshot.playlists[0].playlist.track_ids, ["b"]);
    }

    #[test]
    fn lagged_and_reconnected_consumers_recover_from_authoritative_snapshot() {
        let (store, _db) = test_store();
        let mut subscription = store.subscribe_collections("alice");
        for index in 0..70 {
            store.add_like("alice", &format!("track-{index}")).unwrap();
        }
        assert!(matches!(
            subscription.receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));
        drop(subscription);
        store.remove_like("alice", "track-0").unwrap();
        let _subscription = store.subscribe_collections("alice");
        let snapshot = store.collection_snapshot("alice").unwrap();
        assert_eq!(snapshot.revision, 71);
        assert_eq!(
            snapshot.likes.iter().filter(|state| state.liked).count(),
            69
        );
    }

    #[test]
    fn cover_and_delete_changes_advance_revisions_and_noops_do_not() {
        let (store, _db) = test_store();
        let playlist = store
            .create_playlist("alice", "Mix".into(), None, vec![])
            .unwrap();
        store.clear_playlist_image("alice", &playlist.id).unwrap();
        assert_eq!(store.collection_revision("alice").unwrap(), 1);
        store
            .set_playlist_image("alice", &playlist.id, "image/png".into(), vec![1])
            .unwrap();
        assert_eq!(
            store
                .versioned_playlist("alice", &playlist.id)
                .unwrap()
                .unwrap()
                .revision,
            2
        );
        store.clear_playlist_image("alice", &playlist.id).unwrap();
        assert_eq!(store.collection_revision("alice").unwrap(), 3);
        assert!(!store.delete_playlist("bob", &playlist.id).unwrap());
        assert_eq!(store.collection_revision("bob").unwrap(), 0);
        assert!(store.delete_playlist("alice", &playlist.id).unwrap());
        assert_eq!(store.collection_revision("alice").unwrap(), 4);
        assert!(!store.delete_playlist("alice", &playlist.id).unwrap());
        assert_eq!(store.collection_revision("alice").unwrap(), 4);
    }

    #[test]
    fn legacy_like_rows_decode_as_liked() {
        let state = decode_like_state(&[1]);
        assert!(state.liked);
        assert_eq!(state.updated_at, 0);
    }

    #[test]
    fn unlike_writes_tombstone_and_list_likes_excludes_it() {
        let (store, _db) = test_store();
        store.add_like("alice", "track-1").unwrap();
        assert_eq!(store.list_likes("alice").unwrap(), vec!["track-1"]);

        store.remove_like("alice", "track-1").unwrap();
        assert!(store.list_likes("alice").unwrap().is_empty());
        let states = store.list_like_states("alice").unwrap();
        let (_, state) = states
            .iter()
            .find(|(track_id, _)| track_id == "track-1")
            .expect("tombstone should be preserved");
        assert!(!state.liked);
        assert!(state.updated_at > 0);
    }

    #[test]
    fn list_likes_orders_newest_liked_first() {
        let (store, _db) = test_store();
        let write_txn = ::library::begin_write(&store.db).unwrap();
        {
            let mut table = write_txn.open_table(LIKES_TABLE).unwrap();
            let older = encode_value(&LikeState {
                liked: true,
                updated_at: 1000,
            })
            .unwrap();
            let newer = encode_value(&LikeState {
                liked: true,
                updated_at: 3000,
            })
            .unwrap();
            let tombstone = encode_value(&LikeState {
                liked: false,
                updated_at: 4000,
            })
            .unwrap();
            table
                .insert(user_key("alice", "older-liked").as_str(), older.as_slice())
                .unwrap();
            table
                .insert(user_key("alice", "newer-liked").as_str(), newer.as_slice())
                .unwrap();
            table
                .insert(
                    user_key("alice", "newer-unliked").as_str(),
                    tombstone.as_slice(),
                )
                .unwrap();
        }
        write_txn.commit().unwrap();

        assert_eq!(
            store.list_likes("alice").unwrap(),
            vec!["newer-liked", "older-liked"]
        );
    }

    #[test]
    fn set_like_state_returns_updated_state() {
        let (store, _db) = test_store();
        let state = store.set_like_state("alice", "track-2", true).unwrap();
        assert!(state.liked);
        assert!(state.updated_at > 0);
    }

    #[test]
    fn set_like_state_can_preserve_client_like_timestamp() {
        let (store, _db) = test_store();
        let state = store
            .set_like_state_with_updated_at("alice", "track-3", true, Some(1234))
            .unwrap();

        assert!(state.liked);
        assert_eq!(state.updated_at, 1234);
        assert_eq!(store.list_likes("alice").unwrap(), vec!["track-3"]);
    }

    #[test]
    fn legacy_playlist_rows_decode_without_image_ref() {
        let legacy = LegacyPlaylist {
            id: "playlist-legacy".to_string(),
            name: "Legacy".to_string(),
            track_ids: vec!["track-1".to_string()],
        };
        let playlist = decode_playlist(&encode_value(&legacy).unwrap()).unwrap();
        assert_eq!(playlist.id, "playlist-legacy");
        assert_eq!(playlist.track_ids, vec!["track-1"]);
        assert_eq!(playlist.description, None);
        assert_eq!(playlist.image_ref, None);
    }

    #[test]
    fn playlist_rows_with_image_ref_decode_without_description() {
        let legacy = PlaylistWithImageRefLegacy {
            id: "playlist-image-legacy".to_string(),
            name: "Legacy Cover".to_string(),
            track_ids: vec!["track-1".to_string()],
            image_ref: Some("cover-rev".to_string()),
        };
        let playlist = decode_playlist(&encode_value(&legacy).unwrap()).unwrap();
        assert_eq!(playlist.description, None);
        assert_eq!(playlist.image_ref.as_deref(), Some("cover-rev"));
    }

    #[test]
    fn playlist_image_lifecycle_updates_ref_and_deletes_blob() {
        let (store, _db) = test_store();
        let playlist = store
            .create_playlist(
                "alice",
                "Mix".to_string(),
                Some("Playlist summary".to_string()),
                vec!["track-1".to_string()],
            )
            .unwrap();
        assert_eq!(playlist.description.as_deref(), Some("Playlist summary"));
        assert_eq!(playlist.image_ref, None);

        let updated = store
            .set_playlist_image(
                "alice",
                &playlist.id,
                "image/png".to_string(),
                vec![1, 2, 3],
            )
            .unwrap()
            .expect("playlist should exist");
        assert!(updated.image_ref.is_some());
        let image = store
            .get_playlist_image("alice", &playlist.id)
            .unwrap()
            .expect("image should exist");
        assert_eq!(image.content_type, "image/png");
        assert_eq!(image.bytes, vec![1, 2, 3]);

        let cleared = store
            .clear_playlist_image("alice", &playlist.id)
            .unwrap()
            .expect("playlist should exist");
        assert_eq!(cleared.image_ref, None);
        assert!(store
            .get_playlist_image("alice", &playlist.id)
            .unwrap()
            .is_none());

        store
            .set_playlist_image(
                "alice",
                &playlist.id,
                "image/jpeg".to_string(),
                vec![4, 5, 6],
            )
            .unwrap();
        assert!(store.delete_playlist("alice", &playlist.id).unwrap());
        assert!(store
            .get_playlist_image("alice", &playlist.id)
            .unwrap()
            .is_none());
    }

    #[test]
    fn likes_and_tombstones_are_isolated_between_users() {
        let (store, _db) = test_store();
        store
            .set_like_state_with_updated_at("alice", "shared-track", true, Some(100))
            .unwrap();
        assert!(store.list_likes("bob").unwrap().is_empty());
        assert!(store.list_like_states("bob").unwrap().is_empty());

        store
            .set_like_state_with_updated_at("bob", "shared-track", true, Some(200))
            .unwrap();
        store.remove_like("alice", "shared-track").unwrap();
        assert!(store.list_likes("alice").unwrap().is_empty());
        assert_eq!(store.list_likes("bob").unwrap(), vec!["shared-track"]);
        let bob_states = store.list_like_states("bob").unwrap();
        assert!(bob_states[0].1.liked);
        assert_eq!(bob_states[0].1.updated_at, 200);
        assert!(!store.list_like_states("alice").unwrap()[0].1.liked);
        store.add_like("alice", "alice-only").unwrap();
        store.remove_like("bob", "alice-only").unwrap();
        assert_eq!(store.list_likes("alice").unwrap(), vec!["alice-only"]);
    }

    #[test]
    fn another_user_cannot_read_change_or_delete_a_playlist_or_its_cover() {
        let (store, _db) = test_store();
        let alice = store
            .create_playlist(
                "alice",
                "Alice mix".into(),
                Some("Private".into()),
                vec!["a".into()],
            )
            .unwrap();
        let bob = store
            .create_playlist("bob", "Bob mix".into(), None, vec!["b".into()])
            .unwrap();
        store
            .set_playlist_image("alice", &alice.id, "image/png".into(), vec![1, 2])
            .unwrap();
        store
            .set_playlist_image("bob", &bob.id, "image/jpeg".into(), vec![3, 4])
            .unwrap();
        assert_eq!(store.list_playlists("alice").unwrap()[0].id, alice.id);
        assert_eq!(store.list_playlists("bob").unwrap()[0].id, bob.id);
        assert!(store.get_playlist("bob", &alice.id).unwrap().is_none());
        assert!(store
            .get_playlist_image("bob", &alice.id)
            .unwrap()
            .is_none());
        assert!(store
            .update_playlist(
                "bob",
                &alice.id,
                Some("Stolen".into()),
                Some("Changed".into()),
                Some(vec![])
            )
            .unwrap()
            .is_none());
        assert!(store
            .set_playlist_image("bob", &alice.id, "image/png".into(), vec![9])
            .unwrap()
            .is_none());
        assert!(store
            .clear_playlist_image("bob", &alice.id)
            .unwrap()
            .is_none());
        assert!(!store.delete_playlist("bob", &alice.id).unwrap());
        let unchanged = store.get_playlist("alice", &alice.id).unwrap().unwrap();
        assert_eq!(unchanged.name, "Alice mix");
        assert_eq!(unchanged.description.as_deref(), Some("Private"));
        assert_eq!(unchanged.track_ids, vec!["a"]);
        assert_eq!(
            store
                .get_playlist_image("alice", &alice.id)
                .unwrap()
                .unwrap()
                .bytes,
            vec![1, 2]
        );
        let updated = store
            .update_playlist("alice", &alice.id, Some("Updated".into()), None, None)
            .unwrap()
            .unwrap();
        assert_eq!(updated.name, "Updated");
        assert!(store.delete_playlist("alice", &alice.id).unwrap());
        assert!(store
            .get_playlist_image("alice", &alice.id)
            .unwrap()
            .is_none());
        assert_eq!(
            store.get_playlist("bob", &bob.id).unwrap().unwrap().name,
            "Bob mix"
        );
        assert_eq!(
            store
                .get_playlist_image("bob", &bob.id)
                .unwrap()
                .unwrap()
                .bytes,
            vec![3, 4]
        );
    }

    #[test]
    fn cover_blobs_require_an_owned_playlist_even_if_orphaned() {
        let (store, _db) = test_store();
        let key = user_key("alice", "orphan");
        let bytes = encode_value(&PlaylistImage {
            content_type: "image/png".into(),
            bytes: vec![1],
            updated_at: 1,
        })
        .unwrap();
        let txn = ::library::begin_write(&store.db).unwrap();
        txn.open_table(PLAYLIST_IMAGES_TABLE)
            .unwrap()
            .insert(key.as_str(), bytes.as_slice())
            .unwrap();
        txn.commit().unwrap();
        assert!(store
            .get_playlist_image("alice", "orphan")
            .unwrap()
            .is_none());
        assert!(store
            .set_playlist_image("alice", "orphan", "image/png".into(), vec![2])
            .unwrap()
            .is_none());
        assert!(store
            .clear_playlist_image("alice", "orphan")
            .unwrap()
            .is_none());
        assert!(!store.delete_playlist("alice", "orphan").unwrap());
        let txn = store.db.begin_read().unwrap();
        let table = txn.open_table(PLAYLIST_IMAGES_TABLE).unwrap();
        assert_eq!(
            table.get(key.as_str()).unwrap().unwrap().value(),
            bytes.as_slice()
        );
    }

    #[test]
    fn playback_settings_are_isolated_and_new_users_have_no_override() {
        let (store, _db) = test_store();
        assert!(store.get_playback_settings("alice").unwrap().is_none());
        store
            .set_playback_settings(
                "alice",
                PlaybackSettings {
                    repeat_mode: "one".into(),
                },
            )
            .unwrap();
        assert!(store.get_playback_settings("bob").unwrap().is_none());
        store
            .set_playback_settings(
                "bob",
                PlaybackSettings {
                    repeat_mode: "off".into(),
                },
            )
            .unwrap();
        assert_eq!(
            store
                .get_playback_settings("alice")
                .unwrap()
                .unwrap()
                .repeat_mode,
            "one"
        );
        assert_eq!(
            store
                .get_playback_settings("bob")
                .unwrap()
                .unwrap()
                .repeat_mode,
            "off"
        );
        assert!(store
            .get_playback_settings("new-account")
            .unwrap()
            .is_none());
    }

    #[test]
    fn user_namespaces_resist_prefix_separator_and_unicode_collisions() {
        let (store, _db) = test_store();
        let users = ["a", "ab", "a:", "a:b", "a:b:", "", "é", "é:"];
        for (index, user) in users.iter().enumerate() {
            store
                .set_like_state_with_updated_at(user, ":same:track", true, Some(index as u64 + 1))
                .unwrap();
            store
                .create_playlist(user, format!("Mix {index}"), None, vec![])
                .unwrap();
        }
        for (index, user) in users.iter().enumerate() {
            assert_eq!(store.list_likes(user).unwrap(), vec![":same:track"]);
            assert_eq!(
                store.list_like_states(user).unwrap()[0].1.updated_at,
                index as u64 + 1
            );
            let playlists = store.list_playlists(user).unwrap();
            assert_eq!(playlists.len(), 1);
            assert_eq!(playlists[0].name, format!("Mix {index}"));
        }
        // These pairs collide when a plain separator joins user and track IDs.
        store.add_like("a", "b:c").unwrap();
        store.remove_like("a:b", "c").unwrap();
        assert!(store.list_likes("a").unwrap().contains(&"b:c".to_string()));
        assert!(!store.list_likes("a:b").unwrap().contains(&"c".to_string()));
    }

    #[test]
    fn scoped_personal_data_survives_reopening_the_database() {
        let (store, db) = test_store();
        let playlist = store
            .create_playlist("alice", "Persistent".into(), None, vec![])
            .unwrap();
        store
            .set_playlist_image("alice", &playlist.id, "image/png".into(), vec![5])
            .unwrap();
        store.add_like("alice", "track").unwrap();
        store.remove_like("bob", "track").unwrap();
        store
            .set_playback_settings(
                "alice",
                PlaybackSettings {
                    repeat_mode: "all".into(),
                },
            )
            .unwrap();
        drop(store);
        let reopened = UserDataStore::new(Arc::new(open_or_create_db(&db.path).unwrap()));
        reopened.init_tables().unwrap();
        assert_eq!(reopened.list_likes("alice").unwrap(), vec!["track"]);
        assert!(reopened.list_likes("bob").unwrap().is_empty());
        assert!(!reopened.list_like_states("bob").unwrap()[0].1.liked);
        assert_eq!(
            reopened
                .get_playlist_image("alice", &playlist.id)
                .unwrap()
                .unwrap()
                .bytes,
            vec![5]
        );
        assert!(reopened
            .get_playlist("bob", &playlist.id)
            .unwrap()
            .is_none());
        assert_eq!(
            reopened
                .get_playback_settings("alice")
                .unwrap()
                .unwrap()
                .repeat_mode,
            "all"
        );
        assert!(reopened.get_playback_settings("bob").unwrap().is_none());
        drop(reopened);
    }

    #[test]
    fn legacy_global_data_is_preserved_and_never_assigned_to_users() {
        const LEGACY_TABLES: [TableDefinition<&str, &[u8]>; 4] = [
            TableDefinition::new("playlists"),
            TableDefinition::new("playlist_images"),
            TableDefinition::new("likes"),
            TableDefinition::new("playback_settings"),
        ];
        let (store, _db) = test_store();
        let playlist = Playlist {
            id: "old-playlist".into(),
            name: "Legacy".into(),
            track_ids: vec!["track".into()],
            description: None,
            image_ref: Some("old-image".into()),
        };
        let rows = [
            ("old-playlist", encode_value(&playlist).unwrap()),
            (
                "old-playlist",
                encode_value(&PlaylistImage {
                    content_type: "image/png".into(),
                    bytes: vec![8],
                    updated_at: 1,
                })
                .unwrap(),
            ),
            (
                "track",
                encode_value(&LikeState {
                    liked: true,
                    updated_at: 1,
                })
                .unwrap(),
            ),
            (
                "global",
                encode_value(&PlaybackSettings {
                    repeat_mode: "one".into(),
                })
                .unwrap(),
            ),
        ];
        let txn = ::library::begin_write(&store.db).unwrap();
        for (definition, (key, bytes)) in LEGACY_TABLES.into_iter().zip(&rows) {
            txn.open_table(definition)
                .unwrap()
                .insert(*key, bytes.as_slice())
                .unwrap();
        }
        txn.commit().unwrap();
        store.init_tables().unwrap();
        for user in ["alice", "bob", "global"] {
            assert!(store.list_likes(user).unwrap().is_empty());
            assert!(store.list_like_states(user).unwrap().is_empty());
            assert!(store.list_playlists(user).unwrap().is_empty());
            assert!(store.get_playlist(user, "old-playlist").unwrap().is_none());
            assert!(store
                .get_playlist_image(user, "old-playlist")
                .unwrap()
                .is_none());
            assert!(store.get_playback_settings(user).unwrap().is_none());
            assert!(store
                .update_playlist(user, "old-playlist", Some("Changed".into()), None, None)
                .unwrap()
                .is_none());
            assert!(!store.delete_playlist(user, "old-playlist").unwrap());
            store.add_like(user, "track").unwrap();
            store
                .set_playback_settings(
                    user,
                    PlaybackSettings {
                        repeat_mode: "off".into(),
                    },
                )
                .unwrap();
            store
                .create_playlist(user, "New".into(), None, vec![])
                .unwrap();
        }
        let txn = store.db.begin_read().unwrap();
        for (definition, (key, bytes)) in LEGACY_TABLES.into_iter().zip(&rows) {
            let table = txn.open_table(definition).unwrap();
            assert_eq!(table.len().unwrap(), 1);
            assert_eq!(table.get(*key).unwrap().unwrap().value(), bytes.as_slice());
        }
    }
}
