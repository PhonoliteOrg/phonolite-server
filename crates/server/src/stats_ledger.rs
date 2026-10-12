//! Legacy v1 migration adapter. Ingestion also updates the v2 monthly counters
//! in the same durable redb transaction until each old device completes handoff.
//! The original `stats` table remains a legacy baseline and is never replayed as events.

use std::collections::{BTreeSet, HashMap};
use std::ops::Bound;

use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use time::{Date, Month, OffsetDateTime};

use super::{stats_key, StatsError, StatsStore, UserPeriodStats};

#[path = "stats_counters.rs"]
pub mod counters;

const META: TableDefinition<&str, &[u8]> = TableDefinition::new("listening_meta_v1");
const EVENTS: TableDefinition<&str, &[u8]> = TableDefinition::new("listening_events_v1");
const CHANGES: TableDefinition<&str, &[u8]> = TableDefinition::new("listening_changes_v1");
const SESSIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("listening_sessions_v1");
const PERIODS: TableDefinition<&str, &[u8]> = TableDefinition::new("listening_periods_v1");
const PREFERENCES: TableDefinition<&str, &[u8]> = TableDefinition::new("listening_preferences_v1");
const TRACK_METADATA: TableDefinition<&str, &[u8]> = TableDefinition::new("listening_tracks_v1");
const ARTIST_METADATA: TableDefinition<&str, &[u8]> = TableDefinition::new("listening_artists_v1");
const GENRE_METADATA: TableDefinition<&str, &[u8]> =
    TableDefinition::new("listening_genre_metadata_v1");
const GENRE_DURATION_POLICY: &str = "genre_duration_policy_v2";
const PRIMARY_ARTIST_POLICY: &str = "primary_artist_policy_v2";
const GENRE_MIGRATION_CACHE_LIMIT: usize = 512;
pub const MAX_EVENT_BATCH: usize = 200;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ListeningArtist {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ListeningEvent {
    pub event_id: String,
    pub session_id: String,
    pub device_id: String,
    pub track_id: String,
    pub title: String,
    pub artist_id: String,
    pub artist_name: String,
    pub album_id: String,
    pub album_title: String,
    #[serde(default)]
    pub artists: Vec<ListeningArtist>,
    #[serde(default)]
    pub genres: Vec<String>,
    pub track_duration_ms: u64,
    pub session_started_at_ms: i64,
    pub started_at_ms: i64,
    pub ended_at_ms: i64,
    pub listened_ms: u64,
    pub rule_version: u32,
    #[serde(default)]
    pub end_reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct DevicePreference {
    pub device_id: String,
    pub enabled: bool,
    pub revision: u64,
}

#[derive(Debug, Serialize)]
pub struct EventRejection {
    pub event_id: String,
    pub reason: String,
}

#[derive(Debug, Default, Serialize)]
pub struct EventBatchResult {
    pub accepted: Vec<String>,
    pub rejected: Vec<EventRejection>,
}

#[derive(Debug, Serialize)]
pub struct ListeningChange {
    pub cursor: u64,
    pub event: ListeningEvent,
}

#[derive(Debug, Serialize)]
pub struct StatsSyncPage {
    pub server_id: String,
    pub user_id: String,
    pub collection_enabled: bool,
    pub history_generation: u64,
    pub events: Vec<ListeningChange>,
    pub next_cursor: u64,
    pub has_more: bool,
    pub genre_metadata: HashMap<String, Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preference_revision: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct StatsResetResult {
    pub server_id: String,
    pub user_id: String,
    pub history_generation: u64,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct AccountState {
    cursor: u64,
    // Never remove this marker: disconnected clients must retire old uploads.
    generation: u64,
}

#[derive(Debug, Deserialize, Serialize)]
struct SessionState {
    device_id: String,
    track_id: String,
    started_at_ms: i64,
    track_duration_ms: u64,
    listened_ms: u64,
    qualified: bool,
    metadata: String,
}

impl StatsStore {
    /// Delete one account's history atomically. Retrying an old generation is
    /// harmless and cannot delete listening recorded after the first reset.
    pub fn reset_history(
        &self,
        user_id: &str,
        expected_generation: u64,
    ) -> Result<StatsResetResult, StatsError> {
        if expected_generation == 0 {
            return Err(StatsError::Invalid("invalid history generation".into()));
        }
        let txn = self.db.begin_write()?;
        let result;
        {
            let mut meta = txn.open_table(META)?;
            let key = scoped_key(user_id, "account");
            let mut account = match meta.get(key.as_str())? {
                Some(value) => serde_json::from_slice::<AccountState>(value.value())?,
                None => AccountState {
                    cursor: 0,
                    generation: 1,
                },
            };
            if expected_generation > account.generation {
                return Err(StatsError::GenerationMismatch);
            }
            if expected_generation == account.generation {
                account.generation = account
                    .generation
                    .checked_add(1)
                    .ok_or_else(|| StatsError::Invalid("history generation exhausted".into()))?;
                account.cursor = 0;
                let prefix = account_prefix(user_id);
                for definition in [
                    EVENTS,
                    CHANGES,
                    SESSIONS,
                    TRACK_METADATA,
                    ARTIST_METADATA,
                    GENRE_METADATA,
                ] {
                    let mut table = txn.open_table(definition)?;
                    for removed in
                        table.drain_filter::<&str, _>(.., |key, _| key.starts_with(&prefix))?
                    {
                        removed?;
                    }
                }
                for definition in [PERIODS, super::STATS_TABLE] {
                    let mut table = txn.open_table(definition)?;
                    for removed in table.drain_filter::<&str, _>(.., |key, _| {
                        stored_period_user(key).ok() == Some(user_id)
                    })? {
                        removed?;
                    }
                }
                counters::reset(&txn, user_id)?;
                let bytes = serde_json::to_vec(&account)?;
                meta.insert(key.as_str(), bytes.as_slice())?;
            }
            let server_id = meta
                .get("server_id")?
                .map(|value| String::from_utf8_lossy(value.value()).into_owned())
                .ok_or_else(|| StatsError::Invalid("stats store identity is missing".into()))?;
            result = StatsResetResult {
                server_id,
                user_id: user_id.into(),
                history_generation: account.generation,
            };
        }
        txn.commit()?;
        Ok(result)
    }

    pub(super) fn init_ledger(&self) -> Result<(), StatsError> {
        let txn = self.db.begin_write()?;
        {
            let mut meta = txn.open_table(META)?;
            if meta.get("server_id")?.is_none() {
                let id = uuid::Uuid::new_v4().to_string();
                meta.insert("server_id", id.as_bytes())?;
            }
            txn.open_table(EVENTS)?;
            txn.open_table(CHANGES)?;
            txn.open_table(SESSIONS)?;
            txn.open_table(PERIODS)?;
            txn.open_table(PREFERENCES)?;
            txn.open_table(TRACK_METADATA)?;
            txn.open_table(ARTIST_METADATA)?;
            txn.open_table(GENRE_METADATA)?;
            if meta.get(GENRE_DURATION_POLICY)?.is_none() {
                // Only derived genre totals change. Immutable events, account
                // cursors, consent, and the aggregate-only legacy baseline stay
                // intact, including on a failed or interrupted migration.
                rebuild_genre_durations(&txn, None)?;
                meta.insert(GENRE_DURATION_POLICY, b"2".as_slice())?;
            }
            if meta.get(PRIMARY_ARTIST_POLICY)?.is_none() {
                // Artist credits in older snapshots can include band members
                // and production contributors. Rebuild their derived rankings
                // using the saved primary identity without changing history.
                rebuild_primary_artist_durations(&txn)?;
                meta.insert(PRIMARY_ARTIST_POLICY, b"2".as_slice())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Revision numbers make preference retries safe. A stale enable cannot
    /// overwrite a newer disable delivered by this installation.
    pub fn set_device_preference(
        &self,
        user_id: &str,
        preference: DevicePreference,
    ) -> Result<DevicePreference, StatsError> {
        validate_id(&preference.device_id)?;
        let txn = self.db.begin_write()?;
        let key = scoped_key(user_id, &preference.device_id);
        let result;
        {
            let mut table = txn.open_table(PREFERENCES)?;
            let existing = table
                .get(key.as_str())?
                .map(|value| serde_json::from_slice::<DevicePreference>(value.value()))
                .transpose()?;
            result = match existing {
                Some(existing) if existing.revision >= preference.revision => existing,
                _ => {
                    let bytes = serde_json::to_vec(&preference)?;
                    table.insert(key.as_str(), bytes.as_slice())?;
                    preference
                }
            };
        }
        txn.commit()?;
        Ok(result)
    }

    pub fn ingest_events(
        &self,
        user_id: &str,
        device_id: &str,
        generation: u64,
        batch: &[ListeningEvent],
    ) -> Result<EventBatchResult, StatsError> {
        validate_id(device_id)?;
        if batch.len() > MAX_EVENT_BATCH {
            return Err(StatsError::Invalid(
                "at most 200 listening events per batch".into(),
            ));
        }
        let txn = self.db.begin_write()?;
        counters::check_legacy(&txn, user_id, device_id)?;
        let mut result = EventBatchResult::default();
        {
            let preferences = txn.open_table(PREFERENCES)?;
            let pref_key = scoped_key(user_id, device_id);
            if let Some(value) = preferences.get(pref_key.as_str())? {
                let preference: DevicePreference = serde_json::from_slice(value.value())?;
                if !preference.enabled {
                    return Err(StatsError::DeviceDisabled);
                }
            }
            let mut meta = txn.open_table(META)?;
            let account_key = scoped_key(user_id, "account");
            let mut account = match meta.get(account_key.as_str())? {
                Some(value) => serde_json::from_slice::<AccountState>(value.value())?,
                None => AccountState {
                    cursor: 0,
                    generation: 1,
                },
            };
            if generation != account.generation {
                return Err(StatsError::GenerationMismatch);
            }
            let mut events = txn.open_table(EVENTS)?;
            let mut changes = txn.open_table(CHANGES)?;
            let mut sessions = txn.open_table(SESSIONS)?;
            let mut periods = txn.open_table(PERIODS)?;
            // Decode each affected monthly/annual summary once per batch,
            // instead of repeatedly serializing large track maps per checkpoint.
            let mut period_cache = HashMap::new();
            let mut tracks = txn.open_table(TRACK_METADATA)?;
            let mut artists = txn.open_table(ARTIST_METADATA)?;
            let genre_metadata = txn.open_table(GENRE_METADATA)?;
            for event in batch {
                let validation = validate_event(event, device_id);
                if let Err(reason) = validation {
                    result.rejected.push(EventRejection {
                        event_id: event.event_id.clone(),
                        reason,
                    });
                    continue;
                }
                let event_key = scoped_key(user_id, &event.event_id);
                let existing = events
                    .get(event_key.as_str())?
                    .map(|value| serde_json::from_slice::<ListeningEvent>(value.value()))
                    .transpose()?;
                if let Some(existing) = existing {
                    if existing == *event {
                        result.accepted.push(event.event_id.clone());
                    } else {
                        result.rejected.push(EventRejection {
                            event_id: event.event_id.clone(),
                            reason: "event_id already exists with different contents".into(),
                        });
                    }
                    continue;
                }
                let session_key = scoped_key(user_id, &event.session_id);
                let metadata = session_metadata(event)?;
                let mut session = match sessions.get(session_key.as_str())? {
                    Some(value) => serde_json::from_slice::<SessionState>(value.value())?,
                    None => SessionState {
                        device_id: event.device_id.clone(),
                        track_id: event.track_id.clone(),
                        started_at_ms: event.session_started_at_ms,
                        track_duration_ms: event.track_duration_ms,
                        listened_ms: 0,
                        qualified: false,
                        metadata: metadata.clone(),
                    },
                };
                if session.device_id != event.device_id
                    || session.track_id != event.track_id
                    || session.started_at_ms != event.session_started_at_ms
                    || session.track_duration_ms != event.track_duration_ms
                    || session.metadata != metadata
                {
                    result.rejected.push(EventRejection {
                        event_id: event.event_id.clone(),
                        reason: "session identity or metadata does not match existing session"
                            .into(),
                    });
                    continue;
                }
                let genres = reporting_genres(&genre_metadata, user_id, event)?;
                let mut genre_offset = 0;
                for (year, month, milliseconds) in split_duration(event)? {
                    let genre_shares =
                        genre_duration_shares(&genres, genre_offset, genre_offset + milliseconds);
                    update_ledger_period(
                        &mut periods,
                        &mut period_cache,
                        user_id,
                        year,
                        Some(month),
                        event,
                        milliseconds,
                        0,
                        &genre_shares,
                    )?;
                    update_ledger_period(
                        &mut periods,
                        &mut period_cache,
                        user_id,
                        year,
                        None,
                        event,
                        milliseconds,
                        0,
                        &genre_shares,
                    )?;
                    genre_offset += milliseconds;
                }
                session.listened_ms = session.listened_ms.saturating_add(event.listened_ms);
                let threshold = if session.track_duration_ms == 0 {
                    30_000
                } else {
                    30_000.min((session.track_duration_ms.saturating_add(1) / 2).max(1))
                };
                let newly_qualified = !session.qualified && session.listened_ms >= threshold;
                if newly_qualified {
                    // Attribute the single play to the session start, independent
                    // of which checkpoint arrives first or crosses the threshold.
                    let at = datetime(session.started_at_ms)?;
                    update_ledger_period(
                        &mut periods,
                        &mut period_cache,
                        user_id,
                        at.year(),
                        Some(at.month() as u8),
                        event,
                        0,
                        1,
                        &[],
                    )?;
                    update_ledger_period(
                        &mut periods,
                        &mut period_cache,
                        user_id,
                        at.year(),
                        None,
                        event,
                        0,
                        1,
                        &[],
                    )?;
                    session.qualified = true;
                }
                counters::add_legacy(&txn, user_id, event, newly_qualified)?;
                let bytes = serde_json::to_vec(event)?;
                events.insert(event_key.as_str(), bytes.as_slice())?;
                account.cursor = account
                    .cursor
                    .checked_add(1)
                    .ok_or_else(|| StatsError::Invalid("stats cursor exhausted".into()))?;
                let change_key = cursor_key(user_id, account.cursor);
                changes.insert(change_key.as_str(), event.event_id.as_bytes())?;
                let session_bytes = serde_json::to_vec(&session)?;
                sessions.insert(session_key.as_str(), session_bytes.as_slice())?;
                let track_key = scoped_key(user_id, &event.track_id);
                tracks.insert(track_key.as_str(), bytes.as_slice())?;
                if let Some((id, name)) = primary_artist(event) {
                    let key = scoped_key(user_id, id);
                    artists.insert(key.as_str(), name.as_bytes())?;
                }
                result.accepted.push(event.event_id.clone());
            }
            for (key, stats) in period_cache {
                let bytes = serde_json::to_vec(&stats)?;
                periods.insert(key.as_str(), bytes.as_slice())?;
            }
            let bytes = serde_json::to_vec(&account)?;
            meta.insert(account_key.as_str(), bytes.as_slice())?;
        }
        // Acknowledgments are only returned after a successful durable commit.
        txn.commit()?;
        Ok(result)
    }

    pub fn sync_events(
        &self,
        user_id: &str,
        after: u64,
        limit: usize,
        collection_enabled: bool,
        device_id: Option<&str>,
    ) -> Result<StatsSyncPage, StatsError> {
        if let Some(device_id) = device_id {
            validate_id(device_id)?;
        }
        let txn = self.db.begin_read()?;
        if counters::legacy_retired(&txn, user_id)? {
            return Err(StatsError::ProtocolRetired);
        }
        let meta = txn.open_table(META)?;
        let server_id = meta
            .get("server_id")?
            .map(|value| String::from_utf8_lossy(value.value()).into_owned())
            .ok_or_else(|| StatsError::Invalid("stats store identity is missing".into()))?;
        let key = scoped_key(user_id, "account");
        let account = match meta.get(key.as_str())? {
            Some(value) => serde_json::from_slice::<AccountState>(value.value())?,
            None => AccountState {
                cursor: 0,
                generation: 1,
            },
        };
        if after > account.cursor {
            return Err(StatsError::Invalid(
                "stats cursor is ahead of account history".into(),
            ));
        }
        let mut page = StatsSyncPage {
            server_id,
            user_id: user_id.into(),
            collection_enabled,
            history_generation: account.generation,
            events: Vec::new(),
            next_cursor: after,
            has_more: false,
            genre_metadata: HashMap::new(),
            device_enabled: None,
            preference_revision: None,
        };
        if let Some(device_id) = device_id {
            let table = txn.open_table(PREFERENCES)?;
            let key = scoped_key(user_id, device_id);
            let preference = match table.get(key.as_str())? {
                Some(value) => serde_json::from_slice::<DevicePreference>(value.value())?,
                None => DevicePreference {
                    device_id: device_id.into(),
                    enabled: true,
                    revision: 0,
                },
            };
            page.device_enabled = Some(preference.enabled);
            page.preference_revision = Some(preference.revision);
        }
        let changes = txn.open_table(CHANGES)?;
        let stored_events = txn.open_table(EVENTS)?;
        let prefix = account_prefix(user_id);
        let start = cursor_key(user_id, after.saturating_add(1));
        for item in changes.range(start.as_str()..)? {
            let (key, value) = item?;
            if !key.value().starts_with(&prefix) {
                break;
            }
            if page.events.len() >= limit.clamp(1, MAX_EVENT_BATCH) {
                page.has_more = true;
                break;
            }
            let cursor = key.value()[prefix.len()..]
                .parse::<u64>()
                .map_err(|_| StatsError::Invalid("invalid stored stats cursor".into()))?;
            let event_id = std::str::from_utf8(value.value())
                .map_err(|_| StatsError::Invalid("invalid stored event reference".into()))?;
            let event_key = scoped_key(user_id, event_id);
            let stored_event = stored_events
                .get(event_key.as_str())?
                .ok_or_else(|| StatsError::Invalid("stored stats event is missing".into()))?;
            page.events.push(ListeningChange {
                cursor,
                event: serde_json::from_slice(stored_event.value())?,
            });
            page.next_cursor = cursor;
        }
        let genre_metadata = txn.open_table(GENRE_METADATA)?;
        for change in &page.events {
            let track_id = &change.event.track_id;
            if page.genre_metadata.contains_key(track_id) {
                continue;
            }
            let key = scoped_key(user_id, track_id);
            if let Some(value) = genre_metadata.get(key.as_str())? {
                page.genre_metadata
                    .insert(track_id.clone(), serde_json::from_slice(value.value())?);
            }
        }
        Ok(page)
    }

    pub(super) fn get_ledger_period(
        &self,
        user_id: &str,
        year: i32,
        month: Option<u8>,
    ) -> Result<Option<UserPeriodStats>, StatsError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(PERIODS)?;
        let key = stats_key(user_id, year, month);
        let result = table
            .get(key.as_str())?
            .map(|value| serde_json::from_slice(value.value()).map_err(StatsError::from))
            .transpose();
        result
    }

    pub fn track_snapshot(
        &self,
        user_id: &str,
        track_id: &str,
    ) -> Result<Option<ListeningEvent>, StatsError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(TRACK_METADATA)?;
        let key = scoped_key(user_id, track_id);
        let result = table
            .get(key.as_str())?
            .map(|value| serde_json::from_slice(value.value()).map_err(StatsError::from))
            .transpose();
        result
    }

    pub fn artist_snapshot(
        &self,
        user_id: &str,
        artist_id: &str,
    ) -> Result<Option<String>, StatsError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(ARTIST_METADATA)?;
        let key = scoped_key(user_id, artist_id);
        let result = table
            .get(key.as_str())?
            .map(|value| String::from_utf8_lossy(value.value()).into_owned());
        Ok(result)
    }

    /// These IDs come from this account's immutable history, including tracks
    /// no longer in the library. Callers must only resolve exact catalog matches.
    pub fn listening_track_ids(&self, user_id: &str) -> Result<Vec<String>, StatsError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(TRACK_METADATA)?;
        let prefix = account_prefix(user_id);
        let mut ids = Vec::new();
        for item in table.range(prefix.as_str()..)? {
            let (key, _) = item?;
            let Some(id) = key.value().strip_prefix(&prefix) else {
                break;
            };
            ids.push(id.to_owned());
        }
        Ok(ids)
    }

    /// Replace only the reporting projection, preserving original event/session
    /// snapshots and deduplication identities. Empty metadata is a valid override.
    pub fn update_genre_metadata(
        &self,
        user_id: &str,
        metadata: &HashMap<String, Vec<String>>,
    ) -> Result<(), StatsError> {
        if metadata.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin_write()?;
        let mut changed = false;
        {
            let tracks = txn.open_table(TRACK_METADATA)?;
            let mut genres = txn.open_table(GENRE_METADATA)?;
            for (track_id, raw) in metadata {
                let key = scoped_key(user_id, track_id);
                if tracks.get(key.as_str())?.is_none() {
                    continue;
                }
                let canonical = canonical_genres(raw);
                let existing = genres
                    .get(key.as_str())?
                    .map(|value| serde_json::from_slice::<Vec<String>>(value.value()))
                    .transpose()?;
                if existing.as_ref() == Some(&canonical) {
                    continue;
                }
                let bytes = serde_json::to_vec(&canonical)?;
                genres.insert(key.as_str(), bytes.as_slice())?;
                changed = true;
            }
        }
        if changed {
            rebuild_genre_durations(&txn, Some(user_id))?;
            counters::genres_changed(&txn, user_id)?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn genre_metadata(
        &self,
        user_id: &str,
    ) -> Result<HashMap<String, Vec<String>>, StatsError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(GENRE_METADATA)?;
        let prefix = account_prefix(user_id);
        let mut metadata = HashMap::new();
        for item in table.range(prefix.as_str()..)? {
            let (key, value) = item?;
            let Some(id) = key.value().strip_prefix(&prefix) else {
                break;
            };
            metadata.insert(id.to_owned(), serde_json::from_slice(value.value())?);
        }
        Ok(metadata)
    }
}

fn account_prefix(user_id: &str) -> String {
    format!("{}:{}:", user_id.len(), user_id)
}
fn scoped_key(user_id: &str, id: &str) -> String {
    format!("{}{id}", account_prefix(user_id))
}
fn cursor_key(user_id: &str, cursor: u64) -> String {
    format!("{}{:020}", account_prefix(user_id), cursor)
}

fn validate_id(id: &str) -> Result<(), StatsError> {
    if id.is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
        return Err(StatsError::Invalid("invalid stats identifier".into()));
    }
    Ok(())
}

fn validate_event(event: &ListeningEvent, device_id: &str) -> Result<(), String> {
    for id in [
        &event.event_id,
        &event.session_id,
        &event.device_id,
        &event.track_id,
    ] {
        validate_id(id).map_err(|err| err.to_string())?;
    }
    if event.device_id != device_id {
        return Err("event device does not match request device".into());
    }
    if event.rule_version != 1 {
        return Err("unsupported listening rule version".into());
    }
    if event.title.len() > 4096
        || event.artist_name.len() > 4096
        || event.album_title.len() > 4096
        || event.artist_id.len() > 256
        || event.album_id.len() > 256
        || event.genres.len() > 64
        || event.genres.iter().any(|genre| genre.len() > 256)
        || event.artists.len() > 64
        || event
            .artists
            .iter()
            .any(|artist| artist.id.len() > 256 || artist.name.len() > 4096)
    {
        return Err("listening metadata exceeds size limits".into());
    }
    if event
        .end_reason
        .as_deref()
        .is_some_and(|reason| !matches!(reason, "finished" | "skipped" | "stopped" | "interrupted"))
    {
        return Err("invalid session end reason".into());
    }
    if event.session_started_at_ms > event.started_at_ms
        || event.started_at_ms > event.ended_at_ms
        || event.session_started_at_ms < 0
        || datetime(event.session_started_at_ms).is_err()
        || datetime(event.ended_at_ms).is_err()
    {
        return Err("invalid listening timestamps".into());
    }
    let interval = event.ended_at_ms - event.started_at_ms;
    if event.listened_ms > 3_600_000
        || interval > 86_400_000
        || event.track_duration_ms > 604_800_000
        || (event.listened_ms == 0 && event.end_reason.is_none())
        || (event.listened_ms > 0 && interval == 0)
        || event.listened_ms > interval as u64 + 5_000
    {
        return Err("invalid listening duration".into());
    }
    Ok(())
}

fn datetime(ms: i64) -> Result<OffsetDateTime, StatsError> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .map_err(|_| StatsError::Invalid("timestamp is outside supported calendar range".into()))
}

fn session_metadata(event: &ListeningEvent) -> Result<String, StatsError> {
    // Match the client database's fixed session snapshot. Changing a snapshot
    // midway through a session would make the replay impossible to reconcile.
    Ok(serde_json::to_string(&(
        &event.title,
        &event.artist_id,
        &event.artist_name,
        &event.album_id,
        &event.album_title,
        &event.artists,
        &event.genres,
    ))?)
}

fn primary_artist(event: &ListeningEvent) -> Option<(&str, &str)> {
    let id = event.artist_id.trim();
    let name = event.artist_name.trim();
    if !id.is_empty() {
        // Display metadata may contain a joined credit string. The matching
        // primary credit supplies the band's name without adding contributors.
        let display_name = event
            .artists
            .iter()
            .find(|artist| artist.id.trim() == id && !artist.name.trim().is_empty())
            .map(|artist| artist.name.trim())
            .unwrap_or(if name.is_empty() { id } else { name });
        return Some((id, display_name));
    }
    if !name.is_empty() {
        return Some((name, name));
    }
    event.artists.iter().find_map(|artist| {
        let id = artist.id.trim();
        let name = artist.name.trim();
        if id.is_empty() && name.is_empty() {
            None
        } else {
            Some((
                if id.is_empty() { name } else { id },
                if name.is_empty() { id } else { name },
            ))
        }
    })
}

/// Proportionally distribute consumed duration across UTC month boundaries.
/// Integer allocation preserves the event's exact total even at a year boundary.
fn split_duration(event: &ListeningEvent) -> Result<Vec<(i32, u8, u64)>, StatsError> {
    if event.listened_ms == 0 {
        return Ok(Vec::new());
    }
    let elapsed = (event.ended_at_ms - event.started_at_ms) as u64;
    let mut output = Vec::new();
    let mut position = event.started_at_ms;
    let mut assigned = 0;
    while position < event.ended_at_ms {
        let at = datetime(position)?;
        let (year, month) = if at.month() == Month::December {
            (at.year() + 1, Month::January)
        } else {
            (at.year(), at.month().next())
        };
        let boundary = Date::from_calendar_date(year, month, 1)
            .map(|date| date.midnight().assume_utc().unix_timestamp_nanos() / 1_000_000)
            .unwrap_or(i128::from(event.ended_at_ms));
        let end = event.ended_at_ms.min(boundary as i64);
        let milliseconds = if end == event.ended_at_ms {
            event.listened_ms - assigned
        } else {
            (u128::from(event.listened_ms) * (end - position) as u128 / u128::from(elapsed)) as u64
        };
        assigned += milliseconds;
        output.push((at.year(), at.month() as u8, milliseconds));
        position = end;
    }
    Ok(output)
}

/// Canonicalize labels only for reporting; raw session snapshots retain their
/// original labels so retries and replication keep the same event identity.
fn canonical_genres(raw: &[String]) -> Vec<String> {
    let normalized: BTreeSet<String> = raw
        .iter()
        .map(|genre| {
            genre
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase()
        })
        .filter(|genre| !genre.is_empty())
        .collect();
    normalized
        .into_iter()
        .map(|genre| {
            genre
                .split(' ')
                .map(|word| {
                    let mut chars = word.chars();
                    let first = chars.next().expect("canonical words are not empty");
                    first.to_uppercase().collect::<String>() + chars.as_str()
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

/// Allocate an event's finite duration once across its distinct tags. Using
/// differences of cumulative shares also preserves each tag's exact total when
/// that event is split between monthly and annual reporting periods.
fn genre_duration_shares(genres: &[String], start: u64, end: u64) -> Vec<(&str, u64)> {
    if genres.is_empty() || start == end {
        return Vec::new();
    }
    let count = genres.len() as u64;
    genres
        .iter()
        .enumerate()
        .map(|(index, genre)| {
            let index = index as u64;
            let before = start / count + u64::from(index < start % count);
            let after = end / count + u64::from(index < end % count);
            (genre.as_str(), after - before)
        })
        .collect()
}

fn stored_event_user(key: &str) -> Result<&str, StatsError> {
    let (length, suffix) = key
        .split_once(':')
        .ok_or_else(|| StatsError::Invalid("invalid stored stats account key".into()))?;
    let length = length
        .parse::<usize>()
        .map_err(|_| StatsError::Invalid("invalid stored stats account key".into()))?;
    let user = suffix
        .get(..length)
        .filter(|_| {
            suffix
                .get(length..)
                .is_some_and(|rest| rest.starts_with(':'))
        })
        .ok_or_else(|| StatsError::Invalid("invalid stored stats account key".into()))?;
    Ok(user)
}

fn reporting_genres(
    metadata: &redb::Table<&str, &[u8]>,
    user_id: &str,
    event: &ListeningEvent,
) -> Result<Vec<String>, StatsError> {
    let key = scoped_key(user_id, &event.track_id);
    match metadata.get(key.as_str())? {
        Some(value) => Ok(serde_json::from_slice(value.value())?),
        None => Ok(canonical_genres(&event.genres)),
    }
}

fn stored_period_user(key: &str) -> Result<&str, StatsError> {
    key.rsplit_once(':')
        .and_then(|(prefix, _)| prefix.rsplit_once(':'))
        .map(|(user, _)| user)
        .ok_or_else(|| StatsError::Invalid("invalid stored stats period key".into()))
}

fn rebuild_genre_durations(
    txn: &redb::WriteTransaction,
    target_user: Option<&str>,
) -> Result<(), StatsError> {
    let events = txn.open_table(EVENTS)?;
    let mut periods = txn.open_table(PERIODS)?;
    let metadata = txn.open_table(GENRE_METADATA)?;
    // Keyset batches bound decoded summary memory without rescanning earlier
    // periods. Flush each batch before moving on, within the same transaction.
    let mut after_key: Option<String> = None;
    loop {
        let lower = after_key
            .as_deref()
            .map_or(Bound::Unbounded, Bound::Excluded);
        let mut batch = Vec::new();
        let mut last_key = None;
        for item in periods
            .range::<&str>((lower, Bound::Unbounded))?
            .take(GENRE_MIGRATION_CACHE_LIMIT)
        {
            let (key, value) = item?;
            last_key = Some(key.value().to_owned());
            if target_user.is_some_and(|user| stored_period_user(key.value()).ok() != Some(user)) {
                continue;
            }
            let mut stats: UserPeriodStats = serde_json::from_slice(value.value())?;
            stats.genre_ms.clear();
            batch.push((key.value().to_owned(), stats));
        }
        let Some(last_key) = last_key else {
            break;
        };
        after_key = Some(last_key);
        for (key, stats) in batch {
            let bytes = serde_json::to_vec(&stats)?;
            periods.insert(key.as_str(), bytes.as_slice())?;
        }
    }
    // Read immutable events exactly once. A flushed period can be revisited by
    // later events, so reload its already-rebuilt totals instead of starting over.
    let mut cache = HashMap::new();
    let account_start = target_user.map(account_prefix);
    let lower = account_start
        .as_deref()
        .map_or(Bound::Unbounded, Bound::Included);
    for item in events.range::<&str>((lower, Bound::Unbounded))? {
        let (key, value) = item?;
        let user_id = stored_event_user(key.value())?;
        if target_user.is_some_and(|user| user != user_id) {
            break;
        }
        let event: ListeningEvent = serde_json::from_slice(value.value())?;
        let genres = reporting_genres(&metadata, user_id, &event)?;
        if genres.is_empty() {
            continue;
        }
        let mut offset = 0;
        for (year, month, milliseconds) in split_duration(&event)? {
            let shares = genre_duration_shares(&genres, offset, offset + milliseconds);
            for period_month in [Some(month), None] {
                let key = stats_key(user_id, year, period_month);
                if !cache.contains_key(&key) {
                    if cache.len() == GENRE_MIGRATION_CACHE_LIMIT {
                        flush_migration_periods(&mut periods, &mut cache)?;
                    }
                    let stats = match periods.get(key.as_str())? {
                        Some(value) => serde_json::from_slice::<UserPeriodStats>(value.value())?,
                        None => UserPeriodStats::default(),
                    };
                    cache.insert(key.clone(), stats);
                }
                let stats = cache.get_mut(&key).expect("period inserted above");
                for (genre, share) in &shares {
                    add_count(&mut stats.genre_ms, genre, *share);
                }
            }
            offset += milliseconds;
        }
    }
    flush_migration_periods(&mut periods, &mut cache)
}

fn flush_migration_periods(
    periods: &mut redb::Table<&str, &[u8]>,
    cache: &mut HashMap<String, UserPeriodStats>,
) -> Result<(), StatsError> {
    for (key, stats) in cache.drain() {
        let bytes = serde_json::to_vec(&stats)?;
        periods.insert(key.as_str(), bytes.as_slice())?;
    }
    Ok(())
}

fn rebuild_primary_artist_durations(txn: &redb::WriteTransaction) -> Result<(), StatsError> {
    let events = txn.open_table(EVENTS)?;
    let mut periods = txn.open_table(PERIODS)?;
    let mut artists = txn.open_table(ARTIST_METADATA)?;
    // Clear only ledger artist totals, retaining every other summary field and
    // the original aggregate baseline. Keyset batches bound decoded memory.
    let mut after_key: Option<String> = None;
    loop {
        let lower = after_key
            .as_deref()
            .map_or(Bound::Unbounded, Bound::Excluded);
        let mut batch = Vec::new();
        for item in periods
            .range::<&str>((lower, Bound::Unbounded))?
            .take(GENRE_MIGRATION_CACHE_LIMIT)
        {
            let (key, value) = item?;
            let mut stats: UserPeriodStats = serde_json::from_slice(value.value())?;
            stats.artist_ms.clear();
            batch.push((key.value().to_owned(), stats));
        }
        let Some((last_key, _)) = batch.last() else {
            break;
        };
        after_key = Some(last_key.clone());
        for (key, stats) in batch {
            let bytes = serde_json::to_vec(&stats)?;
            periods.insert(key.as_str(), bytes.as_slice())?;
        }
    }
    let mut cache = HashMap::new();
    for item in events.iter()? {
        let (key, value) = item?;
        let user_id = stored_event_user(key.value())?;
        let event: ListeningEvent = serde_json::from_slice(value.value())?;
        let Some((artist_id, artist_name)) = primary_artist(&event) else {
            continue;
        };
        // Leave unrelated artist names available for any legacy baseline, and
        // correct only the names used by event-based primary artist rankings.
        let artist_key = scoped_key(user_id, artist_id);
        artists.insert(artist_key.as_str(), artist_name.as_bytes())?;
        for (year, month, milliseconds) in split_duration(&event)? {
            for period_month in [Some(month), None] {
                let key = stats_key(user_id, year, period_month);
                if !cache.contains_key(&key) {
                    if cache.len() == GENRE_MIGRATION_CACHE_LIMIT {
                        flush_migration_periods(&mut periods, &mut cache)?;
                    }
                    let stats = match periods.get(key.as_str())? {
                        Some(value) => serde_json::from_slice::<UserPeriodStats>(value.value())?,
                        None => UserPeriodStats::default(),
                    };
                    cache.insert(key.clone(), stats);
                }
                let stats = cache.get_mut(&key).expect("period inserted above");
                add_count(&mut stats.artist_ms, artist_id, milliseconds);
            }
        }
    }
    flush_migration_periods(&mut periods, &mut cache)
}

fn update_ledger_period(
    table: &mut redb::Table<&str, &[u8]>,
    cache: &mut HashMap<String, UserPeriodStats>,
    user_id: &str,
    year: i32,
    month: Option<u8>,
    event: &ListeningEvent,
    milliseconds: u64,
    plays: u64,
    genre_shares: &[(&str, u64)],
) -> Result<(), StatsError> {
    let key = stats_key(user_id, year, month);
    if !cache.contains_key(&key) {
        let stats = match table.get(key.as_str())? {
            Some(value) => serde_json::from_slice::<UserPeriodStats>(value.value())?,
            None => UserPeriodStats::default(),
        };
        cache.insert(key.clone(), stats);
    }
    let stats = cache.get_mut(&key).expect("period inserted above");
    stats.total_ms = stats.total_ms.saturating_add(milliseconds);
    add_count(&mut stats.track_ms, &event.track_id, milliseconds);
    if let Some((artist, _)) = primary_artist(event) {
        add_count(&mut stats.artist_ms, artist, milliseconds);
    }
    for (genre, share) in genre_shares {
        add_count(&mut stats.genre_ms, genre, *share);
    }
    add_count(&mut stats.track_plays, &event.track_id, plays);
    Ok(())
}

fn add_count(map: &mut std::collections::HashMap<String, u64>, id: &str, value: u64) {
    if value == 0 || id.is_empty() {
        return;
    }
    let total = map.entry(id.into()).or_default();
    *total = total.saturating_add(value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::PathBuf, sync::Arc};

    struct Fixture {
        store: StatsStore,
        path: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("phonolite-stats-{}.redb", uuid::Uuid::new_v4()));
            let store = StatsStore::new(Arc::new(redb::Database::create(&path).unwrap()));
            store.init_tables().unwrap();
            Self { store, path }
        }
    }
    fn event(id: &str, session: &str, start: i64, milliseconds: u64) -> ListeningEvent {
        ListeningEvent {
            event_id: id.into(),
            session_id: session.into(),
            device_id: "desktop".into(),
            track_id: "track".into(),
            title: "Track".into(),
            artist_id: "artist".into(),
            artist_name: "Artist".into(),
            album_id: "album".into(),
            album_title: "Album".into(),
            artists: Vec::new(),
            genres: vec!["Rock".into()],
            track_duration_ms: 120_000,
            session_started_at_ms: 1_735_689_600_000,
            started_at_ms: start,
            ended_at_ms: start + milliseconds as i64,
            listened_ms: milliseconds,
            rule_version: 1,
            end_reason: None,
        }
    }
    fn cleanup(fixture: Fixture) {
        let path = fixture.path.clone();
        drop(fixture);
        std::fs::remove_file(path).unwrap();
    }

    fn table_contents(
        store: &StatsStore,
        definition: TableDefinition<&str, &[u8]>,
    ) -> Vec<(String, Vec<u8>)> {
        let txn = store.db.begin_read().unwrap();
        let table = txn.open_table(definition).unwrap();
        table
            .iter()
            .unwrap()
            .map(|item| {
                let (key, value) = item.unwrap();
                (key.value().to_owned(), value.value().to_vec())
            })
            .collect()
    }

    #[test]
    fn reset_deletes_only_target_account_history_including_legacy_and_preserves_consent() {
        let fixture = Fixture::new();
        let user = "user:one";
        let other = "user:one:other";
        let song = event("song", "session", 1_735_689_600_000, 60_000);
        for owner in [user, other] {
            fixture
                .store
                .ingest_events(owner, "desktop", 1, &[song.clone()])
                .unwrap();
            fixture
                .store
                .update_genre_metadata(
                    owner,
                    &HashMap::from([("track".into(), vec!["Jazz".into()])]),
                )
                .unwrap();
            let txn = fixture.store.db.begin_write().unwrap();
            {
                let mut legacy = txn.open_table(super::super::STATS_TABLE).unwrap();
                let bytes = bincode::serialize(&UserPeriodStats {
                    total_ms: 123,
                    ..Default::default()
                })
                .unwrap();
                legacy
                    .insert(stats_key(owner, 2024, None).as_str(), bytes.as_slice())
                    .unwrap();
            }
            txn.commit().unwrap();
        }
        fixture
            .store
            .set_device_preference(
                user,
                DevicePreference {
                    device_id: "desktop".into(),
                    enabled: false,
                    revision: 3,
                },
            )
            .unwrap();
        let preferences = table_contents(&fixture.store, PREFERENCES);
        let server_id = fixture
            .store
            .sync_events(user, 0, 1, false, None)
            .unwrap()
            .server_id;
        let reset = fixture.store.reset_history(user, 1).unwrap();
        assert_eq!(reset.history_generation, 2);
        assert_eq!(reset.user_id, user);
        assert_eq!(reset.server_id, server_id);
        for definition in [
            EVENTS,
            CHANGES,
            SESSIONS,
            TRACK_METADATA,
            ARTIST_METADATA,
            GENRE_METADATA,
        ] {
            let contents = table_contents(&fixture.store, definition);
            assert!(!contents
                .iter()
                .any(|(key, _)| key.starts_with(&account_prefix(user))));
            assert!(!contents.is_empty());
        }
        for definition in [PERIODS, super::super::STATS_TABLE] {
            let contents = table_contents(&fixture.store, definition);
            assert!(!contents
                .iter()
                .any(|(key, _)| stored_period_user(key).unwrap() == user));
            assert!(!contents.is_empty());
        }
        assert!(fixture
            .store
            .get_period(user, 2025, None)
            .unwrap()
            .is_none());
        assert!(fixture
            .store
            .get_period(user, 2024, None)
            .unwrap()
            .is_none());
        assert_eq!(
            fixture
                .store
                .get_period(other, 2025, None)
                .unwrap()
                .unwrap()
                .total_ms,
            60_000
        );
        assert_eq!(table_contents(&fixture.store, PREFERENCES), preferences);
        let state = fixture
            .store
            .sync_events(user, 0, 1, false, Some("desktop"))
            .unwrap();
        assert_eq!(state.device_enabled, Some(false));
        assert_eq!(state.preference_revision, Some(3));
        assert_eq!(state.next_cursor, 0);
        cleanup(fixture);
    }

    #[test]
    fn reset_retries_preserve_fresh_listening_and_persist_generation() {
        let fixture = Fixture::new();
        let song = event("song", "session", 1_735_689_600_000, 60_000);
        fixture
            .store
            .ingest_events("user", "desktop", 1, &[song.clone()])
            .unwrap();
        fixture.store.reset_history("user", 1).unwrap();
        assert!(matches!(
            fixture
                .store
                .ingest_events("user", "desktop", 1, &[song.clone()]),
            Err(StatsError::GenerationMismatch)
        ));
        fixture
            .store
            .ingest_events("user", "desktop", 2, &[song.clone()])
            .unwrap();
        assert_eq!(
            fixture
                .store
                .reset_history("user", 1)
                .unwrap()
                .history_generation,
            2
        );
        assert_eq!(
            fixture
                .store
                .get_period("user", 2025, None)
                .unwrap()
                .unwrap()
                .total_ms,
            60_000
        );
        assert!(matches!(
            fixture.store.reset_history("user", 99),
            Err(StatsError::GenerationMismatch)
        ));
        assert!(matches!(
            fixture.store.reset_history("user", 0),
            Err(StatsError::Invalid(_))
        ));
        let path = fixture.path.clone();
        drop(fixture);
        let store = StatsStore::new(Arc::new(redb::Database::create(&path).unwrap()));
        store.init_tables().unwrap();
        assert_eq!(
            store
                .sync_events("user", 0, 1, true, None)
                .unwrap()
                .history_generation,
            2
        );
        assert!(matches!(
            store.ingest_events("user", "desktop", 1, &[song]),
            Err(StatsError::GenerationMismatch)
        ));
        cleanup(Fixture { store, path });
    }

    #[test]
    fn primary_band_receives_time_without_credited_members() {
        let fixture = Fixture::new();
        let mut song = event("song", "jamiroquai-session", 1_735_689_600_000, 30_005);
        song.artist_id = " jamiroquai ".into();
        song.artist_name = "Jamiroquai, Jason Kay, Sola Akingbola".into();
        song.artists = vec![
            ListeningArtist {
                id: "jason-kay".into(),
                name: "Jason Kay".into(),
            },
            ListeningArtist {
                id: "jamiroquai".into(),
                name: " Jamiroquai ".into(),
            },
            ListeningArtist {
                id: "sola-akingbola".into(),
                name: "Sola Akingbola".into(),
            },
        ];
        fixture
            .store
            .ingest_events("user", "desktop", 1, &[song.clone()])
            .unwrap();
        let stats = fixture
            .store
            .get_ledger_period("user", 2025, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            stats.artist_ms,
            HashMap::from([("jamiroquai".into(), 30_005)])
        );
        assert_eq!(stats.artist_ms.values().sum::<u64>(), stats.total_ms);
        assert_eq!(
            fixture.store.artist_snapshot("user", "jamiroquai").unwrap(),
            Some("Jamiroquai".into())
        );
        assert_eq!(
            fixture.store.artist_snapshot("user", "jason-kay").unwrap(),
            None
        );
        assert_eq!(
            fixture
                .store
                .sync_events("user", 0, 200, true, None)
                .unwrap()
                .events[0]
                .event,
            song
        );
        cleanup(fixture);
    }

    #[test]
    fn primary_artist_fallbacks_match_client_without_multiple_credit_totals() {
        let mut song = event("song", "session", 1_735_689_600_000, 30_000);
        song.artists = vec![
            ListeningArtist {
                id: " ".into(),
                name: " ".into(),
            },
            ListeningArtist {
                id: " secondary ".into(),
                name: " Contributor ".into(),
            },
        ];
        assert_eq!(primary_artist(&song), Some(("artist", "Artist")));
        song.artist_name.clear();
        assert_eq!(primary_artist(&song), Some(("artist", "artist")));
        song.artist_id.clear();
        song.artist_name = " Primary Artist ".into();
        assert_eq!(
            primary_artist(&song),
            Some(("Primary Artist", "Primary Artist"))
        );
        song.artist_name.clear();
        assert_eq!(primary_artist(&song), Some(("secondary", "Contributor")));
        song.artists[1].name.clear();
        assert_eq!(primary_artist(&song), Some(("secondary", "secondary")));
        song.artists[1].id.clear();
        song.artists[1].name = " Name Only ".into();
        assert_eq!(primary_artist(&song), Some(("Name Only", "Name Only")));
        song.artists.clear();
        assert_eq!(primary_artist(&song), None);
    }

    #[test]
    fn primary_artist_migration_preserves_history_and_rebuilds_scoped_rankings_once() {
        let fixture = Fixture::new();
        let user = "u:sér";
        let mut band = event("band", "band-session", 1_735_689_599_995, 30_005);
        band.session_started_at_ms = band.started_at_ms;
        band.artist_id = "jamiroquai".into();
        band.artist_name = "Jamiroquai, Jason Kay".into();
        band.artists = vec![
            ListeningArtist {
                id: "jamiroquai".into(),
                name: "Jamiroquai".into(),
            },
            ListeningArtist {
                id: "jason-kay".into(),
                name: "Jason Kay".into(),
            },
        ];
        fixture
            .store
            .ingest_events(user, "desktop", 1, &[band.clone()])
            .unwrap();
        let mut other = band;
        other.artist_id = "in-flames".into();
        other.artist_name = "In Flames".into();
        fixture
            .store
            .ingest_events("other", "desktop", 1, &[other])
            .unwrap();
        fixture
            .store
            .set_device_preference(
                user,
                DevicePreference {
                    device_id: "desktop".into(),
                    enabled: false,
                    revision: 5,
                },
            )
            .unwrap();
        let expected_periods: HashMap<String, serde_json::Value> =
            table_contents(&fixture.store, PERIODS)
                .into_iter()
                .map(|(key, bytes)| (key, serde_json::from_slice(&bytes).unwrap()))
                .collect();
        let txn = fixture.store.db.begin_write().unwrap();
        {
            let mut meta = txn.open_table(META).unwrap();
            meta.remove(PRIMARY_ARTIST_POLICY).unwrap();
            let mut periods = txn.open_table(PERIODS).unwrap();
            for (key, value) in &expected_periods {
                let mut stats: UserPeriodStats = serde_json::from_value(value.clone()).unwrap();
                stats.artist_ms.insert("jason-kay".into(), stats.total_ms);
                stats.artist_ms.insert("stale-credit".into(), 999_999);
                let bytes = serde_json::to_vec(&stats).unwrap();
                periods.insert(key.as_str(), bytes.as_slice()).unwrap();
            }
            let mut artists = txn.open_table(ARTIST_METADATA).unwrap();
            artists
                .insert(
                    scoped_key(user, "jamiroquai").as_str(),
                    b"Jamiroquai, Jason Kay".as_slice(),
                )
                .unwrap();
            artists
                .insert(
                    scoped_key(user, "legacy-artist").as_str(),
                    b"Legacy Artist".as_slice(),
                )
                .unwrap();
            let mut legacy = txn.open_table(super::super::STATS_TABLE).unwrap();
            let baseline = UserPeriodStats {
                total_ms: 60_000,
                artist_ms: HashMap::from([("legacy-artist".into(), 60_000)]),
                ..Default::default()
            };
            let bytes = bincode::serialize(&baseline).unwrap();
            legacy
                .insert(stats_key(user, 2025, None).as_str(), bytes.as_slice())
                .unwrap();
        }
        txn.commit().unwrap();
        let immutable_tables = [
            EVENTS,
            CHANGES,
            SESSIONS,
            TRACK_METADATA,
            PREFERENCES,
            super::super::STATS_TABLE,
        ];
        let immutable_before: Vec<_> = immutable_tables
            .iter()
            .map(|table| table_contents(&fixture.store, *table))
            .collect();
        let meta_before = table_contents(&fixture.store, META);
        fixture.store.init_ledger().unwrap();
        let actual_periods: HashMap<String, serde_json::Value> =
            table_contents(&fixture.store, PERIODS)
                .into_iter()
                .map(|(key, bytes)| (key, serde_json::from_slice(&bytes).unwrap()))
                .collect();
        assert_eq!(actual_periods, expected_periods);
        for (table, before) in immutable_tables.iter().zip(immutable_before) {
            assert_eq!(table_contents(&fixture.store, *table), before);
        }
        let meta_after: Vec<_> = table_contents(&fixture.store, META)
            .into_iter()
            .filter(|(key, _)| key != PRIMARY_ARTIST_POLICY)
            .collect();
        assert_eq!(meta_after, meta_before);
        assert_eq!(
            fixture.store.artist_snapshot(user, "jamiroquai").unwrap(),
            Some("Jamiroquai".into())
        );
        assert_eq!(
            fixture
                .store
                .artist_snapshot(user, "legacy-artist")
                .unwrap(),
            Some("Legacy Artist".into())
        );
        let combined = fixture.store.get_period(user, 2025, None).unwrap().unwrap();
        assert_eq!(combined.total_ms, 90_000);
        assert_eq!(combined.artist_ms["jamiroquai"], 30_000);
        assert_eq!(combined.artist_ms["legacy-artist"], 60_000);
        assert!(!combined.artist_ms.contains_key("jason-kay"));
        assert_eq!(
            fixture
                .store
                .get_ledger_period("other", 2025, None)
                .unwrap()
                .unwrap()
                .artist_ms,
            HashMap::from([("in-flames".into(), 30_000)])
        );
        let periods_after = table_contents(&fixture.store, PERIODS);
        let artists_after = table_contents(&fixture.store, ARTIST_METADATA);
        fixture.store.init_ledger().unwrap();
        assert_eq!(table_contents(&fixture.store, PERIODS), periods_after);
        assert_eq!(
            table_contents(&fixture.store, ARTIST_METADATA),
            artists_after
        );
        cleanup(fixture);
    }

    #[test]
    fn catalog_genres_reclassify_saved_history_without_changing_raw_events_or_other_accounts() {
        let fixture = Fixture::new();
        let mut song = event("song", "session", 1_735_689_599_995, 30_005);
        song.session_started_at_ms = song.started_at_ms;
        song.genres = vec!["Rock".into(), "Pop".into()];
        for user in ["u", "u:sér"] {
            fixture
                .store
                .ingest_events(user, "desktop", 1, &[song.clone()])
                .unwrap();
        }
        assert_eq!(fixture.store.listening_track_ids("u").unwrap(), ["track"]);
        let immutable_tables = [
            EVENTS,
            CHANGES,
            SESSIONS,
            TRACK_METADATA,
            ARTIST_METADATA,
            PREFERENCES,
            META,
            super::super::STATS_TABLE,
        ];
        let immutable_before: Vec<_> = immutable_tables
            .iter()
            .map(|table| table_contents(&fixture.store, *table))
            .collect();
        let periods_before = table_contents(&fixture.store, PERIODS);
        let metadata = HashMap::from([
            (
                "track".into(),
                vec![
                    "acid jazz".into(),
                    "Electronic".into(),
                    "funk".into(),
                    " ACID\tJAZZ ".into(),
                ],
            ),
            ("unrecorded".into(), vec!["Never Used".into()]),
        ]);
        fixture.store.update_genre_metadata("u", &metadata).unwrap();
        assert_eq!(
            fixture.store.genre_metadata("u").unwrap(),
            HashMap::from([(
                "track".into(),
                vec!["Acid Jazz".into(), "Electronic".into(), "Funk".into()]
            )])
        );
        assert!(fixture.store.genre_metadata("u:sér").unwrap().is_empty());
        let old = fixture
            .store
            .get_ledger_period("u", 2024, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            old.genre_ms,
            HashMap::from([
                ("Acid Jazz".into(), 2),
                ("Electronic".into(), 2),
                ("Funk".into(), 1),
            ])
        );
        let current = fixture
            .store
            .get_ledger_period("u", 2025, None)
            .unwrap()
            .unwrap();
        assert_eq!(current.total_ms, 30_000);
        assert_eq!(
            current.genre_ms,
            HashMap::from([
                ("Acid Jazz".into(), 10_000),
                ("Electronic".into(), 10_000),
                ("Funk".into(), 10_000),
            ])
        );
        for (key, before) in periods_before {
            let actual = table_contents(&fixture.store, PERIODS)
                .into_iter()
                .find(|(actual_key, _)| actual_key == &key)
                .unwrap()
                .1;
            if stored_period_user(&key).unwrap() != "u" {
                assert_eq!(actual, before);
            } else {
                let mut actual: UserPeriodStats = serde_json::from_slice(&actual).unwrap();
                let mut before: UserPeriodStats = serde_json::from_slice(&before).unwrap();
                actual.genre_ms.clear();
                before.genre_ms.clear();
                assert_eq!(
                    serde_json::to_value(actual).unwrap(),
                    serde_json::to_value(before).unwrap()
                );
            }
        }
        for (table, before) in immutable_tables.iter().zip(immutable_before) {
            assert_eq!(table_contents(&fixture.store, *table), before);
        }
        let periods_after = table_contents(&fixture.store, PERIODS);
        fixture.store.update_genre_metadata("u", &metadata).unwrap();
        assert_eq!(table_contents(&fixture.store, PERIODS), periods_after);
        let page = fixture.store.sync_events("u", 0, 1, true, None).unwrap();
        assert_eq!(page.events[0].event, song);
        assert_eq!(
            page.genre_metadata,
            fixture.store.genre_metadata("u").unwrap()
        );
        assert!(fixture
            .store
            .sync_events("u", 1, 1, true, None)
            .unwrap()
            .genre_metadata
            .is_empty());

        // New checkpoints use the persisted projection even if an older client
        // still uploads the original broad file tags.
        let mut next = event("next", "next-session", 1_735_689_640_000, 1_000);
        next.genres = vec!["Rock".into(), "Pop".into()];
        fixture
            .store
            .ingest_events("u", "desktop", 1, &[next])
            .unwrap();
        let current = fixture
            .store
            .get_ledger_period("u", 2025, None)
            .unwrap()
            .unwrap();
        assert_eq!(current.genre_ms["Acid Jazz"], 10_334);
        assert_eq!(current.genre_ms["Electronic"], 10_333);
        assert_eq!(current.genre_ms["Funk"], 10_333);
        assert_eq!(current.genre_ms.values().sum::<u64>(), current.total_ms);

        let txn = fixture.store.db.begin_write().unwrap();
        txn.open_table(META)
            .unwrap()
            .remove(GENRE_DURATION_POLICY)
            .unwrap();
        txn.commit().unwrap();
        fixture.store.init_ledger().unwrap();
        let rebuilt = fixture
            .store
            .get_ledger_period("u", 2025, None)
            .unwrap()
            .unwrap();
        assert_eq!(rebuilt.genre_ms, current.genre_ms);
        cleanup(fixture);
    }

    #[test]
    fn empty_catalog_genres_remove_old_classification_and_sync_metadata_is_page_bounded() {
        let fixture = Fixture::new();
        let one = event("one", "one-session", 1_735_689_600_000, 30_000);
        let mut two = event("two", "two-session", 1_735_689_640_000, 30_000);
        two.track_id = "second-track".into();
        fixture
            .store
            .ingest_events("user", "desktop", 1, &[one, two])
            .unwrap();
        fixture
            .store
            .update_genre_metadata(
                "user",
                &HashMap::from([
                    ("track".into(), Vec::new()),
                    ("second-track".into(), vec!["Metal".into()]),
                ]),
            )
            .unwrap();
        let stats = fixture
            .store
            .get_ledger_period("user", 2025, None)
            .unwrap()
            .unwrap();
        assert_eq!(stats.total_ms, 60_000);
        assert_eq!(stats.genre_ms, HashMap::from([("Metal".into(), 30_000)]));
        let first = fixture.store.sync_events("user", 0, 1, true, None).unwrap();
        assert_eq!(
            first.genre_metadata,
            HashMap::from([("track".into(), Vec::new())])
        );
        let second = fixture
            .store
            .sync_events("user", first.next_cursor, 1, true, None)
            .unwrap();
        assert_eq!(
            second.genre_metadata,
            HashMap::from([("second-track".into(), vec!["Metal".into()])])
        );
        cleanup(fixture);
    }

    #[test]
    fn genre_shares_conserve_duration_and_deduplicate_case_and_whitespace() {
        let fixture = Fixture::new();
        let mut tagged = event("tagged", "tagged-session", 1_735_689_600_000, 30_005);
        tagged.genres = vec![
            " ROCK ".into(),
            "Pop".into(),
            "rock".into(),
            "Acid \t jazz".into(),
            "acid jazz".into(),
            " ".into(),
        ];
        let mut untagged = event("untagged", "untagged-session", 1_735_689_640_000, 10);
        untagged.genres.clear();
        fixture
            .store
            .ingest_events("user", "desktop", 1, &[tagged.clone(), untagged])
            .unwrap();
        let stats = fixture
            .store
            .get_period("user", 2025, None)
            .unwrap()
            .unwrap();
        assert_eq!(stats.total_ms, 30_015);
        assert_eq!(stats.genre_ms.len(), 3);
        assert_eq!(stats.genre_ms["Acid Jazz"], 10_002);
        assert_eq!(stats.genre_ms["Pop"], 10_002);
        assert_eq!(stats.genre_ms["Rock"], 10_001);
        assert_eq!(stats.genre_ms.values().sum::<u64>(), tagged.listened_ms);
        let page = fixture
            .store
            .sync_events("user", 0, 200, true, None)
            .unwrap();
        assert_eq!(page.events[0].event, tagged);
        cleanup(fixture);
    }

    #[test]
    fn genre_shares_keep_odd_remainders_consistent_across_year_boundary() {
        let fixture = Fixture::new();
        let mut boundary = event("boundary", "session", 1_735_689_599_999, 5);
        boundary.session_started_at_ms = boundary.started_at_ms;
        boundary.genres = vec!["Rock".into(), "Acid Jazz".into(), "Pop".into()];
        fixture
            .store
            .ingest_events("user", "desktop", 1, &[boundary])
            .unwrap();
        let december = fixture
            .store
            .get_period("user", 2024, Some(12))
            .unwrap()
            .unwrap();
        let january = fixture
            .store
            .get_period("user", 2025, Some(1))
            .unwrap()
            .unwrap();
        assert_eq!(december.total_ms, 1);
        assert_eq!(december.genre_ms, HashMap::from([("Acid Jazz".into(), 1)]));
        assert_eq!(january.total_ms, 4);
        assert_eq!(
            january.genre_ms,
            HashMap::from([
                ("Acid Jazz".into(), 1),
                ("Pop".into(), 2),
                ("Rock".into(), 1),
            ])
        );
        for (year, month) in [(2024, december), (2025, january)] {
            let annual = fixture
                .store
                .get_period("user", year, None)
                .unwrap()
                .unwrap();
            assert_eq!(annual.genre_ms, month.genre_ms);
            assert_eq!(annual.genre_ms.values().sum::<u64>(), annual.total_ms);
        }
        cleanup(fixture);
    }

    #[test]
    fn genre_cumulative_shares_telescope_for_every_chunk_and_empty_tags() {
        let genres = canonical_genres(&["Rock".into(), "Pop".into(), "Jazz".into()]);
        assert!(genre_duration_shares(&[], 0, 10).is_empty());
        assert!(genre_duration_shares(&genres, 3, 3).is_empty());
        for duration in 1..100 {
            let whole = genre_duration_shares(&genres, 0, duration);
            assert_eq!(whole.iter().map(|(_, share)| share).sum::<u64>(), duration);
            for cut in 0..=duration {
                let mut merged = HashMap::new();
                for (genre, share) in genre_duration_shares(&genres, 0, cut)
                    .into_iter()
                    .chain(genre_duration_shares(&genres, cut, duration))
                {
                    add_count(&mut merged, genre, share);
                }
                for (genre, share) in &whole {
                    assert_eq!(merged.get(*genre).copied().unwrap_or(0), *share);
                }
            }
        }
    }

    #[test]
    fn genre_migration_rebuilds_only_derived_genres_once_and_preserves_legacy() {
        let fixture = Fixture::new();
        let user = "u:sér";
        let mut tagged = event("tagged", "session", 1_735_689_599_999, 5);
        tagged.session_started_at_ms = tagged.started_at_ms;
        tagged.genres = vec!["Rock".into(), "Acid Jazz".into(), "Pop".into()];
        fixture
            .store
            .ingest_events(user, "desktop", 1, &[tagged])
            .unwrap();
        fixture
            .store
            .ingest_events(
                "other",
                "desktop",
                1,
                &[event("other", "other-session", 1_735_689_600_000, 30_000)],
            )
            .unwrap();
        fixture
            .store
            .set_device_preference(
                user,
                DevicePreference {
                    device_id: "desktop".into(),
                    enabled: false,
                    revision: 3,
                },
            )
            .unwrap();
        let before = fixture
            .store
            .get_ledger_period(user, 2025, None)
            .unwrap()
            .unwrap();
        let txn = fixture.store.db.begin_write().unwrap();
        {
            let mut meta = txn.open_table(META).unwrap();
            meta.remove(GENRE_DURATION_POLICY).unwrap();
            let mut periods = txn.open_table(PERIODS).unwrap();
            let entries: Vec<_> = periods
                .iter()
                .unwrap()
                .map(|item| {
                    let (key, value) = item.unwrap();
                    let mut stats: UserPeriodStats = serde_json::from_slice(value.value()).unwrap();
                    stats.genre_ms = HashMap::from([
                        ("Rock".into(), stats.total_ms),
                        ("Pop".into(), stats.total_ms),
                        ("Stale".into(), 50),
                    ]);
                    (key.value().to_owned(), serde_json::to_vec(&stats).unwrap())
                })
                .collect();
            for (key, bytes) in entries {
                periods.insert(key.as_str(), bytes.as_slice()).unwrap();
            }
            let mut legacy = txn.open_table(super::super::STATS_TABLE).unwrap();
            let baseline = UserPeriodStats {
                total_ms: 60_000,
                genre_ms: HashMap::from([("Legacy Rock".into(), 60_000)]),
                ..Default::default()
            };
            let bytes = bincode::serialize(&baseline).unwrap();
            legacy
                .insert(stats_key(user, 2025, None).as_str(), bytes.as_slice())
                .unwrap();
        }
        txn.commit().unwrap();
        let immutable_tables = [
            EVENTS,
            CHANGES,
            SESSIONS,
            TRACK_METADATA,
            ARTIST_METADATA,
            PREFERENCES,
            super::super::STATS_TABLE,
        ];
        let immutable_before: Vec<_> = immutable_tables
            .iter()
            .map(|table| table_contents(&fixture.store, *table))
            .collect();
        let meta_before = table_contents(&fixture.store, META);
        fixture.store.init_ledger().unwrap();
        let after = fixture
            .store
            .get_ledger_period(user, 2025, None)
            .unwrap()
            .unwrap();
        assert_eq!(after.total_ms, before.total_ms);
        assert_eq!(after.track_ms, before.track_ms);
        assert_eq!(after.artist_ms, before.artist_ms);
        assert_eq!(after.track_plays, before.track_plays);
        assert_eq!(after.genre_ms, before.genre_ms);
        for (table, expected) in immutable_tables.iter().zip(immutable_before) {
            assert_eq!(table_contents(&fixture.store, *table), expected);
        }
        let meta_after: Vec<_> = table_contents(&fixture.store, META)
            .into_iter()
            .filter(|(key, _)| key != GENRE_DURATION_POLICY)
            .collect();
        assert_eq!(meta_after, meta_before);
        let combined = fixture.store.get_period(user, 2025, None).unwrap().unwrap();
        assert_eq!(combined.total_ms, 60_004);
        assert_eq!(combined.genre_ms["Legacy Rock"], 60_000);
        assert_eq!(combined.genre_ms["Pop"], 2);
        assert!(!combined.genre_ms.contains_key("Stale"));
        assert_eq!(
            fixture
                .store
                .get_period("other", 2025, None)
                .unwrap()
                .unwrap()
                .genre_ms,
            HashMap::from([("Rock".into(), 30_000)])
        );
        let periods_after = table_contents(&fixture.store, PERIODS);
        fixture.store.init_ledger().unwrap();
        assert_eq!(table_contents(&fixture.store, PERIODS), periods_after);
        cleanup(fixture);
    }

    #[test]
    fn derived_migrations_flush_bounded_batches_and_reload_revisited_periods() {
        let fixture = Fixture::new();
        for user in ["user-a", "user:b"] {
            let mut history = Vec::new();
            // Each account has 520 period summaries, exceeding the cache limit
            // before the final event revisits its earliest month and year.
            for year in 1970..2010 {
                for month in 1..=12 {
                    let start = Date::from_calendar_date(year, Month::try_from(month).unwrap(), 1)
                        .unwrap()
                        .midnight()
                        .assume_utc()
                        .unix_timestamp_nanos()
                        / 1_000_000;
                    let id = format!("month-{year}-{month:02}");
                    let mut segment = event(&id, &id, start as i64, 60_005);
                    segment.session_started_at_ms = segment.started_at_ms;
                    segment.genres = vec!["Rock".into(), "Pop".into()];
                    history.push(segment);
                }
            }
            let mut revisit = event("zz-revisit", "zz-revisit", 0, 60_005);
            revisit.session_started_at_ms = 0;
            revisit.genres = vec!["Rock".into(), "Pop".into()];
            history.push(revisit);
            for batch in history.chunks(MAX_EVENT_BATCH) {
                let result = fixture
                    .store
                    .ingest_events(user, "desktop", 1, batch)
                    .unwrap();
                assert!(result.rejected.is_empty());
                assert_eq!(result.accepted.len(), batch.len());
            }
        }
        let expected: HashMap<String, serde_json::Value> = table_contents(&fixture.store, PERIODS)
            .into_iter()
            .map(|(key, bytes)| (key, serde_json::from_slice(&bytes).unwrap()))
            .collect();
        assert!(expected.len() > GENRE_MIGRATION_CACHE_LIMIT * 2);
        let immutable_events = table_contents(&fixture.store, EVENTS);
        let txn = fixture.store.db.begin_write().unwrap();
        {
            let mut meta = txn.open_table(META).unwrap();
            meta.remove(GENRE_DURATION_POLICY).unwrap();
            meta.remove(PRIMARY_ARTIST_POLICY).unwrap();
            let mut periods = txn.open_table(PERIODS).unwrap();
            for (key, value) in &expected {
                let mut stats: UserPeriodStats = serde_json::from_value(value.clone()).unwrap();
                stats.genre_ms = HashMap::from([("Stale".into(), u64::MAX)]);
                stats.artist_ms = HashMap::from([("Stale credit".into(), u64::MAX)]);
                let bytes = serde_json::to_vec(&stats).unwrap();
                periods.insert(key.as_str(), bytes.as_slice()).unwrap();
            }
        }
        txn.commit().unwrap();

        fixture.store.init_ledger().unwrap();
        let actual: HashMap<String, serde_json::Value> = table_contents(&fixture.store, PERIODS)
            .into_iter()
            .map(|(key, bytes)| (key, serde_json::from_slice(&bytes).unwrap()))
            .collect();
        assert_eq!(actual, expected);
        assert_eq!(table_contents(&fixture.store, EVENTS), immutable_events);
        for user in ["user-a", "user:b"] {
            let earliest = fixture
                .store
                .get_ledger_period(user, 1970, Some(1))
                .unwrap()
                .unwrap();
            assert_eq!(earliest.total_ms, 120_010);
            assert_eq!(earliest.track_plays["track"], 2);
            assert_eq!(earliest.genre_ms["Pop"], 60_006);
            assert_eq!(earliest.genre_ms["Rock"], 60_004);
        }
        // An account-only catalog repair must also reload periods after cache
        // flushes, while leaving another account's summaries byte-identical.
        let before_projection = table_contents(&fixture.store, PERIODS);
        fixture
            .store
            .update_genre_metadata(
                "user-a",
                &HashMap::from([("track".into(), vec!["Acid Jazz".into(), "Funk".into()])]),
            )
            .unwrap();
        let after_projection: HashMap<_, _> = table_contents(&fixture.store, PERIODS)
            .into_iter()
            .collect();
        for (key, before) in before_projection {
            let actual = &after_projection[&key];
            if stored_period_user(&key).unwrap() != "user-a" {
                assert_eq!(actual, &before);
                continue;
            }
            let mut expected: UserPeriodStats = serde_json::from_slice(&before).unwrap();
            expected.genre_ms = HashMap::from([
                ("Acid Jazz".into(), expected.genre_ms["Pop"]),
                ("Funk".into(), expected.genre_ms["Rock"]),
            ]);
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(actual).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
        }
        assert_eq!(table_contents(&fixture.store, EVENTS), immutable_events);
        cleanup(fixture);
    }

    #[test]
    fn retry_conflict_and_out_of_order_session_are_idempotent() {
        let fixture = Fixture::new();
        let first = event("first", "session", 1_735_689_600_000, 15_000);
        let second = event("second", "session", 1_735_689_615_000, 15_000);
        fixture
            .store
            .ingest_events("user", "desktop", 1, &[second.clone(), first.clone()])
            .unwrap();
        let retry = fixture
            .store
            .ingest_events("user", "desktop", 1, &[first.clone(), second])
            .unwrap();
        assert_eq!(retry.accepted.len(), 2);
        let mut conflicting = first;
        conflicting.title = "Different".into();
        assert_eq!(
            fixture
                .store
                .ingest_events("user", "desktop", 1, &[conflicting])
                .unwrap()
                .rejected
                .len(),
            1
        );
        let stats = fixture
            .store
            .get_period("user", 2025, None)
            .unwrap()
            .unwrap();
        assert_eq!(stats.total_ms, 30_000);
        assert_eq!(stats.track_plays["track"], 1);
        let page = fixture
            .store
            .sync_events("user", 0, 200, true, None)
            .unwrap();
        assert_eq!(page.next_cursor, 2);
        cleanup(fixture);
    }

    #[test]
    fn account_partition_device_disable_and_stale_preferences() {
        let fixture = Fixture::new();
        let event = event("same-id", "session", 1_735_689_600_000, 30_000);
        fixture
            .store
            .ingest_events("alice", "desktop", 1, &[event.clone()])
            .unwrap();
        fixture
            .store
            .ingest_events("bob", "desktop", 1, &[event.clone()])
            .unwrap();
        let off = DevicePreference {
            device_id: "desktop".into(),
            enabled: false,
            revision: 2,
        };
        fixture
            .store
            .set_device_preference("alice", off.clone())
            .unwrap();
        let stale = DevicePreference {
            device_id: "desktop".into(),
            enabled: true,
            revision: 1,
        };
        assert_eq!(
            fixture.store.set_device_preference("alice", stale).unwrap(),
            off
        );
        assert!(matches!(
            fixture
                .store
                .ingest_events("alice", "desktop", 1, &[event.clone()]),
            Err(StatsError::DeviceDisabled)
        ));
        fixture
            .store
            .ingest_events("bob", "desktop", 1, &[event])
            .unwrap();
        assert_eq!(
            fixture
                .store
                .sync_events("alice", 0, 200, false, Some("desktop"))
                .unwrap()
                .device_enabled,
            Some(false)
        );
        assert_eq!(
            fixture
                .store
                .sync_events("bob", 0, 200, true, None)
                .unwrap()
                .events
                .len(),
            1
        );
        cleanup(fixture);
    }

    #[test]
    fn late_year_boundary_and_cursor_pagination_preserve_totals() {
        let fixture = Fixture::new();
        let mut boundary = event("year", "session", 1_735_689_590_000, 20_000);
        boundary.session_started_at_ms = boundary.started_at_ms;
        fixture
            .store
            .ingest_events("user", "desktop", 1, &[boundary])
            .unwrap();
        let subsequent = event("next", "next-session", 1_735_689_630_000, 30_000);
        fixture
            .store
            .ingest_events("user", "desktop", 1, &[subsequent])
            .unwrap();
        assert_eq!(
            fixture
                .store
                .get_period("user", 2024, None)
                .unwrap()
                .unwrap()
                .total_ms,
            10_000
        );
        assert_eq!(
            fixture
                .store
                .get_period("user", 2025, None)
                .unwrap()
                .unwrap()
                .total_ms,
            40_000
        );
        let one = fixture.store.sync_events("user", 0, 1, true, None).unwrap();
        assert!(one.has_more);
        let two = fixture
            .store
            .sync_events("user", one.next_cursor, 1, true, None)
            .unwrap();
        assert!(!two.has_more);
        assert_eq!(two.next_cursor, 2);
        assert_eq!(one.server_id, two.server_id);
        assert!(matches!(
            fixture.store.ingest_events("user", "desktop", 2, &[]),
            Err(StatsError::GenerationMismatch)
        ));
        cleanup(fixture);
    }

    #[test]
    fn invalid_record_isolated_and_end_marker_never_adds_time() {
        let fixture = Fixture::new();
        let valid = event("valid", "session", 1_735_689_600_000, 20_000);
        let mut invalid = valid.clone();
        invalid.event_id = "bad".into();
        invalid.device_id = "other-device".into();
        let mut end = event("end", "session", 1_735_689_620_000, 0);
        end.end_reason = Some("finished".into());
        let result = fixture
            .store
            .ingest_events("user", "desktop", 1, &[invalid, valid, end.clone()])
            .unwrap();
        assert_eq!(result.accepted.len(), 2);
        assert_eq!(result.rejected.len(), 1);
        fixture
            .store
            .ingest_events("user", "desktop", 1, &[end])
            .unwrap();
        let stats = fixture
            .store
            .get_period("user", 2025, None)
            .unwrap()
            .unwrap();
        assert_eq!(stats.total_ms, 20_000);
        assert!(stats.track_plays.is_empty());
        assert_eq!(
            fixture
                .store
                .sync_events("user", 0, 200, true, None)
                .unwrap()
                .events
                .len(),
            2
        );
        cleanup(fixture);
    }

    #[test]
    fn device_partition_unknown_duration_and_short_track_threshold() {
        let fixture = Fixture::new();
        let mut desktop = event("desktop", "desktop-session", 1_735_689_600_000, 10_000);
        desktop.track_duration_ms = 20_000;
        let mut mobile = event("mobile", "mobile-session", 1_735_689_600_000, 30_000);
        mobile.device_id = "mobile".into();
        mobile.track_duration_ms = 0;
        fixture
            .store
            .set_device_preference(
                "user",
                DevicePreference {
                    device_id: "mobile".into(),
                    enabled: false,
                    revision: 1,
                },
            )
            .unwrap();
        fixture
            .store
            .ingest_events("user", "desktop", 1, &[desktop])
            .unwrap();
        assert!(matches!(
            fixture
                .store
                .ingest_events("user", "mobile", 1, &[mobile.clone()]),
            Err(StatsError::DeviceDisabled)
        ));
        fixture
            .store
            .set_device_preference(
                "user",
                DevicePreference {
                    device_id: "mobile".into(),
                    enabled: true,
                    revision: 2,
                },
            )
            .unwrap();
        fixture
            .store
            .ingest_events("user", "mobile", 1, &[mobile])
            .unwrap();
        let stats = fixture
            .store
            .get_period("user", 2025, None)
            .unwrap()
            .unwrap();
        assert_eq!(stats.total_ms, 40_000);
        assert_eq!(stats.track_plays["track"], 2);
        cleanup(fixture);
    }

    #[test]
    fn legacy_baseline_is_preserved_but_never_replicated() {
        let fixture = Fixture::new();
        let txn = fixture.store.db.begin_write().unwrap();
        {
            let mut table = txn.open_table(super::super::STATS_TABLE).unwrap();
            let mut legacy = UserPeriodStats {
                total_ms: 60_000,
                ..Default::default()
            };
            legacy.track_ms.insert("old-track".into(), 60_000);
            let bytes = bincode::serialize(&legacy).unwrap();
            table.insert("user:2025:00", bytes.as_slice()).unwrap();
        }
        txn.commit().unwrap();
        fixture
            .store
            .ingest_events(
                "user",
                "desktop",
                1,
                &[event("new", "session", 1_735_689_600_000, 30_000)],
            )
            .unwrap();
        assert_eq!(
            fixture
                .store
                .get_period("user", 2025, None)
                .unwrap()
                .unwrap()
                .total_ms,
            90_000
        );
        let page = fixture
            .store
            .sync_events("user", 0, 200, true, None)
            .unwrap();
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].event.event_id, "new");
        cleanup(fixture);
    }

    #[test]
    fn ledger_identity_and_acknowledged_records_survive_reopen() {
        let Fixture { store, path } = Fixture::new();
        let checkpoint = event("durable", "session", 1_735_689_600_000, 30_000);
        store
            .ingest_events("user", "desktop", 1, &[checkpoint.clone()])
            .unwrap();
        let identity = store
            .sync_events("user", 0, 200, true, None)
            .unwrap()
            .server_id;
        drop(store);
        let store = StatsStore::new(Arc::new(redb::Database::create(&path).unwrap()));
        store.init_tables().unwrap();
        assert_eq!(
            store
                .sync_events("user", 0, 200, true, None)
                .unwrap()
                .server_id,
            identity
        );
        assert_eq!(
            store
                .ingest_events("user", "desktop", 1, &[checkpoint])
                .unwrap()
                .accepted
                .len(),
            1
        );
        assert_eq!(
            store
                .get_period("user", 2025, None)
                .unwrap()
                .unwrap()
                .total_ms,
            30_000
        );
        drop(store);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn metadata_fallback_and_odd_duration_match_client_rules() {
        let fixture = Fixture::new();
        let mut first = event("one", "session", 1_735_689_600_000, 10_000);
        first.track_duration_ms = 20_001;
        first.artist_id.clear();
        first.genres = vec![" Rock ".into(), "Rock".into(), " ".into()];
        fixture
            .store
            .ingest_events("user", "desktop", 1, &[first.clone()])
            .unwrap();
        let stats = fixture
            .store
            .get_period("user", 2025, None)
            .unwrap()
            .unwrap();
        assert!(stats.track_plays.is_empty());
        assert_eq!(stats.artist_ms["Artist"], 10_000);
        assert_eq!(stats.genre_ms["Rock"], 10_000);
        assert_eq!(stats.genre_ms.len(), 1);
        let mut second = first;
        second.event_id = "two".into();
        second.started_at_ms += 10_000;
        second.ended_at_ms = second.started_at_ms + 1;
        second.listened_ms = 1;
        fixture
            .store
            .ingest_events("user", "desktop", 1, &[second.clone()])
            .unwrap();
        let stats = fixture
            .store
            .get_period("user", 2025, None)
            .unwrap()
            .unwrap();
        assert_eq!(stats.track_plays["track"], 1);
        second.event_id = "metadata-change".into();
        second.artist_name = "Changed artist".into();
        assert_eq!(
            fixture
                .store
                .ingest_events("user", "desktop", 1, &[second])
                .unwrap()
                .rejected
                .len(),
            1
        );
        cleanup(fixture);
    }
}
