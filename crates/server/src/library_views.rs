//! Library metadata projections shared by API responses and offline snapshots.

use std::collections::{HashMap, HashSet};

use common::{genre_display_label, Album, Artist, ArtistCredit, ArtistCredits};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArtistView {
    pub id: String,
    pub name: String,
    pub genres: Vec<String>,
    pub album_count: usize,
    #[serde(default)]
    pub release_count: Option<usize>,
    #[serde(default)]
    pub collab_album_count: usize,
    #[serde(default)]
    pub appearance_count: usize,
    #[serde(default)]
    pub other_credit_count: usize,
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
    #[serde(default)]
    pub artist_credits: Vec<ArtistCredit>,
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
    #[serde(default)]
    pub artist_ids: Vec<String>,
    #[serde(default)]
    pub artist_names: Vec<String>,
    #[serde(default)]
    pub artist_credits: Vec<ArtistCredit>,
    pub album_id: String,
    pub duration_ms: u32,
    pub track_no: Option<u16>,
    pub disc_no: Option<u16>,
    pub genres: Vec<String>,
    /// Album-first projection for listening statistics. Song-tag semantics stay
    /// in `genres` so library filtering and shuffle keep their existing policy.
    #[serde(default)]
    pub listening_genres: Vec<String>,
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
    Ok(artists
        .into_iter()
        .map(|artist| {
            let releases = library
                .artist_release_relationships(&artist.id)
                .map_err(|err| err.to_string())?;
            let album_count = releases.albums.len();
            let collab_album_count = releases.collab_albums.len();
            let release_count = releases
                .albums
                .iter()
                .map(|album| &album.id)
                .chain(releases.collab_albums.iter().map(|entry| &entry.album.id))
                .chain(releases.appears_on.iter().map(|entry| &entry.album.id))
                .collect::<HashSet<_>>()
                .len();
            let (appearance_count, other_credit_count) =
                (releases.appears_on.len(), releases.other_credits.len());
            Ok(ArtistView {
                id: artist.id,
                name: artist.name,
                genres: genre_labels(&artist.genres),
                album_count,
                release_count: Some(release_count),
                collab_album_count,
                appearance_count,
                other_credit_count,
                summary: artist.summary,
                logo_ref: artist.logo_ref,
                banner_ref: artist.banner_ref,
            })
        })
        .collect::<Result<Vec<_>, String>>()?)
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
        .map(|album| {
            let credits = library
                .album_credits(&album.id)
                .map_err(|err| err.to_string())?;
            Ok(album_view(album, &track_counts, &artist_names, credits))
        })
        .collect::<Result<Vec<_>, String>>()?)
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
            let genres = track_genres(
                library,
                track,
                &headlines,
                &mut album_genres,
                &mut artist_genres,
            )?;
            let listening_genres = listening_track_genres(
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

fn track_genres(
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
    let genres = cached_album_genres(library, &track.album_id, album_cache)?;
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

fn listening_track_genres(
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
    let genres = cached_album_genres(library, &track.album_id, album_cache)?;
    if !genres.is_empty() {
        return Ok(genres.clone());
    }
    track_genres(library, track, performers, album_cache, artist_cache)
}

fn cached_album_genres<'a>(
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

#[cfg(test)]
mod genre_display_tests {
    #[test]
    fn genre_labels_capitalize_old_metadata_without_changing_the_source() {
        let raw = vec![
            " acid jazz ".into(),
            "ACID JAZZ".into(),
            "edm".into(),
            "r&b".into(),
        ];
        assert_eq!(super::genre_labels(&raw), ["Acid Jazz", "EDM", "R&B"]);
        assert_eq!(raw[0], " acid jazz ");
    }
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
    mut credits: ArtistCredits,
) -> AlbumView {
    let artist_name = if credits.display_name.trim().is_empty() {
        album_artist_name(&album, artist_names.get(&album.artist_id).cloned())
    } else {
        credits.display_name.clone()
    };
    // Verified album metadata can add a headline artist without rewriting the
    // original tag credits. Include those identities in API and offline views.
    for (index, id) in album.all_artist_ids().iter().enumerate() {
        if credits
            .credits
            .iter()
            .any(|credit| credit.artist_id == *id && credit.role == "album_artist")
        {
            continue;
        }
        let name = artist_names
            .get(id)
            .cloned()
            .or_else(|| album.artist_names.get(index).cloned())
            .unwrap_or_else(|| "Unknown Artist".to_string());
        credits.credits.push(ArtistCredit {
            artist_id: id.clone(),
            name: name.clone(),
            credited_name: name,
            role: "album_artist".to_string(),
            join_phrase: String::new(),
            source: "verified_album_metadata".to_string(),
        });
    }
    let headlines = credits
        .credits
        .iter()
        .filter(|credit| credit.role == "album_artist")
        .collect::<Vec<_>>();
    let track_count = track_counts.get(&album.id).copied().unwrap_or(0);
    AlbumView {
        id: album.id,
        artist_id: album.artist_id.clone(),
        artist_ids: headlines
            .iter()
            .map(|credit| credit.artist_id.clone())
            .collect(),
        artist_name,
        artist_names: headlines
            .iter()
            .map(|credit| credit.credited_name.clone())
            .collect(),
        artist_credits: credits.credits,
        title: album.title,
        year: album.year,
        genres: genre_labels(&album.genres),
        track_count,
        summary: album.summary,
    }
}

fn is_track_artist(role: &str) -> bool {
    matches!(role, "track_artist" | "featured_artist")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArtistReleaseContributionView {
    pub album: AlbumView,
    pub track_ids: Vec<String>,
    pub track_titles: Vec<String>,
    pub roles: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArtistReleasesView {
    pub albums: Vec<AlbumView>,
    #[serde(default)]
    pub collab_albums: Vec<ArtistReleaseContributionView>,
    pub appears_on: Vec<ArtistReleaseContributionView>,
    pub other_credits: Vec<ArtistReleaseContributionView>,
}

pub fn build_artist_releases_view(
    library: &::library::Library,
    artist_id: &str,
) -> Result<ArtistReleasesView, String> {
    let releases = library
        .artist_release_relationships(artist_id)
        .map_err(|err| err.to_string())?;
    let project = |items: Vec<::library::ArtistReleaseContribution>| -> Result<Vec<ArtistReleaseContributionView>, String> {
        items.into_iter().map(|item| Ok(ArtistReleaseContributionView {
            album: build_album_view(library, item.album)?,
            track_ids: item.track_ids,
            track_titles: item.track_titles,
            roles: item.roles,
        })).collect()
    };
    Ok(ArtistReleasesView {
        albums: build_album_views(library, releases.albums)?,
        collab_albums: project(releases.collab_albums)?,
        appears_on: project(releases.appears_on)?,
        other_credits: project(releases.other_credits)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::artist_identity_id;
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    struct SampleDirectory(PathBuf);
    impl Drop for SampleDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_flac(path: &Path, tags: &[(&str, &str)]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut stream = [0u8; 34];
        stream[..2].copy_from_slice(&4096_u16.to_be_bytes());
        stream[2..4].copy_from_slice(&4096_u16.to_be_bytes());
        stream[10..18]
            .copy_from_slice(&((44_100_u64 << 44) | (1 << 41) | (15 << 36) | 44_100).to_be_bytes());
        let mut comments = Vec::new();
        comments.extend_from_slice(&4_u32.to_le_bytes());
        comments.extend_from_slice(b"test");
        comments.extend_from_slice(&(tags.len() as u32).to_le_bytes());
        for (key, value) in tags {
            let entry = format!("{key}={value}");
            comments.extend_from_slice(&(entry.len() as u32).to_le_bytes());
            comments.extend_from_slice(entry.as_bytes());
        }
        let mut bytes = b"fLaC".to_vec();
        bytes.extend_from_slice(&[0, 0, 0, 34]);
        bytes.extend_from_slice(&stream);
        bytes.push(0x84);
        bytes.extend_from_slice(&(comments.len() as u32).to_be_bytes()[1..]);
        bytes.extend_from_slice(&comments);
        fs::write(path, bytes).unwrap();
    }

    fn prince_revolution_provider_credits() -> [ArtistCredit; 2] {
        [
            ArtistCredit {
                artist_id: "070d193a-845c-479f-980e-bef15710653e".into(),
                name: "Prince".into(),
                credited_name: "Prince".into(),
                role: "album_artist".into(),
                join_phrase: " and ".into(),
                source: "musicbrainz".into(),
            },
            ArtistCredit {
                artist_id: "4c8ead39-b9df-4c56-a27c-51bc049cfd48".into(),
                name: "The Revolution".into(),
                credited_name: "the Revolution".into(),
                role: "album_artist".into(),
                join_phrase: String::new(),
                source: "musicbrainz".into(),
            },
        ]
    }

    fn assert_credit_projections(
        library: &::library::Library,
        album_id: &str,
        track_id: &str,
        display: &str,
        artist_ids: &[String],
    ) {
        let album = library.get_album(album_id).unwrap().unwrap();
        let track = library.get_track(track_id).unwrap().unwrap();
        let album_view = build_album_view(library, album).unwrap();
        let track_views = build_track_views(
            library,
            std::slice::from_ref(&track),
            &HashSet::new(),
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(album_view.artist_name, display);
        assert_eq!(album_view.artist_id, artist_ids[0]);
        assert_eq!(album_view.artist_ids, artist_ids);
        assert_eq!(track_views[0].artist, display);
        assert_eq!(track_views[0].artist_id, artist_ids[0]);
        assert_eq!(track_views[0].artist_ids, artist_ids);

        let metadata = crate::offline_metadata::offline_metadata_for_track(
            library,
            &track,
            &HashSet::new(),
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(metadata.album.artist_name, display);
        assert_eq!(metadata.album.artist_ids, artist_ids);
        assert_eq!(metadata.track.artist, display);
        assert_eq!(metadata.track.artist_ids, artist_ids);
        assert_eq!(metadata.artist.id, artist_ids[0]);
        assert_eq!(
            metadata
                .artists
                .iter()
                .map(|artist| artist.id.clone())
                .collect::<HashSet<_>>(),
            artist_ids.iter().cloned().collect::<HashSet<_>>()
        );
    }

    #[test]
    fn track_and_offline_genres_use_current_enrichment_without_overwriting_tags() {
        let workspace = SampleDirectory(std::env::temp_dir().join(format!(
            "phonolite-genre-projection-{}",
            uuid::Uuid::new_v4()
        )));
        let music = workspace.0.join("music");
        for (album, title, genre) in [
            ("Tagged Album", "Tagged Song", Some("Rock")),
            ("Untagged Album", "Untagged Song", None),
        ] {
            let mut tags = vec![
                ("ALBUM", album),
                ("ALBUMARTIST", "Mira Vale"),
                ("ARTIST", "Mira Vale"),
                ("TITLE", title),
            ];
            if let Some(genre) = genre {
                tags.push(("GENRE", genre));
            }
            write_flac(&music.join("Mira Vale").join(album).join("1.flac"), &tags);
        }
        let library = ::library::Library::load_or_scan(
            vec![::library::LibraryRoot::new(String::new(), music)],
            workspace.0.join("library.redb"),
        )
        .unwrap()
        .0;
        let tracks = library.list_tracks(None, 10, 0).unwrap().0;
        let tagged = tracks
            .iter()
            .find(|track| track.title == "Tagged Song")
            .unwrap();
        let untagged = tracks
            .iter()
            .find(|track| track.title == "Untagged Song")
            .unwrap();
        assert!(untagged.genres.is_empty());
        library
            .update_artist_enrichment(
                &untagged.artist_id,
                None,
                &["Jazz".into()],
                None,
                None,
                true,
            )
            .unwrap();
        let artist_fallback = build_track_views(
            &library,
            std::slice::from_ref(untagged),
            &HashSet::new(),
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(artist_fallback[0].genres, ["Jazz"]);

        for track in &tracks {
            library
                .update_album_enrichment(&track.album_id, None, &["Electronic".into()], true)
                .unwrap();
        }
        let projected =
            build_track_views(&library, &tracks, &HashSet::new(), &HashSet::new()).unwrap();
        assert_eq!(
            projected
                .iter()
                .find(|view| view.id == tagged.id)
                .unwrap()
                .genres,
            ["Rock"]
        );
        assert_eq!(
            projected
                .iter()
                .find(|view| view.id == untagged.id)
                .unwrap()
                .genres,
            ["Electronic"]
        );
        // Enrichment stays in its own catalog record; the current projection is
        // shared by online playback and downloadable metadata snapshots.
        assert!(library
            .get_track(&untagged.id)
            .unwrap()
            .unwrap()
            .genres
            .is_empty());
        let offline = crate::offline_metadata::offline_metadata_for_track(
            &library,
            untagged,
            &HashSet::new(),
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(offline.track.genres, ["Electronic"]);
    }

    #[test]
    fn listening_genres_prefer_current_release_metadata_and_preserve_song_tags() {
        let workspace = SampleDirectory(std::env::temp_dir().join(format!(
            "phonolite-listening-genres-{}",
            uuid::Uuid::new_v4()
        )));
        let music = workspace.0.join("music");
        for (file, title) in [("1.flac", "Virtual Insanity"), ("2.flac", "Cosmic Girl")] {
            write_flac(
                &music
                    .join("Jamiroquai")
                    .join("Travelling Without Moving")
                    .join(file),
                &[
                    ("ALBUM", "Travelling Without Moving"),
                    ("ALBUMARTIST", "Jamiroquai"),
                    ("ARTIST", "Jamiroquai"),
                    ("TITLE", title),
                    ("GENRE", "Rock;Pop"),
                ],
            );
        }
        let library = ::library::Library::load_or_scan(
            vec![::library::LibraryRoot::new(String::new(), music)],
            workspace.0.join("library.redb"),
        )
        .unwrap()
        .0;
        let tracks = library.list_tracks(None, 10, 0).unwrap().0;
        assert_eq!(tracks.len(), 2);
        let track = &tracks[0];
        library
            .update_artist_enrichment(
                &track.artist_id,
                None,
                &[
                    "acid jazz".into(),
                    "disco".into(),
                    "funk".into(),
                    "house".into(),
                    "r&b".into(),
                ],
                None,
                None,
                true,
            )
            .unwrap();
        library
            .update_album_enrichment(
                &track.album_id,
                None,
                &["acid jazz".into(), "Electronic".into(), "funk".into()],
                true,
            )
            .unwrap();
        let views = build_track_views(&library, &tracks, &HashSet::new(), &HashSet::new()).unwrap();
        for view in &views {
            assert_eq!(view.genres, ["Rock", "Pop"]);
            assert_eq!(view.listening_genres, ["Acid Jazz", "Electronic", "Funk"]);
        }
        assert_eq!(
            library.get_track(&track.id).unwrap().unwrap().genres,
            ["Rock", "Pop"]
        );

        let offline = crate::offline_metadata::offline_metadata_for_track(
            &library,
            track,
            &HashSet::new(),
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(offline.track.genres, ["Rock", "Pop"]);
        assert_eq!(offline.track.listening_genres, views[0].listening_genres);
        let restored: crate::offline_metadata::OfflineTrackMetadata =
            serde_json::from_value(serde_json::to_value(&offline).unwrap()).unwrap();
        assert_eq!(
            restored.track.listening_genres,
            ["Acid Jazz", "Electronic", "Funk"]
        );

        let mut legacy = serde_json::to_value(&views[0]).unwrap();
        legacy.as_object_mut().unwrap().remove("listening_genres");
        let restored: TrackView = serde_json::from_value(legacy).unwrap();
        assert_eq!(restored.genres, ["Rock", "Pop"]);
        assert!(restored.listening_genres.is_empty());

        // Projection caches last for one response. A later metadata update must
        // be reflected on every sibling track in the next request.
        library
            .update_album_enrichment(&track.album_id, None, &["Soul".into()], true)
            .unwrap();
        let refreshed =
            build_track_views(&library, &tracks, &HashSet::new(), &HashSet::new()).unwrap();
        assert!(refreshed
            .iter()
            .all(|view| view.listening_genres == ["Soul"]));
        assert!(refreshed.iter().all(|view| view.genres == ["Rock", "Pop"]));
    }

    #[test]
    fn verified_artist_display_is_normalized_in_api_and_offline_metadata() {
        let workspace = SampleDirectory(std::env::temp_dir().join(format!(
            "phonolite-verified-display-api-{}",
            uuid::Uuid::new_v4()
        )));
        let music = workspace.0.join("music");
        write_flac(
            &music.join("Prince/Purple Rain/1.flac"),
            &[
                ("ALBUM", "Purple Rain"),
                ("ALBUMARTIST", "Prince, Prince and the Revolution"),
                ("ARTIST", "Prince, Prince and the Revolution"),
                ("TITLE", "Purple Rain"),
            ],
        );
        let library = ::library::Library::load_or_scan(
            vec![::library::LibraryRoot::new(String::new(), music)],
            workspace.0.join("library.redb"),
        )
        .unwrap()
        .0;
        let album = library.list_albums(None, 10, 0).unwrap().0.remove(0);
        let track_id = library.get_album_tracks(&album.id).unwrap()[0].id.clone();
        let combined_id = artist_identity_id("Prince, Prince and the Revolution");
        assert_eq!(album.artist_id, combined_id);
        assert!(library
            .resolve_album_display_credit(&album.id, &prince_revolution_provider_credits())
            .unwrap());
        assert!(library.get_artist(&combined_id).unwrap().is_none());
        assert_credit_projections(
            &library,
            &album.id,
            &track_id,
            "Prince and the Revolution",
            &[
                artist_identity_id("Prince"),
                artist_identity_id("The Revolution"),
            ],
        );
    }

    #[test]
    fn local_structured_artist_display_is_normalized_without_remote_resolution() {
        let workspace = SampleDirectory(std::env::temp_dir().join(format!(
            "phonolite-local-display-api-{}",
            uuid::Uuid::new_v4()
        )));
        let music = workspace.0.join("music");
        write_flac(
            &music.join("Prince/Unmatched Edition/1.flac"),
            &[
                ("ALBUM", "Unmatched Edition"),
                ("ALBUMARTIST", "Prince, Prince and the Revolution"),
                ("ALBUMARTISTS", "Prince"),
                ("ALBUMARTISTS", "The Revolution"),
                ("ARTIST", "Prince, Prince and the Revolution"),
                ("ARTISTS", "Prince"),
                ("ARTISTS", "The Revolution"),
                ("TITLE", "A Local Track"),
            ],
        );
        let library = ::library::Library::load_or_scan(
            vec![::library::LibraryRoot::new(String::new(), music)],
            workspace.0.join("library.redb"),
        )
        .unwrap()
        .0;
        let album = library.list_albums(None, 10, 0).unwrap().0.remove(0);
        let track_id = library.get_album_tracks(&album.id).unwrap()[0].id.clone();
        assert!(library
            .get_artist(&artist_identity_id("Prince, Prince and the Revolution"))
            .unwrap()
            .is_none());
        assert_credit_projections(
            &library,
            &album.id,
            &track_id,
            "Prince and the Revolution",
            &[
                artist_identity_id("Prince"),
                artist_identity_id("The Revolution"),
            ],
        );
    }

    #[test]
    fn explicit_single_artist_identity_keeps_punctuation_in_api_and_offline_metadata() {
        let workspace = SampleDirectory(std::env::temp_dir().join(format!(
            "phonolite-explicit-display-api-{}",
            uuid::Uuid::new_v4()
        )));
        let music = workspace.0.join("music");
        let artist = "Prince, Prince and the Revolution";
        write_flac(
            &music.join("Explicit Identity/Album/1.flac"),
            &[
                ("ALBUM", "Explicit Identity"),
                ("ALBUMARTIST", artist),
                ("ALBUMARTISTS", artist),
                ("ARTIST", artist),
                ("ARTISTS", artist),
                ("TITLE", "A Local Track"),
            ],
        );
        let library = ::library::Library::load_or_scan(
            vec![::library::LibraryRoot::new(String::new(), music)],
            workspace.0.join("library.redb"),
        )
        .unwrap()
        .0;
        let album = library.list_albums(None, 10, 0).unwrap().0.remove(0);
        let track_id = library.get_album_tracks(&album.id).unwrap()[0].id.clone();
        assert!(!library
            .resolve_album_display_credit(&album.id, &prince_revolution_provider_credits())
            .unwrap());
        assert_credit_projections(
            &library,
            &album.id,
            &track_id,
            artist,
            &[artist_identity_id(artist)],
        );
    }

    #[test]
    fn verified_joint_artist_is_retained_in_album_and_offline_metadata() {
        let workspace = SampleDirectory(std::env::temp_dir().join(format!(
            "phonolite-verified-credit-{}",
            uuid::Uuid::new_v4()
        )));
        let music = workspace.0.join("music");
        write_flac(
            &music.join("Mira Vale/Shared Horizon/1.flac"),
            &[
                ("ALBUM", "Shared Horizon"),
                ("ALBUMARTIST", "Mira Vale"),
                ("ARTIST", "Mira Vale"),
                ("TITLE", "Horizon"),
            ],
        );
        let library = ::library::Library::load_or_scan(
            vec![::library::LibraryRoot::new(String::new(), music)],
            workspace.0.join("library.redb"),
        )
        .unwrap()
        .0;
        let primary = library
            .get_artist(&artist_identity_id("Mira Vale"))
            .unwrap()
            .unwrap();
        let joint = Artist {
            id: artist_identity_id("Theo Ellis"),
            name: "Theo Ellis".to_string(),
            genres: vec!["Jazz".to_string()],
            summary: Some("Theo's own biography".to_string()),
            logo_ref: Some("theo.jpg".to_string()),
            banner_ref: None,
        };
        let album = library.list_albums(None, 10, 0).unwrap().0.remove(0);
        assert!(library
            .merge_album_artists(&album.id, &[primary.clone(), joint.clone()])
            .unwrap());
        let updated = library.get_album(&album.id).unwrap().unwrap();
        let view = build_album_view(&library, updated).unwrap();
        assert_eq!(view.artist_ids, [primary.id.clone(), joint.id.clone()]);
        assert_eq!(view.artist_name, "Mira Vale");
        assert!(view.artist_credits.iter().any(|credit| {
            credit.artist_id == joint.id
                && credit.role == "album_artist"
                && credit.source == "verified_album_metadata"
        }));
        let track = library.list_tracks(None, 10, 0).unwrap().0.remove(0);
        let metadata = crate::offline_metadata::offline_metadata_for_track(
            &library,
            &track,
            &HashSet::new(),
            &HashSet::new(),
        )
        .unwrap();
        let contributor = metadata
            .artists
            .iter()
            .find(|artist| artist.id == joint.id)
            .unwrap();
        assert_eq!(contributor.summary, joint.summary);
        assert_eq!(contributor.logo_ref, joint.logo_ref);
        assert_eq!(contributor.album_count, 0);
        assert_eq!(contributor.collab_album_count, 1);
        assert_eq!(contributor.release_count, Some(1));
        assert_eq!(metadata.album.artist_ids, view.artist_ids);
    }

    #[test]
    fn artist_profiles_and_playback_keep_guest_scope_and_shared_album_identity() {
        let workspace = SampleDirectory(
            std::env::temp_dir().join(format!("phonolite-credit-api-{}", uuid::Uuid::new_v4())),
        );
        let music = workspace.0.join("music");
        write_flac(
            &music.join("Mira Vale/Night Windows/1.flac"),
            &[
                ("ALBUM", "Night Windows"),
                ("ALBUMARTIST", "Mira Vale & Theo Ellis"),
                ("ALBUMARTISTS", "Mira Vale"),
                ("ALBUMARTISTS", "Theo Ellis"),
                ("ARTIST", "Mira Vale & Theo Ellis"),
                ("ARTISTS", "Mira Vale"),
                ("ARTISTS", "Theo Ellis"),
                ("TITLE", "Stillwater"),
                ("TRACKNUMBER", "1"),
            ],
        );
        write_flac(
            &music.join("Mira Vale/Night Windows/2.flac"),
            &[
                ("ALBUM", "Night Windows"),
                ("ALBUMARTIST", "Mira Vale & Theo Ellis"),
                ("ALBUMARTISTS", "Mira Vale"),
                ("ALBUMARTISTS", "Theo Ellis"),
                ("ARTIST", "Mira Vale"),
                ("TITLE", "Mira's Interlude"),
                ("TRACKNUMBER", "2"),
            ],
        );
        write_flac(
            &music.join("Mira Vale/Riverlight/1.flac"),
            &[
                ("ALBUM", "Riverlight"),
                ("ALBUMARTIST", "Mira Vale"),
                ("ARTIST", "Mira Vale"),
                ("TITLE", "Riverlight"),
                ("TRACKNUMBER", "1"),
            ],
        );
        write_flac(
            &music.join("Mira Vale/Riverlight/2.flac"),
            &[
                ("ALBUM", "Riverlight"),
                ("ALBUMARTIST", "Mira Vale"),
                ("ARTIST", "Mira Vale feat. Theo Ellis"),
                ("ARTISTS", "Mira Vale"),
                ("ARTISTS", "Theo Ellis"),
                ("TITLE", "Paper Moon"),
                ("TRACKNUMBER", "2"),
                ("COMPOSER", "Guest Composer"),
            ],
        );
        let library = ::library::Library::load_or_scan(
            vec![::library::LibraryRoot::new(String::new(), music)],
            workspace.0.join("library.redb"),
        )
        .unwrap()
        .0;
        let mira = artist_identity_id("Mira Vale");
        let theo = artist_identity_id("Theo Ellis");
        let composer = artist_identity_id("Guest Composer");
        let mira_releases = build_artist_releases_view(&library, &mira).unwrap();
        let theo_releases = build_artist_releases_view(&library, &theo).unwrap();
        assert_eq!(mira_releases.albums.len(), 1);
        assert!(theo_releases.albums.is_empty());
        assert_eq!(theo_releases.collab_albums.len(), 1);
        let joint = theo_releases
            .collab_albums
            .iter()
            .find(|entry| entry.album.title == "Night Windows")
            .unwrap();
        let joint = &joint.album;
        assert_eq!(joint.artist_ids, [mira.clone(), theo.clone()]);
        assert_eq!(joint.artist_name, "Mira Vale & Theo Ellis");
        assert!(mira_releases
            .collab_albums
            .iter()
            .any(|entry| entry.album.id == joint.id));
        assert_eq!(theo_releases.appears_on.len(), 1);
        let appearance = &theo_releases.appears_on[0];
        assert_eq!(appearance.album.title, "Riverlight");
        assert_eq!(appearance.album.artist_ids, [mira.clone()]);
        assert_eq!(appearance.track_titles, ["Paper Moon"]);
        assert_eq!(appearance.roles, ["featured_artist"]);
        let track = library
            .get_track(&appearance.track_ids[0])
            .unwrap()
            .unwrap();
        let offline = crate::offline_metadata::offline_metadata_for_track(
            &library,
            &track,
            &HashSet::new(),
            &HashSet::new(),
        )
        .unwrap();
        assert!(!offline.artists.iter().any(|artist| artist.id == composer));
        assert!(offline
            .track
            .artist_credits
            .iter()
            .any(|credit| credit.artist_id == composer && credit.role == "composer"));
        let views =
            build_track_views(&library, &[track], &HashSet::new(), &HashSet::new()).unwrap();
        assert_eq!(views[0].artist, "Mira Vale feat. Theo Ellis");
        assert_eq!(views[0].artist_ids, [mira, theo.clone()]);
        assert!(views[0]
            .artist_credits
            .iter()
            .any(|credit| credit.artist_id == composer && credit.role == "composer"));
        let default_artists = library
            .list_artists_for_browse(None, 60, 0, false)
            .unwrap()
            .0;
        assert!(!default_artists.iter().any(|artist| artist.id == composer));
        assert!(library
            .list_artists_for_browse(Some("Guest Composer"), 60, 0, false)
            .unwrap()
            .0
            .is_empty());
        let searched = library
            .list_artists_for_browse(Some("Guest Composer"), 60, 0, true)
            .unwrap()
            .0;
        assert_eq!(searched.len(), 1);
        let composer_view = build_artist_view(&library, searched[0].clone()).unwrap();
        assert_eq!(composer_view.album_count, 0);
        assert_eq!(composer_view.other_credit_count, 1);
        assert_eq!(composer_view.collab_album_count, 0);
        assert_eq!(composer_view.release_count, Some(0));
        assert_eq!(
            serde_json::to_value(&composer_view).unwrap()["release_count"],
            0
        );
        let theo_view =
            build_artist_views(&library, vec![library.get_artist(&theo).unwrap().unwrap()])
                .unwrap()
                .pop()
                .unwrap();
        assert_eq!(theo_view.album_count, 0);
        assert_eq!(theo_view.collab_album_count, 1);
        assert_eq!(theo_view.appearance_count, 1);
        assert_eq!(theo_view.release_count, Some(2));
        let browse_views = build_artist_views(&library, default_artists).unwrap();
        assert!(browse_views
            .iter()
            .all(|artist| artist.release_count.unwrap() > 0));
        let mira_view = browse_views
            .iter()
            .find(|artist| artist.id == artist_identity_id("Mira Vale"))
            .unwrap();
        assert_eq!(mira_view.album_count, 1);
        assert_eq!(mira_view.release_count, Some(2));
        let queue = crate::shuffle::artist_tracks(&library, &theo).unwrap();
        let titles = queue
            .iter()
            .map(|track| track.title.as_str())
            .collect::<HashSet<_>>();
        assert_eq!(
            titles,
            HashSet::from(["Stillwater", "Mira's Interlude", "Paper Moon"])
        );
        let downloads = crate::shuffle::artist_album_tracks(&library, &theo).unwrap();
        assert_eq!(
            downloads
                .iter()
                .map(|track| track.title.as_str())
                .collect::<HashSet<_>>(),
            HashSet::from(["Stillwater", "Mira's Interlude"])
        );
        let custom = crate::shuffle::build_shuffle_queue(
            &library,
            crate::shuffle::ShuffleMode::Custom,
            None,
            None,
            &[theo],
            &[],
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(custom.len(), 3);

        let metadata = crate::offline_metadata::offline_metadata_for_track(
            &library,
            &library
                .get_track(&appearance.track_ids[0])
                .unwrap()
                .unwrap(),
            &HashSet::new(),
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(metadata.schema_version, 6);
        assert_eq!(
            metadata
                .artists
                .iter()
                .map(|artist| artist.id.clone())
                .collect::<HashSet<_>>(),
            HashSet::from([
                artist_identity_id("Mira Vale"),
                artist_identity_id("Theo Ellis"),
            ])
        );
    }
}
