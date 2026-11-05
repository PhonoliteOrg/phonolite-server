//! Typo-tolerant search over catalog names. Browse pagination keeps its existing
//! literal filtering; ranked search must consider matches before applying limits.
mod matcher;
pub use matcher::{normalize_library_search, LibrarySearchQuery};

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use common::{Album, Artist, Track};
use parking_lot::Mutex;
use redb::ReadableTable;

use crate::storage::{
    decode_value, prefix_key, split_key_last, ALBUMS_BY_NAME_TABLE, ALBUMS_TABLE,
    ARTISTS_BY_NAME_TABLE, ARTISTS_TABLE, ARTIST_ALBUMS_TABLE, KEY_SEP, TRACKS_BY_NAME_TABLE,
    TRACKS_TABLE,
};
use crate::{Library, LibraryError};

const MAX_BILLED_CACHE_IDS: usize = 65_536;
const MAX_BILLED_CACHE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Default)]
pub(super) struct SearchState {
    billed: Mutex<Option<BilledArtists>>,
}

struct BilledArtists {
    revision: (u64, u64),
    ids: Arc<HashSet<String>>,
}

fn billed_cache_fits(ids: &HashSet<String>) -> bool {
    // Include generous bucket/control overhead as well as retained string
    // capacities. Oversized catalogues still search normally without caching.
    ids.len() <= MAX_BILLED_CACHE_IDS
        && ids
            .iter()
            .fold(ids.capacity().saturating_mul(64), |bytes, id| {
                bytes.saturating_add(id.capacity())
            })
            <= MAX_BILLED_CACHE_BYTES
}

#[derive(Default)]
pub struct SearchCandidates {
    pub artists: Vec<(Artist, u32)>,
    pub albums: Vec<(Album, u32)>,
    pub tracks: Vec<(Track, u32)>,
}

/// Display metadata for a ranked result set, hydrated in one read transaction.
/// Credit graphs are discarded after resolving their display wording.
#[derive(Default)]
pub struct SearchMetadata {
    pub artist_names: HashMap<String, String>,
    pub album_titles: HashMap<String, String>,
    /// Display strings follow the corresponding candidate order, including duplicates.
    pub album_artist_displays: Vec<String>,
    pub track_artist_displays: Vec<String>,
}

impl Library {
    fn search_revision(&self) -> (u64, u64) {
        (self.catalog_generation(), self.metadata_revision())
    }

    fn search_billed_artist_ids(
        &self,
        read: &redb::ReadTransaction,
        revision: (u64, u64),
    ) -> Result<Arc<HashSet<String>>, LibraryError> {
        if self.search_revision() != revision {
            return self.billed_artist_ids_from_read(read).map(Arc::new);
        }
        // Clones share this single bounded entry and coalesce cold builds.
        let mut cached = self.search_state.billed.lock();
        if self.search_revision() == revision {
            if let Some(entry) = cached.as_ref().filter(|entry| entry.revision == revision) {
                return Ok(Arc::clone(&entry.ids));
            }
        }
        let ids = Arc::new(self.billed_artist_ids_from_read(read)?);
        // A writer may have committed while this snapshot was being read.
        // Never label an older snapshot with the writer's newer revision.
        if self.search_revision() == revision {
            *cached = billed_cache_fits(&ids).then(|| BilledArtists {
                revision,
                ids: Arc::clone(&ids),
            });
        }
        Ok(ids)
    }

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
        // Capture freshness before opening the snapshot, not after: an old
        // reader must never populate the cache for a newer committed revision.
        let revision = self.search_revision();
        let read = self.db.begin_read()?;
        let billed = self.search_billed_artist_ids(&read, revision)?;
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

    pub fn search_metadata(
        &self,
        candidates: &SearchCandidates,
    ) -> Result<SearchMetadata, LibraryError> {
        let read = self.db.begin_read()?;
        let mut metadata = SearchMetadata::default();
        let artist_ids: HashSet<_> = candidates
            .albums
            .iter()
            .map(|(album, _)| &album.artist_id)
            .chain(candidates.tracks.iter().map(|(track, _)| &track.artist_id))
            .collect();
        let artists = read.open_table(ARTISTS_TABLE)?;
        for id in artist_ids {
            self.check_read_cancelled()?;
            if let Some(value) = artists.get(id.as_str())? {
                let artist: Artist = decode_value(value.value())?;
                metadata.artist_names.insert(id.clone(), artist.name);
            }
        }
        let album_ids: HashSet<_> = candidates
            .tracks
            .iter()
            .map(|(track, _)| &track.album_id)
            .collect();
        let albums = read.open_table(ALBUMS_TABLE)?;
        for id in album_ids {
            self.check_read_cancelled()?;
            if let Some(value) = albums.get(id.as_str())? {
                let album: Album = decode_value(value.value())?;
                metadata.album_titles.insert(id.clone(), album.title);
            }
        }
        for (album, _) in &candidates.albums {
            self.check_read_cancelled()?;
            metadata
                .album_artist_displays
                .push(self.album_credits_from_read(&read, &album.id)?.display_name);
        }
        for (track, _) in &candidates.tracks {
            self.check_read_cancelled()?;
            metadata
                .track_artist_displays
                .push(self.track_credits_from_read(&read, &track.id)?.display_name);
        }
        Ok(metadata)
    }
}
