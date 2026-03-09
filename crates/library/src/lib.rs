use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bincode;
use common::{
    join_relpath, relpath_from, stable_id, Album, Artist, Codec, CoverRef, SeekIndex, SeekPoint,
    Track,
};
use metadata::{read_tags, MetadataError, TagInfo};
use redb::{
    CommitError, Database, DatabaseError, ReadableTable, StorageError, TableDefinition, TableError,
    TransactionError, WriteTransaction,
};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use walkdir::WalkDir;

const INDEX_VERSION: u32 = 8;
const SEEK_STEP_MS: u32 = 5000;
const KEY_SEP: char = '\x1f';
const ROOT_SEP: &str = "::";

const META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const ARTISTS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("artists");
const ARTISTS_BY_NAME_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("artists_by_name");
const ALBUMS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("albums");
const ALBUMS_BY_NAME_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("albums_by_name");
const ARTIST_ALBUMS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("artist_albums");
const TRACKS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("tracks");
const TRACKS_BY_NAME_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("tracks_by_name");
const ALBUM_TRACKS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("album_tracks");
const TRACK_EMBEDDED_COVER_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("track_embedded_cover");
const SEEK_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("seek");
const EXTERNAL_ATTEMPTS_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("external_attempts");
const TAG_ERRORS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("tag_errors");
const TAG_ERROR_FILES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("tag_error_files");

const META_VERSION_KEY: &str = "version";
const META_STATS_KEY: &str = "stats";

#[derive(Clone, Debug)]
pub struct LibraryRoot {
    pub id: String,
    pub path: PathBuf,
}

impl LibraryRoot {
    pub fn new(id: String, path: PathBuf) -> Self {
        Self { id, path }
    }
}

#[derive(Clone)]
pub struct Library {
    roots: Vec<LibraryRoot>,
    db: Arc<Database>,
}

impl Library {
    pub fn load_or_scan(
        roots: Vec<LibraryRoot>,
        db_path: PathBuf,
    ) -> Result<(Self, bool), LibraryError> {
        let db = open_or_create_db(&db_path)?;
        let library = Self {
            roots,
            db: Arc::new(db),
        };

        let mut scanned = false;
        match read_version(&library.db)? {
            Some(version) if version == INDEX_VERSION => {
                info!("Loaded index from {:?}", db_path);
            }
            Some(version) => {
                warn!("Index version mismatch ({}); rescanning", version);
                library.rescan()?;
                scanned = true;
            }
            None => {
                warn!("Index missing; scanning");
                library.rescan()?;
                scanned = true;
            }
        }

        Ok((library, scanned))
    }

    pub fn load_or_scan_with_db(
        roots: Vec<LibraryRoot>,
        db: Arc<Database>,
    ) -> Result<(Self, bool), LibraryError> {
        let library = Self { roots, db };
        let mut scanned = false;
        match read_version(&library.db)? {
            Some(version) if version == INDEX_VERSION => {
                info!("Loaded index from existing database");
            }
            Some(version) => {
                warn!("Index version mismatch ({}); rescanning", version);
                library.rescan()?;
                scanned = true;
            }
            None => {
                warn!("Index missing; scanning");
                library.rescan()?;
                scanned = true;
            }
        }
        Ok((library, scanned))
    }

    pub fn open_db(path: &Path) -> Result<Arc<Database>, LibraryError> {
        let db = open_or_create_db(path)?;
        Ok(Arc::new(db))
    }

    pub fn rescan(&self) -> Result<LibraryStats, LibraryError> {
        scan_library(&self.roots, &self.db)
    }

    pub fn incremental_scan(&self) -> Result<LibraryStats, LibraryError> {
        scan_library_incremental(&self.roots, &self.db)
    }

    pub fn stats(&self) -> Result<LibraryStats, LibraryError> {
        read_stats(&self.db)
    }

    pub fn list_artists(
        &self,
        search: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<Artist>, usize), LibraryError> {
        let search = search
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(|value| value.to_lowercase());

        let read_txn = self.db.begin_read()?;
        let name_table = read_txn.open_table(ARTISTS_BY_NAME_TABLE)?;
        let artist_table = read_txn.open_table(ARTISTS_TABLE)?;
        let artist_album_table = read_txn.open_table(ARTIST_ALBUMS_TABLE)?;

        let mut total = 0usize;
        let mut items = Vec::new();

        for entry in name_table.iter()? {
            let entry = entry?;
            let key = entry.0.value();
            let (name_lower, artist_id) = split_key_last(key)?;
            if let Some(search) = &search {
                if !name_lower.contains(search) {
                    continue;
                }
            }
            let prefix = prefix_key(artist_id);
            let mut end = prefix.clone();
            end.push('\u{10ffff}');
            if artist_album_table
                .range(prefix.as_str()..end.as_str())?
                .next()
                .transpose()?
                .is_none()
            {
                continue;
            }

            total += 1;
            if total <= offset {
                continue;
            }
            if items.len() >= limit {
                continue;
            }

            if let Some(value) = artist_table.get(artist_id)? {
                let artist: Artist = decode_value(value.value())?;
                items.push(artist);
            }
        }

        Ok((items, total))
    }

    pub fn get_artist(&self, artist_id: &str) -> Result<Option<Artist>, LibraryError> {
        let read_txn = self.db.begin_read()?;
        let artist_table = read_txn.open_table(ARTISTS_TABLE)?;
        let artist = match artist_table.get(artist_id)? {
            Some(value) => Some(decode_value(value.value())?),
            None => None,
        };
        Ok(artist)
    }

    pub fn list_artist_albums(&self, artist_id: &str) -> Result<Vec<Album>, LibraryError> {
        let read_txn = self.db.begin_read()?;
        let album_table = read_txn.open_table(ALBUMS_TABLE)?;
        let artist_album_table = read_txn.open_table(ARTIST_ALBUMS_TABLE)?;

        let prefix = prefix_key(artist_id);
        let mut end = prefix.clone();
        end.push('\u{10ffff}');
        let mut albums = Vec::new();

        for entry in artist_album_table.range(prefix.as_str()..end.as_str())? {
            let entry = entry?;
            let key = entry.0.value();
            let (_, album_id) = split_key_last(key)?;
            if let Some(value) = album_table.get(album_id)? {
                let album: Album = decode_value(value.value())?;
                albums.push(album);
            }
        }

        Ok(albums)
    }

    pub fn list_albums(
        &self,
        search: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<Album>, usize), LibraryError> {
        let search = search
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(|value| value.to_lowercase());

        let read_txn = self.db.begin_read()?;
        let name_table = read_txn.open_table(ALBUMS_BY_NAME_TABLE)?;
        let album_table = read_txn.open_table(ALBUMS_TABLE)?;

        let mut total = 0usize;
        let mut items = Vec::new();

        for entry in name_table.iter()? {
            let entry = entry?;
            let key = entry.0.value();
            let (_, album_id) = split_key_last(key)?;
            if let Some(search) = &search {
                if !key.contains(search) {
                    continue;
                }
            }

            total += 1;
            if total <= offset {
                continue;
            }
            if items.len() >= limit {
                continue;
            }

            if let Some(value) = album_table.get(album_id)? {
                let album: Album = decode_value(value.value())?;
                items.push(album);
            }
        }

        Ok((items, total))
    }

    pub fn list_tracks(
        &self,
        search: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<Track>, usize), LibraryError> {
        let search = search
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(|value| value.to_lowercase());

        let read_txn = self.db.begin_read()?;
        let name_table = read_txn.open_table(TRACKS_BY_NAME_TABLE)?;
        let track_table = read_txn.open_table(TRACKS_TABLE)?;

        let mut total = 0usize;
        let mut items = Vec::new();

        for entry in name_table.iter()? {
            let entry = entry?;
            let key = entry.0.value();
            let (_, track_id) = split_key_last(key)?;
            if let Some(search) = &search {
                if !key.contains(search) {
                    continue;
                }
            }

            total += 1;
            if total <= offset {
                continue;
            }
            if items.len() >= limit {
                continue;
            }

            if let Some(value) = track_table.get(track_id)? {
                let track: Track = decode_value(value.value())?;
                items.push(track);
            }
        }

        Ok((items, total))
    }

    pub fn list_tag_errors(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<TagErrorInfo>, usize), LibraryError> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(TAG_ERRORS_TABLE) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok((Vec::new(), 0)),
            Err(err) => return Err(err.into()),
        };

        let mut total = 0usize;
        let mut items = Vec::new();
        for entry in table.iter()? {
            let entry = entry?;
            total += 1;
            if total <= offset {
                continue;
            }
            if items.len() >= limit {
                continue;
            }
            let info: TagErrorInfo = decode_value(entry.1.value())?;
            items.push(info);
        }

        Ok((items, total))
    }

    pub fn list_tag_error_files(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<TagErrorFile>, usize), LibraryError> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(TAG_ERROR_FILES_TABLE) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok((Vec::new(), 0)),
            Err(err) => return Err(err.into()),
        };

        let mut total = 0usize;
        let mut items = Vec::new();
        for entry in table.iter()? {
            let entry = entry?;
            total += 1;
            if total <= offset {
                continue;
            }
            if items.len() >= limit {
                continue;
            }
            let info: TagErrorFile = decode_value(entry.1.value())?;
            items.push(info);
        }

        Ok((items, total))
    }

    pub fn get_album(&self, album_id: &str) -> Result<Option<Album>, LibraryError> {
        let read_txn = self.db.begin_read()?;
        let album_table = read_txn.open_table(ALBUMS_TABLE)?;
        let album = match album_table.get(album_id)? {
            Some(value) => Some(decode_value(value.value())?),
            None => None,
        };
        Ok(album)
    }

    pub fn get_album_tracks(&self, album_id: &str) -> Result<Vec<Track>, LibraryError> {
        let read_txn = self.db.begin_read()?;
        let track_table = read_txn.open_table(TRACKS_TABLE)?;
        let album_track_table = read_txn.open_table(ALBUM_TRACKS_TABLE)?;

        let prefix = prefix_key(album_id);
        let mut end = prefix.clone();
        end.push('\u{10ffff}');
        let mut tracks = Vec::new();

        for entry in album_track_table.range(prefix.as_str()..end.as_str())? {
            let entry = entry?;
            let key = entry.0.value();
            let (_, track_id) = split_key_last(key)?;
            if let Some(value) = track_table.get(track_id)? {
                let track: Track = decode_value(value.value())?;
                tracks.push(track);
            }
        }

        Ok(tracks)
    }

    pub fn get_track(&self, track_id: &str) -> Result<Option<Track>, LibraryError> {
        let read_txn = self.db.begin_read()?;
        let track_table = read_txn.open_table(TRACKS_TABLE)?;
        let track = match track_table.get(track_id)? {
            Some(value) => Some(decode_value(value.value())?),
            None => None,
        };
        Ok(track)
    }

    pub fn get_seek(&self, track_id: &str) -> Result<Option<SeekIndex>, LibraryError> {
        let read_txn = self.db.begin_read()?;
        let seek_table = read_txn.open_table(SEEK_TABLE)?;
        let seek = match seek_table.get(track_id)? {
            Some(value) => Some(decode_value(value.value())?),
            None => None,
        };
        Ok(seek)
    }

    pub fn track_has_embedded_cover(&self, track_id: &str) -> Result<bool, LibraryError> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(TRACK_EMBEDDED_COVER_TABLE)?;
        let has_cover = match table.get(track_id)? {
            Some(value) => value.value().first().copied().unwrap_or(0) != 0,
            None => false,
        };
        Ok(has_cover)
    }

    pub fn update_artist_enrichment(
        &self,
        artist_id: &str,
        summary: Option<String>,
        genres: &[String],
        logo_ref: Option<String>,
        banner_ref: Option<String>,
        replace: bool,
    ) -> Result<bool, LibraryError> {
        let write_txn = self.db.begin_write()?;
        let updated = {
            let mut artist_table = write_txn.open_table(ARTISTS_TABLE)?;
            let mut artist: Artist = match artist_table.get(artist_id)? {
                Some(value) => decode_value(value.value())?,
                None => return Ok(false),
            };

            let mut updated = false;
            if let Some(summary) = summary.and_then(clean_summary) {
                let should_update = replace
                    || artist
                        .summary
                        .as_ref()
                        .map(|s| s.trim().is_empty())
                        .unwrap_or(true);
                if should_update && artist.summary.as_deref() != Some(summary.as_str()) {
                    artist.summary = Some(summary);
                    updated = true;
                }
            }
            if !genres.is_empty() {
                if replace {
                    let mut normalized = Vec::new();
                    merge_genres(&mut normalized, genres);
                    if artist.genres != normalized {
                        artist.genres = normalized;
                        updated = true;
                    }
                } else {
                    let before = artist.genres.len();
                    merge_genres(&mut artist.genres, genres);
                    if artist.genres.len() != before {
                        updated = true;
                    }
                }
            }
            if let Some(logo_ref) = logo_ref {
                if artist.logo_ref.as_deref() != Some(logo_ref.as_str()) {
                    artist.logo_ref = Some(logo_ref);
                    updated = true;
                }
            }
            if let Some(banner_ref) = banner_ref {
                if artist.banner_ref.as_deref() != Some(banner_ref.as_str()) {
                    artist.banner_ref = Some(banner_ref);
                    updated = true;
                }
            }

            if updated {
                let artist_bytes = encode_value(&artist)?;
                artist_table.insert(artist_id, artist_bytes.as_slice())?;
            }
            updated
        };

        if updated {
            write_txn.commit()?;
        }
        Ok(updated)
    }

    pub fn update_album_enrichment(
        &self,
        album_id: &str,
        summary: Option<String>,
        genres: &[String],
    ) -> Result<bool, LibraryError> {
        let write_txn = self.db.begin_write()?;
        let updated = {
            let mut album_table = write_txn.open_table(ALBUMS_TABLE)?;
            let mut album: Album = match album_table.get(album_id)? {
                Some(value) => decode_value(value.value())?,
                None => return Ok(false),
            };

            let mut updated = false;
            if album
                .summary
                .as_ref()
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
            {
                if let Some(summary) = summary.and_then(clean_summary) {
                    album.summary = Some(summary);
                    updated = true;
                }
            }
            if !genres.is_empty() {
                let before = album.genres.len();
                merge_genres(&mut album.genres, genres);
                if album.genres.len() != before {
                    updated = true;
                }
            }

            if updated {
                let album_bytes = encode_value(&album)?;
                album_table.insert(album_id, album_bytes.as_slice())?;
            }
            updated
        };

        if updated {
            write_txn.commit()?;
        }
        Ok(updated)
    }

    pub fn merge_album_artists(
        &self,
        album_id: &str,
        artists: &[Artist],
    ) -> Result<bool, LibraryError> {
        let artists = normalize_album_artists(artists);
        if artists.is_empty() {
            return Ok(false);
        }

        let write_txn = self.db.begin_write()?;
        let (updated, new_artist_count) = {
            let mut meta_table = write_txn.open_table(META_TABLE)?;
            let mut artists_table = write_txn.open_table(ARTISTS_TABLE)?;
            let mut artists_by_name_table = write_txn.open_table(ARTISTS_BY_NAME_TABLE)?;
            let mut albums_table = write_txn.open_table(ALBUMS_TABLE)?;
            let mut albums_by_name_table = write_txn.open_table(ALBUMS_BY_NAME_TABLE)?;
            let mut artist_albums_table = write_txn.open_table(ARTIST_ALBUMS_TABLE)?;
            let mut tracks_table = write_txn.open_table(TRACKS_TABLE)?;
            let mut tracks_by_name_table = write_txn.open_table(TRACKS_BY_NAME_TABLE)?;
            let album_tracks_table = write_txn.open_table(ALBUM_TRACKS_TABLE)?;

            let mut album: Album = match albums_table.get(album_id)? {
                Some(value) => decode_value(value.value())?,
                None => return Ok(false),
            };

            let old_artist_ids = album.all_artist_ids().to_vec();
            let old_album_name_key = album_name_key(&album_search_artist_text(&album), &album);
            let old_primary_artist_id = album.artist_id.clone();
            let old_primary_artist_name = album
                .artist_names
                .first()
                .cloned()
                .filter(|name| !name.trim().is_empty())
                .or_else(|| {
                    artists_table
                        .get(old_primary_artist_id.as_str())
                        .ok()
                        .flatten()
                        .and_then(|value| decode_value::<Artist>(value.value()).ok())
                        .map(|artist| artist.name)
                        .filter(|name| !name.trim().is_empty())
                })
                .unwrap_or_default();
            let new_artist_ids = artists
                .iter()
                .map(|artist| artist.id.clone())
                .collect::<Vec<_>>();
            let new_artist_names = artists
                .iter()
                .map(|artist| artist.name.clone())
                .collect::<Vec<_>>();

            let mut new_artist_count = 0usize;
            for artist in &artists {
                let existing = match artists_table.get(artist.id.as_str())? {
                    Some(value) => Some(decode_value::<Artist>(value.value())?),
                    None => None,
                };
                match existing {
                    Some(mut existing) => {
                        let mut changed = false;
                        if existing.name.trim().is_empty() && !artist.name.trim().is_empty() {
                            existing.name = artist.name.clone();
                            changed = true;
                        }
                        if existing.summary.is_none() && artist.summary.is_some() {
                            existing.summary = artist.summary.clone();
                            changed = true;
                        }
                        if existing.logo_ref.is_none() && artist.logo_ref.is_some() {
                            existing.logo_ref = artist.logo_ref.clone();
                            changed = true;
                        }
                        if existing.banner_ref.is_none() && artist.banner_ref.is_some() {
                            existing.banner_ref = artist.banner_ref.clone();
                            changed = true;
                        }
                        let genre_count = existing.genres.len();
                        merge_genres(&mut existing.genres, &artist.genres);
                        if existing.genres.len() != genre_count {
                            changed = true;
                        }
                        if changed {
                            let bytes = encode_value(&existing)?;
                            artists_table.insert(existing.id.as_str(), bytes.as_slice())?;
                        }
                        if !existing.name.trim().is_empty() {
                            let key = artist_name_key(&existing.name, &existing.id);
                            artists_by_name_table.insert(key.as_str(), existing.id.as_bytes())?;
                        }
                    }
                    None => {
                        let bytes = encode_value(artist)?;
                        artists_table.insert(artist.id.as_str(), bytes.as_slice())?;
                        if !artist.name.trim().is_empty() {
                            let key = artist_name_key(&artist.name, &artist.id);
                            artists_by_name_table.insert(key.as_str(), artist.id.as_bytes())?;
                        }
                        new_artist_count = new_artist_count.saturating_add(1);
                    }
                }
            }

            let mut updated = false;
            let primary_artist_id = new_artist_ids[0].clone();
            let primary_artist_name = new_artist_names.first().cloned().unwrap_or_default();
            if album.artist_id != primary_artist_id {
                album.artist_id = primary_artist_id.clone();
                updated = true;
            }
            if album.artist_ids != new_artist_ids {
                album.artist_ids = new_artist_ids.clone();
                updated = true;
            }
            if album.artist_names != new_artist_names {
                album.artist_names = new_artist_names;
                updated = true;
            }

            let new_album_name_key = album_name_key(&album_search_artist_text(&album), &album);
            if old_album_name_key != new_album_name_key {
                let _ = albums_by_name_table.remove(old_album_name_key.as_str())?;
                albums_by_name_table.insert(new_album_name_key.as_str(), album.id.as_bytes())?;
            }

            if old_artist_ids != album.all_artist_ids() {
                for artist_id in &old_artist_ids {
                    let key = album_index_key(artist_id, &album);
                    let _ = artist_albums_table.remove(key.as_str())?;
                }
                for artist_id in album.all_artist_ids() {
                    let key = album_index_key(artist_id, &album);
                    artist_albums_table.insert(key.as_str(), album.id.as_bytes())?;
                }
            }

            if old_primary_artist_id != primary_artist_id
                || old_primary_artist_name != primary_artist_name
            {
                let prefix = prefix_key(album.id.as_str());
                let mut end = prefix.clone();
                end.push('\u{10ffff}');
                for entry in album_tracks_table.range(prefix.as_str()..end.as_str())? {
                    let entry = entry?;
                    let key = entry.0.value();
                    let (_, track_id) = split_key_last(key)?;
                    let mut track: Track = {
                        let Some(value) = tracks_table.get(track_id)? else {
                            continue;
                        };
                        decode_value(value.value())?
                    };
                    let old_track_name_key = track_name_key(
                        &old_primary_artist_name,
                        &album.title,
                        track.disc_no,
                        track.track_no,
                        &track.title,
                        &track.id,
                    );
                    let new_track_name_key = track_name_key(
                        &primary_artist_name,
                        &album.title,
                        track.disc_no,
                        track.track_no,
                        &track.title,
                        &track.id,
                    );

                    if track.artist_id != primary_artist_id {
                        track.artist_id = primary_artist_id.clone();
                        updated = true;
                    }
                    if old_track_name_key != new_track_name_key {
                        let _ = tracks_by_name_table.remove(old_track_name_key.as_str())?;
                        tracks_by_name_table
                            .insert(new_track_name_key.as_str(), track.id.as_bytes())?;
                        updated = true;
                    }

                    let bytes = encode_value(&track)?;
                    tracks_table.insert(track.id.as_str(), bytes.as_slice())?;
                }
            }

            if updated {
                let bytes = encode_value(&album)?;
                albums_table.insert(album.id.as_str(), bytes.as_slice())?;
            }

            if new_artist_count > 0 {
                let mut stats: LibraryStats = match meta_table.get(META_STATS_KEY)? {
                    Some(value) => decode_value(value.value())?,
                    None => LibraryStats {
                        artists: 0,
                        albums: 0,
                        tracks: 0,
                    },
                };
                stats.artists = stats.artists.saturating_add(new_artist_count);
                let bytes = encode_value(&stats)?;
                meta_table.insert(META_STATS_KEY, bytes.as_slice())?;
            }

            (updated || new_artist_count > 0, new_artist_count)
        };

        if updated || new_artist_count > 0 {
            write_txn.commit()?;
        }

        Ok(updated)
    }

    pub fn should_attempt_external(
        &self,
        key: &str,
        min_interval: Duration,
    ) -> Result<bool, LibraryError> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(EXTERNAL_ATTEMPTS_TABLE) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(true),
            Err(err) => return Err(err.into()),
        };
        let attempt = match table.get(key)? {
            Some(value) => decode_value::<ExternalAttempt>(value.value())?,
            None => return Ok(true),
        };
        let now = now_secs();
        let min_interval = min_interval.as_secs();
        Ok(now.saturating_sub(attempt.last_attempt) >= min_interval)
    }

    pub fn record_external_attempt(&self, key: &str, success: bool) -> Result<(), LibraryError> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(EXTERNAL_ATTEMPTS_TABLE)?;
            let now = now_secs();
            let mut record = match table.get(key)? {
                Some(value) => decode_value::<ExternalAttempt>(value.value())?,
                None => ExternalAttempt {
                    last_attempt: now,
                    last_success: None,
                },
            };
            record.last_attempt = now;
            if success {
                record.last_success = Some(now);
            }
            let bytes = encode_value(&record)?;
            table.insert(key, bytes.as_slice())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    pub fn roots(&self) -> &[LibraryRoot] {
        &self.roots
    }

    pub fn root(&self) -> &Path {
        self.default_root()
            .map(|root| root.path.as_path())
            .unwrap_or_else(|| Path::new("."))
    }

    pub fn resolve_relpath(&self, relpath: &str) -> Option<PathBuf> {
        let (prefix, rest) = split_root_relpath(relpath);
        if let Some(prefix) = prefix {
            if let Some(root) = self.root_by_id(prefix) {
                return Some(join_relpath(&root.path, rest));
            }
        }
        let root = self.default_root()?;
        Some(join_relpath(&root.path, relpath))
    }

    pub fn db(&self) -> Arc<Database> {
        Arc::clone(&self.db)
    }

    fn root_by_id(&self, id: &str) -> Option<&LibraryRoot> {
        self.roots.iter().find(|root| root.id == id)
    }

    fn default_root(&self) -> Option<&LibraryRoot> {
        self.roots
            .iter()
            .find(|root| root.id.is_empty())
            .or_else(|| self.roots.first())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LibraryStats {
    pub artists: usize,
    pub albums: usize,
    pub tracks: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TagErrorInfo {
    pub album_id: String,
    pub artist_id: String,
    pub album_title: String,
    pub artist_name: String,
    pub folder_relpath: String,
    pub last_seen: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TagErrorFile {
    pub file_relpath: String,
    pub folder_relpath: String,
    pub error: String,
    pub last_seen: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ExternalAttempt {
    last_attempt: u64,
    last_success: Option<u64>,
}

#[derive(Debug)]
pub enum LibraryError {
    Io(std::io::Error),
    Metadata(MetadataError),
    Redb(redb::Error),
    Bincode(Box<bincode::ErrorKind>),
    KeyParse(String),
    VersionMismatch(u32),
}

impl std::fmt::Display for LibraryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LibraryError::Io(err) => write!(f, "io error: {}", err),
            LibraryError::Metadata(err) => write!(f, "metadata error: {:?}", err),
            LibraryError::Redb(err) => write!(f, "db error: {}", err),
            LibraryError::Bincode(err) => write!(f, "bincode error: {}", err),
            LibraryError::KeyParse(value) => write!(f, "key parse error: {}", value),
            LibraryError::VersionMismatch(version) => {
                write!(f, "index version mismatch: {}", version)
            }
        }
    }
}

impl std::error::Error for LibraryError {}

impl From<std::io::Error> for LibraryError {
    fn from(err: std::io::Error) -> Self {
        LibraryError::Io(err)
    }
}

impl From<MetadataError> for LibraryError {
    fn from(err: MetadataError) -> Self {
        LibraryError::Metadata(err)
    }
}

impl From<redb::Error> for LibraryError {
    fn from(err: redb::Error) -> Self {
        LibraryError::Redb(err)
    }
}

impl From<DatabaseError> for LibraryError {
    fn from(err: DatabaseError) -> Self {
        LibraryError::Redb(err.into())
    }
}

impl From<TableError> for LibraryError {
    fn from(err: TableError) -> Self {
        LibraryError::Redb(err.into())
    }
}

impl From<TransactionError> for LibraryError {
    fn from(err: TransactionError) -> Self {
        LibraryError::Redb(err.into())
    }
}

impl From<StorageError> for LibraryError {
    fn from(err: StorageError) -> Self {
        LibraryError::Redb(err.into())
    }
}

impl From<CommitError> for LibraryError {
    fn from(err: CommitError) -> Self {
        LibraryError::Redb(err.into())
    }
}

impl From<Box<bincode::ErrorKind>> for LibraryError {
    fn from(err: Box<bincode::ErrorKind>) -> Self {
        LibraryError::Bincode(err)
    }
}

fn open_or_create_db(path: &Path) -> Result<Database, LibraryError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if path.exists() {
        Ok(Database::open(path)?)
    } else {
        Ok(Database::create(path)?)
    }
}

fn read_version(db: &Database) -> Result<Option<u32>, LibraryError> {
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

fn read_stats(db: &Database) -> Result<LibraryStats, LibraryError> {
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

fn scan_library(roots: &[LibraryRoot], db: &Database) -> Result<LibraryStats, LibraryError> {
    let mut album_dirs = Vec::new();
    for root in roots {
        album_dirs.extend(
            collect_album_dirs(&root.path)
                .into_iter()
                .map(|dir| (root, dir)),
        );
    }
    info!("Found {} album folders", album_dirs.len());

    let write_txn = db.begin_write()?;

    clear_table(&write_txn, META_TABLE)?;
    clear_table(&write_txn, ARTISTS_TABLE)?;
    clear_table(&write_txn, ARTISTS_BY_NAME_TABLE)?;
    clear_table(&write_txn, ALBUMS_TABLE)?;
    clear_table(&write_txn, ALBUMS_BY_NAME_TABLE)?;
    clear_table(&write_txn, ARTIST_ALBUMS_TABLE)?;
    clear_table(&write_txn, TRACKS_TABLE)?;
    clear_table(&write_txn, TRACKS_BY_NAME_TABLE)?;
    clear_table(&write_txn, ALBUM_TRACKS_TABLE)?;
    clear_table(&write_txn, TRACK_EMBEDDED_COVER_TABLE)?;
    clear_table(&write_txn, SEEK_TABLE)?;
    clear_table(&write_txn, EXTERNAL_ATTEMPTS_TABLE)?;
    clear_table(&write_txn, TAG_ERRORS_TABLE)?;
    clear_table(&write_txn, TAG_ERROR_FILES_TABLE)?;

    let stats = {
        let mut meta_table = write_txn.open_table(META_TABLE)?;
        let mut artists_table = write_txn.open_table(ARTISTS_TABLE)?;
        let mut artists_by_name_table = write_txn.open_table(ARTISTS_BY_NAME_TABLE)?;
        let mut albums_table = write_txn.open_table(ALBUMS_TABLE)?;
        let mut albums_by_name_table = write_txn.open_table(ALBUMS_BY_NAME_TABLE)?;
        let mut artist_albums_table = write_txn.open_table(ARTIST_ALBUMS_TABLE)?;
        let mut tracks_table = write_txn.open_table(TRACKS_TABLE)?;
        let mut tracks_by_name_table = write_txn.open_table(TRACKS_BY_NAME_TABLE)?;
        let mut album_tracks_table = write_txn.open_table(ALBUM_TRACKS_TABLE)?;
        let mut embedded_cover_table = write_txn.open_table(TRACK_EMBEDDED_COVER_TABLE)?;
        let mut seek_table = write_txn.open_table(SEEK_TABLE)?;
        let mut tag_errors_table = write_txn.open_table(TAG_ERRORS_TABLE)?;
        let mut tag_error_files_table = write_txn.open_table(TAG_ERROR_FILES_TABLE)?;

        let mut artist_count = 0usize;
        let mut album_count = 0usize;
        let mut track_count = 0usize;
        let mut artist_sidecar_cache: HashMap<PathBuf, Option<SidecarInfo>> = HashMap::new();

        for (root, album_dir) in album_dirs {
            let files = audio_files_in_dir(&album_dir);
            if files.is_empty() {
                continue;
            }

            let folder_relpath = match relpath_from(&root.path, &album_dir) {
                Some(rel) => prefix_relpath(&root.id, &rel),
                None => continue,
            };
            info!(
                "Scanning album folder '{}' ({} audio files)",
                folder_relpath.as_str(),
                files.len()
            );

            let folder_name = album_dir
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "Unknown Album".to_string());
            let (folder_title, folder_year) = split_title_year(&folder_name);

            let fallback_artist = album_dir
                .parent()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "Unknown Artist".to_string());

            let album_sidecar = read_sidecar_info(&album_dir.join("album.json"));
            let artist_sidecar = album_dir.parent().and_then(|parent| {
                load_sidecar_info(&mut artist_sidecar_cache, parent.join("artist.json"))
            });
            if let Some(info) = &album_sidecar {
                info!(
                    "Album sidecar loaded for '{}': summary_present={}, genres={:?}",
                    folder_relpath.as_str(),
                    info.summary
                        .as_ref()
                        .map(|summary| !summary.trim().is_empty())
                        .unwrap_or(false),
                    info.genres
                );
            }
            if let Some(info) = &artist_sidecar {
                info!(
                    "Artist sidecar loaded for '{}': summary_present={}, genres={:?}",
                    fallback_artist.as_str(),
                    info.summary
                        .as_ref()
                        .map(|summary| !summary.trim().is_empty())
                        .unwrap_or(false),
                    info.genres
                );
            }

            let mut album_title: Option<String> = None;
            let mut album_artist: Option<String> = None;
            let mut album_year: Option<i32> = None;
            let mut album_cover: Option<CoverRef> = None;
            let mut album_summary: Option<String> = None;
            let mut album_genres: Vec<String> = Vec::new();
            let mut track_drafts = Vec::new();
            let mut album_tag_error = false;
            let mut tag_error_files: Vec<TagErrorFile> = Vec::new();

            if let Some(info) = &album_sidecar {
                if info.summary.as_ref().is_some() {
                    album_summary = info.summary.clone();
                }
                merge_genres(&mut album_genres, &info.genres);
            }

            for file in files {
                let relpath = match relpath_from(&root.path, &file) {
                    Some(rel) => prefix_relpath(&root.id, &rel),
                    None => continue,
                };
                info!("Reading tags for '{}'", relpath.as_str());

                let tag = match read_tags(&file) {
                    Ok(tag) => tag,
                    Err(err) => {
                        warn!(
                            target: "issue",
                            "file={} | folder={} | error={:?}",
                            relpath.as_str(),
                            folder_relpath.as_str(),
                            err
                        );
                        album_tag_error = true;
                        tag_error_files.push(TagErrorFile {
                            file_relpath: relpath.clone(),
                            folder_relpath: folder_relpath.clone(),
                            error: format!("{:?}", err),
                            last_seen: now_secs(),
                        });
                        TagInfo::default()
                    }
                };

                if album_title.is_none() {
                    album_title = tag.album.clone();
                }
                if album_artist.is_none() {
                    album_artist = tag.album_artist.clone().or_else(|| tag.artist.clone());
                }
                if album_year.is_none() {
                    album_year = tag.year;
                }
                if album_summary.is_none() {
                    album_summary = tag.summary.clone();
                }
                if !tag.genres.is_empty() {
                    merge_genres(&mut album_genres, &tag.genres);
                }

                info!(
                    "Tag fields for '{}': album={:?}, album_artist={:?}, artist={:?}, year={:?}, title={:?}, track_no={:?}, disc_no={:?}, summary_present={}, genres={:?}",
                    relpath.as_str(),
                    tag.album.as_deref(),
                    tag.album_artist.as_deref(),
                    tag.artist.as_deref(),
                    tag.year,
                    tag.title.as_deref(),
                    tag.track_no,
                    tag.disc_no,
                    tag.summary
                        .as_ref()
                        .map(|summary| !summary.trim().is_empty())
                        .unwrap_or(false),
                    &tag.genres
                );

                let title = tag.title.clone().unwrap_or_else(|| file_stem(&file));
                let duration_ms = tag.duration_ms.unwrap_or(0);
                let codec = match audio_codec(&file) {
                    Some(codec) => codec,
                    None => continue,
                };
                let file_size = fs::metadata(&file)?.len();
                let id = stable_id(&relpath);
                let disc_no = tag
                    .disc_no
                    .or_else(|| disc_number_from_path(&file, &album_dir));
                let album_hint = tag
                    .album
                    .clone()
                    .or_else(|| album_title.clone())
                    .unwrap_or_else(|| folder_title.clone());
                let artist_hint = tag
                    .album_artist
                    .clone()
                    .or_else(|| tag.artist.clone())
                    .or_else(|| album_artist.clone())
                    .unwrap_or_else(|| fallback_artist.clone());
                let year_hint = tag.year.or(album_year).or(folder_year);
                info!(
                    "Indexing track '{}' for album '{}' (artist='{}', year={:?})",
                    title.as_str(),
                    album_hint,
                    artist_hint,
                    year_hint
                );
                info!(
                    "Track metadata: relpath='{}', track_no={:?}, disc_no={:?}, duration_ms={}, codec={:?}, sample_rate={:?}, channels={:?}, bitrate={:?}, genres={:?}, embedded_cover={}",
                    relpath.as_str(),
                    tag.track_no,
                    disc_no,
                    duration_ms,
                    &codec,
                    tag.sample_rate,
                    tag.channels,
                    tag.bitrate,
                    &tag.genres,
                    tag.has_embedded_cover
                );

                if tag.has_embedded_cover && album_cover.is_none() {
                    album_cover = Some(CoverRef::Embedded {
                        track_id: id.clone(),
                    });
                    info!(
                        "Album cover set from embedded art in '{}'",
                        relpath.as_str()
                    );
                } else if tag.has_embedded_cover {
                    info!("Found embedded art in '{}'", relpath.as_str());
                }

                embedded_cover_table.insert(id.as_str(), bool_bytes(tag.has_embedded_cover))?;

                let seek = build_seek_index(duration_ms, file_size);
                let seek_bytes = encode_value(&seek)?;
                seek_table.insert(id.as_str(), seek_bytes.as_slice())?;

                track_drafts.push(TrackDraft {
                    id,
                    relpath,
                    title,
                    track_no: tag.track_no,
                    disc_no,
                    duration_ms,
                    codec,
                    sample_rate: tag.sample_rate,
                    channels: tag.channels,
                    bitrate: tag.bitrate,
                    file_size,
                    genres: tag.genres,
                });
            }

            if track_drafts.is_empty() {
                continue;
            }

            if album_year.is_none() {
                album_year = folder_year;
            }

            let album_title = album_title.unwrap_or(folder_title);
            let album_artist = album_artist.unwrap_or(fallback_artist).trim().to_string();
            let artist_id = stable_id(album_artist.trim());
            let album_id = stable_id(&folder_relpath);
            info!(
                "Detected album '{}' (artist='{}', year={:?}) with {} tracks",
                album_title.as_str(),
                album_artist.as_str(),
                album_year,
                track_drafts.len()
            );

            if album_cover.is_none() {
                if let Some(cover_rel) = find_folder_cover(&root.path, &root.id, &album_dir) {
                    info!(
                        "Found folder cover for album '{}': {}",
                        album_title.as_str(),
                        cover_rel.as_str()
                    );
                    album_cover = Some(CoverRef::File { relpath: cover_rel });
                } else {
                    info!(
                        "No folder cover detected for album '{}'",
                        album_title.as_str()
                    );
                }
            }

            let mut artist_genres = Vec::new();
            merge_genres(&mut artist_genres, &album_genres);
            if let Some(info) = &artist_sidecar {
                merge_genres(&mut artist_genres, &info.genres);
            }
            let mut artist_summary = artist_sidecar
                .as_ref()
                .and_then(|info| info.summary.clone());
            let mut artist_logo = None;
            let mut artist_banner = None;

            if let Some(value) = artists_table.get(artist_id.as_str())? {
                let existing: Artist = decode_value(value.value())?;
                merge_genres(&mut artist_genres, &existing.genres);
                if artist_summary.is_none() {
                    artist_summary = existing.summary;
                }
                if artist_logo.is_none() {
                    artist_logo = existing.logo_ref;
                }
                if artist_banner.is_none() {
                    artist_banner = existing.banner_ref;
                }
            }

            let artist = Artist {
                id: artist_id.clone(),
                name: album_artist.clone(),
                genres: artist_genres,
                summary: artist_summary,
                logo_ref: artist_logo,
                banner_ref: artist_banner,
            };
            let artist_bytes = encode_value(&artist)?;
            let prev = artists_table.insert(artist_id.as_str(), artist_bytes.as_slice())?;
            if prev.is_none() {
                artist_count += 1;
            }

            let artist_name_key = artist_name_key(&artist.name, &artist.id);
            artists_by_name_table.insert(artist_name_key.as_str(), artist.id.as_bytes())?;

            let album = Album {
                id: album_id.clone(),
                artist_id: artist_id.clone(),
                artist_ids: vec![artist_id.clone()],
                artist_names: vec![artist.name.clone()],
                title: album_title,
                year: album_year,
                folder_relpath,
                cover_ref: album_cover,
                genres: album_genres.clone(),
                summary: album_summary,
            };

            let album_bytes = encode_value(&album)?;
            let prev = albums_table.insert(album_id.as_str(), album_bytes.as_slice())?;
            if prev.is_none() {
                album_count += 1;
            }

            let album_name_key = album_name_key(&artist.name, &album);
            albums_by_name_table.insert(album_name_key.as_str(), album.id.as_bytes())?;

            let album_index_key = album_index_key(&artist_id, &album);
            artist_albums_table.insert(album_index_key.as_str(), album_id.as_bytes())?;

            if album_tag_error {
                warn!(
                    target: "issue",
                    "album={} | folder={} | files={}",
                    album.title.as_str(),
                    album.folder_relpath.as_str(),
                    tag_error_files.len()
                );
                let tag_error = TagErrorInfo {
                    album_id: album.id.clone(),
                    artist_id: artist.id.clone(),
                    album_title: album.title.clone(),
                    artist_name: artist.name.clone(),
                    folder_relpath: album.folder_relpath.clone(),
                    last_seen: now_secs(),
                };
                let tag_bytes = encode_value(&tag_error)?;
                tag_errors_table.insert(album.id.as_str(), tag_bytes.as_slice())?;
            }

            for info in tag_error_files {
                let bytes = encode_value(&info)?;
                tag_error_files_table.insert(info.file_relpath.as_str(), bytes.as_slice())?;
            }

            track_drafts.sort_by(|a, b| {
                let disc_a = a.disc_no.unwrap_or(u16::MAX);
                let disc_b = b.disc_no.unwrap_or(u16::MAX);
                let track_a = a.track_no.unwrap_or(u16::MAX);
                let track_b = b.track_no.unwrap_or(u16::MAX);
                disc_a
                    .cmp(&disc_b)
                    .then_with(|| track_a.cmp(&track_b))
                    .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
                    .then_with(|| a.relpath.cmp(&b.relpath))
            });

            for (order, draft) in track_drafts.into_iter().enumerate() {
                let TrackDraft {
                    id,
                    relpath,
                    title,
                    track_no,
                    disc_no,
                    duration_ms,
                    codec,
                    sample_rate,
                    channels,
                    bitrate,
                    file_size,
                    genres,
                } = draft;
                let mut track_genres = genres;
                merge_genres(&mut track_genres, &album_genres);
                let track = Track {
                    id,
                    album_id: album_id.clone(),
                    artist_id: artist_id.clone(),
                    title,
                    track_no,
                    disc_no,
                    duration_ms,
                    codec,
                    sample_rate,
                    channels,
                    bitrate,
                    file_relpath: relpath,
                    file_size,
                    genres: track_genres,
                };

                let track_bytes = encode_value(&track)?;
                let prev = tracks_table.insert(track.id.as_str(), track_bytes.as_slice())?;
                if prev.is_none() {
                    track_count += 1;
                }

                let track_index_key = album_track_key(&album_id, order, &track.id);
                album_tracks_table.insert(track_index_key.as_str(), track.id.as_bytes())?;

                let track_name_key = track_name_key(
                    &artist.name,
                    &album.title,
                    track.disc_no,
                    track.track_no,
                    &track.title,
                    &track.id,
                );
                tracks_by_name_table.insert(track_name_key.as_str(), track.id.as_bytes())?;
            }
        }

        let stats = LibraryStats {
            artists: artist_count,
            albums: album_count,
            tracks: track_count,
        };

        let version_bytes = encode_value(&INDEX_VERSION)?;
        meta_table.insert(META_VERSION_KEY, version_bytes.as_slice())?;
        let stats_bytes = encode_value(&stats)?;
        meta_table.insert(META_STATS_KEY, stats_bytes.as_slice())?;

        stats
    };

    write_txn.commit()?;
    Ok(stats)
}

fn scan_library_incremental(
    roots: &[LibraryRoot],
    db: &Database,
) -> Result<LibraryStats, LibraryError> {
    let mut album_dirs = Vec::new();
    for root in roots {
        album_dirs.extend(
            collect_album_dirs(&root.path)
                .into_iter()
                .map(|dir| (root, dir)),
        );
    }
    info!("Found {} album folders", album_dirs.len());

    let mut running_stats = read_stats(db)?;
    let write_txn = db.begin_write()?;

    let stats = {
        let mut meta_table = write_txn.open_table(META_TABLE)?;
        let mut artists_table = write_txn.open_table(ARTISTS_TABLE)?;
        let mut artists_by_name_table = write_txn.open_table(ARTISTS_BY_NAME_TABLE)?;
        let mut albums_table = write_txn.open_table(ALBUMS_TABLE)?;
        let mut albums_by_name_table = write_txn.open_table(ALBUMS_BY_NAME_TABLE)?;
        let mut artist_albums_table = write_txn.open_table(ARTIST_ALBUMS_TABLE)?;
        let mut tracks_table = write_txn.open_table(TRACKS_TABLE)?;
        let mut tracks_by_name_table = write_txn.open_table(TRACKS_BY_NAME_TABLE)?;
        let mut album_tracks_table = write_txn.open_table(ALBUM_TRACKS_TABLE)?;
        let mut embedded_cover_table = write_txn.open_table(TRACK_EMBEDDED_COVER_TABLE)?;
        let mut seek_table = write_txn.open_table(SEEK_TABLE)?;
        let mut tag_errors_table = write_txn.open_table(TAG_ERRORS_TABLE)?;
        let mut tag_error_files_table = write_txn.open_table(TAG_ERROR_FILES_TABLE)?;

        let mut artist_count = running_stats.artists;
        let mut album_count = running_stats.albums;
        let mut track_count = running_stats.tracks;
        let mut artist_sidecar_cache: HashMap<PathBuf, Option<SidecarInfo>> = HashMap::new();

        for (root, album_dir) in album_dirs {
            let files = audio_files_in_dir(&album_dir);
            if files.is_empty() {
                continue;
            }

            let folder_relpath = match relpath_from(&root.path, &album_dir) {
                Some(rel) => prefix_relpath(&root.id, &rel),
                None => continue,
            };
            info!(
                "Scanning album folder '{}' ({} audio files)",
                folder_relpath.as_str(),
                files.len()
            );

            let album_id = stable_id(&folder_relpath);
            if albums_table.get(album_id.as_str())?.is_some() {
                info!(
                    "Skipping already indexed album folder '{}'",
                    folder_relpath.as_str()
                );
                continue;
            }

            let folder_name = album_dir
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "Unknown Album".to_string());
            let (folder_title, folder_year) = split_title_year(&folder_name);

            let fallback_artist = album_dir
                .parent()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "Unknown Artist".to_string());

            let album_sidecar = read_sidecar_info(&album_dir.join("album.json"));
            let artist_sidecar = album_dir.parent().and_then(|parent| {
                load_sidecar_info(&mut artist_sidecar_cache, parent.join("artist.json"))
            });
            if let Some(info) = &album_sidecar {
                info!(
                    "Album sidecar loaded for '{}': summary_present={}, genres={:?}",
                    folder_relpath.as_str(),
                    info.summary
                        .as_ref()
                        .map(|summary| !summary.trim().is_empty())
                        .unwrap_or(false),
                    info.genres
                );
            }
            if let Some(info) = &artist_sidecar {
                info!(
                    "Artist sidecar loaded for '{}': summary_present={}, genres={:?}",
                    fallback_artist.as_str(),
                    info.summary
                        .as_ref()
                        .map(|summary| !summary.trim().is_empty())
                        .unwrap_or(false),
                    info.genres
                );
            }

            let mut album_title: Option<String> = None;
            let mut album_artist: Option<String> = None;
            let mut album_year: Option<i32> = None;
            let mut album_cover: Option<CoverRef> = None;
            let mut album_summary: Option<String> = None;
            let mut album_genres: Vec<String> = Vec::new();
            let mut track_drafts = Vec::new();
            let mut album_tag_error = false;
            let mut tag_error_files: Vec<TagErrorFile> = Vec::new();

            if let Some(info) = &album_sidecar {
                if info.summary.as_ref().is_some() {
                    album_summary = info.summary.clone();
                }
                merge_genres(&mut album_genres, &info.genres);
            }

            for file in files {
                let relpath = match relpath_from(&root.path, &file) {
                    Some(rel) => prefix_relpath(&root.id, &rel),
                    None => continue,
                };
                info!("Reading tags for '{}'", relpath.as_str());

                let tag = match read_tags(&file) {
                    Ok(tag) => tag,
                    Err(err) => {
                        warn!(
                            target: "issue",
                            "file={} | folder={} | error={:?}",
                            relpath.as_str(),
                            folder_relpath.as_str(),
                            err
                        );
                        album_tag_error = true;
                        tag_error_files.push(TagErrorFile {
                            file_relpath: relpath.clone(),
                            folder_relpath: folder_relpath.clone(),
                            error: format!("{:?}", err),
                            last_seen: now_secs(),
                        });
                        TagInfo::default()
                    }
                };

                if album_title.is_none() {
                    album_title = tag.album.clone();
                }
                if album_artist.is_none() {
                    album_artist = tag.album_artist.clone().or_else(|| tag.artist.clone());
                }
                if album_year.is_none() {
                    album_year = tag.year;
                }
                if album_summary.is_none() {
                    album_summary = tag.summary.clone();
                }
                if !tag.genres.is_empty() {
                    merge_genres(&mut album_genres, &tag.genres);
                }

                info!(
                    "Tag fields for '{}': album={:?}, album_artist={:?}, artist={:?}, year={:?}, title={:?}, track_no={:?}, disc_no={:?}, summary_present={}, genres={:?}",
                    relpath.as_str(),
                    tag.album.as_deref(),
                    tag.album_artist.as_deref(),
                    tag.artist.as_deref(),
                    tag.year,
                    tag.title.as_deref(),
                    tag.track_no,
                    tag.disc_no,
                    tag.summary
                        .as_ref()
                        .map(|summary| !summary.trim().is_empty())
                        .unwrap_or(false),
                    &tag.genres
                );

                let title = tag.title.clone().unwrap_or_else(|| file_stem(&file));
                let duration_ms = tag.duration_ms.unwrap_or(0);
                let codec = match audio_codec(&file) {
                    Some(codec) => codec,
                    None => continue,
                };
                let file_size = fs::metadata(&file)?.len();
                let id = stable_id(&relpath);
                let disc_no = tag
                    .disc_no
                    .or_else(|| disc_number_from_path(&file, &album_dir));
                let album_hint = tag
                    .album
                    .clone()
                    .or_else(|| album_title.clone())
                    .unwrap_or_else(|| folder_title.clone());
                let artist_hint = tag
                    .album_artist
                    .clone()
                    .or_else(|| tag.artist.clone())
                    .or_else(|| album_artist.clone())
                    .unwrap_or_else(|| fallback_artist.clone());
                let year_hint = tag.year.or(album_year).or(folder_year);
                info!(
                    "Indexing track '{}' for album '{}' (artist='{}', year={:?})",
                    title.as_str(),
                    album_hint,
                    artist_hint,
                    year_hint
                );
                info!(
                    "Track metadata: relpath='{}', track_no={:?}, disc_no={:?}, duration_ms={}, codec={:?}, sample_rate={:?}, channels={:?}, bitrate={:?}, genres={:?}, embedded_cover={}",
                    relpath.as_str(),
                    tag.track_no,
                    disc_no,
                    duration_ms,
                    &codec,
                    tag.sample_rate,
                    tag.channels,
                    tag.bitrate,
                    &tag.genres,
                    tag.has_embedded_cover
                );

                if tag.has_embedded_cover && album_cover.is_none() {
                    album_cover = Some(CoverRef::Embedded {
                        track_id: id.clone(),
                    });
                    info!(
                        "Album cover set from embedded art in '{}'",
                        relpath.as_str()
                    );
                } else if tag.has_embedded_cover {
                    info!("Found embedded art in '{}'", relpath.as_str());
                }

                embedded_cover_table.insert(id.as_str(), bool_bytes(tag.has_embedded_cover))?;

                let seek = build_seek_index(duration_ms, file_size);
                let seek_bytes = encode_value(&seek)?;
                seek_table.insert(id.as_str(), seek_bytes.as_slice())?;

                track_drafts.push(TrackDraft {
                    id,
                    relpath,
                    title,
                    track_no: tag.track_no,
                    disc_no,
                    duration_ms,
                    codec,
                    sample_rate: tag.sample_rate,
                    channels: tag.channels,
                    bitrate: tag.bitrate,
                    file_size,
                    genres: tag.genres,
                });
            }

            if track_drafts.is_empty() {
                continue;
            }

            if album_year.is_none() {
                album_year = folder_year;
            }

            let album_title = album_title.unwrap_or(folder_title);
            let album_artist = album_artist.unwrap_or(fallback_artist).trim().to_string();
            let artist_id = stable_id(album_artist.trim());
            info!(
                "Detected album '{}' (artist='{}', year={:?}) with {} tracks",
                album_title.as_str(),
                album_artist.as_str(),
                album_year,
                track_drafts.len()
            );

            if album_cover.is_none() {
                if let Some(cover_rel) = find_folder_cover(&root.path, &root.id, &album_dir) {
                    info!(
                        "Found folder cover for album '{}': {}",
                        album_title.as_str(),
                        cover_rel.as_str()
                    );
                    album_cover = Some(CoverRef::File { relpath: cover_rel });
                } else {
                    info!(
                        "No folder cover detected for album '{}'",
                        album_title.as_str()
                    );
                }
            }

            let mut artist_genres = Vec::new();
            merge_genres(&mut artist_genres, &album_genres);
            if let Some(info) = &artist_sidecar {
                merge_genres(&mut artist_genres, &info.genres);
            }
            let mut artist_summary = artist_sidecar
                .as_ref()
                .and_then(|info| info.summary.clone());
            let mut artist_logo = None;
            let mut artist_banner = None;

            if let Some(value) = artists_table.get(artist_id.as_str())? {
                let existing: Artist = decode_value(value.value())?;
                merge_genres(&mut artist_genres, &existing.genres);
                if artist_summary.is_none() {
                    artist_summary = existing.summary;
                }
                if artist_logo.is_none() {
                    artist_logo = existing.logo_ref;
                }
                if artist_banner.is_none() {
                    artist_banner = existing.banner_ref;
                }
            }

            let artist = Artist {
                id: artist_id.clone(),
                name: album_artist.clone(),
                genres: artist_genres,
                summary: artist_summary,
                logo_ref: artist_logo,
                banner_ref: artist_banner,
            };
            let artist_bytes = encode_value(&artist)?;
            let prev = artists_table.insert(artist_id.as_str(), artist_bytes.as_slice())?;
            if prev.is_none() {
                artist_count += 1;
            }

            let artist_name_key = artist_name_key(&artist.name, &artist.id);
            artists_by_name_table.insert(artist_name_key.as_str(), artist.id.as_bytes())?;

            let album = Album {
                id: album_id.clone(),
                artist_id: artist_id.clone(),
                artist_ids: vec![artist_id.clone()],
                artist_names: vec![artist.name.clone()],
                title: album_title,
                year: album_year,
                folder_relpath,
                cover_ref: album_cover,
                genres: album_genres.clone(),
                summary: album_summary,
            };

            let album_bytes = encode_value(&album)?;
            let prev = albums_table.insert(album_id.as_str(), album_bytes.as_slice())?;
            if prev.is_none() {
                album_count += 1;
            }

            let album_name_key = album_name_key(&artist.name, &album);
            albums_by_name_table.insert(album_name_key.as_str(), album.id.as_bytes())?;

            let album_index_key = album_index_key(&artist_id, &album);
            artist_albums_table.insert(album_index_key.as_str(), album_id.as_bytes())?;

            if album_tag_error {
                warn!(
                    target: "issue",
                    "album={} | folder={} | files={}",
                    album.title.as_str(),
                    album.folder_relpath.as_str(),
                    tag_error_files.len()
                );
                let tag_error = TagErrorInfo {
                    album_id: album.id.clone(),
                    artist_id: artist.id.clone(),
                    album_title: album.title.clone(),
                    artist_name: artist.name.clone(),
                    folder_relpath: album.folder_relpath.clone(),
                    last_seen: now_secs(),
                };
                let tag_bytes = encode_value(&tag_error)?;
                tag_errors_table.insert(album.id.as_str(), tag_bytes.as_slice())?;
            }

            for info in tag_error_files {
                let bytes = encode_value(&info)?;
                tag_error_files_table.insert(info.file_relpath.as_str(), bytes.as_slice())?;
            }

            track_drafts.sort_by(|a, b| {
                let disc_a = a.disc_no.unwrap_or(u16::MAX);
                let disc_b = b.disc_no.unwrap_or(u16::MAX);
                let track_a = a.track_no.unwrap_or(u16::MAX);
                let track_b = b.track_no.unwrap_or(u16::MAX);
                disc_a
                    .cmp(&disc_b)
                    .then_with(|| track_a.cmp(&track_b))
                    .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
                    .then_with(|| a.relpath.cmp(&b.relpath))
            });

            for (order, draft) in track_drafts.into_iter().enumerate() {
                let TrackDraft {
                    id,
                    relpath,
                    title,
                    track_no,
                    disc_no,
                    duration_ms,
                    codec,
                    sample_rate,
                    channels,
                    bitrate,
                    file_size,
                    genres,
                } = draft;
                let mut track_genres = genres;
                merge_genres(&mut track_genres, &album_genres);
                let track = Track {
                    id,
                    album_id: album_id.clone(),
                    artist_id: artist_id.clone(),
                    title,
                    track_no,
                    disc_no,
                    duration_ms,
                    codec,
                    sample_rate,
                    channels,
                    bitrate,
                    file_relpath: relpath,
                    file_size,
                    genres: track_genres,
                };

                let track_bytes = encode_value(&track)?;
                let prev = tracks_table.insert(track.id.as_str(), track_bytes.as_slice())?;
                if prev.is_none() {
                    track_count += 1;
                }

                let track_index_key = album_track_key(&album_id, order, &track.id);
                album_tracks_table.insert(track_index_key.as_str(), track.id.as_bytes())?;

                let track_name_key = track_name_key(
                    &artist.name,
                    &album.title,
                    track.disc_no,
                    track.track_no,
                    &track.title,
                    &track.id,
                );
                tracks_by_name_table.insert(track_name_key.as_str(), track.id.as_bytes())?;
            }
        }

        running_stats = LibraryStats {
            artists: artist_count,
            albums: album_count,
            tracks: track_count,
        };

        let version_bytes = encode_value(&INDEX_VERSION)?;
        meta_table.insert(META_VERSION_KEY, version_bytes.as_slice())?;
        let stats_bytes = encode_value(&running_stats)?;
        meta_table.insert(META_STATS_KEY, stats_bytes.as_slice())?;

        running_stats
    };

    write_txn.commit()?;
    Ok(stats)
}

fn clear_table(
    txn: &WriteTransaction,
    table: TableDefinition<&str, &[u8]>,
) -> Result<(), LibraryError> {
    match txn.delete_table(table) {
        Ok(_) => Ok(()),
        Err(TableError::TableDoesNotExist(_)) => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn encode_value<T: Serialize>(value: &T) -> Result<Vec<u8>, LibraryError> {
    Ok(bincode::serialize(value)?)
}

fn decode_value<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, LibraryError> {
    Ok(bincode::deserialize(bytes)?)
}

fn bool_bytes(value: bool) -> &'static [u8] {
    if value {
        &[1u8]
    } else {
        &[0u8]
    }
}

fn merge_genres(target: &mut Vec<String>, incoming: &[String]) {
    if incoming.is_empty() {
        return;
    }
    let mut seen: HashSet<String> = target
        .iter()
        .map(|genre| normalize_genre_key(genre))
        .collect();
    for genre in incoming {
        let trimmed = normalize_genre_label(genre.trim());
        if trimmed.is_empty() {
            continue;
        }
        let key = normalize_genre_key(&trimmed);
        if seen.insert(key) {
            target.push(trimmed);
        }
    }
}

fn normalize_genre_label(value: &str) -> String {
    let lower = value.trim().to_lowercase();
    let normalized = normalize_genre_probe(&lower);
    let exact = match lower.as_str() {
        "musique classique" => "Classical".to_string(),
        "musique de chambre" => "Chamber music".to_string(),
        "musique de film" => "Soundtrack".to_string(),
        "musiques de film" => "Soundtrack".to_string(),
        "musique électronique" => "Electronic".to_string(),
        "musique electronique" => "Electronic".to_string(),
        "musique instrumentale" => "Instrumental".to_string(),
        "musique ambient" => "Ambient".to_string(),
        "musique ambiante" => "Ambient".to_string(),
        "piano solo" => "Solo piano".to_string(),
        "classique" => "Classical".to_string(),
        "musica clasica" => "Classical".to_string(),
        "música clásica" => "Classical".to_string(),
        "musica classica" => "Classical".to_string(),
        "música clásica contemporánea" => "Contemporary classical".to_string(),
        "musica contemporanea" => "Contemporary classical".to_string(),
        "musica de camara" => "Chamber music".to_string(),
        "música de cámara" => "Chamber music".to_string(),
        "musica de pelicula" => "Soundtrack".to_string(),
        "música de película" => "Soundtrack".to_string(),
        "banda sonora" => "Soundtrack".to_string(),
        "colonna sonora" => "Soundtrack".to_string(),
        "musica elettronica" => "Electronic".to_string(),
        "musica elettronica sperimentale" => "Electronic".to_string(),
        "musica strumentale" => "Instrumental".to_string(),
        "musica ambient" => "Ambient".to_string(),
        "música instrumental" => "Instrumental".to_string(),
        "música electrónica" => "Electronic".to_string(),
        "música electronica" => "Electronic".to_string(),
        "klassik" => "Classical".to_string(),
        "klassische musik" => "Classical".to_string(),
        "filmmusik" => "Soundtrack".to_string(),
        "elektronische musik" => "Electronic".to_string(),
        "instrumentalmusik" => "Instrumental".to_string(),
        "kammermusik" => "Chamber music".to_string(),
        "zeitgenössische klassische musik" => "Contemporary classical".to_string(),
        "zeitgenossische klassische musik" => "Contemporary classical".to_string(),
        _ => String::new(),
    };
    if !exact.is_empty() {
        return exact;
    }
    match match_genre_probe(&normalized) {
        Some(mapped) => mapped,
        None => value.trim().to_string(),
    }
}

fn match_genre_probe(value: &str) -> Option<String> {
    let rules: [(&str, &[&str]); 6] = [
        (
            "Classical",
            &[
                "classical",
                "klassik",
                "musica clasica",
                "musique classique",
            ],
        ),
        (
            "Contemporary classical",
            &["contemporary classical", "zeitgenossisch", "contemporanea"],
        ),
        (
            "Chamber music",
            &[
                "chamber music",
                "musique de chambre",
                "musica de camara",
                "kammermusik",
            ],
        ),
        (
            "Soundtrack",
            &[
                "soundtrack",
                "musique de film",
                "banda sonora",
                "colonna sonora",
                "filmmusik",
            ],
        ),
        (
            "Electronic",
            &["electronic", "electronique", "electronica", "elektronisch"],
        ),
        (
            "Instrumental",
            &[
                "instrumental",
                "instrumentale",
                "strumentale",
                "instrumentalmusik",
            ],
        ),
    ];

    for (label, terms) in rules {
        for term in terms {
            if value.contains(term) {
                return Some(label.to_string());
            }
        }
    }
    if value.contains("ambient") {
        return Some("Ambient".to_string());
    }
    if value.contains("solo piano") || value.contains("piano solo") {
        return Some("Solo piano".to_string());
    }
    None
}

fn normalize_genre_probe(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut last_space = false;
    for ch in value.chars() {
        let mapped = match ch {
            'à' | 'á' | 'â' | 'ä' | 'ã' | 'å' => 'a',
            'ç' => 'c',
            'è' | 'é' | 'ê' | 'ë' => 'e',
            'ì' | 'í' | 'î' | 'ï' => 'i',
            'ñ' => 'n',
            'ò' | 'ó' | 'ô' | 'ö' | 'õ' => 'o',
            'ù' | 'ú' | 'û' | 'ü' => 'u',
            'ý' | 'ÿ' => 'y',
            'œ' => {
                out.push('o');
                'e'
            }
            'æ' => {
                out.push('a');
                'e'
            }
            _ => ch,
        };
        if mapped.is_ascii_alphanumeric() {
            out.push(mapped);
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim().to_string()
}

fn normalize_genre_key(value: &str) -> String {
    value.trim().to_lowercase()
}

fn clean_summary(summary: String) -> Option<String> {
    let trimmed = summary.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn artist_name_key(name: &str, artist_id: &str) -> String {
    let mut out = String::new();
    out.push_str(name.trim().to_lowercase().as_str());
    out.push(KEY_SEP);
    out.push_str(artist_id);
    out
}

fn album_search_artist_text(album: &Album) -> String {
    if !album.artist_names.is_empty() {
        album.artist_names.join(" ")
    } else {
        String::new()
    }
}

fn album_name_key(artist_name: &str, album: &Album) -> String {
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

fn album_index_key(artist_id: &str, album: &Album) -> String {
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

fn album_track_key(album_id: &str, order: usize, track_id: &str) -> String {
    let mut out = String::new();
    out.push_str(album_id);
    out.push(KEY_SEP);
    out.push_str(&format!("{:08}", order));
    out.push(KEY_SEP);
    out.push_str(track_id);
    out
}

fn track_name_key(
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

fn prefix_key(prefix: &str) -> String {
    let mut out = String::new();
    out.push_str(prefix);
    out.push(KEY_SEP);
    out
}

fn normalize_album_artists(artists: &[Artist]) -> Vec<Artist> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for artist in artists {
        let id = artist.id.trim();
        let name = artist.name.trim();
        if id.is_empty() || name.is_empty() {
            continue;
        }
        if !seen.insert(id.to_string()) {
            continue;
        }
        out.push(Artist {
            id: id.to_string(),
            name: name.to_string(),
            genres: artist.genres.clone(),
            summary: artist.summary.clone(),
            logo_ref: artist.logo_ref.clone(),
            banner_ref: artist.banner_ref.clone(),
        });
    }
    out
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs()
}

fn split_key_last(value: &str) -> Result<(&str, &str), LibraryError> {
    let idx = value
        .rfind(KEY_SEP)
        .ok_or_else(|| LibraryError::KeyParse(value.to_string()))?;
    let next = idx + KEY_SEP.len_utf8();
    Ok((&value[..idx], &value[next..]))
}

fn prefix_relpath(root_id: &str, relpath: &str) -> String {
    let id = root_id.trim();
    if id.is_empty() {
        relpath.to_string()
    } else {
        format!("{}{}{}", id, ROOT_SEP, relpath)
    }
}

fn split_root_relpath(relpath: &str) -> (Option<&str>, &str) {
    let Some(idx) = relpath.find(ROOT_SEP) else {
        return (None, relpath);
    };
    let (prefix, rest) = relpath.split_at(idx);
    let rest = &rest[ROOT_SEP.len()..];
    if prefix.is_empty() || rest.is_empty() {
        (None, relpath)
    } else {
        (Some(prefix), rest)
    }
}

#[derive(Debug)]
struct TrackDraft {
    id: String,
    relpath: String,
    title: String,
    track_no: Option<u16>,
    disc_no: Option<u16>,
    duration_ms: u32,
    codec: Codec,
    sample_rate: Option<u32>,
    channels: Option<u8>,
    bitrate: Option<u32>,
    file_size: u64,
    genres: Vec<String>,
}

fn collect_album_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs_with_audio = HashSet::new();

    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        if audio_codec(entry.path()).is_some() {
            if let Some(parent) = entry.path().parent() {
                dirs_with_audio.insert(parent.to_path_buf());
            }
        }
    }

    let mut has_descendant_audio = HashSet::new();
    for dir in &dirs_with_audio {
        let mut ancestor = dir.parent();
        while let Some(current) = ancestor {
            if !current.starts_with(root) {
                break;
            }
            has_descendant_audio.insert(current.to_path_buf());
            if current == root {
                break;
            }
            ancestor = current.parent();
        }
    }

    let mut album_dirs: HashSet<PathBuf> = dirs_with_audio
        .iter()
        .filter(|dir| !has_descendant_audio.contains(*dir))
        .cloned()
        .collect();

    let mut parent_to_children: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for dir in &album_dirs {
        if let Some(parent) = dir.parent() {
            if parent == root {
                continue;
            }
            parent_to_children
                .entry(parent.to_path_buf())
                .or_default()
                .push(dir.clone());
        }
    }

    let mut promoted: Vec<(PathBuf, Vec<PathBuf>)> = Vec::new();
    for (parent, children) in parent_to_children {
        if children.is_empty() {
            continue;
        }
        if dirs_with_audio.contains(&parent) {
            continue;
        }
        if !children.iter().all(|child| {
            child
                .file_name()
                .and_then(|name| name.to_str())
                .map(is_disc_folder_name)
                .unwrap_or(false)
        }) {
            continue;
        }
        promoted.push((parent, children));
    }

    for (parent, children) in promoted {
        for child in children {
            album_dirs.remove(&child);
        }
        album_dirs.insert(parent);
    }

    let mut album_dirs: Vec<PathBuf> = album_dirs.into_iter().collect();
    album_dirs.sort();
    album_dirs
}

fn audio_files_in_dir(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in WalkDir::new(dir)
        .follow_links(false)
        .min_depth(1)
        .max_depth(2)
        .into_iter()
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if path.is_file() && audio_codec(path).is_some() {
            files.push(path.to_path_buf());
        }
    }
    files.sort();
    files
}

fn audio_codec(path: &Path) -> Option<Codec> {
    let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
    match ext.as_str() {
        "mp3" => Some(Codec::Mp3),
        "flac" => Some(Codec::Flac),
        _ => None,
    }
}

fn build_seek_index(duration_ms: u32, file_size: u64) -> SeekIndex {
    let mut points = Vec::new();
    points.push(SeekPoint { t_ms: 0, byte: 0 });

    if duration_ms > 0 && file_size > 0 {
        let mut t = SEEK_STEP_MS;
        while t < duration_ms {
            let byte = file_size.saturating_mul(u64::from(t)) / u64::from(duration_ms);
            points.push(SeekPoint { t_ms: t, byte });
            t = t.saturating_add(SEEK_STEP_MS);
        }
        points.push(SeekPoint {
            t_ms: duration_ms,
            byte: file_size.saturating_sub(1),
        });
    }

    SeekIndex {
        duration_ms,
        points,
        hint: "Client should request Range: bytes=byte-".to_string(),
    }
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Unknown Track".to_string())
}

fn split_title_year(input: &str) -> (String, Option<i32>) {
    let trimmed = input.trim();
    if let Some((title, year)) = split_year_suffix(trimmed, '(', ')') {
        return (title.to_string(), Some(year));
    }
    if let Some((title, year)) = split_year_suffix(trimmed, '[', ']') {
        return (title.to_string(), Some(year));
    }
    (trimmed.to_string(), None)
}

fn split_year_suffix(input: &str, open: char, close: char) -> Option<(&str, i32)> {
    let trimmed = input.trim_end();
    if !trimmed.ends_with(close) {
        return None;
    }
    let open_idx = trimmed.rfind(open)?;
    let year_str = trimmed
        .get(open_idx + open.len_utf8()..trimmed.len() - close.len_utf8())?
        .trim();
    if year_str.len() != 4 || !year_str.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let year = year_str.parse::<i32>().ok()?;
    let title = trimmed[..open_idx].trim_end();
    if title.is_empty() {
        return None;
    }
    Some((title, year))
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(default)]
struct SidecarInfo {
    summary: Option<String>,
    genres: Vec<String>,
}

fn read_sidecar_info(path: &Path) -> Option<SidecarInfo> {
    let data = fs::read(path).ok()?;
    let mut info: SidecarInfo = serde_json::from_slice(&data).ok()?;
    if let Some(summary) = &info.summary {
        if summary.trim().is_empty() {
            info.summary = None;
        }
    }
    info.genres.retain(|genre| !genre.trim().is_empty());
    if info.summary.is_none() && info.genres.is_empty() {
        None
    } else {
        Some(info)
    }
}

fn load_sidecar_info(
    cache: &mut HashMap<PathBuf, Option<SidecarInfo>>,
    path: PathBuf,
) -> Option<SidecarInfo> {
    if let Some(cached) = cache.get(&path) {
        return cached.clone();
    }
    let info = read_sidecar_info(&path);
    cache.insert(path, info.clone());
    info
}

fn find_folder_cover(root: &Path, root_id: &str, album_dir: &Path) -> Option<String> {
    const COVERS: &[&str] = &[
        "cover.jpg",
        "cover.jpeg",
        "cover.png",
        "folder.jpg",
        "folder.jpeg",
        "folder.png",
        "front.jpg",
        "front.jpeg",
        "front.png",
        "album.jpg",
        "album.png",
    ];

    let mut candidates = HashSet::new();
    for name in COVERS {
        candidates.insert(*name);
    }

    let entries = fs::read_dir(album_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_ascii_lowercase())?;
        if candidates.contains(name.as_str()) {
            let relpath = relpath_from(root, &path)?;
            return Some(prefix_relpath(root_id, &relpath));
        }
    }
    None
}

fn is_disc_folder_name(name: &str) -> bool {
    parse_disc_number(name).is_some()
}

fn disc_number_from_path(file: &Path, album_dir: &Path) -> Option<u16> {
    if !file.starts_with(album_dir) {
        return None;
    }
    let mut current = file.parent()?;
    loop {
        if current == album_dir {
            break;
        }
        if let Some(name) = current.file_name().and_then(|s| s.to_str()) {
            if let Some(num) = parse_disc_number(name) {
                return Some(num);
            }
        }
        current = current.parent()?;
    }
    None
}

fn parse_disc_number(name: &str) -> Option<u16> {
    let cleaned = normalize_disc_name(name);
    if cleaned.is_empty() {
        return None;
    }

    for prefix in DISC_KEYWORDS {
        if let Some(rest) = cleaned.strip_prefix(prefix) {
            let rest = rest.trim();
            if let Some(num) = parse_number_token(rest) {
                return Some(num);
            }
        }
    }

    let tokens: Vec<&str> = cleaned.split_whitespace().collect();
    if tokens.len() < 2 {
        return None;
    }

    let last = tokens[tokens.len() - 1];
    let num = parse_number_token(last)?;
    if tokens[..tokens.len() - 1]
        .iter()
        .any(|token| is_disc_keyword(token))
    {
        return Some(num);
    }

    None
}

fn normalize_disc_name(name: &str) -> String {
    let mut cleaned = String::with_capacity(name.len());
    for ch in name.chars() {
        let ch = ch.to_ascii_lowercase();
        if ch == '_' || ch == '-' || ch == '.' {
            cleaned.push(' ');
        } else {
            cleaned.push(ch);
        }
    }
    cleaned.trim().to_string()
}

fn parse_number_token(token: &str) -> Option<u16> {
    if token.chars().all(|c| c.is_ascii_digit()) {
        return token.parse::<u16>().ok();
    }
    if token.chars().all(is_roman_char) {
        return roman_to_u16(token);
    }
    None
}

fn is_disc_keyword(token: &str) -> bool {
    matches!(
        token,
        "cd" | "disc"
            | "disk"
            | "dvd"
            | "medium"
            | "media"
            | "format"
            | "vol"
            | "volume"
            | "part"
            | "side"
            | "lp"
    )
}

const DISC_KEYWORDS: &[&str] = &[
    "cd", "disc", "disk", "dvd", "medium", "media", "format", "vol", "volume", "part", "side", "lp",
];

fn is_roman_char(ch: char) -> bool {
    matches!(ch, 'i' | 'v' | 'x' | 'l' | 'c' | 'd' | 'm')
}

fn roman_to_u16(input: &str) -> Option<u16> {
    let mut total = 0u16;
    let mut prev = 0u16;
    for ch in input.chars().rev() {
        let value = match ch {
            'i' => 1,
            'v' => 5,
            'x' => 10,
            'l' => 50,
            'c' => 100,
            'd' => 500,
            'm' => 1000,
            _ => return None,
        };
        if value < prev {
            total = total.saturating_sub(value);
        } else {
            total = total.saturating_add(value);
            prev = value;
        }
    }
    if total == 0 {
        None
    } else {
        Some(total)
    }
}
