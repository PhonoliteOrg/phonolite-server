//! Private on-disk catalog schema and index encoding.
//!
//! Table names, key ordering, and serialized values are compatibility contracts.
//! Readers, enrichment transactions, and indexing all use this one owner.
use std::fs;
use std::path::Path;

use common::Album;
use redb::{Database, ReadableTable, TableDefinition, TableError, WriteTransaction};
use serde::{Deserialize, Serialize};

use crate::{LibraryError, LibraryStats};

pub(super) const INDEX_VERSION: u32 = 10;
pub(super) const KEY_SEP: char = '\x1f';

pub(super) const META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
pub(super) const ARTISTS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("artists");
pub(super) const ARTISTS_BY_NAME_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("artists_by_name");
pub(super) const ALBUMS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("albums");
pub(super) const ALBUMS_BY_NAME_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("albums_by_name");
pub(super) const ARTIST_ALBUMS_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("artist_albums");
pub(super) const ALBUM_CREDITS_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("album_credits_v1");
// Optional JSON sidecar; adding it does not change existing positional records
// or require an index rebuild. Local context fingerprints govern cache reuse.
pub(super) const ALBUM_SOURCE_IDENTITY_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("album_source_identity_v1");
pub(super) const TRACK_CREDITS_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("track_credits_v1");
pub(super) const TRACKS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("tracks");
pub(super) const TRACKS_BY_NAME_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("tracks_by_name");
pub(super) const ALBUM_TRACKS_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("album_tracks");
pub(super) const TRACK_EMBEDDED_COVER_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("track_embedded_cover");
pub(super) const SEEK_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("seek");
pub(super) const EXTERNAL_ATTEMPTS_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("external_attempts");
pub(super) const TAG_ERRORS_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("tag_errors");
pub(super) const TAG_ERROR_FILES_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("tag_error_files");

pub(super) const META_VERSION_KEY: &str = "version";
pub(super) const META_STATS_KEY: &str = "stats";
pub(super) const META_ALBUM_METADATA_KEY: &str = "album_metadata_stats_v1";

pub(super) fn open_or_create_db(path: &Path) -> Result<Database, LibraryError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if path.exists() {
        Ok(Database::open(path)?)
    } else {
        Ok(Database::create(path)?)
    }
}

pub(super) fn read_version(db: &Database) -> Result<Option<u32>, LibraryError> {
    let read_txn = db.begin_read()?;
    let table = match read_txn.open_table(META_TABLE) {
        Ok(table) => table,
        Err(TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let version = match table.get(META_VERSION_KEY)? {
        Some(value) => Some(decode_value(value.value())?),
        None => None,
    };
    Ok(version)
}

pub(super) fn read_stats(db: &Database) -> Result<LibraryStats, LibraryError> {
    let read_txn = db.begin_read()?;
    let table = match read_txn.open_table(META_TABLE) {
        Ok(table) => table,
        Err(TableError::TableDoesNotExist(_)) => {
            return Ok(LibraryStats {
                artists: 0,
                albums: 0,
                tracks: 0,
            })
        }
        Err(err) => return Err(err.into()),
    };
    let stats = match table.get(META_STATS_KEY)? {
        Some(value) => decode_value(value.value())?,
        None => LibraryStats {
            artists: 0,
            albums: 0,
            tracks: 0,
        },
    };
    Ok(stats)
}

pub(super) fn clear_table(
    txn: &WriteTransaction,
    table: TableDefinition<&str, &[u8]>,
) -> Result<(), LibraryError> {
    match txn.delete_table(table) {
        Ok(_) => Ok(()),
        Err(TableError::TableDoesNotExist(_)) => Ok(()),
        Err(err) => Err(err.into()),
    }
}

pub(super) fn encode_value<T: Serialize>(value: &T) -> Result<Vec<u8>, LibraryError> {
    Ok(bincode::serialize(value)?)
}

pub(super) fn decode_value<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, LibraryError> {
    Ok(bincode::deserialize(bytes)?)
}

pub(super) fn bool_bytes(value: bool) -> &'static [u8] {
    if value {
        &[1u8]
    } else {
        &[0u8]
    }
}

pub(super) fn artist_name_key(name: &str, artist_id: &str) -> String {
    let mut out = String::new();
    out.push_str(name.trim().to_lowercase().as_str());
    out.push(KEY_SEP);
    out.push_str(artist_id);
    out
}

pub(super) fn album_search_artist_text(album: &Album) -> String {
    if !album.artist_names.is_empty() {
        album.artist_names.join(" ")
    } else {
        String::new()
    }
}

pub(super) fn album_name_key(artist_name: &str, album: &Album) -> String {
    let year = album.year.unwrap_or(9999).clamp(-9999, 9999);
    let mut out = String::new();
    out.push_str(artist_name.trim().to_lowercase().as_str());
    out.push(KEY_SEP);
    out.push_str(&format!("{:04}", year.max(0)));
    out.push(KEY_SEP);
    out.push_str(&album.title.to_lowercase());
    out.push(KEY_SEP);
    out.push_str(&album.id);
    out
}

pub(super) fn album_index_key(artist_id: &str, album: &Album) -> String {
    let year = album.year.unwrap_or(9999).clamp(-9999, 9999);
    let mut out = String::new();
    out.push_str(artist_id);
    out.push(KEY_SEP);
    out.push_str(&format!("{:04}", year.max(0)));
    out.push(KEY_SEP);
    out.push_str(&album.title.to_lowercase());
    out.push(KEY_SEP);
    out.push_str(&album.id);
    out
}

pub(super) fn album_track_key(album_id: &str, order: usize, track_id: &str) -> String {
    let mut out = String::new();
    out.push_str(album_id);
    out.push(KEY_SEP);
    out.push_str(&format!("{:08}", order));
    out.push(KEY_SEP);
    out.push_str(track_id);
    out
}

pub(super) fn track_name_key(
    artist_name: &str,
    album_title: &str,
    disc_no: Option<u16>,
    track_no: Option<u16>,
    title: &str,
    track_id: &str,
) -> String {
    let disc = disc_no.unwrap_or(u16::MAX);
    let track = track_no.unwrap_or(u16::MAX);
    let mut out = String::new();
    out.push_str(artist_name.trim().to_lowercase().as_str());
    out.push(KEY_SEP);
    out.push_str(&album_title.to_lowercase());
    out.push(KEY_SEP);
    out.push_str(&format!("{:05}", disc));
    out.push(KEY_SEP);
    out.push_str(&format!("{:05}", track));
    out.push(KEY_SEP);
    out.push_str(&title.to_lowercase());
    out.push(KEY_SEP);
    out.push_str(track_id);
    out
}

pub(super) fn prefix_key(prefix: &str) -> String {
    let mut out = String::new();
    out.push_str(prefix);
    out.push(KEY_SEP);
    out
}

pub(super) fn split_key_last(value: &str) -> Result<(&str, &str), LibraryError> {
    let idx = value
        .rfind(KEY_SEP)
        .ok_or_else(|| LibraryError::KeyParse(value.to_string()))?;
    let next = idx + KEY_SEP.len_utf8();
    Ok((&value[..idx], &value[next..]))
}
