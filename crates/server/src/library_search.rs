//! Ranked catalog search shared by the listening API and the web console.
use std::collections::HashSet;

use library::{search::LibrarySearchQuery, Library, LibraryError};

use crate::library_views::album_artist_name;
use crate::state::SearchResult;

pub(crate) fn search_library(
    library: &Library,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchResult>, LibraryError> {
    let mut results: Vec<SearchResult> = Vec::new();
    let matcher = LibrarySearchQuery::new(query);
    let candidates = library.search_candidates(&matcher)?;
    for (artist, score) in candidates.artists {
        if score > 0 {
            results.push(SearchResult {
                kind: "artist".to_string(),
                album_id: None,
                id: artist.id,
                title: artist.name,
                subtitle: None,
                score,
            });
        }
    }

    let albums = candidates.albums;
    let album_artist_ids = albums
        .iter()
        .map(|(album, _)| album.artist_id.clone())
        .collect::<HashSet<_>>();
    let album_artist_names = library.artist_name_map(&album_artist_ids)?;
    for (album, catalog_score) in albums {
        let credits = library.album_credits(&album.id)?;
        let artist_name = if credits.display_name.trim().is_empty() {
            album_artist_name(&album, album_artist_names.get(&album.artist_id).cloned())
        } else {
            credits.display_name
        };
        let combined = format!("{} {}", album.title, artist_name);
        let score = matcher.score(&combined).max(catalog_score);
        if score > 0 {
            results.push(SearchResult {
                kind: "album".to_string(),
                album_id: Some(album.id.clone()),
                id: album.id,
                title: album.title,
                subtitle: Some(artist_name),
                score,
            });
        }
    }

    let tracks = candidates.tracks;
    let track_artist_ids = tracks
        .iter()
        .map(|(track, _)| track.artist_id.clone())
        .collect::<HashSet<_>>();
    let track_album_ids = tracks
        .iter()
        .map(|(track, _)| track.album_id.clone())
        .collect::<HashSet<_>>();
    let track_artist_names = library.artist_name_map(&track_artist_ids)?;
    let track_album_titles = library.album_title_map(&track_album_ids)?;
    for (track, catalog_score) in tracks {
        let credits = library.track_credits(&track.id)?;
        let artist_name = if credits.display_name.trim().is_empty() {
            track_artist_names
                .get(&track.artist_id)
                .cloned()
                .unwrap_or_else(|| "Unknown Artist".to_string())
        } else {
            credits.display_name
        };
        let album_title = track_album_titles
            .get(&track.album_id)
            .cloned()
            .unwrap_or_else(|| "Unknown Album".to_string());
        let combined = format!("{} {} {}", track.title, artist_name, album_title);
        let score = matcher.score(&combined).max(catalog_score);
        if score > 0 {
            results.push(SearchResult {
                kind: "track".to_string(),
                album_id: Some(track.album_id.clone()),
                id: track.id,
                title: track.title,
                subtitle: Some(format!("{} - {}", artist_name, album_title)),
                score,
            });
        }
    }

    results.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));
    results.truncate(limit);

    Ok(results)
}
