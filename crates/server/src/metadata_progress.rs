//! Active metadata work is independent of local indexing and catalog readiness.
use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use parking_lot::RwLock;
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataPhase {
    Artists,
    AlbumArtists,
    Albums,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MetadataTarget {
    pub artist: String,
    pub album: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MetadataProgress {
    pub phase: MetadataPhase,
    /// Items checked, including partial, unmatched, failed, and deferred lookups.
    /// This is independent of the number of fully enriched catalog albums.
    pub checked: usize,
    pub total: Option<usize>,
    pub current: Option<MetadataTarget>,
}

impl MetadataProgress {
    fn new(phase: MetadataPhase) -> Self {
        Self {
            phase,
            checked: 0,
            total: None,
            current: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct MetadataWork {
    next_id: AtomicU64,
    active: RwLock<BTreeMap<u64, MetadataProgress>>,
}

impl MetadataWork {
    pub fn start(self: &Arc<Self>, phase: MetadataPhase) -> MetadataWorkHandle {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.active.write().insert(id, MetadataProgress::new(phase));
        MetadataWorkHandle {
            work: Arc::clone(self),
            id,
        }
    }

    pub fn snapshot(&self) -> Option<MetadataProgress> {
        self.active.read().values().next_back().cloned()
    }
}

pub struct MetadataWorkHandle {
    work: Arc<MetadataWork>,
    id: u64,
}

impl MetadataWorkHandle {
    pub fn set_phase(&self, phase: MetadataPhase) {
        self.work
            .active
            .write()
            .insert(self.id, MetadataProgress::new(phase));
    }

    pub fn set_total(&self, total: usize) {
        if let Some(progress) = self.work.active.write().get_mut(&self.id) {
            progress.total = Some(total);
        }
    }

    /// Expose the target throughout its fetch/store work, then clear it on exit.
    pub fn item(&self, artist: &str, album: Option<&str>) -> MetadataItem<'_> {
        if let Some(progress) = self.work.active.write().get_mut(&self.id) {
            progress.current = Some(MetadataTarget {
                artist: artist.to_owned(),
                album: album.map(str::to_owned),
            });
        }
        MetadataItem(self)
    }
}

pub struct MetadataItem<'a>(&'a MetadataWorkHandle);

impl Drop for MetadataItem<'_> {
    fn drop(&mut self) {
        if let Some(progress) = self.0.work.active.write().get_mut(&self.0.id) {
            progress.checked = progress.checked.saturating_add(1);
            progress.current = None;
        }
    }
}

impl Drop for MetadataWorkHandle {
    fn drop(&mut self) {
        self.work.active.write().remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_stays_active_until_all_work_finishes_and_old_scans_are_isolated() {
        let work = Arc::new(MetadataWork::default());
        let first = work.start(MetadataPhase::Artists);
        let second = work.start(MetadataPhase::Albums);
        assert_eq!(work.snapshot().unwrap().phase, MetadataPhase::Albums);
        drop(second);
        assert_eq!(work.snapshot().unwrap().phase, MetadataPhase::Artists);
        first.set_phase(MetadataPhase::AlbumArtists);
        assert_eq!(work.snapshot().unwrap().phase, MetadataPhase::AlbumArtists);
        let next_scan = Arc::new(MetadataWork::default());
        let next = next_scan.start(MetadataPhase::Albums);
        drop(first);
        assert_eq!(work.snapshot(), None);
        assert_eq!(next_scan.snapshot().unwrap().phase, MetadataPhase::Albums);
        drop(next);
        assert_eq!(next_scan.snapshot(), None);
    }

    #[test]
    fn checked_counts_follow_finished_items_and_reset_between_phases() {
        let work = Arc::new(MetadataWork::default());
        let job = work.start(MetadataPhase::Artists);
        job.set_total(3);
        let item = job.item("Artist", None);
        assert_eq!(
            work.snapshot().unwrap().checked,
            0,
            "a fetch is still running"
        );
        drop(item);
        for _ in 0..2 {
            let _item = job.item("Artist", None);
            continue; // Deferred and failed checks still advance work progress.
        }
        assert_eq!(
            work.snapshot(),
            Some(MetadataProgress {
                phase: MetadataPhase::Artists,
                checked: 3,
                total: Some(3),
                current: None,
            })
        );
        let other = work.start(MetadataPhase::Albums);
        other.set_total(1);
        drop(other.item("Artist", Some("Album")));
        assert_eq!(work.snapshot().unwrap().checked, 1);
        drop(other);
        assert_eq!(
            work.snapshot().unwrap().checked,
            3,
            "overlapping work keeps its own counts"
        );
        job.set_phase(MetadataPhase::AlbumArtists);
        assert_eq!(
            work.snapshot(),
            Some(MetadataProgress::new(MetadataPhase::AlbumArtists))
        );
    }

    #[test]
    fn current_targets_follow_active_items_and_restore_overlapping_work() {
        let work = Arc::new(MetadataWork::default());
        let artist_job = work.start(MetadataPhase::Artists);
        let artist = artist_job.item("Björk", None);
        let artist_target = Some(MetadataTarget {
            artist: "Björk".into(),
            album: None,
        });
        assert_eq!(work.snapshot().unwrap().current, artist_target);
        let album_job = work.start(MetadataPhase::AlbumArtists);
        let album = album_job.item("Björk", Some("Homogenic"));
        assert_eq!(
            work.snapshot().unwrap().current,
            Some(MetadataTarget {
                artist: "Björk".into(),
                album: Some("Homogenic".into()),
            })
        );
        drop(album);
        assert_eq!(work.snapshot().unwrap().current, None);
        drop(album_job);
        assert_eq!(work.snapshot().unwrap().current, artist_target);
        drop(artist);
        assert_eq!(work.snapshot().unwrap().current, None);
        artist_job.set_phase(MetadataPhase::Albums);
        assert_eq!(work.snapshot().unwrap().current, None);
        let _next = artist_job.item("Another artist", Some("Another album"));
        assert_eq!(
            work.snapshot().unwrap().current.unwrap().album.as_deref(),
            Some("Another album")
        );
    }
}
