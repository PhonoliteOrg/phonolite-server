use std::collections::HashMap;
use std::sync::Arc;

use redb::{CommitError, Database, ReadableTable, StorageError, TableDefinition, TableError, TransactionError};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

const STATS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("stats");

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct UserPeriodStats {
    pub total_ms: u64,
    pub track_ms: HashMap<String, u64>,
    pub artist_ms: HashMap<String, u64>,
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
        Ok(())
    }

    pub fn record_listen(
        &self,
        user_id: &str,
        track_id: &str,
        artist_id: &str,
        genres: &[String],
        duration_ms: u64,
    ) -> Result<(), StatsError> {
        if duration_ms == 0 {
            return Ok(());
        }
        let (year, month) = current_year_month();
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(STATS_TABLE)?;
            update_period(
                &mut table,
                user_id,
                year,
                Some(month),
                track_id,
                artist_id,
                genres,
                duration_ms,
            )?;
            update_period(
                &mut table,
                user_id,
                year,
                None,
                track_id,
                artist_id,
                genres,
                duration_ms,
            )?;
        }
        write_txn.commit()?;
        Ok(())
    }

    pub fn get_period(
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
        let Some(value) = table.get(key.as_str())? else {
            return Ok(None);
        };
        let stats: UserPeriodStats = bincode::deserialize(value.value())?;
        Ok(Some(stats))
    }
}

fn update_period(
    table: &mut redb::Table<&str, &[u8]>,
    user_id: &str,
    year: i32,
    month: Option<u8>,
    track_id: &str,
    artist_id: &str,
    genres: &[String],
    duration_ms: u64,
) -> Result<(), StatsError> {
    let key = stats_key(user_id, year, month);
    let mut stats = match table.get(key.as_str())? {
        Some(value) => bincode::deserialize(value.value())?,
        None => UserPeriodStats::default(),
    };
    stats.total_ms = stats.total_ms.saturating_add(duration_ms);
    *stats.track_ms.entry(track_id.to_string()).or_insert(0) += duration_ms;
    *stats.artist_ms.entry(artist_id.to_string()).or_insert(0) += duration_ms;
    for genre in genres {
        let trimmed = genre.trim();
        if trimmed.is_empty() {
            continue;
        }
        *stats.genre_ms.entry(trimmed.to_string()).or_insert(0) += duration_ms;
    }
    let bytes = bincode::serialize(&stats)?;
    table.insert(key.as_str(), bytes.as_slice())?;
    Ok(())
}

fn stats_key(user_id: &str, year: i32, month: Option<u8>) -> String {
    let month_value = month.unwrap_or(0);
    format!("{}:{}:{:02}", user_id, year, month_value)
}

fn current_year_month() -> (i32, u8) {
    let now = OffsetDateTime::now_utc();
    (now.year(), now.month() as u8)
}

#[derive(Debug)]
pub enum StatsError {
    Redb(redb::Error),
    Table(TableError),
    Transaction(TransactionError),
    Storage(StorageError),
    Commit(CommitError),
    Bincode(Box<bincode::ErrorKind>),
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
        }
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
