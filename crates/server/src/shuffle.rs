use std::collections::{HashMap, HashSet};

use common::Track;
use library::{Library, LibraryError};
use rand::seq::SliceRandom;

#[derive(Clone, Copy, Debug)]
pub enum ShuffleMode {
    All,
    Artist,
    Album,
    Custom,
    Liked,
}

impl ShuffleMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "all" | "any" | "random" => Some(Self::All),
            "artist" => Some(Self::Artist),
            "album" => Some(Self::Album),
            "custom" => Some(Self::Custom),
            "liked" | "likes" => Some(Self::Liked),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum ShuffleError {
    MissingArtistId,
    MissingAlbumId,
    Library(LibraryError),
}

impl From<LibraryError> for ShuffleError {
    fn from(err: LibraryError) -> Self {
        ShuffleError::Library(err)
    }
}

pub fn build_shuffle_queue(
    library: &Library,
    mode: ShuffleMode,
    artist_id: Option<&str>,
    album_id: Option<&str>,
    custom_artist_ids: &[String],
    custom_genres: &[String],
    liked_set: &HashSet<String>,
) -> Result<Vec<Track>, ShuffleError> {
    let mut tracks = match mode {
        ShuffleMode::All => {
            let (tracks, _) = library.list_tracks(None, usize::MAX, 0)?;
            tracks
        }
        ShuffleMode::Artist => {
            let artist_id = artist_id.ok_or(ShuffleError::MissingArtistId)?;
            let albums = library.list_artist_albums(artist_id)?;
            let mut tracks = Vec::new();
            for album in albums {
                let mut album_tracks = library.get_album_tracks(&album.id)?;
                tracks.append(&mut album_tracks);
            }
            tracks
        }
        ShuffleMode::Album => {
            let album_id = album_id.ok_or(ShuffleError::MissingAlbumId)?;
            library.get_album_tracks(album_id)?
        }
        ShuffleMode::Custom => {
            let (tracks, _) = library.list_tracks(None, usize::MAX, 0)?;
            let artist_filter: HashSet<&str> =
                custom_artist_ids.iter().map(|id| id.as_str()).collect();
            let genre_filter: HashSet<String> = custom_genres
                .iter()
                .map(|genre| genre.trim().to_ascii_lowercase())
                .filter(|genre| !genre.is_empty())
                .collect();

            let filter_artists = !artist_filter.is_empty();
            let filter_genres = !genre_filter.is_empty();

            if !filter_artists && !filter_genres {
                tracks
            } else {
                let mut album_genres_cache: HashMap<String, Vec<String>> = HashMap::new();
                let mut artist_genres_cache: HashMap<String, Vec<String>> = HashMap::new();
                let mut filtered = Vec::new();

                for track in tracks {
                    let matches_artist =
                        filter_artists && artist_filter.contains(track.artist_id.as_str());
                    let matches_genre = if filter_genres {
                        if matches_genres_raw(&track.genres, &genre_filter) {
                            true
                        } else {
                            let album_genres = cached_album_genres(
                                library,
                                &track.album_id,
                                &mut album_genres_cache,
                            )?;
                            if matches_genres_normalized(&album_genres, &genre_filter) {
                                true
                            } else {
                                let artist_genres = cached_artist_genres(
                                    library,
                                    &track.artist_id,
                                    &mut artist_genres_cache,
                                )?;
                                matches_genres_normalized(&artist_genres, &genre_filter)
                            }
                        }
                    } else {
                        false
                    };

                    let include = match (filter_artists, filter_genres) {
                        (true, true) => matches_artist || matches_genre,
                        (true, false) => matches_artist,
                        (false, true) => matches_genre,
                        (false, false) => true,
                    };

                    if include {
                        filtered.push(track);
                    }
                }

                filtered
            }
        }
        ShuffleMode::Liked => {
            let (tracks, _) = library.list_tracks(None, usize::MAX, 0)?;
            tracks
                .into_iter()
                .filter(|track| liked_set.contains(&track.id))
                .collect()
        }
    };

    let mut rng = rand::rng();
    tracks.shuffle(&mut rng);
    Ok(tracks)
}

fn normalize_genres(genres: &[String]) -> Vec<String> {
    genres
        .iter()
        .map(|genre| genre.trim().to_ascii_lowercase())
        .filter(|genre| !genre.is_empty())
        .collect()
}

fn matches_genres_raw(genres: &[String], filter: &HashSet<String>) -> bool {
    genres.iter().any(|genre| {
        let normalized = genre.trim().to_ascii_lowercase();
        !normalized.is_empty() && filter.contains(&normalized)
    })
}

fn matches_genres_normalized(genres: &[String], filter: &HashSet<String>) -> bool {
    genres.iter().any(|genre| filter.contains(genre))
}

fn cached_album_genres(
    library: &Library,
    album_id: &str,
    cache: &mut HashMap<String, Vec<String>>,
) -> Result<Vec<String>, ShuffleError> {
    if let Some(genres) = cache.get(album_id) {
        return Ok(genres.clone());
    }
    let genres = match library.get_album(album_id)? {
        Some(album) => normalize_genres(&album.genres),
        None => Vec::new(),
    };
    cache.insert(album_id.to_string(), genres.clone());
    Ok(genres)
}

fn cached_artist_genres(
    library: &Library,
    artist_id: &str,
    cache: &mut HashMap<String, Vec<String>>,
) -> Result<Vec<String>, ShuffleError> {
    if let Some(genres) = cache.get(artist_id) {
        return Ok(genres.clone());
    }
    let genres = match library.get_artist(artist_id)? {
        Some(artist) => normalize_genres(&artist.genres),
        None => Vec::new(),
    };
    cache.insert(artist_id.to_string(), genres.clone());
    Ok(genres)
}
