//! Filesystem change detection, cached tag parsing, and atomic live-catalog commit.
mod catalog;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use common::{artist_identity_key, Album, Artist, ArtistCredits};
use metadata::{read_tags, TagInfo};
use redb::{Database, ReadableTable, TableDefinition, TableError};
use serde::{Deserialize, Serialize};
use tracing::info;
use walkdir::WalkDir;

use self::catalog::{audio_codec, build_catalog};
use crate::normalization::merge_genres;
use crate::storage::{
    clear_table, decode_value, encode_value, prefix_key, read_stats, ALBUMS_BY_NAME_TABLE,
    ALBUMS_TABLE, ALBUM_CREDITS_TABLE, ALBUM_TRACKS_TABLE, ARTISTS_BY_NAME_TABLE, ARTISTS_TABLE,
    ARTIST_ALBUMS_TABLE, EXTERNAL_ATTEMPTS_TABLE, META_TABLE, SEEK_TABLE, TAG_ERRORS_TABLE,
    TAG_ERROR_FILES_TABLE, TRACKS_BY_NAME_TABLE, TRACKS_TABLE, TRACK_CREDITS_TABLE,
    TRACK_EMBEDDED_COVER_TABLE,
};
use crate::{LibraryError, LibraryRoot, LibraryStats, ScanObserver, ScanProgress};

const FILE_TAGS: TableDefinition<&str, &[u8]> = TableDefinition::new("file_tags_v4");
const LOCAL_ARTISTS: TableDefinition<&str, &[u8]> = TableDefinition::new("local_artists_v1");
pub(super) const LOCAL_ALBUMS: TableDefinition<&str, &[u8]> =
    TableDefinition::new("local_albums_v1");
pub(super) static SCAN_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
const MANIFEST: &str = "file_manifest_v1";
const EXTERNAL_IDENTITY_POLICY_KEY: &str = "external_metadata_identity_policy";
const EXTERNAL_IDENTITY_POLICY_VERSION: &[u8] = &[3];
const ARTIST_CREDIT_POLICY_KEY: &str = "artist_credit_identity_policy";
const ARTIST_CREDIT_POLICY_VERSION: &[u8] = &[3];
pub(super) const DISPLAY_CREDIT_RESOLUTIONS_DIRTY: &str = "display_credit_resolutions_dirty";

impl crate::Library {
    /// The album identity read from files, before external enrichment.
    pub fn get_local_album(&self, album_id: &str) -> Result<Option<Album>, LibraryError> {
        let read = self.db.begin_read()?;
        let table = match read.open_table(LOCAL_ALBUMS) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let album = table
            .get(album_id)?
            .map(|value| decode_value(value.value()))
            .transpose()?;
        Ok(album)
    }
}

pub(super) fn needs_identity_policy_recovery(db: &Database) -> Result<bool, LibraryError> {
    let read = db.begin_read()?;
    let table = match read.open_table(META_TABLE) {
        Ok(table) => table,
        Err(TableError::TableDoesNotExist(_)) => return Ok(true),
        Err(err) => return Err(err.into()),
    };
    let current = table
        .get(EXTERNAL_IDENTITY_POLICY_KEY)?
        .is_some_and(|value| value.value() == EXTERNAL_IDENTITY_POLICY_VERSION);
    if !current {
        return Ok(true);
    }
    for (credits, media) in [
        (ALBUM_CREDITS_TABLE, ALBUMS_TABLE),
        (TRACK_CREDITS_TABLE, TRACKS_TABLE),
    ] {
        let credits = match read.open_table(credits) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(true),
            Err(err) => return Err(err.into()),
        };
        let media = read.open_table(media)?;
        if credits.len()? != media.len()? {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn needs_catalog_recovery(db: &Database) -> Result<bool, LibraryError> {
    if needs_identity_policy_recovery(db)? || needs_credit_policy_recovery(db)? {
        return Ok(true);
    }
    let read = db.begin_read()?;
    let table = read.open_table(META_TABLE)?;
    let dirty = table.get(DISPLAY_CREDIT_RESOLUTIONS_DIRTY)?.is_some();
    Ok(dirty)
}

fn needs_credit_policy_recovery(db: &Database) -> Result<bool, LibraryError> {
    let read = db.begin_read()?;
    let table = match read.open_table(META_TABLE) {
        Ok(table) => table,
        Err(TableError::TableDoesNotExist(_)) => return Ok(true),
        Err(err) => return Err(err.into()),
    };
    let current = table
        .get(ARTIST_CREDIT_POLICY_KEY)?
        .is_some_and(|value| value.value() == ARTIST_CREDIT_POLICY_VERSION);
    Ok(!current)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Fingerprint {
    size: u64,
    modified: Option<SystemTime>,
}

#[derive(Clone, Serialize, Deserialize)]
struct CachedTags {
    fingerprint: Fingerprint,
    tags: TagInfo,
}

// Tag parsing and catalog assembly use an isolated staging database. The live
// writer is acquired only for the atomic delta commit, never for filesystem I/O.
pub(super) fn scan(
    roots: &[LibraryRoot],
    db: &Database,
    force: bool,
    changed_paths: Option<&[PathBuf]>,
    progress: Option<ScanObserver<'_>>,
) -> Result<LibraryStats, LibraryError> {
    let started = Instant::now();
    if let Some(progress) = progress {
        progress(ScanProgress::Discovering);
    }
    let _scan = SCAN_LOCK.lock();
    scan_locked(roots, db, force, changed_paths, started, progress)
}

// The caller holds SCAN_LOCK, including any preceding resolution-table write.
// This keeps a concurrent scan from clearing a correction before applying it.
pub(super) fn scan_locked(
    roots: &[LibraryRoot],
    db: &Database,
    force: bool,
    changed_paths: Option<&[PathBuf]>,
    started: Instant,
    progress: Option<ScanObserver<'_>>,
) -> Result<LibraryStats, LibraryError> {
    let lock_done = started.elapsed();
    let recover_external_metadata = needs_identity_policy_recovery(db)?;
    let recover_artist_credits = needs_credit_policy_recovery(db)?;
    let read = db.begin_read()?;
    let previous = read
        .open_table(META_TABLE)
        .ok()
        .and_then(|table| {
            table
                .get(MANIFEST)
                .ok()
                .flatten()
                .map(|value| value.value().to_vec())
        })
        .and_then(|bytes| decode_value::<BTreeMap<String, Fingerprint>>(&bytes).ok());
    let resolutions_dirty = read
        .open_table(META_TABLE)
        .ok()
        .and_then(|table| {
            table
                .get(DISPLAY_CREDIT_RESOLUTIONS_DIRTY)
                .ok()
                .flatten()
                .map(|_| true)
        })
        .unwrap_or(false);
    let root_keys: Vec<_> = roots
        .iter()
        .enumerate()
        .map(|(index, root)| format!("root:{index}:{}:{}", root.id, root.path.display()))
        .collect();
    let compatible = previous
        .as_ref()
        .map(|manifest| {
            manifest
                .keys()
                .filter(|key| key.starts_with("root:"))
                .count()
                == root_keys.len()
                && root_keys.iter().all(|key| manifest.contains_key(key))
        })
        .unwrap_or(false);
    let targeted = !force && compatible && changed_paths.is_some();
    let mut manifest = if targeted {
        previous.clone().unwrap()
    } else {
        BTreeMap::new()
    };
    let paths: Vec<_> = if targeted {
        changed_paths
            .unwrap()
            .iter()
            .filter(|path| roots.iter().any(|root| path.starts_with(&root.path)))
            .cloned()
            .collect()
    } else {
        roots.iter().map(|root| root.path.clone()).collect()
    };
    for path in paths {
        // Removal and directory rename events delete the old subtree from the
        // manifest; additions and modifications repopulate only that subtree.
        manifest.retain(|key, _| key.starts_with("root:") || !Path::new(key).starts_with(&path));
        if !path.exists() {
            continue;
        }
        for entry in WalkDir::new(&path).follow_links(false) {
            let entry = entry.map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let metadata = entry
                .metadata()
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))?;
            manifest.insert(
                entry.path().to_string_lossy().into_owned(),
                Fingerprint {
                    size: metadata.len(),
                    modified: metadata.modified().ok(),
                },
            );
        }
    }
    for key in root_keys {
        manifest.insert(
            key,
            Fingerprint {
                size: 0,
                modified: None,
            },
        );
    }
    let inventory_done = started.elapsed();
    if !force
        && !recover_external_metadata
        && !recover_artist_credits
        && !resolutions_dirty
        && previous.as_ref() == Some(&manifest)
    {
        let stats = read_stats(db)?;
        if profile_enabled() {
            info!(target: "performance", event = "metadata_local_scan", source = "local_tags", outcome = "ok", force, targeted, unchanged = true, duration_us = started.elapsed().as_micros() as u64, lock_wait_us = lock_done.as_micros() as u64, inventory_us = (inventory_done - lock_done).as_micros() as u64, parsed_files = 0u64, "Local metadata scan reused its unchanged catalog");
        }
        return Ok(stats);
    }
    let audio: Vec<_> = manifest
        .iter()
        .filter(|(path, _)| !path.starts_with("root:") && audio_codec(Path::new(path)).is_some())
        .map(|(path, fingerprint)| (PathBuf::from(path), fingerprint.clone()))
        .collect();
    let mut tags = HashMap::new();
    let mut pending = Vec::new();
    let cached = read.open_table(FILE_TAGS).ok();
    for (path, fingerprint) in &audio {
        let previous = if force || recover_external_metadata || recover_artist_credits {
            None
        } else {
            cached
                .as_ref()
                .and_then(|table| table.get(path.to_string_lossy().as_ref()).ok().flatten())
                .and_then(|value| decode_value::<CachedTags>(value.value()).ok())
        };
        if let Some(previous) = previous.filter(|item| item.fingerprint == *fingerprint) {
            tags.insert(path.clone(), Ok(previous.tags));
        } else {
            pending.push(path.clone());
        }
    }
    drop(cached);
    drop(read);
    let reused_files = tags.len();
    if let Some(progress) = progress {
        progress(ScanProgress::ReadingTracks {
            completed: reused_files,
            total: audio.len(),
        });
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results = std::sync::Mutex::new(Vec::new());
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .min(4);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(path) = pending.get(index) else {
                    break;
                };
                let result = read_tags(path).map_err(|err| format!("{:?}", err));
                let mut results = results.lock().unwrap();
                results.push((path.clone(), result));
                if let Some(progress) = progress {
                    progress(ScanProgress::ReadingTracks {
                        completed: reused_files + results.len(),
                        total: audio.len(),
                    });
                }
            });
        }
    });
    info!(
        "Indexing {} audio files: {} changed, {} reused",
        audio.len(),
        pending.len(),
        tags.len()
    );
    tags.extend(results.into_inner().unwrap());
    let tags_done = started.elapsed();
    let staged = std::sync::Arc::new(
        Database::builder().create_with_backend(redb::backends::InMemoryBackend::new())?,
    );
    let file_sizes = audio
        .iter()
        .map(|(path, fingerprint)| (path.clone(), fingerprint.size))
        .collect();
    let resolutions = crate::credits::display_credit_resolutions(db)?;
    build_catalog(roots, &staged, &tags, &file_sizes, &resolutions, progress)?;
    if let Some(progress) = progress {
        progress(ScanProgress::Committing);
    }
    let local_artists = source_records::<Artist>(&staged, ARTISTS_TABLE)?;
    let local_albums = source_records::<Album>(&staged, ALBUMS_TABLE)?;
    let catalog_done = started.elapsed();
    let write = db.begin_write()?;
    let writer_done = started.elapsed();
    // Previous provider lookups lacked reliable artist/release identity
    // checks. Rebuild from local sources once rather than carrying any of
    // those old credits, descriptions, genres, or artwork references forward.
    let invalidated = if !force && !recover_external_metadata {
        preserve_enrichment(
            roots,
            &staged,
            &write,
            &local_artists,
            &local_albums,
            !recover_artist_credits,
            &resolutions,
        )?
    } else {
        InvalidatedSources::default()
    };
    let stats = read_stats(&staged)?;
    crate::enrichment::album_metadata_stats(&staged)?;
    let staging_read = staged.begin_read()?;
    let enrichment_done = started.elapsed();
    for definition in [
        META_TABLE,
        ARTISTS_TABLE,
        ARTISTS_BY_NAME_TABLE,
        ALBUMS_TABLE,
        ALBUMS_BY_NAME_TABLE,
        ARTIST_ALBUMS_TABLE,
        TRACKS_TABLE,
        TRACKS_BY_NAME_TABLE,
        ALBUM_TRACKS_TABLE,
        TRACK_EMBEDDED_COVER_TABLE,
        SEEK_TABLE,
        TAG_ERRORS_TABLE,
        TAG_ERROR_FILES_TABLE,
        ALBUM_CREDITS_TABLE,
        TRACK_CREDITS_TABLE,
    ] {
        let source = staging_read.open_table(definition)?;
        let mut target = write.open_table(definition)?;
        let mut obsolete = Vec::new();
        for entry in target.iter()? {
            let (key, _) = entry?;
            if key.value() != MANIFEST && source.get(key.value())?.is_none() {
                obsolete.push(key.value().to_owned());
            }
        }
        for key in obsolete {
            target.remove(key.as_str())?;
        }
        for entry in source.iter()? {
            let (key, value) = entry?;
            let changed = target
                .get(key.value())?
                .map(|old| old.value() != value.value())
                .unwrap_or(true);
            if changed {
                target.insert(key.value(), value.value())?;
            }
        }
    }
    if force || recover_external_metadata {
        clear_table(&write, EXTERNAL_ATTEMPTS_TABLE)?;
    } else {
        prune_external_attempts(&write, &staging_read, &invalidated)?;
    }
    if recover_external_metadata || recover_artist_credits {
        write.delete_table(TableDefinition::<&str, &[u8]>::new("file_tags_v1"))?;
        write.delete_table(TableDefinition::<&str, &[u8]>::new("file_tags_v2"))?;
        write.delete_table(TableDefinition::<&str, &[u8]>::new("file_tags_v3"))?;
    }
    replace_source_records(&write, LOCAL_ARTISTS, &local_artists)?;
    replace_source_records(&write, LOCAL_ALBUMS, &local_albums)?;
    {
        let mut cache = write.open_table(FILE_TAGS)?;
        let mut obsolete = Vec::new();
        for entry in cache.iter()? {
            let (key, _) = entry?;
            if !manifest.contains_key(key.value()) {
                obsolete.push(key.value().to_owned());
            }
        }
        for key in obsolete {
            cache.remove(key.as_str())?;
        }
        // Unchanged tags already have the same cached fingerprint and value.
        // Avoid cloning and serializing every track when just one file changed.
        for path in &pending {
            let fingerprint = manifest.get(path.to_string_lossy().as_ref()).unwrap();
            if let Some(Ok(tag)) = tags.get(path) {
                let value = encode_value(&CachedTags {
                    fingerprint: fingerprint.clone(),
                    tags: tag.clone(),
                })?;
                let key = path.to_string_lossy();
                let changed = cache
                    .get(key.as_ref())?
                    .map(|old| old.value() != value.as_slice())
                    .unwrap_or(true);
                if changed {
                    cache.insert(key.as_ref(), value.as_slice())?;
                }
            }
        }
        let mut meta = write.open_table(META_TABLE)?;
        let manifest_bytes = encode_value(&manifest)?;
        meta.insert(MANIFEST, manifest_bytes.as_slice())?;
        meta.insert(
            EXTERNAL_IDENTITY_POLICY_KEY,
            EXTERNAL_IDENTITY_POLICY_VERSION,
        )?;
        meta.insert(ARTIST_CREDIT_POLICY_KEY, ARTIST_CREDIT_POLICY_VERSION)?;
    }
    write.commit()?;
    if profile_enabled() {
        info!(target: "performance", event = "metadata_local_scan", source = "local_tags", outcome = "ok", force, targeted, unchanged = false, duration_us = started.elapsed().as_micros() as u64, lock_wait_us = lock_done.as_micros() as u64, inventory_us = (inventory_done - lock_done).as_micros() as u64, tags_us = (tags_done - inventory_done).as_micros() as u64, catalog_us = (catalog_done - tags_done).as_micros() as u64, writer_wait_us = (writer_done - catalog_done).as_micros() as u64, enrichment_reuse_us = (enrichment_done - writer_done).as_micros() as u64, commit_us = (started.elapsed() - enrichment_done).as_micros() as u64, audio_files = audio.len(), parsed_files = pending.len(), reused_files, "Local metadata scan completed");
    }
    Ok(stats)
}

fn profile_enabled() -> bool {
    std::env::var("PHONOLITE_PROFILE")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes"
            )
        })
        .unwrap_or(false)
}

type SourceRecord<T> = (String, T, Vec<u8>);

#[derive(Default)]
struct InvalidatedSources {
    artists: HashSet<String>,
    albums: HashSet<String>,
    album_artists: HashSet<String>,
}

fn source_records<T: for<'de> Deserialize<'de>>(
    db: &Database,
    definition: TableDefinition<&str, &[u8]>,
) -> Result<Vec<SourceRecord<T>>, LibraryError> {
    let read = db.begin_read()?;
    let table = read.open_table(definition)?;
    let records = table
        .iter()?
        .map(|entry| {
            let (key, value) = entry?;
            let bytes = value.value().to_vec();
            Ok((key.value().to_string(), decode_value(&bytes)?, bytes))
        })
        .collect();
    records
}

// Keep the previous local values separately from the enriched catalog. This
// lets edits to tags/sidecars take precedence without throwing away fields
// fetched from a provider merely because an unrelated file changed.
fn preserve_enrichment(
    roots: &[LibraryRoot],
    staged: &std::sync::Arc<Database>,
    live: &redb::WriteTransaction,
    local_artists: &[SourceRecord<Artist>],
    local_albums: &[SourceRecord<Album>],
    preserve_album_artists: bool,
    resolutions: &HashMap<String, ArtistCredits>,
) -> Result<InvalidatedSources, LibraryError> {
    let mut invalidated = InvalidatedSources::default();
    let library = crate::Library {
        roots: roots.to_vec(),
        db: std::sync::Arc::clone(staged),
    };
    let artists = live.open_table(ARTISTS_TABLE)?;
    let artist_sources = live.open_table(LOCAL_ARTISTS)?;
    let mut artist_names = HashMap::new();
    for (id, incoming, _) in local_artists {
        let Some(value) = artists.get(id.as_str())? else {
            continue;
        };
        let existing: Artist = decode_value(value.value())?;
        let baseline = artist_sources
            .get(id.as_str())?
            .map(|value| decode_value::<Artist>(value.value()))
            .transpose()?;
        let query_name = baseline.as_ref().unwrap_or(&existing).name.as_str();
        if artist_identity_key(&incoming.name) != artist_identity_key(query_name) {
            invalidated.artists.insert(id.clone());
            continue;
        }
        artist_names.insert(
            id.clone(),
            if baseline
                .as_ref()
                .is_some_and(|old| old.name == incoming.name)
            {
                existing.name.clone()
            } else {
                incoming.name.clone()
            },
        );
        let summary = preserve_field(
            &incoming.summary,
            baseline.as_ref().map(|old| &old.summary),
            &existing.summary,
        );
        let genres = preserve_genres(
            &incoming.genres,
            baseline.as_ref().map(|old| old.genres.as_slice()),
            &existing.genres,
        );
        let logo = preserve_field(
            &incoming.logo_ref,
            baseline.as_ref().map(|old| &old.logo_ref),
            &existing.logo_ref,
        );
        let banner = preserve_field(
            &incoming.banner_ref,
            baseline.as_ref().map(|old| &old.banner_ref),
            &existing.banner_ref,
        );
        if summary != incoming.summary
            || genres != incoming.genres
            || logo != incoming.logo_ref
            || banner != incoming.banner_ref
        {
            library.update_artist_enrichment(id, summary, &genres, logo, banner, true)?;
        }
    }

    let albums = live.open_table(ALBUMS_TABLE)?;
    let album_sources = live.open_table(LOCAL_ALBUMS)?;
    let live_tracks = live.open_table(ALBUM_TRACKS_TABLE)?;
    let stage_read = staged.begin_read()?;
    let stage_tracks = stage_read.open_table(ALBUM_TRACKS_TABLE)?;
    let previous_credits = live.open_table(ALBUM_CREDITS_TABLE)?;
    let current_credits = stage_read.open_table(ALBUM_CREDITS_TABLE)?;
    for (id, incoming, _) in local_albums {
        let Some(value) = albums.get(id.as_str())? else {
            continue;
        };
        let existing: Album = decode_value(value.value())?;
        let baseline = album_sources
            .get(id.as_str())?
            .map(|value| decode_value::<Album>(value.value()))
            .transpose()?;
        let same_query_identity = baseline.as_ref().map_or_else(
            || {
                // Legacy catalogs lack the prior local artist credits. The
                // current local primary must still be one of the old credits.
                same_album_title_year(incoming, &existing)
                    && existing.all_artist_ids().contains(&incoming.artist_id)
            },
            |old| {
                same_album_title_year(incoming, old)
                    && incoming.artist_id == old.artist_id
                    && incoming.artist_ids == old.artist_ids
                    && incoming
                        .artist_names
                        .iter()
                        .map(|name| artist_identity_key(name))
                        .eq(old
                            .artist_names
                            .iter()
                            .map(|name| artist_identity_key(name)))
            },
        );
        let repaired_display = if !same_query_identity
            && baseline
                .as_ref()
                .is_some_and(|old| same_album_title_year(incoming, old))
        {
            match (
                previous_credits.get(id.as_str())?,
                current_credits.get(id.as_str())?,
            ) {
                (Some(previous), Some(current)) => {
                    let previous: ArtistCredits = decode_value(previous.value())?;
                    let current: ArtistCredits = decode_value(current.value())?;
                    let key = crate::credits::display_credit_key(&current.display_name);
                    key == crate::credits::display_credit_key(&previous.display_name)
                        && resolutions.get(&key).is_some_and(|resolution| {
                            resolution
                                .credits
                                .iter()
                                .map(|credit| artist_identity_key(&credit.name))
                                .eq(incoming
                                    .artist_names
                                    .iter()
                                    .map(|name| artist_identity_key(name)))
                        })
                }
                _ => false,
            }
        } else {
            false
        };
        if !same_query_identity && !repaired_display {
            invalidated.albums.insert(id.clone());
            invalidated.album_artists.insert(id.clone());
            continue;
        }
        let prefix = prefix_key(id);
        let mut end = prefix.clone();
        end.push('\u{10ffff}');
        let previous_count = live_tracks
            .range(prefix.as_str()..end.as_str())?
            .try_fold(0usize, |count, entry| entry.map(|_| count + 1))?;
        let current_count = stage_tracks
            .range(prefix.as_str()..end.as_str())?
            .try_fold(0usize, |count, entry| entry.map(|_| count + 1))?;
        if previous_count != current_count {
            // Release matching uses track count. Keep the displayed credits
            // until resolution succeeds but allow a fresh provider lookup.
            invalidated.album_artists.insert(id.clone());
        }
        let summary = preserve_field(
            &incoming.summary,
            baseline.as_ref().map(|old| &old.summary),
            &existing.summary,
        );
        let genres = preserve_genres(
            &incoming.genres,
            baseline.as_ref().map(|old| old.genres.as_slice()),
            &existing.genres,
        );
        if summary != incoming.summary || genres != incoming.genres {
            library.update_album_enrichment(id, summary, &genres, true)?;
        }
        if !preserve_album_artists || repaired_display {
            // Rebuild old album associations from source billing once, while
            // preserving descriptions, artwork, media IDs and download links.
            invalidated.album_artists.insert(id.clone());
            continue;
        }
        {
            let mut credited = Vec::new();
            for artist_id in existing.all_artist_ids() {
                let mut artist = match library.get_artist(artist_id)? {
                    Some(artist) => artist,
                    None => match artists.get(artist_id.as_str())? {
                        Some(value) => decode_value(value.value())?,
                        None => continue,
                    },
                };
                if let Some(name) = artist_names.get(artist_id) {
                    artist.name = name.clone();
                }
                credited.push(artist);
            }
            // The existing transaction helper updates album/artist/track
            // search indices together, including added credit artists.
            let credited_ids = credited
                .iter()
                .map(|artist| artist.id.clone())
                .collect::<Vec<_>>();
            let credited_names = credited
                .iter()
                .map(|artist| artist.name.clone())
                .collect::<Vec<_>>();
            if incoming.all_artist_ids() != credited_ids || incoming.artist_names != credited_names
            {
                library.merge_album_artists(id, &credited)?;
            }
        }
    }
    Ok(invalidated)
}

fn same_album_title_year(incoming: &Album, previous: &Album) -> bool {
    incoming
        .title
        .trim()
        .eq_ignore_ascii_case(previous.title.trim())
        && incoming.year == previous.year
}

fn preserve_field(
    incoming: &Option<String>,
    baseline: Option<&Option<String>>,
    existing: &Option<String>,
) -> Option<String> {
    if baseline.map_or(incoming.is_none(), |old| incoming == old) {
        existing.clone()
    } else {
        incoming.clone()
    }
}

fn preserve_genres(
    incoming: &[String],
    baseline: Option<&[String]>,
    existing: &[String],
) -> Vec<String> {
    if baseline.is_some_and(|old| incoming == old) {
        return existing.to_vec();
    }
    let mut merged = incoming.to_vec();
    let external = existing
        .iter()
        .filter(|genre| {
            !baseline.is_some_and(|old| old.iter().any(|local| local.eq_ignore_ascii_case(genre)))
        })
        .cloned()
        .collect::<Vec<_>>();
    merge_genres(&mut merged, &external);
    merged
}

fn replace_source_records<T>(
    live: &redb::WriteTransaction,
    definition: TableDefinition<&str, &[u8]>,
    records: &[SourceRecord<T>],
) -> Result<(), LibraryError> {
    let mut table = live.open_table(definition)?;
    let current = records
        .iter()
        .map(|(id, _, _)| id.as_str())
        .collect::<std::collections::HashSet<_>>();
    let obsolete = table
        .iter()?
        .filter_map(|entry| match entry {
            Ok((key, _)) if !current.contains(key.value()) => Some(Ok(key.value().to_string())),
            Ok(_) => None,
            Err(err) => Some(Err(err)),
        })
        .collect::<Result<Vec<_>, _>>()?;
    for id in obsolete {
        table.remove(id.as_str())?;
    }
    for (id, _, bytes) in records {
        let changed = table
            .get(id.as_str())?
            .map_or(true, |old| old.value() != bytes.as_slice());
        if changed {
            table.insert(id.as_str(), bytes.as_slice())?;
        }
    }
    Ok(())
}

fn prune_external_attempts(
    live: &redb::WriteTransaction,
    staged: &redb::ReadTransaction,
    invalidated: &InvalidatedSources,
) -> Result<(), LibraryError> {
    let artists = staged.open_table(ARTISTS_TABLE)?;
    let albums = staged.open_table(ALBUMS_TABLE)?;
    let mut attempts = live.open_table(EXTERNAL_ATTEMPTS_TABLE)?;
    let mut obsolete = Vec::new();
    for entry in attempts.iter()? {
        let (key, _) = entry?;
        let retain = match key.value().split_once(':') {
            Some(("artist", id)) => artists.get(id)?.is_some() && !invalidated.artists.contains(id),
            Some(("album", id)) => albums.get(id)?.is_some() && !invalidated.albums.contains(id),
            Some(("album_artists", id)) => {
                albums.get(id)?.is_some() && !invalidated.album_artists.contains(id)
            }
            // Leave unknown caller-owned keys intact.
            _ => true,
        };
        if !retain {
            obsolete.push(key.value().to_string());
        }
    }
    for key in obsolete {
        attempts.remove(key.as_str())?;
    }
    Ok(())
}
