//! Build a complete staged catalog from parsed tags and filesystem metadata.
//!
//! Identity, duplicate precedence, multi-disc grouping, and their index writes stay
//! together so a catalog is always produced as one coherent transaction.
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use common::{
    artist_identity_id, artist_identity_key, relpath_from, stable_id, Album, Artist, ArtistCredits,
    Codec, CoverRef, SeekIndex, SeekPoint, Track,
};
use metadata::TagInfo;
use redb::{Database, ReadableTable};
use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::credits::{resolved_credit_names, source_credits, track_credit_search_text};
use crate::normalization::{clean_summary, merge_genres};
use crate::storage::{
    album_index_key, album_name_key, album_track_key, artist_name_key, bool_bytes, clear_table,
    decode_value, encode_value, track_name_key, ALBUMS_BY_NAME_TABLE, ALBUMS_TABLE,
    ALBUM_CREDITS_TABLE, ALBUM_TRACKS_TABLE, ARTISTS_BY_NAME_TABLE, ARTISTS_TABLE,
    ARTIST_ALBUMS_TABLE, EXTERNAL_ATTEMPTS_TABLE, INDEX_VERSION, KEY_SEP, META_STATS_KEY,
    META_TABLE, META_VERSION_KEY, SEEK_TABLE, TAG_ERRORS_TABLE, TAG_ERROR_FILES_TABLE,
    TRACKS_BY_NAME_TABLE, TRACKS_TABLE, TRACK_CREDITS_TABLE, TRACK_EMBEDDED_COVER_TABLE,
};
use crate::{
    now_secs, prefix_relpath, LibraryError, LibraryRoot, LibraryStats, ScanObserver, ScanProgress,
    TagErrorFile, TagErrorInfo,
};

const SEEK_STEP_MS: u32 = 5000;

pub(super) fn build_catalog(
    roots: &[LibraryRoot],
    db: &Database,
    tags: &HashMap<PathBuf, Result<TagInfo, String>>,
    file_sizes: &HashMap<PathBuf, u64>,
    resolutions: &HashMap<String, ArtistCredits>,
    progress: Option<ScanObserver<'_>>,
) -> Result<LibraryStats, LibraryError> {
    // Resolve ambiguous display joins against identities from the complete
    // inventory so scan order cannot turn a known band into multiple artists.
    let mut known_artists = HashSet::new();
    known_artists.extend(resolutions.values().flat_map(|resolved| {
        resolved
            .credits
            .iter()
            .map(|credit| artist_identity_key(&credit.name))
    }));
    for tag in tags.values().filter_map(|tag| tag.as_ref().ok()) {
        for (display, names, structured) in [
            (tag.artist.as_deref(), &tag.artists, tag.artists_structured),
            (
                tag.album_artist.as_deref(),
                &tag.album_artists,
                tag.album_artists_structured,
            ),
        ] {
            if structured || names.len() > 1 {
                known_artists.extend(names.iter().map(|name| artist_identity_key(name)));
            } else if let Some(name) = display.or_else(|| names.first().map(String::as_str)) {
                if unambiguous_identity(name) {
                    known_artists.insert(artist_identity_key(name));
                }
            }
        }
    }
    let mut album_dirs = Vec::new();
    for root in roots {
        album_dirs.extend(
            collect_album_dirs(&root.path, tags.keys())
                .into_iter()
                .map(|dir| (root, dir)),
        );
    }
    info!("Found {} album folders", album_dirs.len());
    let total_albums = album_dirs.len();
    if let Some(progress) = progress {
        progress(ScanProgress::IndexingAlbums {
            completed: 0,
            total: total_albums,
        });
    }
    for (_, dir) in &album_dirs {
        if let Some(name) = dir
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
        {
            if unambiguous_identity(name) {
                known_artists.insert(artist_identity_key(name));
            }
        }
    }
    let mut files_by_dir: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for path in tags.keys() {
        for parent in path.ancestors().skip(1).take(2) {
            files_by_dir
                .entry(parent.to_path_buf())
                .or_default()
                .push(path.clone());
        }
    }
    for files in files_by_dir.values_mut() {
        files.sort();
    }

    let write_txn = db.begin_write()?;

    clear_table(&write_txn, META_TABLE)?;
    clear_table(&write_txn, ARTISTS_TABLE)?;
    clear_table(&write_txn, ARTISTS_BY_NAME_TABLE)?;
    clear_table(&write_txn, ALBUMS_TABLE)?;
    clear_table(&write_txn, ALBUMS_BY_NAME_TABLE)?;
    clear_table(&write_txn, ARTIST_ALBUMS_TABLE)?;
    clear_table(&write_txn, TRACKS_TABLE)?;
    clear_table(&write_txn, TRACKS_BY_NAME_TABLE)?;
    clear_table(&write_txn, ALBUM_TRACKS_TABLE)?;
    clear_table(&write_txn, TRACK_EMBEDDED_COVER_TABLE)?;
    clear_table(&write_txn, SEEK_TABLE)?;
    clear_table(&write_txn, EXTERNAL_ATTEMPTS_TABLE)?;
    clear_table(&write_txn, TAG_ERRORS_TABLE)?;
    clear_table(&write_txn, TAG_ERROR_FILES_TABLE)?;
    clear_table(&write_txn, ALBUM_CREDITS_TABLE)?;
    clear_table(&write_txn, TRACK_CREDITS_TABLE)?;

    let stats = {
        let mut meta_table = write_txn.open_table(META_TABLE)?;
        let mut artists_table = write_txn.open_table(ARTISTS_TABLE)?;
        let mut artists_by_name_table = write_txn.open_table(ARTISTS_BY_NAME_TABLE)?;
        let mut albums_table = write_txn.open_table(ALBUMS_TABLE)?;
        let mut albums_by_name_table = write_txn.open_table(ALBUMS_BY_NAME_TABLE)?;
        let mut artist_albums_table = write_txn.open_table(ARTIST_ALBUMS_TABLE)?;
        let mut tracks_table = write_txn.open_table(TRACKS_TABLE)?;
        let mut tracks_by_name_table = write_txn.open_table(TRACKS_BY_NAME_TABLE)?;
        let mut album_tracks_table = write_txn.open_table(ALBUM_TRACKS_TABLE)?;
        let mut embedded_cover_table = write_txn.open_table(TRACK_EMBEDDED_COVER_TABLE)?;
        let mut seek_table = write_txn.open_table(SEEK_TABLE)?;
        let mut tag_errors_table = write_txn.open_table(TAG_ERRORS_TABLE)?;
        let mut tag_error_files_table = write_txn.open_table(TAG_ERROR_FILES_TABLE)?;
        let mut album_credits_table = write_txn.open_table(ALBUM_CREDITS_TABLE)?;
        let mut track_credits_table = write_txn.open_table(TRACK_CREDITS_TABLE)?;

        let mut artist_count = 0usize;
        let mut album_count = 0usize;
        let mut track_count = 0usize;
        let mut artist_sidecar_cache: HashMap<PathBuf, Option<SidecarInfo>> = HashMap::new();
        let mut artist_ids_by_key: HashMap<String, String> = HashMap::new();
        let mut album_ids_by_group: HashMap<String, Vec<AlbumIdentity>> = HashMap::new();
        let mut track_identities_by_album: HashMap<String, Vec<TrackIdentity>> = HashMap::new();
        let mut next_track_order_by_album: HashMap<String, usize> = HashMap::new();

        for (index, (root, album_dir)) in album_dirs.into_iter().enumerate() {
            if let Some(progress) = progress {
                progress(ScanProgress::IndexingAlbums {
                    completed: index,
                    total: total_albums,
                });
            }
            let files = files_by_dir.remove(&album_dir).unwrap_or_default();
            if files.is_empty() {
                continue;
            }

            let folder_relpath = match relpath_from(&root.path, &album_dir) {
                Some(rel) => prefix_relpath(&root.id, &rel),
                None => continue,
            };
            debug!(
                "Scanning album folder '{}' ({} audio files)",
                folder_relpath.as_str(),
                files.len()
            );

            let folder_name = album_dir
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "Unknown Album".to_string());
            let (folder_title, folder_year) = split_title_year(&folder_name);

            let fallback_artist = album_dir
                .parent()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "Unknown Artist".to_string());

            let album_sidecar = read_sidecar_info(&album_dir.join("album.json"));
            let artist_sidecar = album_dir.parent().and_then(|parent| {
                load_sidecar_info(&mut artist_sidecar_cache, parent.join("artist.json"))
            });
            if let Some(info) = &album_sidecar {
                debug!(
                    "Album sidecar loaded for '{}': summary_present={}, genres={:?}",
                    folder_relpath.as_str(),
                    info.summary
                        .as_ref()
                        .map(|summary| !summary.trim().is_empty())
                        .unwrap_or(false),
                    info.genres
                );
            }
            if let Some(info) = &artist_sidecar {
                debug!(
                    "Artist sidecar loaded for '{}': summary_present={}, genres={:?}",
                    fallback_artist.as_str(),
                    info.summary
                        .as_ref()
                        .map(|summary| !summary.trim().is_empty())
                        .unwrap_or(false),
                    info.genres
                );
            }

            let mut album_title: Option<String> = None;
            // A later album-artist tag outranks an early track artist. A
            // compilation without that tag uses its folder's artist name.
            let declared_album_artist = files.iter().find_map(|file| {
                tags.get(file)
                    .and_then(|tag| tag.as_ref().ok())
                    .and_then(|tag| {
                        let names = resolved_credit_names(
                            tag.album_artist.as_deref(),
                            &tag.album_artists,
                            tag.album_artists_structured,
                            &known_artists,
                            resolutions,
                        );
                        (!names.is_empty()).then(|| {
                            (
                                tag.album_artist.clone().unwrap_or_else(|| names.join("; ")),
                                names,
                                tag.album_artists_structured || tag.album_artists.len() > 1,
                            )
                        })
                    })
            });
            let mut track_artist_names = Vec::new();
            let mut track_artist_keys = HashSet::new();
            for file in &files {
                if let Some(tag) = tags.get(file).and_then(|tag| tag.as_ref().ok()) {
                    let names = resolved_credit_names(
                        tag.artist.as_deref(),
                        &tag.artists,
                        tag.artists_structured,
                        &known_artists,
                        resolutions,
                    );
                    let key = names
                        .iter()
                        .map(|name| canonical_artist_key(name))
                        .collect::<Vec<_>>()
                        .join("\0");
                    if !names.is_empty() && track_artist_keys.insert(key) {
                        track_artist_names.push((
                            tag.artist.clone().unwrap_or_else(|| names.join("; ")),
                            names,
                            tag.artists_structured || tag.artists.len() > 1,
                        ));
                    }
                }
            }
            let source = if declared_album_artist.is_some() || track_artist_names.len() == 1 {
                "local_tags"
            } else {
                "folder"
            };
            let (album_display_artist, album_artist_names, album_structured) =
                declared_album_artist.unwrap_or_else(|| {
                    if track_artist_names.len() == 1 {
                        let (display, names, structured) = track_artist_names.remove(0);
                        let credits =
                            source_credits(&display, &names, "track_artist", "local_tags");
                        let main_names: Vec<_> = credits
                            .credits
                            .into_iter()
                            .filter(|credit| credit.role != "featured_artist")
                            .map(|credit| credit.name)
                            .collect();
                        if main_names.len() != names.len() {
                            (main_names.join("; "), main_names, structured)
                        } else {
                            (display, names, structured)
                        }
                    } else {
                        (
                            fallback_artist.clone(),
                            resolved_credit_names(
                                Some(&fallback_artist),
                                &[],
                                false,
                                &known_artists,
                                resolutions,
                            ),
                            false,
                        )
                    }
                });
            let album_artist = album_artist_names[0].clone();
            let album_credits = source_credits(
                &album_display_artist,
                &album_artist_names,
                "album_artist",
                if album_structured {
                    "local_tags_structured"
                } else {
                    source
                },
            );
            let mut album_year: Option<i32> = None;
            let mut album_cover: Option<CoverRef> = None;
            let mut album_summary: Option<String> = None;
            let mut album_genres: Vec<String> = Vec::new();
            let mut track_drafts = Vec::new();
            let mut album_tag_error = false;
            let mut tag_error_files: Vec<TagErrorFile> = Vec::new();

            if let Some(info) = &album_sidecar {
                if info.summary.as_ref().is_some() {
                    album_summary = info.summary.clone();
                }
                merge_genres(&mut album_genres, &info.genres);
            }

            for file in files {
                let relpath = match relpath_from(&root.path, &file) {
                    Some(rel) => prefix_relpath(&root.id, &rel),
                    None => continue,
                };
                debug!("Reading tags for '{}'", relpath.as_str());

                let tag = match tags.get(&file).cloned().unwrap_or_else(|| {
                    Err("File appeared during scan; retry on next change".to_owned())
                }) {
                    Ok(tag) => tag,
                    Err(err) => {
                        warn!(
                            target: "issue",
                            "file={} | folder={} | error={:?}",
                            relpath.as_str(),
                            folder_relpath.as_str(),
                            err
                        );
                        album_tag_error = true;
                        tag_error_files.push(TagErrorFile {
                            file_relpath: relpath.clone(),
                            folder_relpath: folder_relpath.clone(),
                            error: format!("{:?}", err),
                            last_seen: now_secs(),
                        });
                        TagInfo::default()
                    }
                };

                if album_title.is_none() {
                    album_title = clean_tag_text(tag.album.as_deref());
                }
                if album_year.is_none() {
                    album_year = tag.year;
                }
                if album_summary.is_none() {
                    album_summary = clean_tag_text(tag.summary.as_deref());
                }
                if !tag.genres.is_empty() {
                    merge_genres(&mut album_genres, &tag.genres);
                }

                debug!(
                    "Tag fields for '{}': album={:?}, album_artist={:?}, artist={:?}, year={:?}, title={:?}, track_no={:?}, disc_no={:?}, summary_present={}, genres={:?}",
                    relpath.as_str(),
                    tag.album.as_deref(),
                    tag.album_artist.as_deref(),
                    tag.artist.as_deref(),
                    tag.year,
                    tag.title.as_deref(),
                    tag.track_no,
                    tag.disc_no,
                    tag.summary
                        .as_ref()
                        .map(|summary| !summary.trim().is_empty())
                        .unwrap_or(false),
                    &tag.genres
                );

                let title =
                    clean_tag_text(tag.title.as_deref()).unwrap_or_else(|| file_stem(&file));
                let duration_ms = tag.duration_ms.unwrap_or(0);
                let codec = match audio_codec(&file) {
                    Some(codec) => codec,
                    None => continue,
                };
                let file_size = file_sizes.get(&file).copied().unwrap_or(0);
                let id = stable_id(&relpath);
                let disc_no = tag
                    .disc_no
                    .or_else(|| disc_number_from_path(&file, &album_dir));
                let album_hint = clean_tag_text(tag.album.as_deref())
                    .or_else(|| album_title.clone())
                    .unwrap_or_else(|| folder_title.clone());
                let artist_hint = album_artist.clone();
                let year_hint = tag.year.or(album_year).or(folder_year);
                debug!(
                    "Indexing track '{}' for album '{}' (artist='{}', year={:?})",
                    title.as_str(),
                    album_hint,
                    artist_hint,
                    year_hint
                );
                debug!(
                    "Track metadata: relpath='{}', track_no={:?}, disc_no={:?}, duration_ms={}, codec={:?}, sample_rate={:?}, channels={:?}, bitrate={:?}, genres={:?}, embedded_cover={}",
                    relpath.as_str(),
                    tag.track_no,
                    disc_no,
                    duration_ms,
                    &codec,
                    tag.sample_rate,
                    tag.channels,
                    tag.bitrate,
                    &tag.genres,
                    tag.has_embedded_cover
                );

                if tag.has_embedded_cover && album_cover.is_none() {
                    album_cover = Some(CoverRef::Embedded {
                        track_id: id.clone(),
                    });
                    debug!(
                        "Album cover set from embedded art in '{}'",
                        relpath.as_str()
                    );
                } else if tag.has_embedded_cover {
                    debug!("Found embedded art in '{}'", relpath.as_str());
                }

                track_drafts.push(TrackDraft {
                    id,
                    relpath,
                    title,
                    artist: clean_tag_text(tag.artist.as_deref()),
                    artists: resolved_credit_names(
                        tag.artist.as_deref(),
                        &tag.artists,
                        tag.artists_structured,
                        &known_artists,
                        resolutions,
                    ),
                    artists_structured: tag.artists_structured || tag.artists.len() > 1,
                    contributions: tag.contributions,
                    track_no: tag.track_no,
                    disc_no,
                    duration_ms,
                    codec,
                    sample_rate: tag.sample_rate,
                    channels: tag.channels,
                    bitrate: tag.bitrate,
                    file_size,
                    genres: tag.genres,
                    has_embedded_cover: tag.has_embedded_cover,
                });
            }

            if track_drafts.is_empty() {
                continue;
            }

            if album_year.is_none() {
                album_year = folder_year;
            }

            let album_title = album_title.unwrap_or(folder_title);
            let album_artist = album_artist.trim().to_string();
            let artist_sidecar = artist_sidecar.filter(|_| {
                canonical_artist_key(&fallback_artist) == canonical_artist_key(&album_artist)
            });
            let mut album_identity_artists = album_artist_names
                .iter()
                .map(|name| canonical_artist_key(name))
                .collect::<Vec<_>>();
            album_identity_artists.sort();
            let album_group_key =
                canonical_album_group_key(&album_identity_artists.join("\0"), &album_title);
            let candidate_album_id = stable_id(&folder_relpath);
            let album_identities = album_ids_by_group
                .entry(album_group_key.clone())
                .or_default();
            let album_identity_idx = find_compatible_album_identity(album_identities, album_year);
            let (album_id, is_new_album) = match album_identity_idx {
                Some(idx) => {
                    let identity = &mut album_identities[idx];
                    identity.year = merged_album_year(identity.year, album_year);
                    (identity.id.clone(), false)
                }
                None => {
                    album_identities.push(AlbumIdentity {
                        id: candidate_album_id.clone(),
                        year: album_year,
                    });
                    (candidate_album_id, true)
                }
            };
            // The preferred physical copy owns the source display credit;
            // later duplicates can add tracks without multiplying album indexes.
            let album_credits = album_credits_table
                .get(album_id.as_str())?
                .map(|value| decode_value::<ArtistCredits>(value.value()))
                .transpose()?
                .unwrap_or(album_credits);
            let album_display_artist = album_credits.display_name.clone();
            let album_artist_names: Vec<_> = album_credits
                .credits
                .iter()
                .map(|credit| credit.credited_name.clone())
                .collect();
            // Physical copies may write the same joint identity in a different
            // order. The preferred copy owns the primary as well as its display.
            let artist_id = album_credits.credits[0].artist_id.clone();
            let album_artist = album_artist_names[0].clone();
            let artist_sidecar = artist_sidecar.filter(|_| {
                canonical_artist_key(&fallback_artist) == canonical_artist_key(&album_artist)
            });
            debug!(
                "Detected album '{}' (artist='{}', year={:?}) with {} tracks",
                album_title.as_str(),
                album_artist.as_str(),
                album_year,
                track_drafts.len()
            );

            if album_cover.is_none() {
                if let Some(cover_rel) = find_folder_cover(&root.path, &root.id, &album_dir) {
                    debug!(
                        "Found folder cover for album '{}': {}",
                        album_title.as_str(),
                        cover_rel.as_str()
                    );
                    album_cover = Some(CoverRef::File { relpath: cover_rel });
                } else {
                    debug!(
                        "No folder cover detected for album '{}'",
                        album_title.as_str()
                    );
                }
            }

            let mut artist_genres = Vec::new();
            merge_genres(&mut artist_genres, &album_genres);
            if let Some(info) = &artist_sidecar {
                merge_genres(&mut artist_genres, &info.genres);
            }
            let mut artist_summary = artist_sidecar
                .as_ref()
                .and_then(|info| info.summary.clone());
            let mut artist_logo = None;
            let mut artist_banner = None;
            let mut artist_name = album_artist.clone();

            if let Some(value) = artists_table.get(artist_id.as_str())? {
                let existing: Artist = decode_value(value.value())?;
                merge_genres(&mut artist_genres, &existing.genres);
                artist_name = existing.name;
                if artist_summary.is_none() {
                    artist_summary = existing.summary;
                }
                if artist_logo.is_none() {
                    artist_logo = existing.logo_ref;
                }
                if artist_banner.is_none() {
                    artist_banner = existing.banner_ref;
                }
            }

            let artist = Artist {
                id: artist_id.clone(),
                name: artist_name,
                genres: artist_genres,
                summary: artist_summary,
                logo_ref: artist_logo,
                banner_ref: artist_banner,
            };
            let artist_bytes = encode_value(&artist)?;
            let new_artist = artists_table
                .insert(artist_id.as_str(), artist_bytes.as_slice())?
                .is_none();
            if new_artist {
                artist_count += 1;
            }

            let album_artist_name_key = artist_name_key(&artist.name, &artist.id);
            artists_by_name_table.insert(album_artist_name_key.as_str(), artist.id.as_bytes())?;

            let (mut album, old_album_name_key, old_album_index_key) = if is_new_album {
                let album = Album {
                    id: album_id.clone(),
                    artist_id: artist_id.clone(),
                    artist_ids: vec![artist_id.clone()],
                    artist_names: vec![artist.name.clone()],
                    title: album_title,
                    year: album_year,
                    folder_relpath,
                    cover_ref: album_cover,
                    genres: album_genres.clone(),
                    summary: album_summary,
                };
                let old_name_key = album_name_key(&artist.name, &album);
                let old_index_key = album_index_key(&artist_id, &album);
                (album, old_name_key, old_index_key)
            } else {
                let mut existing = match albums_table.get(album_id.as_str())? {
                    Some(value) => decode_value::<Album>(value.value())?,
                    None => Album {
                        id: album_id.clone(),
                        artist_id: artist_id.clone(),
                        artist_ids: vec![artist_id.clone()],
                        artist_names: vec![artist.name.clone()],
                        title: album_title,
                        year: album_year,
                        folder_relpath,
                        cover_ref: album_cover.clone(),
                        genres: Vec::new(),
                        summary: None,
                    },
                };
                let old_name_key = album_name_key(&album_display_artist, &existing);
                let old_index_key = album_index_key(&artist_id, &existing);
                if existing
                    .summary
                    .as_ref()
                    .map(|s| s.trim().is_empty())
                    .unwrap_or(true)
                {
                    if let Some(summary) = album_summary.and_then(clean_summary) {
                        existing.summary = Some(summary);
                    }
                }
                merge_genres(&mut existing.genres, &album_genres);
                if existing.year.is_none() {
                    let previous_album = existing.clone();
                    existing.year = album_year;
                    if existing.year != previous_album.year {
                        // Earlier copies may already have track-artist links.
                        // Their ordering keys include the album year too.
                        let track_artists = track_identities_by_album
                            .get(&album_id)
                            .into_iter()
                            .flatten()
                            .map(|track| &track.artist_id)
                            .collect::<HashSet<_>>();
                        for track_artist_id in track_artists {
                            let old_key = album_index_key(track_artist_id, &previous_album);
                            artist_albums_table.remove(old_key.as_str())?;
                            let new_key = album_index_key(track_artist_id, &existing);
                            artist_albums_table.insert(new_key.as_str(), existing.id.as_bytes())?;
                        }
                    }
                }
                if existing.cover_ref.is_none() {
                    if let Some(CoverRef::File { relpath }) = album_cover {
                        existing.cover_ref = Some(CoverRef::File { relpath });
                    }
                }
                (existing, old_name_key, old_index_key)
            };

            album.artist_id = artist_id.clone();
            album.artist_ids = album_credits
                .credits
                .iter()
                .map(|credit| credit.artist_id.clone())
                .collect();
            album.artist_names = album_artist_names.clone();
            let credits_bytes = encode_value(&album_credits)?;
            album_credits_table.insert(album_id.as_str(), credits_bytes.as_slice())?;

            let album_bytes = encode_value(&album)?;
            let prev = albums_table.insert(album_id.as_str(), album_bytes.as_slice())?;
            if prev.is_none() {
                album_count += 1;
            }

            let album_name_key = album_name_key(&album_display_artist, &album);
            if album_name_key != old_album_name_key {
                let _ = albums_by_name_table.remove(old_album_name_key.as_str())?;
            }
            albums_by_name_table.insert(album_name_key.as_str(), album.id.as_bytes())?;

            let primary_album_index_key = album_index_key(&artist_id, &album);
            if primary_album_index_key != old_album_index_key {
                let _ = artist_albums_table.remove(old_album_index_key.as_str())?;
            }
            artist_albums_table.insert(primary_album_index_key.as_str(), album_id.as_bytes())?;

            if album_tag_error {
                warn!(
                    target: "issue",
                    "album={} | folder={} | files={}",
                    album.title.as_str(),
                    album.folder_relpath.as_str(),
                    tag_error_files.len()
                );
                let tag_error = TagErrorInfo {
                    album_id: album.id.clone(),
                    artist_id: artist.id.clone(),
                    album_title: album.title.clone(),
                    artist_name: artist.name.clone(),
                    folder_relpath: album.folder_relpath.clone(),
                    last_seen: now_secs(),
                };
                let tag_bytes = encode_value(&tag_error)?;
                tag_errors_table.insert(album.id.as_str(), tag_bytes.as_slice())?;
            }

            for info in tag_error_files {
                let bytes = encode_value(&info)?;
                tag_error_files_table.insert(info.file_relpath.as_str(), bytes.as_slice())?;
            }

            track_drafts.sort_by(|a, b| {
                let disc_a = a.disc_no.unwrap_or(u16::MAX);
                let disc_b = b.disc_no.unwrap_or(u16::MAX);
                let track_a = a.track_no.unwrap_or(u16::MAX);
                let track_b = b.track_no.unwrap_or(u16::MAX);
                disc_a
                    .cmp(&disc_b)
                    .then_with(|| track_a.cmp(&track_b))
                    .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
                    .then_with(|| a.relpath.cmp(&b.relpath))
            });

            for draft in track_drafts.into_iter() {
                let track_names = if draft.artists.is_empty() {
                    album_artist_names.clone()
                } else {
                    draft.artists.clone()
                };
                let track_artist_name = &track_names[0];
                let display_name = draft.artist.as_deref().unwrap_or(&album_display_artist);
                let mut credits = source_credits(
                    display_name,
                    &track_names,
                    "track_artist",
                    if draft.artists_structured {
                        "local_tags_structured"
                    } else {
                        "local_tags"
                    },
                );
                for contribution in &draft.contributions {
                    let names = resolved_credit_names(
                        Some(&contribution.name),
                        &[],
                        false,
                        &known_artists,
                        resolutions,
                    );
                    let extra = source_credits(
                        &contribution.name,
                        &names,
                        &contribution.role,
                        "local_tags",
                    );
                    for credit in extra.credits {
                        if !credits.credits.iter().any(|existing| {
                            existing.artist_id == credit.artist_id && existing.role == credit.role
                        }) {
                            credits.credits.push(credit);
                        }
                    }
                }
                let mut credit_identity = credits
                    .credits
                    .iter()
                    .filter(|credit| {
                        matches!(credit.role.as_str(), "track_artist" | "featured_artist")
                    })
                    .map(|credit| format!("{}:{}", canonical_artist_key(&credit.name), credit.role))
                    .collect::<Vec<_>>();
                credit_identity.sort();
                let credit_identity = credit_identity.join("\0");
                let track_artist_id = artist_ids_by_key
                    .entry(canonical_artist_key(track_artist_name))
                    .or_insert_with(|| artist_identity_id(track_artist_name))
                    .clone();
                let mut track_genres = draft.genres.clone();
                // An album can aggregate many genres. Do not assign every
                // song its neighbors' genres when it has its own tags.
                let fallback_genres = if track_genres.is_empty() {
                    album_genres.as_slice()
                } else {
                    &[]
                };
                merge_genres(&mut track_genres, fallback_genres);
                let duplicate_track_id = {
                    let identities = track_identities_by_album
                        .entry(album_id.clone())
                        .or_default();
                    find_duplicate_track_id(identities, &draft, &credit_identity)
                };
                if let Some(track_id) = duplicate_track_id {
                    let existing = match tracks_table.get(track_id.as_str())? {
                        Some(value) => Some(decode_value::<Track>(value.value())?),
                        None => None,
                    };
                    if let Some(mut existing) = existing {
                        let before = existing.genres.clone();
                        merge_genres(&mut existing.genres, &track_genres);
                        if existing.genres != before {
                            let bytes = encode_value(&existing)?;
                            tracks_table.insert(track_id.as_str(), bytes.as_slice())?;
                        }
                    }
                    let mut existing_credits = track_credits_table
                        .get(track_id.as_str())?
                        .map(|value| decode_value::<ArtistCredits>(value.value()))
                        .transpose()?
                        .unwrap_or_default();
                    for credit in &credits.credits {
                        if !existing_credits.credits.iter().any(|existing| {
                            existing.artist_id == credit.artist_id && existing.role == credit.role
                        }) {
                            existing_credits.credits.push(credit.clone());
                        }
                    }
                    let bytes = encode_value(&existing_credits)?;
                    track_credits_table.insert(track_id.as_str(), bytes.as_slice())?;
                    continue;
                }

                let mut track_artist = match artists_table.get(track_artist_id.as_str())? {
                    Some(value) => decode_value::<Artist>(value.value())?,
                    None => {
                        artist_count += 1;
                        Artist {
                            id: track_artist_id.clone(),
                            name: track_artist_name.to_owned(),
                            genres: Vec::new(),
                            summary: None,
                            logo_ref: None,
                            banner_ref: None,
                        }
                    }
                };
                merge_genres(&mut track_artist.genres, &draft.genres);
                let artist_bytes = encode_value(&track_artist)?;
                artists_table.insert(track_artist.id.as_str(), artist_bytes.as_slice())?;
                let artist_key = artist_name_key(&track_artist.name, &track_artist.id);
                artists_by_name_table.insert(artist_key.as_str(), track_artist.id.as_bytes())?;
                // Track artists can browse a compilation without changing its
                // primary artist or its displayed album-artist credits.
                let artist_album_key = album_index_key(&track_artist.id, &album);
                artist_albums_table.insert(artist_album_key.as_str(), album.id.as_bytes())?;

                let order = next_track_order_by_album
                    .entry(album_id.clone())
                    .or_insert_with(|| {
                        track_identities_by_album
                            .get(&album_id)
                            .map(|items| items.len())
                            .unwrap_or(0)
                    });
                let track = Track {
                    id: draft.id.clone(),
                    album_id: album_id.clone(),
                    artist_id: track_artist_id,
                    title: draft.title.clone(),
                    track_no: draft.track_no,
                    disc_no: draft.disc_no,
                    duration_ms: draft.duration_ms,
                    codec: draft.codec.clone(),
                    sample_rate: draft.sample_rate,
                    channels: draft.channels,
                    bitrate: draft.bitrate,
                    file_relpath: draft.relpath.clone(),
                    file_size: draft.file_size,
                    genres: track_genres,
                };

                let track_bytes = encode_value(&track)?;
                let credits_bytes = encode_value(&credits)?;
                track_credits_table.insert(track.id.as_str(), credits_bytes.as_slice())?;
                let prev = tracks_table.insert(track.id.as_str(), track_bytes.as_slice())?;
                if prev.is_none() {
                    track_count += 1;
                }

                embedded_cover_table
                    .insert(track.id.as_str(), bool_bytes(draft.has_embedded_cover))?;
                let seek = build_seek_index(track.duration_ms, track.file_size);
                let seek_bytes = encode_value(&seek)?;
                seek_table.insert(track.id.as_str(), seek_bytes.as_slice())?;

                let track_index_key = album_track_key(&album_id, *order, &track.id);
                album_tracks_table.insert(track_index_key.as_str(), track.id.as_bytes())?;
                *order += 1;

                let track_name_key = track_name_key(
                    &track_credit_search_text(&credits),
                    &album.title,
                    track.disc_no,
                    track.track_no,
                    &track.title,
                    &track.id,
                );
                tracks_by_name_table.insert(track_name_key.as_str(), track.id.as_bytes())?;
                track_identities_by_album
                    .entry(album_id.clone())
                    .or_default()
                    .push(track_identity_from_track(&track, credit_identity));
            }
        }

        if let Some(progress) = progress {
            progress(ScanProgress::IndexingAlbums {
                completed: total_albums,
                total: total_albums,
            });
        }

        // Every explicit collaborator has one artist record and one shared
        // album relationship. Headline vs appearances is read from credit roles.
        // Rebuild these keys after years settle across duplicate physical copies.
        drop(artist_albums_table);
        clear_table(&write_txn, ARTIST_ALBUMS_TABLE)?;
        let mut artist_albums_table = write_txn.open_table(ARTIST_ALBUMS_TABLE)?;
        for entry in albums_table.iter()? {
            let (album_id, value) = entry?;
            let album: Album = decode_value(value.value())?;
            let mut all_credits = album_credits_table
                .get(album_id.value())?
                .map(|value| decode_value::<ArtistCredits>(value.value()))
                .transpose()?
                .unwrap_or_default()
                .credits;
            let prefix = crate::storage::prefix_key(&album.id);
            let mut end = prefix.clone();
            end.push('\u{10ffff}');
            for entry in album_tracks_table.range(prefix.as_str()..end.as_str())? {
                let (key, _) = entry?;
                let (_, track_id) = crate::storage::split_key_last(key.value())?;
                if let Some(value) = track_credits_table.get(track_id)? {
                    all_credits.extend(decode_value::<ArtistCredits>(value.value())?.credits);
                }
            }
            for credit in all_credits {
                let mut credited_artist = match artists_table.get(credit.artist_id.as_str())? {
                    Some(value) => decode_value::<Artist>(value.value())?,
                    None => {
                        artist_count += 1;
                        Artist {
                            id: credit.artist_id.clone(),
                            name: credit.name.clone(),
                            genres: Vec::new(),
                            summary: None,
                            logo_ref: None,
                            banner_ref: None,
                        }
                    }
                };
                merge_genres(&mut credited_artist.genres, &album.genres);
                let bytes = encode_value(&credited_artist)?;
                artists_table.insert(credited_artist.id.as_str(), bytes.as_slice())?;
                let key = artist_name_key(&credited_artist.name, &credited_artist.id);
                artists_by_name_table.insert(key.as_str(), credited_artist.id.as_bytes())?;
                let key = album_index_key(&credited_artist.id, &album);
                artist_albums_table.insert(key.as_str(), album.id.as_bytes())?;
            }
        }

        let stats = LibraryStats {
            artists: artist_count,
            albums: album_count,
            tracks: track_count,
        };

        let version_bytes = encode_value(&INDEX_VERSION)?;
        meta_table.insert(META_VERSION_KEY, version_bytes.as_slice())?;
        let stats_bytes = encode_value(&stats)?;
        meta_table.insert(META_STATS_KEY, stats_bytes.as_slice())?;

        stats
    };

    write_txn.commit()?;
    Ok(stats)
}

fn canonical_artist_key(name: &str) -> String {
    artist_identity_key(name)
}

fn unambiguous_identity(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    !name.contains([',', ';', '\0'])
        && ![
            " & ",
            " and ",
            " x ",
            " / ",
            " featuring ",
            " feat ",
            " feat. ",
            " ft ",
            " ft. ",
        ]
        .iter()
        .any(|join| lower.contains(join))
}

fn canonical_album_group_key(artist_key: &str, album_title: &str) -> String {
    let mut out = String::new();
    out.push_str(artist_key);
    out.push(KEY_SEP);
    out.push_str(&canonical_text_key(album_title));
    out
}

fn canonical_text_key(value: &str) -> String {
    artist_identity_key(value)
}

fn clean_tag_text(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn find_compatible_album_identity(
    identities: &[AlbumIdentity],
    year: Option<i32>,
) -> Option<usize> {
    identities
        .iter()
        .position(|existing| album_years_compatible(existing.year, year))
}

fn album_years_compatible(existing: Option<i32>, incoming: Option<i32>) -> bool {
    match (existing, incoming) {
        (Some(left), Some(right)) => left == right,
        _ => true,
    }
}

fn merged_album_year(existing: Option<i32>, incoming: Option<i32>) -> Option<i32> {
    existing.or(incoming)
}

fn track_identity_from_track(track: &Track, artist_credit_key: String) -> TrackIdentity {
    TrackIdentity {
        id: track.id.clone(),
        artist_id: track.artist_id.clone(),
        artist_credit_key,
        disc_no: track.disc_no,
        track_no: track.track_no,
        title_key: canonical_text_key(&track.title),
        duration_ms: track.duration_ms,
    }
}

fn find_duplicate_track_id(
    identities: &[TrackIdentity],
    draft: &TrackDraft,
    artist_id: &str,
) -> Option<String> {
    identities
        .iter()
        .find(|existing| track_identity_matches(existing, draft, artist_id))
        .map(|existing| existing.id.clone())
}

fn track_identity_matches(existing: &TrackIdentity, draft: &TrackDraft, artist_id: &str) -> bool {
    if existing.artist_credit_key != artist_id {
        return false;
    }
    if let (Some(left), Some(right)) = (existing.track_no, draft.track_no) {
        if left != right || !disc_numbers_compatible(existing.disc_no, draft.disc_no) {
            return false;
        }
    }
    existing.title_key == canonical_text_key(&draft.title)
        && durations_compatible(existing.duration_ms, draft.duration_ms)
}

fn disc_numbers_compatible(existing: Option<u16>, incoming: Option<u16>) -> bool {
    match (existing, incoming) {
        (Some(left), Some(right)) => left == right,
        _ => true,
    }
}

fn durations_compatible(existing_ms: u32, incoming_ms: u32) -> bool {
    if existing_ms == 0 || incoming_ms == 0 {
        return true;
    }
    existing_ms.abs_diff(incoming_ms) <= 5_000
}

#[derive(Debug)]
struct TrackDraft {
    artists_structured: bool,
    id: String,
    relpath: String,
    title: String,
    artist: Option<String>,
    artists: Vec<String>,
    contributions: Vec<metadata::TagContribution>,
    track_no: Option<u16>,
    disc_no: Option<u16>,
    duration_ms: u32,
    codec: Codec,
    sample_rate: Option<u32>,
    channels: Option<u8>,
    bitrate: Option<u32>,
    file_size: u64,
    genres: Vec<String>,
    has_embedded_cover: bool,
}

#[derive(Debug, Clone)]
struct AlbumIdentity {
    id: String,
    year: Option<i32>,
}

#[derive(Debug, Clone)]
struct TrackIdentity {
    id: String,
    artist_id: String,
    artist_credit_key: String,
    disc_no: Option<u16>,
    track_no: Option<u16>,
    title_key: String,
    duration_ms: u32,
}

fn collect_album_dirs<'a>(root: &Path, files: impl Iterator<Item = &'a PathBuf>) -> Vec<PathBuf> {
    let dirs_with_audio: HashSet<_> = files
        .filter(|file| file.starts_with(root))
        .filter_map(|file| file.parent().map(Path::to_path_buf))
        .collect();

    let mut has_descendant_audio = HashSet::new();
    for dir in &dirs_with_audio {
        let mut ancestor = dir.parent();
        while let Some(current) = ancestor {
            if !current.starts_with(root) {
                break;
            }
            has_descendant_audio.insert(current.to_path_buf());
            if current == root {
                break;
            }
            ancestor = current.parent();
        }
    }

    let mut album_dirs: HashSet<PathBuf> = dirs_with_audio
        .iter()
        .filter(|dir| !has_descendant_audio.contains(*dir))
        .cloned()
        .collect();

    let mut parent_to_children: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for dir in &album_dirs {
        if let Some(parent) = dir.parent() {
            if parent == root {
                continue;
            }
            parent_to_children
                .entry(parent.to_path_buf())
                .or_default()
                .push(dir.clone());
        }
    }

    let mut promoted: Vec<(PathBuf, Vec<PathBuf>)> = Vec::new();
    for (parent, children) in parent_to_children {
        if children.is_empty() {
            continue;
        }
        if dirs_with_audio.contains(&parent) {
            continue;
        }
        if !children.iter().all(|child| {
            child
                .file_name()
                .and_then(|name| name.to_str())
                .map(is_disc_folder_name)
                .unwrap_or(false)
        }) {
            continue;
        }
        promoted.push((parent, children));
    }

    for (parent, children) in promoted {
        for child in children {
            album_dirs.remove(&child);
        }
        album_dirs.insert(parent);
    }

    let mut album_dirs: Vec<PathBuf> = album_dirs.into_iter().collect();
    album_dirs.sort();
    album_dirs
}

pub(super) fn audio_codec(path: &Path) -> Option<Codec> {
    let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
    match ext.as_str() {
        "mp3" => Some(Codec::Mp3),
        "flac" => Some(Codec::Flac),
        _ => None,
    }
}

fn build_seek_index(duration_ms: u32, file_size: u64) -> SeekIndex {
    let mut points = Vec::new();
    points.push(SeekPoint { t_ms: 0, byte: 0 });

    if duration_ms > 0 && file_size > 0 {
        let mut t = SEEK_STEP_MS;
        while t < duration_ms {
            let byte = file_size.saturating_mul(u64::from(t)) / u64::from(duration_ms);
            points.push(SeekPoint { t_ms: t, byte });
            t = t.saturating_add(SEEK_STEP_MS);
        }
        points.push(SeekPoint {
            t_ms: duration_ms,
            byte: file_size.saturating_sub(1),
        });
    }

    SeekIndex {
        duration_ms,
        points,
        hint: "Client should request Range: bytes=byte-".to_string(),
    }
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Unknown Track".to_string())
}

fn split_title_year(input: &str) -> (String, Option<i32>) {
    let trimmed = input.trim();
    if let Some((title, year)) = split_year_suffix(trimmed, '(', ')') {
        return (title.to_string(), Some(year));
    }
    if let Some((title, year)) = split_year_suffix(trimmed, '[', ']') {
        return (title.to_string(), Some(year));
    }
    (trimmed.to_string(), None)
}

fn split_year_suffix(input: &str, open: char, close: char) -> Option<(&str, i32)> {
    let trimmed = input.trim_end();
    if !trimmed.ends_with(close) {
        return None;
    }
    let open_idx = trimmed.rfind(open)?;
    let year_str = trimmed
        .get(open_idx + open.len_utf8()..trimmed.len() - close.len_utf8())?
        .trim();
    if year_str.len() != 4 || !year_str.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let year = year_str.parse::<i32>().ok()?;
    let title = trimmed[..open_idx].trim_end();
    if title.is_empty() {
        return None;
    }
    Some((title, year))
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(default)]
struct SidecarInfo {
    summary: Option<String>,
    genres: Vec<String>,
}

fn read_sidecar_info(path: &Path) -> Option<SidecarInfo> {
    let data = fs::read(path).ok()?;
    let mut info: SidecarInfo = serde_json::from_slice(&data).ok()?;
    if let Some(summary) = &info.summary {
        if summary.trim().is_empty() {
            info.summary = None;
        }
    }
    info.genres.retain(|genre| !genre.trim().is_empty());
    if info.summary.is_none() && info.genres.is_empty() {
        None
    } else {
        Some(info)
    }
}

fn load_sidecar_info(
    cache: &mut HashMap<PathBuf, Option<SidecarInfo>>,
    path: PathBuf,
) -> Option<SidecarInfo> {
    if let Some(cached) = cache.get(&path) {
        return cached.clone();
    }
    let info = read_sidecar_info(&path);
    cache.insert(path, info.clone());
    info
}

fn find_folder_cover(root: &Path, root_id: &str, album_dir: &Path) -> Option<String> {
    const COVERS: &[&str] = &[
        "cover.jpg",
        "cover.jpeg",
        "cover.png",
        "folder.jpg",
        "folder.jpeg",
        "folder.png",
        "front.jpg",
        "front.jpeg",
        "front.png",
        "album.jpg",
        "album.png",
    ];

    let mut candidates = HashSet::new();
    for name in COVERS {
        candidates.insert(*name);
    }

    let entries = fs::read_dir(album_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_ascii_lowercase())?;
        if candidates.contains(name.as_str()) {
            let relpath = relpath_from(root, &path)?;
            return Some(prefix_relpath(root_id, &relpath));
        }
    }
    None
}

fn is_disc_folder_name(name: &str) -> bool {
    parse_disc_number(name).is_some()
}

fn disc_number_from_path(file: &Path, album_dir: &Path) -> Option<u16> {
    if !file.starts_with(album_dir) {
        return None;
    }
    let mut current = file.parent()?;
    loop {
        if current == album_dir {
            break;
        }
        if let Some(name) = current.file_name().and_then(|s| s.to_str()) {
            if let Some(num) = parse_disc_number(name) {
                return Some(num);
            }
        }
        current = current.parent()?;
    }
    None
}

fn parse_disc_number(name: &str) -> Option<u16> {
    let cleaned = normalize_disc_name(name);
    if cleaned.is_empty() {
        return None;
    }

    for prefix in DISC_KEYWORDS {
        if let Some(rest) = cleaned.strip_prefix(prefix) {
            let rest = rest.trim();
            if let Some(num) = parse_number_token(rest) {
                return Some(num);
            }
        }
    }

    let tokens: Vec<&str> = cleaned.split_whitespace().collect();
    if tokens.len() < 2 {
        return None;
    }

    let last = tokens[tokens.len() - 1];
    let num = parse_number_token(last)?;
    if tokens[..tokens.len() - 1]
        .iter()
        .any(|token| is_disc_keyword(token))
    {
        return Some(num);
    }

    None
}

fn normalize_disc_name(name: &str) -> String {
    let mut cleaned = String::with_capacity(name.len());
    for ch in name.chars() {
        let ch = ch.to_ascii_lowercase();
        if ch == '_' || ch == '-' || ch == '.' {
            cleaned.push(' ');
        } else {
            cleaned.push(ch);
        }
    }
    cleaned.trim().to_string()
}

fn parse_number_token(token: &str) -> Option<u16> {
    if token.chars().all(|c| c.is_ascii_digit()) {
        return token.parse::<u16>().ok();
    }
    if token.chars().all(is_roman_char) {
        return roman_to_u16(token);
    }
    None
}

fn is_disc_keyword(token: &str) -> bool {
    matches!(
        token,
        "cd" | "disc"
            | "disk"
            | "dvd"
            | "medium"
            | "media"
            | "format"
            | "vol"
            | "volume"
            | "part"
            | "side"
            | "lp"
    )
}

const DISC_KEYWORDS: &[&str] = &[
    "cd", "disc", "disk", "dvd", "medium", "media", "format", "vol", "volume", "part", "side", "lp",
];

fn is_roman_char(ch: char) -> bool {
    matches!(ch, 'i' | 'v' | 'x' | 'l' | 'c' | 'd' | 'm')
}

fn roman_to_u16(input: &str) -> Option<u16> {
    let mut total = 0u16;
    let mut prev = 0u16;
    for ch in input.chars().rev() {
        let value = match ch {
            'i' => 1,
            'v' => 5,
            'x' => 10,
            'l' => 50,
            'c' => 100,
            'd' => 500,
            'm' => 1000,
            _ => return None,
        };
        if value < prev {
            total = total.saturating_sub(value);
        } else {
            total = total.saturating_add(value);
            prev = value;
        }
    }
    if total == 0 {
        None
    } else {
        Some(total)
    }
}
