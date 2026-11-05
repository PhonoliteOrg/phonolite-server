//! Response parity against the pre-batching catalogue projection.
use super::*;

fn legacy_album_views(
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
        .map(|album| {
            let credits = library
                .album_credits(&album.id)
                .map_err(|err| err.to_string())?;
            Ok(album_view(album, &track_counts, &artist_names, credits))
        })
        .collect::<Result<Vec<_>, String>>()?)
}

fn legacy_track_views(
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
    let mut album_genres = HashMap::new();
    let mut artist_genres = HashMap::new();

    Ok(tracks
        .iter()
        .map(|track| {
            let credits = library
                .track_credits(&track.id)
                .map_err(|err| err.to_string())?;
            let headlines = credits
                .credits
                .iter()
                .filter(|credit| is_track_artist(&credit.role))
                .collect::<Vec<_>>();
            let genres = legacy_track_genres(
                library,
                track,
                &headlines,
                &mut album_genres,
                &mut artist_genres,
            )?;
            let listening_genres = legacy_listening_track_genres(
                library,
                track,
                &headlines,
                &mut album_genres,
                &mut artist_genres,
            )?;
            Ok(TrackView {
                id: track.id.clone(),
                title: track.title.clone(),
                artist: if credits.display_name.trim().is_empty() {
                    artist_names
                        .get(&track.artist_id)
                        .cloned()
                        .unwrap_or_else(|| "Unknown Artist".to_string())
                } else {
                    credits.display_name.clone()
                },
                album: album_titles
                    .get(&track.album_id)
                    .cloned()
                    .unwrap_or_else(|| "Unknown Album".to_string()),
                artist_id: track.artist_id.clone(),
                artist_ids: headlines
                    .iter()
                    .map(|credit| credit.artist_id.clone())
                    .collect(),
                artist_names: headlines
                    .iter()
                    .map(|credit| credit.credited_name.clone())
                    .collect(),
                artist_credits: credits.credits,
                album_id: track.album_id.clone(),
                duration_ms: track.duration_ms,
                track_no: track.track_no,
                disc_no: track.disc_no,
                genres,
                listening_genres,
                liked: liked_set.contains(&track.id),
                in_playlists: playlist_set.contains(&track.id),
            })
        })
        .collect::<Result<Vec<_>, String>>()?)
}

fn legacy_track_genres(
    library: &::library::Library,
    track: &common::Track,
    performers: &[&ArtistCredit],
    album_cache: &mut HashMap<String, Vec<String>>,
    artist_cache: &mut HashMap<String, Vec<String>>,
) -> Result<Vec<String>, String> {
    let own = genre_labels(&track.genres);
    if !own.is_empty() {
        return Ok(own);
    }
    // Provider enrichment can arrive after indexing. Project the current album
    // metadata for untagged songs without copying every album genre onto tags.
    let genres = legacy_cached_album_genres(library, &track.album_id, album_cache)?;
    if !genres.is_empty() {
        return Ok(genres.clone());
    }
    let performer_ids = if performers.is_empty() {
        vec![track.artist_id.as_str()]
    } else {
        performers
            .iter()
            .map(|credit| credit.artist_id.as_str())
            .collect()
    };
    let mut genres = Vec::new();
    for id in performer_ids {
        if !artist_cache.contains_key(id) {
            let values = library
                .get_artist(id)
                .map_err(|err| err.to_string())?
                .map(|artist| genre_labels(&artist.genres))
                .unwrap_or_default();
            artist_cache.insert(id.to_string(), values);
        }
        genres.extend(artist_cache.get(id).into_iter().flatten().cloned());
    }
    Ok(genre_labels(&genres))
}

fn legacy_listening_track_genres(
    library: &::library::Library,
    track: &common::Track,
    performers: &[&ArtistCredit],
    album_cache: &mut HashMap<String, Vec<String>>,
    artist_cache: &mut HashMap<String, Vec<String>>,
) -> Result<Vec<String>, String> {
    // Catalog release enrichment describes this recording more specifically
    // than broad file labels or the performer's whole discography. Reuse the
    // request's cache so sibling songs and offline projections need no extra
    // per-song album reads.
    let genres = legacy_cached_album_genres(library, &track.album_id, album_cache)?;
    if !genres.is_empty() {
        return Ok(genres.clone());
    }
    legacy_track_genres(library, track, performers, album_cache, artist_cache)
}

fn legacy_cached_album_genres<'a>(
    library: &::library::Library,
    album_id: &str,
    album_cache: &'a mut HashMap<String, Vec<String>>,
) -> Result<&'a Vec<String>, String> {
    if !album_cache.contains_key(album_id) {
        let genres = library
            .get_album(album_id)
            .map_err(|err| err.to_string())?
            .map(|album| genre_labels(&album.genres))
            .unwrap_or_default();
        album_cache.insert(album_id.to_string(), genres);
    }
    Ok(&album_cache[album_id])
}

fn genre_labels(values: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    values
        .iter()
        .map(|genre| genre_display_label(genre))
        .filter(|genre| !genre.is_empty() && seen.insert(genre.to_lowercase()))
        .collect()
}

use common::{Artist, Codec, Track};
use redb::{Database, TableDefinition};
use std::{fs, path::PathBuf, sync::Arc};

const ARTISTS: TableDefinition<&str, &[u8]> = TableDefinition::new("artists");
const ALBUMS: TableDefinition<&str, &[u8]> = TableDefinition::new("albums");
const TRACKS: TableDefinition<&str, &[u8]> = TableDefinition::new("tracks");
const ALBUM_TRACKS: TableDefinition<&str, &[u8]> = TableDefinition::new("album_tracks");
const ALBUM_CREDITS: TableDefinition<&str, &[u8]> = TableDefinition::new("album_credits_v1");
const TRACK_CREDITS: TableDefinition<&str, &[u8]> = TableDefinition::new("track_credits_v1");
const DISPLAY_RESOLUTIONS: TableDefinition<&str, &[u8]> =
    TableDefinition::new("display_credit_resolutions_v1");

struct Directory(PathBuf);
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    library: ::library::Library,
    database: Arc<Database>,
    tracks: Vec<Track>,
    albums: Vec<Album>,
    // Declared last so Windows closes the database before removing its files.
    _directory: Directory,
}

fn insert_raw(
    database: &Database,
    definition: TableDefinition<&str, &[u8]>,
    id: &str,
    bytes: &[u8],
) {
    let write = database.begin_write().unwrap();
    {
        let mut table = write.open_table(definition).unwrap();
        table.insert(id, bytes).unwrap();
    }
    write.commit().unwrap();
}

fn insert<T: Serialize>(
    database: &Database,
    definition: TableDefinition<&str, &[u8]>,
    id: &str,
    value: &T,
) {
    insert_raw(
        database,
        definition,
        id,
        &bincode::serialize(value).unwrap(),
    );
}

fn credit(id: &str, name: &str, role: &str) -> ArtistCredit {
    ArtistCredit {
        artist_id: id.into(),
        name: name.into(),
        credited_name: name.into(),
        role: role.into(),
        join_phrase: String::new(),
        source: "local_tags_structured".into(),
    }
}

fn album(id: &str, genres: &[&str]) -> Album {
    Album {
        id: id.into(),
        artist_id: "primary".into(),
        artist_ids: Vec::new(),
        artist_names: Vec::new(),
        title: format!("Record {id}"),
        year: Some(2004),
        folder_relpath: id.into(),
        cover_ref: None,
        genres: genres.iter().map(|genre| (*genre).into()).collect(),
        summary: Some("Fixture summary".into()),
    }
}

fn track(id: &str, album_id: &str, genres: &[&str]) -> Track {
    Track {
        id: id.into(),
        album_id: album_id.into(),
        artist_id: "primary".into(),
        title: format!("Song {id} β"),
        track_no: Some(2),
        disc_no: Some(1),
        duration_ms: 123_456,
        codec: Codec::Flac,
        sample_rate: Some(44_100),
        channels: Some(2),
        bitrate: None,
        file_relpath: format!("{album_id}/{id}.flac"),
        file_size: 1024,
        genres: genres.iter().map(|genre| (*genre).into()).collect(),
    }
}

fn fixture() -> Fixture {
    let directory = Directory(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!("catalogue-view-parity-{}", uuid::Uuid::new_v4())),
    );
    fs::create_dir_all(&directory.0).unwrap();
    let database = ::library::Library::open_db(&directory.0.join("library.redb")).unwrap();
    // Empty roots initialize the real schema without parsers, workers or media.
    let library = ::library::Library::load_or_scan_with_db(Vec::new(), Arc::clone(&database))
        .unwrap()
        .0;
    for (id, name, genres) in [
        ("primary", "Catalog Primary", vec!["ambient"]),
        ("guest", "Guest β", vec!["r&b", "edm", "R&B"]),
    ] {
        insert(
            &database,
            ARTISTS,
            id,
            &Artist {
                id: id.into(),
                name: name.into(),
                genres: genres.into_iter().map(String::from).collect(),
                summary: None,
                logo_ref: None,
                banner_ref: None,
            },
        );
    }
    // This artist is referenced by credits but must remain unread when track
    // or album genres already satisfy the existing projection rules.
    insert_raw(
        &database,
        ARTISTS,
        "corrupt-unused",
        b"not a serialized artist",
    );

    let mut enriched = album("enriched", &[" acid jazz ", "ACID JAZZ", "edm"]);
    enriched.artist_ids = vec!["primary".into(), "guest".into(), "absent-artist".into()];
    enriched.artist_names = vec!["Original Primary".into(), "Original Guest".into()];
    let bare = album("bare", &[]);
    let mut absent = album("absent-album", &[]);
    absent.artist_id = "absent-artist".into();
    absent.year = None;
    absent.summary = None;
    for value in [&enriched, &bare] {
        insert(&database, ALBUMS, &value.id, value);
    }
    insert(
        &database,
        ALBUM_CREDITS,
        "enriched",
        &ArtistCredits {
            display_name: "Primary & Guest β".into(),
            credits: vec![
                credit("primary", "Catalog Primary", "album_artist"),
                credit("guest", "Guest β", "album_artist"),
            ],
        },
    );

    let mut unknown = track("unknown", "absent-album", &[]);
    unknown.artist_id = "absent-artist".into();
    let tracks = vec![
        track("tagged", "enriched", &[" rock ", "ROCK", ""]),
        track("album-fallback", "enriched", &[]),
        track("performer-fallback", "bare", &[]),
        track("legacy", "bare", &[]),
        unknown,
        track("support-only", "bare", &[]),
        track("unused-artist", "enriched", &[]),
        track("unused-artist-tagged", "bare", &["pop"]),
    ];
    for value in &tracks {
        insert(&database, TRACKS, &value.id, value);
        insert_raw(
            &database,
            ALBUM_TRACKS,
            &format!("{}\x1f{}", value.album_id, value.id),
            &[],
        );
    }
    insert(
        &database,
        TRACK_CREDITS,
        "performer-fallback",
        &ArtistCredits {
            display_name: "Guest β feat. Catalog Primary".into(),
            credits: vec![
                credit("guest", "Guest β", "track_artist"),
                credit("primary", "Catalog Primary", "featured_artist"),
                credit("corrupt-unused", "Support", "producer"),
            ],
        },
    );
    insert(
        &database,
        TRACK_CREDITS,
        "support-only",
        &ArtistCredits {
            display_name: "Production credit".into(),
            credits: vec![credit("corrupt-unused", "Support", "producer")],
        },
    );
    for id in ["unused-artist", "unused-artist-tagged"] {
        insert(
            &database,
            TRACK_CREDITS,
            id,
            &ArtistCredits {
                display_name: "Credited Guest".into(),
                credits: vec![credit("corrupt-unused", "Credited Guest", "track_artist")],
            },
        );
    }
    Fixture {
        library,
        database,
        tracks,
        albums: vec![enriched, bare, absent],
        _directory: directory,
    }
}

fn json_result<T: Serialize>(result: Result<T, String>) -> Result<serde_json::Value, String> {
    result.map(|value| serde_json::to_value(value).unwrap())
}

#[test]
fn catalogue_batch_track_responses_match_previous_reads_for_order_duplicates_and_users() {
    let fixture = fixture();
    let mut tracks = fixture.tracks.clone();
    tracks.reverse();
    tracks.push(fixture.tracks[2].clone());
    tracks.push(fixture.tracks[0].clone());
    for (liked, playlists) in [
        (HashSet::new(), HashSet::new()),
        (
            HashSet::from(["tagged".into(), "unknown".into()]),
            HashSet::from(["performer-fallback".into()]),
        ),
        (
            HashSet::from(["performer-fallback".into()]),
            HashSet::from(["tagged".into(), "unknown".into()]),
        ),
    ] {
        let actual = build_track_views(&fixture.library, &tracks, &liked, &playlists).unwrap();
        let expected = legacy_track_views(&fixture.library, &tracks, &liked, &playlists).unwrap();
        assert_eq!(
            serde_json::to_value(&actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        assert_eq!(
            actual.iter().map(|view| &view.id).collect::<Vec<_>>(),
            tracks.iter().map(|track| &track.id).collect::<Vec<_>>()
        );
        for view in &actual {
            assert_eq!(view.liked, liked.contains(&view.id));
            assert_eq!(view.in_playlists, playlists.contains(&view.id));
        }
        let tagged = actual.iter().find(|view| view.id == "tagged").unwrap();
        assert_eq!(tagged.genres, ["Rock"]);
        assert_eq!(tagged.listening_genres, ["Acid Jazz", "EDM"]);
        let performer = actual
            .iter()
            .find(|view| view.id == "performer-fallback")
            .unwrap();
        assert_eq!(performer.genres, ["R&B", "EDM", "Ambient"]);
        assert_eq!(performer.artist_ids, ["guest", "primary"]);
        let unknown = actual.iter().find(|view| view.id == "unknown").unwrap();
        assert_eq!(unknown.artist, "Unknown Artist");
        assert_eq!(unknown.album, "Unknown Album");
    }
}

#[test]
fn catalogue_batch_album_responses_match_previous_reads_with_verified_and_legacy_credits() {
    let fixture = fixture();
    let albums = vec![
        fixture.albums[2].clone(),
        fixture.albums[0].clone(),
        fixture.albums[1].clone(),
        fixture.albums[0].clone(),
    ];
    let actual = build_album_views(&fixture.library, albums.clone()).unwrap();
    let expected = legacy_album_views(&fixture.library, albums).unwrap();
    assert_eq!(
        serde_json::to_value(&actual).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
    assert_eq!(
        actual
            .iter()
            .map(|view| view.id.as_str())
            .collect::<Vec<_>>(),
        ["absent-album", "enriched", "bare", "enriched"]
    );
    assert_eq!(actual[1].track_count, 3);
    assert_eq!(actual[2].track_count, 4);
    assert_eq!(actual[1].artist_ids, ["primary", "guest", "absent-artist"]);
    assert_eq!(actual[0].artist_name, "Unknown Artist");
}

#[test]
fn catalogue_batch_responses_preserve_missing_credit_tables_and_empty_inputs() {
    let fixture = fixture();
    let write = fixture.database.begin_write().unwrap();
    write.delete_table(TRACK_CREDITS).unwrap();
    write.delete_table(ALBUM_CREDITS).unwrap();
    write.commit().unwrap();
    let empty = HashSet::new();
    for tracks in [&fixture.tracks[..], &[]] {
        assert_eq!(
            json_result(build_track_views(&fixture.library, tracks, &empty, &empty)),
            json_result(legacy_track_views(&fixture.library, tracks, &empty, &empty))
        );
    }
    for albums in [fixture.albums.clone(), Vec::new()] {
        assert_eq!(
            json_result(build_album_views(&fixture.library, albums.clone())),
            json_result(legacy_album_views(&fixture.library, albums))
        );
    }
}

#[test]
fn catalogue_batch_responses_preserve_required_metadata_decode_errors() {
    let fixture = fixture();
    let empty = HashSet::new();
    insert_raw(&fixture.database, ALBUMS, "enriched", b"invalid album");
    let tracks = &fixture.tracks[..2];
    let actual = json_result(build_track_views(&fixture.library, tracks, &empty, &empty));
    let expected = json_result(legacy_track_views(&fixture.library, tracks, &empty, &empty));
    assert!(actual.is_err());
    assert_eq!(actual, expected);
}

#[test]
fn catalogue_batch_responses_preserve_verified_display_credit_joining() {
    let fixture = fixture();
    let mut source = ArtistCredits {
        display_name: "Catalog Primary, Guest β".into(),
        credits: vec![
            credit("primary", "Catalog Primary", "track_artist"),
            credit("guest", "Guest β", "featured_artist"),
        ],
    };
    for credit in &mut source.credits {
        credit.source = "local_tags".into();
    }
    let mut resolved = source.clone();
    resolved.credits[0].join_phrase = " and ".into();
    resolved.credits[1].credited_name = "the Guest β".into();
    insert(
        &fixture.database,
        DISPLAY_RESOLUTIONS,
        &source.display_name.to_lowercase(),
        &resolved,
    );
    insert(&fixture.database, TRACK_CREDITS, "tagged", &source);
    for credit in &mut source.credits {
        credit.role = "album_artist".into();
    }
    insert(&fixture.database, ALBUM_CREDITS, "enriched", &source);
    let empty = HashSet::new();
    let tracks = &fixture.tracks[..1];
    let actual = build_track_views(&fixture.library, tracks, &empty, &empty).unwrap();
    assert_eq!(actual[0].artist, "Catalog Primary and the Guest β");
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        json_result(legacy_track_views(&fixture.library, tracks, &empty, &empty)).unwrap()
    );
    let albums = fixture.albums[..1].to_vec();
    let actual = build_album_views(&fixture.library, albums.clone()).unwrap();
    assert_eq!(actual[0].artist_name, "Catalog Primary and the Guest β");
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        json_result(legacy_album_views(&fixture.library, albums)).unwrap()
    );
}
