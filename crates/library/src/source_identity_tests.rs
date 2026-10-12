use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use common::Album;
use redb::{Database, ReadableTable, TableError};

use super::{AlbumSourceIdentity, ALBUM_SOURCE_IDENTITY_SCHEMA_VERSION};
use crate::storage::{
    encode_value, read_version, ALBUMS_TABLE, ALBUM_SOURCE_IDENTITY_TABLE, INDEX_VERSION,
    META_ALBUM_METADATA_KEY, META_TABLE, META_VERSION_KEY,
};
use crate::{AlbumMetadataStats, Library};

fn identity() -> AlbumSourceIdentity {
    AlbumSourceIdentity {
        schema_version: ALBUM_SOURCE_IDENTITY_SCHEMA_VERSION,
        context_fingerprint: "local-track-and-credit-fingerprint".into(),
        musicbrainz_release_id: "c547b57a-0cf7-4fbe-85d4-759a1193a6f8".into(),
        musicbrainz_release_group_id: "4587e021-3f9d-43a6-81f3-50505e05fbd3".into(),
        canonical_title: "共同制作".into(),
        artist_names: vec!["Prince".into(), "The New Power Generation".into()],
        artist_ids: vec![
            "070d193a-845c-479f-980e-bef15710653e".into(),
            "debaee31-738f-4f99-9868-2367b1d46470".into(),
        ],
        verified_at_ms: 1_791_215_900_000,
        evidence: "Verified complete ordered tracks and independently supported billing.".into(),
    }
}

fn memory_library() -> Library {
    Library {
        roots: Vec::new(),
        db: Arc::new(
            Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        ),
    }
}

fn write_cache_bytes(library: &Library, bytes: &[u8]) {
    let write = library.db.begin_write().unwrap();
    {
        write
            .open_table(ALBUM_SOURCE_IDENTITY_TABLE)
            .unwrap()
            .insert("album", bytes)
            .unwrap();
    }
    write.commit().unwrap();
}

#[test]
fn raw_source_credit_reads_do_not_create_rows_or_fall_back_to_enriched_records() {
    let library = memory_library();
    assert_eq!(
        library.raw_album_credits("album").unwrap(),
        common::ArtistCredits::default()
    );
    assert_eq!(
        library.raw_track_credits("track").unwrap(),
        common::ArtistCredits::default()
    );
    let album = Album {
        id: "album".into(),
        artist_id: "enriched-artist".into(),
        artist_ids: vec!["enriched-artist".into()],
        artist_names: vec!["Provider display".into()],
        title: "Enriched catalog title".into(),
        year: None,
        folder_relpath: "Artist/Album".into(),
        cover_ref: None,
        genres: Vec::new(),
        summary: None,
    };
    let write = library.db.begin_write().unwrap();
    {
        write
            .open_table(ALBUMS_TABLE)
            .unwrap()
            .insert("album", encode_value(&album).unwrap().as_slice())
            .unwrap();
    }
    write.commit().unwrap();
    assert_eq!(
        library.album_credits("album").unwrap().display_name,
        "Provider display"
    );
    assert_eq!(
        library.raw_album_credits("album").unwrap(),
        common::ArtistCredits::default()
    );
    let read = library.db.begin_read().unwrap();
    assert!(matches!(
        read.open_table(crate::storage::ALBUM_CREDITS_TABLE),
        Err(TableError::TableDoesNotExist(_))
    ));
    assert!(matches!(
        read.open_table(crate::storage::TRACK_CREDITS_TABLE),
        Err(TableError::TableDoesNotExist(_))
    ));
}

#[test]
fn source_identity_roundtrips_json_and_survives_database_reopen() {
    let file = TestDbFile::new();
    let expected = identity();
    {
        let library = Library {
            roots: Vec::new(),
            db: Library::open_db(&file.path).unwrap(),
        };
        library
            .set_album_source_identity("album", &expected)
            .unwrap();
        assert_eq!(
            library
                .get_album_source_identity("album", &expected.context_fingerprint)
                .unwrap(),
            Some(expected.clone())
        );
        let read = library.db.begin_read().unwrap();
        let cache = read.open_table(ALBUM_SOURCE_IDENTITY_TABLE).unwrap();
        let bytes = cache.get("album").unwrap().unwrap();
        assert_eq!(
            serde_json::from_slice::<AlbumSourceIdentity>(bytes.value()).unwrap(),
            expected
        );
    }
    let reopened = Library {
        roots: Vec::new(),
        db: Library::open_db(&file.path).unwrap(),
    };
    assert_eq!(
        reopened
            .get_album_source_identity("album", &expected.context_fingerprint)
            .unwrap(),
        Some(expected)
    );
}

#[test]
fn source_identity_missing_sidecar_does_not_change_legacy_album_or_index_version() {
    let library = memory_library();
    let album = Album {
        id: "album".into(),
        artist_id: "local-artist-id".into(),
        artist_ids: vec!["local-artist-id".into()],
        artist_names: vec!["Local source artist".into()],
        title: "Local source title".into(),
        year: Some(1992),
        folder_relpath: "Artist/Album".into(),
        cover_ref: None,
        genres: vec!["Funk".into()],
        summary: Some("Stored description".into()),
    };
    let album_bytes = encode_value(&album).unwrap();
    let stats = AlbumMetadataStats {
        completed: 1,
        total: 1,
    };
    let write = library.db.begin_write().unwrap();
    {
        write
            .open_table(ALBUMS_TABLE)
            .unwrap()
            .insert("album", album_bytes.as_slice())
            .unwrap();
        let mut meta = write.open_table(META_TABLE).unwrap();
        meta.insert(
            META_VERSION_KEY,
            encode_value(&INDEX_VERSION).unwrap().as_slice(),
        )
        .unwrap();
        meta.insert(
            META_ALBUM_METADATA_KEY,
            encode_value(&stats).unwrap().as_slice(),
        )
        .unwrap();
    }
    write.commit().unwrap();

    assert_eq!(
        library
            .get_album_source_identity("album", "not-cached")
            .unwrap(),
        None
    );
    {
        let read = library.db.begin_read().unwrap();
        assert!(matches!(
            read.open_table(ALBUM_SOURCE_IDENTITY_TABLE),
            Err(TableError::TableDoesNotExist(_))
        ));
    }
    library
        .set_album_source_identity("album", &identity())
        .unwrap();
    assert_eq!(read_version(&library.db).unwrap(), Some(INDEX_VERSION));
    assert_eq!(library.album_metadata_stats().unwrap(), stats);
    assert_eq!(
        library.get_album("album").unwrap().unwrap().title,
        album.title
    );
    let read = library.db.begin_read().unwrap();
    let albums = read.open_table(ALBUMS_TABLE).unwrap();
    assert_eq!(
        albums.get("album").unwrap().unwrap().value(),
        album_bytes.as_slice(),
        "new provider proof must not rewrite positional Album records"
    );
}

#[test]
fn source_identity_context_changes_or_unknown_schema_are_cache_misses() {
    let library = memory_library();
    let expected = identity();
    library
        .set_album_source_identity("album", &expected)
        .unwrap();
    for (album_id, fingerprint) in [
        ("album", "edited-local-source-fingerprint"),
        ("other-album", expected.context_fingerprint.as_str()),
        ("album", ""),
    ] {
        assert_eq!(
            library
                .get_album_source_identity(album_id, fingerprint)
                .unwrap(),
            None
        );
    }
    let mut future = expected.clone();
    future.schema_version += 1;
    write_cache_bytes(&library, &serde_json::to_vec(&future).unwrap());
    assert_eq!(
        library
            .get_album_source_identity("album", &expected.context_fingerprint)
            .unwrap(),
        None
    );
}

#[test]
fn source_identity_malformed_or_incomplete_json_never_poison_album_reads() {
    let library = memory_library();
    let expected = identity();
    let mut bad_id = expected.clone();
    bad_id.musicbrainz_release_id = "../not-a-provider-id".into();
    let mut bad_billing = expected.clone();
    bad_billing.artist_ids.pop();
    for bytes in [
        b"not-json".to_vec(),
        b"{}".to_vec(),
        serde_json::to_vec(&bad_id).unwrap(),
        serde_json::to_vec(&bad_billing).unwrap(),
    ] {
        write_cache_bytes(&library, &bytes);
        assert_eq!(
            library
                .get_album_source_identity("album", &expected.context_fingerprint)
                .unwrap(),
            None
        );
    }
    library
        .set_album_source_identity("album", &expected)
        .unwrap();
    assert_eq!(
        library
            .get_album_source_identity("album", &expected.context_fingerprint)
            .unwrap(),
        Some(expected)
    );
}

#[test]
fn source_identity_invalid_writes_preserve_previous_verified_proof() {
    let library = memory_library();
    let expected = identity();
    library
        .set_album_source_identity("album", &expected)
        .unwrap();
    let mut invalids = Vec::new();
    let mut unknown_version = expected.clone();
    unknown_version.schema_version = 0;
    invalids.push(unknown_version);
    let mut missing_context = expected.clone();
    missing_context.context_fingerprint = " ".into();
    invalids.push(missing_context);
    let mut invalid_release = expected.clone();
    invalid_release.musicbrainz_release_id = "00000000-0000-0000-0000-000000000000".into();
    invalids.push(invalid_release);
    let mut invalid_group = expected.clone();
    invalid_group.musicbrainz_release_group_id = "a-url-or-title".into();
    invalids.push(invalid_group);
    let mut invalid_artist = expected.clone();
    invalid_artist.artist_ids[0] = "local-artist-id".into();
    invalids.push(invalid_artist);
    let mut missing_name = expected.clone();
    missing_name.artist_names[0].clear();
    invalids.push(missing_name);
    let mut missing_title = expected.clone();
    missing_title.canonical_title.clear();
    invalids.push(missing_title);
    let mut missing_time = expected.clone();
    missing_time.verified_at_ms = 0;
    invalids.push(missing_time);
    let mut missing_evidence = expected.clone();
    missing_evidence.evidence.clear();
    invalids.push(missing_evidence);
    for invalid in invalids {
        assert!(library
            .set_album_source_identity("album", &invalid)
            .is_err());
    }
    assert!(library.set_album_source_identity(" ", &expected).is_err());
    assert_eq!(
        library
            .get_album_source_identity("album", &expected.context_fingerprint)
            .unwrap(),
        Some(expected)
    );
}

#[test]
fn source_identity_survives_incremental_and_forced_scans_without_index_rebuild_policy() {
    let library = memory_library();
    library.rescan().unwrap();
    let expected = identity();
    library
        .set_album_source_identity("album", &expected)
        .unwrap();
    library.incremental_scan().unwrap();
    library.rescan().unwrap();
    assert_eq!(read_version(&library.db).unwrap(), Some(INDEX_VERSION));
    assert_eq!(
        library
            .get_album_source_identity("album", &expected.context_fingerprint)
            .unwrap(),
        Some(expected)
    );
}

struct TestDbFile {
    path: PathBuf,
}

impl TestDbFile {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self {
            path: std::env::temp_dir().join(format!(
                "phonolite-source-identity-{}-{nonce}.redb",
                std::process::id()
            )),
        }
    }
}

impl Drop for TestDbFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
