use std::collections::HashMap;
use std::sync::Arc;

use redb::{
    CommitError, Database, ReadableTable, StorageError, TableDefinition, TableError,
    TransactionError,
};
use serde::{Deserialize, Serialize};
use tracing::warn;

const STATS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("stats");

#[path = "stats_ledger.rs"]
mod ledger;
pub use ledger::counters::{CounterBatch, CounterCheckpoint, CounterSnapshot, CounterState};
pub use ledger::{
    DevicePreference, EventBatchResult, ListeningEvent, StatsResetResult, StatsSyncPage,
    MAX_EVENT_BATCH,
};

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct UserPeriodStats {
    pub total_ms: u64,
    #[serde(default)]
    pub track_plays: HashMap<String, u64>,
    #[serde(default)]
    pub track_ms: HashMap<String, u64>,
    #[serde(default)]
    pub artist_ms: HashMap<String, u64>,
    #[serde(default)]
    pub genre_ms: HashMap<String, u64>,
}

#[derive(Clone)]
pub struct StatsStore {
    db: Arc<Database>,
}

impl StatsStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    pub fn init_tables(&self) -> Result<(), StatsError> {
        let write_txn = self.db.begin_write()?;
        {
            let _ = write_txn.open_table(STATS_TABLE)?;
        }
        write_txn.commit()?;
        self.init_ledger()?;
        self.init_counters()?;
        Ok(())
    }

    pub fn get_period(
        &self,
        user_id: &str,
        year: i32,
        month: Option<u8>,
    ) -> Result<Option<UserPeriodStats>, StatsError> {
        let mut stats = self.get_legacy_period(user_id, year, month)?;
        if let Some(ledger) = self.get_counter_period(user_id, year, month)? {
            let combined = stats.get_or_insert_with(UserPeriodStats::default);
            combined.total_ms = combined.total_ms.saturating_add(ledger.total_ms);
            merge_counts(&mut combined.track_plays, ledger.track_plays);
            merge_counts(&mut combined.track_ms, ledger.track_ms);
            merge_counts(&mut combined.artist_ms, ledger.artist_ms);
            merge_counts(&mut combined.genre_ms, ledger.genre_ms);
        }
        Ok(stats)
    }

    fn get_legacy_period(
        &self,
        user_id: &str,
        year: i32,
        month: Option<u8>,
    ) -> Result<Option<UserPeriodStats>, StatsError> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(STATS_TABLE) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let key = stats_key(user_id, year, month);
        let mut should_clear = false;
        let stats = {
            let Some(value) = table.get(key.as_str())? else {
                return Ok(None);
            };
            match bincode::deserialize::<UserPeriodStats>(value.value()) {
                Ok(stats) => Some(stats),
                Err(err) => {
                    warn!(
                        "stats decode failed for key {}: {}; clearing entry",
                        key, err
                    );
                    should_clear = true;
                    None
                }
            }
        };
        drop(table);
        drop(read_txn);
        if should_clear {
            let _ = self.clear_entry(&key);
        }
        Ok(stats)
    }

    fn clear_entry(&self, key: &str) -> Result<(), StatsError> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(STATS_TABLE)?;
            let _ = table.remove(key)?;
        }
        write_txn.commit()?;
        Ok(())
    }
}

fn merge_counts(target: &mut HashMap<String, u64>, source: HashMap<String, u64>) {
    for (id, value) in source {
        let total = target.entry(id).or_default();
        *total = total.saturating_add(value);
    }
}

fn stats_key(user_id: &str, year: i32, month: Option<u8>) -> String {
    let month_value = month.unwrap_or(0);
    format!("{}:{}:{:02}", user_id, year, month_value)
}

#[derive(Debug)]
pub enum StatsError {
    Redb(redb::Error),
    Table(TableError),
    Transaction(TransactionError),
    Storage(StorageError),
    Commit(CommitError),
    Bincode(Box<bincode::ErrorKind>),
    Json(serde_json::Error),
    DeviceDisabled,
    GenerationMismatch,
    Conflict(String),
    ProtocolRetired,
    Invalid(String),
}

impl std::fmt::Display for StatsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StatsError::Redb(err) => write!(f, "redb error: {}", err),
            StatsError::Table(err) => write!(f, "redb table error: {}", err),
            StatsError::Transaction(err) => write!(f, "redb transaction error: {}", err),
            StatsError::Storage(err) => write!(f, "redb storage error: {}", err),
            StatsError::Commit(err) => write!(f, "redb commit error: {}", err),
            StatsError::Bincode(err) => write!(f, "bincode error: {}", err),
            StatsError::Json(err) => write!(f, "stats JSON error: {}", err),
            StatsError::DeviceDisabled => write!(f, "stats collection disabled for device"),
            StatsError::GenerationMismatch => write!(f, "stats history generation mismatch"),
            StatsError::Conflict(reason) => write!(f, "{}", reason),
            StatsError::ProtocolRetired => write!(
                f,
                "update this client to use listening statistics protocol v2"
            ),
            StatsError::Invalid(reason) => write!(f, "{}", reason),
        }
    }
}

impl From<serde_json::Error> for StatsError {
    fn from(err: serde_json::Error) -> Self {
        StatsError::Json(err)
    }
}

impl std::error::Error for StatsError {}

impl From<redb::Error> for StatsError {
    fn from(err: redb::Error) -> Self {
        StatsError::Redb(err)
    }
}

impl From<TableError> for StatsError {
    fn from(err: TableError) -> Self {
        StatsError::Table(err)
    }
}

impl From<TransactionError> for StatsError {
    fn from(err: TransactionError) -> Self {
        StatsError::Transaction(err)
    }
}

impl From<StorageError> for StatsError {
    fn from(err: StorageError) -> Self {
        StatsError::Storage(err)
    }
}

impl From<CommitError> for StatsError {
    fn from(err: CommitError) -> Self {
        StatsError::Commit(err)
    }
}

impl From<Box<bincode::ErrorKind>> for StatsError {
    fn from(err: Box<bincode::ErrorKind>) -> Self {
        StatsError::Bincode(err)
    }
}
