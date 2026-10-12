//! Persistent music catalog facade.
//!
//! The public API is stable; private modules own read queries, enrichment
//! transactions, storage encoding, and filesystem indexing independently.
mod credits;
mod enrichment;
mod indexing;
mod normalization;
mod queries;
mod source_identity;
mod storage;

pub use credits::resolve_display_credit;
pub use source_identity::{AlbumSourceIdentity, ALBUM_SOURCE_IDENTITY_SCHEMA_VERSION};

use std::path::{Path, PathBuf};
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
}

impl Library {
    pub fn load_or_scan(
        roots: Vec<LibraryRoot>,
        db_path: PathBuf,
    ) -> Result<(Self, bool), LibraryError> {
        let db = open_or_create_db(&db_path)?;
        let library = Self {
            roots,
            db: Arc::new(db),
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
                library.rescan()?;
                scanned = true;
            }
            None => {
                warn!("Index missing; scanning");
                library.rescan()?;
                scanned = true;
            }
        }

        Ok((library, scanned))
    }

    pub fn load_or_scan_with_db(
        roots: Vec<LibraryRoot>,
        db: Arc<Database>,
    ) -> Result<(Self, bool), LibraryError> {
        Self::load_or_scan_db(roots, db, None)
    }

    pub fn load_or_scan_with_db_and_progress(
        roots: Vec<LibraryRoot>,
        db: Arc<Database>,
        progress: ScanObserver<'_>,
    ) -> Result<(Self, bool), LibraryError> {
        Self::load_or_scan_db(roots, db, Some(progress))
    }

    fn load_or_scan_db(
        roots: Vec<LibraryRoot>,
        db: Arc<Database>,
        progress: Option<ScanObserver<'_>>,
    ) -> Result<(Self, bool), LibraryError> {
        let library = Self { roots, db };
        let mut scanned = false;
        match read_version(&library.db)? {
            Some(version) if version == INDEX_VERSION => {
                if indexing::needs_catalog_recovery(&library.db)? {
                    info!(
                        "Restoring local metadata before verified enrichment in existing database"
                    );
                    indexing::scan(&library.roots, &library.db, false, None, progress)?;
                    scanned = true;
                } else {
                    info!("Loaded index from existing database");
                }
            }
            Some(version) => {
                warn!("Index version mismatch ({}); rescanning", version);
                indexing::scan(&library.roots, &library.db, true, None, progress)?;
                scanned = true;
            }
            None => {
                warn!("Index missing; scanning");
                indexing::scan(&library.roots, &library.db, true, None, progress)?;
                scanned = true;
            }
        }
        Ok((library, scanned))
    }

    pub fn open_db(path: &Path) -> Result<Arc<Database>, LibraryError> {
        let db = open_or_create_db(path)?;
        Ok(Arc::new(db))
    }

    pub fn rescan(&self) -> Result<LibraryStats, LibraryError> {
        indexing::scan(&self.roots, &self.db, true, None, None)
    }

    pub fn incremental_scan(&self) -> Result<LibraryStats, LibraryError> {
        indexing::scan(&self.roots, &self.db, false, None, None)
    }

    pub fn incremental_scan_paths(&self, paths: &[PathBuf]) -> Result<LibraryStats, LibraryError> {
        indexing::scan(&self.roots, &self.db, false, Some(paths), None)
    }

    pub fn scan_with_progress(
        &self,
        force: bool,
        changed_paths: Option<&[PathBuf]>,
        progress: ScanObserver<'_>,
    ) -> Result<LibraryStats, LibraryError> {
        indexing::scan(&self.roots, &self.db, force, changed_paths, Some(progress))
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
