use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{artist_identity_id, Artist, ArtistCredit};

use crate::normalization::merge_genres;
use crate::{Library, LibraryRoot, ScanProgress};

fn write_backup_track(workspace: &TestWorkspace, path: &str) {
    workspace.write_audio(path);
    fs::write(
        workspace.path(path),
        tagged_flac(&[
            "TITLE=One",
            "ALBUM=Portable Release",
            "ALBUMARTIST=Portable Artist",
            "ARTIST=Portable Artist",
            "TRACKNUMBER=1",
            "DATE=2024",
        ]),
    )
    .unwrap();
}

fn enriched_backup(workspace: &TestWorkspace) -> (Library, Vec<crate::backup::CatalogBackup>) {
    write_backup_track(workspace, "old/Portable Artist/Portable Release/One.flac");
    let library = workspace.load_library(&[("old-root", "old")]);
    let album = library.list_albums(None, 10, 0).unwrap().0.remove(0);
    library
        .update_album_enrichment(
            &album.id,
            Some("Saved release description".into()),
            &["Jazz".into()],
            false,
        )
        .unwrap();
    library
        .update_artist_enrichment(
            &album.artist_id,
            Some("Saved biography".into()),
            &["Jazz".into()],
            None,
            None,
            false,
        )
        .unwrap();
    library
        .set_album_source_identity(
            &album.id,
            &crate::AlbumSourceIdentity {
                schema_version: 1,
                context_fingerprint: "backup-test-source-context".into(),
                musicbrainz_release_id: "11111111-1111-4111-8111-111111111111".into(),
                musicbrainz_release_group_id: "22222222-2222-4222-8222-222222222222".into(),
                canonical_title: "Portable Release".into(),
                artist_names: vec!["Portable Artist".into()],
                artist_ids: vec!["33333333-3333-4333-8333-333333333333".into()],
                verified_at_ms: 123,
                evidence: "Verified test programme".into(),
            },
        )
        .unwrap();
    let backup = library.export_metadata().unwrap();
    (library, backup)
}

#[test]
fn metadata_backup_matches_renamed_paths_and_root_ids_without_replacing_local_fields() {
    let source = TestWorkspace::new("backup-source");
    let (old, backup) = enriched_backup(&source);
    let target = TestWorkspace::new("backup-moved");
    write_backup_track(
        &target,
        "new/renamed artist/renamed folder/renamed file.flac",
    );
    let library = target.load_library(&[("new-root", "new")]);
    let album = library.list_albums(None, 10, 0).unwrap().0.remove(0);
    let original_id = old.list_albums(None, 10, 0).unwrap().0.remove(0).id;
    assert_ne!(album.id, original_id);
    let preview = library.preview_metadata(&backup).unwrap();
    assert_eq!(preview.matched_albums, 1);
    assert!(library
        .get_album(&album.id)
        .unwrap()
        .unwrap()
        .summary
        .is_none());
    assert_eq!(
        library.export_metadata().unwrap().len(),
        1,
        "preview must not retain the upload"
    );
    let report = library.restore_metadata(&backup).unwrap();
    assert_eq!(report.updated_albums, 1);
    assert_eq!(report.updated_artists, 1);
    let restored = library.get_album(&album.id).unwrap().unwrap();
    assert_eq!(
        restored.summary.as_deref(),
        Some("Saved release description")
    );
    assert_eq!(restored.folder_relpath, album.folder_relpath);
    assert_eq!(library.album_metadata_stats().unwrap().completed, 1);
    assert_eq!(
        library
            .get_artist(&album.artist_id)
            .unwrap()
            .unwrap()
            .summary
            .as_deref(),
        Some("Saved biography")
    );
    assert!(library
        .get_album_source_identity(&album.id, "backup-test-source-context")
        .unwrap()
        .is_some());
    assert!(library
        .get_album_source_identity(&album.id, "changed-source-context")
        .unwrap()
        .is_none());
    assert_eq!(library.restore_metadata(&backup).unwrap().updated_albums, 0);
    assert_eq!(
        library.export_metadata().unwrap().len(),
        2,
        "repeated uploads are deduplicated"
    );
    library
        .update_album_enrichment(
            &album.id,
            Some("New local description".into()),
            &["Rock".into()],
            true,
        )
        .unwrap();
    library.restore_metadata(&backup).unwrap();
    assert_eq!(
        library
            .get_album(&album.id)
            .unwrap()
            .unwrap()
            .summary
            .as_deref(),
        Some("New local description")
    );
    assert_eq!(
        library.get_album(&album.id).unwrap().unwrap().genres,
        ["Rock"]
    );
}

#[test]
fn metadata_backup_retains_missing_music_across_restart_export_and_later_scan() {
    let source = TestWorkspace::new("backup-missing-source");
    let (_old, backup) = enriched_backup(&source);
    let target = TestWorkspace::new("backup-missing-target");
    target.write_audio("new/Unrelated/Other/Two.mp3");
    let library = target.load_library(&[("new-root", "new")]);
    let report = library.restore_metadata(&backup).unwrap();
    assert_eq!(report.unmatched_albums, 1);
    assert_eq!(report.unmatched_artists, 1);
    assert_eq!(report.library_albums_without_backup, 1);
    assert_eq!(
        library.stats().unwrap().albums,
        1,
        "unmatched records are never ghost albums"
    );
    assert_eq!(
        library.export_metadata().unwrap()[1].albums[0]
            .metadata
            .summary
            .as_deref(),
        Some("Saved release description")
    );
    drop(library);
    let library = target.load_library(&[("new-root", "new")]);
    write_backup_track(&target, "new/moved/Returned/One.flac");
    library.incremental_scan().unwrap();
    let restored = library
        .list_albums(None, 10, 0)
        .unwrap()
        .0
        .into_iter()
        .find(|album| album.title == "Portable Release")
        .unwrap();
    assert_eq!(
        restored.summary.as_deref(),
        Some("Saved release description")
    );
    assert_eq!(restored.genres, ["Jazz"]);
    assert!(library
        .get_album_source_identity(&restored.id, "backup-test-source-context")
        .unwrap()
        .is_some());
    assert_eq!(library.album_metadata_stats().unwrap().completed, 1);
    // A future export carries unresolved imports too, enabling another redeploy.
    assert!(library.export_metadata().unwrap().len() >= 2);
    fs::remove_file(target.path("new/moved/Returned/One.flac")).unwrap();
    library.incremental_scan().unwrap();
    write_backup_track(&target, "new/moved/Returned/One.flac");
    library.incremental_scan().unwrap();
    assert!(library
        .list_albums(None, 10, 0)
        .unwrap()
        .0
        .iter()
        .any(|album| album.summary.as_deref() == Some("Saved release description")));
    library.scan_with_progress(true, None, &|_| {}).unwrap();
    assert!(
        library.export_metadata().unwrap().len() >= 2,
        "root/schema rebuilds retain imports"
    );
    assert!(library
        .list_albums(None, 10, 0)
        .unwrap()
        .0
        .iter()
        .any(|album| album.summary.as_deref() == Some("Saved release description")));
    library.rescan().unwrap();
    assert_eq!(
        library.export_metadata().unwrap().len(),
        1,
        "explicit reindex clears retained imports"
    );
}

#[test]
fn metadata_backup_rejects_same_path_with_changed_release_or_tracklist() {
    let source = TestWorkspace::new("backup-identity");
    let (library, mut backup) = enriched_backup(&source);
    let album = library.list_albums(None, 10, 0).unwrap().0.remove(0);
    backup[0].albums[0].tracks[0].track.title = "A different song".into();
    assert_eq!(
        library.preview_metadata(&backup).unwrap().unmatched_albums,
        1
    );
    backup[0].albums[0].tracks[0].track.title = "One".into();
    backup[0].albums[0].source.year = Some(2001);
    assert_eq!(
        library.preview_metadata(&backup).unwrap().unmatched_albums,
        1
    );
    assert_eq!(
        library
            .get_album(&album.id)
            .unwrap()
            .unwrap()
            .summary
            .as_deref(),
        Some("Saved release description")
    );
}

#[test]
fn metadata_backup_keeps_ambiguous_editions_unattached() {
    let source = TestWorkspace::new("backup-ambiguous-source");
    let (_old, mut backup) = enriched_backup(&source);
    let target = TestWorkspace::new("backup-ambiguous-target");
    write_backup_track(&target, "new/artist/release/One.flac");
    let library = target.load_library(&[("new-root", "new")]);
    let mut album = library.list_albums(None, 10, 0).unwrap().0.remove(0);
    let mut track = library.get_album_tracks(&album.id).unwrap().remove(0);
    album.id = "other-edition".into();
    album.folder_relpath = "new-root::artist/other release".into();
    track.id = "other-track".into();
    track.album_id = album.id.clone();
    let db = library.db();
    let write = db.begin_write().unwrap();
    write
        .open_table(crate::storage::ALBUMS_TABLE)
        .unwrap()
        .insert(
            album.id.as_str(),
            crate::storage::encode_value(&album).unwrap().as_slice(),
        )
        .unwrap();
    write
        .open_table(crate::storage::TRACKS_TABLE)
        .unwrap()
        .insert(
            track.id.as_str(),
            crate::storage::encode_value(&track).unwrap().as_slice(),
        )
        .unwrap();
    write.commit().unwrap();
    backup[0].albums[0].source.folder_relpath = "old-root::no current path hint".into();
    let report = library.restore_metadata(&backup).unwrap();
    assert_eq!(report.ambiguous_albums, 1);
    assert_eq!(report.matched_albums, 0);
    assert!(library
        .get_album(&album.id)
        .unwrap()
        .unwrap()
        .summary
        .is_none());
}

#[test]
fn metadata_backup_restores_verified_credit_repairs_through_the_regular_catalog_builder() {
    let source = TestWorkspace::new("backup-credit-source");
    let target = TestWorkspace::new("backup-credit-target");
    let tags = tagged_flac(&[
        "TITLE=Song",
        "ALBUM=Album",
        "ARTIST=Earth, Wind, Fire",
        "ALBUMARTIST=Earth, Wind, Fire",
    ]);
    for (workspace, path) in [
        (&source, "root/Old/Album/Song.flac"),
        (&target, "root/New/Album/Song.flac"),
    ] {
        workspace.write_audio(path);
        fs::write(workspace.path(path), &tags).unwrap();
    }
    let old = source.load_library(&[("old", "root")]);
    let album = old.list_albums(None, 1, 0).unwrap().0.remove(0);
    assert!(old
        .resolve_album_display_credit(
            &album.id,
            &[ArtistCredit {
                artist_id: "verified-provider-group-id".into(),
                name: "Earth, Wind & Fire".into(),
                credited_name: "Earth, Wind, Fire".into(),
                role: "album_artist".into(),
                join_phrase: String::new(),
                source: "musicbrainz".into(),
            }]
        )
        .unwrap());
    let album = old.get_album(&album.id).unwrap().unwrap();
    old.update_album_enrichment(
        &album.id,
        Some("Verified release".into()),
        &["Soul".into()],
        false,
    )
    .unwrap();
    old.update_artist_enrichment(
        &album.artist_id,
        Some("Verified group biography".into()),
        &[],
        None,
        None,
        false,
    )
    .unwrap();
    let backup = old.export_metadata().unwrap();
    let library = target.load_library(&[("new", "root")]);
    let report = library.restore_metadata(&backup).unwrap();
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(report.restored_credit_resolutions, 1);
    assert_eq!(report.matched_albums, 1);
    assert_eq!(report.matched_artists, 1);
    let restored = library.list_albums(None, 1, 0).unwrap().0.remove(0);
    assert_eq!(restored.artist_names, ["Earth, Wind & Fire"]);
    assert_eq!(restored.summary.as_deref(), Some("Verified release"));
    assert_eq!(
        library
            .get_artist(&restored.artist_id)
            .unwrap()
            .unwrap()
            .summary
            .as_deref(),
        Some("Verified group biography")
    );
    assert_eq!(
        library
            .restore_metadata(&backup)
            .unwrap()
            .restored_credit_resolutions,
        0
    );
}

#[test]
fn album_metadata_counts_only_complete_stored_records_and_survives_reload_and_rescans() {
    let workspace = TestWorkspace::new("metadata-counts");
    workspace.write_audio("root/Artist/First/One.mp3");
    workspace.write_audio("root/Artist/Second/Two.mp3");
    let library = workspace.load_library(&[("", "root")]);
    let albums = library.list_albums(None, 10, 0).unwrap().0;
    let first = &albums[0].id;
    let second = &albums[1].id;
    assert_eq!(
        library.album_metadata_stats().unwrap(),
        crate::AlbumMetadataStats {
            completed: 0,
            total: 2
        }
    );
    library
        .update_album_enrichment(first, Some("First album description.".into()), &[], false)
        .unwrap();
    assert_eq!(
        library.album_metadata_stats().unwrap().completed,
        0,
        "a partial provider response is incomplete"
    );
    library
        .update_album_enrichment(first, None, &["Jazz".into()], false)
        .unwrap();
    assert_eq!(library.album_metadata_stats().unwrap().completed, 1);
    assert!(!library
        .update_album_enrichment(
            first,
            Some("First album description.".into()),
            &["Jazz".into()],
            false
        )
        .unwrap());
    assert_eq!(
        library.album_metadata_stats().unwrap().completed,
        1,
        "duplicate results must not count twice"
    );
    library
        .update_album_enrichment(second, None, &["Rock".into()], false)
        .unwrap();
    assert_eq!(library.album_metadata_stats().unwrap().completed, 1);
    library
        .update_album_enrichment(second, Some("Second album description.".into()), &[], false)
        .unwrap();
    assert_eq!(library.album_metadata_stats().unwrap().completed, 2);
    library
        .update_album_enrichment(second, None, &[" ".into()], true)
        .unwrap();
    assert_eq!(
        library.album_metadata_stats().unwrap().completed,
        1,
        "completion follows the stored record when fields become missing"
    );

    fs::remove_file(workspace.path("root/Artist/Second/Two.mp3")).unwrap();
    library.incremental_scan().unwrap();
    assert_eq!(
        library.album_metadata_stats().unwrap(),
        crate::AlbumMetadataStats {
            completed: 1,
            total: 1
        }
    );
    drop(library);
    let reloaded = workspace.load_library(&[("", "root")]);
    assert_eq!(
        reloaded.album_metadata_stats().unwrap(),
        crate::AlbumMetadataStats {
            completed: 1,
            total: 1
        }
    );
    reloaded.rescan().unwrap();
    assert_eq!(
        reloaded.album_metadata_stats().unwrap(),
        crate::AlbumMetadataStats {
            completed: 0,
            total: 1
        },
        "a full reindex clears old enrichment and its completion count"
    );
}

#[test]
fn legacy_metadata_count_upgrade_serializes_with_concurrent_enrichments() {
    let workspace = TestWorkspace::new("metadata-upgrade");
    workspace.write_audio("root/Artist/First/One.mp3");
    workspace.write_audio("root/Artist/Second/Two.mp3");
    let library = workspace.load_library(&[("", "root")]);
    let albums = library.list_albums(None, 10, 0).unwrap().0;
    {
        let db = library.db();
        let write = db.begin_write().unwrap();
        write
            .open_table(crate::storage::META_TABLE)
            .unwrap()
            .remove(crate::storage::META_ALBUM_METADATA_KEY)
            .unwrap();
        write.commit().unwrap();
    }
    std::thread::scope(|scope| {
        for album in &albums {
            scope.spawn(|| {
                library
                    .update_album_enrichment(
                        &album.id,
                        Some("Stored album description.".into()),
                        &["Jazz".into()],
                        false,
                    )
                    .unwrap();
            });
        }
        let snapshot = library.album_metadata_stats().unwrap();
        assert_eq!(snapshot.total, 2);
        assert!(snapshot.completed <= 2);
    });
    assert_eq!(
        library.album_metadata_stats().unwrap(),
        crate::AlbumMetadataStats {
            completed: 2,
            total: 2
        }
    );
}

#[test]
fn scan_progress_tracks_files_and_groups_multidisc_albums_before_commit() {
    let workspace = TestWorkspace::new("scan-progress");
    workspace.write_audio("root/Artist/First/CD1/One.mp3");
    workspace.write_audio("root/Artist/First/CD2/Two.mp3");
    workspace.write_audio("root/Artist/Second/Three.mp3");
    let events = std::sync::Mutex::new(Vec::new());
    let observer = |progress| events.lock().unwrap().push(progress);
    let db = Library::open_db(&workspace.path("library.redb")).unwrap();
    let (library, scanned) = Library::load_or_scan_with_db_and_progress(
        vec![LibraryRoot::new("".into(), workspace.path("root"))],
        db,
        &observer,
    )
    .unwrap();
    assert!(scanned);
    assert_eq!(library.stats().unwrap().albums, 2);
    let progress = events.lock().unwrap();
    assert_eq!(progress.first(), Some(&ScanProgress::Discovering));
    assert_eq!(progress.last(), Some(&ScanProgress::Committing));
    let tracks: Vec<_> = progress
        .iter()
        .filter_map(|event| match event {
            ScanProgress::ReadingTracks { completed, total } => {
                assert_eq!(*total, 3);
                Some(*completed)
            }
            _ => None,
        })
        .collect();
    assert_eq!(tracks, vec![0, 1, 2, 3]);
    let albums: Vec<_> = progress
        .iter()
        .filter_map(|event| match event {
            ScanProgress::IndexingAlbums { completed, total } => {
                assert_eq!(*total, 2);
                Some(*completed)
            }
            _ => None,
        })
        .collect();
    assert_eq!(albums.first(), Some(&0));
    assert_eq!(albums.last(), Some(&2));
    assert!(albums.windows(2).all(|pair| pair[0] <= pair[1]));
    drop(progress);

    events.lock().unwrap().clear();
    library.scan_with_progress(false, None, &observer).unwrap();
    assert_eq!(
        *events.lock().unwrap(),
        vec![ScanProgress::Discovering],
        "an unchanged index does not report catalog work that was skipped"
    );
    workspace.write_audio("root/Artist/Third/Four.mp3");
    let stats = library
        .scan_with_progress(
            false,
            Some(&[workspace.path("root/Artist/Third")]),
            &observer,
        )
        .unwrap();
    assert_eq!(stats.albums, 3);
    assert!(events
        .lock()
        .unwrap()
        .contains(&ScanProgress::IndexingAlbums {
            completed: 3,
            total: 3
        }));
}

#[test]
fn empty_scan_progress_finishes_with_zero_totals() {
    let workspace = TestWorkspace::new("empty-scan-progress");
    fs::create_dir_all(workspace.path("root")).unwrap();
    let library = workspace.load_library(&[("", "root")]);
    let events = std::sync::Mutex::new(Vec::new());
    let stats = library
        .scan_with_progress(true, None, &|progress| {
            events.lock().unwrap().push(progress)
        })
        .unwrap();
    assert_eq!(stats.albums, 0);
    let events = events.lock().unwrap();
    assert!(events.contains(&ScanProgress::ReadingTracks {
        completed: 0,
        total: 0
    }));
    assert!(events.contains(&ScanProgress::IndexingAlbums {
        completed: 0,
        total: 0
    }));
    assert_eq!(events.last(), Some(&ScanProgress::Committing));
}

#[test]
fn merge_genres_splits_bullet_mojibake_and_dedupes() {
    let mut genres = vec![
        "Rock \u{00C2}\u{20AC}\u{00A2} Alternative Rock".to_string(),
        "rock".to_string(),
    ];
    let incoming = vec!["Industrial Rock \u{2022} Alternative Rock".to_string()];

    merge_genres(&mut genres, &incoming);

    assert_eq!(genres, vec!["Rock", "Alternative Rock", "Industrial Rock"]);
}

#[test]
fn genre_cleanup_preserves_subgenres_and_compound_labels() {
    let mut genres = Vec::new();
    merge_genres(
        &mut genres,
        &[
            "Contemporary classical".into(),
            "Instrumental rock".into(),
            "Electronic rock".into(),
            "Dark ambient".into(),
            "funk / soul".into(),
            "Musique classique".into(),
        ],
    );
    assert_eq!(
        genres,
        [
            "Contemporary classical",
            "Instrumental rock",
            "Electronic rock",
            "Dark ambient",
            "funk / soul",
            "Classical"
        ]
    );
}

#[test]
fn copied_album_across_roots_collapses_to_one_catalog_entry() {
    let workspace = TestWorkspace::new("copied-album");
    workspace.write_audio("root-a/Artist/Album/Song.mp3");
    workspace.write_audio("root-b/Artist/Album/Song.mp3");

    let library = workspace.load_library(&[("", "root-a"), ("root-b", "root-b")]);
    let stats = library.stats().unwrap();

    assert_eq!(stats.artists, 1);
    assert_eq!(stats.albums, 1);
    assert_eq!(stats.tracks, 1);
}

#[test]
fn normalized_artist_names_share_one_artist_with_distinct_albums() {
    let workspace = TestWorkspace::new("artist-merge");
    workspace.write_audio("root-a/Jay-Z/First/Song.mp3");
    workspace.write_audio("root-b/jay z/Second/Song.mp3");

    let library = workspace.load_library(&[("", "root-a"), ("root-b", "root-b")]);
    let (artists, total) = library.list_artists(None, 20, 0).unwrap();

    assert_eq!(total, 1);
    assert_eq!(artists.len(), 1);
    let albums = library.list_artist_albums(&artists[0].id).unwrap();
    let mut titles = albums
        .into_iter()
        .map(|album| album.title)
        .collect::<Vec<_>>();
    titles.sort();
    assert_eq!(titles, vec!["First", "Second"]);
}

#[test]
fn canonical_album_artist_merge_reuses_local_artist_identity() {
    let workspace = TestWorkspace::new("canonical-album-artist");
    workspace.write_audio("root/System Of A Down/System Of A Down (Deluxe)/Song.mp3");
    workspace.write_audio("root/System Of A Down/Toxicity/Song.mp3");

    let library = workspace.load_library(&[("", "root")]);
    let (albums, total_albums) = library.list_albums(None, 20, 0).unwrap();
    let toxicity = albums
        .iter()
        .find(|album| album.title == "Toxicity")
        .expect("toxicity album");

    assert_eq!(total_albums, 2);
    assert!(library
        .merge_album_artists(
            &toxicity.id,
            &[Artist {
                id: artist_identity_id("System of a Down"),
                name: "System of a Down".to_string(),
                genres: Vec::new(),
                summary: None,
                logo_ref: None,
                banner_ref: None,
            }],
        )
        .unwrap());

    let (artists, total_artists) = library.list_artists(None, 20, 0).unwrap();
    assert_eq!(total_artists, 1);
    assert_eq!(artists[0].name, "System of a Down");
    assert_eq!(library.list_artist_albums(&artists[0].id).unwrap().len(), 2);
}

#[test]
fn external_album_credits_require_and_preserve_local_primary() {
    let workspace = TestWorkspace::new("credit-primary");
    workspace.write_audio("root/Janine Jansen/The Four Seasons/Song.mp3");
    let library = workspace.load_library(&[("", "root")]);
    let album = library.list_albums(None, 1, 0).unwrap().0.remove(0);
    let mut primary = library.get_artist(&album.artist_id).unwrap().unwrap();
    let unrelated = Artist {
        id: artist_identity_id("Jacques Loussier Trio"),
        name: "Jacques Loussier Trio".into(),
        genres: Vec::new(),
        summary: None,
        logo_ref: None,
        banner_ref: None,
    };

    assert!(!library
        .merge_album_artists(&album.id, &[unrelated.clone()])
        .unwrap());
    assert!(library.get_artist(&unrelated.id).unwrap().is_none());
    assert_eq!(library.stats().unwrap().artists, 1);
    assert_eq!(
        library.get_album(&album.id).unwrap().unwrap().artist_id,
        album.artist_id
    );

    // Provider IDs and order cannot change the identity from local files.
    primary.id = "provider-specific-id".into();
    primary.name = "JANINE JANSEN".into();
    assert!(library
        .merge_album_artists(&album.id, &[unrelated.clone(), primary])
        .unwrap());
    let enriched = library.get_album(&album.id).unwrap().unwrap();
    assert_eq!(enriched.artist_id, album.artist_id);
    assert_eq!(
        enriched.all_artist_ids(),
        &[album.artist_id.clone(), unrelated.id.clone()]
    );
    assert_eq!(
        library.get_album_tracks(&album.id).unwrap()[0].artist_id,
        album.artist_id
    );
    assert_eq!(library.list_tracks(Some("janine"), 10, 0).unwrap().1, 1);
    assert_eq!(library.list_tracks(Some("loussier"), 10, 0).unwrap().1, 0);
    assert!(library
        .get_artist("provider-specific-id")
        .unwrap()
        .is_none());
    let local = library.get_local_album(&album.id).unwrap().unwrap();
    assert_eq!(local.artist_names, ["Janine Jansen"]);
    assert_eq!(local.all_artist_ids(), &[album.artist_id]);
}

#[test]
fn duplicate_album_can_contribute_extra_tracks() {
    let workspace = TestWorkspace::new("extra-track");
    workspace.write_audio("root-a/Artist/Album/Song.mp3");
    workspace.write_audio("root-b/Artist/Album/Song.mp3");
    workspace.write_audio("root-b/Artist/Album/Bonus.mp3");

    let library = workspace.load_library(&[("", "root-a"), ("root-b", "root-b")]);
    let (albums, total) = library.list_albums(None, 20, 0).unwrap();
    let tracks = library.get_album_tracks(&albums[0].id).unwrap();
    let mut titles = tracks
        .into_iter()
        .map(|track| track.title)
        .collect::<Vec<_>>();
    titles.sort();

    assert_eq!(total, 1);
    assert_eq!(titles, vec!["Bonus", "Song"]);
}

#[test]
fn incremental_scan_merges_duplicate_album_from_later_root() {
    let workspace = TestWorkspace::new("incremental-extra-track");
    workspace.write_audio("root-a/Artist/Album/Song.mp3");
    let library = workspace.load_library(&[("", "root-a"), ("root-b", "root-b")]);

    workspace.write_audio("root-b/Artist/Album/Song.mp3");
    workspace.write_audio("root-b/Artist/Album/Bonus.mp3");
    let stats = library.incremental_scan().unwrap();
    let (albums, total) = library.list_albums(None, 20, 0).unwrap();
    let tracks = library.get_album_tracks(&albums[0].id).unwrap();

    assert_eq!(stats.artists, 1);
    assert_eq!(stats.albums, 1);
    assert_eq!(stats.tracks, 2);
    assert_eq!(total, 1);
    assert_eq!(tracks.len(), 2);
}

#[test]
fn first_configured_root_wins_duplicate_track_source() {
    let workspace = TestWorkspace::new("root-order");
    workspace.write_audio("root-a/Artist/Album/Song.mp3");
    workspace.write_audio("root-b/Artist/Album/Song.mp3");

    let library = workspace.load_library(&[("preferred", "root-b"), ("", "root-a")]);
    let (tracks, total) = library.list_tracks(None, 20, 0).unwrap();

    assert_eq!(total, 1);
    assert_eq!(tracks[0].file_relpath, "preferred::Artist/Album/Song.mp3");
}

#[test]
fn incremental_scan_reconciles_edits_additions_and_deletions_in_existing_album() {
    let workspace = TestWorkspace::new("incremental-edits");
    workspace.write_audio("root/Artist/Album/First.mp3");
    workspace.write_audio("root/Artist/Album/Deleted.mp3");
    let library = workspace.load_library(&[("", "root")]);
    fs::write(
        workspace.path("root/Artist/Album/First.mp3"),
        b"changed source bytes",
    )
    .unwrap();
    fs::remove_file(workspace.path("root/Artist/Album/Deleted.mp3")).unwrap();
    workspace.write_audio("root/Artist/Album/Added.mp3");
    let stats = library
        .incremental_scan_paths(&[
            workspace.path("root/Artist/Album/First.mp3"),
            workspace.path("root/Artist/Album/Deleted.mp3"),
            workspace.path("root/Artist/Album/Added.mp3"),
        ])
        .unwrap();
    let (tracks, _) = library.list_tracks(None, 20, 0).unwrap();
    assert_eq!(stats.tracks, 2);
    assert!(tracks.iter().all(|track| track.title != "Deleted"));
    assert_eq!(
        tracks
            .iter()
            .find(|track| track.title == "First")
            .unwrap()
            .file_size,
        20
    );
    assert!(tracks.iter().any(|track| track.title == "Added"));
}

#[test]
fn unchanged_incremental_scan_does_not_rebuild_catalog() {
    let workspace = TestWorkspace::new("unchanged-scan");
    workspace.write_audio("root/Artist/Album/Song.mp3");
    let library = workspace.load_library(&[("", "root")]);
    let (albums, _) = library.list_albums(None, 20, 0).unwrap();
    library
        .update_album_enrichment(
            &albums[0].id,
            Some("Retained enrichment".into()),
            &[],
            false,
        )
        .unwrap();
    library.incremental_scan().unwrap();
    assert_eq!(
        library
            .get_album(&albums[0].id)
            .unwrap()
            .unwrap()
            .summary
            .as_deref(),
        Some("Retained enrichment")
    );
}

#[test]
fn unrelated_source_change_preserves_enrichment_credits_and_retry_state() {
    let workspace = TestWorkspace::new("enriched-incremental");
    workspace.write_audio("root/Artist/Album/Song.mp3");
    let library = workspace.load_library(&[("", "root")]);
    let album = library.list_albums(None, 1, 0).unwrap().0.remove(0);
    let artist = library.get_artist(&album.artist_id).unwrap().unwrap();
    library
        .update_artist_enrichment(
            &artist.id,
            Some("Provider artist".into()),
            &["Jazz".into()],
            Some("logo.webp".into()),
            Some("banner.webp".into()),
            true,
        )
        .unwrap();
    library
        .update_album_enrichment(
            &album.id,
            Some("Provider album".into()),
            &["Rock".into()],
            true,
        )
        .unwrap();
    let guest = Artist {
        id: artist_identity_id("Guest"),
        name: "Guest".into(),
        genres: vec!["Blues".into()],
        summary: Some("Provider guest".into()),
        logo_ref: None,
        banner_ref: None,
    };
    library
        .merge_album_artists(&album.id, &[guest.clone(), artist.clone()])
        .unwrap();
    let keys = [
        format!("artist:{}", artist.id),
        format!("album:{}", album.id),
        format!("album_artists:{}", album.id),
    ];
    for key in &keys {
        library.record_external_attempt(key, true).unwrap();
    }
    workspace.write_audio("root/Other/New/Song.mp3");
    library.incremental_scan().unwrap();
    let retained = library.get_album(&album.id).unwrap().unwrap();
    assert_eq!(retained.summary.as_deref(), Some("Provider album"));
    assert_eq!(retained.genres, ["Rock"]);
    assert_eq!(retained.artist_id, artist.id);
    assert_eq!(
        retained.all_artist_ids(),
        &[artist.id.clone(), guest.id.clone()]
    );
    assert_eq!(
        library.get_album_tracks(&album.id).unwrap()[0].artist_id,
        artist.id
    );
    assert_eq!(
        library.list_artist_albums(&guest.id).unwrap()[0].id,
        album.id
    );
    assert_eq!(library.list_albums(Some("guest"), 10, 0).unwrap().1, 1);
    assert_eq!(library.list_tracks(Some("guest"), 10, 0).unwrap().1, 0);
    assert_eq!(library.list_tracks(Some("artist"), 10, 0).unwrap().1, 1);
    assert_eq!(library.stats().unwrap().artists, 3);
    let retained_artist = library.get_artist(&artist.id).unwrap().unwrap();
    assert_eq!(retained_artist.summary.as_deref(), Some("Provider artist"));
    assert_eq!(retained_artist.logo_ref.as_deref(), Some("logo.webp"));
    assert_eq!(retained_artist.banner_ref.as_deref(), Some("banner.webp"));
    assert_eq!(
        library
            .get_artist(&guest.id)
            .unwrap()
            .unwrap()
            .summary
            .as_deref(),
        Some("Provider guest")
    );
    for key in &keys {
        assert!(!library
            .should_attempt_external(key, Duration::from_secs(86_400))
            .unwrap());
    }
}

#[test]
fn startup_identity_recovery_rebuilds_local_metadata_once() {
    use crate::storage::{
        album_name_key, album_search_artist_text, encode_value, track_name_key,
        ALBUMS_BY_NAME_TABLE, ALBUMS_TABLE, META_TABLE, TRACKS_BY_NAME_TABLE, TRACKS_TABLE,
    };

    let workspace = TestWorkspace::new("legacy-credit-recovery");
    workspace.write_audio("root/Tagged Artist/Album/Song.mp3");
    fs::write(
        workspace.path("root/Tagged Artist/artist.json"),
        br#"{"summary":"Local artist description","genres":["Classical"]}"#,
    )
    .unwrap();
    fs::write(
        workspace.path("root/Tagged Artist/Album/album.json"),
        br#"{"summary":"Local album description","genres":["Classical"]}"#,
    )
    .unwrap();
    let library = workspace.load_library(&[("", "root")]);
    let album = library.list_albums(None, 1, 0).unwrap().0.remove(0);
    let primary = library.get_artist(&album.artist_id).unwrap().unwrap();
    let guest = Artist {
        id: artist_identity_id("Unrelated Trio"),
        name: "Unrelated Trio".into(),
        genres: Vec::new(),
        summary: Some("Guest biography".into()),
        logo_ref: None,
        banner_ref: None,
    };
    library
        .update_artist_enrichment(
            &primary.id,
            Some("Old unverified artist description".into()),
            &["Jazz".into()],
            Some("logo.webp".into()),
            Some("banner.webp".into()),
            true,
        )
        .unwrap();
    library
        .update_album_enrichment(
            &album.id,
            Some("Old unverified album description".into()),
            &["Jazz".into()],
            true,
        )
        .unwrap();
    library
        .merge_album_artists(&album.id, &[primary.clone(), guest.clone()])
        .unwrap();
    let old = library.get_album(&album.id).unwrap().unwrap();
    let mut corrupted = old.clone();
    corrupted.artist_id = guest.id.clone();
    corrupted.artist_ids = vec![guest.id.clone(), primary.id.clone()];
    corrupted.artist_names = vec![guest.name.clone(), primary.name.clone()];
    let tracks = library.get_album_tracks(&album.id).unwrap();
    let db = library.db();
    let write = db.begin_write().unwrap();
    {
        // Reproduce the persisted catalog written by the former policy,
        // including its primary-based track and search indexes.
        let mut albums = write.open_table(ALBUMS_TABLE).unwrap();
        albums
            .insert(
                album.id.as_str(),
                encode_value(&corrupted).unwrap().as_slice(),
            )
            .unwrap();
        let mut album_names = write.open_table(ALBUMS_BY_NAME_TABLE).unwrap();
        album_names
            .remove(album_name_key(&album_search_artist_text(&old), &old).as_str())
            .unwrap();
        album_names
            .insert(
                album_name_key(&album_search_artist_text(&corrupted), &corrupted).as_str(),
                album.id.as_bytes(),
            )
            .unwrap();
        let mut track_values = write.open_table(TRACKS_TABLE).unwrap();
        let mut track_names = write.open_table(TRACKS_BY_NAME_TABLE).unwrap();
        for mut track in tracks {
            track_names
                .remove(
                    track_name_key(
                        &primary.name,
                        &album.title,
                        track.disc_no,
                        track.track_no,
                        &track.title,
                        &track.id,
                    )
                    .as_str(),
                )
                .unwrap();
            track_names
                .insert(
                    track_name_key(
                        &guest.name,
                        &album.title,
                        track.disc_no,
                        track.track_no,
                        &track.title,
                        &track.id,
                    )
                    .as_str(),
                    track.id.as_bytes(),
                )
                .unwrap();
            track.artist_id = guest.id.clone();
            track_values
                .insert(track.id.as_str(), encode_value(&track).unwrap().as_slice())
                .unwrap();
        }
        write
            .open_table(META_TABLE)
            .unwrap()
            .remove("external_metadata_identity_policy")
            .unwrap();
    }
    write.commit().unwrap();
    let old_retry_keys = [
        format!("artist:{}", primary.id),
        format!("album:{}", album.id),
        format!("album_artists:{}", album.id),
        format!("artist:{}", guest.id),
    ];
    for key in &old_retry_keys {
        library.record_external_attempt(key, true).unwrap();
    }
    assert_eq!(library.list_tracks(Some("unrelated"), 10, 0).unwrap().1, 1);

    let (recovered, scanned) = Library::load_or_scan_with_db(
        vec![LibraryRoot::new("".into(), workspace.path("root"))],
        library.db(),
    )
    .unwrap();
    assert!(scanned);
    let restored = recovered.get_album(&album.id).unwrap().unwrap();
    assert_eq!(restored.artist_id, primary.id);
    assert_eq!(restored.all_artist_ids(), &[primary.id.clone()]);
    assert_eq!(restored.artist_names, [primary.name.clone()]);
    assert_eq!(restored.summary.as_deref(), Some("Local album description"));
    assert_eq!(restored.genres, ["Classical"]);
    let artist = recovered.get_artist(&primary.id).unwrap().unwrap();
    assert_eq!(artist.summary.as_deref(), Some("Local artist description"));
    assert_eq!(artist.genres, ["Classical"]);
    assert!(artist.logo_ref.is_none());
    assert!(artist.banner_ref.is_none());
    assert_eq!(
        recovered.get_album_tracks(&album.id).unwrap()[0].artist_id,
        primary.id
    );
    assert_eq!(recovered.stats().unwrap().artists, 1);
    assert!(recovered.get_artist(&guest.id).unwrap().is_none());
    assert!(recovered.list_artist_albums(&guest.id).unwrap().is_empty());
    assert_eq!(
        recovered.list_albums(Some("unrelated"), 10, 0).unwrap().1,
        0
    );
    assert_eq!(
        recovered.list_tracks(Some("unrelated"), 10, 0).unwrap().1,
        0
    );
    assert_eq!(recovered.list_tracks(Some("tagged"), 10, 0).unwrap().1, 1);
    for key in &old_retry_keys {
        assert!(recovered
            .should_attempt_external(key, Duration::from_secs(86_400))
            .unwrap());
    }

    // The one-time repair must not erase newly verified enrichment thereafter.
    recovered
        .update_album_enrichment(
            &album.id,
            Some("Verified album description".into()),
            &["Verified genre".into()],
            true,
        )
        .unwrap();
    recovered
        .merge_album_artists(&album.id, &[primary.clone(), guest.clone()])
        .unwrap();
    workspace.write_audio("root/Other Artist/New Album/Song.mp3");
    recovered.incremental_scan().unwrap();
    let retained = recovered.get_album(&album.id).unwrap().unwrap();
    assert_eq!(
        retained.summary.as_deref(),
        Some("Verified album description")
    );
    assert_eq!(retained.genres, ["Verified genre"]);
    assert_eq!(
        recovered
            .get_album(&album.id)
            .unwrap()
            .unwrap()
            .all_artist_ids(),
        &[primary.id, guest.id]
    );
    let (_, rescanned) = Library::load_or_scan_with_db(
        vec![LibraryRoot::new("".into(), workspace.path("root"))],
        recovered.db(),
    )
    .unwrap();
    assert!(!rescanned);
}

#[test]
fn local_metadata_edits_override_retained_provider_fields_without_losing_external_genres() {
    let workspace = TestWorkspace::new("local-source-precedence");
    workspace.write_audio("root/Artist/Album/Song.mp3");
    for source in ["root/Artist/artist.json", "root/Artist/Album/album.json"] {
        fs::write(
            workspace.path(source),
            br#"{"summary":"Local original","genres":["Rock"]}"#,
        )
        .unwrap();
    }
    let library = workspace.load_library(&[("", "root")]);
    let album = library.list_albums(None, 1, 0).unwrap().0.remove(0);
    library
        .update_album_enrichment(
            &album.id,
            Some("Provider override".into()),
            &["Jazz".into()],
            true,
        )
        .unwrap();
    library
        .update_artist_enrichment(
            &album.artist_id,
            Some("Provider override".into()),
            &["Jazz".into()],
            None,
            None,
            true,
        )
        .unwrap();
    for source in ["root/Artist/artist.json", "root/Artist/Album/album.json"] {
        fs::write(
            workspace.path(source),
            br#"{"summary":"Local revised","genres":["Country"]}"#,
        )
        .unwrap();
    }
    library.incremental_scan().unwrap();
    let retained_album = library.get_album(&album.id).unwrap().unwrap();
    let retained_artist = library.get_artist(&album.artist_id).unwrap().unwrap();
    assert_eq!(retained_album.summary.as_deref(), Some("Local revised"));
    assert_eq!(retained_artist.summary.as_deref(), Some("Local revised"));
    assert_eq!(retained_album.genres, ["Country", "Jazz"]);
    assert_eq!(retained_artist.genres, ["Country", "Jazz"]);
    // Removing a populated local field must clear it rather than resurrecting
    // the previous local value from the enriched catalog.
    for source in ["root/Artist/artist.json", "root/Artist/Album/album.json"] {
        fs::write(workspace.path(source), br#"{"genres":["Country"]}"#).unwrap();
    }
    library.incremental_scan().unwrap();
    assert!(library
        .get_album(&album.id)
        .unwrap()
        .unwrap()
        .summary
        .is_none());
    assert!(library
        .get_artist(&album.artist_id)
        .unwrap()
        .unwrap()
        .summary
        .is_none());
}

#[test]
fn removed_sources_prune_enriched_credit_artists_and_retry_keys() {
    let workspace = TestWorkspace::new("enrichment-delete");
    workspace.write_audio("root/Artist/Album/Song.mp3");
    let library = workspace.load_library(&[("", "root")]);
    let album = library.list_albums(None, 1, 0).unwrap().0.remove(0);
    let keys = [
        format!("artist:{}", album.artist_id),
        format!("album:{}", album.id),
        format!("album_artists:{}", album.id),
    ];
    for key in &keys {
        library.record_external_attempt(key, false).unwrap();
    }
    fs::remove_file(workspace.path("root/Artist/Album/Song.mp3")).unwrap();
    library.incremental_scan().unwrap();
    assert_eq!(library.stats().unwrap().artists, 0);
    assert!(library.get_album(&album.id).unwrap().is_none());
    for key in &keys {
        assert!(library
            .should_attempt_external(key, Duration::from_secs(86_400))
            .unwrap());
    }
}

#[test]
fn force_rescan_discards_enrichment_and_retry_state() {
    let workspace = TestWorkspace::new("enrichment-force");
    workspace.write_audio("root/Artist/Album/Song.mp3");
    let library = workspace.load_library(&[("", "root")]);
    let album = library.list_albums(None, 1, 0).unwrap().0.remove(0);
    library
        .update_album_enrichment(&album.id, Some("Provider".into()), &["Jazz".into()], false)
        .unwrap();
    let key = format!("album:{}", album.id);
    library.record_external_attempt(&key, true).unwrap();
    library.rescan().unwrap();
    let reset = library.get_album(&album.id).unwrap().unwrap();
    assert!(reset.summary.is_none());
    assert!(reset.genres.is_empty());
    assert!(library
        .should_attempt_external(&key, Duration::from_secs(86_400))
        .unwrap());
}

#[test]
fn legacy_catalog_without_local_snapshots_retains_missing_enrichment() {
    let workspace = TestWorkspace::new("enrichment-upgrade");
    workspace.write_audio("root/Artist/Album/Song.mp3");
    let library = workspace.load_library(&[("", "root")]);
    let album = library.list_albums(None, 1, 0).unwrap().0.remove(0);
    library
        .update_album_enrichment(&album.id, Some("Provider".into()), &["Jazz".into()], false)
        .unwrap();
    let db = library.db();
    let write = db.begin_write().unwrap();
    write
        .delete_table(redb::TableDefinition::<&str, &[u8]>::new(
            "local_artists_v1",
        ))
        .unwrap();
    write
        .delete_table(redb::TableDefinition::<&str, &[u8]>::new("local_albums_v1"))
        .unwrap();
    write.commit().unwrap();
    assert!(library.get_local_album(&album.id).unwrap().is_none());
    workspace.write_audio("root/Other/New/Song.mp3");
    library.incremental_scan().unwrap();
    let retained = library.get_album(&album.id).unwrap().unwrap();
    assert_eq!(retained.summary.as_deref(), Some("Provider"));
    assert_eq!(retained.genres, ["Jazz"]);
}

#[test]
fn retagging_album_in_place_resets_provider_metadata_credits_and_retry_state() {
    for (label, new_title, new_artist, new_year) in [
        ("retag-title", "Different release", "Original Artist", 2025),
        ("retag-artist", "Original release", "Different Artist", 2001),
    ] {
        let workspace = TestWorkspace::new(label);
        workspace.write_audio("root/Folder Artist/Folder Album/Song.mp3");
        let library = workspace.load_library(&[("", "root")]);
        let path = workspace.path("root/Folder Artist/Folder Album/Song.mp3");
        seed_cached_local_tags(
            &library,
            &path,
            "Original release",
            "Original Artist",
            2001,
            b"original tag fixture",
        );
        library.incremental_scan().unwrap();
        let album = library.list_albums(None, 1, 0).unwrap().0.remove(0);
        let original_artist = library.get_artist(&album.artist_id).unwrap().unwrap();
        library
            .update_album_enrichment(
                &album.id,
                Some("Old release description".into()),
                &["Provider genre".into()],
                false,
            )
            .unwrap();
        let guest = Artist {
            id: artist_identity_id("Old Guest"),
            name: "Old Guest".into(),
            genres: Vec::new(),
            summary: None,
            logo_ref: None,
            banner_ref: None,
        };
        library
            .merge_album_artists(&album.id, &[guest.clone(), original_artist])
            .unwrap();
        let keys = [
            format!("album:{}", album.id),
            format!("album_artists:{}", album.id),
        ];
        for key in &keys {
            library.record_external_attempt(key, true).unwrap();
        }
        seed_cached_local_tags(
            &library,
            &path,
            new_title,
            new_artist,
            new_year,
            b"different retagged source fixture",
        );
        library.incremental_scan().unwrap();
        let retagged = library.get_album(&album.id).unwrap().unwrap();
        assert_eq!(retagged.title, new_title);
        assert_eq!(retagged.year, Some(new_year));
        assert_eq!(retagged.artist_id, artist_identity_id(new_artist));
        assert_eq!(retagged.all_artist_ids(), &[artist_identity_id(new_artist)]);
        assert!(retagged.summary.is_none());
        assert!(retagged.genres.is_empty());
        assert!(library.get_artist(&guest.id).unwrap().is_none());
        assert_eq!(
            library.get_album_tracks(&album.id).unwrap()[0].artist_id,
            retagged.artist_id
        );
        for key in &keys {
            assert!(library
                .should_attempt_external(key, Duration::from_secs(86_400))
                .unwrap());
        }
    }
}

#[test]
fn track_count_change_allows_fresh_credit_resolution_and_keeps_album_metadata_retry() {
    let workspace = TestWorkspace::new("credit-track-count");
    workspace.write_audio("root/Artist/Album/Song.mp3");
    let library = workspace.load_library(&[("", "root")]);
    let album = library.list_albums(None, 1, 0).unwrap().0.remove(0);
    let album_key = format!("album:{}", album.id);
    let credits_key = format!("album_artists:{}", album.id);
    library.record_external_attempt(&album_key, true).unwrap();
    library.record_external_attempt(&credits_key, true).unwrap();
    workspace.write_audio("root/Artist/Album/Bonus.mp3");
    library.incremental_scan().unwrap();
    assert!(!library
        .should_attempt_external(&album_key, Duration::from_secs(86_400))
        .unwrap());
    assert!(library
        .should_attempt_external(&credits_key, Duration::from_secs(86_400))
        .unwrap());
}

fn seed_cached_local_tags(
    library: &Library,
    path: &std::path::Path,
    title: &str,
    artist: &str,
    year: i32,
    source: &[u8],
) {
    let tags = metadata::TagInfo {
        album: Some(title.into()),
        album_artist: Some(artist.into()),
        year: Some(year),
        title: Some("Song".into()),
        ..Default::default()
    };
    seed_cached_tags(library, path, tags, source);
}

fn seed_cached_tags(
    library: &Library,
    path: &std::path::Path,
    tags: metadata::TagInfo,
    source: &[u8],
) {
    // Supply parsed tags and an updated fingerprint without depending on an
    // audio encoder or network services in catalog attribution tests.
    fs::write(path, source).unwrap();
    let metadata = fs::metadata(path).unwrap();
    let bytes = bincode::serialize(&(metadata.len(), metadata.modified().ok(), tags)).unwrap();
    let db = library.db();
    let write = db.begin_write().unwrap();
    {
        let mut table = write
            .open_table(redb::TableDefinition::<&str, &[u8]>::new("file_tags_v4"))
            .unwrap();
        table
            .insert(path.to_string_lossy().as_ref(), bytes.as_slice())
            .unwrap();
    }
    write.commit().unwrap();
}

#[test]
fn startup_repairs_composite_artists_once_without_copying_their_metadata() {
    use crate::storage::{
        album_index_key, artist_name_key, encode_value, ALBUMS_TABLE, ALBUM_CREDITS_TABLE,
        ARTISTS_BY_NAME_TABLE, ARTISTS_TABLE, ARTIST_ALBUMS_TABLE, META_TABLE, TRACKS_TABLE,
        TRACK_CREDITS_TABLE,
    };
    let workspace = TestWorkspace::new("composite-credit-startup-repair");
    for (path, comments) in [
        (
            "root/Artist A/Joint/Song.flac",
            vec![
                "ALBUM=Joint",
                "ALBUMARTIST=Artist A, Artist B",
                "ARTIST=Artist A, Artist B",
                "TITLE=Duet",
            ],
        ),
        (
            "root/Artist A/Solo/Song.flac",
            vec![
                "ALBUM=Solo",
                "ALBUMARTIST=Artist A",
                "ARTIST=Artist A",
                "TITLE=Solo Song",
            ],
        ),
    ] {
        let path = workspace.path(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, tagged_flac(&comments)).unwrap();
    }
    let library = workspace.load_library(&[("", "root")]);
    let artist_a = artist_identity_id("Artist A");
    let artist_b = artist_identity_id("Artist B");
    library
        .update_artist_enrichment(
            &artist_a,
            Some("Artist A's own biography".into()),
            &[],
            Some("own-portrait".into()),
            None,
            true,
        )
        .unwrap();
    let mut album = library
        .list_albums(Some("Joint"), 1, 0)
        .unwrap()
        .0
        .remove(0);
    let mut track = library.get_album_tracks(&album.id).unwrap().remove(0);
    let composite = Artist {
        id: artist_identity_id("Artist A, Artist B"),
        name: "Artist A, Artist B".into(),
        genres: Vec::new(),
        summary: Some("Incorrect combined biography".into()),
        logo_ref: Some("incorrect-combined-portrait".into()),
        banner_ref: None,
    };
    let credits = crate::credits::source_credits(
        &composite.name,
        &[composite.name.clone()],
        "album_artist",
        "local_tags",
    );
    let mut track_credits = credits.clone();
    track_credits.credits[0].role = "track_artist".into();
    let db = library.db();
    let write = db.begin_write().unwrap();
    {
        // Reproduce an existing catalog written before display credits were
        // resolved into independent identities. Only this temporary DB is used.
        let mut artists = write.open_table(ARTISTS_TABLE).unwrap();
        artists.remove(artist_b.as_str()).unwrap();
        artists
            .insert(
                composite.id.as_str(),
                encode_value(&composite).unwrap().as_slice(),
            )
            .unwrap();
        let mut names = write.open_table(ARTISTS_BY_NAME_TABLE).unwrap();
        names
            .remove(artist_name_key("Artist B", &artist_b).as_str())
            .unwrap();
        names
            .insert(
                artist_name_key(&composite.name, &composite.id).as_str(),
                composite.id.as_bytes(),
            )
            .unwrap();
        let mut links = write.open_table(ARTIST_ALBUMS_TABLE).unwrap();
        links
            .remove(album_index_key(&artist_a, &album).as_str())
            .unwrap();
        links
            .remove(album_index_key(&artist_b, &album).as_str())
            .unwrap();
        links
            .insert(
                album_index_key(&composite.id, &album).as_str(),
                album.id.as_bytes(),
            )
            .unwrap();
        album.artist_id = composite.id.clone();
        album.artist_ids = vec![composite.id.clone()];
        album.artist_names = vec![composite.name.clone()];
        write
            .open_table(ALBUMS_TABLE)
            .unwrap()
            .insert(album.id.as_str(), encode_value(&album).unwrap().as_slice())
            .unwrap();
        write
            .open_table(crate::indexing::LOCAL_ALBUMS)
            .unwrap()
            .insert(album.id.as_str(), encode_value(&album).unwrap().as_slice())
            .unwrap();
        write
            .open_table(ALBUM_CREDITS_TABLE)
            .unwrap()
            .insert(
                album.id.as_str(),
                encode_value(&credits).unwrap().as_slice(),
            )
            .unwrap();
        track.artist_id = composite.id.clone();
        write
            .open_table(TRACKS_TABLE)
            .unwrap()
            .insert(track.id.as_str(), encode_value(&track).unwrap().as_slice())
            .unwrap();
        write
            .open_table(TRACK_CREDITS_TABLE)
            .unwrap()
            .insert(
                track.id.as_str(),
                encode_value(&track_credits).unwrap().as_slice(),
            )
            .unwrap();
        write
            .open_table(META_TABLE)
            .unwrap()
            .remove("artist_credit_identity_policy")
            .unwrap();
        write
            .open_table(redb::TableDefinition::<&str, &[u8]>::new("file_tags_v3"))
            .unwrap()
            .insert("old-cache", &[0u8][..])
            .unwrap();
    }
    write.commit().unwrap();
    let roots = vec![LibraryRoot::new("".into(), workspace.path("root"))];
    let (repaired, scanned) = Library::load_or_scan_with_db(roots.clone(), db.clone()).unwrap();
    assert!(scanned);
    assert_eq!(repaired.stats().unwrap().artists, 2);
    assert!(repaired.get_artist(&composite.id).unwrap().is_none());
    let retained = repaired.get_artist(&artist_a).unwrap().unwrap();
    assert_eq!(
        retained.summary.as_deref(),
        Some("Artist A's own biography")
    );
    assert_eq!(retained.logo_ref.as_deref(), Some("own-portrait"));
    let individual_b = repaired.get_artist(&artist_b).unwrap().unwrap();
    assert!(individual_b.summary.is_none());
    assert!(individual_b.logo_ref.is_none());
    let repaired_album = repaired.get_album(&album.id).unwrap().unwrap();
    assert_eq!(
        repaired_album.all_artist_ids(),
        &[artist_a.clone(), artist_b.clone()]
    );
    assert_eq!(
        repaired.get_album_tracks(&album.id).unwrap()[0].id,
        track.id
    );
    let relationships = repaired.artist_release_relationships(&artist_a).unwrap();
    assert_eq!(relationships.albums.len(), 1);
    assert_eq!(relationships.collab_albums[0].album.id, album.id);
    assert_eq!(
        repaired
            .artist_release_relationships(&artist_b)
            .unwrap()
            .collab_albums[0]
            .track_ids,
        [track.id]
    );
    assert!(db
        .begin_read()
        .unwrap()
        .open_table(redb::TableDefinition::<&str, &[u8]>::new("file_tags_v3"))
        .is_err());
    assert!(!Library::load_or_scan_with_db(roots, db).unwrap().1);
}

#[test]
fn headline_albums_do_not_repeat_in_other_credits() {
    use crate::storage::{encode_value, ALBUM_CREDITS_TABLE};

    for shared in [false, true] {
        let workspace = TestWorkspace::new("headline-other-credits");
        workspace.write_audio("root/Main/Album/01.mp3");
        let library = workspace.load_library(&[("", "root")]);
        let names = if shared {
            vec!["Main", "Partner"]
        } else {
            vec!["Main"]
        };
        seed_cached_tags(
            &library,
            &workspace.path("root/Main/Album/01.mp3"),
            metadata::TagInfo {
                album_artist: Some(names.join(" & ")),
                album_artists: names.iter().map(|name| (*name).into()).collect(),
                artist: Some("Main".into()),
                artists: vec!["Main".into()],
                title: Some("Song".into()),
                contributions: vec![
                    metadata::TagContribution {
                        name: "Main".into(),
                        role: "composer".into(),
                    },
                    metadata::TagContribution {
                        name: "Partner".into(),
                        role: "producer".into(),
                    },
                ],
                ..Default::default()
            },
            b"headline supporting roles fixture",
        );
        library.incremental_scan().unwrap();
        let album = library.list_albums(None, 1, 0).unwrap().0.remove(0);
        let mut credits = library.album_credits(&album.id).unwrap();
        credits.credits.extend(
            crate::credits::source_credits("Main", &["Main".into()], "producer", "local_tags")
                .credits,
        );
        let db = library.db();
        let write = db.begin_write().unwrap();
        write
            .open_table(ALBUM_CREDITS_TABLE)
            .unwrap()
            .insert(
                album.id.as_str(),
                encode_value(&credits).unwrap().as_slice(),
            )
            .unwrap();
        write.commit().unwrap();

        for name in &names {
            let releases = library
                .artist_release_relationships(&artist_identity_id(name))
                .unwrap();
            assert_eq!(releases.albums.len(), usize::from(!shared));
            assert_eq!(releases.collab_albums.len(), usize::from(shared));
            assert!(releases.appears_on.is_empty());
            assert!(
                releases.other_credits.is_empty(),
                "{name} is billed on the album"
            );
        }
        if !shared {
            let partner = library
                .artist_release_relationships(&artist_identity_id("Partner"))
                .unwrap();
            assert_eq!(partner.other_credits[0].album.id, album.id);
            assert_eq!(partner.other_credits[0].roles, ["producer"]);
        }
        assert!(library
            .album_credits(&album.id)
            .unwrap()
            .credits
            .iter()
            .any(|credit| credit.artist_id == artist_identity_id("Main")
                && credit.role == "producer"));
        let track = library.get_album_tracks(&album.id).unwrap().remove(0);
        assert!(library
            .track_credits(&track.id)
            .unwrap()
            .credits
            .iter()
            .any(|credit| credit.artist_id == artist_identity_id("Main")
                && credit.role == "composer"));
    }
}

#[test]
fn song_appearances_and_supporting_roles_do_not_become_joint_albums() {
    use crate::storage::{
        album_index_key, artist_name_key, encode_value, ALBUM_CREDITS_TABLE, ARTISTS_BY_NAME_TABLE,
        ARTISTS_TABLE, ARTIST_ALBUMS_TABLE,
    };
    let workspace = TestWorkspace::new("collaboration-role-union");
    workspace.write_audio("root/Main/Album/01.mp3");
    workspace.write_audio("root/Main/Album/02.mp3");
    let library = workspace.load_library(&[("", "root")]);
    for (number, featured) in [("01", true), ("02", false)] {
        seed_cached_tags(
            &library,
            &workspace.path(&format!("root/Main/Album/{number}.mp3")),
            metadata::TagInfo {
                album_artist: Some("Main".into()),
                artist: Some(if featured { "Main feat. Guest" } else { "Main" }.into()),
                artists: if featured {
                    vec!["Main".into(), "Guest".into()]
                } else {
                    vec!["Main".into()]
                },
                title: Some(if featured { "Guest Song" } else { "Solo Song" }.into()),
                track_no: number.parse().ok(),
                contributions: if featured {
                    vec![
                        metadata::TagContribution {
                            name: "Guest".into(),
                            role: "composer".into(),
                        },
                        metadata::TagContribution {
                            name: "Guest".into(),
                            role: "producer".into(),
                        },
                    ]
                } else {
                    Vec::new()
                },
                ..Default::default()
            },
            b"collaboration roles fixture",
        );
    }
    library.incremental_scan().unwrap();
    let album = library.list_albums(None, 1, 0).unwrap().0.remove(0);
    let mut credits = library.album_credits(&album.id).unwrap();
    for (name, role) in [("Guest", "arranger"), ("Album Producer", "producer")] {
        credits.credits.extend(
            crate::credits::source_credits(name, &[name.into()], role, "local_tags").credits,
        );
    }
    let album_only = Artist {
        id: artist_identity_id("Album Producer"),
        name: "Album Producer".into(),
        genres: Vec::new(),
        summary: None,
        logo_ref: None,
        banner_ref: None,
    };
    let db = library.db();
    let write = db.begin_write().unwrap();
    {
        write
            .open_table(ALBUM_CREDITS_TABLE)
            .unwrap()
            .insert(
                album.id.as_str(),
                encode_value(&credits).unwrap().as_slice(),
            )
            .unwrap();
        write
            .open_table(ARTISTS_TABLE)
            .unwrap()
            .insert(
                album_only.id.as_str(),
                encode_value(&album_only).unwrap().as_slice(),
            )
            .unwrap();
        write
            .open_table(ARTISTS_BY_NAME_TABLE)
            .unwrap()
            .insert(
                artist_name_key(&album_only.name, &album_only.id).as_str(),
                album_only.id.as_bytes(),
            )
            .unwrap();
        write
            .open_table(ARTIST_ALBUMS_TABLE)
            .unwrap()
            .insert(
                album_index_key(&album_only.id, &album).as_str(),
                album.id.as_bytes(),
            )
            .unwrap();
    }
    write.commit().unwrap();
    let guest = library
        .artist_release_relationships(&artist_identity_id("Guest"))
        .unwrap();
    assert!(guest.collab_albums.is_empty());
    assert_eq!(guest.appears_on[0].track_titles, ["Guest Song"]);
    assert_eq!(guest.appears_on[0].roles, ["featured_artist"]);
    assert_eq!(
        guest.other_credits[0].roles,
        ["arranger", "composer", "producer"]
    );
    let producer = library
        .artist_release_relationships(&album_only.id)
        .unwrap();
    assert!(producer.albums.is_empty());
    assert!(producer.collab_albums.is_empty());
    assert!(producer.other_credits[0].track_ids.is_empty());
    assert_eq!(producer.other_credits[0].roles, ["producer"]);
}

#[test]
fn artist_browse_separates_joint_albums_song_guests_and_explicit_roles() {
    let workspace = TestWorkspace::new("artist-collaborations");
    for file in ["Joint/01", "Solo/01", "Solo/02"] {
        workspace.write_audio(&format!("root/Artist A/{file}.mp3"));
    }
    let library = workspace.load_library(&[("", "root")]);
    for (folder, number, title, album_display, album_names, display, names, composer) in [
        (
            "Joint",
            "01",
            "Duet",
            "Artist A & Artist B",
            vec!["Artist A", "Artist B"],
            "Artist A & Artist B",
            vec!["Artist A", "Artist B"],
            false,
        ),
        (
            "Solo",
            "01",
            "Guest Song",
            "Artist A",
            vec!["Artist A"],
            "Artist A feat. Guest",
            vec!["Artist A", "Guest"],
            true,
        ),
        (
            "Solo",
            "02",
            "Solo Song",
            "Artist A",
            vec!["Artist A"],
            "Artist A",
            vec!["Artist A"],
            false,
        ),
    ] {
        seed_cached_tags(
            &library,
            &workspace.path(&format!("root/Artist A/{folder}/{number}.mp3")),
            metadata::TagInfo {
                album: Some(folder.into()),
                title: Some(title.into()),
                album_artist: Some(album_display.into()),
                album_artists: album_names.into_iter().map(str::to_owned).collect(),
                artist: Some(display.into()),
                artists: names.into_iter().map(str::to_owned).collect(),
                track_no: number.parse().ok(),
                contributions: if composer {
                    vec![metadata::TagContribution {
                        name: "Composer".into(),
                        role: "composer".into(),
                    }]
                } else {
                    Vec::new()
                },
                ..Default::default()
            },
            b"cached collaboration fixture",
        );
    }
    library.incremental_scan().unwrap();
    let artist_a = artist_identity_id("Artist A");
    let artist_b = artist_identity_id("Artist B");
    let guest = artist_identity_id("Guest");
    let composer = artist_identity_id("Composer");
    let (browse, total) = library.list_artists_for_browse(None, 20, 0, false).unwrap();
    assert_eq!(total, 3);
    assert_eq!(
        browse
            .iter()
            .map(|artist| artist.name.as_str())
            .collect::<Vec<_>>(),
        ["Artist A", "Artist B", "Guest"]
    );
    assert_eq!(
        library
            .list_artists_for_browse(None, 20, 0, true)
            .unwrap()
            .1,
        4
    );
    assert_eq!(
        library
            .list_artists_for_browse(Some("guest"), 20, 0, false)
            .unwrap()
            .0[0]
            .id,
        guest
    );
    assert_eq!(
        library
            .list_artists_for_browse(None, 1, 1, false)
            .unwrap()
            .0[0]
            .id,
        artist_b
    );
    let joint = library.artist_release_relationships(&artist_b).unwrap();
    assert!(joint.albums.is_empty());
    assert_eq!(joint.collab_albums.len(), 1);
    assert_eq!(joint.collab_albums[0].album.title, "Joint");
    assert!(joint.appears_on.is_empty());
    let credits = library
        .album_credits(&joint.collab_albums[0].album.id)
        .unwrap();
    assert_eq!(credits.display_name, "Artist A & Artist B");
    assert_eq!(credits.credits[0].join_phrase, " & ");
    let appearance = library.artist_release_relationships(&guest).unwrap();
    assert!(appearance.albums.is_empty());
    assert_eq!(appearance.appears_on.len(), 1);
    assert_eq!(appearance.appears_on[0].track_titles, ["Guest Song"]);
    assert_eq!(appearance.appears_on[0].roles, ["featured_artist"]);
    assert!(appearance.collab_albums.is_empty());
    let role = library.artist_release_relationships(&composer).unwrap();
    assert!(role.albums.is_empty());
    assert!(role.appears_on.is_empty());
    assert_eq!(role.other_credits[0].track_titles, ["Guest Song"]);
    assert_eq!(role.other_credits[0].roles, ["composer"]);
    assert!(role.collab_albums.is_empty());
    assert!(!library.billed_artist_ids().unwrap().contains(&composer));
    assert_eq!(
        library
            .list_artists_for_browse(Some("composer"), 20, 0, false)
            .unwrap()
            .1,
        0
    );
    assert_eq!(
        library
            .list_artists_for_browse(None, 1, 2, false)
            .unwrap()
            .0[0]
            .id,
        guest
    );
    let counts = library
        .headline_album_counts(&[artist_a.clone(), artist_b.clone(), guest.clone(), composer])
        .unwrap();
    assert_eq!(counts[&artist_a], 2);
    assert_eq!(counts[&artist_b], 1);
    assert_eq!(counts[&guest], 0);
    let guest_track = &appearance.appears_on[0].track_ids[0];
    assert_eq!(library.list_tracks(Some("Guest"), 20, 0).unwrap().1, 1);
    assert_eq!(library.list_tracks(Some("Composer"), 20, 0).unwrap().1, 0);
    assert_eq!(
        library.track_artist_ids(guest_track).unwrap(),
        [artist_a.clone(), guest.clone()]
    );
    let primary = library.get_artist(&artist_a).unwrap().unwrap();
    let guest_artist = library.get_artist(&guest).unwrap().unwrap();
    library
        .merge_album_artists(
            &appearance.appears_on[0].album.id,
            &[primary.clone(), guest_artist],
        )
        .unwrap();
    assert_eq!(
        library
            .get_album(&appearance.appears_on[0].album.id)
            .unwrap()
            .unwrap()
            .all_artist_ids(),
        &[artist_a.clone()]
    );
    library
        .merge_album_artists(&joint.collab_albums[0].album.id, &[primary])
        .unwrap();
    assert_eq!(
        library
            .get_album(&joint.collab_albums[0].album.id)
            .unwrap()
            .unwrap()
            .all_artist_ids(),
        &[artist_a, artist_b]
    );
    assert!(library
        .get_artist(&artist_identity_id("Artist A & Artist B"))
        .unwrap()
        .is_none());
    workspace.write_audio("root/Unrelated/New/Song.mp3");
    library.incremental_scan().unwrap();
    assert_eq!(
        library
            .artist_release_relationships(&guest)
            .unwrap()
            .appears_on[0]
            .track_ids,
        appearance.appears_on[0].track_ids
    );
    assert_eq!(
        library
            .artist_release_relationships(&artist_identity_id("Composer"))
            .unwrap()
            .other_credits[0]
            .roles,
        ["composer"]
    );
}

#[test]
fn songs_with_the_same_primary_artist_and_different_guests_are_distinct() {
    let workspace = TestWorkspace::new("guest-track-identity");
    for root in ["root-a", "root-b"] {
        workspace.write_audio(&format!("{root}/Artist/Album/Song.mp3"));
    }
    let library = workspace.load_library(&[("", "root-a"), ("b", "root-b")]);
    for (root, guest) in [("root-a", "Guest One"), ("root-b", "Guest Two")] {
        seed_cached_tags(
            &library,
            &workspace.path(&format!("{root}/Artist/Album/Song.mp3")),
            metadata::TagInfo {
                album: Some("Album".into()),
                album_artist: Some("Artist".into()),
                title: Some("Song".into()),
                artist: Some(format!("Artist feat. {guest}")),
                artists: vec!["Artist".into(), guest.into()],
                track_no: Some(1),
                duration_ms: Some(180_000),
                ..Default::default()
            },
            b"cached guest version fixture",
        );
    }
    library.incremental_scan().unwrap();
    assert_eq!(library.stats().unwrap().albums, 1);
    assert_eq!(library.stats().unwrap().tracks, 2);
    for guest in ["Guest One", "Guest Two"] {
        assert_eq!(
            library
                .artist_release_relationships(&artist_identity_id(guest))
                .unwrap()
                .appears_on[0]
                .track_ids
                .len(),
            1
        );
    }
}

fn tagged_flac(comments: &[&str]) -> Vec<u8> {
    let mut bytes = b"fLaC".to_vec();
    bytes.extend_from_slice(&[0, 0, 0, 34]);
    let mut stream_info = [0u8; 34];
    stream_info[..4].copy_from_slice(&[0x10, 0, 0x10, 0]);
    let properties = (44_100u64 << 44) | (1u64 << 41) | (15u64 << 36) | 44_100;
    stream_info[10..18].copy_from_slice(&properties.to_be_bytes());
    bytes.extend_from_slice(&stream_info);
    let vendor = b"Phonolite test";
    let mut vorbis = (vendor.len() as u32).to_le_bytes().to_vec();
    vorbis.extend_from_slice(vendor);
    vorbis.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for comment in comments {
        vorbis.extend_from_slice(&(comment.len() as u32).to_le_bytes());
        vorbis.extend_from_slice(comment.as_bytes());
    }
    let length = (vorbis.len() as u32).to_be_bytes();
    bytes.extend_from_slice(&[0x84, length[1], length[2], length[3]]);
    bytes.extend_from_slice(&vorbis);
    bytes
}

#[test]
fn policy_upgrade_and_force_scan_recover_structured_credits_from_real_flac_tags() {
    use redb::{ReadableTable, TableDefinition};
    let workspace = TestWorkspace::new("structured-credit-upgrade");
    let path = workspace.path("root/Folders/Album/Song.flac");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        tagged_flac(&[
            "ALBUM=Joint",
            "ALBUMARTIST=Artist A & Artist B",
            "ALBUMARTISTS=Artist A",
            "ALBUMARTISTS=Artist B",
            "TITLE=Song",
            "ARTIST=Artist A feat. Guest",
            "ARTISTS=Artist A",
            "ARTISTS=Guest",
            "PERFORMER=Soloist (violin)",
        ]),
    )
    .unwrap();
    let library = workspace.load_library(&[("", "root")]);
    let album = library.list_albums(None, 1, 0).unwrap().0.remove(0);
    let before = library.get_album_tracks(&album.id).unwrap().remove(0);
    assert_eq!(library.stats().unwrap().artists, 4);
    let db = library.db();
    let write = db.begin_write().unwrap();
    {
        write
            .open_table(crate::storage::META_TABLE)
            .unwrap()
            .insert("external_metadata_identity_policy", &[2u8][..])
            .unwrap();
        write
            .delete_table(crate::storage::ALBUM_CREDITS_TABLE)
            .unwrap();
        write
            .delete_table(crate::storage::TRACK_CREDITS_TABLE)
            .unwrap();
        let fingerprint = fs::metadata(&path).unwrap();
        let wrong_tags = metadata::TagInfo {
            artist: Some("Wrong Flattened Artist".into()),
            ..Default::default()
        };
        let value =
            bincode::serialize(&(fingerprint.len(), fingerprint.modified().ok(), wrong_tags))
                .unwrap();
        write
            .open_table(TableDefinition::<&str, &[u8]>::new("file_tags_v2"))
            .unwrap()
            .insert(path.to_string_lossy().as_ref(), value.as_slice())
            .unwrap();
        // A policy upgrade reparses even an unchanged file and discards any
        // cache produced before independent credits were available.
        write
            .delete_table(TableDefinition::<&str, &[u8]>::new("file_tags_v4"))
            .unwrap();
    }
    write.commit().unwrap();
    let (recovered, scanned) = Library::load_or_scan_with_db(
        vec![LibraryRoot::new("".into(), workspace.path("root"))],
        db,
    )
    .unwrap();
    assert!(scanned);
    assert_eq!(recovered.album_credits(&album.id).unwrap().credits.len(), 2);
    assert_eq!(
        recovered.track_artist_ids(&before.id).unwrap(),
        [artist_identity_id("Artist A"), artist_identity_id("Guest")]
    );
    assert_eq!(
        recovered
            .artist_release_relationships(&artist_identity_id("Soloist"))
            .unwrap()
            .other_credits[0]
            .roles,
        ["violin"]
    );
    assert!(recovered
        .db()
        .begin_read()
        .unwrap()
        .open_table(TableDefinition::<&str, &[u8]>::new("file_tags_v2"))
        .is_err());
    recovered.rescan().unwrap();
    assert_eq!(
        recovered.get_album_tracks(&album.id).unwrap()[0].id,
        before.id
    );
    assert_eq!(
        recovered
            .artist_release_relationships(&artist_identity_id("Guest"))
            .unwrap()
            .appears_on[0]
            .track_ids,
        [before.id]
    );
    assert_eq!(
        recovered
            .list_artists_for_browse(None, 20, 0, false)
            .unwrap()
            .1,
        3
    );
    let read = recovered.db();
    let read = read.begin_read().unwrap();
    assert_eq!(
        read.open_table(crate::storage::ALBUM_CREDITS_TABLE)
            .unwrap()
            .len()
            .unwrap(),
        1
    );
}

fn prince_provider_credits() -> [ArtistCredit; 2] {
    [
        (
            "070d193a-845c-479f-980e-bef15710653e",
            "Prince",
            "Prince",
            " and ",
        ),
        (
            "4c8ead39-b9df-4c56-a27c-51bc049cfd48",
            "The Revolution",
            "the Revolution",
            "",
        ),
    ]
    .map(|(id, name, credited_name, join_phrase)| ArtistCredit {
        artist_id: id.into(),
        name: name.into(),
        credited_name: credited_name.into(),
        role: "album_artist".into(),
        join_phrase: join_phrase.into(),
        source: "musicbrainz".into(),
    })
}

#[test]
fn verified_display_resolution_rebuilds_browse_and_persists_without_changing_media_ids() {
    use redb::ReadableTable;

    let workspace = TestWorkspace::new("verified-display-credit");
    for (file, title) in [("One", "First"), ("Two", "Second")] {
        let path = workspace.path(&format!("root/Files/Purple Rain/{file}.flac"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            tagged_flac(&[
                "ALBUM=Purple Rain",
                "ALBUMARTIST=Prince, Prince and the Revolution",
                "ARTIST=Prince, Prince and the Revolution",
                &format!("TITLE={title}"),
            ]),
        )
        .unwrap();
    }
    let library = workspace.load_library(&[("", "root")]);
    let album = library.list_albums(None, 10, 0).unwrap().0.remove(0);
    let combined_id = artist_identity_id("Prince, Prince and the Revolution");
    assert_eq!(album.artist_id, combined_id);
    let original_track_ids: HashSet<_> = library
        .get_album_tracks(&album.id)
        .unwrap()
        .into_iter()
        .map(|track| track.id)
        .collect();
    let original_display = library.raw_album_credits(&album.id).unwrap().display_name;
    assert_eq!(original_display, "Prince, Prince and the Revolution");
    library
        .update_album_enrichment(&album.id, Some("Keep this description".into()), &[], false)
        .unwrap();

    assert!(library
        .resolve_album_display_credit(&album.id, &prince_provider_credits())
        .unwrap());
    let resolved = library.get_album(&album.id).unwrap().unwrap();
    assert_eq!(resolved.artist_names, ["Prince", "The Revolution"]);
    assert_eq!(resolved.summary.as_deref(), Some("Keep this description"));
    assert!(library.get_artist(&combined_id).unwrap().is_none());
    assert_eq!(
        library
            .list_artists_for_browse(None, 20, 0, false)
            .unwrap()
            .1,
        2
    );
    assert_eq!(
        library.album_credits(&album.id).unwrap().display_name,
        "Prince and the Revolution"
    );
    assert_eq!(
        library.raw_album_credits(&album.id).unwrap().display_name,
        original_display,
        "verified presentation repair must not change source fingerprint wording"
    );
    // The presentation correction never overwrites the original source or its
    // verification key, including existing persisted resolutions from before
    // the display cleanup was introduced.
    let read = library.db.begin_read().unwrap();
    let stored: common::ArtistCredits = crate::storage::decode_value(
        read.open_table(crate::storage::ALBUM_CREDITS_TABLE)
            .unwrap()
            .get(album.id.as_str())
            .unwrap()
            .unwrap()
            .value(),
    )
    .unwrap();
    assert_eq!(stored.display_name, "Prince, Prince and the Revolution");
    drop(read);
    for name in ["Prince", "The Revolution"] {
        let relationships = library
            .artist_release_relationships(&artist_identity_id(name))
            .unwrap();
        assert_eq!(relationships.collab_albums[0].album.id, album.id);
        assert_eq!(relationships.collab_albums[0].track_ids.len(), 2);
    }
    for track in library.get_album_tracks(&album.id).unwrap() {
        assert!(original_track_ids.contains(&track.id));
        let credits = library.track_credits(&track.id).unwrap();
        assert_eq!(credits.display_name, "Prince and the Revolution");
        assert_eq!(
            library.raw_track_credits(&track.id).unwrap().display_name,
            original_display
        );
        assert_eq!(credits.credits[0].join_phrase, " and ");
        assert_eq!(credits.credits[1].credited_name, "the Revolution");
        assert_eq!(credits.credits[1].role, "track_artist");
        assert_eq!(
            library.track_artist_ids(&track.id).unwrap(),
            [
                artist_identity_id("Prince"),
                artist_identity_id("The Revolution")
            ]
        );
    }
    assert!(!library
        .resolve_album_display_credit(&album.id, &prince_provider_credits())
        .unwrap());

    // Reusing the same spelling in an explicit identity tag still names one
    // artist. A cached resolution only applies to unstructured display tags.
    let explicit = workspace.path("root/Files/Explicit/Song.flac");
    fs::create_dir_all(explicit.parent().unwrap()).unwrap();
    fs::write(
        explicit,
        tagged_flac(&[
            "ALBUM=Explicit",
            "ALBUMARTIST=Prince, Prince and the Revolution",
            "ALBUMARTISTS=Prince, Prince and the Revolution",
            "TITLE=Song",
            "ARTIST=Prince, Prince and the Revolution",
            "ARTISTS=Prince, Prince and the Revolution",
        ]),
    )
    .unwrap();
    library.incremental_scan().unwrap();
    let explicit_album = library
        .list_albums(Some("Explicit"), 10, 0)
        .unwrap()
        .0
        .remove(0);
    assert_eq!(explicit_album.artist_id, combined_id);
    assert_eq!(explicit_album.all_artist_ids().len(), 1);
    assert!(!library
        .resolve_album_display_credit(&explicit_album.id, &prince_provider_credits())
        .unwrap());

    let db = library.db();
    let (reloaded, scanned) = Library::load_or_scan_with_db(
        vec![LibraryRoot::new("".into(), workspace.path("root"))],
        db,
    )
    .unwrap();
    assert!(!scanned);
    assert_eq!(
        reloaded.album_credits(&album.id).unwrap().display_name,
        "Prince and the Revolution"
    );
    assert_eq!(
        reloaded.raw_album_credits(&album.id).unwrap().display_name,
        original_display
    );
    for _ in 0..2 {
        reloaded.rescan().unwrap();
        assert_eq!(
            reloaded.album_credits(&album.id).unwrap().display_name,
            "Prince and the Revolution"
        );
        assert_eq!(
            reloaded.get_album(&album.id).unwrap().unwrap().artist_names,
            ["Prince", "The Revolution"]
        );
        assert_eq!(
            reloaded
                .get_album_tracks(&album.id)
                .unwrap()
                .into_iter()
                .map(|track| track.id)
                .collect::<HashSet<_>>(),
            original_track_ids
        );
        assert_eq!(
            reloaded
                .get_album(&explicit_album.id)
                .unwrap()
                .unwrap()
                .all_artist_ids()
                .len(),
            1
        );
    }

    // A different raw display does not inherit this provider resolution.
    let path = workspace.path("root/Files/Purple Rain/One.flac");
    fs::write(
        &path,
        tagged_flac(&[
            "ALBUM=Purple Rain",
            "ALBUMARTIST=Prince & Unknown Band",
            "ARTIST=Prince & Unknown Band",
            "TITLE=First",
        ]),
    )
    .unwrap();
    reloaded.incremental_scan_paths(&[path]).unwrap();
    assert_eq!(
        reloaded.get_album(&album.id).unwrap().unwrap().artist_names,
        ["Prince & Unknown Band"]
    );
}

#[test]
fn explicit_local_credits_clean_redundant_display_without_remote_metadata() {
    let workspace = TestWorkspace::new("local-credit-display");
    let path = workspace.path("root/Files/Expanded/Song.flac");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let bytes = tagged_flac(&[
        "ALBUM=Around The World In A Day (Deluxe Expanded Edition)",
        "ALBUMARTIST=Prince, Prince and the Revolution",
        "ALBUMARTISTS=Prince",
        "ALBUMARTISTS=The Revolution",
        "ARTIST=Prince, Prince and the Revolution",
        "ARTISTS=Prince",
        "ARTISTS=The Revolution",
        "TITLE=Song",
    ]);
    fs::write(&path, &bytes).unwrap();
    let library = workspace.load_library(&[("", "root")]);
    let album = library.list_albums(None, 10, 0).unwrap().0.remove(0);
    let track = library.get_album_tracks(&album.id).unwrap().remove(0);
    let artist_ids = [
        artist_identity_id("Prince"),
        artist_identity_id("The Revolution"),
    ];
    assert_eq!(album.all_artist_ids(), artist_ids);
    assert!(!library
        .resolve_album_display_credit(&album.id, &prince_provider_credits())
        .unwrap());
    for credits in [
        library.album_credits(&album.id).unwrap(),
        library.track_credits(&track.id).unwrap(),
    ] {
        assert_eq!(credits.display_name, "Prince and the Revolution");
        assert_eq!(credits.credits[0].join_phrase, " and ");
        assert_eq!(credits.credits[1].credited_name, "the Revolution");
        assert!(credits
            .credits
            .iter()
            .all(|credit| credit.source == "local_tags_structured"));
    }
    let (reloaded, scanned) = Library::load_or_scan_with_db(
        vec![LibraryRoot::new("".into(), workspace.path("root"))],
        library.db(),
    )
    .unwrap();
    assert!(!scanned);
    reloaded.rescan().unwrap();
    assert_eq!(
        reloaded
            .get_album(&album.id)
            .unwrap()
            .unwrap()
            .all_artist_ids(),
        artist_ids
    );
    assert_eq!(
        reloaded.get_album_tracks(&album.id).unwrap()[0].id,
        track.id
    );
    assert_eq!(
        reloaded.album_credits(&album.id).unwrap().display_name,
        "Prince and the Revolution"
    );
    assert_eq!(
        reloaded.track_credits(&track.id).unwrap().display_name,
        "Prince and the Revolution"
    );
    assert_eq!(fs::read(path).unwrap(), bytes);
}

#[test]
fn local_display_cleanup_preserves_complete_names_identity_order_and_unknown_credits() {
    for (title, display, names, expected) in [
        (
            "Complete Band",
            "Earth, Wind & Fire & Guest",
            vec!["Earth, Wind & Fire", "Guest"],
            "Earth, Wind & Fire & Guest",
        ),
        (
            "Authoritative Band",
            "Prince, Prince and the Revolution",
            vec!["Prince, Prince and the Revolution"],
            "Prince, Prince and the Revolution",
        ),
        (
            "Unknown",
            "Artist A & Unknown Band",
            vec!["Artist A", "Artist B"],
            "Artist A & Unknown Band",
        ),
        (
            "Repeated Tail",
            "Artist A, Artist B and Artist A",
            vec!["Artist A", "Artist B"],
            "Artist A, Artist B",
        ),
        (
            "Distinct Display Order",
            "Artist B and Artist A",
            vec!["Artist A", "Artist B"],
            "Artist B and Artist A",
        ),
        (
            "Interleaved",
            "Artist A, Artist B, Artist A and Artist C",
            vec!["Artist A", "Artist B", "Artist C"],
            "Artist A; Artist B; Artist C",
        ),
        (
            "Accented",
            "Björk, Björk and 東京",
            vec!["Björk", "東京"],
            "Björk and 東京",
        ),
    ] {
        let workspace = TestWorkspace::new("credit-display-boundaries");
        let path = workspace.path(&format!("root/Files/{title}/Song.flac"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut tags = vec![
            format!("ALBUM={title}"),
            format!("ALBUMARTIST={display}"),
            format!("ARTIST={display}"),
            "TITLE=Song".into(),
        ];
        tags.extend(names.iter().map(|name| format!("ALBUMARTISTS={name}")));
        tags.extend(names.iter().map(|name| format!("ARTISTS={name}")));
        fs::write(
            path,
            tagged_flac(&tags.iter().map(String::as_str).collect::<Vec<_>>()),
        )
        .unwrap();
        let library = workspace.load_library(&[("", "root")]);
        let album = library.list_albums(Some(title), 10, 0).unwrap().0.remove(0);
        assert_eq!(
            library.album_credits(&album.id).unwrap().display_name,
            expected,
            "{title}"
        );
        let track = library.get_album_tracks(&album.id).unwrap().remove(0);
        assert_eq!(
            library.track_credits(&track.id).unwrap().display_name,
            expected,
            "{title}"
        );
        assert_eq!(
            album.all_artist_ids(),
            names
                .iter()
                .map(|name| artist_identity_id(name))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn verified_display_resolution_cannot_import_unmentioned_artists_or_rename_a_solo_alias() {
    let workspace = TestWorkspace::new("unmentioned-display-credit");
    workspace.write_audio("root/Prince/Album/Song.mp3");
    let library = workspace.load_library(&[("", "root")]);
    let album = library.list_albums(None, 1, 0).unwrap().0.remove(0);
    assert!(!library
        .resolve_album_display_credit(&album.id, &prince_provider_credits())
        .unwrap());
    let credits = [ArtistCredit {
        artist_id: "prince".into(),
        name: "A Different Canonical Spelling".into(),
        credited_name: "Prince".into(),
        role: "album_artist".into(),
        join_phrase: String::new(),
        source: "musicbrainz".into(),
    }];
    assert!(!library
        .resolve_album_display_credit(&album.id, &credits)
        .unwrap());
    assert_eq!(
        library.get_album(&album.id).unwrap().unwrap().artist_names,
        ["Prince"]
    );
}

#[test]
fn pending_verified_resolution_is_applied_on_startup_and_can_restore_one_provider_group() {
    let workspace = TestWorkspace::new("pending-display-credit");
    let path = workspace.path("root/Files/Album/Song.flac");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        tagged_flac(&[
            "ALBUM=Album",
            "ALBUMARTIST=Earth, Wind, Fire",
            "ARTIST=Earth, Wind, Fire",
            "TITLE=Song",
        ]),
    )
    .unwrap();
    let library = workspace.load_library(&[("", "root")]);
    let album = library.list_albums(None, 1, 0).unwrap().0.remove(0);
    assert_eq!(album.all_artist_ids().len(), 3);
    let track = library.get_album_tracks(&album.id).unwrap().remove(0);
    let credits = [ArtistCredit {
        artist_id: "group-id".into(),
        name: "Earth, Wind & Fire".into(),
        credited_name: "Earth, Wind, Fire".into(),
        role: "album_artist".into(),
        join_phrase: String::new(),
        source: "musicbrainz".into(),
    }];
    let resolution = common::ArtistCredits {
        display_name: "Earth, Wind, Fire".into(),
        credits: crate::resolve_display_credit("Earth, Wind, Fire", &credits).unwrap(),
    };
    let db = library.db();
    let write = db.begin_write().unwrap();
    {
        write
            .open_table(crate::credits::DISPLAY_CREDIT_RESOLUTIONS)
            .unwrap()
            .insert(
                "earth, wind, fire",
                crate::storage::encode_value(&resolution)
                    .unwrap()
                    .as_slice(),
            )
            .unwrap();
        write
            .open_table(crate::storage::META_TABLE)
            .unwrap()
            .insert(
                crate::indexing::DISPLAY_CREDIT_RESOLUTIONS_DIRTY,
                &[1u8][..],
            )
            .unwrap();
    }
    write.commit().unwrap();
    let (recovered, scanned) = Library::load_or_scan_with_db(
        vec![LibraryRoot::new("".into(), workspace.path("root"))],
        db.clone(),
    )
    .unwrap();
    assert!(scanned);
    assert_eq!(
        recovered
            .get_album(&album.id)
            .unwrap()
            .unwrap()
            .artist_names,
        ["Earth, Wind & Fire"]
    );
    assert_eq!(
        recovered.get_album_tracks(&album.id).unwrap()[0].id,
        track.id
    );
    assert_eq!(recovered.stats().unwrap().artists, 1);
    assert_eq!(
        recovered.track_artist_ids(&track.id).unwrap(),
        [artist_identity_id("Earth, Wind & Fire")]
    );
    assert!(
        !Library::load_or_scan_with_db(
            vec![LibraryRoot::new("".into(), workspace.path("root"))],
            db,
        )
        .unwrap()
        .1
    );
}

#[test]
fn billing_policy_upgrade_repairs_old_album_links_and_preserves_metadata_and_media_ids() {
    use crate::storage::{encode_value, ALBUMS_TABLE, META_TABLE};
    let workspace = TestWorkspace::new("billed-artist-policy-upgrade");
    let path = workspace.path("root/Main/Album/Song.flac");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        tagged_flac(&[
            "ALBUM=Album",
            "ALBUMARTIST=Main",
            "ARTIST=Main feat. Guest",
            "TITLE=Song",
            "PRODUCER=Producer",
        ]),
    )
    .unwrap();
    let library = workspace.load_library(&[("", "root")]);
    let mut album = library.list_albums(None, 1, 0).unwrap().0.remove(0);
    let track = library.get_album_tracks(&album.id).unwrap().remove(0);
    library
        .update_album_enrichment(
            &album.id,
            Some("Album description".into()),
            &["Jazz".into()],
            true,
        )
        .unwrap();
    library
        .update_artist_enrichment(
            &album.artist_id,
            Some("Main biography".into()),
            &[],
            Some("main/logo.png".into()),
            None,
            true,
        )
        .unwrap();
    // Simulate an old catalog that promoted a song guest into album ownership.
    album = library.get_album(&album.id).unwrap().unwrap();
    album.artist_ids.push(artist_identity_id("Guest"));
    album.artist_names.push("Guest".into());
    let db = library.db();
    let write = db.begin_write().unwrap();
    write
        .open_table(ALBUMS_TABLE)
        .unwrap()
        .insert(album.id.as_str(), encode_value(&album).unwrap().as_slice())
        .unwrap();
    write
        .open_table(META_TABLE)
        .unwrap()
        .insert("artist_credit_identity_policy", &[1u8][..])
        .unwrap();
    write.commit().unwrap();
    let roots = vec![LibraryRoot::new("".into(), workspace.path("root"))];
    let (repaired, scanned) = Library::load_or_scan_with_db(roots.clone(), db.clone()).unwrap();
    assert!(scanned);
    let restored = repaired.get_album(&album.id).unwrap().unwrap();
    assert_eq!(restored.all_artist_ids(), &[album.artist_id.clone()]);
    assert_eq!(restored.summary.as_deref(), Some("Album description"));
    assert_eq!(restored.genres, ["Jazz"]);
    assert_eq!(
        repaired.get_album_tracks(&album.id).unwrap()[0].id,
        track.id
    );
    let main = repaired.get_artist(&album.artist_id).unwrap().unwrap();
    assert_eq!(main.summary.as_deref(), Some("Main biography"));
    assert_eq!(main.logo_ref.as_deref(), Some("main/logo.png"));
    assert_eq!(
        repaired
            .list_artists_for_browse(None, 1, 1, false)
            .unwrap()
            .1,
        2
    );
    assert_eq!(
        repaired
            .list_artists_for_browse(None, 20, 0, true)
            .unwrap()
            .1,
        3
    );
    assert_eq!(
        repaired
            .track_credits(&track.id)
            .unwrap()
            .credits
            .iter()
            .filter(|credit| credit.role == "producer")
            .count(),
        1
    );
    assert!(!Library::load_or_scan_with_db(roots, db).unwrap().1);
}

#[test]
fn a_featured_guest_does_not_become_an_album_artist_when_album_artist_is_missing() {
    let workspace = TestWorkspace::new("missing-albumartist-feature");
    workspace.write_audio("root/Artist/Single/Song.mp3");
    let library = workspace.load_library(&[("", "root")]);
    seed_cached_tags(
        &library,
        &workspace.path("root/Artist/Single/Song.mp3"),
        metadata::TagInfo {
            artist: Some("Artist feat. Guest".into()),
            artists: vec!["Artist".into(), "Guest".into()],
            ..Default::default()
        },
        b"cached one song collaboration fixture",
    );
    library.incremental_scan().unwrap();
    assert_eq!(
        library
            .list_artists_for_browse(None, 20, 0, false)
            .unwrap()
            .1,
        2
    );
    let guest = library
        .artist_release_relationships(&artist_identity_id("Guest"))
        .unwrap();
    assert!(guest.albums.is_empty());
    assert_eq!(guest.appears_on[0].track_titles, ["Song"]);
}

#[test]
fn copied_joint_album_preserves_preferred_display_and_primary_across_reordered_credits() {
    let workspace = TestWorkspace::new("copied-joint-album");
    for root in ["root-a", "root-b"] {
        workspace.write_audio(&format!("{root}/A/Joint/Song.mp3"));
    }
    let library = workspace.load_library(&[("", "root-a"), ("b", "root-b")]);
    for (root, display, names) in [
        ("root-a", "A & B", vec!["A", "B"]),
        ("root-b", "B with A", vec!["B", "A"]),
    ] {
        seed_cached_tags(
            &library,
            &workspace.path(&format!("{root}/A/Joint/Song.mp3")),
            metadata::TagInfo {
                album: Some("Joint".into()),
                album_artist: Some(display.into()),
                album_artists: names.iter().map(|name| (*name).into()).collect(),
                artist: Some(display.into()),
                artists: names.iter().map(|name| (*name).into()).collect(),
                title: Some("Song".into()),
                ..Default::default()
            },
            b"cached joint duplicate fixture",
        );
    }
    library.incremental_scan().unwrap();
    let (albums, total) = library.list_albums(None, 20, 0).unwrap();
    assert_eq!(total, 1);
    assert_eq!(library.stats().unwrap().tracks, 1);
    assert_eq!(albums[0].artist_id, artist_identity_id("A"));
    assert_eq!(
        library.album_credits(&albums[0].id).unwrap().display_name,
        "A & B"
    );
    assert_eq!(
        library
            .artist_release_relationships(&artist_identity_id("B"))
            .unwrap()
            .collab_albums
            .len(),
        1
    );
}

#[test]
fn compilation_tracks_keep_their_own_artists_during_credit_updates_and_rescans() {
    let workspace = TestWorkspace::new("compilation-track-artists");
    for file in ["01", "02"] {
        workspace.write_audio(&format!("root/Various Artists/Compilation/{file}.mp3"));
    }
    let library = workspace.load_library(&[("", "root")]);
    for (index, artist) in ["Performer One", "Performer Two"].into_iter().enumerate() {
        let tags = metadata::TagInfo {
            title: Some(format!("Song {}", index + 1)),
            album: Some("Compilation".into()),
            artist: Some(artist.into()),
            // The first track has no album-artist tag; a later declared
            // album artist must outrank its track artist for the album.
            album_artist: (index == 1).then(|| "Various Artists".into()),
            track_no: Some((index + 1) as u16),
            genres: vec![if index == 0 { "Classical" } else { "Rock" }.into()],
            ..Default::default()
        };
        seed_cached_tags(
            &library,
            &workspace.path(&format!(
                "root/Various Artists/Compilation/0{}.mp3",
                index + 1
            )),
            tags,
            b"cached tagged compilation fixture",
        );
    }
    library.incremental_scan().unwrap();
    let album = library.list_albums(None, 1, 0).unwrap().0.remove(0);
    assert_eq!(album.artist_names, ["Various Artists"]);
    let performer = library
        .get_artist(&artist_identity_id("Performer One"))
        .unwrap()
        .unwrap();
    let mut primary = library.get_artist(&album.artist_id).unwrap().unwrap();
    primary.name = "VARIOUS ARTISTS".into();
    library
        .merge_album_artists(&album.id, &[primary.clone(), performer.clone()])
        .unwrap();
    library.merge_album_artists(&album.id, &[primary]).unwrap();
    let tracks = library.get_album_tracks(&album.id).unwrap();
    assert_eq!(tracks[0].artist_id, performer.id);
    assert_eq!(tracks[1].artist_id, artist_identity_id("Performer Two"));
    assert_eq!(tracks[0].genres, ["Classical"]);
    assert_eq!(tracks[1].genres, ["Rock"]);
    assert_eq!(
        library.list_tracks(Some("performer one"), 10, 0).unwrap().1,
        1
    );
    assert_eq!(library.list_artist_albums(&performer.id).unwrap().len(), 1);
    assert_eq!(library.list_artists(None, 10, 0).unwrap().1, 3);
    workspace.write_audio("root/Other Artist/Other Album/Song.mp3");
    library.incremental_scan().unwrap();
    assert_eq!(
        library.get_album_tracks(&album.id).unwrap()[0].artist_id,
        performer.id
    );
    assert_eq!(library.list_artist_albums(&performer.id).unwrap().len(), 1);
}

#[test]
fn numbered_tracks_with_different_titles_durations_or_artists_are_not_duplicates() {
    let workspace = TestWorkspace::new("numbered-track-identity");
    for (root, file) in [
        ("root-a", "Original"),
        ("root-b", "Other Title"),
        ("root-c", "Longer"),
        ("root-d", "Other Performer"),
    ] {
        workspace.write_audio(&format!("{root}/Artist/Album/{file}.mp3"));
    }
    let library = workspace.load_library(&[
        ("", "root-a"),
        ("b", "root-b"),
        ("c", "root-c"),
        ("d", "root-d"),
    ]);
    for (root, file, title, duration, artist) in [
        ("root-a", "Original", "Song", 120_000, "Artist"),
        ("root-b", "Other Title", "Different Song", 120_000, "Artist"),
        ("root-c", "Longer", "Song", 180_000, "Artist"),
        ("root-d", "Other Performer", "Song", 120_000, "Guest Artist"),
    ] {
        seed_cached_tags(
            &library,
            &workspace.path(&format!("{root}/Artist/Album/{file}.mp3")),
            metadata::TagInfo {
                album: Some("Album".into()),
                album_artist: Some("Artist".into()),
                title: Some(title.into()),
                artist: Some(artist.into()),
                track_no: Some(1),
                duration_ms: Some(duration),
                ..Default::default()
            },
            b"cached numbered track identity fixture",
        );
    }
    library.incremental_scan().unwrap();
    assert_eq!(library.stats().unwrap().tracks, 4);
    let album = library.list_albums(None, 1, 0).unwrap().0.remove(0);
    assert_eq!(library.get_album_tracks(&album.id).unwrap().len(), 4);
}

#[test]
fn duplicate_album_year_updates_every_track_artist_browse_index() {
    let workspace = TestWorkspace::new("compilation-year-index");
    for root in ["root-a", "root-b"] {
        workspace.write_audio(&format!("{root}/Various Artists/Compilation/Song.mp3"));
    }
    let library = workspace.load_library(&[("", "root-a"), ("b", "root-b")]);
    for (root, year) in [("root-a", None), ("root-b", Some(2005))] {
        seed_cached_tags(
            &library,
            &workspace.path(&format!("{root}/Various Artists/Compilation/Song.mp3")),
            metadata::TagInfo {
                title: Some("Song".into()),
                album: Some("Compilation".into()),
                artist: Some("Performer".into()),
                album_artist: Some("Various Artists".into()),
                year,
                track_no: Some(1),
                ..Default::default()
            },
            b"cached compilation year fixture",
        );
    }
    library.incremental_scan().unwrap();
    let albums = library
        .list_artist_albums(&artist_identity_id("Performer"))
        .unwrap();
    assert_eq!(albums.len(), 1);
    assert_eq!(albums[0].year, Some(2005));
    assert_eq!(
        library
            .artist_album_counts(&[artist_identity_id("Performer")])
            .unwrap()[&artist_identity_id("Performer")],
        1
    );
}

#[test]
fn non_latin_artist_album_and_song_names_remain_distinct() {
    let workspace = TestWorkspace::new("unicode-catalog");
    for relpath in [
        "root/東京/東京/春.mp3",
        "root/東京/東京/夏.mp3",
        "root/東京/大阪/春.mp3",
        "root/大阪/東京/春.mp3",
    ] {
        workspace.write_audio(relpath);
    }
    let library = workspace.load_library(&[("", "root")]);
    assert_eq!(library.stats().unwrap().artists, 2);
    assert_eq!(library.stats().unwrap().albums, 3);
    assert_eq!(library.stats().unwrap().tracks, 4);
    assert_eq!(library.list_tracks(Some("夏"), 10, 0).unwrap().1, 1);
}

#[test]
fn artist_folder_sidecar_does_not_describe_a_different_tagged_artist() {
    let workspace = TestWorkspace::new("sidecar-artist-identity");
    workspace.write_audio("root/Folder Artist/Album/Song.mp3");
    fs::write(
        workspace.path("root/Folder Artist/artist.json"),
        br#"{"summary":"Folder artist biography","genres":["Jazz"]}"#,
    )
    .unwrap();
    let library = workspace.load_library(&[("", "root")]);
    seed_cached_tags(
        &library,
        &workspace.path("root/Folder Artist/Album/Song.mp3"),
        metadata::TagInfo {
            album_artist: Some("Tagged Artist".into()),
            artist: Some("Tagged Artist".into()),
            genres: vec!["Classical".into()],
            ..Default::default()
        },
        b"cached different tagged artist fixture",
    );
    library.incremental_scan().unwrap();
    let artist = library
        .get_artist(&artist_identity_id("Tagged Artist"))
        .unwrap()
        .unwrap();
    assert_eq!(artist.summary, None);
    assert_eq!(artist.genres, ["Classical"]);
}

#[test]
fn metadata_policy_upgrade_rereads_files_instead_of_reusing_old_tag_cache() {
    let workspace = TestWorkspace::new("tag-cache-upgrade");
    workspace.write_audio("root/Correct Artist/Album/Song.mp3");
    let library = workspace.load_library(&[("", "root")]);
    let path = workspace.path("root/Correct Artist/Album/Song.mp3");
    let fingerprint = fs::metadata(&path).unwrap();
    let old_tags = metadata::TagInfo {
        artist: Some("Wrong Cached Artist".into()),
        album: Some("Wrong Cached Album".into()),
        ..Default::default()
    };
    let bytes =
        bincode::serialize(&(fingerprint.len(), fingerprint.modified().ok(), old_tags)).unwrap();
    let db = library.db();
    let write = db.begin_write().unwrap();
    {
        write
            .open_table(redb::TableDefinition::<&str, &[u8]>::new("file_tags_v1"))
            .unwrap()
            .insert(path.to_string_lossy().as_ref(), bytes.as_slice())
            .unwrap();
        write
            .open_table(crate::storage::META_TABLE)
            .unwrap()
            .insert("external_metadata_identity_policy", &[1u8][..])
            .unwrap();
    }
    write.commit().unwrap();
    let (reloaded, scanned) = Library::load_or_scan_with_db(
        vec![LibraryRoot::new("".into(), workspace.path("root"))],
        db.clone(),
    )
    .unwrap();
    assert!(scanned);
    let album = reloaded.list_albums(None, 1, 0).unwrap().0.remove(0);
    assert_eq!(album.artist_names, ["Correct Artist"]);
    assert_eq!(album.title, "Album");
    assert!(matches!(
        db.begin_read()
            .unwrap()
            .open_table(redb::TableDefinition::<&str, &[u8]>::new("file_tags_v1")),
        Err(redb::TableError::TableDoesNotExist(_))
    ));
}

#[test]
fn cursor_pages_visit_each_track_once() {
    let workspace = TestWorkspace::new("cursor-pages");
    for name in ["One", "Two", "Three"] {
        workspace.write_audio(&format!("root/Artist/Album/{name}.mp3"));
    }
    let library = workspace.load_library(&[("", "root")]);
    let mut cursor = None;
    let mut ids = HashSet::new();
    for _ in 0..3 {
        let (page, total) = library.page_tracks(&mut cursor, 1).unwrap();
        assert_eq!(total, 3);
        assert_eq!(page.len(), 1);
        assert!(ids.insert(page[0].id.clone()));
    }
    assert!(library.page_tracks(&mut cursor, 1).unwrap().0.is_empty());
}

#[test]
fn targeted_directory_rename_reconciles_multidisc_album() {
    let workspace = TestWorkspace::new("targeted-rename");
    workspace.write_audio("root/Artist/Old/CD1/First.mp3");
    workspace.write_audio("root/Artist/Old/CD2/Second.mp3");
    let library = workspace.load_library(&[("", "root")]);
    let old = workspace.path("root/Artist/Old");
    let new = workspace.path("root/Artist/New");
    fs::rename(&old, &new).unwrap();
    let stats = library.incremental_scan_paths(&[old, new]).unwrap();
    assert_eq!(stats.albums, 1);
    assert_eq!(stats.tracks, 2);
    let (tracks, _) = library.list_tracks(None, 20, 0).unwrap();
    assert!(tracks
        .iter()
        .all(|track| track.file_relpath.contains("/New/")));
    assert!(tracks.iter().any(|track| track.disc_no == Some(1)));
    assert!(tracks.iter().any(|track| track.disc_no == Some(2)));
}

#[test]
fn conflicting_non_empty_album_years_do_not_merge() {
    let workspace = TestWorkspace::new("year-conflict");
    workspace.write_audio("root-a/Artist/Album (1999)/Song.mp3");
    workspace.write_audio("root-b/Artist/Album (2000)/Song.mp3");

    let library = workspace.load_library(&[("", "root-a"), ("root-b", "root-b")]);
    let (albums, total) = library.list_albums(None, 20, 0).unwrap();
    let mut years = albums
        .into_iter()
        .map(|album| album.year)
        .collect::<Vec<_>>();
    years.sort();

    assert_eq!(total, 2);
    assert_eq!(years, vec![Some(1999), Some(2000)]);
}

#[test]
fn artist_and_album_cursors_preserve_order_and_zero_limit_position() {
    let workspace = TestWorkspace::new("catalog-cursors");
    for name in ["Charlie", "Alpha", "Bravo"] {
        workspace.write_audio(&format!("root/{name}/{name}/Song.mp3"));
    }
    let library = workspace.load_library(&[("", "root")]);
    let mut artist_cursor = None;
    let mut album_cursor = None;
    assert!(library
        .page_artists(&mut artist_cursor, 0)
        .unwrap()
        .0
        .is_empty());
    assert!(library
        .page_albums(&mut album_cursor, 0)
        .unwrap()
        .0
        .is_empty());
    assert!(artist_cursor.is_none());
    assert!(album_cursor.is_none());

    for expected in ["Alpha", "Bravo", "Charlie"] {
        let (artists, total) = library.page_artists(&mut artist_cursor, 1).unwrap();
        assert_eq!(total, 3);
        assert_eq!(artists.len(), 1);
        assert_eq!(artists[0].name, expected);
        let (albums, total) = library.page_albums(&mut album_cursor, 1).unwrap();
        assert_eq!(total, 3);
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].title, expected);
    }
    let last_artist = artist_cursor.clone();
    let last_album = album_cursor.clone();
    assert!(library
        .page_artists(&mut artist_cursor, 1)
        .unwrap()
        .0
        .is_empty());
    assert!(library
        .page_albums(&mut album_cursor, 1)
        .unwrap()
        .0
        .is_empty());
    assert_eq!(artist_cursor, last_artist);
    assert_eq!(album_cursor, last_album);
}

#[test]
fn filtered_queries_count_matches_independently_of_page_size_and_offset() {
    let workspace = TestWorkspace::new("filtered-pages");
    workspace.write_audio("root/Artist/Alpha/One.mp3");
    workspace.write_audio("root/Artist/Alpha/Two.mp3");
    workspace.write_audio("root/Band/Beta/Three.mp3");
    let library = workspace.load_library(&[("", "root")]);

    let (artists, total) = library.list_artists(Some(" ARTIST "), 0, 0).unwrap();
    assert!(artists.is_empty());
    assert_eq!(total, 1);
    let (albums, total) = library.list_albums(Some(" AlPhA "), 1, 1).unwrap();
    assert!(albums.is_empty());
    assert_eq!(total, 1);
    let (tracks, total) = library.list_tracks(Some(" ALPHA "), 1, 1).unwrap();
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].title, "Two");
    assert_eq!(total, 2);
    let (tracks, total) = library.list_tracks(Some("   "), 1, 99).unwrap();
    assert!(tracks.is_empty());
    assert_eq!(total, 3);
    assert_eq!(library.list_artists(None, 20, 0).unwrap().1, 2);
    assert_eq!(library.list_albums(None, 20, 0).unwrap().1, 2);
}

#[test]
fn album_enrichment_distinguishes_merge_replace_and_empty_updates() {
    let workspace = TestWorkspace::new("album-enrichment");
    workspace.write_audio("root/Artist/Album/Song.mp3");
    let library = workspace.load_library(&[("", "root")]);
    let album_id = library.list_albums(None, 1, 0).unwrap().0[0].id.clone();
    assert!(library
        .update_album_enrichment(
            &album_id,
            Some("  Original  ".into()),
            &["Rock".into()],
            false
        )
        .unwrap());
    assert!(library
        .update_album_enrichment(
            &album_id,
            Some("Ignored".into()),
            &["rock".into(), "Blues".into()],
            false
        )
        .unwrap());
    let album = library.get_album(&album_id).unwrap().unwrap();
    assert_eq!(album.summary.as_deref(), Some("Original"));
    assert_eq!(album.genres, ["Rock", "Blues"]);
    assert!(!library
        .update_album_enrichment(&album_id, Some("Ignored".into()), &["Rock".into()], false)
        .unwrap());
    assert!(library
        .update_album_enrichment(
            &album_id,
            Some(" Replacement ".into()),
            &["Jazz".into()],
            true
        )
        .unwrap());
    assert!(!library
        .update_album_enrichment(&album_id, Some("  ".into()), &[], true)
        .unwrap());
    let album = library.get_album(&album_id).unwrap().unwrap();
    assert_eq!(album.summary.as_deref(), Some("Replacement"));
    assert_eq!(album.genres, ["Jazz"]);
    assert!(!library
        .update_album_enrichment("missing", Some("unused".into()), &[], true)
        .unwrap());
}

#[test]
fn artist_enrichment_preserves_summary_and_updates_artwork_without_replace() {
    let workspace = TestWorkspace::new("artist-enrichment");
    workspace.write_audio("root/Artist/Album/Song.mp3");
    let library = workspace.load_library(&[("", "root")]);
    let artist_id = library.list_artists(None, 1, 0).unwrap().0[0].id.clone();
    assert!(library
        .update_artist_enrichment(
            &artist_id,
            Some(" Original ".into()),
            &["Rock".into()],
            Some("old-logo".into()),
            None,
            false
        )
        .unwrap());
    assert!(library
        .update_artist_enrichment(
            &artist_id,
            Some("Ignored".into()),
            &["rock".into()],
            Some("new-logo".into()),
            Some("banner".into()),
            false
        )
        .unwrap());
    let artist = library.get_artist(&artist_id).unwrap().unwrap();
    assert_eq!(artist.summary.as_deref(), Some("Original"));
    assert_eq!(artist.genres, ["Rock"]);
    assert_eq!(artist.logo_ref.as_deref(), Some("new-logo"));
    assert_eq!(artist.banner_ref.as_deref(), Some("banner"));
    assert!(!library
        .update_artist_enrichment(&artist_id, None, &[], None, None, true)
        .unwrap());
    assert!(library
        .update_artist_enrichment(
            &artist_id,
            Some(" Replacement ".into()),
            &["Jazz".into()],
            None,
            None,
            true
        )
        .unwrap());
    let artist = library.get_artist(&artist_id).unwrap().unwrap();
    assert_eq!(artist.summary.as_deref(), Some("Replacement"));
    assert_eq!(artist.genres, ["Jazz"]);
}

#[test]
fn external_attempts_throttle_successes_and_failures_per_key() {
    let workspace = TestWorkspace::new("external-attempts");
    workspace.write_audio("root/Artist/Album/Song.mp3");
    let library = workspace.load_library(&[("", "root")]);
    let interval = Duration::from_secs(86_400);
    assert!(library
        .should_attempt_external("artist:one", interval)
        .unwrap());
    library
        .record_external_attempt("artist:one", false)
        .unwrap();
    assert!(!library
        .should_attempt_external("artist:one", interval)
        .unwrap());
    assert!(library
        .should_attempt_external("artist:one", Duration::ZERO)
        .unwrap());
    assert!(library
        .should_attempt_external("artist:two", interval)
        .unwrap());
    library.record_external_attempt("artist:one", true).unwrap();
    assert!(!library
        .should_attempt_external("artist:one", interval)
        .unwrap());
}

#[test]
fn rooted_paths_resolve_named_roots_and_preserve_legacy_default_fallback() {
    let workspace = TestWorkspace::new("root-paths");
    workspace.write_audio("primary/Artist/Album/Song.mp3");
    workspace.write_audio("secondary/Artist/Album/Song.mp3");
    let library = workspace.load_library(&[("second", "secondary"), ("", "primary")]);
    assert_eq!(library.root(), workspace.path("primary"));
    assert_eq!(
        library.resolve_relpath("second::Artist/Album/Song.mp3"),
        Some(workspace.path("secondary/Artist/Album/Song.mp3"))
    );
    assert_eq!(
        library.resolve_relpath("Artist/Album/Song.mp3"),
        Some(workspace.path("primary/Artist/Album/Song.mp3"))
    );
    // Unknown prefixes are kept verbatim for backwards compatibility.
    assert_eq!(
        library.resolve_relpath("unknown::Song.mp3"),
        Some(workspace.path("primary/unknown::Song.mp3"))
    );
}

struct TestWorkspace {
    root: PathBuf,
}

impl TestWorkspace {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "phonolite-library-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn write_audio(&self, relpath: &str) {
        let path = self.path(relpath);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"not really audio").unwrap();
    }

    fn load_library(&self, roots: &[(&str, &str)]) -> Library {
        let roots = roots
            .iter()
            .map(|(id, relpath)| LibraryRoot::new((*id).to_string(), self.path(relpath)))
            .collect::<Vec<_>>();
        let db_path = self.path("library.redb");
        Library::load_or_scan(roots, db_path).unwrap().0
    }

    fn path(&self, relpath: &str) -> PathBuf {
        let mut path = self.root.clone();
        for part in relpath.split('/') {
            path.push(part);
        }
        path
    }
}

impl Drop for TestWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
