//! Cancellation exercises real scanner boundaries without running an application.
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use redb::{Database, TableError};

use crate::{Library, LibraryError, LibraryRoot, ScanProgress};

fn database() -> Arc<Database> {
    Arc::new(
        Database::builder()
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .unwrap(),
    )
}

struct Fixture {
    path: PathBuf,
    library: Library,
    flag: Arc<AtomicBool>,
}

impl Fixture {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "phonolite-scan-cancel-{}-{stamp}",
            std::process::id()
        ));
        let album = path.join("Artist/Album");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("Original.mp3"), b"tag scan fixture").unwrap();
        let flag = Arc::new(AtomicBool::new(false));
        let library = Library::load_or_scan_with_db_and_progress_cancellable(
            vec![LibraryRoot::new(String::new(), path.clone())],
            database(),
            &|_| {},
            flag.clone(),
        )
        .unwrap()
        .0;
        Self {
            path,
            library,
            flag,
        }
    }

    fn add(&self, name: &str) {
        std::fs::write(
            self.path.join("Artist/Album").join(name),
            b"new tag scan fixture",
        )
        .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn pre_cancelled_startup_never_creates_live_catalog_tables() {
    let db = database();
    let flag = Arc::new(AtomicBool::new(true));
    let result = Library::load_or_scan_with_db_and_progress_cancellable(
        Vec::new(),
        db.clone(),
        &|_| {},
        flag,
    );
    assert!(matches!(result, Err(LibraryError::Cancelled)));
    assert!(matches!(
        db.begin_read()
            .unwrap()
            .open_table(crate::storage::TRACKS_TABLE),
        Err(TableError::TableDoesNotExist(_))
    ));
}

#[test]
fn operational_reads_cancel_while_committed_data_remains_inspectable() {
    let fixture = Fixture::new();
    let operational = fixture.library.clone().with_cancellable_reads();
    fixture.flag.store(true, Ordering::Release);
    assert!(matches!(
        operational.track_ids(),
        Err(LibraryError::Cancelled)
    ));
    assert!(matches!(operational.stats(), Err(LibraryError::Cancelled)));
    assert!(matches!(
        operational.artist_browse_page(10, 0),
        Err(LibraryError::Cancelled)
    ));
    assert_eq!(fixture.library.track_ids().unwrap().len(), 1);
    assert_eq!(fixture.library.stats().unwrap().tracks, 1);
}

#[test]
fn tag_workers_stop_between_files_and_leave_the_previous_catalog_intact() {
    let fixture = Fixture::new();
    let original = fixture.library.track_ids().unwrap();
    for index in 0..64 {
        fixture.add(&format!("New-{index}.mp3"));
    }
    let result = fixture.library.scan_with_progress(false, None, &|progress| {
        if matches!(progress, ScanProgress::ReadingTracks { completed, total } if completed > 1 && total > 1) {
            fixture.flag.store(true, Ordering::Release);
        }
    });
    assert!(matches!(result, Err(LibraryError::Cancelled)));
    assert_eq!(fixture.library.track_ids().unwrap(), original);
    assert_eq!(fixture.library.stats().unwrap().tracks, 1);
    assert!(fixture.library.clone().is_cancelled());
}

#[test]
fn injected_tag_workers_discard_results_when_shutdown_arrives() {
    let fixture = Fixture::new();
    let original = fixture.library.track_ids().unwrap();
    fixture.add("Blocked.mp3");
    let (entered_tx, entered_rx) = mpsc::channel();
    let reader: Arc<crate::TagReader> = Arc::new(move |_, cancellation| {
        let _ = entered_tx.send(());
        while !cancellation.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(5));
        }
        Err("cancelled parser".into())
    });
    let flag = fixture.flag.clone();
    let roots = fixture.library.roots.clone();
    let db = fixture.library.db.clone();
    let worker_flag = flag.clone();
    let worker = std::thread::spawn(move || {
        let library = Library::load_or_scan_with_db_and_progress_cancellable_with_reader(
            roots,
            db,
            &|_| {},
            worker_flag,
            reader,
        )?
        .0;
        library.scan_with_progress(false, None, &|_| {})
    });
    // A normal startup can reuse the existing catalogue. Force a changed-file
    // scan through the injected reader after loading it.
    let entered = entered_rx.recv_timeout(Duration::from_secs(2));
    flag.store(true, Ordering::Release);
    let result = worker.join().unwrap();
    entered.unwrap();
    assert!(matches!(result, Err(LibraryError::Cancelled)));
    assert_eq!(fixture.library.track_ids().unwrap(), original);
}

#[test]
fn cancellation_after_staging_aborts_the_atomic_live_catalog_commit() {
    let fixture = Fixture::new();
    let original = fixture.library.track_ids().unwrap();
    let generation = fixture.library.catalog_generation();
    fixture.add("Unpublished.mp3");
    let result = fixture
        .library
        .scan_with_progress(false, None, &|progress| {
            if matches!(progress, ScanProgress::Committing) {
                fixture.flag.store(true, Ordering::Release);
            }
        });
    assert!(matches!(result, Err(LibraryError::Cancelled)));
    assert_eq!(fixture.library.track_ids().unwrap(), original);
    assert_eq!(fixture.library.catalog_generation(), generation);
}

#[test]
fn cancellation_during_album_assembly_does_not_publish_partial_records() {
    let fixture = Fixture::new();
    let original = fixture.library.track_ids().unwrap();
    fixture.add("Unpublished.mp3");
    let result = fixture
        .library
        .scan_with_progress(false, None, &|progress| {
            if matches!(progress, ScanProgress::IndexingAlbums { .. }) {
                fixture.flag.store(true, Ordering::Release);
            }
        });
    assert!(matches!(result, Err(LibraryError::Cancelled)));
    assert_eq!(fixture.library.track_ids().unwrap(), original);
}

#[test]
fn a_cancelled_scan_waiter_exits_without_waiting_for_the_global_scan_lock() {
    let fixture = Fixture::new();
    let guard = crate::indexing::SCAN_LOCK.lock();
    let library = fixture.library.clone();
    let (started_tx, started_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = library.scan_with_progress(false, None, &|progress| {
            if matches!(progress, ScanProgress::Discovering) {
                let _ = started_tx.send(());
            }
        });
        result_tx
            .send(matches!(result, Err(LibraryError::Cancelled)))
            .unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    fixture.flag.store(true, Ordering::Release);
    let result_while_locked = result_rx.recv_timeout(Duration::from_secs(2));
    drop(guard);
    worker.join().unwrap();
    assert_eq!(result_while_locked.unwrap(), true);
}

#[test]
fn cancelled_metadata_repairs_and_writes_do_not_start_more_catalog_work() {
    let fixture = Fixture::new();
    fixture.flag.store(true, Ordering::Release);
    assert!(Arc::ptr_eq(
        &fixture.library.cancellation_flag(),
        &fixture.flag
    ));
    assert!(matches!(
        fixture.library.resolve_album_display_credit("missing", &[]),
        Err(LibraryError::Cancelled)
    ));
    assert!(matches!(
        fixture.library.record_external_attempt("test", true),
        Err(LibraryError::Cancelled)
    ));
    assert!(matches!(
        fixture
            .library
            .update_album_enrichment("missing", None, &[], false),
        Err(LibraryError::Cancelled)
    ));
    assert!(matches!(
        fixture
            .library
            .update_artist_enrichment("missing", None, &[], None, None, false),
        Err(LibraryError::Cancelled)
    ));
    assert!(matches!(
        fixture.library.merge_album_artists("missing", &[]),
        Err(LibraryError::Cancelled)
    ));
}
