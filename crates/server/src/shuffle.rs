use std::collections::HashSet;

use common::Track;
use library::{Library, LibraryError};
use rand::seq::SliceRandom;

#[derive(Clone, Copy, Debug)]
pub enum ShuffleMode {
    All,
    Artist,
    Album,
    Custom,
}

impl ShuffleMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "all" | "any" | "random" => Some(Self::All),
            "artist" => Some(Self::Artist),
            "album" => Some(Self::Album),
            "custom" => Some(Self::Custom),
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
                tracks
                    .into_iter()
                    .filter(|track| {
                        let matches_artist = filter_artists
                            && artist_filter.contains(track.artist_id.as_str());
                        let matches_genre = filter_genres
                            && track.genres.iter().any(|genre| {
                                genre_filter.contains(&genre.trim().to_ascii_lowercase())
                            });

                        match (filter_artists, filter_genres) {
                            (true, true) => matches_artist || matches_genre,
                            (true, false) => matches_artist,
                            (false, true) => matches_genre,
                            (false, false) => true,
                        }
                    })
                    .collect()
            }
        }
    };

    let mut rng = rand::rng();
    tracks.shuffle(&mut rng);
    Ok(tracks)
}
