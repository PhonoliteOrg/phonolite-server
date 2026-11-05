//! Optional JSON retry receipts, independent of positional catalog records.
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use redb::{ReadableTable, TableDefinition, TableError};
use serde::{Deserialize, Serialize};

use crate::{Library, LibraryError};

pub(super) const METADATA_RETRIES: TableDefinition<&str, &[u8]> =
    TableDefinition::new("metadata_retries_v1");

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Receipt {
    fingerprint: String,
    outcome: String,
    next_attempt: u64,
    owner: Option<u64>,
    #[serde(default)]
    refresh: Option<u64>,
    #[serde(default)]
    last_attempt: u64,
}

impl Receipt {
    fn due(&self, fingerprint: &str, refresh: Option<u64>, interval: Option<Duration>) -> bool {
        if self.fingerprint != fingerprint {
            return true;
        }
        if self.outcome == "cancelled" {
            return true;
        }
        if self.outcome == "in_progress" && crate::now_secs() < self.next_attempt {
            return false;
        }
        match refresh {
            Some(refresh) => self.refresh != Some(refresh) || self.outcome == "cancelled",
            None => {
                crate::now_secs()
                    >= interval
                        .filter(|_| self.last_attempt > 0)
                        .map_or(self.next_attempt, |interval| {
                            self.last_attempt.saturating_add(interval.as_secs())
                        })
            }
        }
    }
}

pub struct MetadataAttempt {
    library: Library,
    key: String,
    receipt: Receipt,
    interval: Duration,
    finished: bool,
}

struct CancelledAttempt {
    db: Weak<redb::Database>,
    key: String,
    receipt: Receipt,
}

fn cancelled_attempts() -> &'static parking_lot::Mutex<Vec<CancelledAttempt>> {
    static CANCELLED: OnceLock<parking_lot::Mutex<Vec<CancelledAttempt>>> = OnceLock::new();
    CANCELLED.get_or_init(Default::default)
}

fn cancelled_in_memory(db: &redb::Database, key: &str, receipt: &Receipt) -> bool {
    cancelled_attempts().lock().iter().any(|attempt| {
        std::ptr::eq(attempt.db.as_ptr(), db)
            && attempt.key == key
            && attempt.receipt.owner == receipt.owner
            && attempt.receipt.fingerprint == receipt.fingerprint
    })
}

/// Persist optional request cancellation while the server remains running.
/// Call from supervised blocking work; shutdown never waits for this cleanup.
pub fn flush_cancelled_metadata_attempts(
    db: &redb::Database,
    cancellation: &AtomicBool,
) -> Result<usize, LibraryError> {
    crate::check_cancelled(cancellation)?;
    let pending = {
        let mut queue = cancelled_attempts().lock();
        queue.retain(|attempt| attempt.db.strong_count() > 0);
        let mut pending = Vec::new();
        let mut index = 0;
        while index < queue.len() {
            if std::ptr::eq(queue[index].db.as_ptr(), db) {
                pending.push(queue.swap_remove(index));
            } else {
                index += 1;
            }
        }
        pending
    };
    if pending.is_empty() {
        return Ok(0);
    }
    let result = (|| {
        let write = crate::begin_write_cancellable(db, cancellation)?;
        let mut changed = 0;
        {
            let mut table = write.open_table(METADATA_RETRIES)?;
            for attempt in &pending {
                crate::check_cancelled(cancellation)?;
                let old = table
                    .get(attempt.key.as_str())?
                    .and_then(|value| serde_json::from_slice::<Receipt>(value.value()).ok());
                if old.is_some_and(|old| {
                    old.owner == attempt.receipt.owner
                        && old.fingerprint == attempt.receipt.fingerprint
                }) {
                    let receipt = Receipt {
                        outcome: "cancelled".into(),
                        next_attempt: 0,
                        owner: None,
                        ..attempt.receipt.clone()
                    };
                    let bytes = serde_json::to_vec(&receipt)
                        .map_err(|err| LibraryError::Io(std::io::Error::other(err.to_string())))?;
                    table.insert(attempt.key.as_str(), bytes.as_slice())?;
                    changed += 1;
                }
            }
        }
        write.commit_cancellable(cancellation)?;
        Ok(changed)
    })();
    if result.is_err() {
        cancelled_attempts().lock().extend(pending);
    }
    result
}

/// Run exactly once after opening the server database and before starting jobs.
/// Reopening a Library during a running server must not reset active requests.
pub fn recover_interrupted_metadata_attempts(db: &redb::Database) -> Result<usize, LibraryError> {
    {
        let read = db.begin_read()?;
        match read.open_table(METADATA_RETRIES) {
            Ok(_) => {}
            Err(TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(err) => return Err(err.into()),
        };
    }
    let write = crate::begin_write(db)?;
    let mut changed = 0;
    {
        let mut table = write.open_table(METADATA_RETRIES)?;
        let mut pending = Vec::new();
        for entry in table.iter()? {
            let (key, value) = entry?;
            if let Ok(receipt) = serde_json::from_slice::<Receipt>(value.value()) {
                if receipt.outcome == "in_progress" {
                    pending.push((key.value().to_owned(), receipt));
                }
            }
        }
        for (key, mut receipt) in pending {
            receipt.outcome = "cancelled".into();
            receipt.owner = None;
            receipt.next_attempt = 0;
            let bytes = serde_json::to_vec(&receipt)
                .map_err(|err| LibraryError::Io(std::io::Error::other(err.to_string())))?;
            table.insert(key.as_str(), bytes.as_slice())?;
            changed += 1;
        }
    }
    write.commit()?;
    Ok(changed)
}

impl Library {
    pub fn metadata_request_outcome(
        &self,
        key: &str,
        fingerprint: &str,
    ) -> Result<Option<String>, LibraryError> {
        let read = self.db.begin_read()?;
        let table = match read.open_table(METADATA_RETRIES) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let receipt = table
            .get(key)?
            .and_then(|value| serde_json::from_slice::<Receipt>(value.value()).ok());
        Ok(receipt
            .filter(|receipt| receipt.fingerprint == fingerprint)
            .map(|receipt| receipt.outcome))
    }

    pub fn metadata_request_due(&self, key: &str, fingerprint: &str) -> Result<bool, LibraryError> {
        self.metadata_refresh_due(key, fingerprint, None)
    }

    pub fn metadata_refresh_due(
        &self,
        key: &str,
        fingerprint: &str,
        refresh: Option<u64>,
    ) -> Result<bool, LibraryError> {
        self.metadata_due(key, fingerprint, refresh, None)
    }

    pub fn metadata_refresh_due_with_interval(
        &self,
        key: &str,
        fingerprint: &str,
        refresh: Option<u64>,
        interval: Duration,
    ) -> Result<bool, LibraryError> {
        self.metadata_due(key, fingerprint, refresh, Some(interval))
    }

    fn metadata_due(
        &self,
        key: &str,
        fingerprint: &str,
        refresh: Option<u64>,
        interval: Option<Duration>,
    ) -> Result<bool, LibraryError> {
        let read = self.db.begin_read()?;
        let table = match read.open_table(METADATA_RETRIES) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(true),
            Err(err) => return Err(err.into()),
        };
        let receipt = table
            .get(key)?
            .and_then(|value| serde_json::from_slice::<Receipt>(value.value()).ok());
        Ok(receipt.is_none_or(|receipt| {
            cancelled_in_memory(&self.db, key, &receipt)
                || receipt.due(fingerprint, refresh, interval)
        }))
    }

    /// Check and reserve in the same write transaction. The guard releases an
    /// unfinished reservation when a source/configuration change cancels work.
    pub fn claim_metadata_request(
        &self,
        key: &str,
        fingerprint: &str,
        interval: Duration,
        force: bool,
    ) -> Result<Option<MetadataAttempt>, LibraryError> {
        self.claim_metadata_refresh(key, fingerprint, interval, force.then_some(0))
    }

    pub fn claim_metadata_refresh(
        &self,
        key: &str,
        fingerprint: &str,
        interval: Duration,
        refresh: Option<u64>,
    ) -> Result<Option<MetadataAttempt>, LibraryError> {
        self.check_cancelled()?;
        let write = crate::begin_write_cancellable(&self.db, &self.cancellation)?;
        let receipt = {
            let mut table = write.open_table(METADATA_RETRIES)?;
            let old = table
                .get(key)?
                .and_then(|value| serde_json::from_slice::<Receipt>(value.value()).ok());
            if old.is_some_and(|old| {
                !cancelled_in_memory(&self.db, key, &old)
                    && !old.due(fingerprint, refresh, Some(interval))
            }) {
                return Ok(None);
            }
            static NEXT_OWNER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            let receipt = Receipt {
                fingerprint: fingerprint.into(),
                outcome: "in_progress".into(),
                next_attempt: crate::now_secs().saturating_add(interval.as_secs()),
                owner: Some(NEXT_OWNER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)),
                refresh,
                last_attempt: crate::now_secs(),
            };
            let bytes = serde_json::to_vec(&receipt)
                .map_err(|err| LibraryError::Io(std::io::Error::other(err.to_string())))?;
            table.insert(key, bytes.as_slice())?;
            receipt
        };
        write.commit_cancellable(&self.cancellation)?;
        Ok(Some(MetadataAttempt {
            library: self.clone(),
            key: key.into(),
            receipt,
            interval,
            finished: false,
        }))
    }
}

pub(super) fn prune_retries(
    write: &redb::WriteTransaction,
    staged: &redb::ReadTransaction,
) -> Result<(), LibraryError> {
    let mut table = write.open_table(METADATA_RETRIES)?;
    let artists = staged.open_table(crate::storage::ARTISTS_TABLE)?;
    let albums = staged.open_table(crate::storage::ALBUMS_TABLE)?;
    let mut removed = Vec::new();
    for entry in table.iter()? {
        let (key, _) = entry?;
        let Ok((kind, id, _)) = serde_json::from_str::<(String, String, String)>(key.value())
        else {
            continue;
        };
        let exists = if kind == "artist" {
            artists.get(id.as_str())?.is_some()
        } else {
            albums.get(id.as_str())?.is_some()
        };
        if !exists {
            removed.push(key.value().to_owned());
        }
    }
    for key in removed {
        table.remove(key.as_str())?;
    }
    Ok(())
}

impl MetadataAttempt {
    pub fn finish(mut self, outcome: &str) -> Result<(), LibraryError> {
        self.update(
            outcome,
            crate::now_secs().saturating_add(self.interval.as_secs()),
        )?;
        self.finished = true;
        Ok(())
    }

    fn update(&self, outcome: &str, next_attempt: u64) -> Result<(), LibraryError> {
        let write = crate::begin_write_cancellable(&self.library.db, &self.library.cancellation)?;
        {
            let mut table = write.open_table(METADATA_RETRIES)?;
            let old = table
                .get(self.key.as_str())?
                .and_then(|value| serde_json::from_slice::<Receipt>(value.value()).ok());
            // A stale/cancelled guard must not overwrite a newer reservation.
            if !old.is_some_and(|old| {
                old.owner == self.receipt.owner && old.fingerprint == self.receipt.fingerprint
            }) {
                return Ok(());
            }
            let receipt = Receipt {
                outcome: outcome.into(),
                next_attempt,
                owner: None,
                last_attempt: crate::now_secs(),
                ..self.receipt.clone()
            };
            let bytes = serde_json::to_vec(&receipt)
                .map_err(|err| LibraryError::Io(std::io::Error::other(err.to_string())))?;
            table.insert(self.key.as_str(), bytes.as_slice())?;
        }
        write.commit_cancellable(&self.library.cancellation)?;
        Ok(())
    }
}

impl Drop for MetadataAttempt {
    fn drop(&mut self) {
        if !self.finished {
            // Destructors run while cancelled futures are being dropped. They
            // must never acquire a database writer or wait for filesystem I/O.
            let mut queue = cancelled_attempts().lock();
            queue.retain(|attempt| attempt.db.strong_count() > 0);
            queue.push(CancelledAttempt {
                db: Arc::downgrade(&self.library.db),
                key: self.key.clone(),
                receipt: self.receipt.clone(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn library() -> Library {
        let db = std::sync::Arc::new(
            redb::Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        );
        Library::load_or_scan_with_db(Vec::new(), db).unwrap().0
    }

    #[test]
    fn dropping_cancelled_work_never_waits_for_a_database_writer() {
        let library = library();
        let attempt = library
            .claim_metadata_request("artist:drop", "source", Duration::from_secs(3600), false)
            .unwrap()
            .unwrap();
        let held = crate::begin_write(&library.db).unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            drop(attempt);
            done_tx.send(()).unwrap();
        });
        let result = done_rx.recv_timeout(Duration::from_secs(2));
        drop(held);
        worker.join().unwrap();
        result.unwrap();
        assert!(library
            .metadata_request_due("artist:drop", "source")
            .unwrap());
        assert_eq!(
            flush_cancelled_metadata_attempts(&library.db, &library.cancellation).unwrap(),
            1
        );
    }

    #[test]
    fn delayed_cancellation_receipt_cannot_overwrite_a_new_owner() {
        let library = library();
        let old = library
            .claim_metadata_request("artist:owner", "source", Duration::from_secs(3600), false)
            .unwrap()
            .unwrap();
        drop(old);
        let replacement = library
            .claim_metadata_request("artist:owner", "source", Duration::from_secs(3600), false)
            .unwrap()
            .unwrap();
        assert_eq!(
            flush_cancelled_metadata_attempts(&library.db, &library.cancellation).unwrap(),
            0
        );
        assert!(!library
            .metadata_request_due("artist:owner", "source")
            .unwrap());
        replacement.finish("matched").unwrap();
        assert_eq!(
            library
                .metadata_request_outcome("artist:owner", "source")
                .unwrap()
                .as_deref(),
            Some("matched")
        );
    }

    #[test]
    fn startup_recovery_releases_legacy_orphans_but_keeps_completed_cooldowns() {
        let library = library();
        library
            .claim_metadata_request(
                "artist:complete",
                "source",
                Duration::from_secs(3600),
                false,
            )
            .unwrap()
            .unwrap()
            .finish("matched")
            .unwrap();
        let write = crate::begin_write(&library.db).unwrap();
        {
            let mut table = write.open_table(METADATA_RETRIES).unwrap();
            // Persisted legacy receipts have no refresh or last_attempt fields.
            let bytes = serde_json::to_vec(&serde_json::json!({"fingerprint":"source","outcome":"in_progress","next_attempt":crate::now_secs()+86400,"owner":17})).unwrap();
            table.insert("artist:orphan", bytes.as_slice()).unwrap();
        }
        write.commit().unwrap();
        assert!(!library
            .metadata_request_due("artist:orphan", "source")
            .unwrap());
        assert_eq!(
            recover_interrupted_metadata_attempts(&library.db).unwrap(),
            1
        );
        assert!(library
            .metadata_request_due("artist:orphan", "source")
            .unwrap());
        assert!(!library
            .metadata_request_due("artist:complete", "source")
            .unwrap());
        assert_eq!(
            recover_interrupted_metadata_attempts(&library.db).unwrap(),
            0
        );
        let empty = redb::Database::builder()
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .unwrap();
        assert_eq!(recover_interrupted_metadata_attempts(&empty).unwrap(), 0);
    }

    #[test]
    fn shutdown_does_not_start_optional_receipt_cleanup() {
        let library = library();
        let attempt = library
            .claim_metadata_request("artist:stop", "source", Duration::from_secs(3600), false)
            .unwrap()
            .unwrap();
        drop(attempt);
        library
            .cancellation
            .store(true, std::sync::atomic::Ordering::Release);
        let held = crate::begin_write(&library.db).unwrap();
        assert!(matches!(
            flush_cancelled_metadata_attempts(&library.db, &library.cancellation),
            Err(LibraryError::Cancelled)
        ));
        drop(held);
        let read = library.db.begin_read().unwrap();
        let table = read.open_table(METADATA_RETRIES).unwrap();
        let receipt: Receipt =
            serde_json::from_slice(table.get("artist:stop").unwrap().unwrap().value()).unwrap();
        assert_eq!(receipt.outcome, "in_progress");
    }

    #[test]
    fn claims_throttle_per_source_and_release_cancelled_work() {
        let library = library();
        let interval = Duration::from_secs(86_400);
        let attempt = library
            .claim_metadata_request("artist:a:mb", "source", interval, false)
            .unwrap()
            .unwrap();
        assert!(library
            .claim_metadata_request("artist:a:mb", "source", interval, false)
            .unwrap()
            .is_none());
        assert!(library
            .metadata_request_due("artist:a:audiodb", "source")
            .unwrap());
        drop(attempt);
        assert!(library
            .metadata_request_due("artist:a:mb", "source")
            .unwrap());
        library
            .claim_metadata_request("artist:a:mb", "source", interval, false)
            .unwrap()
            .unwrap()
            .finish("no_match")
            .unwrap();
        assert!(!library
            .metadata_request_due("artist:a:mb", "source")
            .unwrap());
        assert!(library
            .metadata_request_due("artist:a:mb", "changed")
            .unwrap());
        assert!(library
            .claim_metadata_request("artist:a:mb", "source", interval, true)
            .unwrap()
            .is_some());
    }

    #[test]
    fn unsuccessful_results_become_due_again_and_survive_reopening() {
        let library = library();
        library
            .claim_metadata_request("album:a:mb", "source", Duration::ZERO, false)
            .unwrap()
            .unwrap()
            .finish("ambiguous")
            .unwrap();
        assert!(library
            .metadata_request_due("album:a:mb", "source")
            .unwrap());
        library
            .claim_metadata_request("album:a:mb", "source", Duration::from_secs(60), true)
            .unwrap()
            .unwrap()
            .finish("request_failed")
            .unwrap();
        let reopened = Library::load_or_scan_with_db(Vec::new(), library.db.clone())
            .unwrap()
            .0;
        assert!(!reopened
            .metadata_request_due("album:a:mb", "source")
            .unwrap());
    }

    #[test]
    fn interrupted_refresh_does_not_refetch_completed_requests_in_the_same_run() {
        let library = library();
        let interval = Duration::from_secs(3600);
        library
            .claim_metadata_refresh("artist:a:mb", "source", interval, Some(1))
            .unwrap()
            .unwrap()
            .finish("matched")
            .unwrap();
        assert!(library
            .claim_metadata_refresh("artist:a:mb", "source", interval, Some(1))
            .unwrap()
            .is_none());
        let fresh = library
            .claim_metadata_refresh("artist:a:mb", "source", interval, Some(2))
            .unwrap()
            .unwrap();
        assert!(library
            .claim_metadata_refresh("artist:a:mb", "source", interval, Some(3))
            .unwrap()
            .is_none());
        drop(fresh);
        assert!(library
            .claim_metadata_refresh("artist:a:mb", "source", interval, Some(2))
            .unwrap()
            .is_some());
    }

    #[test]
    fn simultaneous_requests_reserve_a_provider_only_once() {
        let library = library();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(10));
        let threads: Vec<_> = (0..10)
            .map(|_| {
                let library = library.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    library
                        .claim_metadata_request(
                            "artist:a:mb",
                            "source",
                            Duration::from_secs(60),
                            false,
                        )
                        .unwrap()
                })
            })
            .collect();
        let attempts: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(
            attempts.iter().filter(|attempt| attempt.is_some()).count(),
            1
        );
        drop(attempts);
        assert!(library
            .metadata_request_due("artist:a:mb", "source")
            .unwrap());
    }
    #[test]
    fn changed_retry_interval_applies_to_persisted_attempts() {
        let library = library();
        library
            .claim_metadata_request("artist:a:mb", "source", Duration::from_secs(3600), false)
            .unwrap()
            .unwrap()
            .finish("no_usable_metadata")
            .unwrap();
        assert!(!library
            .metadata_request_due("artist:a:mb", "source")
            .unwrap());
        assert!(library
            .metadata_refresh_due_with_interval("artist:a:mb", "source", None, Duration::ZERO)
            .unwrap());
        assert!(library
            .claim_metadata_request("artist:a:mb", "source", Duration::ZERO, false)
            .unwrap()
            .is_some());
    }
}
