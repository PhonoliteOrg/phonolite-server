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

fn billed(library: &Library) -> Arc<HashSet<String>> {
    let revision = library.search_revision();
    let read = library.db.begin_read().unwrap();
    library.search_billed_artist_ids(&read, revision).unwrap()
}

#[test]
fn search_eligibility_reuses_one_shared_entry_and_invalidates_on_catalogue_changes() {
    let workspace = TestCatalog::new();
    workspace.audio("First/Record/Song.mp3");
    let library = workspace.library();
    let first = billed(&library);
    assert!(Arc::ptr_eq(&first, &billed(&library.clone())));
    library
        .search_candidates(&LibrarySearchQuery::new("song"))
        .unwrap();
    assert!(Arc::ptr_eq(&first, &billed(&library)));

    workspace.audio("Second/Record/Other.mp3");
    library.incremental_scan().unwrap();
    let changed = billed(&library);
    assert!(!Arc::ptr_eq(&first, &changed));
    assert_eq!(changed.len(), 2);
    assert_eq!(*changed, library.billed_artist_ids().unwrap());
}

#[test]
fn search_eligibility_invalidates_after_enrichment_and_restore() {
    let workspace = TestCatalog::new();
    workspace.audio("Artist/Record/Song.mp3");
    let library = workspace.library();
    let first = billed(&library);
    let id = first.iter().next().unwrap();
    library
        .update_artist_enrichment(id, Some("Biography".into()), &[], None, None, false)
        .unwrap();
    let enriched = billed(&library);
    assert!(!Arc::ptr_eq(&first, &enriched));
    assert_eq!(*first, *enriched);

    let backup = library.export_metadata().unwrap();
    library.restore_metadata(&backup).unwrap();
    let restored = billed(&library);
    assert!(!Arc::ptr_eq(&enriched, &restored));
    assert_eq!(*restored, library.billed_artist_ids().unwrap());
}

#[test]
fn an_older_read_snapshot_never_populates_a_newer_eligibility_revision() {
    let workspace = TestCatalog::new();
    workspace.audio("First/Record/Song.mp3");
    let library = workspace.library();
    let old_revision = library.search_revision();
    let old_read = library.db.begin_read().unwrap();
    workspace.audio("Second/Record/Other.mp3");
    library.incremental_scan().unwrap();
    let current = billed(&library);
    let old = library
        .search_billed_artist_ids(&old_read, old_revision)
        .unwrap();
    assert_eq!(old.len(), 1);
    assert_eq!(current.len(), 2);
    assert!(Arc::ptr_eq(&current, &billed(&library)));
}

#[test]
fn eligibility_cache_has_a_byte_budget() {
    let mut ids = HashSet::new();
    let mut oversized = String::with_capacity(MAX_BILLED_CACHE_BYTES);
    oversized.push_str("artist");
    ids.insert(oversized);
    assert!(!billed_cache_fits(&ids));
    assert!(billed_cache_fits(&HashSet::from(["artist".to_owned()])));
}

#[test]
fn batch_search_metadata_matches_individual_reads_including_legacy_fallbacks() {
    let workspace = TestCatalog::new();
    workspace.audio("Alice/Record/Song.mp3");
    workspace.audio("Björk/Record/Song.mp3");
    let library = workspace.library();
    let candidates = library
        .search_candidates(&LibrarySearchQuery::new("record"))
        .unwrap();
    for legacy in [false, true] {
        if legacy {
            let write = library.db.begin_write().unwrap();
            write
                .delete_table(crate::storage::ALBUM_CREDITS_TABLE)
                .unwrap();
            write
                .delete_table(crate::storage::TRACK_CREDITS_TABLE)
                .unwrap();
            write.commit().unwrap();
            library.metadata_restored();
        }
        let metadata = library.search_metadata(&candidates).unwrap();
        for ((album, _), display) in candidates
            .albums
            .iter()
            .zip(&metadata.album_artist_displays)
        {
            assert_eq!(
                *display,
                library.album_credits(&album.id).unwrap().display_name
            );
        }
        for ((track, _), display) in candidates
            .tracks
            .iter()
            .zip(&metadata.track_artist_displays)
        {
            assert_eq!(
                *display,
                library.track_credits(&track.id).unwrap().display_name
            );
            assert_eq!(
                metadata.artist_names[&track.artist_id],
                library.get_artist(&track.artist_id).unwrap().unwrap().name
            );
            assert_eq!(
                metadata.album_titles[&track.album_id],
                library.get_album(&track.album_id).unwrap().unwrap().title
            );
        }
    }
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
