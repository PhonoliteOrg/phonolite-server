//! Read-only catalog capabilities. Cursor and search ordering follow storage keys.
use std::collections::{HashMap, HashSet};

use common::{Album, Artist, SeekIndex, Track};
use redb::{ReadableTable, TableError};

use crate::storage::{
    decode_value, prefix_key, read_stats, split_key_last, ALBUMS_BY_NAME_TABLE, ALBUMS_TABLE,
    ALBUM_TRACKS_TABLE, ARTISTS_BY_NAME_TABLE, ARTISTS_TABLE, ARTIST_ALBUMS_TABLE, SEEK_TABLE,
    TAG_ERRORS_TABLE, TAG_ERROR_FILES_TABLE, TRACKS_BY_NAME_TABLE, TRACKS_TABLE,
    TRACK_EMBEDDED_COVER_TABLE,
};
use crate::{Library, LibraryError, LibraryStats, TagErrorFile, TagErrorInfo};

impl Library {
    pub fn stats(&self) -> Result<LibraryStats, LibraryError> {
        read_stats(&self.db)
    }

    pub fn page_albums(
        &self,
        cursor: &mut Option<String>,
        limit: usize,
    ) -> Result<(Vec<Album>, usize), LibraryError> {
        let read = self.db.begin_read()?;
        let names = read.open_table(ALBUMS_BY_NAME_TABLE)?;
        let values = read.open_table(ALBUMS_TABLE)?;
        let total = names.len()? as usize;
        let mut items = Vec::new();
        let start = cursor.clone().unwrap_or_default();
        for entry in names.range(start.as_str()..)? {
            let (key, _) = entry?;
            if cursor.is_some() && key.value() == start {
                continue;
            }
            if items.len() >= limit {
                break;
            }
            let (_, id) = split_key_last(key.value())?;
            if let Some(value) = values.get(id)? {
                items.push(decode_value(value.value())?);
            }
            *cursor = Some(key.value().to_owned());
        }
        Ok((items, total))
    }

    pub fn page_artists(
        &self,
        cursor: &mut Option<String>,
        limit: usize,
    ) -> Result<(Vec<Artist>, usize), LibraryError> {
        let read = self.db.begin_read()?;
        let names = read.open_table(ARTISTS_BY_NAME_TABLE)?;
        let values = read.open_table(ARTISTS_TABLE)?;
        let total = names.len()? as usize;
        let mut items = Vec::new();
        let start = cursor.clone().unwrap_or_default();
        for entry in names.range(start.as_str()..)? {
            let (key, _) = entry?;
            if cursor.is_some() && key.value() == start {
                continue;
            }
            if items.len() >= limit {
                break;
            }
            let (_, id) = split_key_last(key.value())?;
            if let Some(value) = values.get(id)? {
                items.push(decode_value(value.value())?);
            }
            *cursor = Some(key.value().to_owned());
        }
        Ok((items, total))
    }

    pub fn page_tracks(
        &self,
        cursor: &mut Option<String>,
        limit: usize,
    ) -> Result<(Vec<Track>, usize), LibraryError> {
        let read = self.db.begin_read()?;
        let names = read.open_table(TRACKS_BY_NAME_TABLE)?;
        let values = read.open_table(TRACKS_TABLE)?;
        let total = names.len()? as usize;
        let mut items = Vec::new();
        let start = cursor.clone().unwrap_or_default();
        for entry in names.range(start.as_str()..)? {
            let (key, _) = entry?;
            if cursor.is_some() && key.value() == start {
                continue;
            }
            if items.len() >= limit {
                break;
            }
            let (_, id) = split_key_last(key.value())?;
            if let Some(value) = values.get(id)? {
                items.push(decode_value(value.value())?);
            }
            *cursor = Some(key.value().to_owned());
        }
        Ok((items, total))
    }

    pub fn list_artists(
        &self,
        search: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<Artist>, usize), LibraryError> {
        self.list_artists_matching(search, limit, offset, None)
    }

    pub(super) fn list_artists_matching(
        &self,
        search: Option<&str>,
        limit: usize,
        offset: usize,
        eligible: Option<&HashSet<String>>,
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
            if eligible.is_some_and(|ids| !ids.contains(artist_id)) {
                continue;
            }
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

    pub fn artist_name_map(
        &self,
        artist_ids: &HashSet<String>,
    ) -> Result<HashMap<String, String>, LibraryError> {
        let read_txn = self.db.begin_read()?;
        let artist_table = read_txn.open_table(ARTISTS_TABLE)?;
        let mut names = HashMap::with_capacity(artist_ids.len());
        for artist_id in artist_ids {
            if let Some(value) = artist_table.get(artist_id.as_str())? {
                let artist: Artist = decode_value(value.value())?;
                names.insert(artist_id.clone(), artist.name);
            }
        }
        Ok(names)
    }

    pub fn artist_album_counts(
        &self,
        artist_ids: &[String],
    ) -> Result<HashMap<String, usize>, LibraryError> {
        let read_txn = self.db.begin_read()?;
        let artist_album_table = read_txn.open_table(ARTIST_ALBUMS_TABLE)?;
        let mut counts = HashMap::with_capacity(artist_ids.len());

        for artist_id in artist_ids {
            let prefix = prefix_key(artist_id);
            let mut end = prefix.clone();
            end.push('\u{10ffff}');
            let mut count = 0usize;
            for entry in artist_album_table.range(prefix.as_str()..end.as_str())? {
                entry?;
                count += 1;
            }
            counts.insert(artist_id.clone(), count);
        }

        Ok(counts)
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

    pub fn album_track_counts(
        &self,
        album_ids: &[String],
    ) -> Result<HashMap<String, usize>, LibraryError> {
        let read_txn = self.db.begin_read()?;
        let album_track_table = read_txn.open_table(ALBUM_TRACKS_TABLE)?;
        let mut counts = HashMap::with_capacity(album_ids.len());

        for album_id in album_ids {
            let prefix = prefix_key(album_id);
            let mut end = prefix.clone();
            end.push('\u{10ffff}');
            let mut count = 0usize;
            for entry in album_track_table.range(prefix.as_str()..end.as_str())? {
                entry?;
                count += 1;
            }
            counts.insert(album_id.clone(), count);
        }

        Ok(counts)
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

        if search.is_none() {
            let total = name_table.len()? as usize;
            let mut items = Vec::new();
            for entry in name_table.iter()?.skip(offset).take(limit) {
                let (key, _) = entry?;
                let (_, id) = split_key_last(key.value())?;
                if let Some(value) = album_table.get(id)? {
                    items.push(decode_value::<Album>(value.value())?);
                }
            }
            return Ok((items, total));
        }

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

        if search.is_none() {
            let total = name_table.len()? as usize;
            let mut items = Vec::new();
            for entry in name_table.iter()?.skip(offset).take(limit) {
                let (key, _) = entry?;
                let (_, id) = split_key_last(key.value())?;
                if let Some(value) = track_table.get(id)? {
                    items.push(decode_value::<Track>(value.value())?);
                }
            }
            return Ok((items, total));
        }

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

    pub fn album_title_map(
        &self,
        album_ids: &HashSet<String>,
    ) -> Result<HashMap<String, String>, LibraryError> {
        let read_txn = self.db.begin_read()?;
        let album_table = read_txn.open_table(ALBUMS_TABLE)?;
        let mut titles = HashMap::with_capacity(album_ids.len());
        for album_id in album_ids {
            if let Some(value) = album_table.get(album_id.as_str())? {
                let album: Album = decode_value(value.value())?;
                titles.insert(album_id.clone(), album.title);
            }
        }
        Ok(titles)
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
}
