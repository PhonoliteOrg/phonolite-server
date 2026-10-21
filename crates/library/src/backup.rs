//! Portable metadata, independent of database layout and absolute music paths.
//! Imported catalogs remain separate from playable media and are retried by scans.
use std::collections::{BTreeMap, HashMap, HashSet};

use common::{artist_identity_key, Album, Artist, ArtistCredits, Track};
use redb::{ReadTransaction, ReadableTable, TableDefinition, TableError, WriteTransaction};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::credits::{display_credit_key, resolve_display_credit, DISPLAY_CREDIT_RESOLUTIONS};
use crate::indexing::{LOCAL_ALBUMS, SCAN_LOCK};
use crate::storage::*;
use crate::{AlbumSourceIdentity, Library, LibraryError};

const RETAINED: TableDefinition<&str, &[u8]> = TableDefinition::new("metadata_backups_v1");

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CatalogBackup {
    pub artists: Vec<Artist>,
    pub albums: Vec<BackupAlbum>,
    pub credit_resolutions: BTreeMap<String, ArtistCredits>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackupAlbum {
    /// Source identity before provider enrichment; paths are hints, never targets.
    pub source: Album,
    pub metadata: Album,
    pub credits: ArtistCredits,
    pub tracks: Vec<BackupTrack>,
    pub provider_identity: Option<AlbumSourceIdentity>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackupTrack {
    pub track: Track,
    pub credits: ArtistCredits,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct RestoreReport {
    pub matched_artists: usize,
    pub matched_albums: usize,
    pub unmatched_artists: usize,
    pub unmatched_albums: usize,
    pub ambiguous_albums: usize,
    pub updated_artists: usize,
    pub updated_albums: usize,
    pub updated_tracks: usize,
    pub library_albums_without_backup: usize,
    pub restored_credit_resolutions: usize,
    pub warnings: Vec<String>,
}

fn json_error(err: impl std::fmt::Display) -> LibraryError {
    LibraryError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        err.to_string(),
    ))
}

fn records<T: DeserializeOwned>(
    read: &ReadTransaction,
    def: TableDefinition<&str, &[u8]>,
) -> Result<BTreeMap<String, T>, LibraryError> {
    let table = match read.open_table(def) {
        Ok(table) => table,
        Err(TableError::TableDoesNotExist(_)) => return Ok(BTreeMap::new()),
        Err(err) => return Err(err.into()),
    };
    let result = table
        .iter()?
        .map(|entry| {
            let (key, value) = entry?;
            Ok((key.value().to_owned(), decode_value(value.value())?))
        })
        .collect();
    result
}

pub(super) fn retained(read: &ReadTransaction) -> Result<Vec<CatalogBackup>, LibraryError> {
    let table = match read.open_table(RETAINED) {
        Ok(table) => table,
        Err(TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let result = table
        .iter()?
        .map(|entry| {
            let (_, value) = entry?;
            serde_json::from_slice(value.value()).map_err(json_error)
        })
        .collect();
    result
}

pub(super) fn discard_retained(write: &WriteTransaction) -> Result<(), LibraryError> {
    clear_table(write, RETAINED)
}

impl Library {
    /// Explicit operator reset; ordinary root/schema rebuilds retain imports.
    pub fn discard_metadata_imports(&self) -> Result<(), LibraryError> {
        let _scan = SCAN_LOCK.lock();
        let write = self.db.begin_write()?;
        discard_retained(&write)?;
        write.commit()?;
        Ok(())
    }

    pub fn preview_metadata(
        &self,
        catalogs: &[CatalogBackup],
    ) -> Result<RestoreReport, LibraryError> {
        let _scan = SCAN_LOCK.lock();
        let write = self.db.begin_write()?;
        // Dropping the transaction rolls back every proposed field change.
        let count = import_resolutions(&write, catalogs)?;
        let mut report = apply(&write, catalogs)?;
        report.restored_credit_resolutions = count;
        Ok(report)
    }
    /// A single redb read transaction gives a consistent metadata snapshot.
    /// Retained imports are included so temporarily absent music is not lost.
    pub fn export_metadata(&self) -> Result<Vec<CatalogBackup>, LibraryError> {
        let read = self.db.begin_read()?;
        let artists = records::<Artist>(&read, ARTISTS_TABLE)?;
        let albums = records::<Album>(&read, ALBUMS_TABLE)?;
        let local = records::<Album>(&read, LOCAL_ALBUMS)?;
        let album_credits = records::<ArtistCredits>(&read, ALBUM_CREDITS_TABLE)?;
        let track_credits = records::<ArtistCredits>(&read, TRACK_CREDITS_TABLE)?;
        let mut tracks: HashMap<String, Vec<BackupTrack>> = HashMap::new();
        for (id, track) in records::<Track>(&read, TRACKS_TABLE)? {
            tracks
                .entry(track.album_id.clone())
                .or_default()
                .push(BackupTrack {
                    track,
                    credits: track_credits.get(&id).cloned().unwrap_or_default(),
                });
        }
        let mut identities = BTreeMap::new();
        match read.open_table(ALBUM_SOURCE_IDENTITY_TABLE) {
            Ok(table) => {
                for entry in table.iter()? {
                    let (key, value) = entry?;
                    if let Ok(identity) =
                        serde_json::from_slice::<AlbumSourceIdentity>(value.value())
                    {
                        identities.insert(key.value().to_owned(), identity);
                    }
                }
            }
            Err(TableError::TableDoesNotExist(_)) => {}
            Err(err) => return Err(err.into()),
        }
        let mut catalog = CatalogBackup {
            artists: artists.into_values().collect(),
            albums: Vec::new(),
            credit_resolutions: records(&read, DISPLAY_CREDIT_RESOLUTIONS)?,
        };
        for (id, album) in albums {
            let mut tracks = tracks.remove(&id).unwrap_or_default();
            sort_tracks(&mut tracks);
            catalog.albums.push(BackupAlbum {
                source: local.get(&id).unwrap_or(&album).clone(),
                metadata: album,
                credits: album_credits.get(&id).cloned().unwrap_or_default(),
                tracks,
                provider_identity: identities.remove(&id),
            });
        }
        let mut catalogs = vec![catalog];
        catalogs.extend(retained(&read)?);
        Ok(catalogs)
    }

    /// Additive and idempotent. The archive and field changes commit together.
    /// No paths, media IDs, local credits, or source file tags are overwritten.
    pub fn restore_metadata(
        &self,
        catalogs: &[CatalogBackup],
    ) -> Result<RestoreReport, LibraryError> {
        let _scan = SCAN_LOCK.lock();
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(RETAINED)?;
            for catalog in catalogs {
                let bytes = serde_json::to_vec(catalog).map_err(json_error)?;
                let key = common::stable_id(std::str::from_utf8(&bytes).map_err(json_error)?);
                table.insert(key.as_str(), bytes.as_slice())?;
            }
        }
        let count = import_resolutions(&write, catalogs)?;
        let mut report = apply(&write, catalogs)?;
        write.commit()?;
        report.restored_credit_resolutions = count;
        if count > 0 {
            // Use the normal source-driven rebuild for restored collaboration
            // repairs. Explicit local credit tags retain their normal precedence.
            match crate::indexing::scan_locked(&self.roots, &self.db, false, None, std::time::Instant::now(), None) {
                Ok(_) => {
                    let preview = self.db.begin_write()?;
                    let current = apply(&preview, catalogs)?;
                    report.matched_albums = current.matched_albums;
                    report.matched_artists = current.matched_artists;
                    report.unmatched_albums = current.unmatched_albums;
                    report.unmatched_artists = current.unmatched_artists;
                    report.ambiguous_albums = current.ambiguous_albums;
                    report.library_albums_without_backup = current.library_albums_without_backup;
                },
                Err(err) => report.warnings.push(format!("Metadata was restored, but rebuilding artist credits failed: {err}. Retry a library scan.")),
            }
        }
        Ok(report)
    }
}

fn import_resolutions(
    write: &WriteTransaction,
    catalogs: &[CatalogBackup],
) -> Result<usize, LibraryError> {
    let mut table = write.open_table(DISPLAY_CREDIT_RESOLUTIONS)?;
    let mut count = 0;
    for catalog in catalogs {
        for (key, resolution) in &catalog.credit_resolutions {
            if key != &display_credit_key(&resolution.display_name)
                || resolve_display_credit(&resolution.display_name, &resolution.credits).is_none()
                || table.get(key.as_str())?.is_some()
            {
                continue;
            }
            table.insert(key.as_str(), encode_value(resolution)?.as_slice())?;
            count += 1;
        }
    }
    if count > 0 {
        write.open_table(META_TABLE)?.insert(
            crate::indexing::DISPLAY_CREDIT_RESOLUTIONS_DIRTY,
            &[1u8][..],
        )?;
    }
    Ok(count)
}

fn text_key(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn album_key(album: &Album) -> (String, Option<i32>, Vec<String>) {
    let mut artists = album
        .artist_names
        .iter()
        .map(|name| artist_identity_key(name))
        .collect::<Vec<_>>();
    artists.sort();
    (text_key(&album.title), album.year, artists)
}

fn sort_tracks(tracks: &mut [BackupTrack]) {
    tracks.sort_by(|a, b| {
        (a.track.disc_no, a.track.track_no, text_key(&a.track.title)).cmp(&(
            b.track.disc_no,
            b.track.track_no,
            text_key(&b.track.title),
        ))
    });
}

fn same_tracks(a: &[BackupTrack], b: &[BackupTrack]) -> bool {
    !a.is_empty()
        && a.len() == b.len()
        && a.iter().zip(b).all(|(a, b)| {
            text_key(&a.track.title) == text_key(&b.track.title)
                && a.track.disc_no == b.track.disc_no
                && a.track.track_no == b.track.track_no
                && a.track.duration_ms.abs_diff(b.track.duration_ms) <= 1500
        })
}

fn relative_hint(path: &str) -> &str {
    path.split_once("::").map_or(path, |(_, path)| path)
}

fn fill_text(target: &mut Option<String>, source: &Option<String>) -> bool {
    if target.as_ref().is_none_or(|value| value.trim().is_empty()) {
        if let Some(value) = source.as_ref().filter(|value| !value.trim().is_empty()) {
            *target = Some(value.clone());
            return true;
        }
    }
    false
}

fn fill_genres(target: &mut Vec<String>, source: &[String]) -> bool {
    if target.is_empty() && source.iter().any(|value| !value.trim().is_empty()) {
        crate::normalization::merge_genres(target, source);
        return !target.is_empty();
    }
    false
}

/// Works on the live restore transaction or on a staged scan transaction.
pub(super) fn apply(
    write: &WriteTransaction,
    catalogs: &[CatalogBackup],
) -> Result<RestoreReport, LibraryError> {
    let mut report = RestoreReport::default();
    let mut artists = write.open_table(ARTISTS_TABLE)?;
    let mut artist_ids: HashMap<String, Vec<String>> = HashMap::new();
    for entry in artists.iter()? {
        let (id, value) = entry?;
        let artist: Artist = decode_value(value.value())?;
        artist_ids
            .entry(artist_identity_key(&artist.name))
            .or_default()
            .push(id.value().to_owned());
    }
    let mut albums = write.open_table(ALBUMS_TABLE)?;
    let mut tracks = write.open_table(TRACKS_TABLE)?;
    let mut by_album: HashMap<String, Vec<BackupTrack>> = HashMap::new();
    for entry in tracks.iter()? {
        let (_, value) = entry?;
        let track: Track = decode_value(value.value())?;
        by_album
            .entry(track.album_id.clone())
            .or_default()
            .push(BackupTrack {
                track,
                credits: ArtistCredits::default(),
            });
    }
    for tracks in by_album.values_mut() {
        sort_tracks(tracks);
    }
    let local = write.open_table(LOCAL_ALBUMS)?;
    let mut album_ids: HashMap<_, Vec<Album>> = HashMap::new();
    for entry in albums.iter()? {
        let (id, value) = entry?;
        let album: Album = decode_value(value.value())?;
        let source: Album = local
            .get(id.value())?
            .map(|value| decode_value(value.value()))
            .transpose()?
            .unwrap_or(album.clone());
        album_ids.entry(album_key(&source)).or_default().push(album);
    }
    let mut matched_albums = HashSet::new();
    let mut seen_artists = HashSet::new();
    let mut seen_albums = HashSet::new();
    let mut identities = write.open_table(ALBUM_SOURCE_IDENTITY_TABLE)?;
    for catalog in catalogs {
        for saved in &catalog.artists {
            // Repeated retained snapshots must not inflate the report.
            let identity = artist_identity_key(&saved.name);
            let candidates = artist_ids
                .get(&identity)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let count = seen_artists.insert(identity);
            let [id] = candidates else {
                report.unmatched_artists += usize::from(count);
                continue;
            };
            report.matched_artists += usize::from(count);
            let mut artist: Artist = decode_value(artists.get(id.as_str())?.unwrap().value())?;
            let mut changed = fill_text(&mut artist.summary, &saved.summary);
            changed |= fill_genres(&mut artist.genres, &saved.genres);
            changed |= fill_text(&mut artist.logo_ref, &saved.logo_ref);
            changed |= fill_text(&mut artist.banner_ref, &saved.banner_ref);
            if changed {
                artists.insert(id.as_str(), encode_value(&artist)?.as_slice())?;
                report.updated_artists += 1;
            }
        }
        for saved in &catalog.albums {
            let mut saved_tracks = saved.tracks.clone();
            sort_tracks(&mut saved_tracks);
            let key = album_key(&saved.source);
            let candidates = album_ids.get(&key).map(Vec::as_slice).unwrap_or_default();
            let mut candidates = candidates
                .iter()
                .filter(|album| {
                    by_album
                        .get(&album.id)
                        .is_some_and(|tracks| same_tracks(&saved_tracks, tracks))
                })
                .collect::<Vec<_>>();
            if candidates.len() > 1 {
                let hints = candidates
                    .iter()
                    .copied()
                    .filter(|album| {
                        relative_hint(&album.folder_relpath)
                            == relative_hint(&saved.source.folder_relpath)
                    })
                    .collect::<Vec<_>>();
                if hints.len() == 1 {
                    candidates = hints;
                }
            }
            let unique = seen_albums.insert((saved.source.id.clone(), key));
            let [candidate] = candidates.as_slice() else {
                if unique {
                    if candidates.is_empty() {
                        report.unmatched_albums += 1;
                    } else {
                        report.ambiguous_albums += 1;
                    }
                }
                continue;
            };
            report.matched_albums += usize::from(unique);
            matched_albums.insert(candidate.id.clone());
            let mut album: Album =
                decode_value(albums.get(candidate.id.as_str())?.unwrap().value())?;
            let mut changed = fill_text(&mut album.summary, &saved.metadata.summary);
            changed |= fill_genres(&mut album.genres, &saved.metadata.genres);
            if changed {
                albums.insert(album.id.as_str(), encode_value(&album)?.as_slice())?;
                report.updated_albums += 1;
            }
            // Proof is still checked by the normal source-context reader.
            if let Some(identity) = &saved.provider_identity {
                if identity.validate().is_ok() && identities.get(album.id.as_str())?.is_none() {
                    identities.insert(
                        album.id.as_str(),
                        serde_json::to_vec(identity).map_err(json_error)?.as_slice(),
                    )?;
                }
            }
            for (source, target) in saved_tracks.iter().zip(&by_album[&album.id]) {
                let mut track: Track =
                    decode_value(tracks.get(target.track.id.as_str())?.unwrap().value())?;
                if fill_genres(&mut track.genres, &source.track.genres) {
                    tracks.insert(track.id.as_str(), encode_value(&track)?.as_slice())?;
                    report.updated_tracks += 1;
                }
            }
        }
    }
    report.library_albums_without_backup = albums.len()? as usize - matched_albums.len();
    // Recompute cached completeness in this same transaction.
    let mut stats = crate::AlbumMetadataStats::default();
    for entry in albums.iter()? {
        let (_, value) = entry?;
        let album: Album = decode_value(value.value())?;
        stats.total += 1;
        stats.completed += usize::from(album.has_enriched_metadata());
    }
    write
        .open_table(META_TABLE)?
        .insert(META_ALBUM_METADATA_KEY, encode_value(&stats)?.as_slice())?;
    Ok(report)
}
