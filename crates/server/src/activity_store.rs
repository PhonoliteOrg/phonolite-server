use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use redb::{Database, ReadableTable, TableDefinition, TableError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const ACTIVITY_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("activity");

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActivityEntry {
    pub id: String,
    pub message: String,
    pub created_at: u64,
    pub kind: String,
}

#[derive(Clone)]
pub struct ActivityStore {
    db: Arc<Database>,
}

impl ActivityStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    pub fn init_tables(&self) -> Result<(), String> {
        let write_txn = self.db.begin_write().map_err(|e| e.to_string())?;
        let _ = write_txn
            .open_table(ACTIVITY_TABLE)
            .map_err(|e| e.to_string())?;
        write_txn.commit().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn add_event(&self, kind: &str, message: impl Into<String>) -> Result<(), String> {
        let created_at = now_secs();
        let id = format!("{}-{}", created_at, Uuid::new_v4());
        let entry = ActivityEntry {
            id: id.clone(),
            message: message.into(),
            created_at,
            kind: kind.to_string(),
        };
        let bytes = bincode::serialize(&entry).map_err(|e| e.to_string())?;
        let write_txn = self.db.begin_write().map_err(|e| e.to_string())?;
        {
            let mut table = write_txn
                .open_table(ACTIVITY_TABLE)
                .map_err(|e| e.to_string())?;
            table
                .insert(id.as_str(), bytes.as_slice())
                .map_err(|e| e.to_string())?;
        }
        write_txn.commit().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn list_events(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<ActivityEntry>, usize), String> {
        let read_txn = self.db.begin_read().map_err(|e| e.to_string())?;
        let table = match read_txn.open_table(ACTIVITY_TABLE) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok((Vec::new(), 0)),
            Err(err) => return Err(err.to_string()),
        };

        let mut all = Vec::new();
        for entry in table.iter().map_err(|err| err.to_string())? {
            let entry = entry.map_err(|err| err.to_string())?;
            let item: ActivityEntry =
                bincode::deserialize(entry.1.value()).map_err(|err| err.to_string())?;
            all.push(item);
        }
        all.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        let total = all.len();
        let items = all.into_iter().skip(offset).take(limit).collect();
        Ok((items, total))
    }

    pub fn clear_events(&self) -> Result<(), String> {
        let write_txn = self.db.begin_write().map_err(|e| e.to_string())?;
        match write_txn.delete_table(ACTIVITY_TABLE) {
            Ok(_) | Err(TableError::TableDoesNotExist(_)) => {}
            Err(err) => return Err(err.to_string()),
        }
        write_txn
            .open_table(ACTIVITY_TABLE)
            .map_err(|e| e.to_string())?;
        write_txn.commit().map_err(|e| e.to_string())?;
        Ok(())
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
}
