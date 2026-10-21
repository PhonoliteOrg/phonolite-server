//! Bounded, disk-backed jobs retained across requests and console navigation.
use super::{archive::Scratch, parts};
use library::backup::RestoreReport;
use parking_lot::Mutex;
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{Duration, Instant},
};

const RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_JOBS: usize = 8;

#[derive(Clone, Serialize)]
pub(crate) struct DownloadPart {
    pub index: usize,
    pub name: String,
    pub bytes: u64,
    pub url: String,
}

#[derive(Clone, Serialize)]
pub(crate) struct Snapshot {
    pub id: String,
    pub set_id: Option<String>,
    pub kind: &'static str,
    pub phase: &'static str,
    pub message: String,
    pub received_parts: usize,
    pub total_parts: usize,
    pub started_downloads: usize,
    pub bytes: u64,
    pub parts: Vec<DownloadPart>,
    pub report: Option<RestoreReport>,
}

/// The persistent sidebar polls this small view, without download links/reports.
#[derive(Clone, Serialize)]
pub(crate) struct Progress {
    pub id: String,
    pub kind: &'static str,
    pub phase: &'static str,
    pub message: String,
    pub received_parts: usize,
    pub total_parts: usize,
    pub bytes: u64,
}

impl Snapshot {
    fn progress(&self) -> Progress {
        Progress {
            id: self.id.clone(),
            kind: self.kind,
            phase: self.phase,
            message: self.message.clone(),
            received_parts: self.received_parts,
            total_parts: self.total_parts,
            bytes: self.bytes,
        }
    }
    pub(super) fn new(kind: &'static str, phase: &'static str, message: &str) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            set_id: None,
            kind,
            phase,
            message: message.into(),
            received_parts: 0,
            total_parts: 0,
            started_downloads: 0,
            bytes: 0,
            parts: vec![],
            report: None,
        }
    }
    fn active(&self) -> bool {
        matches!(
            self.phase,
            "preparing" | "uploading" | "checking" | "restoring"
        )
    }
}

pub(super) struct Job {
    pub snapshot: Snapshot,
    pub scratch: Arc<Scratch>,
    pub received: BTreeMap<u32, parts::Header>,
    pub filenames: Vec<String>,
    pub started: BTreeSet<usize>,
    updated: Instant,
}

impl Job {
    pub fn new(snapshot: Snapshot, scratch: Arc<Scratch>) -> Self {
        Self {
            snapshot,
            scratch,
            received: BTreeMap::new(),
            filenames: vec![],
            started: BTreeSet::new(),
            updated: Instant::now(),
        }
    }
}

#[derive(Default)]
struct Registry {
    jobs: BTreeMap<String, Job>,
    transfer: Option<Snapshot>,
}

#[derive(Clone, Default)]
pub(crate) struct Store(Arc<Mutex<Registry>>);

impl Store {
    fn prune(registry: &mut Registry) {
        registry.jobs.retain(|_, job| {
            job.snapshot.active()
                || Arc::strong_count(&job.scratch) > 1
                || job.updated.elapsed() < RETENTION
        });
    }
    pub(super) fn insert(&self, job: Job) -> Result<Snapshot, String> {
        let mut registry = self.0.lock();
        Self::prune(&mut registry);
        if registry.jobs.len() >= MAX_JOBS {
            return Err(
                "Eight backups are already staged. Remove an old backup before uploading another."
                    .into(),
            );
        }
        let snapshot = job.snapshot.clone();
        registry.jobs.insert(snapshot.id.clone(), job);
        Ok(snapshot)
    }
    pub(super) fn with<T>(&self, id: &str, action: impl FnOnce(&mut Job) -> T) -> Option<T> {
        let mut registry = self.0.lock();
        Self::prune(&mut registry);
        let job = registry.jobs.get_mut(id)?;
        job.updated = Instant::now();
        Some(action(job))
    }
    pub(super) fn get(&self, id: &str) -> Option<Snapshot> {
        self.with(id, |job| job.snapshot.clone())
    }
    pub(super) fn find_set(&self, header: &parts::Header) -> Option<String> {
        let mut registry = self.0.lock();
        Self::prune(&mut registry);
        registry
            .jobs
            .iter()
            .find(|(_, job)| {
                job.received
                    .values()
                    .next()
                    .is_some_and(|part| part.set == header.set)
            })
            .map(|(id, _)| id.clone())
    }
    pub(super) fn list(&self) -> Vec<Snapshot> {
        let mut registry = self.0.lock();
        Self::prune(&mut registry);
        registry
            .jobs
            .values()
            .map(|job| job.snapshot.clone())
            .collect()
    }
    pub(super) fn phase(&self, id: &str, phase: &'static str, message: impl Into<String>) {
        self.with(id, |job| {
            job.snapshot.phase = phase;
            job.snapshot.message = message.into();
        });
    }
    pub(super) fn remove(&self, id: &str) -> Result<(), String> {
        let mut registry = self.0.lock();
        if registry
            .jobs
            .get(id)
            .is_some_and(|job| job.snapshot.active())
        {
            return Err("Wait for this backup operation to finish.".into());
        }
        registry.jobs.remove(id);
        Ok(())
    }
    pub(super) fn transfer(&self) -> Transfer {
        self.0.lock().transfer = Some(Snapshot::new(
            "restore",
            "uploading",
            "Uploading backup to disk…",
        ));
        Transfer(self.clone())
    }
    pub(crate) fn latest(&self) -> Option<Progress> {
        let mut registry = self.0.lock();
        Self::prune(&mut registry);
        if let Some(transfer) = &registry.transfer {
            return Some(transfer.progress());
        }
        registry
            .jobs
            .values()
            .filter(|job| {
                job.snapshot.active()
                    || job.snapshot.phase == "waiting"
                    || job.updated.elapsed() < Duration::from_secs(90)
            })
            .max_by_key(|job| (job.snapshot.active(), job.updated))
            .map(|job| job.snapshot.progress())
    }
}

pub(super) struct Transfer(Store);
impl Transfer {
    pub fn checking(&self) {
        if let Some(snapshot) = &mut self.0 .0.lock().transfer {
            snapshot.phase = "checking";
            snapshot.message = "Validating backup file from disk…".into();
        }
    }
    pub fn progress(&self, bytes: u64) {
        if let Some(snapshot) = &mut self.0 .0.lock().transfer {
            snapshot.bytes = bytes;
            snapshot.message = format!("Uploading backup: {} MiB received", bytes / (1024 * 1024));
        }
    }
}
impl Drop for Transfer {
    fn drop(&mut self) {
        self.0 .0.lock().transfer = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(phase: &'static str) -> Job {
        Job::new(
            Snapshot::new("restore", phase, phase),
            Arc::new(Scratch::new().unwrap()),
        )
    }

    #[test]
    fn jobs_are_bounded_expire_and_keep_active_operations_safe() {
        let store = Store::default();
        let active = store.insert(job("restoring")).unwrap().id;
        assert!(store.remove(&active).is_err());
        for _ in 1..MAX_JOBS {
            store.insert(job("waiting")).unwrap();
        }
        assert!(store.insert(job("waiting")).is_err());
        for job in store.0.lock().jobs.values_mut() {
            job.updated -= RETENTION + Duration::from_secs(1);
        }
        assert_eq!(store.list().len(), 1);
        assert_eq!(store.latest().unwrap().id, active);
        store.phase(&active, "complete", "Backup restored");
        store.remove(&active).unwrap();
        assert!(store.list().is_empty());
    }

    #[test]
    fn transfer_progress_is_visible_and_cleared_on_interruption() {
        let store = Store::default();
        store.insert(job("waiting")).unwrap();
        let transfer = store.transfer();
        transfer.progress(3 * 1024 * 1024);
        assert_eq!(store.latest().unwrap().phase, "uploading");
        assert_eq!(store.latest().unwrap().bytes, 3 * 1024 * 1024);
        drop(transfer);
        assert_eq!(store.latest().unwrap().phase, "waiting");
    }

    #[test]
    fn an_active_file_reader_keeps_the_rest_of_its_backup_available() {
        let store = Store::default();
        let id = store.insert(job("ready")).unwrap().id;
        let reader = store.with(&id, |job| job.scratch.clone()).unwrap();
        store.0.lock().jobs.get_mut(&id).unwrap().updated -= RETENTION + Duration::from_secs(1);
        assert_eq!(store.list().len(), 1);
        drop(reader);
        assert!(store.list().is_empty());
    }
}
