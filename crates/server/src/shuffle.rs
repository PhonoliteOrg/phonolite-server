use std::collections::{HashMap, HashSet};

use common::Track;
use library::{Library, LibraryError};
use rand::seq::SliceRandom;

#[derive(Clone, Copy, Debug)]
pub enum ShuffleMode {
    All,
    Artist,
    Album,
    Custom,
    Liked,
}

impl ShuffleMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "all" | "any" | "random" => Some(Self::All),
            "artist" => Some(Self::Artist),
            "album" => Some(Self::Album),
            "custom" => Some(Self::Custom),
            "liked" | "likes" => Some(Self::Liked),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum ShuffleError {
    MissingArtistId,
    MissingAlbumId,
    Library(LibraryError),
}

impl From<LibraryError> for ShuffleError {
    fn from(err: LibraryError) -> Self {
        ShuffleError::Library(err)
    }
}

pub fn build_shuffle_queue(
    library: &Library,
    mode: ShuffleMode,
    artist_id: Option<&str>,
    album_id: Option<&str>,
    custom_artist_ids: &[String],
    custom_genres: &[String],
    liked_set: &HashSet<String>,
) -> Result<Vec<Track>, ShuffleError> {
    let mut tracks = match mode {
        ShuffleMode::All => {
            let (tracks, _) = library.list_tracks(None, usize::MAX, 0)?;
            tracks
        }
        ShuffleMode::Artist => {
            let artist_id = artist_id.ok_or(ShuffleError::MissingArtistId)?;
            artist_tracks(library, artist_id)?
        }
        ShuffleMode::Album => {
            let album_id = album_id.ok_or(ShuffleError::MissingAlbumId)?;
            library.get_album_tracks(album_id)?
        }
        ShuffleMode::Custom => {
            let (tracks, _) = library.list_tracks(None, usize::MAX, 0)?;
            let artist_filter: HashSet<&str> =
                custom_artist_ids.iter().map(|id| id.as_str()).collect();
            let genre_filter: HashSet<String> = custom_genres
                .iter()
                .map(|genre| genre.trim().to_ascii_lowercase())
                .filter(|genre| !genre.is_empty())
                .collect();

            let filter_artists = !artist_filter.is_empty();
            let filter_genres = !genre_filter.is_empty();

            if !filter_artists && !filter_genres {
                tracks
            } else {
                let mut album_genres_cache: HashMap<String, Vec<String>> = HashMap::new();
                let mut artist_genres_cache: HashMap<String, Vec<String>> = HashMap::new();
                let mut album_artist_cache: HashMap<String, Vec<String>> = HashMap::new();
                let mut filtered = Vec::new();

                for track in tracks {
                    let mut scoped_artist_ids = None;
                    let matches_artist = if filter_artists {
                        let ids = credited_artist_ids(library, &track, &mut album_artist_cache)?;
                        let matched = ids.iter().any(|id| artist_filter.contains(id.as_str()));
                        scoped_artist_ids = Some(ids);
                        matched
                    } else {
                        false
                    };
                    let matches_genre = if filter_genres {
                        if matches_genres_raw(&track.genres, &genre_filter) {
                            true
                        } else {
                            let album_genres = cached_album_genres(
                                library,
                                &track.album_id,
                                &mut album_genres_cache,
                            )?;
                            if matches_genres_normalized(&album_genres, &genre_filter) {
                                true
                            } else {
                                let artist_ids = match scoped_artist_ids {
                                    Some(ids) => ids,
                                    None => credited_artist_ids(
                                        library,
                                        &track,
                                        &mut album_artist_cache,
                                    )?,
                                };
                                let mut matched = false;
                                for artist_id in artist_ids {
                                    let artist_genres = cached_artist_genres(
                                        library,
                                        &artist_id,
                                        &mut artist_genres_cache,
                                    )?;
                                    if matches_genres_normalized(&artist_genres, &genre_filter) {
                                        matched = true;
                                        break;
                                    }
                                }
                                matched
                            }
                        }
                    } else {
                        false
                    };

                    let include = match (filter_artists, filter_genres) {
                        (true, true) => matches_artist || matches_genre,
                        (true, false) => matches_artist,
                        (false, true) => matches_genre,
                        (false, false) => true,
                    };

                    if include {
                        filtered.push(track);
                    }
                }

                filtered
            }
        }
        ShuffleMode::Liked => {
            let (tracks, _) = library.list_tracks(None, usize::MAX, 0)?;
            tracks
                .into_iter()
                .filter(|track| liked_set.contains(&track.id))
                .collect()
        }
    };

    let mut rng = rand::rng();
    tracks.shuffle(&mut rng);
    Ok(tracks)
}

/// Album scope for the profile's Download artist action.
pub fn artist_album_tracks(library: &Library, artist_id: &str) -> Result<Vec<Track>, LibraryError> {
    let releases = library.artist_release_relationships(artist_id)?;
    let mut tracks = Vec::new();
    let mut seen = HashSet::new();
    let albums = releases.albums.into_iter().chain(
        releases
            .collab_albums
            .into_iter()
            .filter(|contribution| contribution.roles.iter().any(|role| role == "album_artist"))
            .map(|contribution| contribution.album),
    );
    for album in albums {
        let mut album_tracks = library.get_album_tracks(&album.id)?;
        album_tracks.retain(|track| seen.insert(track.id.clone()));
        tracks.append(&mut album_tracks);
    }
    Ok(tracks)
}

/// Song scope for artist playback. Guest and contributor
/// releases supply only the specifically credited songs.
pub fn artist_tracks(library: &Library, artist_id: &str) -> Result<Vec<Track>, LibraryError> {
    let releases = library.artist_release_relationships(artist_id)?;
    let mut tracks = Vec::new();
    let mut seen = HashSet::new();
    for album in releases.albums {
        let mut album_tracks = library.get_album_tracks(&album.id)?;
        album_tracks.retain(|track| seen.insert(track.id.clone()));
        tracks.append(&mut album_tracks);
    }
    for contribution in releases
        .collab_albums
        .into_iter()
        .chain(releases.appears_on)
        .chain(releases.other_credits)
    {
        for id in contribution.track_ids {
            if seen.insert(id.clone()) {
                if let Some(track) = library.get_track(&id)? {
                    tracks.push(track);
                }
            }
        }
    }
    tracks.sort_by(|a, b| {
        a.album_id
            .cmp(&b.album_id)
            .then_with(|| a.disc_no.unwrap_or(0).cmp(&b.disc_no.unwrap_or(0)))
            .then_with(|| a.track_no.unwrap_or(0).cmp(&b.track_no.unwrap_or(0)))
            .then_with(|| a.title.cmp(&b.title))
    });
    Ok(tracks)
}

fn normalize_genres(genres: &[String]) -> Vec<String> {
    genres
        .iter()
        .map(|genre| genre.trim().to_ascii_lowercase())
        .filter(|genre| !genre.is_empty())
        .collect()
}

fn matches_genres_raw(genres: &[String], filter: &HashSet<String>) -> bool {
    genres.iter().any(|genre| {
        let normalized = genre.trim().to_ascii_lowercase();
        !normalized.is_empty() && filter.contains(&normalized)
    })
}

fn matches_genres_normalized(genres: &[String], filter: &HashSet<String>) -> bool {
    genres.iter().any(|genre| filter.contains(genre))
}

fn credited_artist_ids(
    library: &Library,
    track: &Track,
    album_cache: &mut HashMap<String, Vec<String>>,
) -> Result<Vec<String>, ShuffleError> {
    let mut ids = vec![track.artist_id.clone()];
    for credit in library.track_credits(&track.id)?.credits {
        if credit.is_billed_artist() && !ids.contains(&credit.artist_id) {
            ids.push(credit.artist_id);
        }
    }
    if !album_cache.contains_key(&track.album_id) {
        let mut album_ids = library
            .album_credits(&track.album_id)?
            .credits
            .into_iter()
            .filter(|credit| credit.role == "album_artist")
            .map(|credit| credit.artist_id)
            .collect::<Vec<_>>();
        // A verified joint release applies to all its tracks; a featured or
        // song guest above applies only to the specifically credited song.
        if let Some(album) = library.get_album(&track.album_id)? {
            for id in album.all_artist_ids() {
                if !album_ids.contains(id) {
                    album_ids.push(id.clone());
                }
            }
        }
        album_cache.insert(track.album_id.clone(), album_ids);
    }
    for id in &album_cache[&track.album_id] {
        if !ids.contains(id) {
            ids.push(id.clone());
        }
    }
    Ok(ids)
}

fn cached_album_genres(
    library: &Library,
    album_id: &str,
    cache: &mut HashMap<String, Vec<String>>,
) -> Result<Vec<String>, ShuffleError> {
    if let Some(genres) = cache.get(album_id) {
        return Ok(genres.clone());
    }
    let genres = match library.get_album(album_id)? {
        Some(album) => normalize_genres(&album.genres),
        None => Vec::new(),
    };
    cache.insert(album_id.to_string(), genres.clone());
    Ok(genres)
}

fn cached_artist_genres(
    library: &Library,
    artist_id: &str,
    cache: &mut HashMap<String, Vec<String>>,
) -> Result<Vec<String>, ShuffleError> {
    if let Some(genres) = cache.get(artist_id) {
        return Ok(genres.clone());
    }
    let genres = match library.get_artist(artist_id)? {
        Some(artist) => normalize_genres(&artist.genres),
        None => Vec::new(),
    };
    cache.insert(artist_id.to_string(), genres.clone());
    Ok(genres)
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::{artist_identity_id, Artist};
    use library::LibraryRoot;
    use std::{fs, path::PathBuf};

    struct Fixture(PathBuf);

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn verified_joint_album_artist_is_in_playback_downloads_and_custom_shuffle() {
        let fixture = Fixture(std::env::temp_dir().join(format!(
            "phonolite-verified-artist-shuffle-{}",
            uuid::Uuid::new_v4()
        )));
        let music = fixture.0.join("music");
        let album_dir = music.join("Main/Shared");
        fs::create_dir_all(&album_dir).unwrap();
        // Folder fallback supplies local identity; no codec or application is
        // needed to verify a provider credit extending that identity.
        for name in ["First.mp3", "Second.mp3"] {
            fs::write(album_dir.join(name), b"catalog scope fixture").unwrap();
        }
        let (library, _) = Library::load_or_scan(
            vec![LibraryRoot::new(String::new(), music)],
            fixture.0.join("library.redb"),
        )
        .unwrap();
        let album = library.list_albums(None, 1, 0).unwrap().0.remove(0);
        let primary = library.get_artist(&album.artist_id).unwrap().unwrap();
        let joint = Artist {
            id: artist_identity_id("Verified Collaborator"),
            name: "Verified Collaborator".into(),
            genres: vec!["Jazz".into()],
            summary: None,
            logo_ref: None,
            banner_ref: None,
        };
        library
            .merge_album_artists(&album.id, &[primary, joint.clone()])
            .unwrap();
        assert!(!library
            .album_credits(&album.id)
            .unwrap()
            .credits
            .iter()
            .any(|credit| credit.artist_id == joint.id));
        let expected: HashSet<_> = library
            .get_album_tracks(&album.id)
            .unwrap()
            .into_iter()
            .map(|track| track.id)
            .collect();
        assert_eq!(expected.len(), 2);
        let queue = artist_tracks(&library, &joint.id).unwrap();
        assert_eq!(
            queue
                .into_iter()
                .map(|track| track.id)
                .collect::<HashSet<_>>(),
            expected
        );
        let downloads = artist_album_tracks(&library, &joint.id).unwrap();
        assert_eq!(
            downloads
                .into_iter()
                .map(|track| track.id)
                .collect::<HashSet<_>>(),
            expected
        );
        let custom = build_shuffle_queue(
            &library,
            ShuffleMode::Custom,
            None,
            None,
            &[joint.id],
            &[],
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(
            custom
                .into_iter()
                .map(|track| track.id)
                .collect::<HashSet<_>>(),
            expected
        );
        let jazz = build_shuffle_queue(
            &library,
            ShuffleMode::Custom,
            None,
            None,
            &[],
            &[" jazz ".into()],
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(
            jazz.into_iter()
                .map(|track| track.id)
                .collect::<HashSet<_>>(),
            expected
        );
    }

    fn write_flac(path: &std::path::Path, tags: &[(&str, &str)]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut stream = [0u8; 34];
        stream[..4].copy_from_slice(&[0x10, 0, 0x10, 0]);
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

    #[test]
    fn custom_genres_use_song_billing_without_supporting_role_metadata() {
        let fixture = Fixture(std::env::temp_dir().join(format!(
            "phonolite-guest-genre-shuffle-{}",
            uuid::Uuid::new_v4()
        )));
        let music = fixture.0.join("music");
        for (name, artist, composer) in [
            ("Guest Song", "Main feat. Guest", ""),
            ("Composer Song", "Main", "Composer"),
            ("Solo Song", "Main", ""),
        ] {
            write_flac(
                &music.join(format!("Main/Album/{name}.flac")),
                &[
                    ("ALBUM", "Album"),
                    ("ALBUMARTIST", "Main"),
                    ("ARTIST", artist),
                    ("TITLE", name),
                    ("COMPOSER", composer),
                ],
            );
        }
        let (library, _) = Library::load_or_scan(
            vec![LibraryRoot::new(String::new(), music)],
            fixture.0.join("library.redb"),
        )
        .unwrap();
        for artist in ["Guest", "Composer"] {
            library
                .update_artist_enrichment(
                    &artist_identity_id(artist),
                    None,
                    &["Jazz".into()],
                    None,
                    None,
                    false,
                )
                .unwrap();
        }
        let jazz = build_shuffle_queue(
            &library,
            ShuffleMode::Custom,
            None,
            None,
            &[],
            &["Jazz".into()],
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(
            jazz.iter()
                .map(|track| track.title.as_str())
                .collect::<HashSet<_>>(),
            HashSet::from(["Guest Song"])
        );
        assert!(library
            .get_artist(&artist_identity_id("Main"))
            .unwrap()
            .unwrap()
            .genres
            .is_empty());
        assert!(library.list_albums(None, 1, 0).unwrap().0[0]
            .genres
            .is_empty());
    }
}
