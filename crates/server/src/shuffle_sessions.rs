//! Account-owned playback decks with small, repeatable metadata windows.
//!
//! Only catalog candidate IDs are shared. A session owns its random permutation,
//! and personal collection candidates never enter the shared cache.
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use library::Library;
use parking_lot::Mutex;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use tokio::sync::{OnceCell, OwnedSemaphorePermit, Semaphore};

use crate::shuffle::{
    cached_album_genres, cached_artist_genres, credited_artist_ids, normalize_genres, ShuffleError,
    ShuffleMode,
};

const DEFAULT_PAGE_SIZE: usize = 10;
const MAX_PAGE_SIZE: usize = 50;
const SESSION_TTL: Duration = Duration::from_secs(2 * 60 * 60);
const MAX_SESSIONS: usize = 128;
const MAX_SESSION_IDS: usize = 1_000_000;
const MAX_TOTAL_DECK_IDS: usize = 2_000_000;
const MAX_CACHE_ENTRIES: usize = 8;
const MAX_CACHE_IDS: usize = 1_000_000;
const MAX_FILTERS: usize = 128;
const MAX_IDENTIFIER_BYTES: usize = 256;
const MAX_RULE_INDEXES: usize = 2;
const MAX_RULE_INDEX_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
pub struct ShuffleSessionRequest {
    pub mode: String,
    #[serde(default)]
    pub artist_id: Option<String>,
    #[serde(default)]
    pub album_id: Option<String>,
    #[serde(default)]
    pub artist_ids: Vec<String>,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub playlist_id: Option<String>,
    #[serde(default)]
    pub start_track_id: Option<String>,
    pub request_id: String,
    pub client_id: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ShufflePageIds {
    pub session_id: String,
    pub offset: usize,
    pub next_offset: usize,
    pub total: usize,
    pub has_more: bool,
    pub track_ids: Vec<String>,
}

/// An authenticated import into durable account resume storage. Never exposed
/// directly over HTTP; metadata pages remain bounded.
pub struct ShuffleDeckSnapshot {
    pub track_ids: Vec<String>,
    pub mode: String,
    pub source_id: Option<String>,
    pub artist_ids: Vec<String>,
    pub genres: Vec<String>,
}

#[derive(Clone, Debug)]
pub enum ShuffleSessionError {
    BadRequest(String),
    NotFound,
    Conflict(String),
    Busy,
    Library(String),
}

#[derive(Clone)]
pub struct ShuffleSessionStore {
    inner: Arc<Inner>,
    shutdown: crate::shutdown::Shutdown,
}

struct Inner {
    state: Mutex<StoreState>,
    work: Arc<Semaphore>,
    waiting: Arc<Semaphore>,
    catalog_work: Arc<Semaphore>,
    catalog_waiting: Arc<Semaphore>,
    #[cfg(test)]
    builds: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    custom_builds: std::sync::atomic::AtomicUsize,
}

#[derive(Default)]
struct StoreState {
    sessions: HashMap<String, Session>,
    requests: HashMap<RequestKey, RequestRecord>,
    candidates: HashMap<CandidateKey, CandidateEntry>,
    rules: HashMap<RuleKey, RuleEntry>,
    deck_ids: usize,
}

struct Session {
    user_id: String,
    device_id: String,
    request_key: RequestKey,
    candidates: Arc<Vec<String>>,
    order: Vec<u32>,
    last_access: Instant,
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct RequestKey {
    user_id: String,
    device_id: String,
    request_id: String,
}

struct RequestRecord {
    session_id: String,
    fingerprint: Fingerprint,
}

#[derive(Clone, PartialEq, Eq)]
struct Fingerprint {
    scope: Scope,
    start_track_id: Option<String>,
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct CandidateKey {
    generation: u64,
    revision: u64,
    scope: Scope,
}

#[derive(Clone, Hash, PartialEq, Eq)]
enum Scope {
    All,
    Artist(String),
    Album(String),
    Custom(Vec<String>, Vec<String>),
    Liked,
    Playlist(String),
}

struct CandidateEntry {
    value: Arc<OnceCell<Arc<Vec<String>>>>,
    last_access: Instant,
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct RuleKey {
    generation: u64,
    revision: u64,
}

struct RuleEntry {
    value: Arc<OnceCell<Arc<CustomRuleIndex>>>,
    last_access: Instant,
}

/// The rule index is public catalog metadata only. Artist and genre postings
/// describe exactly the OR predicates used by the legacy custom queue.
struct CustomRuleIndex {
    ids: Vec<String>,
    artists: HashMap<String, Vec<u32>>,
    genres: HashMap<String, Vec<u32>>,
}

impl Default for ShuffleSessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ShuffleSessionStore {
    /// Public-catalog rule evaluation reuses the bounded metadata index. Empty
    /// persisted custom rules never inherit the legacy "all" alias.
    pub async fn resume_custom_candidates(
        &self,
        library: Library,
        revision: u64,
        mut artists: Vec<String>,
        genres: Vec<String>,
    ) -> Result<Arc<Vec<String>>, ShuffleSessionError> {
        artists.sort_unstable();
        artists.dedup();
        let mut genres: Vec<String> = genres
            .into_iter()
            .map(|g| g.trim().to_ascii_lowercase())
            .filter(|g| !g.is_empty())
            .collect();
        genres.sort_unstable();
        genres.dedup();
        if artists.is_empty() && genres.is_empty() {
            return Ok(Arc::new(vec![]));
        }
        let scope = Scope::Custom(artists, genres);
        let key = CandidateKey {
            generation: library.catalog_generation(),
            revision,
            scope: scope.clone(),
        };
        let cell = {
            let mut state = self.inner.state.lock();
            state.expire(Instant::now());
            if !state.candidates.contains_key(&key) {
                state.make_cache_space();
                if state.candidates.len() >= MAX_CACHE_ENTRIES {
                    return Err(ShuffleSessionError::Busy);
                }
            }
            let entry = state
                .candidates
                .entry(key.clone())
                .or_insert_with(|| CandidateEntry {
                    value: Arc::new(OnceCell::new()),
                    last_access: Instant::now(),
                });
            entry.last_access = Instant::now();
            entry.value.clone()
        };
        // Resume pages repeatedly evaluate one rule. Keep its public candidate
        // IDs in the existing bounded cache, rather than selecting every page.
        let result = cell
            .get_or_try_init(|| self.catalog_candidates(library, revision, scope))
            .await;
        match result {
            Ok(ids) => {
                let ids = ids.clone();
                self.inner.state.lock().trim_cache_ids(&key);
                Ok(ids)
            }
            Err(error) => {
                let mut state = self.inner.state.lock();
                if state
                    .candidates
                    .get(&key)
                    .is_some_and(|entry| Arc::ptr_eq(&entry.value, &cell))
                {
                    state.candidates.remove(&key);
                }
                Err(error)
            }
        }
    }
    pub fn export_deck(
        &self,
        user: &str,
        client: &str,
        id: &str,
    ) -> Result<ShuffleDeckSnapshot, ShuffleSessionError> {
        let mut state = self.inner.state.lock();
        state.expire(Instant::now());
        let session = state
            .sessions
            .get(id)
            .ok_or(ShuffleSessionError::NotFound)?;
        if session.user_id != user || session.device_id != client {
            return Err(ShuffleSessionError::NotFound);
        }
        let record = state
            .requests
            .get(&session.request_key)
            .ok_or(ShuffleSessionError::NotFound)?;
        let (mode, source_id, artist_ids, genres) = match &record.fingerprint.scope {
            Scope::All => ("library", None, vec![], vec![]),
            Scope::Artist(id) => ("artist", Some(id.clone()), vec![], vec![]),
            Scope::Album(id) => ("album", Some(id.clone()), vec![], vec![]),
            Scope::Playlist(id) => ("playlist", Some(id.clone()), vec![], vec![]),
            Scope::Liked => ("liked", None, vec![], vec![]),
            Scope::Custom(artists, genres) => ("custom", None, artists.clone(), genres.clone()),
        };
        Ok(ShuffleDeckSnapshot {
            track_ids: session
                .order
                .iter()
                .map(|&i| session.candidates[i as usize].clone())
                .collect(),
            mode: mode.into(),
            source_id,
            artist_ids,
            genres,
        })
    }

    pub fn purge_user(&self, user: &str) {
        let mut state = self.inner.state.lock();
        let ids: Vec<_> = state
            .sessions
            .iter()
            .filter(|(_, session)| session.user_id == user)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            state.remove_session(&id);
        }
    }
    pub fn with_shutdown(mut self, shutdown: crate::shutdown::Shutdown) -> Self {
        self.shutdown = shutdown;
        self
    }

    fn running(&self) -> Result<(), ShuffleSessionError> {
        if self.shutdown.is_cancelled() {
            Err(ShuffleSessionError::Busy)
        } else {
            Ok(())
        }
    }

    fn spawn_read<R: Send + 'static>(
        &self,
        work: impl FnOnce() -> Result<R, ShuffleSessionError> + Send + 'static,
    ) -> tokio::task::JoinHandle<Result<R, ShuffleSessionError>> {
        let stop = self.shutdown.clone();
        self.shutdown
            .spawn_blocking_named("shuffle_preparation", move || {
                if stop.is_cancelled() {
                    return Err(ShuffleSessionError::Busy);
                }
                let value = work();
                if stop.is_cancelled() {
                    return Err(ShuffleSessionError::Busy);
                }
                value
            })
    }

    pub fn new() -> Self {
        Self {
            shutdown: Default::default(),
            inner: Arc::new(Inner {
                state: Mutex::new(StoreState::default()),
                work: Arc::new(Semaphore::new(2)),
                waiting: Arc::new(Semaphore::new(32)),
                catalog_work: Arc::new(Semaphore::new(1)),
                catalog_waiting: Arc::new(Semaphore::new(MAX_CACHE_ENTRIES)),
                #[cfg(test)]
                builds: std::sync::atomic::AtomicUsize::new(0),
                #[cfg(test)]
                custom_builds: std::sync::atomic::AtomicUsize::new(0),
            }),
        }
    }

    /// Shared admission for catalog builds and HTTP page hydration. The waiting
    /// queue is bounded as well as the actual blocking work.
    pub async fn acquire_work_permit(&self) -> Result<OwnedSemaphorePermit, ShuffleSessionError> {
        let _waiting = self
            .inner
            .waiting
            .clone()
            .try_acquire_owned()
            .map_err(|_| ShuffleSessionError::Busy)?;
        self.running()?;
        tokio::select! {
            biased;
            _ = self.shutdown.cancelled() => Err(ShuffleSessionError::Busy),
            permit = tokio::time::timeout(Duration::from_secs(15), self.inner.work.clone().acquire_owned()) =>
                permit.map_err(|_| ShuffleSessionError::Busy)?.map_err(|_| ShuffleSessionError::Busy),
        }
    }

    pub async fn create(
        &self,
        library: Library,
        revision: u64,
        user_id: String,
        device_id: String,
        request: ShuffleSessionRequest,
        personal_ids: Option<Vec<String>>,
    ) -> Result<ShufflePageIds, ShuffleSessionError> {
        request.validate()?;
        validate_short(&user_id, "user_id", 128)?;
        validate_short(&device_id, "client_id", 128)?;
        validate_short(&request.request_id, "request_id", 128)?;
        if request.client_id != device_id {
            return Err(bad_request("client_id does not match the playback client"));
        }
        let limit = page_size(request.limit.unwrap_or(DEFAULT_PAGE_SIZE))?;
        let scope = request.scope()?;
        let fingerprint = Fingerprint {
            scope: scope.clone(),
            start_track_id: request.start_track_id,
        };
        let request_key = RequestKey {
            user_id: user_id.clone(),
            device_id: device_id.clone(),
            request_id: request.request_id,
        };
        // Check retries before touching mutable collection data or the library.
        // A retry keeps the original snapshot even after a like or playlist edit.
        if let Some(page) = self.retry(&request_key, &fingerprint, limit)? {
            return Ok(page);
        }

        let candidates = match &scope {
            Scope::Liked | Scope::Playlist(_) => {
                let ids =
                    personal_ids.ok_or_else(|| bad_request("personal collection required"))?;
                if ids.len() > MAX_SESSION_IDS {
                    return Err(ShuffleSessionError::Busy);
                }
                for id in &ids {
                    validate_short(id, "track_id", MAX_IDENTIFIER_BYTES)?;
                }
                let catalog = self
                    .catalog_candidates(library, revision, Scope::All)
                    .await?;
                let permit = self.acquire_work_permit().await?;
                Arc::new(
                    self.spawn_read(move || {
                        let _permit = permit;
                        let mut ids = ids;
                        ids.sort_unstable();
                        ids.dedup();
                        ids.retain(|id| catalog.binary_search(id).is_ok());
                        Ok(ids)
                    })
                    .await
                    .map_err(|err| ShuffleSessionError::Library(err.to_string()))??,
                )
            }
            _ => self.catalog_candidates(library, revision, scope).await?,
        };
        if candidates.is_empty() {
            return Err(ShuffleSessionError::NotFound);
        }
        let start_index = fingerprint
            .start_track_id
            .as_ref()
            .and_then(|id| candidates.binary_search(id).ok());
        let total = candidates.len();
        let permit = self.acquire_work_permit().await?;
        let order = self
            .spawn_read(move || {
                let _permit = permit;
                let mut order: Vec<u32> = (0..total as u32).collect();
                order.shuffle(&mut rand::rng());
                if let Some(start) = start_index {
                    let position = order
                        .iter()
                        .position(|&index| index as usize == start)
                        .unwrap();
                    order.swap(0, position);
                }
                Ok(order)
            })
            .await
            .map_err(|err| ShuffleSessionError::Library(err.to_string()))??;

        self.running()?;
        let now = Instant::now();
        let mut state = self.inner.state.lock();
        state.expire(now);
        // Concurrent identical creates may have both awaited the same cache
        // build. Publish one deck, then serve every caller that exact deck.
        if let Some(record) = state.requests.get(&request_key) {
            if record.fingerprint != fingerprint {
                return Err(ShuffleSessionError::Conflict(
                    "request_id already identifies a different shuffle".into(),
                ));
            }
            let id = record.session_id.clone();
            let session = state.sessions.get_mut(&id).unwrap();
            return session.page(&id, 0, limit, now);
        }
        if state.sessions.len() >= MAX_SESSIONS
            || state.deck_ids.saturating_add(total) > MAX_TOTAL_DECK_IDS
        {
            return Err(ShuffleSessionError::Busy);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let mut session = Session {
            user_id,
            device_id,
            request_key: request_key.clone(),
            candidates,
            order,
            last_access: now,
        };
        let page = session.page(&id, 0, limit, now)?;
        state.deck_ids += total;
        state.sessions.insert(id.clone(), session);
        state.requests.insert(
            request_key,
            RequestRecord {
                session_id: id,
                fingerprint,
            },
        );
        Ok(page)
    }

    pub async fn page(
        &self,
        user_id: String,
        device_id: String,
        session_id: String,
        offset: usize,
        limit: usize,
    ) -> Result<ShufflePageIds, ShuffleSessionError> {
        let limit = page_size(limit)?;
        let now = Instant::now();
        let mut state = self.inner.state.lock();
        state.expire(now);
        let session = state
            .sessions
            .get_mut(&session_id)
            .ok_or(ShuffleSessionError::NotFound)?;
        if session.user_id != user_id || session.device_id != device_id {
            return Err(ShuffleSessionError::NotFound);
        }
        session.page(&session_id, offset, limit, now)
    }

    /// Fast idempotency check before resolving a playlist that may since have
    /// been edited or deleted. This reads only this owner's bounded deck state.
    pub fn existing(
        &self,
        user_id: &str,
        device_id: &str,
        request: &ShuffleSessionRequest,
    ) -> Result<Option<ShufflePageIds>, ShuffleSessionError> {
        request.validate()?;
        validate_short(user_id, "user_id", 128)?;
        validate_short(device_id, "client_id", 128)?;
        if request.client_id != device_id {
            return Err(bad_request("client_id does not match the playback client"));
        }
        self.retry(
            &RequestKey {
                user_id: user_id.into(),
                device_id: device_id.into(),
                request_id: request.request_id.clone(),
            },
            &Fingerprint {
                scope: request.scope()?,
                start_track_id: request.start_track_id.clone(),
            },
            page_size(request.limit.unwrap_or(DEFAULT_PAGE_SIZE))?,
        )
    }

    /// Missing sessions are already released. A different owner/client cannot
    /// release an existing deck, nor learn anything about it from its contents.
    pub async fn remove(
        &self,
        user_id: String,
        device_id: String,
        session_id: String,
    ) -> Result<(), ShuffleSessionError> {
        let mut state = self.inner.state.lock();
        state.expire(Instant::now());
        if let Some(session) = state.sessions.get(&session_id) {
            if session.user_id != user_id || session.device_id != device_id {
                return Err(ShuffleSessionError::NotFound);
            }
        }
        state.remove_session(&session_id);
        Ok(())
    }

    fn retry(
        &self,
        key: &RequestKey,
        fingerprint: &Fingerprint,
        limit: usize,
    ) -> Result<Option<ShufflePageIds>, ShuffleSessionError> {
        let now = Instant::now();
        let mut state = self.inner.state.lock();
        state.expire(now);
        let Some(record) = state.requests.get(key) else {
            return Ok(None);
        };
        if &record.fingerprint != fingerprint {
            return Err(ShuffleSessionError::Conflict(
                "request_id already identifies a different shuffle".into(),
            ));
        }
        let id = record.session_id.clone();
        state
            .sessions
            .get_mut(&id)
            .unwrap()
            .page(&id, 0, limit, now)
            .map(Some)
    }

    async fn catalog_candidates(
        &self,
        library: Library,
        revision: u64,
        scope: Scope,
    ) -> Result<Arc<Vec<String>>, ShuffleSessionError> {
        if let Scope::Custom(artists, genres) = scope {
            let index = self.custom_rules(library, revision).await?;
            let permit = self.acquire_work_permit().await?;
            return self
                .spawn_read(move || {
                    let _permit = permit;
                    Ok(Arc::new(index.select(&artists, &genres)))
                })
                .await
                .map_err(|err| ShuffleSessionError::Library(err.to_string()))?;
        }
        let key = CandidateKey {
            generation: library.catalog_generation(),
            // Metadata enrichment cannot change an all-library ID set. Scans
            // advance the shared catalog generation when that set can change.
            revision: if matches!(scope, Scope::All) {
                0
            } else {
                revision
            },
            scope: scope.clone(),
        };
        let cell = {
            let mut state = self.inner.state.lock();
            state.expire(Instant::now());
            if !state.candidates.contains_key(&key) {
                state.make_cache_space();
                if state.candidates.len() >= MAX_CACHE_ENTRIES {
                    return Err(ShuffleSessionError::Busy);
                }
            }
            let entry = state
                .candidates
                .entry(key.clone())
                .or_insert_with(|| CandidateEntry {
                    value: Arc::new(OnceCell::new()),
                    last_access: Instant::now(),
                });
            entry.last_access = Instant::now();
            entry.value.clone()
        };
        let result = cell
            .get_or_try_init(|| async {
                // At most one cold catalog rule scans metadata. The other work
                // slot remains available to hydrate active playback windows.
                let waiting = self
                    .inner
                    .catalog_waiting
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| ShuffleSessionError::Busy)?;
                let work = self.inner.catalog_work.clone();
                let catalog_permit = tokio::select! {
                    biased;
                    _ = self.shutdown.cancelled() => return Err(ShuffleSessionError::Busy),
                    permit = work.acquire_owned() => permit.map_err(|_| ShuffleSessionError::Busy)?,
                };
                drop(waiting);
                let permit = self.acquire_work_permit().await?;
                let ids = self
                    .spawn_read(move || {
                        let _permit = permit;
                        let _catalog_permit = catalog_permit;
                        candidate_ids(&library, &scope)
                    })
                    .await
                    .map_err(|err| ShuffleSessionError::Library(err.to_string()))??;
                #[cfg(test)]
                self.inner
                    .builds
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(Arc::new(ids))
            })
            .await;
        let ids = match result {
            Ok(ids) => ids.clone(),
            Err(error) => {
                let mut state = self.inner.state.lock();
                if state
                    .candidates
                    .get(&key)
                    .is_some_and(|entry| Arc::ptr_eq(&entry.value, &cell))
                {
                    state.candidates.remove(&key);
                }
                return Err(error);
            }
        };
        self.running()?;
        self.inner.state.lock().trim_cache_ids(&key);
        Ok(ids)
    }

    async fn custom_rules(
        &self,
        library: Library,
        revision: u64,
    ) -> Result<Arc<CustomRuleIndex>, ShuffleSessionError> {
        let key = RuleKey {
            generation: library.catalog_generation(),
            revision,
        };
        let cell = {
            let mut state = self.inner.state.lock();
            state.expire(Instant::now());
            if !state.rules.contains_key(&key) && state.rules.len() >= MAX_RULE_INDEXES {
                let oldest = state
                    .rules
                    .iter()
                    .filter(|(_, entry)| entry.value.get().is_some())
                    .min_by_key(|(_, entry)| entry.last_access)
                    .map(|(key, _)| key.clone());
                if let Some(oldest) = oldest {
                    state.rules.remove(&oldest);
                } else {
                    return Err(ShuffleSessionError::Busy);
                }
            }
            let entry = state.rules.entry(key.clone()).or_insert_with(|| RuleEntry {
                value: Arc::new(OnceCell::new()),
                last_access: Instant::now(),
            });
            entry.last_access = Instant::now();
            entry.value.clone()
        };
        let result = cell
            .get_or_try_init(|| async {
                let waiting = self
                    .inner
                    .catalog_waiting
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| ShuffleSessionError::Busy)?;
                let work = self.inner.catalog_work.clone();
                let catalog_permit = tokio::select! {
                    biased;
                    _ = self.shutdown.cancelled() => return Err(ShuffleSessionError::Busy),
                    permit = work.acquire_owned() => permit.map_err(|_| ShuffleSessionError::Busy)?,
                };
                drop(waiting);
                let permit = self.acquire_work_permit().await?;
                let index = self
                    .spawn_read(move || {
                        let _permit = permit;
                        let _catalog_permit = catalog_permit;
                        CustomRuleIndex::build(&library)
                    })
                    .await
                    .map_err(|err| ShuffleSessionError::Library(err.to_string()))??;
                #[cfg(test)]
                self.inner
                    .custom_builds
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(Arc::new(index))
            })
            .await;
        match result {
            Ok(index) => Ok(index.clone()),
            Err(error) => {
                let mut state = self.inner.state.lock();
                if state
                    .rules
                    .get(&key)
                    .is_some_and(|entry| Arc::ptr_eq(&entry.value, &cell))
                {
                    state.rules.remove(&key);
                }
                Err(error)
            }
        }
    }
}

impl ShuffleSessionRequest {
    pub fn validate(&self) -> Result<(), ShuffleSessionError> {
        validate_short(&self.request_id, "request_id", 128)?;
        validate_short(&self.client_id, "client_id", 128)?;
        page_size(self.limit.unwrap_or(DEFAULT_PAGE_SIZE))?;
        self.scope()?;
        Ok(())
    }

    fn scope(&self) -> Result<Scope, ShuffleSessionError> {
        if self.artist_ids.len() > MAX_FILTERS || self.genres.len() > MAX_FILTERS {
            return Err(bad_request("too many shuffle filters"));
        }
        for id in &self.artist_ids {
            validate_short(id, "shuffle filter", MAX_IDENTIFIER_BYTES)?;
        }
        if self
            .genres
            .iter()
            .any(|value| value.len() > MAX_IDENTIFIER_BYTES)
        {
            return Err(bad_request("invalid genre filter"));
        }
        for (label, value) in [
            ("artist_id", &self.artist_id),
            ("album_id", &self.album_id),
            ("playlist_id", &self.playlist_id),
            ("start_track_id", &self.start_track_id),
        ] {
            if let Some(value) = value {
                validate_short(value, label, MAX_IDENTIFIER_BYTES)?;
            }
        }
        if self.mode.len() > 32 {
            return Err(bad_request("invalid shuffle mode"));
        }
        if self.mode.trim().eq_ignore_ascii_case("playlist") {
            return Ok(Scope::Playlist(
                self.playlist_id
                    .clone()
                    .ok_or_else(|| bad_request("playlist_id required"))?,
            ));
        }
        match ShuffleMode::parse(&self.mode).ok_or_else(|| bad_request("invalid shuffle mode"))? {
            ShuffleMode::All => Ok(Scope::All),
            ShuffleMode::Artist => Ok(Scope::Artist(
                self.artist_id
                    .clone()
                    .ok_or_else(|| bad_request("artist_id required"))?,
            )),
            ShuffleMode::Album => Ok(Scope::Album(
                self.album_id
                    .clone()
                    .ok_or_else(|| bad_request("album_id required"))?,
            )),
            ShuffleMode::Liked => Ok(Scope::Liked),
            ShuffleMode::Custom => {
                let mut artists = self.artist_ids.clone();
                artists.sort_unstable();
                artists.dedup();
                let mut genres = self
                    .genres
                    .iter()
                    .map(|value| value.trim().to_ascii_lowercase())
                    .filter(|value| !value.is_empty())
                    .collect::<Vec<_>>();
                genres.sort_unstable();
                genres.dedup();
                if artists.is_empty() && genres.is_empty() {
                    Ok(Scope::All)
                } else {
                    Ok(Scope::Custom(artists, genres))
                }
            }
        }
    }
}

fn candidate_ids(library: &Library, scope: &Scope) -> Result<Vec<String>, ShuffleSessionError> {
    let mut ids = match scope {
        Scope::All => library
            .track_ids()
            .map_err(|err| ShuffleSessionError::Library(err.to_string()))?,
        Scope::Artist(id) => crate::shuffle::artist_tracks(library, id)
            .map_err(|err| ShuffleSessionError::Library(err.to_string()))?
            .into_iter()
            .map(|track| track.id)
            .collect(),
        Scope::Album(id) => library
            .get_album_tracks(id)
            .map_err(|err| ShuffleSessionError::Library(err.to_string()))?
            .into_iter()
            .map(|track| track.id)
            .collect(),
        Scope::Custom(_, _) => {
            return Err(bad_request(
                "custom candidates require the shared rule index",
            ))
        }
        Scope::Liked | Scope::Playlist(_) => {
            return Err(bad_request("personal candidates cannot be cached"))
        }
    };
    if ids.len() > MAX_SESSION_IDS {
        return Err(ShuffleSessionError::Busy);
    }
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

impl CustomRuleIndex {
    fn build(library: &Library) -> Result<Self, ShuffleSessionError> {
        let generation = library.catalog_generation();
        let mut index = Self {
            ids: Vec::new(),
            artists: HashMap::new(),
            genres: HashMap::new(),
        };
        let mut album_artists = HashMap::new();
        let mut album_genres = HashMap::new();
        let mut artist_genres = HashMap::new();
        let mut bytes = 0usize;
        let mut cursor = None;
        loop {
            if library.is_cancelled() {
                return Err(ShuffleSessionError::Busy);
            }
            let (tracks, total) = library
                .page_tracks(&mut cursor, 256)
                .map_err(|err| ShuffleSessionError::Library(err.to_string()))?;
            if total > MAX_SESSION_IDS
                || index.ids.len().saturating_add(tracks.len()) > MAX_SESSION_IDS
            {
                return Err(ShuffleSessionError::Busy);
            }
            let complete = tracks.len() < 256;
            for track in tracks {
                let position = index.ids.len() as u32;
                bytes = bytes.saturating_add(track.id.len() + std::mem::size_of::<String>());
                if bytes > MAX_RULE_INDEX_BYTES {
                    return Err(ShuffleSessionError::Busy);
                }
                let artists =
                    credited_artist_ids(library, &track, &mut album_artists).map_err(rule_error)?;
                let mut genres: HashSet<String> =
                    normalize_genres(&track.genres).into_iter().collect();
                genres.extend(
                    cached_album_genres(library, &track.album_id, &mut album_genres)
                        .map_err(rule_error)?,
                );
                for artist in artists {
                    genres.extend(
                        cached_artist_genres(library, &artist, &mut artist_genres)
                            .map_err(rule_error)?,
                    );
                    push_posting(&mut index.artists, artist, position, &mut bytes)?;
                }
                for genre in genres {
                    push_posting(&mut index.genres, genre, position, &mut bytes)?;
                }
                index.ids.push(track.id);
            }
            if complete {
                break;
            }
        }
        if library.catalog_generation() != generation {
            return Err(ShuffleSessionError::Busy);
        }
        Ok(index)
    }

    fn select(&self, artists: &[String], genres: &[String]) -> Vec<String> {
        let mut selected = vec![false; self.ids.len()];
        for posting in artists
            .iter()
            .filter_map(|id| self.artists.get(id))
            .chain(genres.iter().filter_map(|genre| self.genres.get(genre)))
        {
            for &position in posting {
                selected[position as usize] = true;
            }
        }
        let mut ids = self
            .ids
            .iter()
            .zip(selected)
            .filter_map(|(id, matched)| matched.then(|| id.clone()))
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        ids
    }
}

fn push_posting(
    map: &mut HashMap<String, Vec<u32>>,
    key: String,
    position: u32,
    bytes: &mut usize,
) -> Result<(), ShuffleSessionError> {
    if !map.contains_key(&key) {
        // Include a conservative allowance for hash buckets, vector headers,
        // and allocation slack. Per-edge accounting allows doubled Vec capacity.
        *bytes = bytes.saturating_add(key.len() + 128);
    }
    *bytes = bytes.saturating_add(2 * std::mem::size_of::<u32>());
    if *bytes > MAX_RULE_INDEX_BYTES {
        return Err(ShuffleSessionError::Busy);
    }
    map.entry(key).or_default().push(position);
    Ok(())
}

fn rule_error(error: ShuffleError) -> ShuffleSessionError {
    match error {
        ShuffleError::Library(error) => ShuffleSessionError::Library(error.to_string()),
        _ => bad_request("invalid custom shuffle collection"),
    }
}

impl Session {
    fn page(
        &mut self,
        id: &str,
        offset: usize,
        limit: usize,
        now: Instant,
    ) -> Result<ShufflePageIds, ShuffleSessionError> {
        if offset > self.order.len() {
            return Err(bad_request("offset exceeds the shuffle deck"));
        }
        self.last_access = now;
        let next = offset.saturating_add(limit).min(self.order.len());
        Ok(ShufflePageIds {
            session_id: id.to_owned(),
            offset,
            next_offset: next,
            total: self.order.len(),
            has_more: next < self.order.len(),
            track_ids: self.order[offset..next]
                .iter()
                .map(|&index| self.candidates[index as usize].clone())
                .collect(),
        })
    }
}

impl StoreState {
    fn remove_session(&mut self, id: &str) {
        if let Some(session) = self.sessions.remove(id) {
            self.deck_ids -= session.order.len();
            self.requests.remove(&session.request_key);
        }
    }

    fn expire(&mut self, now: Instant) {
        let expired: Vec<_> = self
            .sessions
            .iter()
            .filter(|(_, session)| {
                now.saturating_duration_since(session.last_access) >= SESSION_TTL
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            self.remove_session(&id);
        }
        self.candidates
            .retain(|_, entry| now.saturating_duration_since(entry.last_access) < SESSION_TTL);
        self.rules
            .retain(|_, entry| now.saturating_duration_since(entry.last_access) < SESSION_TTL);
    }

    fn oldest_completed(&self, keep: Option<&CandidateKey>) -> Option<CandidateKey> {
        self.candidates
            .iter()
            .filter(|(key, entry)| Some(*key) != keep && entry.value.get().is_some())
            .min_by_key(|(_, entry)| entry.last_access)
            .map(|(key, _)| key.clone())
    }

    fn make_cache_space(&mut self) {
        if self.candidates.len() >= MAX_CACHE_ENTRIES {
            if let Some(key) = self.oldest_completed(None) {
                self.candidates.remove(&key);
            }
        }
    }

    fn trim_cache_ids(&mut self, keep: &CandidateKey) {
        while self
            .candidates
            .values()
            .filter_map(|entry| entry.value.get())
            .map(|ids| ids.len())
            .sum::<usize>()
            > MAX_CACHE_IDS
        {
            let Some(key) = self.oldest_completed(Some(keep)) else {
                break;
            };
            self.candidates.remove(&key);
        }
    }
}

fn page_size(limit: usize) -> Result<usize, ShuffleSessionError> {
    if limit == 0 || limit > MAX_PAGE_SIZE {
        Err(bad_request("limit must be between 1 and 50"))
    } else {
        Ok(limit)
    }
}

fn validate_short(value: &str, label: &str, max: usize) -> Result<(), ShuffleSessionError> {
    if value.trim().is_empty() || value.len() > max {
        Err(bad_request(&format!("invalid {label}")))
    } else {
        Ok(())
    }
}

fn bad_request(message: &str) -> ShuffleSessionError {
    ShuffleSessionError::BadRequest(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use redb::{Database, TableDefinition};
    use std::sync::atomic::Ordering;

    const TRACKS: TableDefinition<&str, &[u8]> = TableDefinition::new("tracks");

    // Invalid metadata bytes deliberately prove Shuffle All only reads IDs.
    // No media files, applications, listeners, or background services are used.
    fn catalog(count: usize) -> (Library, Arc<Database>) {
        let db = Arc::new(
            Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        );
        let library = Library::load_or_scan_with_db(Vec::new(), db.clone())
            .unwrap()
            .0;
        let write = db.begin_write().unwrap();
        {
            let mut tracks = write.open_table(TRACKS).unwrap();
            for index in 0..count {
                tracks
                    .insert(format!("track-{index:06}").as_str(), &[255u8][..])
                    .unwrap();
            }
        }
        write.commit().unwrap();
        (library, db)
    }

    fn request(mode: &str, id: &str) -> ShuffleSessionRequest {
        ShuffleSessionRequest {
            mode: mode.into(),
            artist_id: None,
            album_id: None,
            artist_ids: Vec::new(),
            genres: Vec::new(),
            playlist_id: None,
            start_track_id: None,
            request_id: id.into(),
            client_id: "client".into(),
            limit: None,
        }
    }

    async fn create(
        store: &ShuffleSessionStore,
        library: &Library,
        user: &str,
        request: ShuffleSessionRequest,
        ids: Option<Vec<String>>,
    ) -> Result<ShufflePageIds, ShuffleSessionError> {
        store
            .create(
                library.clone(),
                0,
                user.into(),
                "client".into(),
                request,
                ids,
            )
            .await
    }

    #[tokio::test]
    async fn complete_36k_decks_share_ids_and_keep_pages_history_and_owners_separate() {
        let (library, _db) = catalog(36_000);
        let store = ShuffleSessionStore::new();
        let started = Instant::now();
        let (alice, bob) = tokio::join!(
            create(&store, &library, "alice", request("all", "first"), None),
            create(&store, &library, "bob", request("all", "first"), None),
        );
        let alice = alice.unwrap();
        let bob = bob.unwrap();
        println!(
            "36k ID-only cold candidate build and two session first pages: {:.3}ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
        assert_eq!(alice.track_ids.len(), 10);
        assert_eq!(alice.total, 36_000);
        assert_eq!(bob.total, 36_000);
        assert_ne!(alice.session_id, bob.session_id);
        assert_eq!(store.inner.builds.load(Ordering::Relaxed), 1);
        {
            let state = store.inner.state.lock();
            let a = &state.sessions[&alice.session_id];
            let b = &state.sessions[&bob.session_id];
            assert!(Arc::ptr_eq(&a.candidates, &b.candidates));
            assert_eq!(std::mem::size_of_val(&a.order[0]), 4);
        }
        let retry = create(&store, &library, "alice", request("all", "first"), None)
            .await
            .unwrap();
        assert_eq!(retry, alice);
        let mut seen = HashSet::new();
        let mut offset = 0;
        loop {
            let page = store
                .page(
                    "alice".into(),
                    "client".into(),
                    alice.session_id.clone(),
                    offset,
                    50,
                )
                .await
                .unwrap();
            assert_eq!(page.offset, offset);
            for id in page.track_ids {
                assert!(seen.insert(id), "a shuffle deck repeated a track");
            }
            offset = page.next_offset;
            if !page.has_more {
                break;
            }
        }
        assert_eq!(seen.len(), 36_000);
        assert_eq!(offset, 36_000);
        assert_eq!(
            store
                .page(
                    "alice".into(),
                    "client".into(),
                    alice.session_id.clone(),
                    0,
                    10
                )
                .await
                .unwrap(),
            alice
        );
        assert!(matches!(
            store
                .page(
                    "bob".into(),
                    "client".into(),
                    alice.session_id.clone(),
                    0,
                    10
                )
                .await,
            Err(ShuffleSessionError::NotFound)
        ));
        assert!(matches!(
            store
                .page(
                    "alice".into(),
                    "another-client".into(),
                    alice.session_id,
                    0,
                    10
                )
                .await,
            Err(ShuffleSessionError::NotFound)
        ));
    }

    #[tokio::test]
    async fn concurrent_retries_publish_one_session_and_cache_build() {
        let (library, _db) = catalog(100);
        let store = ShuffleSessionStore::new();
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let store = store.clone();
            let library = library.clone();
            tasks.push(tokio::spawn(async move {
                create(&store, &library, "alice", request("all", "retry"), None)
                    .await
                    .unwrap()
            }));
        }
        let mut pages = Vec::new();
        for task in tasks {
            pages.push(task.await.unwrap());
        }
        assert!(pages.iter().all(|page| page == &pages[0]));
        assert_eq!(store.inner.state.lock().sessions.len(), 1);
        assert_eq!(store.inner.builds.load(Ordering::Relaxed), 1);
        let mut changed = request("album", "retry");
        changed.album_id = Some("different-scope".into());
        assert!(matches!(
            create(&store, &library, "alice", changed, None).await,
            Err(ShuffleSessionError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn private_candidates_start_track_and_snapshot_retries_are_account_owned() {
        let (library, _db) = catalog(20);
        let store = ShuffleSessionStore::new();
        let mut liked = request("liked", "personal");
        liked.start_track_id = Some("track-000001".into());
        let first = create(
            &store,
            &library,
            "alice",
            liked.clone(),
            Some(vec![
                "track-000001".into(),
                "track-000002".into(),
                "track-000002".into(),
                "removed-track".into(),
            ]),
        )
        .await
        .unwrap();
        assert_eq!(first.track_ids, ["track-000001", "track-000002"]);
        let retry = create(
            &store,
            &library,
            "alice",
            liked,
            Some(vec!["track-000019".into()]),
        )
        .await
        .unwrap();
        assert_eq!(retry, first);
        let bob = create(
            &store,
            &library,
            "bob",
            request("liked", "personal"),
            Some(vec!["track-000019".into()]),
        )
        .await
        .unwrap();
        assert_eq!(bob.track_ids, ["track-000019"]);
        assert_eq!(
            store.inner.state.lock().candidates.len(),
            1,
            "only the shared catalog ID list is cached"
        );
        let mut outside = request("all", "outside");
        outside.start_track_id = Some("missing-track".into());
        assert_eq!(
            create(&store, &library, "alice", outside, None)
                .await
                .unwrap()
                .total,
            20
        );
        let mut playlist = request("playlist", "playlist");
        playlist.playlist_id = Some("my-playlist".into());
        let page = create(
            &store,
            &library,
            "alice",
            playlist,
            Some(vec!["track-000003".into()]),
        )
        .await
        .unwrap();
        assert_eq!(page.track_ids, ["track-000003"]);
    }

    #[tokio::test]
    async fn active_reads_refresh_ttl_and_release_is_idempotent_and_owned() {
        let (library, _db) = catalog(10);
        let store = ShuffleSessionStore::new();
        let page = create(&store, &library, "alice", request("all", "ttl"), None)
            .await
            .unwrap();
        let earlier = Instant::now() - SESSION_TTL + Duration::from_secs(60);
        store
            .inner
            .state
            .lock()
            .sessions
            .get_mut(&page.session_id)
            .unwrap()
            .last_access = earlier;
        store
            .page(
                "alice".into(),
                "client".into(),
                page.session_id.clone(),
                0,
                10,
            )
            .await
            .unwrap();
        assert!(store.inner.state.lock().sessions[&page.session_id].last_access > earlier);
        assert!(matches!(
            store
                .remove("bob".into(), "client".into(), page.session_id.clone())
                .await,
            Err(ShuffleSessionError::NotFound)
        ));
        assert!(store
            .inner
            .state
            .lock()
            .sessions
            .contains_key(&page.session_id));
        store
            .inner
            .state
            .lock()
            .sessions
            .get_mut(&page.session_id)
            .unwrap()
            .last_access = Instant::now() - SESSION_TTL;
        assert!(matches!(
            store
                .page(
                    "alice".into(),
                    "client".into(),
                    page.session_id.clone(),
                    0,
                    10
                )
                .await,
            Err(ShuffleSessionError::NotFound)
        ));
        assert!(store.inner.state.lock().requests.is_empty());
        assert_eq!(store.inner.state.lock().deck_ids, 0);
        let replacement = create(&store, &library, "alice", request("all", "ttl"), None)
            .await
            .unwrap();
        assert_ne!(replacement.session_id, page.session_id);
        store
            .remove(
                "alice".into(),
                "client".into(),
                replacement.session_id.clone(),
            )
            .await
            .unwrap();
        store
            .remove("alice".into(), "client".into(), replacement.session_id)
            .await
            .unwrap();
        assert!(store.inner.state.lock().sessions.is_empty());
    }

    #[tokio::test]
    async fn cache_revision_generation_and_memory_bounds_preserve_existing_decks() {
        let (library, _db) = catalog(12);
        let store = ShuffleSessionStore::new();
        let old = create(&store, &library, "alice", request("all", "old"), None)
            .await
            .unwrap();
        let new = store
            .create(
                library.clone(),
                1,
                "alice".into(),
                "client".into(),
                request("all", "new"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(old.total, 12);
        assert_eq!(new.total, 12);
        assert_eq!(
            store.inner.builds.load(Ordering::Relaxed),
            1,
            "enrichment revisions reuse all-library IDs"
        );
        assert_eq!(
            store
                .page("alice".into(), "client".into(), old.session_id, 0, 10)
                .await
                .unwrap()
                .total,
            12
        );
        let (other, _db) = catalog(3);
        assert_eq!(
            library.catalog_generation(),
            library.clone().catalog_generation()
        );
        assert_ne!(library.catalog_generation(), other.catalog_generation());
        assert_eq!(
            create(&store, &other, "alice", request("all", "other"), None)
                .await
                .unwrap()
                .total,
            3
        );
        for revision in 2..15 {
            store
                .catalog_candidates(
                    library.clone(),
                    revision,
                    Scope::Album(format!("missing-{revision}")),
                )
                .await
                .unwrap();
        }
        assert!(store.inner.state.lock().candidates.len() <= MAX_CACHE_ENTRIES);
        store.inner.state.lock().deck_ids = MAX_TOTAL_DECK_IDS;
        assert!(matches!(
            create(
                &store,
                &library,
                "alice",
                request("all", "over-budget"),
                None
            )
            .await,
            Err(ShuffleSessionError::Busy)
        ));
        // The admission budget does not remove another account's active deck.
        assert!(!store.inner.state.lock().sessions.is_empty());
    }

    #[tokio::test]
    async fn scans_invalidate_new_decks_across_library_clones_but_preserve_old_history() {
        let path = std::env::temp_dir().join(format!(
            "phonolite-shuffle-generation-{}",
            uuid::Uuid::new_v4()
        ));
        let music = path.join("music");
        let album = music.join("Artist/Album");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("First.mp3"), b"catalog test fixture").unwrap();
        let library = Library::load_or_scan(
            vec![library::LibraryRoot::new(String::new(), music)],
            path.join("library.redb"),
        )
        .unwrap()
        .0;
        let clone = library.clone();
        let before = clone.catalog_generation();
        let store = ShuffleSessionStore::new();
        let old = create(&store, &clone, "alice", request("all", "before-scan"), None)
            .await
            .unwrap();
        std::fs::remove_file(album.join("First.mp3")).unwrap();
        std::fs::write(album.join("Second.mp3"), b"different catalog test fixture").unwrap();
        library.incremental_scan().unwrap();
        assert_ne!(clone.catalog_generation(), before);
        assert_eq!(clone.catalog_generation(), library.catalog_generation());
        let new = create(&store, &clone, "alice", request("all", "after-scan"), None)
            .await
            .unwrap();
        assert_eq!(new.total, 1);
        assert_ne!(old.track_ids, new.track_ids);
        assert_eq!(
            store
                .page("alice".into(), "client".into(), old.session_id, 0, 10)
                .await
                .unwrap()
                .track_ids,
            old.track_ids
        );
        drop(store);
        drop(clone);
        drop(library);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[tokio::test]
    async fn page_limits_offsets_and_input_sizes_are_checked() {
        let (library, _db) = catalog(3);
        let store = ShuffleSessionStore::new();
        for limit in [0, 51, usize::MAX] {
            let mut invalid = request("all", "invalid");
            invalid.limit = Some(limit);
            assert!(matches!(
                create(&store, &library, "alice", invalid, None).await,
                Err(ShuffleSessionError::BadRequest(_))
            ));
        }
        let mut invalid = request("custom", "filters");
        invalid.artist_ids = vec!["artist".into(); MAX_FILTERS + 1];
        assert!(invalid.validate().is_err());
        invalid = request("all", "x");
        invalid.request_id = "x".repeat(129);
        assert!(invalid.validate().is_err());
        invalid = request("all", "x");
        invalid.client_id = "x".repeat(129);
        assert!(invalid.validate().is_err());
        let page = create(&store, &library, "alice", request("all", "valid"), None)
            .await
            .unwrap();
        let end = store
            .page(
                "alice".into(),
                "client".into(),
                page.session_id.clone(),
                3,
                10,
            )
            .await
            .unwrap();
        assert!(end.track_ids.is_empty());
        assert!(!end.has_more);
        assert!(matches!(
            store
                .page(
                    "alice".into(),
                    "client".into(),
                    page.session_id,
                    usize::MAX,
                    10
                )
                .await,
            Err(ShuffleSessionError::BadRequest(_))
        ));
    }

    #[test]
    fn canonical_rules_share_candidates_without_changing_or_filter_semantics() {
        let mut left = request("custom", "canonical");
        left.artist_ids = vec!["b".into(), "a".into(), "a".into()];
        left.genres = vec![" Jazz ".into(), "jazz".into(), String::new()];
        let mut right = left.clone();
        right.artist_ids = vec!["a".into(), "b".into()];
        right.genres = vec!["jazz".into()];
        assert!(left.scope().unwrap() == right.scope().unwrap());
        assert!(request("custom", "all").scope().unwrap() == Scope::All);
    }

    fn write_flac(path: &std::path::Path, tags: &[(&str, &str)]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut stream = [0u8; 34];
        stream[..4].copy_from_slice(&[0x10, 0, 0x10, 0]);
        stream[10..18]
            .copy_from_slice(&((44_100_u64 << 44) | (1 << 41) | (15 << 36) | 44_100).to_be_bytes());
        let mut comments = Vec::new();
        comments.extend_from_slice(&4_u32.to_le_bytes());
        comments.extend_from_slice(b"test");
        comments.extend_from_slice(&(tags.len() as u32).to_le_bytes());
        for (key, value) in tags {
            let entry = format!("{key}={value}");
            comments.extend_from_slice(&(entry.len() as u32).to_le_bytes());
            comments.extend_from_slice(entry.as_bytes());
        }
        let mut bytes = b"fLaC".to_vec();
        bytes.extend_from_slice(&[0, 0, 0, 34]);
        bytes.extend_from_slice(&stream);
        bytes.push(0x84);
        bytes.extend_from_slice(&(comments.len() as u32).to_be_bytes()[1..]);
        bytes.extend_from_slice(&comments);
        std::fs::write(path, bytes).unwrap();
    }

    #[tokio::test]
    async fn different_custom_filters_share_one_index_and_match_legacy_guest_genre_and_or_rules() {
        let path = std::env::temp_dir().join(format!(
            "phonolite-custom-rule-index-{}",
            uuid::Uuid::new_v4()
        ));
        let music = path.join("music");
        for (title, artist, composer, genre) in [
            ("Guest Song", "Main feat. Guest", "", "Rock"),
            ("Composer Song", "Main", "Composer", "Metal"),
            ("Solo Song", "Main", "", ""),
        ] {
            write_flac(
                &music.join(format!("Main/Album/{title}.flac")),
                &[
                    ("ALBUM", "Album"),
                    ("ALBUMARTIST", "Main"),
                    ("ARTIST", artist),
                    ("TITLE", title),
                    ("COMPOSER", composer),
                    ("GENRE", genre),
                ],
            );
        }
        let library = Library::load_or_scan(
            vec![library::LibraryRoot::new(String::new(), music)],
            path.join("library.redb"),
        )
        .unwrap()
        .0;
        for (name, genre) in [("Guest", "Jazz"), ("Composer", "Blues")] {
            library
                .update_artist_enrichment(
                    &common::artist_identity_id(name),
                    None,
                    &[genre.into()],
                    None,
                    None,
                    false,
                )
                .unwrap();
        }
        let album = library.list_albums(None, 1, 0).unwrap().0.remove(0);
        library
            .update_album_enrichment(&album.id, None, &["Classical".into()], false)
            .unwrap();
        let guest = common::artist_identity_id("Guest");
        let composer = common::artist_identity_id("Composer");
        let store = ShuffleSessionStore::new();
        for (artists, genres) in [
            (vec![], vec!["Jazz"]),
            (vec![], vec!["Blues"]),
            (vec![], vec![" Rock "]),
            (vec![], vec!["Classical"]),
            (vec![guest.clone()], vec![]),
            (vec![composer.clone()], vec![]),
            (vec![guest.clone()], vec![" Metal "]),
            (vec![composer.clone()], vec![" JAZZ "]),
            (vec!["unknown-artist".into()], vec!["unknown-genre"]),
        ] {
            let genres = genres.into_iter().map(str::to_owned).collect::<Vec<_>>();
            let expected = crate::shuffle::build_shuffle_queue(
                &library,
                ShuffleMode::Custom,
                None,
                None,
                &artists,
                &genres,
                &HashSet::new(),
            )
            .unwrap()
            .into_iter()
            .map(|track| track.id)
            .collect::<HashSet<_>>();
            let mut rule = request("custom", "rule");
            rule.artist_ids = artists;
            rule.genres = genres;
            let actual = store
                .catalog_candidates(library.clone(), 1, rule.scope().unwrap())
                .await
                .unwrap();
            assert_eq!(actual.iter().cloned().collect::<HashSet<_>>(), expected);
        }
        assert_eq!(store.inner.custom_builds.load(Ordering::Relaxed), 1);
        assert_eq!(store.inner.state.lock().rules.len(), 1);
        assert_eq!(
            store.inner.state.lock().candidates.len(),
            0,
            "filter combinations do not each cache another full candidate list"
        );
        // Different revisions replace enrichment predicates, while completed
        // decks retain only their selected IDs and keep their original order.
        library
            .update_artist_enrichment(&guest, None, &["Soul".into()], None, None, false)
            .unwrap();
        let candidates = store
            .catalog_candidates(
                library.clone(),
                2,
                Scope::Custom(Vec::new(), vec!["soul".into()]),
            )
            .await
            .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(store.inner.custom_builds.load(Ordering::Relaxed), 2);
        store
            .catalog_candidates(
                library.clone(),
                3,
                Scope::Custom(Vec::new(), vec!["soul".into()]),
            )
            .await
            .unwrap();
        assert!(store.inner.state.lock().rules.len() <= MAX_RULE_INDEXES);
        let resumed = store
            .resume_custom_candidates(library.clone(), 3, vec![], vec!["Soul".into()])
            .await
            .unwrap();
        let repeated = store
            .resume_custom_candidates(library.clone(), 3, vec![], vec![" soul ".into()])
            .await
            .unwrap();
        assert_eq!(resumed.len(), 1);
        assert!(
            Arc::ptr_eq(&resumed, &repeated),
            "resume pages reuse bounded selected IDs"
        );
        assert!(
            store
                .resume_custom_candidates(library.clone(), 3, vec![], vec![])
                .await
                .unwrap()
                .is_empty(),
            "empty persisted rules never select the full catalog"
        );
        drop(store);
        drop(library);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn custom_rule_posting_budget_rejects_growth_before_insertion() {
        let mut postings = HashMap::new();
        let mut bytes = MAX_RULE_INDEX_BYTES - 4;
        assert!(matches!(
            push_posting(&mut postings, "artist".into(), 0, &mut bytes),
            Err(ShuffleSessionError::Busy)
        ));
        assert!(postings.is_empty());
    }

    #[tokio::test]
    async fn expired_sessions_return_capacity_and_work_admission_is_bounded() {
        let (library, _db) = catalog(3);
        let store = ShuffleSessionStore::new();
        let page = create(&store, &library, "alice", request("all", "expired"), None)
            .await
            .unwrap();
        {
            let mut state = store.inner.state.lock();
            state
                .sessions
                .get_mut(&page.session_id)
                .unwrap()
                .last_access = Instant::now() - SESSION_TTL;
            state.expire(Instant::now());
            assert_eq!(state.deck_ids, 0);
        }
        let _waiting = store
            .inner
            .waiting
            .clone()
            .acquire_many_owned(32)
            .await
            .unwrap();
        assert!(matches!(
            store.acquire_work_permit().await,
            Err(ShuffleSessionError::Busy)
        ));
    }

    #[tokio::test]
    async fn shutdown_releases_a_queued_shuffle_before_existing_work_finishes() {
        let shutdown = crate::shutdown::Shutdown::new();
        let store = ShuffleSessionStore::new().with_shutdown(shutdown.clone());
        let held = store
            .inner
            .work
            .clone()
            .acquire_many_owned(2)
            .await
            .unwrap();
        let waiter_store = store.clone();
        let waiter = tokio::spawn(async move { waiter_store.acquire_work_permit().await });
        tokio::task::yield_now().await;
        shutdown.cancel();
        let result = tokio::time::timeout(Duration::from_secs(1), waiter).await;
        drop(held);
        assert!(matches!(
            result.unwrap().unwrap(),
            Err(ShuffleSessionError::Busy)
        ));
    }
}
