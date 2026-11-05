use std::sync::Arc;

use common::{Artist, ArtistCredit, Codec};
use redb::{Database, TableDefinition};
use serde::Serialize;

use super::*;
use crate::storage::{
    album_track_key, encode_value, ALBUMS_TABLE, ALBUM_CREDITS_TABLE, ARTISTS_TABLE, TRACKS_TABLE,
    TRACK_CREDITS_TABLE,
};

fn library() -> Library {
    let db = Database::builder()
        .create_with_backend(redb::backends::InMemoryBackend::new())
        .unwrap();
    Library::load_or_scan_with_db(Vec::new(), Arc::new(db))
        .unwrap()
        .0
}

fn put<T: Serialize>(library: &Library, table: TableDefinition<&str, &[u8]>, id: &str, value: &T) {
    let write = library.db.begin_write().unwrap();
    write
        .open_table(table)
        .unwrap()
        .insert(id, encode_value(value).unwrap().as_slice())
        .unwrap();
    write.commit().unwrap();
}

fn corrupt(library: &Library, id: &str) {
    let write = library.db.begin_write().unwrap();
    write
        .open_table(ARTISTS_TABLE)
        .unwrap()
        .insert(id, &[255u8][..])
        .unwrap();
    write.commit().unwrap();
}

fn artist(id: &str, genres: &[&str]) -> Artist {
    Artist {
        id: id.into(),
        name: id.into(),
        genres: genres.iter().map(|genre| (*genre).into()).collect(),
        summary: None,
        logo_ref: None,
        banner_ref: None,
    }
}

fn album() -> Album {
    Album {
        id: "release".into(),
        artist_id: "main".into(),
        artist_ids: vec!["main".into(), "guest".into()],
        artist_names: vec!["main".into(), "guest".into()],
        title: "Release".into(),
        year: Some(2026),
        folder_relpath: String::new(),
        cover_ref: None,
        genres: vec![" soul ".into(), "SOUL".into()],
        summary: None,
    }
}

fn track(id: &str) -> Track {
    Track {
        id: id.into(),
        album_id: "release".into(),
        artist_id: "main".into(),
        title: id.into(),
        track_no: None,
        disc_no: None,
        duration_ms: 1000,
        codec: Codec::Flac,
        sample_rate: Some(48_000),
        channels: Some(2),
        bitrate: None,
        file_relpath: String::new(),
        file_size: 0,
        genres: Vec::new(),
    }
}

fn credits(role: &str) -> ArtistCredits {
    ArtistCredits {
        display_name: "main feat. guest".into(),
        credits: [
            ("main", role),
            ("guest", "featured_artist"),
            ("unused", "composer"),
        ]
        .into_iter()
        .map(|(id, role)| ArtistCredit {
            artist_id: id.into(),
            name: id.into(),
            credited_name: id.into(),
            role: role.into(),
            join_phrase: if id == "main" {
                " feat. ".into()
            } else {
                String::new()
            },
            source: "local_tags_structured".into(),
        })
        .collect(),
    }
}

fn fixture() -> (Library, Album, Track) {
    let library = library();
    let album = album();
    let track = track("one");
    put(
        &library,
        ARTISTS_TABLE,
        "main",
        &artist("main", &["rock", "jazz"]),
    );
    put(
        &library,
        ARTISTS_TABLE,
        "guest",
        &artist("guest", &["JAZZ", " r&b "]),
    );
    put(&library, ALBUMS_TABLE, &album.id, &album);
    put(&library, TRACKS_TABLE, &track.id, &track);
    put(
        &library,
        TRACK_CREDITS_TABLE,
        &track.id,
        &credits("track_artist"),
    );
    put(
        &library,
        ALBUM_CREDITS_TABLE,
        &album.id,
        &credits("album_artist"),
    );
    (library, album, track)
}

#[test]
fn repeated_inputs_share_table_opens_and_decodes_but_keep_ordered_output() {
    let (library, _, track) = fixture();
    let mut tagged = track.clone();
    tagged.genres = vec![" ambient ".into()];
    let tracks: Vec<_> = (0..1_000)
        .map(|index| {
            if index % 2 == 0 {
                track.clone()
            } else {
                tagged.clone()
            }
        })
        .collect();
    let read = library.db.begin_read().unwrap();
    let mut reader = CreditReader::new(&read);
    let metadata = library
        .track_view_metadata_from_reader(&tracks, &mut reader)
        .unwrap();
    assert_eq!(metadata.tracks.len(), 1_000);
    assert_eq!(metadata.artist_names.len(), 1);
    assert_eq!(metadata.album_titles.len(), 1);
    // Artist, album and credit tables open and decode once for 1,000 inputs.
    assert_eq!(reader.work(), (3, 3));
    for (index, metadata) in metadata.tracks.iter().enumerate() {
        assert_eq!(metadata.credits, credits("track_artist"));
        assert_eq!(
            metadata.genres,
            if index % 2 == 0 {
                vec!["Soul"]
            } else {
                vec!["Ambient"]
            }
        );
        assert_eq!(metadata.listening_genres, vec!["Soul"]);
    }
}

#[test]
fn distinct_tracks_share_table_handles_and_related_metadata_decodes() {
    let (library, _, track) = fixture();
    let tracks: Vec<_> = (0..256)
        .map(|index| {
            let mut item = track.clone();
            item.id = format!("distinct-{index}");
            item
        })
        .collect();
    let write = library.db.begin_write().unwrap();
    {
        let mut table = write.open_table(TRACK_CREDITS_TABLE).unwrap();
        let encoded = encode_value(&credits("track_artist")).unwrap();
        for track in &tracks {
            table.insert(track.id.as_str(), encoded.as_slice()).unwrap();
        }
    }
    write.commit().unwrap();

    let read = library.db.begin_read().unwrap();
    let mut reader = CreditReader::new(&read);
    let metadata = library
        .track_view_metadata_from_reader(&tracks, &mut reader)
        .unwrap();
    assert_eq!(metadata.tracks.len(), tracks.len());
    // One artist and album decode, one credit decode per distinct track, and
    // three table opens for the whole collection.
    assert_eq!(reader.work(), (3, tracks.len() + 2));
    assert!(metadata
        .tracks
        .iter()
        .all(|display| display.genres == ["Soul"] && display.listening_genres == ["Soul"]));
}

#[test]
fn album_metadata_retains_duplicate_credits_and_counts_index_entries() {
    let (library, album, _) = fixture();
    let write = library.db.begin_write().unwrap();
    {
        let mut table = write.open_table(ALBUM_TRACKS_TABLE).unwrap();
        for (order, id) in ["one", "two"].into_iter().enumerate() {
            table
                .insert(
                    album_track_key(&album.id, order, id).as_str(),
                    id.as_bytes(),
                )
                .unwrap();
        }
    }
    write.commit().unwrap();
    let mut missing = album.clone();
    missing.id = "missing".into();
    let metadata = library
        .album_view_metadata(&[album.clone(), missing, album])
        .unwrap();
    assert_eq!(metadata.track_counts.get("release"), Some(&2));
    assert_eq!(metadata.track_counts.get("missing"), Some(&0));
    assert_eq!(
        metadata.artist_names.get("main").map(String::as_str),
        Some("main")
    );
    assert_eq!(
        metadata.artist_names.get("guest").map(String::as_str),
        Some("guest")
    );
    assert_eq!(
        metadata.credits,
        vec![
            credits("album_artist"),
            ArtistCredits::default(),
            credits("album_artist")
        ]
    );
}

#[test]
fn unused_corrupt_performer_is_read_only_when_genre_fallback_needs_it() {
    let (library, mut album, mut track) = fixture();
    corrupt(&library, "guest");
    assert_eq!(
        library
            .track_view_metadata(std::slice::from_ref(&track))
            .unwrap()
            .tracks[0]
            .genres,
        ["Soul"]
    );
    album.genres.clear();
    put(&library, ALBUMS_TABLE, &album.id, &album);
    track.genres = vec![" blues ".into()];
    let metadata = library
        .track_view_metadata(std::slice::from_ref(&track))
        .unwrap();
    assert_eq!(metadata.tracks[0].genres, ["Blues"]);
    assert_eq!(metadata.tracks[0].listening_genres, ["Blues"]);
    track.genres.clear();
    assert!(library.track_view_metadata(&[track]).is_err());
}

#[test]
fn performer_fallback_uses_resolved_billing_and_ignores_supporting_credits() {
    let (library, mut album, track) = fixture();
    album.genres = vec!["  ".into()];
    put(&library, ALBUMS_TABLE, &album.id, &album);
    corrupt(&library, "unused");
    let metadata = library.track_view_metadata(&[track]).unwrap();
    assert_eq!(metadata.tracks[0].genres, ["Rock", "Jazz", "R&B"]);
    assert_eq!(metadata.tracks[0].listening_genres, ["Rock", "Jazz", "R&B"]);
}

#[test]
fn legacy_credit_tables_and_missing_metadata_keep_existing_fallbacks() {
    let (library, album, track) = fixture();
    let write = library.db.begin_write().unwrap();
    write.delete_table(TRACK_CREDITS_TABLE).unwrap();
    write.delete_table(ALBUM_CREDITS_TABLE).unwrap();
    write.commit().unwrap();
    let metadata = library
        .track_view_metadata(std::slice::from_ref(&track))
        .unwrap();
    assert_eq!(
        metadata.tracks[0].credits,
        library.track_credits(&track.id).unwrap()
    );
    assert_eq!(metadata.tracks[0].credits.display_name, "main");
    assert_eq!(metadata.tracks[0].credits.credits[0].source, "legacy");
    let metadata = library
        .album_view_metadata(std::slice::from_ref(&album))
        .unwrap();
    assert_eq!(
        metadata.credits[0],
        library.album_credits(&album.id).unwrap()
    );
    assert_eq!(metadata.credits[0].display_name, "main, guest");
    let mut missing = track;
    missing.id = "missing".into();
    missing.artist_id = "absent".into();
    missing.album_id = "absent".into();
    let metadata = library.track_view_metadata(&[missing]).unwrap();
    assert!(metadata.artist_names.is_empty());
    assert!(metadata.album_titles.is_empty());
    assert_eq!(metadata.tracks[0].credits, ArtistCredits::default());
    assert!(metadata.tracks[0].genres.is_empty());
}

#[test]
fn related_metadata_and_credits_share_the_same_read_snapshot() {
    let (library, mut album, track) = fixture();
    let read = library.db.begin_read().unwrap();
    album.title = "Updated".into();
    album.genres = vec!["techno".into()];
    put(&library, ALBUMS_TABLE, &album.id, &album);
    let mut updated = credits("track_artist");
    updated.display_name = "Updated credits".into();
    put(&library, TRACK_CREDITS_TABLE, &track.id, &updated);
    let metadata = library
        .track_view_metadata_from_reader(
            std::slice::from_ref(&track),
            &mut CreditReader::new(&read),
        )
        .unwrap();
    assert_eq!(
        metadata.album_titles.get("release").map(String::as_str),
        Some("Release")
    );
    assert_eq!(metadata.tracks[0].credits.display_name, "main feat. guest");
    assert_eq!(metadata.tracks[0].genres, ["Soul"]);
    let latest = library.track_view_metadata(&[track]).unwrap();
    assert_eq!(
        latest.album_titles.get("release").map(String::as_str),
        Some("Updated")
    );
    assert_eq!(latest.tracks[0].credits.display_name, "Updated credits");
    assert_eq!(latest.tracks[0].genres, ["Techno"]);
}
