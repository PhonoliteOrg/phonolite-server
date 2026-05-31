use std::collections::{HashMap, HashSet};

use common::{Album, Artist};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArtistView {
    pub id: String,
    pub name: String,
    pub genres: Vec<String>,
    pub album_count: usize,
    pub summary: Option<String>,
    pub logo_ref: Option<String>,
    pub banner_ref: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AlbumView {
    pub id: String,
    pub artist_id: String,
    pub artist_ids: Vec<String>,
    pub artist_name: String,
    pub artist_names: Vec<String>,
    pub title: String,
    pub year: Option<i32>,
    pub genres: Vec<String>,
    pub track_count: usize,
    pub summary: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrackView {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub artist_id: String,
    pub album_id: String,
    pub duration_ms: u32,
    pub track_no: Option<u16>,
    pub disc_no: Option<u16>,
    pub genres: Vec<String>,
    pub liked: bool,
    pub in_playlists: bool,
}

pub fn build_artist_view(
    library: &::library::Library,
    artist: Artist,
) -> Result<ArtistView, String> {
    build_artist_views(library, vec![artist])?
        .pop()
        .ok_or_else(|| "artist not found".to_string())
}

pub fn build_artist_views(
    library: &::library::Library,
    artists: Vec<Artist>,
) -> Result<Vec<ArtistView>, String> {
    let artist_ids = artists
        .iter()
        .map(|artist| artist.id.clone())
        .collect::<Vec<_>>();
    let album_counts = library
        .artist_album_counts(&artist_ids)
        .map_err(|err| err.to_string())?;

    Ok(artists
        .into_iter()
        .map(|artist| {
            let album_count = album_counts.get(&artist.id).copied().unwrap_or(0);
            ArtistView {
                id: artist.id,
                name: artist.name,
                genres: artist.genres,
                album_count,
                summary: artist.summary,
                logo_ref: artist.logo_ref,
                banner_ref: artist.banner_ref,
            }
        })
        .collect())
}

pub fn build_album_view(library: &::library::Library, album: Album) -> Result<AlbumView, String> {
    build_album_views(library, vec![album])?
        .pop()
        .ok_or_else(|| "album not found".to_string())
}

pub fn build_album_views(
    library: &::library::Library,
    albums: Vec<Album>,
) -> Result<Vec<AlbumView>, String> {
    let album_ids = albums
        .iter()
        .map(|album| album.id.clone())
        .collect::<Vec<_>>();
    let track_counts = library
        .album_track_counts(&album_ids)
        .map_err(|err| err.to_string())?;
    let artist_ids = albums
        .iter()
        .flat_map(|album| album.all_artist_ids().iter().cloned())
        .collect::<HashSet<_>>();
    let artist_names = library
        .artist_name_map(&artist_ids)
        .map_err(|err| err.to_string())?;

    Ok(albums
        .into_iter()
        .map(|album| album_view(album, &track_counts, &artist_names))
        .collect())
}

pub fn build_track_views(
    library: &::library::Library,
    tracks: &[common::Track],
    liked_set: &HashSet<String>,
    playlist_set: &HashSet<String>,
) -> Result<Vec<TrackView>, String> {
    let artist_ids = tracks
        .iter()
        .map(|track| track.artist_id.clone())
        .collect::<HashSet<_>>();
    let album_ids = tracks
        .iter()
        .map(|track| track.album_id.clone())
        .collect::<HashSet<_>>();
    let artist_names = library
        .artist_name_map(&artist_ids)
        .map_err(|err| err.to_string())?;
    let album_titles = library
        .album_title_map(&album_ids)
        .map_err(|err| err.to_string())?;

    Ok(tracks
        .iter()
        .map(|track| TrackView {
            id: track.id.clone(),
            title: track.title.clone(),
            artist: artist_names
                .get(&track.artist_id)
                .cloned()
                .unwrap_or_else(|| "Unknown Artist".to_string()),
            album: album_titles
                .get(&track.album_id)
                .cloned()
                .unwrap_or_else(|| "Unknown Album".to_string()),
            artist_id: track.artist_id.clone(),
            album_id: track.album_id.clone(),
            duration_ms: track.duration_ms,
            track_no: track.track_no,
            disc_no: track.disc_no,
            genres: track.genres.clone(),
            liked: liked_set.contains(&track.id),
            in_playlists: playlist_set.contains(&track.id),
        })
        .collect())
}

pub fn album_artist_name(album: &Album, fallback: Option<String>) -> String {
    let display = album.artist_display_name();
    if !display.trim().is_empty() {
        display
    } else {
        fallback.unwrap_or_else(|| "Unknown Artist".to_string())
    }
}

fn album_view(
    album: Album,
    track_counts: &HashMap<String, usize>,
    artist_names: &HashMap<String, String>,
) -> AlbumView {
    let artist_name = album_artist_name(&album, artist_names.get(&album.artist_id).cloned());
    let track_count = track_counts.get(&album.id).copied().unwrap_or(0);
    AlbumView {
        id: album.id,
        artist_id: album.artist_id.clone(),
        artist_ids: album.artist_ids.clone(),
        artist_name,
        artist_names: album.artist_names.clone(),
        title: album.title,
        year: album.year,
        genres: album.genres,
        track_count,
        summary: album.summary,
    }
}
