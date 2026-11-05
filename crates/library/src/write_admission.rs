//! Shared writer admission. Waiting jobs can leave before entering redb.
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use parking_lot::{Condvar, Mutex};
use redb::{CommitError, Database, TransactionError};

use crate::LibraryError;

#[derive(Default)]
struct Gate {
    busy: Mutex<bool>,
    changed: Condvar,
}

fn gate(db: &Database) -> Arc<Gate> {
    static GATES: OnceLock<Mutex<HashMap<usize, Weak<Gate>>>> = OnceLock::new();
    let mut gates = GATES.get_or_init(Default::default).lock();
    gates.retain(|_, gate| gate.strong_count() > 0);
    gates
        .entry(db as *const Database as usize)
        .or_default()
        .upgrade()
        .unwrap_or_else(|| {
            let gate = Arc::new(Gate::default());
            gates.insert(db as *const Database as usize, Arc::downgrade(&gate));
            gate
        })
}

struct Permit(Arc<Gate>);

impl Permit {
    fn acquire(db: &Database, cancellation: Option<&AtomicBool>) -> Result<Self, LibraryError> {
        let gate = gate(db);
        let mut busy = gate.busy.lock();
        loop {
            if cancellation.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                return Err(LibraryError::Cancelled);
            }
            if !*busy {
                *busy = true;
                drop(busy);
                return Ok(Self(gate));
            }
            gate.changed.wait_for(&mut busy, Duration::from_millis(10));
        }
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        *self.0.busy.lock() = false;
        self.0.changed.notify_one();
    }
}

/// The permit outlives commit or rollback; no admitted writer waits inside redb.
pub struct AdmittedWriteTransaction<'db> {
    // Field order makes rollback happen before another writer is admitted.
    inner: Option<redb::WriteTransaction<'db>>,
    _permit: Permit,
}

impl<'db> Deref for AdmittedWriteTransaction<'db> {
    type Target = redb::WriteTransaction<'db>;
    fn deref(&self) -> &Self::Target {
        self.inner.as_ref().expect("live transaction")
    }
}

impl AdmittedWriteTransaction<'_> {
    pub fn commit(mut self) -> Result<(), CommitError> {
        self.inner.take().expect("live transaction").commit()
    }

    pub fn commit_cancellable(self, cancellation: &AtomicBool) -> Result<(), LibraryError> {
        crate::check_cancelled(cancellation)?;
        self.commit()?;
        Ok(())
    }
}

/// Use for short persistent writes, including the server stores sharing this DB.
pub fn begin_write(db: &Database) -> Result<AdmittedWriteTransaction<'_>, TransactionError> {
    let permit = Permit::acquire(db, None).expect("uncancellable admission");
    let inner = db.begin_write()?;
    Ok(AdmittedWriteTransaction {
        inner: Some(inner),
        _permit: permit,
    })
}

/// The shared gate must be used by every writer of this Database instance.
pub fn begin_write_cancellable<'db>(
    db: &'db Database,
    cancellation: &AtomicBool,
) -> Result<AdmittedWriteTransaction<'db>, LibraryError> {
    let permit = Permit::acquire(db, Some(cancellation))?;
    let inner = db.begin_write()?;
    let write = AdmittedWriteTransaction {
        inner: Some(inner),
        _permit: permit,
    };
    crate::check_cancelled(cancellation)?;
    Ok(write)
}

#[cfg(test)]
mod tests {
    use super::*;
    use redb::{ReadableTable, TableDefinition};
    use std::sync::mpsc;
    const VALUES: TableDefinition<&str, u64> = TableDefinition::new("values");

    fn database() -> Arc<Database> {
        Arc::new(
            Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        )
    }

    #[test]
    fn cancelled_waiter_leaves_before_the_writer_finishes() {
        let db = database();
        let held = begin_write(&db).unwrap();
        let flag = Arc::new(AtomicBool::new(false));
        let (done_tx, done_rx) = mpsc::channel();
        let worker_db = db.clone();
        let worker_flag = flag.clone();
        let worker = std::thread::spawn(move || {
            done_tx
                .send(matches!(
                    begin_write_cancellable(&worker_db, &worker_flag),
                    Err(LibraryError::Cancelled)
                ))
                .unwrap();
        });
        flag.store(true, Ordering::Release);
        let result = done_rx.recv_timeout(Duration::from_secs(2));
        drop(held);
        worker.join().unwrap();
        assert!(result.unwrap());
    }

    #[test]
    fn cancellation_before_commit_rolls_back_and_after_commit_keeps_data() {
        let db = database();
        let flag = AtomicBool::new(false);
        let write = begin_write_cancellable(&db, &flag).unwrap();
        write.open_table(VALUES).unwrap().insert("old", 1).unwrap();
        write.commit_cancellable(&flag).unwrap();
        let write = begin_write_cancellable(&db, &flag).unwrap();
        write
            .open_table(VALUES)
            .unwrap()
            .insert("discard", 2)
            .unwrap();
        flag.store(true, Ordering::Release);
        assert!(matches!(
            write.commit_cancellable(&flag),
            Err(LibraryError::Cancelled)
        ));
        let read = db.begin_read().unwrap();
        let table = read.open_table(VALUES).unwrap();
        assert_eq!(table.get("old").unwrap().unwrap().value(), 1);
        assert!(table.get("discard").unwrap().is_none());
    }
}
