use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::download_profile::DownloadProfile;
use redb::{Database, ReadableTable, TableDefinition, TableError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[path = "download_jobs/legacy.rs"]
mod legacy;

const RECORD_JSON_PREFIX: &[u8] = b"phonolite-download-json-v1\0";

// Version 2 snapshots may contain likes, playlist membership, and scope item
// lists captured from the old shared personalization store. Preserve those
// tables for recovery, but never serve their jobs, indexes, or event snapshots
// through the account-isolated store. The HTTP download protocol remains v2.
const DOWNLOAD_JOBS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("download_jobs_v3");
const DOWNLOAD_JOB_REQUEST_INDEX: TableDefinition<&str, &[u8]> =
    TableDefinition::new("download_job_request_index_v3");
const DOWNLOAD_JOB_SCOPE_INDEX: TableDefinition<&str, &[u8]> =
    TableDefinition::new("download_job_scope_index_v3");
const DOWNLOAD_JOB_EVENTS_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("download_job_events_v3");

#[derive(Clone)]
pub struct DownloadJobStore {
    db: Arc<Database>,
}

impl DownloadJobStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    pub fn init_tables(&self) -> Result<(), String> {
        let write_txn = ::library::begin_write(&self.db).map_err(|err| err.to_string())?;
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
        let read_txn = self.db.begin_read().map_err(|err| err.to_string())?;
        match read_txn.open_table(TableDefinition::<&str, &[u8]>::new("download_jobs_v2")) {
            Ok(table) => {
                let legacy_jobs = table.len().map_err(|err| err.to_string())?;
                if legacy_jobs > 0 {
                    tracing::warn!(
                        legacy_jobs,
                        "Old download queues may contain shared personalization data; preserved but retired from account access. Recreate download queues to generate account-isolated metadata."
                    );
                }
            }
            Err(TableError::TableDoesNotExist(_)) => {}
            Err(err) => return Err(err.to_string()),
        }
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
        self.create_record_with_profile(
            user_id,
            client_id,
            client_request_id,
            scope,
            items,
            DownloadProfile::Original,
        )
    }

    pub fn create_record_with_profile(
        &self,
        user_id: String,
        client_id: String,
        client_request_id: String,
        scope: DownloadJobScope,
        items: Vec<DownloadJobItem>,
        profile: DownloadProfile,
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
            profile,
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
        self.get_by_scope_profile(user_id, client_id, scope, DownloadProfile::Original)
    }

    pub fn get_by_scope_profile(
        &self,
        user_id: &str,
        client_id: &str,
        scope: &DownloadJobScope,
        profile: DownloadProfile,
    ) -> Result<Option<DownloadJob>, String> {
        let scope_key = profile_scoped_index_key(user_id, client_id, scope, profile);
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
        let scoped_key =
            profile_scoped_index_key(&job.user_id, &job.client_id, &job.scope, job.profile);
        let bytes = encode_value(&job)?;
        let write_txn = ::library::begin_write(&self.db).map_err(|err| err.to_string())?;
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
        let scoped_key =
            profile_scoped_index_key(&job.user_id, &job.client_id, &job.scope, job.profile);
        let write_txn = ::library::begin_write(&self.db).map_err(|err| err.to_string())?;
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
        let scoped_key =
            profile_scoped_index_key(&job.user_id, &job.client_id, &job.scope, job.profile);
        let write_txn = ::library::begin_write(&self.db).map_err(|err| err.to_string())?;
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
    #[serde(default)]
    pub profile: DownloadProfile,
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
    #[serde(default)]
    pub profile: DownloadProfile,
    #[serde(default)]
    pub rendition_id: Option<String>,
    pub position: usize,
    pub track_id: String,
    pub status: DownloadJobItemStatus,
    #[serde(default)]
    pub download_url: Option<String>,
    #[serde(default)]
    pub offline_metadata: Option<crate::offline_metadata::OfflineTrackMetadata>,
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

fn profile_scoped_index_key(
    user_id: &str,
    client_id: &str,
    scope: &DownloadJobScope,
    profile: DownloadProfile,
) -> String {
    let original = scoped_index_key(user_id, client_id, scope);
    // Preserve every existing Original index key across upgrades.
    if profile == DownloadProfile::Original {
        original
    } else {
        format!("{}\u{1f}{}", original, profile.id())
    }
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
    let payload = serde_json::to_vec(value).map_err(|err| err.to_string())?;
    let mut bytes = Vec::with_capacity(RECORD_JSON_PREFIX.len() + payload.len());
    bytes.extend_from_slice(RECORD_JSON_PREFIX);
    bytes.extend_from_slice(&payload);
    Ok(bytes)
}

trait StoredRecord: for<'de> Deserialize<'de> {
    fn read_legacy(bytes: &[u8]) -> Result<Self, String>;
}

impl StoredRecord for DownloadJob {
    fn read_legacy(bytes: &[u8]) -> Result<Self, String> {
        legacy::decode_job(bytes)
    }
}

impl StoredRecord for DownloadJobEvent {
    fn read_legacy(bytes: &[u8]) -> Result<Self, String> {
        legacy::decode_event(bytes)
    }
}

fn decode_value<T: StoredRecord>(bytes: &[u8]) -> Result<T, String> {
    if let Some(payload) = bytes.strip_prefix(RECORD_JSON_PREFIX) {
        serde_json::from_slice(payload).map_err(|err| err.to_string())
    } else {
        T::read_legacy(bytes)
    }
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
                profile: DownloadProfile::Original,
                rendition_id: None,
                position: 0,
                track_id: "track-1".to_string(),
                status: DownloadJobItemStatus::ReadyToDownload,
                download_url: Some("/api/download/tracks/track-1/file".to_string()),
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
    fn scope_deduplication_keeps_original_and_each_compressed_profile_separate() {
        let (store, _db) = test_store();
        let scope = DownloadJobScope {
            kind: "album".into(),
            id: Some("album".into()),
            track_ids: vec![],
        };
        let mut ids = Vec::new();
        for (request, profile) in [
            DownloadProfile::Original,
            DownloadProfile::Opus64,
            DownloadProfile::Opus128,
        ]
        .into_iter()
        .enumerate()
        {
            let job = store.create_record_with_profile(
                "alice".into(),
                "phone".into(),
                request.to_string(),
                scope.clone(),
                vec![],
                profile,
            );
            let job = store.upsert_job(job).unwrap();
            assert_eq!(
                store
                    .get_by_scope_profile("alice", "phone", &scope, profile)
                    .unwrap()
                    .unwrap()
                    .job_id,
                job.job_id
            );
            assert!(store
                .get_by_scope_profile("bob", "phone", &scope, profile)
                .unwrap()
                .is_none());
            ids.push(job.job_id);
        }
        assert_eq!(
            ids.iter().collect::<std::collections::HashSet<_>>().len(),
            3
        );
        assert_eq!(
            store
                .get_by_scope("alice", "phone", &scope)
                .unwrap()
                .unwrap()
                .job_id,
            ids[0]
        );
        assert_eq!(
            store
                .get_by_scope_profile("alice", "phone", &scope, DownloadProfile::Opus64)
                .unwrap()
                .unwrap()
                .job_id,
            ids[1]
        );
    }

    #[test]
    fn tagged_json_without_download_profile_migrates_to_original() {
        let (store, _db) = test_store();
        let job = store.create_record(
            "alice".into(),
            "phone".into(),
            "old".into(),
            DownloadJobScope {
                kind: "track".into(),
                id: Some("song".into()),
                track_ids: vec![],
            },
            vec![],
        );
        let mut json = serde_json::to_value(job).unwrap();
        json.as_object_mut().unwrap().remove("profile");
        let job: DownloadJob = serde_json::from_value(json).unwrap();
        assert_eq!(job.profile, DownloadProfile::Original);
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

    #[test]
    fn shared_personalization_snapshots_are_retired_without_changing_legacy_tables() {
        const LEGACY_JOBS: TableDefinition<&str, &[u8]> = TableDefinition::new("download_jobs_v2");
        const LEGACY_REQUESTS: TableDefinition<&str, &[u8]> =
            TableDefinition::new("download_job_request_index_v2");
        const LEGACY_SCOPES: TableDefinition<&str, &[u8]> =
            TableDefinition::new("download_job_scope_index_v2");
        const LEGACY_EVENTS: TableDefinition<&str, &[u8]> =
            TableDefinition::new("download_job_events_v2");
        let (store, _db) = test_store();
        let scopes = [
            DownloadJobScope {
                kind: "playlist".into(),
                id: Some("legacy-ownerless-playlist".into()),
                track_ids: Vec::new(),
            },
            DownloadJobScope {
                kind: "liked".into(),
                id: None,
                track_ids: Vec::new(),
            },
        ];
        let shared_metadata: crate::offline_metadata::OfflineTrackMetadata =
            serde_json::from_value(serde_json::json!({
                "schema_version": 6,
                "track": {
                    "id": "track-from-shared-collection", "title": "Song", "artist": "Artist",
                    "album": "Album", "artist_id": "artist", "album_id": "album",
                    "duration_ms": 1000, "genres": [], "liked": true, "in_playlists": true
                },
                "album": {
                    "id": "album", "artist_id": "artist", "artist_ids": ["artist"],
                    "artist_name": "Artist", "artist_names": ["Artist"], "title": "Album",
                    "genres": [], "track_count": 1
                },
                "artist": {
                    "id": "artist", "name": "Artist", "genres": [], "album_count": 1
                }
            }))
            .unwrap();
        let mut old_rows = Vec::new();
        for (index, scope) in scopes.iter().enumerate() {
            let mut job = store.create_record(
                "alice".into(),
                "client".into(),
                format!("request-{index}"),
                scope.clone(),
                vec![DownloadJobItem {
                    profile: DownloadProfile::Original,
                    rendition_id: None,
                    position: 0,
                    track_id: "track-from-shared-collection".into(),
                    status: DownloadJobItemStatus::ReadyToDownload,
                    download_url: Some("/api/download/tracks/old/file".into()),
                    offline_metadata: Some(shared_metadata.clone()),
                    byte_length: Some(1),
                    content_type: Some("audio/flac".into()),
                    etag: None,
                    sha256: None,
                    error: None,
                }],
            );
            job.request_key = request_index_key("alice", "client", &job.client_request_id);
            job.event_cursor = 1;
            let event = DownloadJobEvent {
                cursor: 1,
                job_id: job.job_id.clone(),
                kind: "created".into(),
                message: "Old shared collection snapshot".into(),
                created_at: job.created_at,
                job: Some(job.clone()),
            };
            let job_bytes = encode_value(&job).unwrap();
            let event_bytes = encode_value(&event).unwrap();
            let scope_key = scoped_index_key("alice", "client", scope);
            let event_key = job_event_key(&job.job_id, 1);
            let txn = ::library::begin_write(&store.db).unwrap();
            txn.open_table(LEGACY_JOBS)
                .unwrap()
                .insert(job.job_id.as_str(), job_bytes.as_slice())
                .unwrap();
            txn.open_table(LEGACY_REQUESTS)
                .unwrap()
                .insert(job.request_key.as_str(), job.job_id.as_bytes())
                .unwrap();
            txn.open_table(LEGACY_SCOPES)
                .unwrap()
                .insert(scope_key.as_str(), job.job_id.as_bytes())
                .unwrap();
            txn.open_table(LEGACY_EVENTS)
                .unwrap()
                .insert(event_key.as_str(), event_bytes.as_slice())
                .unwrap();
            txn.commit().unwrap();
            old_rows.push((job, job_bytes, scope_key, event_key, event_bytes));
        }

        store.init_tables().unwrap();
        assert!(store
            .list_jobs("alice", None, None, None)
            .unwrap()
            .is_empty());
        for (old_job, _, _, _, _) in &old_rows {
            assert!(store.get_job(&old_job.job_id).unwrap().is_none());
            assert!(store
                .get_by_request("alice", "client", &old_job.client_request_id)
                .unwrap()
                .is_none());
            assert!(store
                .get_by_scope("alice", "client", &old_job.scope)
                .unwrap()
                .is_none());
            assert!(store.events_after(&old_job.job_id, 0).unwrap().is_empty());
        }

        // Reusing the same user, request ID, and scope must create fresh queues
        // without reviving the old collection membership or event snapshots.
        let mut fresh_jobs = Vec::new();
        for (old_job, _, _, _, _) in &old_rows {
            let fresh = store
                .upsert_job(store.create_record(
                    old_job.user_id.clone(),
                    old_job.client_id.clone(),
                    old_job.client_request_id.clone(),
                    old_job.scope.clone(),
                    Vec::new(),
                ))
                .unwrap();
            assert_ne!(fresh.job_id, old_job.job_id);
            assert!(fresh.items.is_empty());
            assert_eq!(
                store
                    .get_by_request("alice", "client", &fresh.client_request_id)
                    .unwrap()
                    .unwrap()
                    .job_id,
                fresh.job_id
            );
            assert_eq!(
                store
                    .get_by_scope("alice", "client", &fresh.scope)
                    .unwrap()
                    .unwrap()
                    .job_id,
                fresh.job_id
            );
            assert!(store
                .get_by_request("bob", "client", &fresh.client_request_id)
                .unwrap()
                .is_none());
            store
                .append_event(fresh.clone(), "created", "Account-isolated queue")
                .unwrap();
            assert_eq!(store.events_after(&fresh.job_id, 0).unwrap().len(), 1);
            assert!(store.events_after(&old_job.job_id, 0).unwrap().is_empty());
            fresh_jobs.push(fresh);
        }
        assert_eq!(store.list_jobs("alice", None, None, None).unwrap().len(), 2);
        assert!(store.list_jobs("bob", None, None, None).unwrap().is_empty());
        for fresh in fresh_jobs {
            store.delete_job(&fresh).unwrap();
        }
        store.init_tables().unwrap();
        assert!(store
            .list_jobs("alice", None, None, None)
            .unwrap()
            .is_empty());

        let txn = store.db.begin_read().unwrap();
        for (definition, key_column) in [
            (LEGACY_JOBS, 0),
            (LEGACY_REQUESTS, 1),
            (LEGACY_SCOPES, 2),
            (LEGACY_EVENTS, 3),
        ] {
            let table = txn.open_table(definition).unwrap();
            assert_eq!(table.len().unwrap(), 2);
            for (job, job_bytes, scope_key, event_key, event_bytes) in &old_rows {
                let (key, expected) = match key_column {
                    0 => (job.job_id.as_str(), job_bytes.as_slice()),
                    1 => (job.request_key.as_str(), job.job_id.as_bytes()),
                    2 => (scope_key.as_str(), job.job_id.as_bytes()),
                    _ => (event_key.as_str(), event_bytes.as_slice()),
                };
                assert_eq!(table.get(key).unwrap().unwrap().value(), expected);
            }
        }
    }
}
