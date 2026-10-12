//! Metadata reconciliation and retry state, including atomic cross-index updates.
//!
//! Network requests remain outside this module: callers supply enrichment values.
use std::collections::HashSet;
use std::time::Duration;

use common::{artist_identity_key, Album, Artist, Track};
use redb::{ReadableTable, TableError};
use serde::{Deserialize, Serialize};

use crate::credits::track_credit_search_text;
use crate::indexing::LOCAL_ALBUMS;
use crate::normalization::{clean_summary, merge_genres};
use crate::storage::{
    album_index_key, album_name_key, album_search_artist_text, artist_name_key, decode_value,
    encode_value, prefix_key, split_key_last, track_name_key, ALBUMS_BY_NAME_TABLE, ALBUMS_TABLE,
    ALBUM_CREDITS_TABLE, ALBUM_TRACKS_TABLE, ARTISTS_BY_NAME_TABLE, ARTISTS_TABLE,
    ARTIST_ALBUMS_TABLE, EXTERNAL_ATTEMPTS_TABLE, META_ALBUM_METADATA_KEY, META_STATS_KEY,
    META_TABLE, TRACKS_BY_NAME_TABLE, TRACKS_TABLE, TRACK_CREDITS_TABLE,
};
use crate::{now_secs, AlbumMetadataStats, Library, LibraryError, LibraryStats};

pub(super) fn album_metadata_stats(
    db: &redb::Database,
) -> Result<AlbumMetadataStats, LibraryError> {
    {
        let read = db.begin_read()?;
        let meta = read.open_table(META_TABLE)?;
        if let Some(value) = meta.get(META_ALBUM_METADATA_KEY)? {
            return decode_value(value.value());
        };
    }
    // Upgrade existing indexes once. Recheck under the writer lock so concurrent
    // enrichments and status requests cannot publish stale counts.
    let write = db.begin_write()?;
    let stats = {
        let mut meta = write.open_table(META_TABLE)?;
        if let Some(value) = meta.get(META_ALBUM_METADATA_KEY)? {
            return decode_value(value.value());
        }
        let albums = write.open_table(ALBUMS_TABLE)?;
        let mut stats = AlbumMetadataStats::default();
        for entry in albums.iter()? {
            let (_, value) = entry?;
            let album: Album = decode_value(value.value())?;
            stats.total += 1;
            stats.completed += usize::from(album.has_enriched_metadata());
        }
        let bytes = encode_value(&stats)?;
        meta.insert(META_ALBUM_METADATA_KEY, bytes.as_slice())?;
        stats
    };
    write.commit()?;
    Ok(stats)
}

impl Library {
    pub fn album_metadata_stats(&self) -> Result<AlbumMetadataStats, LibraryError> {
        album_metadata_stats(&self.db)
    }

    pub fn update_artist_enrichment(
        &self,
        artist_id: &str,
        summary: Option<String>,
        genres: &[String],
        logo_ref: Option<String>,
        banner_ref: Option<String>,
        replace: bool,
    ) -> Result<bool, LibraryError> {
        let write_txn = self.db.begin_write()?;
        let updated = {
            let mut artist_table = write_txn.open_table(ARTISTS_TABLE)?;
            let mut artist: Artist = match artist_table.get(artist_id)? {
                Some(value) => decode_value(value.value())?,
                None => return Ok(false),
            };

            let mut updated = false;
            if let Some(summary) = summary.and_then(clean_summary) {
                let should_update = replace
                    || artist
                        .summary
                        .as_ref()
                        .map(|s| s.trim().is_empty())
                        .unwrap_or(true);
                if should_update && artist.summary.as_deref() != Some(summary.as_str()) {
                    artist.summary = Some(summary);
                    updated = true;
                }
            }
            if !genres.is_empty() {
                if replace {
                    let mut normalized = Vec::new();
                    merge_genres(&mut normalized, genres);
                    if artist.genres != normalized {
                        artist.genres = normalized;
                        updated = true;
                    }
                } else {
                    let before = artist.genres.clone();
                    merge_genres(&mut artist.genres, genres);
                    if artist.genres != before {
                        updated = true;
                    }
                }
            }
            if let Some(logo_ref) = logo_ref {
                if artist.logo_ref.as_deref() != Some(logo_ref.as_str()) {
                    artist.logo_ref = Some(logo_ref);
                    updated = true;
                }
            }
            if let Some(banner_ref) = banner_ref {
                if artist.banner_ref.as_deref() != Some(banner_ref.as_str()) {
                    artist.banner_ref = Some(banner_ref);
                    updated = true;
                }
            }

            if updated {
                let artist_bytes = encode_value(&artist)?;
                artist_table.insert(artist_id, artist_bytes.as_slice())?;
            }
            updated
        };

        if updated {
            write_txn.commit()?;
        }
        Ok(updated)
    }

    pub fn update_album_enrichment(
        &self,
        album_id: &str,
        summary: Option<String>,
        genres: &[String],
        replace: bool,
    ) -> Result<bool, LibraryError> {
        let write_txn = self.db.begin_write()?;
        let (updated, was_complete, is_complete) = {
            let mut album_table = write_txn.open_table(ALBUMS_TABLE)?;
            let mut album: Album = match album_table.get(album_id)? {
                Some(value) => decode_value(value.value())?,
                None => return Ok(false),
            };

            let was_complete = album.has_enriched_metadata();

            let mut updated = false;
            let should_update_summary = replace
                || album
                    .summary
                    .as_ref()
                    .map(|s| s.trim().is_empty())
                    .unwrap_or(true);
            if should_update_summary {
                if let Some(summary) = summary.and_then(clean_summary) {
                    if album.summary.as_deref() != Some(summary.as_str()) {
                        album.summary = Some(summary);
                        updated = true;
                    }
                }
            }
            if !genres.is_empty() {
                if replace {
                    let mut normalized = Vec::new();
                    merge_genres(&mut normalized, genres);
                    if album.genres != normalized {
                        album.genres = normalized;
                        updated = true;
                    }
                } else {
                    let before = album.genres.clone();
                    merge_genres(&mut album.genres, genres);
                    if album.genres != before {
                        updated = true;
                    }
                }
            }

            if updated {
                let album_bytes = encode_value(&album)?;
                album_table.insert(album_id, album_bytes.as_slice())?;
            }
            (updated, was_complete, album.has_enriched_metadata())
        };

        if updated {
            if was_complete != is_complete {
                let mut meta = write_txn.open_table(META_TABLE)?;
                let stats = meta
                    .get(META_ALBUM_METADATA_KEY)?
                    .map(|value| decode_value::<AlbumMetadataStats>(value.value()))
                    .transpose()?;
                if let Some(mut stats) = stats {
                    if is_complete {
                        stats.completed += 1;
                    } else {
                        stats.completed = stats.completed.saturating_sub(1);
                    }
                    let bytes = encode_value(&stats)?;
                    meta.insert(META_ALBUM_METADATA_KEY, bytes.as_slice())?;
                }
            }
            write_txn.commit()?;
        }
        Ok(updated)
    }

    pub fn merge_album_artists(
        &self,
        album_id: &str,
        artists: &[Artist],
    ) -> Result<bool, LibraryError> {
        let mut artists = normalize_album_artists(artists);
        if artists.is_empty() {
            return Ok(false);
        }

        let write_txn = self.db.begin_write()?;
        let (updated, new_artist_count) = {
            let mut meta_table = write_txn.open_table(META_TABLE)?;
            let mut artists_table = write_txn.open_table(ARTISTS_TABLE)?;
            let mut artists_by_name_table = write_txn.open_table(ARTISTS_BY_NAME_TABLE)?;
            let mut albums_table = write_txn.open_table(ALBUMS_TABLE)?;
            let mut albums_by_name_table = write_txn.open_table(ALBUMS_BY_NAME_TABLE)?;
            let mut artist_albums_table = write_txn.open_table(ARTIST_ALBUMS_TABLE)?;
            let mut tracks_table = write_txn.open_table(TRACKS_TABLE)?;
            let mut tracks_by_name_table = write_txn.open_table(TRACKS_BY_NAME_TABLE)?;
            let album_tracks_table = write_txn.open_table(ALBUM_TRACKS_TABLE)?;

            let mut album: Album = match albums_table.get(album_id)? {
                Some(value) => decode_value(value.value())?,
                None => return Ok(false),
            };

            let local_album = {
                let sources = write_txn.open_table(LOCAL_ALBUMS)?;
                let value = sources
                    .get(album_id)?
                    .map(|value| decode_value::<Album>(value.value()))
                    .transpose()?;
                value.unwrap_or_else(|| album.clone())
            };
            let local_primary_name = local_album
                .artist_names
                .first()
                .cloned()
                .filter(|name| !name.trim().is_empty())
                .or_else(|| {
                    artists_table
                        .get(local_album.artist_id.as_str())
                        .ok()
                        .flatten()
                        .and_then(|value| decode_value::<Artist>(value.value()).ok())
                        .map(|artist| artist.name)
                })
                .unwrap_or_default();
            let local_primary_key = artist_identity_key(&local_primary_name);
            let Some(primary_index) = artists.iter().position(|artist| {
                !local_primary_key.is_empty()
                    && artist_identity_key(&artist.name) == local_primary_key
            }) else {
                // Provider credits cannot replace the primary identity from
                // local tags, including when a title matches another release.
                return Ok(false);
            };
            let mut primary_artist = artists.remove(primary_index);
            primary_artist.id = local_album.artist_id.clone();
            artists.retain(|artist| {
                artist.id != primary_artist.id
                    && artist_identity_key(&artist.name) != local_primary_key
            });
            artists.insert(0, primary_artist);

            let old_artist_ids = album.all_artist_ids().to_vec();
            let mut track_artist_ids = HashSet::new();
            let mut source_contributor_ids = HashSet::new();
            let local_headline_credits = {
                let source = write_txn.open_table(ALBUM_CREDITS_TABLE)?;
                let credits = source
                    .get(album.id.as_str())?
                    .map(|value| decode_value::<common::ArtistCredits>(value.value()))
                    .transpose()?
                    .map(|credits| {
                        credits
                            .credits
                            .into_iter()
                            .filter(|credit| credit.role == "album_artist")
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                credits
            };
            let headline_ids: HashSet<_> = local_headline_credits
                .iter()
                .map(|credit| credit.artist_id.clone())
                .collect();
            let track_sources = write_txn.open_table(TRACK_CREDITS_TABLE)?;
            let prefix = prefix_key(album.id.as_str());
            let mut end = prefix.clone();
            end.push('\u{10ffff}');
            for entry in album_tracks_table.range(prefix.as_str()..end.as_str())? {
                let entry = entry?;
                let (_, track_id) = split_key_last(entry.0.value())?;
                if let Some(value) = tracks_table.get(track_id)? {
                    track_artist_ids.insert(decode_value::<Track>(value.value())?.artist_id);
                }
                if let Some(value) = track_sources.get(track_id)? {
                    source_contributor_ids.extend(
                        decode_value::<common::ArtistCredits>(value.value())?
                            .credits
                            .into_iter()
                            .map(|credit| credit.artist_id),
                    );
                }
            }
            // Independently tagged guests and role contributors cannot be
            // promoted into the release's headline by provider album credits.
            artists.retain(|artist| {
                artist.id == local_album.artist_id
                    || headline_ids.contains(&artist.id)
                    || !source_contributor_ids.contains(&artist.id)
            });
            // A provider may refine a joint credit's spelling, but omitting a
            // collaborator cannot remove the explicit identity from local tags.
            let mut joint_artists = Vec::new();
            for credit in &local_headline_credits {
                let matching = artists.iter().position(|artist| {
                    artist_identity_key(&artist.name) == artist_identity_key(&credit.name)
                });
                let mut artist = if let Some(index) = matching {
                    artists.remove(index)
                } else if let Some(value) = artists_table.get(credit.artist_id.as_str())? {
                    decode_value::<Artist>(value.value())?
                } else {
                    continue;
                };
                artist.id = credit.artist_id.clone();
                joint_artists.push(artist);
            }
            if !joint_artists.is_empty() {
                joint_artists.extend(artists);
                artists = joint_artists;
            }
            let old_album_name_key = album_name_key(&album_search_artist_text(&album), &album);
            let old_primary_artist_id = album.artist_id.clone();
            let old_primary_artist_name = album
                .artist_names
                .first()
                .cloned()
                .filter(|name| !name.trim().is_empty())
                .or_else(|| {
                    artists_table
                        .get(old_primary_artist_id.as_str())
                        .ok()
                        .flatten()
                        .and_then(|value| decode_value::<Artist>(value.value()).ok())
                        .map(|artist| artist.name)
                        .filter(|name| !name.trim().is_empty())
                })
                .unwrap_or_default();
            let new_artist_ids = artists
                .iter()
                .map(|artist| artist.id.clone())
                .collect::<Vec<_>>();
            let new_artist_names = artists
                .iter()
                .map(|artist| artist.name.clone())
                .collect::<Vec<_>>();

            let mut new_artist_count = 0usize;
            for artist in &artists {
                let existing = match artists_table.get(artist.id.as_str())? {
                    Some(value) => Some(decode_value::<Artist>(value.value())?),
                    None => None,
                };
                match existing {
                    Some(mut existing) => {
                        let mut changed = false;
                        let old_artist_name_key = if !existing.name.trim().is_empty() {
                            Some(artist_name_key(&existing.name, &existing.id))
                        } else {
                            None
                        };
                        let existing_key = artist_identity_key(&existing.name);
                        let incoming_key = artist_identity_key(&artist.name);
                        if !artist.name.trim().is_empty()
                            && existing.name.trim() != artist.name.trim()
                            && (existing.name.trim().is_empty()
                                || (!existing_key.is_empty() && existing_key == incoming_key))
                        {
                            existing.name = artist.name.clone();
                            changed = true;
                        }
                        if existing.summary.is_none() && artist.summary.is_some() {
                            existing.summary = artist.summary.clone();
                            changed = true;
                        }
                        if existing.logo_ref.is_none() && artist.logo_ref.is_some() {
                            existing.logo_ref = artist.logo_ref.clone();
                            changed = true;
                        }
                        if existing.banner_ref.is_none() && artist.banner_ref.is_some() {
                            existing.banner_ref = artist.banner_ref.clone();
                            changed = true;
                        }
                        let genres_before = existing.genres.clone();
                        merge_genres(&mut existing.genres, &artist.genres);
                        if existing.genres != genres_before {
                            changed = true;
                        }
                        if changed {
                            let bytes = encode_value(&existing)?;
                            artists_table.insert(existing.id.as_str(), bytes.as_slice())?;
                        }
                        if !existing.name.trim().is_empty() {
                            let key = artist_name_key(&existing.name, &existing.id);
                            if old_artist_name_key.as_ref() != Some(&key) {
                                if let Some(old_key) = old_artist_name_key {
                                    let _ = artists_by_name_table.remove(old_key.as_str())?;
                                }
                            }
                            artists_by_name_table.insert(key.as_str(), existing.id.as_bytes())?;
                        }
                    }
                    None => {
                        let bytes = encode_value(artist)?;
                        artists_table.insert(artist.id.as_str(), bytes.as_slice())?;
                        if !artist.name.trim().is_empty() {
                            let key = artist_name_key(&artist.name, &artist.id);
                            artists_by_name_table.insert(key.as_str(), artist.id.as_bytes())?;
                        }
                        new_artist_count = new_artist_count.saturating_add(1);
                    }
                }
            }

            let mut updated = false;
            let primary_artist_id = new_artist_ids[0].clone();
            let primary_artist_name = new_artist_names.first().cloned().unwrap_or_default();
            if album.artist_id != primary_artist_id {
                album.artist_id = primary_artist_id.clone();
                updated = true;
            }
            if album.artist_ids != new_artist_ids {
                album.artist_ids = new_artist_ids.clone();
                updated = true;
            }
            if album.artist_names != new_artist_names {
                album.artist_names = new_artist_names;
                updated = true;
            }

            let new_album_name_key = album_name_key(&album_search_artist_text(&album), &album);
            if old_album_name_key != new_album_name_key {
                let _ = albums_by_name_table.remove(old_album_name_key.as_str())?;
                albums_by_name_table.insert(new_album_name_key.as_str(), album.id.as_bytes())?;
            }

            if old_artist_ids != album.all_artist_ids() {
                for artist_id in &old_artist_ids {
                    if track_artist_ids.contains(artist_id) {
                        continue;
                    }
                    let key = album_index_key(artist_id, &album);
                    let _ = artist_albums_table.remove(key.as_str())?;
                }
                for artist_id in album.all_artist_ids() {
                    let key = album_index_key(artist_id, &album);
                    artist_albums_table.insert(key.as_str(), album.id.as_bytes())?;
                }
            }

            if old_primary_artist_id != primary_artist_id
                || old_primary_artist_name != primary_artist_name
            {
                let prefix = prefix_key(album.id.as_str());
                let mut end = prefix.clone();
                end.push('\u{10ffff}');
                for entry in album_tracks_table.range(prefix.as_str()..end.as_str())? {
                    let entry = entry?;
                    let key = entry.0.value();
                    let (_, track_id) = split_key_last(key)?;
                    let mut track: Track = {
                        let Some(value) = tracks_table.get(track_id)? else {
                            continue;
                        };
                        decode_value(value.value())?
                    };
                    // Album credit updates cannot relabel tracks credited to
                    // an individual performer in the source tags.
                    if track.artist_id != old_primary_artist_id {
                        continue;
                    }
                    let source_search_name = track_sources
                        .get(track.id.as_str())?
                        .map(|value| decode_value::<common::ArtistCredits>(value.value()))
                        .transpose()?
                        .map(|credits| track_credit_search_text(&credits));
                    let old_track_name_key = track_name_key(
                        source_search_name
                            .as_deref()
                            .unwrap_or(&old_primary_artist_name),
                        &album.title,
                        track.disc_no,
                        track.track_no,
                        &track.title,
                        &track.id,
                    );
                    let new_track_name_key = track_name_key(
                        source_search_name
                            .as_deref()
                            .unwrap_or(&primary_artist_name),
                        &album.title,
                        track.disc_no,
                        track.track_no,
                        &track.title,
                        &track.id,
                    );

                    if track.artist_id != primary_artist_id {
                        track.artist_id = primary_artist_id.clone();
                        updated = true;
                    }
                    if old_track_name_key != new_track_name_key {
                        let _ = tracks_by_name_table.remove(old_track_name_key.as_str())?;
                        tracks_by_name_table
                            .insert(new_track_name_key.as_str(), track.id.as_bytes())?;
                        updated = true;
                    }

                    let bytes = encode_value(&track)?;
                    tracks_table.insert(track.id.as_str(), bytes.as_slice())?;
                }
            }

            if updated {
                let bytes = encode_value(&album)?;
                albums_table.insert(album.id.as_str(), bytes.as_slice())?;
            }

            if new_artist_count > 0 {
                let mut stats: LibraryStats = match meta_table.get(META_STATS_KEY)? {
                    Some(value) => decode_value(value.value())?,
                    None => LibraryStats {
                        artists: 0,
                        albums: 0,
                        tracks: 0,
                    },
                };
                stats.artists = stats.artists.saturating_add(new_artist_count);
                let bytes = encode_value(&stats)?;
                meta_table.insert(META_STATS_KEY, bytes.as_slice())?;
            }

            (updated || new_artist_count > 0, new_artist_count)
        };

        if updated || new_artist_count > 0 {
            write_txn.commit()?;
        }

        Ok(updated)
    }

    pub fn should_attempt_external(
        &self,
        key: &str,
        min_interval: Duration,
    ) -> Result<bool, LibraryError> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(EXTERNAL_ATTEMPTS_TABLE) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(true),
            Err(err) => return Err(err.into()),
        };
        let attempt = match table.get(key)? {
            Some(value) => decode_value::<ExternalAttempt>(value.value())?,
            None => return Ok(true),
        };
        let now = now_secs();
        let min_interval = min_interval.as_secs();
        Ok(now.saturating_sub(attempt.last_attempt) >= min_interval)
    }

    pub fn record_external_attempt(&self, key: &str, success: bool) -> Result<(), LibraryError> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(EXTERNAL_ATTEMPTS_TABLE)?;
            let now = now_secs();
            let mut record = match table.get(key)? {
                Some(value) => decode_value::<ExternalAttempt>(value.value())?,
                None => ExternalAttempt {
                    last_attempt: now,
                    last_success: None,
                },
            };
            record.last_attempt = now;
            if success {
                record.last_success = Some(now);
            }
            let bytes = encode_value(&record)?;
            table.insert(key, bytes.as_slice())?;
        }
        write_txn.commit()?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ExternalAttempt {
    last_attempt: u64,
    last_success: Option<u64>,
}

fn normalize_album_artists(artists: &[Artist]) -> Vec<Artist> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for artist in artists {
        let id = artist.id.trim();
        let name = artist.name.trim();
        if id.is_empty() || name.is_empty() {
            continue;
        }
        if !seen.insert(id.to_string()) {
            continue;
        }
        out.push(Artist {
            id: id.to_string(),
            name: name.to_string(),
            genres: artist.genres.clone(),
            summary: artist.summary.clone(),
            logo_ref: artist.logo_ref.clone(),
            banner_ref: artist.banner_ref.clone(),
        });
    }
    out
}
