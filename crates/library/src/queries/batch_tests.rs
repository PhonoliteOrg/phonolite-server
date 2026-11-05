use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use common::{Codec, Track};
use redb::{Database, TableDefinition};

use crate::storage::{encode_value, TRACKS_TABLE};
use crate::Library;

fn library() -> Library {
    Library {
        roots: Vec::new(),
        db: Arc::new(
            Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        ),
        cache_generation: Arc::new(AtomicU64::new(crate::next_catalog_generation())),
        metadata_generation: Arc::new(AtomicU64::new(crate::next_catalog_generation())),
        browse_projection: Arc::new(Default::default()),
        search_state: Arc::new(Default::default()),
        cancellation: Arc::new(AtomicBool::new(false)),
        tag_reader: None,
        cancel_reads: false,
    }
}

fn track(id: &str) -> Track {
    Track {
        id: id.into(),
        album_id: "album".into(),
        artist_id: "artist".into(),
        title: format!("Title {id}"),
        track_no: Some(1),
        disc_no: None,
        duration_ms: 123_456,
        codec: Codec::Flac,
        sample_rate: Some(44_100),
        channels: Some(2),
        bitrate: None,
        file_relpath: format!("Artist/Album/{id}.flac"),
        file_size: 456_789,
        genres: vec!["Jazz".into()],
    }
}

fn store(library: &Library, tracks: &[Track]) {
    let write = library.db.begin_write().unwrap();
    {
        let mut table = write.open_table(TRACKS_TABLE).unwrap();
        for track in tracks {
            table
                .insert(track.id.as_str(), encode_value(track).unwrap().as_slice())
                .unwrap();
        }
    }
    write.commit().unwrap();
}

fn previous_lookup(library: &Library, ids: &[String]) -> Vec<Track> {
    ids.iter()
        .filter_map(|id| library.get_track(id).ok().flatten())
        .collect()
}

fn assert_parity(library: &Library, ids: &[String]) {
    assert_eq!(
        serde_json::to_value(library.tracks_by_ids_best_effort(ids)).unwrap(),
        serde_json::to_value(previous_lookup(library, ids)).unwrap(),
    );
}

#[test]
fn collection_batch_preserves_order_duplicates_and_missing_track_handling() {
    let library = library();
    store(&library, &[track("first"), track("second"), track("third")]);
    let ids: Vec<String> = ["third", "missing", "first", "third", "second", "missing"]
        .into_iter()
        .map(String::from)
        .collect();

    assert_parity(&library, &ids);
    assert_eq!(
        library
            .tracks_by_ids_best_effort(&ids)
            .iter()
            .map(|track| track.id.as_str())
            .collect::<Vec<_>>(),
        ["third", "first", "third", "second"]
    );
}

#[test]
fn collection_batch_skips_corrupt_rows_without_changing_strict_queue_reads() {
    let library = library();
    store(&library, &[track("first"), track("last")]);
    let write = library.db.begin_write().unwrap();
    {
        let mut table = write.open_table(TRACKS_TABLE).unwrap();
        table.insert("corrupt", &[0xff, 0x42][..]).unwrap();
        table
            .insert(
                "alias",
                encode_value(&track("payload-id")).unwrap().as_slice(),
            )
            .unwrap();
    }
    write.commit().unwrap();
    let ids: Vec<String> = ["first", "corrupt", "absent", "alias", "last", "corrupt"]
        .into_iter()
        .map(String::from)
        .collect();

    assert_parity(&library, &ids);
    assert_eq!(library.tracks_by_ids_best_effort(&ids).len(), 3);
    assert!(library.tracks_by_ids(&ids).is_err());
}

#[test]
fn collection_batch_retains_empty_results_for_unavailable_track_tables() {
    let library = library();
    assert_parity(&library, &[]);
    assert_parity(&library, &["missing-table".into()]);

    // Opening a table with the wrong persisted type is also a failed lookup,
    // which the previous collection routes silently omitted for every ID.
    let write = library.db.begin_write().unwrap();
    write
        .open_table(TableDefinition::<&str, u64>::new("tracks"))
        .unwrap()
        .insert("wrong-type", 1)
        .unwrap();
    write.commit().unwrap();
    assert_parity(&library, &["wrong-type".into(), "wrong-type".into()]);
}

#[test]
fn collection_batch_reads_fresh_data_on_each_request() {
    let library = library();
    let ids = vec!["track".into()];
    store(&library, &[track("track")]);
    assert_eq!(
        library.tracks_by_ids_best_effort(&ids)[0].title,
        "Title track"
    );

    let mut updated = track("track");
    updated.title = "Updated title".into();
    store(&library, &[updated]);
    assert_parity(&library, &ids);
    assert_eq!(
        library.tracks_by_ids_best_effort(&ids)[0].title,
        "Updated title"
    );
}

#[test]
fn collection_batch_matches_get_track_cancellation_behavior() {
    let library = library().with_cancellable_reads();
    store(&library, &[track("track")]);
    library.cancellation.store(true, Ordering::Release);
    let ids = vec!["track".into()];
    assert_parity(&library, &ids);
    assert_eq!(library.tracks_by_ids_best_effort(&ids).len(), 1);
    assert!(library.tracks_by_ids(&ids).is_err());
}

#[test]
fn collection_batch_matches_previous_lookup_for_large_mixed_collections() {
    let library = library();
    let tracks: Vec<_> = (0..128)
        .map(|index| track(&format!("track-{index}")))
        .collect();
    store(&library, &tracks);
    let ids: Vec<_> = (0..1_024)
        .map(|index| format!("track-{}", (index * 37) % 160))
        .collect();
    assert_parity(&library, &ids);
}
