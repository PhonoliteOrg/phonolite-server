//! Finite admission and startup memory budgets shared by authenticated workloads.
use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::shutdown::Shutdown;

pub const MIB: usize = 1024 * 1024;

/// Changes apply when the server restarts; live settings do not resize owners.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceLimits {
    pub catalogue_database_mib: usize,
    pub user_database_mib: usize,
    pub statistics_database_mib: usize,
    pub catalogue_responses_mib: usize,
    pub catalogue_response_entries: usize,
    pub audio_cache_mib: usize,
    pub catalogue_active: usize,
    pub catalogue_queued: usize,
    pub catalogue_per_account: usize,
    pub catalogue_queued_per_account: usize,
    pub artwork_active: usize,
    pub artwork_queued: usize,
    pub artwork_per_account: usize,
    pub artwork_queued_per_account: usize,
    pub thumbnail_workers: usize,
    pub thumbnail_queued: usize,
    pub thumbnail_disk_mib: usize,
    pub thumbnail_source_mib: usize,
    pub thumbnail_max_pixels: u64,
    pub thumbnail_max_dimension: u32,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            catalogue_database_mib: 48,
            user_database_mib: 8,
            statistics_database_mib: 8,
            catalogue_responses_mib: 8,
            catalogue_response_entries: 512,
            audio_cache_mib: 32,
            catalogue_active: 4,
            catalogue_queued: 16,
            catalogue_per_account: 2,
            catalogue_queued_per_account: 4,
            artwork_active: 16,
            artwork_queued: 32,
            artwork_per_account: 4,
            artwork_queued_per_account: 8,
            thumbnail_workers: 1,
            thumbnail_queued: 16,
            thumbnail_disk_mib: 1024,
            thumbnail_source_mib: 16,
            thumbnail_max_pixels: 8_000_000,
            thumbnail_max_dimension: 8192,
        }
    }
}

impl ResourceLimits {
    pub fn validate(&self) -> Result<(), &'static str> {
        for value in [
            self.catalogue_database_mib,
            self.user_database_mib,
            self.statistics_database_mib,
            self.catalogue_responses_mib,
            self.thumbnail_disk_mib,
            self.thumbnail_source_mib,
        ] {
            if value == 0 || value > 16384 || value.checked_mul(MIB).is_none() {
                return Err("resource memory/disk budgets must be between 1 and 16384 MiB");
            }
        }
        if !(4..=4096).contains(&self.audio_cache_mib) {
            return Err("audio cache budget must be between 4 and 4096 MiB");
        }
        for value in [
            self.catalogue_active,
            self.catalogue_per_account,
            self.artwork_active,
            self.artwork_per_account,
            self.thumbnail_workers,
        ] {
            if !(1..=256).contains(&value) {
                return Err("resource concurrency must be between 1 and 256");
            }
        }
        for value in [
            self.catalogue_queued,
            self.catalogue_queued_per_account,
            self.artwork_queued,
            self.artwork_queued_per_account,
            self.thumbnail_queued,
        ] {
            if value > 1024 {
                return Err("resource queues may not exceed 1024 entries");
            }
        }
        if !(1..=16384).contains(&self.catalogue_response_entries)
            || !(1..=64_000_000).contains(&self.thumbnail_max_pixels)
            || !(1..=16384).contains(&self.thumbnail_max_dimension)
        {
            return Err("catalogue cache entries or thumbnail dimensions exceed resource limits");
        }
        if self.catalogue_per_account > self.catalogue_active
            || self.artwork_per_account > self.artwork_active
        {
            return Err("per-account concurrency may not exceed global concurrency");
        }
        Ok(())
    }
}

pub struct Resources {
    pub catalogue: WorkGate,
    pub artwork: WorkGate,
}

impl Default for Resources {
    fn default() -> Self {
        Self::new(&ResourceLimits::default())
    }
}

impl Resources {
    pub fn new(limits: &ResourceLimits) -> Self {
        Self {
            catalogue: WorkGate::new(
                limits.catalogue_active,
                limits.catalogue_queued,
                limits.catalogue_per_account,
                limits.catalogue_queued_per_account,
            ),
            artwork: WorkGate::new(
                limits.artwork_active,
                limits.artwork_queued,
                limits.artwork_per_account,
                limits.artwork_queued_per_account,
            ),
        }
    }
}

pub struct WorkGate {
    active: Arc<Semaphore>,
    admitted: Arc<Semaphore>,
    accounts: Mutex<HashMap<String, Weak<AccountGate>>>,
    account_active: usize,
    account_capacity: usize,
}

struct AccountGate {
    active: Arc<Semaphore>,
    admitted: Arc<Semaphore>,
}

/// Own all reservations until the response or computation is dropped.
pub struct WorkPermit {
    _global_active: OwnedSemaphorePermit,
    _account_active: OwnedSemaphorePermit,
    _global_admitted: OwnedSemaphorePermit,
    _account_admitted: OwnedSemaphorePermit,
    _account: Arc<AccountGate>,
}

impl WorkPermit {
    /// Slow response bodies retain admission until completion or cancellation.
    pub fn retain_response(self: Arc<Self>, response: Response) -> Response {
        use futures_util::StreamExt;
        let (parts, body) = response.into_parts();
        let stream = futures_util::stream::unfold(
            (body.into_data_stream(), self),
            |(mut body, permit)| async move { body.next().await.map(|chunk| (chunk, (body, permit))) },
        );
        Response::from_parts(parts, axum::body::Body::from_stream(stream))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionError {
    Account,
    Capacity,
    Shutdown,
}

impl AdmissionError {
    pub fn response(self) -> Response {
        let status = if self == Self::Account {
            StatusCode::TOO_MANY_REQUESTS
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        };
        let mut response =
            crate::http::json_error_response(status, "request capacity reached; retry shortly");
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        response
    }
}

impl WorkGate {
    pub fn new(active: usize, queued: usize, account_active: usize, account_queued: usize) -> Self {
        Self {
            active: Arc::new(Semaphore::new(active)),
            admitted: Arc::new(Semaphore::new(active + queued)),
            accounts: Mutex::new(HashMap::new()),
            account_active,
            account_capacity: account_active + account_queued,
        }
    }

    pub async fn acquire(
        &self,
        account: &str,
        shutdown: &Shutdown,
    ) -> Result<WorkPermit, AdmissionError> {
        if shutdown.is_cancelled() {
            return Err(AdmissionError::Shutdown);
        }
        let global_admitted = self
            .admitted
            .clone()
            .try_acquire_owned()
            .map_err(|_| AdmissionError::Capacity)?;
        let account = {
            let mut accounts = self.accounts.lock();
            accounts.retain(|_, gate| gate.strong_count() > 0);
            if let Some(gate) = accounts.get(account).and_then(Weak::upgrade) {
                gate
            } else {
                let gate = Arc::new(AccountGate {
                    active: Arc::new(Semaphore::new(self.account_active)),
                    admitted: Arc::new(Semaphore::new(self.account_capacity)),
                });
                accounts.insert(account.to_owned(), Arc::downgrade(&gate));
                gate
            }
        };
        let account_admitted = account
            .admitted
            .clone()
            .try_acquire_owned()
            .map_err(|_| AdmissionError::Account)?;
        let acquire = async {
            let account_active = account
                .active
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| AdmissionError::Shutdown)?;
            let global_active = self
                .active
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| AdmissionError::Shutdown)?;
            Ok((global_active, account_active))
        };
        let (global_active, account_active) = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Err(AdmissionError::Shutdown),
            result = tokio::time::timeout(Duration::from_secs(2), acquire) => {
                result.map_err(|_| AdmissionError::Capacity)??
            }
        };
        Ok(WorkPermit {
            _global_active: global_active,
            _account_active: account_active,
            _global_admitted: global_admitted,
            _account_admitted: account_admitted,
            _account: account,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budgets_default_without_legacy_config_and_validate_limits() {
        let defaults: ResourceLimits = serde_json::from_str("{}").unwrap();
        assert_eq!(defaults.catalogue_database_mib, 48);
        assert_eq!(defaults.audio_cache_mib, 32);
        assert!(defaults.validate().is_ok());
        let mut invalid = defaults;
        invalid.catalogue_queued = 1025;
        assert!(invalid.validate().is_err());
        invalid.catalogue_queued = 16;
        invalid.audio_cache_mib = 0;
        assert!(invalid.validate().is_err());
    }

    #[tokio::test]
    async fn account_capacity_does_not_prevent_another_account_and_drop_releases_slots() {
        let gate = WorkGate::new(2, 0, 1, 0);
        let shutdown = Shutdown::new();
        let first = gate.acquire("first", &shutdown).await.unwrap();
        assert_eq!(
            gate.acquire("first", &shutdown).await.err(),
            Some(AdmissionError::Account)
        );
        let second = gate.acquire("second", &shutdown).await.unwrap();
        assert_eq!(
            gate.acquire("third", &shutdown).await.err(),
            Some(AdmissionError::Capacity)
        );
        drop(first);
        drop(second);
        assert!(gate.acquire("third", &shutdown).await.is_ok());
        assert!(gate.accounts.lock().len() <= 1);
    }

    #[tokio::test]
    async fn bounded_queue_rejects_overflow_and_shutdown_releases_waiters() {
        let gate = Arc::new(WorkGate::new(1, 1, 1, 1));
        let shutdown = Shutdown::new();
        let active = gate.acquire("active", &shutdown).await.unwrap();
        let pending = {
            let gate = gate.clone();
            let shutdown = shutdown.clone();
            tokio::spawn(async move { gate.acquire("queued", &shutdown).await })
        };
        tokio::task::yield_now().await;
        assert_eq!(
            gate.acquire("overflow", &shutdown).await.err(),
            Some(AdmissionError::Capacity)
        );
        shutdown.cancel();
        assert_eq!(pending.await.unwrap().err(), Some(AdmissionError::Shutdown));
        drop(active);
        assert_eq!(gate.admitted.available_permits(), 2);
        assert_eq!(gate.active.available_permits(), 1);
    }

    #[tokio::test]
    async fn response_drop_cannot_release_a_running_blocking_workers_budget() {
        let gate = WorkGate::new(1, 0, 1, 0);
        let shutdown = Shutdown::new();
        let permit = Arc::new(gate.acquire("account", &shutdown).await.unwrap());
        let worker_permit = Arc::clone(&permit);
        let (started, running) = tokio::sync::oneshot::channel();
        let (finish, finished) = std::sync::mpsc::channel();
        let worker = tokio::task::spawn_blocking(move || {
            let _permit = worker_permit;
            let _ = started.send(());
            finished.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        running.await.unwrap();
        let response = permit.retain_response(Response::new(axum::body::Body::from("body")));
        assert_eq!(
            gate.acquire("other", &shutdown).await.err(),
            Some(AdmissionError::Capacity)
        );
        drop(response);
        assert_eq!(
            gate.acquire("other", &shutdown).await.err(),
            Some(AdmissionError::Capacity)
        );
        finish.send(()).unwrap();
        worker.await.unwrap();
        assert!(gate.acquire("other", &shutdown).await.is_ok());
    }
}
