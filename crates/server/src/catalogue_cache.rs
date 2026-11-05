//! Bounded shared catalogue payloads and cold-read coalescing. Authentication
//! remains outside this owner; personalized responses must never enter it.
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use parking_lot::Mutex;
use tokio::sync::Notify;

const DEFAULT_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_ENTRIES: usize = 512;
const MAX_FLIGHTS: usize = 128;
const MAX_WAITERS: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CacheError {
    Busy,
    Stale,
    Failed(String),
}

impl CacheError {
    pub fn response(self) -> Response {
        let (status, message) = match self {
            Self::Busy => (
                StatusCode::SERVICE_UNAVAILABLE,
                "catalogue capacity reached; retry shortly".to_owned(),
            ),
            Self::Stale => (
                StatusCode::CONFLICT,
                "catalogue revision changed; reload the requested page".to_owned(),
            ),
            Self::Failed(message) => (StatusCode::INTERNAL_SERVER_ERROR, message),
        };
        let mut response = crate::http::json_error_response(status, message);
        if status == StatusCode::SERVICE_UNAVAILABLE {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        }
        response
    }
}

#[derive(Default)]
struct State {
    entries: HashMap<String, Bytes>,
    order: VecDeque<String>,
    bytes: usize,
    flights: HashMap<String, Arc<Flight>>,
    closed: bool,
    hits: u64,
}

#[derive(Default)]
struct Flight {
    result: Mutex<Option<Result<Bytes, CacheError>>>,
    changed: Notify,
    waiters: AtomicUsize,
}

pub struct CatalogueCache {
    state: Mutex<State>,
    max_bytes: usize,
    max_entries: usize,
}

impl Default for CatalogueCache {
    fn default() -> Self {
        Self::new(DEFAULT_BYTES, DEFAULT_ENTRIES)
    }
}

impl CatalogueCache {
    pub fn new(max_bytes: usize, max_entries: usize) -> Self {
        Self {
            state: Mutex::new(State::default()),
            max_bytes,
            max_entries,
        }
    }

    pub fn retained(&self) -> (usize, usize) {
        let state = self.state.lock();
        (state.bytes, state.entries.len())
    }

    #[cfg(test)]
    pub(crate) fn hit_count(&self) -> u64 {
        self.state.lock().hits
    }

    /// Callers include structural and metadata revisions in `key`. The final
    /// freshness check fences fills that began before a committed update.
    pub async fn get_or_fill<F, C>(
        &self,
        key: String,
        current: C,
        fill: impl FnOnce() -> F,
    ) -> Result<Bytes, CacheError>
    where
        F: Future<Output = Result<Bytes, String>>,
        C: Fn() -> bool,
    {
        if !current() {
            return Err(CacheError::Stale);
        }
        let (flight, owner) = {
            let mut state = self.state.lock();
            if state.closed {
                return Err(CacheError::Busy);
            }
            if let Some(bytes) = state.entries.get(&key).cloned() {
                state.hits = state.hits.saturating_add(1);
                state.order.retain(|entry| entry != &key);
                state.order.push_back(key);
                return if current() {
                    Ok(bytes)
                } else {
                    Err(CacheError::Stale)
                };
            }
            if let Some(flight) = state.flights.get(&key).cloned() {
                if flight
                    .waiters
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                        (count < MAX_WAITERS).then_some(count + 1)
                    })
                    .is_err()
                {
                    return Err(CacheError::Busy);
                }
                (flight, false)
            } else {
                if state.flights.len() >= MAX_FLIGHTS || key.len() > 4096 {
                    return Err(CacheError::Busy);
                }
                let flight = Arc::new(Flight::default());
                state.flights.insert(key.clone(), flight.clone());
                (flight, true)
            }
        };
        if !owner {
            let _waiter = Waiter(&flight);
            loop {
                let changed = flight.changed.notified();
                if let Some(result) = flight.result.lock().clone() {
                    return if current() {
                        result
                    } else {
                        Err(CacheError::Stale)
                    };
                }
                changed.await;
            }
        }
        let mut owner = FillOwner {
            cache: self,
            key: Some(key),
            flight,
        };
        let result = fill().await.map_err(CacheError::Failed);
        let result = if current() {
            result
        } else {
            Err(CacheError::Stale)
        };
        owner.finish(result.clone());
        result
    }

    /// Stop retaining payloads and wake bounded coalesced readers. Running
    /// blocking work still owns its external admission/cancellation lifetime.
    pub fn close(&self) {
        let mut state = self.state.lock();
        state.closed = true;
        state.entries.clear();
        state.order.clear();
        state.bytes = 0;
        for flight in state.flights.values() {
            *flight.result.lock() = Some(Err(CacheError::Busy));
            flight.changed.notify_waiters();
        }
        state.flights.clear();
    }
}

fn charge(key: &str, value: &Bytes) -> usize {
    // Two bounded key copies (map and recency queue), payload and conservative
    // entry/hash bookkeeping. Active body clones are bounded by admission.
    key.len()
        .saturating_mul(2)
        .saturating_add(value.len())
        .saturating_add(192)
}

struct Waiter<'a>(&'a Flight);
impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        self.0.waiters.fetch_sub(1, Ordering::AcqRel);
    }
}

struct FillOwner<'a> {
    cache: &'a CatalogueCache,
    key: Option<String>,
    flight: Arc<Flight>,
}

impl FillOwner<'_> {
    fn finish(&mut self, result: Result<Bytes, CacheError>) {
        let Some(key) = self.key.take() else {
            return;
        };
        let mut state = self.cache.state.lock();
        if state
            .flights
            .get(&key)
            .is_some_and(|entry| Arc::ptr_eq(entry, &self.flight))
        {
            state.flights.remove(&key);
        }
        if !state.closed {
            if let Ok(value) = &result {
                let bytes = charge(&key, value);
                if bytes <= self.cache.max_bytes && self.cache.max_entries > 0 {
                    while state.entries.len() >= self.cache.max_entries
                        || state.bytes.saturating_add(bytes) > self.cache.max_bytes
                    {
                        let Some(oldest) = state.order.pop_front() else {
                            break;
                        };
                        if let Some(value) = state.entries.remove(&oldest) {
                            state.bytes = state.bytes.saturating_sub(charge(&oldest, &value));
                        }
                    }
                    state.bytes = state.bytes.saturating_add(bytes);
                    state.entries.insert(key.clone(), value.clone());
                    state.order.push_back(key);
                }
            }
        }
        *self.flight.result.lock() = Some(if state.closed {
            Err(CacheError::Busy)
        } else {
            result
        });
        self.flight.changed.notify_waiters();
    }
}

impl Drop for FillOwner<'_> {
    fn drop(&mut self) {
        self.finish(Err(CacheError::Failed("catalogue read cancelled".into())));
    }
}

pub fn json_response(bytes: Bytes) -> Response {
    let mut response = Response::new(axum::body::Body::from(bytes));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[tokio::test]
    async fn cache_respects_bytes_counts_and_oversized_bypass() {
        let cache = CatalogueCache::new(1024, 2);
        for id in 0..5 {
            cache
                .get_or_fill(
                    id.to_string(),
                    || true,
                    || async { Ok(Bytes::from_static(b"value")) },
                )
                .await
                .unwrap();
            assert!(cache.retained().0 <= 1024);
            assert!(cache.retained().1 <= 2);
        }
        cache
            .get_or_fill(
                "huge".into(),
                || true,
                || async { Ok(Bytes::from(vec![0; 2048])) },
            )
            .await
            .unwrap();
        assert_eq!(cache.retained().1, 2);
    }

    #[tokio::test]
    async fn stale_fills_never_populate_cache_and_failure_is_retryable() {
        let cache = CatalogueCache::default();
        let current = AtomicBool::new(true);
        let result = cache
            .get_or_fill(
                "artist".into(),
                || current.load(Ordering::Acquire),
                || async {
                    current.store(false, Ordering::Release);
                    Ok(Bytes::from_static(b"old"))
                },
            )
            .await;
        assert_eq!(result, Err(CacheError::Stale));
        assert_eq!(cache.retained(), (0, 0));
        assert!(cache
            .get_or_fill("artist".into(), || true, || async { Err("failure".into()) })
            .await
            .is_err());
        assert_eq!(
            cache
                .get_or_fill(
                    "artist".into(),
                    || true,
                    || async { Ok(Bytes::from_static(b"new")) }
                )
                .await
                .unwrap(),
            "new"
        );
    }

    #[tokio::test]
    async fn concurrent_readers_share_a_fill_and_cancelled_owner_releases_key() {
        let cache = Arc::new(CatalogueCache::default());
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let owner = {
            let cache = cache.clone();
            let started = started.clone();
            let release = release.clone();
            tokio::spawn(async move {
                cache
                    .get_or_fill(
                        "same".into(),
                        || true,
                        || async {
                            started.notify_one();
                            release.notified().await;
                            Ok(Bytes::from_static(b"one"))
                        },
                    )
                    .await
            })
        };
        started.notified().await;
        let follower = {
            let cache = cache.clone();
            tokio::spawn(async move {
                cache
                    .get_or_fill(
                        "same".into(),
                        || true,
                        || async { panic!("duplicate fill") },
                    )
                    .await
            })
        };
        tokio::task::yield_now().await;
        release.notify_one();
        assert_eq!(owner.await.unwrap().unwrap(), "one");
        assert_eq!(follower.await.unwrap().unwrap(), "one");
        let pending = {
            let cache = cache.clone();
            tokio::spawn(async move {
                cache
                    .get_or_fill("cancel".into(), || true, || std::future::pending())
                    .await
            })
        };
        tokio::task::yield_now().await;
        pending.abort();
        let _ = pending.await;
        assert_eq!(
            cache
                .get_or_fill(
                    "cancel".into(),
                    || true,
                    || async { Ok(Bytes::from_static(b"retry")) }
                )
                .await
                .unwrap(),
            "retry"
        );
    }

    #[tokio::test]
    async fn same_key_flood_has_finite_waiters_and_controlled_overload() {
        let cache = Arc::new(CatalogueCache::default());
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let owner = {
            let cache = cache.clone();
            let started = started.clone();
            let release = release.clone();
            tokio::spawn(async move {
                cache
                    .get_or_fill(
                        "flood".into(),
                        || true,
                        || async {
                            started.notify_one();
                            release.notified().await;
                            Ok(Bytes::from_static(b"shared"))
                        },
                    )
                    .await
            })
        };
        started.notified().await;
        let mut followers = Vec::new();
        for _ in 0..100 {
            let cache = cache.clone();
            followers.push(tokio::spawn(async move {
                cache
                    .get_or_fill(
                        "flood".into(),
                        || true,
                        || async { panic!("duplicate fill") },
                    )
                    .await
            }));
        }
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            cache.state.lock().flights["flood"]
                .waiters
                .load(Ordering::Acquire),
            MAX_WAITERS
        );
        release.notify_one();
        owner.await.unwrap().unwrap();
        let mut overloaded = 0;
        for follower in followers {
            match follower.await.unwrap() {
                Ok(bytes) => assert_eq!(bytes, "shared"),
                Err(CacheError::Busy) => overloaded += 1,
                Err(error) => panic!("unexpected result: {error:?}"),
            }
        }
        assert_eq!(overloaded, 100 - MAX_WAITERS);
        assert!(cache.state.lock().flights.is_empty());
    }
}
