use std::fs;
use std::path::Path;
use std::sync::Arc;

use redb::{
    CommitError, Database, DatabaseError, ReadableTable, StorageError, TableDefinition,
    TableError, TransactionError,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::state::Playlist;

const PLAYLISTS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("playlists");
const LIKES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("likes");
const PLAYBACK_SETTINGS_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("playback_settings");

const PLAYBACK_SETTINGS_KEY: &str = "global";

#[derive(Clone, Serialize, Deserialize)]
pub struct PlaybackSettings {
    pub repeat_mode: String,
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
            let playlist: Playlist = decode_value(entry.1.value())?;
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
            Some(value) => Some(decode_value(value.value())?),
            None => None,
        };
        Ok(playlist)
    }

    pub fn create_playlist(
        &self,
        name: String,
        track_ids: Vec<String>,
    ) -> Result<Playlist, UserDataError> {
        let playlist = Playlist {
            id: Uuid::new_v4().to_string(),
            name,
            track_ids,
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
        track_ids: Option<Vec<String>>,
    ) -> Result<Option<Playlist>, UserDataError> {
        let write_txn = self.db.begin_write()?;
        let updated = {
            let mut table = match write_txn.open_table(PLAYLISTS_TABLE) {
                Ok(table) => table,
                Err(TableError::TableDoesNotExist(_)) => return Ok(None),
                Err(err) => return Err(err.into()),
            };
            let mut playlist: Playlist = match table.get(playlist_id)? {
                Some(value) => decode_value(value.value())?,
                None => return Ok(None),
            };
            if let Some(name) = name {
                playlist.name = name;
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

    pub fn delete_playlist(&self, playlist_id: &str) -> Result<bool, UserDataError> {
        let write_txn = self.db.begin_write()?;
        let deleted = {
            let mut table = match write_txn.open_table(PLAYLISTS_TABLE) {
                Ok(table) => table,
                Err(TableError::TableDoesNotExist(_)) => return Ok(false),
                Err(err) => return Err(err.into()),
            };
            let removed = table.remove(playlist_id)?.is_some();
            removed
        };
        write_txn.commit()?;
        Ok(deleted)
    }

    pub fn list_likes(&self) -> Result<Vec<String>, UserDataError> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(LIKES_TABLE) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let mut ids = Vec::new();
        for entry in table.iter()? {
            let entry = entry?;
            ids.push(entry.0.value().to_string());
        }
        Ok(ids)
    }

    pub fn add_like(&self, track_id: &str) -> Result<(), UserDataError> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(LIKES_TABLE)?;
            let value = [1u8];
            table.insert(track_id, value.as_slice())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    pub fn remove_like(&self, track_id: &str) -> Result<(), UserDataError> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = match write_txn.open_table(LIKES_TABLE) {
                Ok(table) => table,
                Err(TableError::TableDoesNotExist(_)) => return Ok(()),
                Err(err) => return Err(err.into()),
            };
            let _ = table.remove(track_id)?;
        }
        write_txn.commit()?;
        Ok(())
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

    pub fn set_playback_settings(
        &self,
        settings: PlaybackSettings,
    ) -> Result<(), UserDataError> {
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
