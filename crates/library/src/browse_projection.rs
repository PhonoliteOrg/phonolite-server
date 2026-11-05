//! Disposable artist-browser indexes. Full biographies/release graphs never
//! enter this read model. Source catalog tables remain authoritative.
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use common::{Artist, ArtistCredits};
use parking_lot::Mutex;
use redb::{ReadableTable, TableDefinition, TableError, WriteTransaction};
use serde::{Deserialize, Serialize};

use crate::storage::{
    decode_value, encode_value, prefix_key, split_key_last, ALBUMS_TABLE, ALBUM_CREDITS_TABLE,
    ALBUM_TRACKS_TABLE, ARTISTS_BY_NAME_TABLE, ARTISTS_TABLE, ARTIST_ALBUMS_TABLE, TRACKS_TABLE,
    TRACK_CREDITS_TABLE,
};
use crate::{Library, LibraryError};

const NAMES: TableDefinition<&str, u8> = TableDefinition::new("artist_browse_names_v1");
const VALUES: TableDefinition<&str, &[u8]> = TableDefinition::new("artist_browse_values_v1");
const ELIGIBLE: TableDefinition<&str, u8> = TableDefinition::new("artist_browse_eligible_v1");

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtistBrowseSummary {
    pub id: String,
    pub name: String,
    pub genres: Vec<String>,
    pub logo_ref: Option<String>,
    pub album_count: usize,
    pub release_count: usize,
    pub collab_album_count: usize,
    pub appearance_count: usize,
    pub other_credit_count: usize,
}

impl From<&Artist> for ArtistBrowseSummary {
    fn from(artist: &Artist) -> Self {
        Self {
            id: artist.id.clone(),
            name: artist.name.clone(),
            genres: artist.genres.clone(),
            logo_ref: artist.logo_ref.clone(),
            album_count: 0,
            release_count: 0,
            collab_album_count: 0,
            appearance_count: 0,
            other_credit_count: 0,
        }
    }
}

#[derive(Default)]
pub(super) struct ProjectionState {
    generation: AtomicU64,
    build: Mutex<()>,
}

impl ProjectionState {
    pub(super) fn invalidate(&self) {
        self.generation.store(0, Ordering::Release);
    }
}

/// Called within the same transaction as a metadata update. A table created
/// before the first materialization is harmless: the full build replaces it.
pub(super) fn refresh_artist(
    transaction: &WriteTransaction,
    artist: &Artist,
) -> Result<(), LibraryError> {
    let mut values = transaction.open_table(VALUES)?;
    let summary = values
        .get(artist.id.as_str())?
        .map(|value| decode_value::<ArtistBrowseSummary>(value.value()))
        .transpose()?;
    if let Some(mut summary) = summary {
        summary.name = artist.name.clone();
        summary.genres = artist.genres.clone();
        summary.logo_ref = artist.logo_ref.clone();
        let bytes = encode_value(&summary)?;
        values.insert(artist.id.as_str(), bytes.as_slice())?;
    }
    Ok(())
}

impl Library {
    /// Exact total and stable name ordering without recomputing credits or
    /// loading full artist/release objects for each requested page.
    pub fn artist_browse_page(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<ArtistBrowseSummary>, usize), LibraryError> {
        self.check_read_cancelled()?;
        self.ensure_browse_projection()?;
        let read = self.db.begin_read()?;
        let names = read.open_table(NAMES)?;
        let values = read.open_table(VALUES)?;
        let total = names.len()? as usize;
        let mut result = Vec::with_capacity(limit.min(total.saturating_sub(offset)));
        for entry in names.iter()?.skip(offset).take(limit) {
            self.check_read_cancelled()?;
            let (key, _) = entry?;
            let (_, id) = split_key_last(key.value())?;
            if let Some(value) = values.get(id)? {
                result.push(decode_value(value.value())?);
            }
        }
        Ok((result, total))
    }

    fn ensure_browse_projection(&self) -> Result<(), LibraryError> {
        if self.browse_projection.generation.load(Ordering::Acquire) == self.catalog_generation() {
            return Ok(());
        }
        let _build = loop {
            self.check_cancelled()?;
            if let Some(guard) = self.browse_projection.build.try_lock() {
                break guard;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        if self.browse_projection.generation.load(Ordering::Acquire) == self.catalog_generation() {
            return Ok(());
        }
        self.check_cancelled()?;
        let generation = self.catalog_generation();
        let write = crate::begin_write_cancellable(&self.db, &self.cancellation)?;
        let source = self.db.begin_read()?;
        write.delete_table(NAMES)?;
        write.delete_table(VALUES)?;
        write.delete_table(ELIGIBLE)?;
        {
            // Eligibility is staged on disk instead of retaining every billed
            // ID or complete source credit graph in server memory.
            let mut eligible = write.open_table(ELIGIBLE)?;
            for entry in source.open_table(ALBUMS_TABLE)?.iter()? {
                self.check_cancelled()?;
                let (_, value) = entry?;
                let album: common::Album = decode_value(value.value())?;
                for id in album.all_artist_ids() {
                    eligible.insert(id.as_str(), 0)?;
                }
            }
            match source.open_table(TRACK_CREDITS_TABLE) {
                Ok(credits) => {
                    for entry in credits.iter()? {
                        self.check_cancelled()?;
                        let (_, value) = entry?;
                        let credits: ArtistCredits = decode_value(value.value())?;
                        for credit in credits.credits {
                            if credit.is_billed_artist() {
                                eligible.insert(credit.artist_id.as_str(), 0)?;
                            }
                        }
                    }
                }
                Err(TableError::TableDoesNotExist(_)) => {
                    for entry in source.open_table(TRACKS_TABLE)?.iter()? {
                        self.check_cancelled()?;
                        let (_, value) = entry?;
                        let track: common::Track = decode_value(value.value())?;
                        eligible.insert(track.artist_id.as_str(), 0)?;
                    }
                }
                Err(error) => return Err(error.into()),
            }
            let artists = source.open_table(ARTISTS_TABLE)?;
            let relationships = source.open_table(ARTIST_ALBUMS_TABLE)?;
            let mut names = write.open_table(NAMES)?;
            let mut values = write.open_table(VALUES)?;
            for entry in source.open_table(ARTISTS_BY_NAME_TABLE)?.iter()? {
                self.check_cancelled()?;
                let (name, _) = entry?;
                let (_, id) = split_key_last(name.value())?;
                if eligible.get(id)?.is_none() {
                    continue;
                }
                let start = prefix_key(id);
                let end = format!("{start}\u{10ffff}");
                if relationships
                    .range(start.as_str()..end.as_str())?
                    .next()
                    .transpose()?
                    .is_none()
                {
                    continue;
                }
                if let Some(value) = artists.get(id)? {
                    let artist: Artist = decode_value(value.value())?;
                    let bytes = encode_value(&ArtistBrowseSummary::from(&artist))?;
                    names.insert(name.value(), 0)?;
                    values.insert(id, bytes.as_slice())?;
                }
            }
        }
        self.aggregate_browse_counts(&write, &source)?;
        write.delete_table(ELIGIBLE)?;
        self.check_cancelled()?;
        write.commit_cancellable(&self.cancellation)?;
        if self.catalog_generation() == generation {
            self.browse_projection
                .generation
                .store(generation, Ordering::Release);
        }
        Ok(())
    }

    fn aggregate_browse_counts(
        &self,
        write: &WriteTransaction,
        source: &redb::ReadTransaction,
    ) -> Result<(), LibraryError> {
        #[derive(Default)]
        struct Roles {
            headline: bool,
            performing: bool,
            supporting: bool,
        }

        let album_credits = match source.open_table(ALBUM_CREDITS_TABLE) {
            Ok(table) => Some(table),
            Err(TableError::TableDoesNotExist(_)) => None,
            Err(error) => return Err(error.into()),
        };
        let track_credits = match source.open_table(TRACK_CREDITS_TABLE) {
            Ok(table) => Some(table),
            Err(TableError::TableDoesNotExist(_)) => None,
            Err(error) => return Err(error.into()),
        };
        let album_tracks = source.open_table(ALBUM_TRACKS_TABLE)?;
        let tracks = source.open_table(TRACKS_TABLE)?;
        let relationships = source.open_table(ARTIST_ALBUMS_TABLE)?;
        let mut values = write.open_table(VALUES)?;
        for entry in source.open_table(ALBUMS_TABLE)?.iter()? {
            self.check_cancelled()?;
            let (_, value) = entry?;
            let album: common::Album = decode_value(value.value())?;
            // Retain one album's role flags, without full release graphs.
            let mut roles = HashMap::<String, Roles>::new();
            let mut headlines: HashSet<String> = album.all_artist_ids().iter().cloned().collect();
            let credits = match &album_credits {
                Some(table) => table.get(album.id.as_str())?,
                None => None,
            };
            if let Some(value) = credits {
                let credits: ArtistCredits = decode_value(value.value())?;
                for credit in credits.credits {
                    if credit.is_album_artist() {
                        headlines.insert(credit.artist_id);
                    } else {
                        roles.entry(credit.artist_id).or_default().supporting = true;
                    }
                }
            }
            for id in &headlines {
                roles.entry(id.clone()).or_default().headline = true;
            }
            let start = prefix_key(&album.id);
            let end = format!("{start}\u{10ffff}");
            for entry in album_tracks.range(start.as_str()..end.as_str())? {
                self.check_cancelled()?;
                let (key, _) = entry?;
                let (_, track_id) = split_key_last(key.value())?;
                let credits = match &track_credits {
                    Some(table) => table.get(track_id)?,
                    None => None,
                };
                if let Some(value) = credits {
                    let credits: ArtistCredits = decode_value(value.value())?;
                    for credit in credits.credits {
                        let flags = roles.entry(credit.artist_id).or_default();
                        if matches!(credit.role.as_str(), "track_artist" | "featured_artist") {
                            flags.performing = true;
                        } else {
                            flags.supporting = true;
                        }
                    }
                } else if let Some(value) = tracks.get(track_id)? {
                    let track: common::Track = decode_value(value.value())?;
                    roles.entry(track.artist_id).or_default().performing = true;
                }
            }
            for (id, roles) in roles {
                if relationships
                    .get(crate::storage::album_index_key(&id, &album).as_str())?
                    .is_none()
                {
                    continue;
                }
                let summary = values
                    .get(id.as_str())?
                    .map(|value| decode_value::<ArtistBrowseSummary>(value.value()))
                    .transpose()?;
                let Some(mut summary) = summary else {
                    continue;
                };
                if roles.headline {
                    if headlines.len() > 1 {
                        summary.collab_album_count += 1;
                    } else {
                        summary.album_count += 1;
                    }
                } else {
                    summary.appearance_count += usize::from(roles.performing);
                    summary.other_credit_count += usize::from(roles.supporting);
                }
                summary.release_count += usize::from(roles.headline || roles.performing);
                let bytes = encode_value(&summary)?;
                values.insert(id.as_str(), bytes.as_slice())?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{atomic::AtomicBool, Arc};

    fn fixture(count: usize) -> Library {
        let db = Arc::new(
            redb::Database::builder()
                .set_cache_size(1024 * 1024)
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        );
        let write = db.begin_write().unwrap();
        {
            let mut artists = write.open_table(ARTISTS_TABLE).unwrap();
            let mut names = write.open_table(ARTISTS_BY_NAME_TABLE).unwrap();
            let mut albums = write.open_table(ALBUMS_TABLE).unwrap();
            let mut relationships = write.open_table(ARTIST_ALBUMS_TABLE).unwrap();
            for index in 0..count {
                let id = format!("artist-{index:05}");
                let artist = Artist {
                    id: id.clone(),
                    name: format!("Artist {index:05}"),
                    genres: vec!["rock".into()],
                    summary: Some("Biography is excluded from browser records".repeat(8)),
                    logo_ref: None,
                    banner_ref: None,
                };
                let album = common::Album {
                    id: format!("album-{index:05}"),
                    artist_id: id.clone(),
                    artist_ids: vec![],
                    artist_names: vec![],
                    title: "Album".into(),
                    year: None,
                    folder_relpath: String::new(),
                    cover_ref: None,
                    genres: vec![],
                    summary: None,
                };
                artists
                    .insert(id.as_str(), encode_value(&artist).unwrap().as_slice())
                    .unwrap();
                names
                    .insert(
                        crate::storage::artist_name_key(&artist.name, &id).as_str(),
                        &[1u8][..],
                    )
                    .unwrap();
                albums
                    .insert(album.id.as_str(), encode_value(&album).unwrap().as_slice())
                    .unwrap();
                relationships
                    .insert(
                        crate::storage::album_index_key(&id, &album).as_str(),
                        &[1u8][..],
                    )
                    .unwrap();
            }
        }
        write.open_table(TRACKS_TABLE).unwrap();
        write.open_table(TRACK_CREDITS_TABLE).unwrap();
        write.open_table(ALBUM_TRACKS_TABLE).unwrap();
        write.commit().unwrap();
        Library {
            roots: vec![],
            db,
            cache_generation: Arc::new(AtomicU64::new(crate::next_catalog_generation())),
            metadata_generation: Arc::new(AtomicU64::new(crate::next_catalog_generation())),
            browse_projection: Arc::new(Default::default()),
            search_state: Arc::new(Default::default()),
            cancellation: Arc::new(AtomicBool::new(false)),
            tag_reader: None,
            cancel_reads: false,
        }
    }

    #[test]
    fn metadata_updates_refresh_compact_records_without_resetting_order() {
        let library = fixture(3);
        let (first, total) = library.artist_browse_page(1, 0).unwrap();
        assert_eq!(total, 3);
        assert_eq!(first[0].id, "artist-00000");
        assert_eq!(first[0].album_count, 1);
        assert_eq!(first[0].release_count, 1);
        assert!(!serde_json::to_string(&first).unwrap().contains("Biography"));
        let ordering = library.browse_revision();
        let metadata = library.metadata_revision();
        library
            .update_artist_enrichment(
                &first[0].id,
                None,
                &["jazz".into()],
                Some("logos/new.png".into()),
                None,
                false,
            )
            .unwrap();
        let (updated, _) = library.artist_browse_page(1, 0).unwrap();
        assert_eq!(updated[0].logo_ref.as_deref(), Some("logos/new.png"));
        assert!(updated[0].genres.contains(&"jazz".into()));
        assert_eq!(updated[0].album_count, 1);
        assert_eq!(library.browse_revision(), ordering);
        assert_ne!(library.metadata_revision(), metadata);
        library.catalog_changed();
        assert_ne!(library.browse_revision(), ordering);
        assert_eq!(library.artist_browse_page(1, 0).unwrap().0[0], updated[0]);
    }

    #[test]
    fn cancelled_projection_waiter_does_not_wait_for_another_build() {
        let library = fixture(1);
        let projection = library.browse_projection.clone();
        let held = projection.build.lock();
        let flag = library.cancellation_flag();
        let operational = library.with_cancellable_reads();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            done_tx
                .send(matches!(
                    operational.artist_browse_page(10, 0),
                    Err(LibraryError::Cancelled)
                ))
                .unwrap();
        });
        flag.store(true, Ordering::Release);
        let result = done_rx.recv_timeout(std::time::Duration::from_secs(2));
        drop(held);
        worker.join().unwrap();
        assert!(result.unwrap());
    }

    #[test]
    fn five_thousand_artists_return_exact_bounded_pages() {
        let library = fixture(5000);
        for offset in [0, 60, 2400, 4980, 5000, usize::MAX] {
            let (items, total) = library.artist_browse_page(60, offset).unwrap();
            assert_eq!(total, 5000);
            assert_eq!(items.len(), 60.min(5000usize.saturating_sub(offset)));
            assert!(serde_json::to_vec(&items).unwrap().len() < 16 * 1024);
        }
    }

    #[test]
    fn featured_artists_remain_eligible_but_supporting_credits_do_not() {
        let library = fixture(3);
        let write = library.db.begin_write().unwrap();
        {
            let mut albums = write.open_table(ALBUMS_TABLE).unwrap();
            albums.remove("album-00001").unwrap();
            albums.remove("album-00002").unwrap();
            let credits = ArtistCredits {
                display_name: "Featured".into(),
                credits: vec![
                    common::ArtistCredit {
                        artist_id: "artist-00001".into(),
                        role: "featured_artist".into(),
                        ..Default::default()
                    },
                    common::ArtistCredit {
                        artist_id: "artist-00002".into(),
                        role: "producer".into(),
                        ..Default::default()
                    },
                ],
            };
            write
                .open_table(TRACK_CREDITS_TABLE)
                .unwrap()
                .insert("track", encode_value(&credits).unwrap().as_slice())
                .unwrap();
        }
        write.commit().unwrap();
        let (items, total) = library.artist_browse_page(60, 0).unwrap();
        assert_eq!(total, 2);
        assert_eq!(
            items.iter().map(|artist| &artist.id).collect::<Vec<_>>(),
            vec!["artist-00000", "artist-00001"]
        );
        let legacy = library.list_artists_for_browse(None, 60, 0, false).unwrap();
        assert_eq!(total, legacy.1);
    }

    #[test]
    fn streamed_summary_counts_match_full_collaboration_and_contribution_views() {
        let library = fixture(4);
        let mut album = library.get_album("album-00000").unwrap().unwrap();
        album.artist_ids = vec!["artist-00000".into(), "artist-00001".into()];
        let track = common::Track {
            id: "song".into(),
            album_id: album.id.clone(),
            artist_id: "artist-00000".into(),
            title: "Song".into(),
            track_no: Some(1),
            disc_no: Some(1),
            duration_ms: 1000,
            codec: common::Codec::Mp3,
            sample_rate: None,
            channels: None,
            bitrate: None,
            file_relpath: "song.mp3".into(),
            file_size: 0,
            genres: Vec::new(),
        };
        let credits = ArtistCredits {
            display_name: "Artist and Guest".into(),
            credits: vec![
                common::ArtistCredit {
                    artist_id: "artist-00000".into(),
                    role: "track_artist".into(),
                    ..Default::default()
                },
                common::ArtistCredit {
                    artist_id: "artist-00002".into(),
                    role: "featured_artist".into(),
                    ..Default::default()
                },
                common::ArtistCredit {
                    artist_id: "artist-00003".into(),
                    role: "producer".into(),
                    ..Default::default()
                },
            ],
        };
        let write = library.db.begin_write().unwrap();
        {
            let mut albums = write.open_table(ALBUMS_TABLE).unwrap();
            albums
                .insert(album.id.as_str(), encode_value(&album).unwrap().as_slice())
                .unwrap();
            albums.remove("album-00001").unwrap();
            albums.remove("album-00002").unwrap();
            let mut relationships = write.open_table(ARTIST_ALBUMS_TABLE).unwrap();
            for id in [
                "artist-00000",
                "artist-00001",
                "artist-00002",
                "artist-00003",
            ] {
                relationships
                    .insert(
                        crate::storage::album_index_key(id, &album).as_str(),
                        &[1u8][..],
                    )
                    .unwrap();
            }
            write
                .open_table(TRACKS_TABLE)
                .unwrap()
                .insert("song", encode_value(&track).unwrap().as_slice())
                .unwrap();
            write
                .open_table(TRACK_CREDITS_TABLE)
                .unwrap()
                .insert("song", encode_value(&credits).unwrap().as_slice())
                .unwrap();
            write
                .open_table(ALBUM_TRACKS_TABLE)
                .unwrap()
                .insert("album-00000\u{1f}0001\u{1f}song", &[1u8][..])
                .unwrap();
        }
        write.commit().unwrap();
        let (summaries, total) = library.artist_browse_page(60, 0).unwrap();
        assert_eq!(total, 4);
        for summary in summaries {
            let full = library.artist_release_relationships(&summary.id).unwrap();
            assert_eq!(
                summary.album_count,
                full.albums.len(),
                "{} solo albums",
                summary.id
            );
            assert_eq!(
                summary.collab_album_count,
                full.collab_albums.len(),
                "{} collaborations",
                summary.id
            );
            assert_eq!(
                summary.appearance_count,
                full.appears_on.len(),
                "{} appearances",
                summary.id
            );
            assert_eq!(
                summary.other_credit_count,
                full.other_credits.len(),
                "{} supporting credits",
                summary.id
            );
            let releases: HashSet<_> = full
                .albums
                .iter()
                .map(|album| &album.id)
                .chain(full.collab_albums.iter().map(|entry| &entry.album.id))
                .chain(full.appears_on.iter().map(|entry| &entry.album.id))
                .collect();
            assert_eq!(summary.release_count, releases.len());
        }
    }
}
