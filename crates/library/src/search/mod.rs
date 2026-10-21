//! Typo-tolerant search over catalog names. Browse pagination keeps its existing
//! literal filtering; ranked search must consider matches before applying limits.
mod matcher;
pub use matcher::{normalize_library_search, LibrarySearchQuery};

#[cfg(test)]
mod tests;

use common::{Album, Artist, Track};
use redb::ReadableTable;

use crate::storage::{
    decode_value, prefix_key, split_key_last, ALBUMS_BY_NAME_TABLE, ALBUMS_TABLE,
    ARTISTS_BY_NAME_TABLE, ARTISTS_TABLE, ARTIST_ALBUMS_TABLE, KEY_SEP, TRACKS_BY_NAME_TABLE,
    TRACKS_TABLE,
};
use crate::{Library, LibraryError};

#[derive(Default)]
pub struct SearchCandidates {
    pub artists: Vec<(Artist, u32)>,
    pub albums: Vec<(Album, u32)>,
    pub tracks: Vec<(Track, u32)>,
}

impl Library {
    /// Scan the existing name indexes, decoding only matching catalog records.
    /// Scores include performing credits already present in those indexes.
    /// No candidate cap: a late exact match must outrank earlier fuzzy matches.
    pub fn search_candidates(
        &self,
        query: &LibrarySearchQuery,
    ) -> Result<SearchCandidates, LibraryError> {
        let mut candidates = SearchCandidates::default();
        if query.is_empty() {
            return Ok(candidates);
        }
        let billed = self.billed_artist_ids()?;
        let read = self.db.begin_read()?;
        let artists = read.open_table(ARTISTS_TABLE)?;
        let artist_albums = read.open_table(ARTIST_ALBUMS_TABLE)?;
        for entry in read.open_table(ARTISTS_BY_NAME_TABLE)?.iter()? {
            let (key, _) = entry?;
            let (name, id) = split_key_last(key.value())?;
            if !billed.contains(id) {
                continue;
            }
            let score = query.score(name);
            if score == 0 {
                continue;
            }
            let prefix = prefix_key(id);
            let end = format!("{prefix}\u{10ffff}");
            if artist_albums
                .range(prefix.as_str()..end.as_str())?
                .next()
                .transpose()?
                .is_none()
            {
                continue;
            }
            if let Some(value) = artists.get(id)? {
                candidates
                    .artists
                    .push((decode_value(value.value())?, score));
            }
        }

        let albums = read.open_table(ALBUMS_TABLE)?;
        for entry in read.open_table(ALBUMS_BY_NAME_TABLE)?.iter()? {
            let (key, _) = entry?;
            let fields: Vec<_> = key.value().split(KEY_SEP).collect();
            let [artist, _year, title, id] = fields.as_slice() else {
                return Err(LibraryError::KeyParse(key.value().to_owned()));
            };
            let score = query.score(&format!("{title} {artist}"));
            if score == 0 {
                continue;
            }
            if let Some(value) = albums.get(*id)? {
                candidates
                    .albums
                    .push((decode_value(value.value())?, score));
            }
        }

        let tracks = read.open_table(TRACKS_TABLE)?;
        for entry in read.open_table(TRACKS_BY_NAME_TABLE)?.iter()? {
            let (key, _) = entry?;
            let fields: Vec<_> = key.value().split(KEY_SEP).collect();
            let [artist, album, _disc, _number, title, id] = fields.as_slice() else {
                return Err(LibraryError::KeyParse(key.value().to_owned()));
            };
            // IDs, years and track/disc ordering numbers are not search terms.
            let score = query.score(&format!("{title} {artist} {album}"));
            if score == 0 {
                continue;
            }
            if let Some(value) = tracks.get(*id)? {
                candidates
                    .tracks
                    .push((decode_value(value.value())?, score));
            }
        }
        Ok(candidates)
    }
}
