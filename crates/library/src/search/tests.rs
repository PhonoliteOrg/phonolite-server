use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::*;
use crate::LibraryRoot;

#[test]
fn catalog_search_handles_typos_spacing_punctuation_and_mixed_fields() {
    let workspace = TestCatalog::new();
    workspace.audio("Guns N’ Roses/First Light/Don’t Stop.mp3");
    workspace.audio("Radiohead/OK Computer/Paranoid Android.mp3");
    let library = workspace.library();
    for query in [
        "dontstop",
        "dont stop",
        "dontstp",
        "dont stpo",
        "gunsnroses",
        "frstlght",
    ] {
        let candidates = library
            .search_candidates(&LibrarySearchQuery::new(query))
            .unwrap();
        assert!(
            candidates
                .tracks
                .iter()
                .any(|(track, _)| track.title == "Don’t Stop"),
            "{query}"
        );
    }
    for query in [
        "radio head",
        "radiohed",
        "raidohead",
        "raidohed",
        "rad iohed",
    ] {
        let candidates = library
            .search_candidates(&LibrarySearchQuery::new(query))
            .unwrap();
        assert!(
            candidates
                .artists
                .iter()
                .any(|(artist, _)| artist.name == "Radiohead"),
            "{query}"
        );
    }
    let candidates = library
        .search_candidates(&LibrarySearchQuery::new("pranoid raidohed"))
        .unwrap();
    assert_eq!(candidates.tracks.len(), 1);
    assert_eq!(candidates.tracks[0].0.title, "Paranoid Android");
    let candidates = library
        .search_candidates(&LibrarySearchQuery::new("frst lght"))
        .unwrap();
    assert_eq!(candidates.albums.len(), 1);
    assert_eq!(candidates.albums[0].0.title, "First Light");

    // Search only metadata, never index IDs, unknown years or track numbers.
    let track_id = library.list_tracks(None, 1, 0).unwrap().0[0].id.clone();
    for query in ["!!!", " ", "absent", "9999", "65535", &track_id] {
        let candidates = library
            .search_candidates(&LibrarySearchQuery::new(query))
            .unwrap();
        assert!(
            candidates.artists.is_empty()
                && candidates.albums.is_empty()
                && candidates.tracks.is_empty(),
            "{query}"
        );
    }
}

#[test]
fn catalog_candidates_are_not_truncated_before_ranking() {
    let workspace = TestCatalog::new();
    for i in 1..=60 {
        workspace.audio(&format!("Artist/Album/{i:02} Moonlight {i}.mp3"));
    }
    workspace.audio("Z Artist/Album/Light.mp3");
    let library = workspace.library();
    let candidates = library
        .search_candidates(&LibrarySearchQuery::new("light"))
        .unwrap();
    assert_eq!(candidates.tracks.len(), 61);
    let exact = candidates
        .tracks
        .iter()
        .position(|(track, _)| track.title == "Light")
        .unwrap();
    assert!(
        exact >= 50,
        "fixture must put the strongest match after the old cap"
    );
    assert_eq!(candidates.tracks[exact].1, 90);
    assert!(candidates
        .tracks
        .iter()
        .take(50)
        .all(|(_, score)| *score < 90));
}

struct TestCatalog(PathBuf);

impl TestCatalog {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("phonolite-search-{}-{nonce}", std::process::id()));
        fs::create_dir_all(path.join("music")).unwrap();
        Self(path)
    }

    fn audio(&self, path: &str) {
        let path = self.0.join("music").join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"not really audio").unwrap();
    }

    fn library(&self) -> Library {
        Library::load_or_scan(
            vec![LibraryRoot::new(String::new(), self.0.join("music"))],
            self.0.join("library.redb"),
        )
        .unwrap()
        .0
    }
}

impl Drop for TestCatalog {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
