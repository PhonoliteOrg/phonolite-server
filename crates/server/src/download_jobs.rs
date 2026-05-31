use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use redb::{Database, ReadableTable, TableDefinition, TableError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const DOWNLOAD_JOBS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("download_jobs_v2");
const DOWNLOAD_JOB_REQUEST_INDEX: TableDefinition<&str, &[u8]> =
    TableDefinition::new("download_job_request_index_v2");
const DOWNLOAD_JOB_SCOPE_INDEX: TableDefinition<&str, &[u8]> =
    TableDefinition::new("download_job_scope_index_v2");
const DOWNLOAD_JOB_EVENTS_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("download_job_events_v2");

#[derive(Clone)]
pub struct DownloadJobStore {
    db: Arc<Database>,
}

impl DownloadJobStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    pub fn init_tables(&self) -> Result<(), String> {
        let write_txn = self.db.begin_write().map_err(|err| err.to_string())?;
        {
            let _ = write_txn
                .open_table(DOWNLOAD_JOBS_TABLE)
                .map_err(|err| err.to_string())?;
            let _ = write_txn
                .open_table(DOWNLOAD_JOB_REQUEST_INDEX)
                .map_err(|err| err.to_string())?;
            let _ = write_txn
                .open_table(DOWNLOAD_JOB_SCOPE_INDEX)
                .map_err(|err| err.to_string())?;
            let _ = write_txn
                .open_table(DOWNLOAD_JOB_EVENTS_TABLE)
                .map_err(|err| err.to_string())?;
        }
        write_txn.commit().map_err(|err| err.to_string())?;
        Ok(())
    }

    pub fn create_record(
        &self,
        user_id: String,
        client_id: String,
        client_request_id: String,
        scope: DownloadJobScope,
        items: Vec<DownloadJobItem>,
    ) -> DownloadJob {
        let now = now_millis();
        let ready_count = items
            .iter()
            .filter(|item| item.status == DownloadJobItemStatus::ReadyToDownload)
            .count();
        let failed_count = items
            .iter()
            .filter(|item| item.status == DownloadJobItemStatus::MetadataFailed)
            .count();
        let status = if ready_count > 0 {
            DownloadJobStatus::ReadyToDownload
        } else if failed_count > 0 {
            DownloadJobStatus::MetadataFailed
        } else {
            DownloadJobStatus::Queued
        };
        DownloadJob {
            job_id: Uuid::new_v4().to_string(),
            user_id,
            client_id,
            client_request_id,
            scope_key: scope_index_key(&scope),
            request_key: String::new(),
            scope,
            status,
            created_at: now,
            updated_at: now,
            total_count: items.len(),
            ready_count,
            completed_count: 0,
            failed_count,
            event_cursor: 0,
            items,
        }
    }

    pub fn get_job(&self, job_id: &str) -> Result<Option<DownloadJob>, String> {
        let read_txn = self.db.begin_read().map_err(|err| err.to_string())?;
        let table = match read_txn.open_table(DOWNLOAD_JOBS_TABLE) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(err.to_string()),
        };
        let value = table
            .get(job_id)
            .map_err(|err| err.to_string())?
            .map(|value| decode_value(value.value()))
            .transpose()?;
        Ok(value)
    }

    pub fn get_by_request(
        &self,
        user_id: &str,
        client_id: &str,
        client_request_id: &str,
    ) -> Result<Option<DownloadJob>, String> {
        let request_key = request_index_key(user_id, client_id, client_request_id);
        let Some(job_id) = self.lookup_index(DOWNLOAD_JOB_REQUEST_INDEX, &request_key)? else {
            return Ok(None);
        };
        self.get_job(&job_id)
    }

    pub fn get_by_scope(
        &self,
        user_id: &str,
        client_id: &str,
        scope: &DownloadJobScope,
    ) -> Result<Option<DownloadJob>, String> {
        let scope_key = scoped_index_key(user_id, client_id, scope);
        let Some(job_id) = self.lookup_index(DOWNLOAD_JOB_SCOPE_INDEX, &scope_key)? else {
            return Ok(None);
        };
        let Some(job) = self.get_job(&job_id)? else {
            return Ok(None);
        };
        if job.status.blocks_new_queue() {
            Ok(Some(job))
        } else {
            Ok(None)
        }
    }

    pub fn list_jobs(
        &self,
        user_id: &str,
        client_id: Option<&str>,
        scope_kind: Option<&str>,
        scope_id: Option<&str>,
    ) -> Result<Vec<DownloadJob>, String> {
        let read_txn = self.db.begin_read().map_err(|err| err.to_string())?;
        let table = match read_txn.open_table(DOWNLOAD_JOBS_TABLE) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(err.to_string()),
        };
        let mut jobs = Vec::new();
        for entry in table.iter().map_err(|err| err.to_string())? {
            let entry = entry.map_err(|err| err.to_string())?;
            let job: DownloadJob = decode_value(entry.1.value())?;
            if job.user_id != user_id {
                continue;
            }
            if client_id
                .map(|client_id| job.client_id != client_id)
                .unwrap_or(false)
            {
                continue;
            }
            if scope_kind
                .map(|scope_kind| job.scope.kind != scope_kind)
                .unwrap_or(false)
            {
                continue;
            }
            if scope_id
                .map(|scope_id| job.scope.id.as_deref() != Some(scope_id))
                .unwrap_or(false)
            {
                continue;
            }
            jobs.push(job);
        }
        jobs.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        Ok(jobs)
    }

    pub fn upsert_job(&self, mut job: DownloadJob) -> Result<DownloadJob, String> {
        job.updated_at = now_millis();
        job.scope_key = scope_index_key(&job.scope);
        job.request_key = request_index_key(&job.user_id, &job.client_id, &job.client_request_id);
        let scoped_key = scoped_index_key(&job.user_id, &job.client_id, &job.scope);
        let bytes = encode_value(&job)?;
        let write_txn = self.db.begin_write().map_err(|err| err.to_string())?;
        {
            let mut jobs = write_txn
                .open_table(DOWNLOAD_JOBS_TABLE)
                .map_err(|err| err.to_string())?;
            jobs.insert(job.job_id.as_str(), bytes.as_slice())
                .map_err(|err| err.to_string())?;
        }
        {
            let mut requests = write_txn
                .open_table(DOWNLOAD_JOB_REQUEST_INDEX)
                .map_err(|err| err.to_string())?;
            requests
                .insert(job.request_key.as_str(), job.job_id.as_bytes())
                .map_err(|err| err.to_string())?;
        }
        {
            let mut scopes = write_txn
                .open_table(DOWNLOAD_JOB_SCOPE_INDEX)
                .map_err(|err| err.to_string())?;
            if job.status.blocks_new_queue() {
                scopes
                    .insert(scoped_key.as_str(), job.job_id.as_bytes())
                    .map_err(|err| err.to_string())?;
            } else {
                let _ = scopes.remove(scoped_key.as_str());
            }
        }
        write_txn.commit().map_err(|err| err.to_string())?;
        Ok(job)
    }

    pub fn delete_job(&self, job: &DownloadJob) -> Result<(), String> {
        let scoped_key = scoped_index_key(&job.user_id, &job.client_id, &job.scope);
        let write_txn = self.db.begin_write().map_err(|err| err.to_string())?;
        {
            let mut jobs = write_txn
                .open_table(DOWNLOAD_JOBS_TABLE)
                .map_err(|err| err.to_string())?;
            let _ = jobs.remove(job.job_id.as_str());
        }
        {
            let mut requests = write_txn
                .open_table(DOWNLOAD_JOB_REQUEST_INDEX)
                .map_err(|err| err.to_string())?;
            let _ = requests.remove(job.request_key.as_str());
        }
        {
            let mut scopes = write_txn
                .open_table(DOWNLOAD_JOB_SCOPE_INDEX)
                .map_err(|err| err.to_string())?;
            let _ = scopes.remove(scoped_key.as_str());
        }
        write_txn.commit().map_err(|err| err.to_string())?;
        Ok(())
    }

    pub fn append_event(
        &self,
        mut job: DownloadJob,
        kind: impl Into<String>,
        message: impl Into<String>,
    ) -> Result<DownloadJobEvent, String> {
        job.event_cursor = job.event_cursor.saturating_add(1);
        job.updated_at = now_millis();
        let event = DownloadJobEvent {
            cursor: job.event_cursor,
            job_id: job.job_id.clone(),
            kind: kind.into(),
            message: message.into(),
            created_at: job.updated_at,
            job: Some(job.clone()),
        };
        let event_key = job_event_key(&job.job_id, event.cursor);
        let event_bytes = encode_value(&event)?;
        let job_bytes = encode_value(&job)?;
        let scoped_key = scoped_index_key(&job.user_id, &job.client_id, &job.scope);
        let write_txn = self.db.begin_write().map_err(|err| err.to_string())?;
        {
            let mut jobs = write_txn
                .open_table(DOWNLOAD_JOBS_TABLE)
                .map_err(|err| err.to_string())?;
            jobs.insert(job.job_id.as_str(), job_bytes.as_slice())
                .map_err(|err| err.to_string())?;
        }
        {
            let mut events = write_txn
                .open_table(DOWNLOAD_JOB_EVENTS_TABLE)
                .map_err(|err| err.to_string())?;
            events
                .insert(event_key.as_str(), event_bytes.as_slice())
                .map_err(|err| err.to_string())?;
        }
        {
            let mut scopes = write_txn
                .open_table(DOWNLOAD_JOB_SCOPE_INDEX)
                .map_err(|err| err.to_string())?;
            if job.status.blocks_new_queue() {
                scopes
                    .insert(scoped_key.as_str(), job.job_id.as_bytes())
                    .map_err(|err| err.to_string())?;
            } else {
                let _ = scopes.remove(scoped_key.as_str());
            }
        }
        write_txn.commit().map_err(|err| err.to_string())?;
        Ok(event)
    }

    pub fn events_after(&self, job_id: &str, cursor: u64) -> Result<Vec<DownloadJobEvent>, String> {
        let read_txn = self.db.begin_read().map_err(|err| err.to_string())?;
        let table = match read_txn.open_table(DOWNLOAD_JOB_EVENTS_TABLE) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(err.to_string()),
        };
        let prefix = format!("{}:", job_id);
        let mut events = Vec::new();
        for entry in table.iter().map_err(|err| err.to_string())? {
            let entry = entry.map_err(|err| err.to_string())?;
            let key = entry.0.value();
            if !key.starts_with(&prefix) {
                continue;
            }
            let event: DownloadJobEvent = decode_value(entry.1.value())?;
            if event.cursor > cursor {
                events.push(event);
            }
        }
        events.sort_by(|left, right| left.cursor.cmp(&right.cursor));
        Ok(events)
    }

    fn lookup_index(
        &self,
        table_def: TableDefinition<&str, &[u8]>,
        key: &str,
    ) -> Result<Option<String>, String> {
        let read_txn = self.db.begin_read().map_err(|err| err.to_string())?;
        let table = match read_txn.open_table(table_def) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(err.to_string()),
        };
        let value = table
            .get(key)
            .map_err(|err| err.to_string())?
            .map(|value| String::from_utf8_lossy(value.value()).to_string());
        Ok(value)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DownloadJob {
    pub job_id: String,
    pub user_id: String,
    pub client_id: String,
    pub client_request_id: String,
    pub request_key: String,
    pub scope_key: String,
    pub scope: DownloadJobScope,
    pub status: DownloadJobStatus,
    pub created_at: u64,
    pub updated_at: u64,
    pub total_count: usize,
    pub ready_count: usize,
    pub completed_count: usize,
    pub failed_count: usize,
    pub event_cursor: u64,
    pub items: Vec<DownloadJobItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DownloadJobScope {
    pub kind: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub track_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadJobStatus {
    Queued,
    ResolvingMetadata,
    ReadyToDownload,
    Downloading,
    Paused,
    Complete,
    Failed,
    MetadataFailed,
    Canceled,
    Removing,
}

impl DownloadJobStatus {
    pub fn blocks_new_queue(&self) -> bool {
        !matches!(
            self,
            DownloadJobStatus::Failed
                | DownloadJobStatus::MetadataFailed
                | DownloadJobStatus::Canceled
                | DownloadJobStatus::Removing
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DownloadJobItem {
    pub position: usize,
    pub track_id: String,
    pub status: DownloadJobItemStatus,
    #[serde(default)]
    pub download_url: Option<String>,
    #[serde(default)]
    pub offline_metadata: Option<crate::api::library::OfflineTrackMetadata>,
    #[serde(default)]
    pub byte_length: Option<u64>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadJobItemStatus {
    Queued,
    ResolvingMetadata,
    ReadyToDownload,
    Downloading,
    Paused,
    Verifying,
    Complete,
    Failed,
    MetadataFailed,
    Canceled,
    Removing,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DownloadJobEvent {
    pub cursor: u64,
    pub job_id: String,
    pub kind: String,
    pub message: String,
    pub created_at: u64,
    #[serde(default)]
    pub job: Option<DownloadJob>,
}

pub fn scoped_index_key(user_id: &str, client_id: &str, scope: &DownloadJobScope) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}",
        user_id,
        client_id.trim(),
        scope_index_key(scope)
    )
}

fn request_index_key(user_id: &str, client_id: &str, client_request_id: &str) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}",
        user_id,
        client_id.trim(),
        client_request_id.trim()
    )
}

fn scope_index_key(scope: &DownloadJobScope) -> String {
    let kind = scope.kind.trim().to_ascii_lowercase();
    if kind == "track_set" {
        let mut ids = scope
            .track_ids
            .iter()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        ids.sort();
        ids.dedup();
        return format!("track_set:{}", ids.join(","));
    }
    format!("{}:{}", kind, scope.id.as_deref().unwrap_or("").trim())
}

fn job_event_key(job_id: &str, cursor: u64) -> String {
    format!("{}:{:020}", job_id, cursor)
}

fn encode_value<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    bincode::serialize(value).map_err(|err| err.to_string())
}

fn decode_value<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, String> {
    bincode::deserialize(bytes).map_err(|err| err.to_string())
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDb {
        path: std::path::PathBuf,
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn test_store() -> (DownloadJobStore, TempDb) {
        let path =
            std::env::temp_dir().join(format!("phonolite_download_jobs_{}.redb", Uuid::new_v4()));
        let db = Arc::new(Database::create(&path).unwrap());
        let store = DownloadJobStore::new(db);
        store.init_tables().unwrap();
        (store, TempDb { path })
    }

    #[test]
    fn indexes_active_jobs_by_request_and_scope() {
        let (store, _db) = test_store();
        let scope = DownloadJobScope {
            kind: "artist".to_string(),
            id: Some("artist-1".to_string()),
            track_ids: Vec::new(),
        };
        let job = store.create_record(
            "user-1".to_string(),
            "client-1".to_string(),
            "request-1".to_string(),
            scope.clone(),
            vec![DownloadJobItem {
                position: 0,
                track_id: "track-1".to_string(),
                status: DownloadJobItemStatus::ReadyToDownload,
                download_url: Some("/api/v1/download/v2/tracks/track-1/file".to_string()),
                offline_metadata: None,
                byte_length: Some(123),
                content_type: Some("audio/mpeg".to_string()),
                etag: Some("\"track-1\"".to_string()),
                sha256: None,
                error: None,
            }],
        );
        let stored = store.upsert_job(job).unwrap();

        assert_eq!(
            store
                .get_by_request("user-1", "client-1", "request-1")
                .unwrap()
                .unwrap()
                .job_id,
            stored.job_id
        );
        assert_eq!(
            store
                .get_by_scope("user-1", "client-1", &scope)
                .unwrap()
                .unwrap()
                .job_id,
            stored.job_id
        );
    }

    #[test]
    fn terminal_jobs_stop_blocking_new_scope_queues() {
        let (store, _db) = test_store();
        let scope = DownloadJobScope {
            kind: "album".to_string(),
            id: Some("album-1".to_string()),
            track_ids: Vec::new(),
        };
        let mut job = store.create_record(
            "user-1".to_string(),
            "client-1".to_string(),
            "request-1".to_string(),
            scope.clone(),
            Vec::new(),
        );
        job.status = DownloadJobStatus::Canceled;
        store.upsert_job(job).unwrap();

        assert!(store
            .get_by_scope("user-1", "client-1", &scope)
            .unwrap()
            .is_none());
    }

    #[test]
    fn job_events_replay_after_cursor() {
        let (store, _db) = test_store();
        let scope = DownloadJobScope {
            kind: "track".to_string(),
            id: Some("track-1".to_string()),
            track_ids: Vec::new(),
        };
        let job = store
            .upsert_job(store.create_record(
                "user-1".to_string(),
                "client-1".to_string(),
                "request-1".to_string(),
                scope,
                Vec::new(),
            ))
            .unwrap();
        let first = store
            .append_event(job.clone(), "created", "created")
            .unwrap();
        let second = store
            .append_event(
                store.get_job(&job.job_id).unwrap().unwrap(),
                "updated",
                "updated",
            )
            .unwrap();

        let replay = store.events_after(&job.job_id, first.cursor).unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].cursor, second.cursor);
        assert_eq!(replay[0].kind, "updated");
    }
}
