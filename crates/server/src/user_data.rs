use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use redb::{
    CommitError, Database, DatabaseError, ReadableTable, StorageError, TableDefinition, TableError,
    TransactionError,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::state::Playlist;

const PLAYLISTS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("playlists");
const PLAYLIST_IMAGES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("playlist_images");
const LIKES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("likes");
const PLAYBACK_SETTINGS_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("playback_settings");

const PLAYBACK_SETTINGS_KEY: &str = "global";

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
}

impl UserDataStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    pub fn init_tables(&self) -> Result<(), UserDataError> {
        let write_txn = self.db.begin_write()?;
        {
            let _ = write_txn.open_table(PLAYLISTS_TABLE)?;
            let _ = write_txn.open_table(PLAYLIST_IMAGES_TABLE)?;
            let _ = write_txn.open_table(LIKES_TABLE)?;
            let _ = write_txn.open_table(PLAYBACK_SETTINGS_TABLE)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    pub fn list_playlists(&self) -> Result<Vec<Playlist>, UserDataError> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(PLAYLISTS_TABLE) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let mut items = Vec::new();
        for entry in table.iter()? {
            let entry = entry?;
            let playlist = decode_playlist(entry.1.value())?;
            items.push(playlist);
        }
        Ok(items)
    }

    pub fn get_playlist(&self, playlist_id: &str) -> Result<Option<Playlist>, UserDataError> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(PLAYLISTS_TABLE) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let playlist = match table.get(playlist_id)? {
            Some(value) => Some(decode_playlist(value.value())?),
            None => None,
        };
        Ok(playlist)
    }

    pub fn create_playlist(
        &self,
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
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(PLAYLISTS_TABLE)?;
            let bytes = encode_value(&playlist)?;
            table.insert(playlist.id.as_str(), bytes.as_slice())?;
        }
        write_txn.commit()?;
        Ok(playlist)
    }

    pub fn update_playlist(
        &self,
        playlist_id: &str,
        name: Option<String>,
        description: Option<String>,
        track_ids: Option<Vec<String>>,
    ) -> Result<Option<Playlist>, UserDataError> {
        let write_txn = self.db.begin_write()?;
        let updated = {
            let mut table = match write_txn.open_table(PLAYLISTS_TABLE) {
                Ok(table) => table,
                Err(TableError::TableDoesNotExist(_)) => return Ok(None),
                Err(err) => return Err(err.into()),
            };
            let mut playlist = match table.get(playlist_id)? {
                Some(value) => decode_playlist(value.value())?,
                None => return Ok(None),
            };
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
            table.insert(playlist_id, bytes.as_slice())?;
            playlist
        };
        write_txn.commit()?;
        Ok(Some(updated))
    }

    pub fn get_playlist_image(
        &self,
        playlist_id: &str,
    ) -> Result<Option<PlaylistImage>, UserDataError> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(PLAYLIST_IMAGES_TABLE) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let image = match table.get(playlist_id)? {
            Some(value) => Some(decode_value(value.value())?),
            None => None,
        };
        Ok(image)
    }

    pub fn set_playlist_image(
        &self,
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
        let write_txn = self.db.begin_write()?;
        let updated = {
            let playlist = {
                let mut table = match write_txn.open_table(PLAYLISTS_TABLE) {
                    Ok(table) => table,
                    Err(TableError::TableDoesNotExist(_)) => return Ok(None),
                    Err(err) => return Err(err.into()),
                };
                let mut playlist = match table.get(playlist_id)? {
                    Some(value) => decode_playlist(value.value())?,
                    None => return Ok(None),
                };
                playlist.image_ref = Some(format!("{}-{}", updated_at, Uuid::new_v4()));
                let playlist_bytes = encode_value(&playlist)?;
                table.insert(playlist_id, playlist_bytes.as_slice())?;
                playlist
            };
            {
                let mut table = write_txn.open_table(PLAYLIST_IMAGES_TABLE)?;
                let image_bytes = encode_value(&image)?;
                table.insert(playlist_id, image_bytes.as_slice())?;
            }
            playlist
        };
        write_txn.commit()?;
        Ok(Some(updated))
    }

    pub fn clear_playlist_image(
        &self,
        playlist_id: &str,
    ) -> Result<Option<Playlist>, UserDataError> {
        let write_txn = self.db.begin_write()?;
        let updated = {
            let playlist = {
                let mut table = match write_txn.open_table(PLAYLISTS_TABLE) {
                    Ok(table) => table,
                    Err(TableError::TableDoesNotExist(_)) => return Ok(None),
                    Err(err) => return Err(err.into()),
                };
                let mut playlist = match table.get(playlist_id)? {
                    Some(value) => decode_playlist(value.value())?,
                    None => return Ok(None),
                };
                playlist.image_ref = None;
                let playlist_bytes = encode_value(&playlist)?;
                table.insert(playlist_id, playlist_bytes.as_slice())?;
                playlist
            };
            {
                let mut table = write_txn.open_table(PLAYLIST_IMAGES_TABLE)?;
                let _ = table.remove(playlist_id)?;
            }
            playlist
        };
        write_txn.commit()?;
        Ok(Some(updated))
    }

    pub fn delete_playlist(&self, playlist_id: &str) -> Result<bool, UserDataError> {
        let write_txn = self.db.begin_write()?;
        let deleted = {
            let removed = {
                let mut table = match write_txn.open_table(PLAYLISTS_TABLE) {
                    Ok(table) => table,
                    Err(TableError::TableDoesNotExist(_)) => return Ok(false),
                    Err(err) => return Err(err.into()),
                };
                let removed = table.remove(playlist_id)?.is_some();
                removed
            };
            if removed {
                let mut images = write_txn.open_table(PLAYLIST_IMAGES_TABLE)?;
                let _ = images.remove(playlist_id)?;
            }
            removed
        };
        write_txn.commit()?;
        Ok(deleted)
    }

    pub fn list_likes(&self) -> Result<Vec<String>, UserDataError> {
        let mut liked_states: Vec<_> = self
            .list_like_states()?
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

    pub fn list_like_states(&self) -> Result<Vec<(String, LikeState)>, UserDataError> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(LIKES_TABLE) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let mut states = Vec::new();
        for entry in table.iter()? {
            let entry = entry?;
            states.push((
                entry.0.value().to_string(),
                decode_like_state(entry.1.value()),
            ));
        }
        Ok(states)
    }

    pub fn add_like(&self, track_id: &str) -> Result<(), UserDataError> {
        self.set_like_state(track_id, true).map(|_| ())
    }

    pub fn remove_like(&self, track_id: &str) -> Result<(), UserDataError> {
        self.set_like_state(track_id, false).map(|_| ())
    }

    pub fn set_like_state(&self, track_id: &str, liked: bool) -> Result<LikeState, UserDataError> {
        self.set_like_state_with_updated_at(track_id, liked, None)
    }

    pub fn set_like_state_with_updated_at(
        &self,
        track_id: &str,
        liked: bool,
        updated_at: Option<u64>,
    ) -> Result<LikeState, UserDataError> {
        let state = updated_at
            .filter(|value| *value > 0)
            .map(|updated_at| LikeState { liked, updated_at })
            .unwrap_or_else(|| LikeState::with_state(liked));
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(LIKES_TABLE)?;
            let bytes = encode_value(&state)?;
            table.insert(track_id, bytes.as_slice())?;
        }
        write_txn.commit()?;
        Ok(state)
    }

    pub fn get_playback_settings(&self) -> Result<Option<PlaybackSettings>, UserDataError> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(PLAYBACK_SETTINGS_TABLE) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let settings = match table.get(PLAYBACK_SETTINGS_KEY)? {
            Some(value) => Some(decode_value(value.value())?),
            None => None,
        };
        Ok(settings)
    }

    pub fn set_playback_settings(&self, settings: PlaybackSettings) -> Result<(), UserDataError> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(PLAYBACK_SETTINGS_TABLE)?;
            let bytes = encode_value(&settings)?;
            table.insert(PLAYBACK_SETTINGS_KEY, bytes.as_slice())?;
        }
        write_txn.commit()?;
        Ok(())
    }
}

pub fn open_or_create_db(path: &Path) -> Result<Database, UserDataError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if path.exists() {
        Ok(Database::open(path)?)
    } else {
        Ok(Database::create(path)?)
    }
}

#[derive(Debug)]
pub enum UserDataError {
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
    fn legacy_like_rows_decode_as_liked() {
        let (store, _db) = test_store();
        let write_txn = store.db.begin_write().unwrap();
        {
            let mut table = write_txn.open_table(LIKES_TABLE).unwrap();
            table.insert("legacy-track", [1u8].as_slice()).unwrap();
        }
        write_txn.commit().unwrap();

        assert_eq!(store.list_likes().unwrap(), vec!["legacy-track"]);
        let states = store.list_like_states().unwrap();
        assert!(states
            .iter()
            .any(|(track_id, state)| track_id == "legacy-track" && state.liked));
    }

    #[test]
    fn unlike_writes_tombstone_and_list_likes_excludes_it() {
        let (store, _db) = test_store();
        store.add_like("track-1").unwrap();
        assert_eq!(store.list_likes().unwrap(), vec!["track-1"]);

        store.remove_like("track-1").unwrap();
        assert!(store.list_likes().unwrap().is_empty());
        let states = store.list_like_states().unwrap();
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
        let write_txn = store.db.begin_write().unwrap();
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
            table.insert("older-liked", older.as_slice()).unwrap();
            table.insert("newer-liked", newer.as_slice()).unwrap();
            table.insert("newer-unliked", tombstone.as_slice()).unwrap();
        }
        write_txn.commit().unwrap();

        assert_eq!(
            store.list_likes().unwrap(),
            vec!["newer-liked", "older-liked"]
        );
    }

    #[test]
    fn set_like_state_returns_updated_state() {
        let (store, _db) = test_store();
        let state = store.set_like_state("track-2", true).unwrap();
        assert!(state.liked);
        assert!(state.updated_at > 0);
    }

    #[test]
    fn set_like_state_can_preserve_client_like_timestamp() {
        let (store, _db) = test_store();
        let state = store
            .set_like_state_with_updated_at("track-3", true, Some(1234))
            .unwrap();

        assert!(state.liked);
        assert_eq!(state.updated_at, 1234);
        assert_eq!(store.list_likes().unwrap(), vec!["track-3"]);
    }

    #[test]
    fn legacy_playlist_rows_decode_without_image_ref() {
        let (store, _db) = test_store();
        let legacy = LegacyPlaylist {
            id: "playlist-legacy".to_string(),
            name: "Legacy".to_string(),
            track_ids: vec!["track-1".to_string()],
        };
        let write_txn = store.db.begin_write().unwrap();
        {
            let mut table = write_txn.open_table(PLAYLISTS_TABLE).unwrap();
            let bytes = encode_value(&legacy).unwrap();
            table.insert("playlist-legacy", bytes.as_slice()).unwrap();
        }
        write_txn.commit().unwrap();

        let playlists = store.list_playlists().unwrap();

        assert_eq!(playlists.len(), 1);
        assert_eq!(playlists[0].id, "playlist-legacy");
        assert_eq!(playlists[0].track_ids, vec!["track-1"]);
        assert_eq!(playlists[0].description, None);
        assert_eq!(playlists[0].image_ref, None);
    }

    #[test]
    fn playlist_rows_with_image_ref_decode_without_description() {
        let (store, _db) = test_store();
        let legacy = PlaylistWithImageRefLegacy {
            id: "playlist-image-legacy".to_string(),
            name: "Legacy Cover".to_string(),
            track_ids: vec!["track-1".to_string()],
            image_ref: Some("cover-rev".to_string()),
        };
        let write_txn = store.db.begin_write().unwrap();
        {
            let mut table = write_txn.open_table(PLAYLISTS_TABLE).unwrap();
            let bytes = encode_value(&legacy).unwrap();
            table
                .insert("playlist-image-legacy", bytes.as_slice())
                .unwrap();
        }
        write_txn.commit().unwrap();

        let playlists = store.list_playlists().unwrap();

        assert_eq!(playlists.len(), 1);
        assert_eq!(playlists[0].description, None);
        assert_eq!(playlists[0].image_ref.as_deref(), Some("cover-rev"));
    }

    #[test]
    fn playlist_image_lifecycle_updates_ref_and_deletes_blob() {
        let (store, _db) = test_store();
        let playlist = store
            .create_playlist(
                "Mix".to_string(),
                Some("Playlist summary".to_string()),
                vec!["track-1".to_string()],
            )
            .unwrap();
        assert_eq!(playlist.description.as_deref(), Some("Playlist summary"));
        assert_eq!(playlist.image_ref, None);

        let updated = store
            .set_playlist_image(&playlist.id, "image/png".to_string(), vec![1, 2, 3])
            .unwrap()
            .expect("playlist should exist");
        assert!(updated.image_ref.is_some());
        let image = store
            .get_playlist_image(&playlist.id)
            .unwrap()
            .expect("image should exist");
        assert_eq!(image.content_type, "image/png");
        assert_eq!(image.bytes, vec![1, 2, 3]);

        let cleared = store
            .clear_playlist_image(&playlist.id)
            .unwrap()
            .expect("playlist should exist");
        assert_eq!(cleared.image_ref, None);
        assert!(store.get_playlist_image(&playlist.id).unwrap().is_none());

        store
            .set_playlist_image(&playlist.id, "image/jpeg".to_string(), vec![4, 5, 6])
            .unwrap();
        assert!(store.delete_playlist(&playlist.id).unwrap());
        assert!(store.get_playlist_image(&playlist.id).unwrap().is_none());
    }
}
