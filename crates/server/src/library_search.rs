//! Ranked catalog search shared by the listening API and the web console.
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
    let metadata = library.search_metadata(&candidates)?;
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
    for ((album, catalog_score), display) in albums.into_iter().zip(metadata.album_artist_displays)
    {
        let artist_name = if display.trim().is_empty() {
            album_artist_name(&album, metadata.artist_names.get(&album.artist_id).cloned())
        } else {
            display
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
    for ((track, catalog_score), display) in tracks.into_iter().zip(metadata.track_artist_displays)
    {
        let artist_name = if display.trim().is_empty() {
            metadata
                .artist_names
                .get(&track.artist_id)
                .cloned()
                .unwrap_or_else(|| "Unknown Artist".to_string())
        } else {
            display
        };
        let album_title = metadata
            .album_titles
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

#[cfg(test)]
mod tests {
    use super::*;

    // Keep the previous independent-read hydration as an output oracle. The
    // optimized path must retain scoring, presentation, stable ties and limits.
    fn reference_search(library: &Library, query: &str, limit: usize) -> Vec<SearchResult> {
        let matcher = LibrarySearchQuery::new(query);
        let candidates = library.search_candidates(&matcher).unwrap();
        let mut results = Vec::new();
        for (artist, score) in candidates.artists {
            if score > 0 {
                results.push(SearchResult {
                    kind: "artist".into(),
                    id: artist.id,
                    title: artist.name,
                    subtitle: None,
                    score,
                    album_id: None,
                });
            }
        }
        for (album, catalog_score) in candidates.albums {
            let credits = library.album_credits(&album.id).unwrap();
            let artist = library
                .get_artist(&album.artist_id)
                .unwrap()
                .map(|artist| artist.name);
            let name = if credits.display_name.trim().is_empty() {
                album_artist_name(&album, artist)
            } else {
                credits.display_name
            };
            let score = matcher
                .score(&format!("{} {name}", album.title))
                .max(catalog_score);
            if score > 0 {
                results.push(SearchResult {
                    kind: "album".into(),
                    album_id: Some(album.id.clone()),
                    id: album.id,
                    title: album.title,
                    subtitle: Some(name),
                    score,
                });
            }
        }
        for (track, catalog_score) in candidates.tracks {
            let credits = library.track_credits(&track.id).unwrap();
            let name = if credits.display_name.trim().is_empty() {
                library
                    .get_artist(&track.artist_id)
                    .unwrap()
                    .map(|artist| artist.name)
                    .unwrap_or_else(|| "Unknown Artist".into())
            } else {
                credits.display_name
            };
            let album = library
                .get_album(&track.album_id)
                .unwrap()
                .map(|album| album.title)
                .unwrap_or_else(|| "Unknown Album".into());
            let score = matcher
                .score(&format!("{} {name} {album}", track.title))
                .max(catalog_score);
            if score > 0 {
                results.push(SearchResult {
                    kind: "track".into(),
                    album_id: Some(track.album_id),
                    id: track.id,
                    title: track.title,
                    subtitle: Some(format!("{name} - {album}")),
                    score,
                });
            }
        }
        results.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));
        results.truncate(limit);
        results
    }

    #[test]
    fn batched_search_preserves_full_ranked_results_and_display_credits() {
        use std::sync::{atomic::AtomicBool, Arc};
        let root =
            std::env::temp_dir().join(format!("phonolite-search-parity-{}", uuid::Uuid::new_v4()));
        let music = root.join("music");
        for index in 0..65 {
            let directory = music.join(format!("{index:03}"));
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join("track.mp3"), b"tag reader fixture").unwrap();
        }
        let reader: Arc<library::TagReader> = Arc::new(|path, _| {
            let index: usize = path
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .parse()
                .unwrap();
            let mut tags = metadata::TagInfo {
                artist: Some(format!("Artist {index:03}")),
                album_artist: Some(format!("Artist {index:03}")),
                album: Some("Record".into()),
                title: Some(format!("Moonlight {index}")),
                ..Default::default()
            };
            match index {
                60 => {
                    tags.artist = Some("Z Artist".into());
                    tags.album_artist = tags.artist.clone();
                    tags.title = Some("Light".into());
                }
                61 => {
                    tags.artist = Some("Alice feat. Björk".into());
                    tags.artists = vec!["Alice".into(), "Björk".into()];
                    tags.artists_structured = true;
                    tags.album_artist = Some("Alice & Björk".into());
                    tags.album_artists = tags.artists.clone();
                    tags.album_artists_structured = true;
                    tags.title = Some("Don’t Stop".into());
                }
                62..=64 => {
                    tags.artist = Some(["α", "\u{e000}", "𐐀"][index - 62].into());
                    tags.album_artist = tags.artist.clone();
                    tags.title = Some("Same".into());
                }
                _ => {}
            }
            Ok(tags)
        });
        let database = Library::open_db(&root.join("library.redb")).unwrap();
        let library = Library::load_or_scan_with_db_and_progress_cancellable_with_reader(
            vec![library::LibraryRoot::new(String::new(), music)],
            database,
            &|_| {},
            Arc::new(AtomicBool::new(false)),
            reader,
        )
        .unwrap()
        .0;
        for query in [
            "light",
            "record",
            "same",
            "alice",
            "björk",
            "alic bjork",
            "dontstop",
            "dont stpo",
            "!!!",
            "absent",
            "",
        ] {
            for limit in [0, 1, 2, 50, usize::MAX] {
                let actual = search_library(&library, query, limit).unwrap();
                let expected = reference_search(&library, query, limit);
                assert_eq!(
                    serde_json::to_value(&actual).unwrap(),
                    serde_json::to_value(&expected).unwrap(),
                    "{query}: {limit}"
                );
            }
        }
        assert_eq!(
            search_library(&library, "light", 1).unwrap()[0].title,
            "Light"
        );
        drop(library);
        std::fs::remove_dir_all(root).unwrap();
    }
}
