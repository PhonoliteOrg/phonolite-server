//! Persistent music catalog facade.
//!
//! The public API is stable; private modules own read queries, enrichment
//! transactions, storage encoding, and filesystem indexing independently.
pub mod backup;
mod browse_projection;
#[cfg(test)]
mod cancellation_tests;
mod catalogue_metadata;
mod credits;
mod enrichment;
mod indexing;
mod metadata_retry;
mod normalization;
mod queries;
pub mod search;
mod source_identity;
mod storage;
mod write_admission;

pub use browse_projection::ArtistBrowseSummary;
pub use catalogue_metadata::{AlbumViewMetadata, TrackDisplayMetadata, TrackViewMetadata};
pub use credits::resolve_display_credit;
pub use metadata_retry::MetadataAttempt;
pub use metadata_retry::{
    flush_cancelled_metadata_attempts, recover_interrupted_metadata_attempts,
};
pub use source_identity::{AlbumSourceIdentity, ALBUM_SOURCE_IDENTITY_SCHEMA_VERSION};
pub use write_admission::{begin_write, begin_write_cancellable, AdmittedWriteTransaction};

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::join_relpath;
use metadata::MetadataError;
use redb::{CommitError, Database, DatabaseError, StorageError, TableError, TransactionError};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::storage::{open_or_create_db, read_version, INDEX_VERSION};

const ROOT_SEP: &str = "::";

/// Work completed by the local indexer. Album progress counts physical album
/// folders (with disc folders grouped); the final catalog can merge copies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum ScanProgress {
    #[default]
    Discovering,
    ReadingTracks {
        completed: usize,
        total: usize,
    },
    IndexingAlbums {
        completed: usize,
        total: usize,
    },
    Committing,
}

type ScanObserver<'a> = &'a (dyn Fn(ScanProgress) + Sync);
/// Production servers can isolate parsers; local tools retain the direct reader.
pub type TagReader =
    dyn Fn(&Path, &AtomicBool) -> Result<metadata::TagInfo, String> + Send + Sync + 'static;

/// Source changes published by one atomic scan. Metadata workers can target
/// these identities without walking the catalog again after a file event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanChanges {
    pub artists: std::collections::BTreeSet<String>,
    pub albums: std::collections::BTreeSet<String>,
    pub removed_artists: std::collections::BTreeSet<String>,
    pub removed_albums: std::collections::BTreeSet<String>,
    pub changed: bool,
}

#[derive(Clone, Debug)]
pub struct ScanOutcome {
    pub stats: LibraryStats,
    pub changes: ScanChanges,
}

/// Catalog albums with a persisted description and genres, rather than attempted lookups.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlbumMetadataStats {
    pub completed: usize,
    pub total: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArtistReleaseContribution {
    pub album: common::Album,
    pub track_ids: Vec<String>,
    pub track_titles: Vec<String>,
    pub roles: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ArtistReleaseRelationships {
    pub albums: Vec<common::Album>,
    /// Joint releases and every album containing this artist's contributions.
    #[serde(default)]
    pub collab_albums: Vec<ArtistReleaseContribution>,
    pub appears_on: Vec<ArtistReleaseContribution>,
    pub other_credits: Vec<ArtistReleaseContribution>,
}

#[derive(Clone, Debug)]
pub struct LibraryRoot {
    pub id: String,
    pub path: PathBuf,
}

impl LibraryRoot {
    pub fn new(id: String, path: PathBuf) -> Self {
        Self { id, path }
    }
}

#[derive(Clone)]
pub struct Library {
    roots: Vec<LibraryRoot>,
    db: Arc<Database>,
    cache_generation: Arc<std::sync::atomic::AtomicU64>,
    metadata_generation: Arc<std::sync::atomic::AtomicU64>,
    browse_projection: Arc<browse_projection::ProjectionState>,
    search_state: Arc<search::SearchState>,
    cancellation: Arc<AtomicBool>,
    tag_reader: Option<Arc<TagReader>>,
    cancel_reads: bool,
}

fn next_catalog_generation() -> u64 {
    // Revisions must not collide with an app's retained window after restart.
    static NEXT: std::sync::OnceLock<std::sync::atomic::AtomicU64> = std::sync::OnceLock::new();
    let next = NEXT.get_or_init(|| {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        std::sync::atomic::AtomicU64::new(seed)
    });
    next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl Library {
    /// Operational views stop disposable queries on shutdown. The default view
    /// remains readable so callers can inspect the last committed catalogue.
    pub fn with_cancellable_reads(mut self) -> Self {
        self.cancel_reads = true;
        self
    }

    fn check_read_cancelled(&self) -> Result<(), LibraryError> {
        if self.cancel_reads {
            self.check_cancelled()
        } else {
            Ok(())
        }
    }
    /// Scan cancellation is shared by every clone, including metadata repairs.
    pub fn cancellation_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancellation)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation.load(Ordering::Acquire)
    }

    fn check_cancelled(&self) -> Result<(), LibraryError> {
        check_cancelled(&self.cancellation)
    }

    /// Identifies this loaded catalog for in-memory caches. Clones share an
    /// identity; reopening or rebuilding the catalog creates a fresh identity.
    pub fn catalog_generation(&self) -> u64 {
        self.cache_generation
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Committed metadata freshness, independent of browse membership/order.
    pub fn metadata_revision(&self) -> u64 {
        self.metadata_generation.load(Ordering::Acquire)
    }

    pub fn browse_revision(&self) -> String {
        self.catalog_generation().to_string()
    }

    fn metadata_changed(&self) {
        self.metadata_generation
            .store(next_catalog_generation(), Ordering::Release);
    }

    /// Restores write source tables directly, so discard derived summary rows.
    fn metadata_restored(&self) {
        self.browse_projection.invalidate();
        self.metadata_changed();
    }

    fn catalog_changed(&self) {
        self.cache_generation.store(
            next_catalog_generation(),
            std::sync::atomic::Ordering::Release,
        );
        self.metadata_changed();
    }

    pub fn load_or_scan(
        roots: Vec<LibraryRoot>,
        db_path: PathBuf,
    ) -> Result<(Self, bool), LibraryError> {
        let db = open_or_create_db(&db_path)?;
        let library = Self {
            roots,
            db: Arc::new(db),
            cancellation: Arc::new(AtomicBool::new(false)),
            tag_reader: None,
            cancel_reads: false,
            cache_generation: Arc::new(
                std::sync::atomic::AtomicU64::new(next_catalog_generation()),
            ),
            metadata_generation: Arc::new(std::sync::atomic::AtomicU64::new(
                next_catalog_generation(),
            )),
            browse_projection: Arc::new(Default::default()),
            search_state: Arc::new(Default::default()),
        };

        let mut scanned = false;
        match read_version(&library.db)? {
            Some(version) if version == INDEX_VERSION => {
                if indexing::needs_catalog_recovery(&library.db)? {
                    info!(
                        "Restoring local metadata before verified enrichment in {:?}",
                        db_path
                    );
                    library.incremental_scan()?;
                    scanned = true;
                } else {
                    info!("Loaded index from {:?}", db_path);
                }
            }
            Some(version) => {
                warn!("Index version mismatch ({}); rescanning", version);
                indexing::scan(
                    &library.roots,
                    &library.db,
                    true,
                    None,
                    None,
                    &library.cancellation,
                    library.tag_reader.as_ref(),
                )?;
                scanned = true;
            }
            None => {
                warn!("Index missing; scanning");
                indexing::scan(
                    &library.roots,
                    &library.db,
                    true,
                    None,
                    None,
                    &library.cancellation,
                    library.tag_reader.as_ref(),
                )?;
                scanned = true;
            }
        }

        Ok((library, scanned))
    }

    pub fn load_or_scan_with_db(
        roots: Vec<LibraryRoot>,
        db: Arc<Database>,
    ) -> Result<(Self, bool), LibraryError> {
        Self::load_or_scan_db(roots, db, None, Arc::new(AtomicBool::new(false)))
    }

    pub fn load_or_scan_with_db_and_progress(
        roots: Vec<LibraryRoot>,
        db: Arc<Database>,
        progress: ScanObserver<'_>,
    ) -> Result<(Self, bool), LibraryError> {
        Self::load_or_scan_db(roots, db, Some(progress), Arc::new(AtomicBool::new(false)))
    }

    pub fn load_or_scan_with_db_and_progress_cancellable(
        roots: Vec<LibraryRoot>,
        db: Arc<Database>,
        progress: ScanObserver<'_>,
        cancellation: Arc<AtomicBool>,
    ) -> Result<(Self, bool), LibraryError> {
        Self::load_or_scan_db(roots, db, Some(progress), cancellation)
    }

    pub fn load_or_scan_with_db_and_progress_cancellable_with_reader(
        roots: Vec<LibraryRoot>,
        db: Arc<Database>,
        progress: ScanObserver<'_>,
        cancellation: Arc<AtomicBool>,
        tag_reader: Arc<TagReader>,
    ) -> Result<(Self, bool), LibraryError> {
        Self::load_or_scan_db_with_reader(roots, db, Some(progress), cancellation, Some(tag_reader))
    }

    fn load_or_scan_db(
        roots: Vec<LibraryRoot>,
        db: Arc<Database>,
        progress: Option<ScanObserver<'_>>,
        cancellation: Arc<AtomicBool>,
    ) -> Result<(Self, bool), LibraryError> {
        Self::load_or_scan_db_with_reader(roots, db, progress, cancellation, None)
    }

    fn load_or_scan_db_with_reader(
        roots: Vec<LibraryRoot>,
        db: Arc<Database>,
        progress: Option<ScanObserver<'_>>,
        cancellation: Arc<AtomicBool>,
        tag_reader: Option<Arc<TagReader>>,
    ) -> Result<(Self, bool), LibraryError> {
        let library = Self {
            roots,
            db,
            cancellation,
            tag_reader,
            cancel_reads: false,
            cache_generation: Arc::new(
                std::sync::atomic::AtomicU64::new(next_catalog_generation()),
            ),
            metadata_generation: Arc::new(std::sync::atomic::AtomicU64::new(
                next_catalog_generation(),
            )),
            browse_projection: Arc::new(Default::default()),
            search_state: Arc::new(Default::default()),
        };
        library.check_cancelled()?;
        let mut scanned = false;
        match read_version(&library.db)? {
            Some(version) if version == INDEX_VERSION => {
                if indexing::needs_catalog_recovery(&library.db)? {
                    info!(
                        "Restoring local metadata before verified enrichment in existing database"
                    );
                    indexing::scan(
                        &library.roots,
                        &library.db,
                        false,
                        None,
                        progress,
                        &library.cancellation,
                        library.tag_reader.as_ref(),
                    )?;
                    scanned = true;
                } else {
                    info!("Loaded index from existing database");
                }
            }
            Some(version) => {
                warn!("Index version mismatch ({}); rescanning", version);
                indexing::scan(
                    &library.roots,
                    &library.db,
                    true,
                    None,
                    progress,
                    &library.cancellation,
                    library.tag_reader.as_ref(),
                )?;
                scanned = true;
            }
            None => {
                warn!("Index missing; scanning");
                indexing::scan(
                    &library.roots,
                    &library.db,
                    true,
                    None,
                    progress,
                    &library.cancellation,
                    library.tag_reader.as_ref(),
                )?;
                scanned = true;
            }
        }
        Ok((library, scanned))
    }

    pub fn open_db(path: &Path) -> Result<Arc<Database>, LibraryError> {
        let db = open_or_create_db(path)?;
        Ok(Arc::new(db))
    }

    pub fn open_db_with_cache(
        path: &Path,
        cache_bytes: usize,
    ) -> Result<Arc<Database>, LibraryError> {
        Ok(Arc::new(storage::open_or_create_db_with_cache(
            path,
            cache_bytes,
        )?))
    }

    pub fn rescan(&self) -> Result<LibraryStats, LibraryError> {
        let started = std::time::Instant::now();
        let _scan = indexing::scan_lock(&self.cancellation)?;
        let write = crate::begin_write_cancellable(&self.db, &self.cancellation)?;
        backup::discard_retained(&write)?;
        self.check_cancelled()?;
        write.commit_cancellable(&self.cancellation)?;
        let result = indexing::scan_locked(
            &self.roots,
            &self.db,
            true,
            None,
            started,
            None,
            &self.cancellation,
            self.tag_reader.as_ref(),
        );
        if result.is_ok() {
            self.catalog_changed();
        }
        result
    }

    pub fn incremental_scan(&self) -> Result<LibraryStats, LibraryError> {
        let result = indexing::scan(
            &self.roots,
            &self.db,
            false,
            None,
            None,
            &self.cancellation,
            self.tag_reader.as_ref(),
        );
        if result.is_ok() {
            self.catalog_changed();
        }
        result
    }

    pub fn incremental_scan_paths(&self, paths: &[PathBuf]) -> Result<LibraryStats, LibraryError> {
        let result = indexing::scan(
            &self.roots,
            &self.db,
            false,
            Some(paths),
            None,
            &self.cancellation,
            self.tag_reader.as_ref(),
        );
        if result.is_ok() {
            self.catalog_changed();
        }
        result
    }

    pub fn scan_with_progress(
        &self,
        force: bool,
        changed_paths: Option<&[PathBuf]>,
        progress: ScanObserver<'_>,
    ) -> Result<LibraryStats, LibraryError> {
        let result = indexing::scan(
            &self.roots,
            &self.db,
            force,
            changed_paths,
            Some(progress),
            &self.cancellation,
            self.tag_reader.as_ref(),
        );
        if result.is_ok() {
            self.catalog_changed();
        }
        result
    }

    pub fn scan_with_changes(
        &self,
        force: bool,
        changed_paths: Option<&[PathBuf]>,
        progress: ScanObserver<'_>,
    ) -> Result<ScanOutcome, LibraryError> {
        self.scan_with_changes_and_prepare(force, changed_paths, progress, &|| {})
    }

    /// Invalidate metadata just before publication, leaving unchanged file
    /// notifications free to reuse valid, already-running provider lookups.
    pub fn scan_with_changes_and_prepare(
        &self,
        force: bool,
        changed_paths: Option<&[PathBuf]>,
        progress: ScanObserver<'_>,
        before_commit: &(dyn Fn() + Sync),
    ) -> Result<ScanOutcome, LibraryError> {
        let result = indexing::scan_detailed(
            &self.roots,
            &self.db,
            force,
            changed_paths,
            Some(progress),
            &self.cancellation,
            Some(before_commit),
            self.tag_reader.as_ref(),
        );
        if result.as_ref().is_ok_and(|outcome| outcome.changes.changed) {
            self.catalog_changed();
        }
        result
    }

    pub fn roots(&self) -> &[LibraryRoot] {
        &self.roots
    }

    pub fn root(&self) -> &Path {
        self.default_root()
            .map(|root| root.path.as_path())
            .unwrap_or_else(|| Path::new("."))
    }

    pub fn resolve_relpath(&self, relpath: &str) -> Option<PathBuf> {
        let (prefix, rest) = split_root_relpath(relpath);
        if let Some(prefix) = prefix {
            if let Some(root) = self.root_by_id(prefix) {
                return Some(join_relpath(&root.path, rest));
            }
        }
        let root = self.default_root()?;
        Some(join_relpath(&root.path, relpath))
    }

    pub fn db(&self) -> Arc<Database> {
        Arc::clone(&self.db)
    }

    fn root_by_id(&self, id: &str) -> Option<&LibraryRoot> {
        self.roots.iter().find(|root| root.id == id)
    }

    fn default_root(&self) -> Option<&LibraryRoot> {
        self.roots
            .iter()
            .find(|root| root.id.is_empty())
            .or_else(|| self.roots.first())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LibraryStats {
    pub artists: usize,
    pub albums: usize,
    pub tracks: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TagErrorInfo {
    pub album_id: String,
    pub artist_id: String,
    pub album_title: String,
    pub artist_name: String,
    pub folder_relpath: String,
    pub last_seen: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TagErrorFile {
    pub file_relpath: String,
    pub folder_relpath: String,
    pub error: String,
    pub last_seen: u64,
}

#[derive(Debug)]
pub enum LibraryError {
    Cancelled,
    Io(std::io::Error),
    Metadata(MetadataError),
    Redb(redb::Error),
    Bincode(Box<bincode::ErrorKind>),
    KeyParse(String),
    InvalidSourceIdentity(String),
    VersionMismatch(u32),
}

impl std::fmt::Display for LibraryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LibraryError::Cancelled => write!(f, "library work cancelled"),
            LibraryError::Io(err) => write!(f, "io error: {}", err),
            LibraryError::Metadata(err) => write!(f, "metadata error: {:?}", err),
            LibraryError::Redb(err) => write!(f, "db error: {}", err),
            LibraryError::Bincode(err) => write!(f, "bincode error: {}", err),
            LibraryError::KeyParse(value) => write!(f, "key parse error: {}", value),
            LibraryError::InvalidSourceIdentity(reason) => {
                write!(f, "invalid album source identity: {}", reason)
            }
            LibraryError::VersionMismatch(version) => {
                write!(f, "index version mismatch: {}", version)
            }
        }
    }
}

fn check_cancelled(cancellation: &AtomicBool) -> Result<(), LibraryError> {
    if cancellation.load(Ordering::Acquire) {
        Err(LibraryError::Cancelled)
    } else {
        Ok(())
    }
}

impl std::error::Error for LibraryError {}

impl From<std::io::Error> for LibraryError {
    fn from(err: std::io::Error) -> Self {
        LibraryError::Io(err)
    }
}

impl From<MetadataError> for LibraryError {
    fn from(err: MetadataError) -> Self {
        LibraryError::Metadata(err)
    }
}

impl From<redb::Error> for LibraryError {
    fn from(err: redb::Error) -> Self {
        LibraryError::Redb(err)
    }
}

impl From<DatabaseError> for LibraryError {
    fn from(err: DatabaseError) -> Self {
        LibraryError::Redb(err.into())
    }
}

impl From<TableError> for LibraryError {
    fn from(err: TableError) -> Self {
        LibraryError::Redb(err.into())
    }
}

impl From<TransactionError> for LibraryError {
    fn from(err: TransactionError) -> Self {
        LibraryError::Redb(err.into())
    }
}

impl From<StorageError> for LibraryError {
    fn from(err: StorageError) -> Self {
        LibraryError::Redb(err.into())
    }
}

impl From<CommitError> for LibraryError {
    fn from(err: CommitError) -> Self {
        LibraryError::Redb(err.into())
    }
}

impl From<Box<bincode::ErrorKind>> for LibraryError {
    fn from(err: Box<bincode::ErrorKind>) -> Self {
        LibraryError::Bincode(err)
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs()
}

fn prefix_relpath(root_id: &str, relpath: &str) -> String {
    let id = root_id.trim();
    if id.is_empty() {
        relpath.to_string()
    } else {
        format!("{}{}{}", id, ROOT_SEP, relpath)
    }
}

fn split_root_relpath(relpath: &str) -> (Option<&str>, &str) {
    let Some(idx) = relpath.find(ROOT_SEP) else {
        return (None, relpath);
    };
    let (prefix, rest) = relpath.split_at(idx);
    let rest = &rest[ROOT_SEP.len()..];
    if prefix.is_empty() || rest.is_empty() {
        (None, relpath)
    } else {
        (Some(prefix), rest)
    }
}

#[cfg(test)]
mod tests;
