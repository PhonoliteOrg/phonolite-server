//! Request-scoped catalogue hydration for existing album and track views.
//! Ordered results retain duplicate inputs; related rows are decoded once from
//! one read snapshot and discarded when the request finishes.
use std::collections::{HashMap, HashSet};

use common::{genre_display_label, Album, ArtistCredit, ArtistCredits, Track};
use redb::ReadableTable;

use crate::credits::CreditReader;
use crate::storage::{prefix_key, ALBUM_TRACKS_TABLE};
use crate::{Library, LibraryError};

#[derive(Debug, Default)]
pub struct AlbumViewMetadata {
    pub track_counts: HashMap<String, usize>,
    pub artist_names: HashMap<String, String>,
    /// One full credit record per input album, in the same order.
    pub credits: Vec<ArtistCredits>,
}

#[derive(Debug, Default)]
pub struct TrackViewMetadata {
    pub artist_names: HashMap<String, String>,
    pub album_titles: HashMap<String, String>,
    /// One display record per input track, including duplicate IDs.
    pub tracks: Vec<TrackDisplayMetadata>,
}

#[derive(Debug, Default)]
pub struct TrackDisplayMetadata {
    pub credits: ArtistCredits,
    pub genres: Vec<String>,
    pub listening_genres: Vec<String>,
}

impl Library {
    pub fn album_view_metadata(&self, albums: &[Album]) -> Result<AlbumViewMetadata, LibraryError> {
        let read = self.db.begin_read()?;
        let album_tracks = read.open_table(ALBUM_TRACKS_TABLE)?;
        let mut metadata = AlbumViewMetadata::default();
        for album in albums {
            self.check_read_cancelled()?;
            if metadata.track_counts.contains_key(&album.id) {
                continue;
            }
            let prefix = prefix_key(&album.id);
            let mut end = prefix.clone();
            end.push('\u{10ffff}');
            let mut count = 0;
            for entry in album_tracks.range(prefix.as_str()..end.as_str())? {
                self.check_read_cancelled()?;
                entry?;
                count += 1;
            }
            metadata.track_counts.insert(album.id.clone(), count);
        }
        let mut reader = CreditReader::new(&read);
        reader.require_artists()?;
        let artist_ids: HashSet<_> = albums
            .iter()
            .flat_map(|album| album.all_artist_ids().iter())
            .collect();
        for id in artist_ids {
            self.check_read_cancelled()?;
            if let Some(artist) = reader.artist(id)? {
                metadata
                    .artist_names
                    .insert(id.clone(), artist.name.clone());
            }
        }
        metadata.credits.reserve(albums.len());
        for album in albums {
            self.check_read_cancelled()?;
            metadata.credits.push(reader.album_credits(&album.id)?);
        }
        Ok(metadata)
    }

    pub fn track_view_metadata(&self, tracks: &[Track]) -> Result<TrackViewMetadata, LibraryError> {
        let read = self.db.begin_read()?;
        let mut reader = CreditReader::new(&read);
        self.track_view_metadata_from_reader(tracks, &mut reader)
    }

    fn track_view_metadata_from_reader(
        &self,
        tracks: &[Track],
        reader: &mut CreditReader<'_, '_>,
    ) -> Result<TrackViewMetadata, LibraryError> {
        let mut metadata = TrackViewMetadata::default();
        // Match the existing name-map / title-map / per-track lookup order.
        // Required tables are still checked for an empty input list.
        reader.require_artists()?;
        let artist_ids: HashSet<_> = tracks.iter().map(|track| &track.artist_id).collect();
        for id in artist_ids {
            self.check_read_cancelled()?;
            if let Some(artist) = reader.artist(id)? {
                metadata
                    .artist_names
                    .insert(id.clone(), artist.name.clone());
            }
        }
        reader.require_albums()?;
        let album_ids: HashSet<_> = tracks.iter().map(|track| &track.album_id).collect();
        for id in album_ids {
            self.check_read_cancelled()?;
            if let Some(album) = reader.album(id)? {
                metadata
                    .album_titles
                    .insert(id.clone(), album.title.clone());
            }
        }
        let mut album_genres = HashMap::new();
        let mut artist_genres = HashMap::new();
        metadata.tracks.reserve(tracks.len());
        for track in tracks {
            self.check_read_cancelled()?;
            let credits = reader.track_credits(&track.id)?;
            let performers: Vec<_> = credits
                .credits
                .iter()
                .filter(|credit| matches!(credit.role.as_str(), "track_artist" | "featured_artist"))
                .collect();
            let genres = track_genres(
                self,
                reader,
                track,
                &performers,
                &mut album_genres,
                &mut artist_genres,
            )?;
            let release_genres = cached_album_genres(reader, &track.album_id, &mut album_genres)?;
            let listening_genres = if release_genres.is_empty() {
                // Within one snapshot the tag/performer fallback just computed
                // above is also the listening fallback.
                genres.clone()
            } else {
                release_genres.clone()
            };
            metadata.tracks.push(TrackDisplayMetadata {
                credits,
                genres,
                listening_genres,
            });
        }
        Ok(metadata)
    }
}

fn track_genres(
    library: &Library,
    reader: &mut CreditReader<'_, '_>,
    track: &Track,
    performers: &[&ArtistCredit],
    album_cache: &mut HashMap<String, Vec<String>>,
    artist_cache: &mut HashMap<String, Vec<String>>,
) -> Result<Vec<String>, LibraryError> {
    let own = genre_labels(&track.genres);
    if !own.is_empty() {
        return Ok(own);
    }
    let genres = cached_album_genres(reader, &track.album_id, album_cache)?;
    if !genres.is_empty() {
        return Ok(genres.clone());
    }
    let performer_ids = if performers.is_empty() {
        vec![track.artist_id.as_str()]
    } else {
        performers
            .iter()
            .map(|credit| credit.artist_id.as_str())
            .collect()
    };
    let mut genres = Vec::new();
    for id in performer_ids {
        library.check_read_cancelled()?;
        if !artist_cache.contains_key(id) {
            let values = reader
                .artist(id)?
                .map(|artist| genre_labels(&artist.genres))
                .unwrap_or_default();
            artist_cache.insert(id.to_owned(), values);
        }
        genres.extend(artist_cache.get(id).into_iter().flatten().cloned());
    }
    Ok(genre_labels(&genres))
}

fn cached_album_genres<'a>(
    reader: &mut CreditReader<'_, '_>,
    album_id: &str,
    cache: &'a mut HashMap<String, Vec<String>>,
) -> Result<&'a Vec<String>, LibraryError> {
    if !cache.contains_key(album_id) {
        let genres = reader
            .album(album_id)?
            .map(|album| genre_labels(&album.genres))
            .unwrap_or_default();
        cache.insert(album_id.to_owned(), genres);
    }
    Ok(&cache[album_id])
}

fn genre_labels(values: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    values
        .iter()
        .map(|genre| genre_display_label(genre))
        .filter(|genre| !genre.is_empty() && seen.insert(genre.to_lowercase()))
        .collect()
}

#[cfg(test)]
mod tests;
