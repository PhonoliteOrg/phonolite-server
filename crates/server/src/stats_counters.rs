//! Bounded synchronization state: monthly counters and one checkpoint per device.
//! No accepted upload, event ID, or checkpoint history is retained by protocol v2.
use super::*;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const ROWS: TableDefinition<&str, &[u8]> = TableDefinition::new("listening_counters_v2");
const SNAPSHOTS: TableDefinition<&str, &[u8]> = TableDefinition::new("listening_metadata_v2");
const DEVICES: TableDefinition<&str, &[u8]> = TableDefinition::new("listening_devices_v2");
const REVISIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("listening_revisions_v2");
const STATE: TableDefinition<&str, &[u8]> = TableDefinition::new("listening_state_v2");
pub const MAX_BATCH_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CounterCheckpoint {
    pub last_sequence: u64,
    #[serde(default)]
    pub last_hash: String,
    #[serde(default)]
    pub upgraded: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        store: Option<StatsStore>,
        path: std::path::PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let fixture = Self::legacy();
            fixture
                .store
                .as_ref()
                .unwrap()
                .activate_counters("user", "device", 1)
                .unwrap();
            fixture
        }
        fn legacy() -> Self {
            let path = std::env::temp_dir()
                .join(format!("phonolite-counters-{}.redb", uuid::Uuid::new_v4()));
            let store =
                StatsStore::new(std::sync::Arc::new(redb::Database::create(&path).unwrap()));
            store.init_tables().unwrap();
            Self {
                store: Some(store),
                path,
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.store.take();
            let _ = std::fs::remove_file(&self.path);
        }
    }
    fn batch(sequence: u64) -> CounterBatch {
        let value = json!({"track_id":"track","title":"Song","artist_id":"artist","artist_name":"Artist",
            "album_id":"album","album_title":"Album","genres":["Rock","Jazz"],"artists":[],"track_duration_ms":180000});
        let id = digest(&value).unwrap();
        assert_eq!(
            id,
            "e708f5b7c70a6ce7499354c4a0c9f7ad7357a58b16278e0cef3f6e86f40201da"
        );
        CounterBatch {
            device_id: "device".into(),
            history_generation: 1,
            collection_revision: 0,
            batch_sequence: sequence,
            rule_version: 3,
            metadata: [(id.clone(), value)].into_iter().collect(),
            contributions: vec![Contribution {
                year: 2026,
                month: 10,
                metadata_id: id,
                listened_ms: 15001,
                plays: 1,
                genre_ms: [("Jazz".into(), 7501), ("Rock".into(), 7500)]
                    .into_iter()
                    .collect(),
            }],
        }
    }
    #[test]
    fn retry_stale_gap_and_changed_payload_are_atomic() {
        let fixture = Fixture::new();
        let store = fixture.store.as_ref().unwrap();
        store.ingest_counters("user", &batch(1)).unwrap();
        store.ingest_counters("user", &batch(1)).unwrap();
        let mut changed = batch(1);
        changed.contributions[0].plays = 5;
        assert!(matches!(
            store.ingest_counters("user", &changed),
            Err(StatsError::Conflict(_))
        ));
        assert!(matches!(
            store.ingest_counters("user", &batch(3)),
            Err(StatsError::Conflict(_))
        ));
        store.ingest_counters("user", &batch(2)).unwrap();
        store.ingest_counters("user", &changed).unwrap(); // Older payloads cannot add counters.
        let period = store.get_period("user", 2026, Some(10)).unwrap().unwrap();
        assert_eq!(period.total_ms, 30002);
        assert_eq!(period.track_plays["track"], 2);
        assert_eq!(period.genre_ms.values().sum::<u64>(), period.total_ms);
        let mut invalid = batch(3);
        invalid.contributions[0].genre_ms.clear();
        assert!(store.ingest_counters("user", &invalid).is_err());
        assert_eq!(
            store
                .counter_state("user", "device", true)
                .unwrap()
                .checkpoint
                .last_sequence,
            2
        );
    }
    #[test]
    fn consent_barrier_and_reset_fence_delayed_batches() {
        let fixture = Fixture::new();
        let store = fixture.store.as_ref().unwrap();
        store
            .set_device_preference(
                "user",
                DevicePreference {
                    device_id: "device".into(),
                    enabled: false,
                    revision: 1,
                },
            )
            .unwrap();
        assert!(matches!(
            store.ingest_counters("user", &batch(1)),
            Err(StatsError::DeviceDisabled)
        ));
        store
            .set_device_preference(
                "user",
                DevicePreference {
                    device_id: "device".into(),
                    enabled: true,
                    revision: 2,
                },
            )
            .unwrap();
        assert!(matches!(
            store.ingest_counters("user", &batch(1)),
            Err(StatsError::Conflict(_))
        ));
        let mut fresh = batch(1);
        fresh.collection_revision = 2;
        store.ingest_counters("user", &fresh).unwrap();
        store.reset_history("user", 1).unwrap();
        assert!(matches!(
            store.ingest_counters("user", &fresh),
            Err(StatsError::GenerationMismatch)
        ));
        assert!(store.get_period("user", 2026, None).unwrap().is_none());
        let state = store.counter_state("user", "device", true).unwrap();
        assert_eq!(state.history_generation, 2);
        assert_eq!(state.preference_revision, 2);
    }
    #[test]
    fn snapshot_revision_prevents_mixed_pages_and_devices_sum() {
        let fixture = Fixture::new();
        let store = fixture.store.as_ref().unwrap();
        store.ingest_counters("user", &batch(1)).unwrap();
        let page = store
            .counter_snapshot("user", "device", 2026, 10, None, "", 1)
            .unwrap();
        store.activate_counters("user", "other", 1).unwrap();
        let mut other = batch(1);
        other.device_id = "other".into();
        store.ingest_counters("user", &other).unwrap();
        assert!(matches!(
            store.counter_snapshot(
                "user",
                "device",
                2026,
                10,
                Some(page.revision),
                &page.next_key,
                1
            ),
            Err(StatsError::Conflict(_))
        ));
        assert_eq!(
            store
                .get_period("user", 2026, None)
                .unwrap()
                .unwrap()
                .total_ms,
            30002
        );
        assert!(store
            .get_period("someone_else", 2026, None)
            .unwrap()
            .is_none());
    }
    #[test]
    fn checkpoint_count_does_not_increase_retained_rows() {
        let fixture = Fixture::new();
        let store = fixture.store.as_ref().unwrap();
        for sequence in 1..=1000 {
            store.ingest_counters("user", &batch(sequence)).unwrap();
        }
        let txn = store.db.begin_read().unwrap();
        assert_eq!(txn.open_table(ROWS).unwrap().len().unwrap(), 1);
        assert_eq!(txn.open_table(SNAPSHOTS).unwrap().len().unwrap(), 1);
        assert_eq!(txn.open_table(DEVICES).unwrap().len().unwrap(), 1);
        assert_eq!(txn.open_table(EVENTS).unwrap().len().unwrap(), 0);
        assert_eq!(txn.open_table(CHANGES).unwrap().len().unwrap(), 0);
    }
    #[test]
    fn legacy_migration_and_handoff_keep_totals_and_retire_old_writers() {
        let fixture = Fixture::legacy();
        let store = fixture.store.as_ref().unwrap();
        let mut event = event_from_metadata(batch(1).metadata.values().next().unwrap()).unwrap();
        event.device_id = "old".into();
        event.session_started_at_ms = 1_791_446_400_000;
        event.started_at_ms = event.session_started_at_ms;
        event.ended_at_ms = event.started_at_ms + 60000;
        event.listened_ms = 60000;
        store
            .ingest_events("user", "old", 1, &[event.clone()])
            .unwrap();
        // Recreate a pre-v2 database. Migration retains its original event IDs
        // until the old installation explicitly completes its handoff.
        let txn = store.db.begin_write().unwrap();
        for definition in [ROWS, SNAPSHOTS, REVISIONS, DEVICES, STATE] {
            let mut table = txn.open_table(definition).unwrap();
            for item in table.drain_filter::<&str, _>(.., |_, _| true).unwrap() {
                item.unwrap();
            }
        }
        txn.commit().unwrap();
        store.init_counters().unwrap();
        store.init_counters().unwrap();
        assert_eq!(
            store
                .get_period("user", 2026, None)
                .unwrap()
                .unwrap()
                .total_ms,
            60000
        );
        assert_eq!(
            store
                .get_period("user", 2026, None)
                .unwrap()
                .unwrap()
                .track_plays["track"],
            1
        );
        assert_eq!(
            store
                .sync_events("user", 0, 200, true, None)
                .unwrap()
                .events[0]
                .event,
            event
        );
        store.activate_counters("user", "new", 1).unwrap();
        assert!(store.sync_events("user", 0, 200, true, None).is_ok());
        // A lost v1 acknowledgment is reconciled using the old ID before activation.
        store
            .ingest_events("user", "old", 1, &[event.clone()])
            .unwrap();
        store.activate_counters("user", "old", 1).unwrap();
        assert!(matches!(
            store.ingest_events("user", "old", 1, &[event]),
            Err(StatsError::ProtocolRetired)
        ));
        assert!(matches!(
            store.sync_events("user", 0, 200, true, None),
            Err(StatsError::ProtocolRetired)
        ));
        assert_eq!(
            store
                .get_period("user", 2026, None)
                .unwrap()
                .unwrap()
                .total_ms,
            60000
        );
        let txn = store.db.begin_read().unwrap();
        assert_eq!(txn.open_table(EVENTS).unwrap().len().unwrap(), 0);
        assert_eq!(txn.open_table(SESSIONS).unwrap().len().unwrap(), 0);
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contribution {
    pub year: i32,
    pub month: u8,
    pub metadata_id: String,
    pub listened_ms: u64,
    pub plays: u64,
    #[serde(default)]
    pub genre_ms: BTreeMap<String, u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CounterBatch {
    pub device_id: String,
    pub history_generation: u64,
    pub collection_revision: u64,
    pub batch_sequence: u64,
    pub rule_version: u32,
    pub metadata: BTreeMap<String, Value>,
    pub contributions: Vec<Contribution>,
}
#[derive(Debug, Serialize)]
pub struct CounterMonth {
    pub year: i32,
    pub month: u8,
    pub revision: u64,
}
#[derive(Debug, Serialize)]
pub struct CounterState {
    pub server_id: String,
    pub user_id: String,
    pub history_generation: u64,
    pub collection_enabled: bool,
    pub device_enabled: bool,
    pub preference_revision: u64,
    pub checkpoint: CounterCheckpoint,
    pub months: Vec<CounterMonth>,
}
#[derive(Debug, Serialize)]
pub struct CounterSnapshot {
    pub history_generation: u64,
    pub year: i32,
    pub month: u8,
    pub revision: u64,
    pub checkpoint: CounterCheckpoint,
    pub contributions: Vec<Contribution>,
    pub metadata: BTreeMap<String, Value>,
    pub genre_metadata: BTreeMap<String, Vec<String>>,
    pub next_key: String,
    pub has_more: bool,
}
#[derive(Debug, Default, Serialize, Deserialize)]
struct Totals {
    listened_ms: u64,
    plays: u64,
    genre_ms: BTreeMap<String, u64>,
}

fn period_key(user: &str, year: i32, month: u8) -> String {
    scoped_key(user, &format!("{year:04}:{month:02}"))
}
fn digest(value: &impl Serialize) -> Result<String, StatsError> {
    // Value's object keys and BTreeMaps are sorted, independent of HTTP key order.
    let canonical = serde_json::to_value(value)?;
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&canonical)?)
    ))
}
fn metadata(event: &ListeningEvent) -> Value {
    json!({"track_id":event.track_id,"title":event.title,"artist_id":event.artist_id,
        "artist_name":event.artist_name,"album_id":event.album_id,"album_title":event.album_title,
        "genres":event.genres,"artists":event.artists,"track_duration_ms":event.track_duration_ms})
}
fn event_from_metadata(value: &Value) -> Result<ListeningEvent, StatsError> {
    let mut value = value.clone();
    let object = value
        .as_object_mut()
        .ok_or_else(|| StatsError::Invalid("invalid metadata".into()))?;
    for (key, item) in json!({"event_id":"snapshot","session_id":"snapshot","device_id":"snapshot",
        "session_started_at_ms":0,"started_at_ms":0,"ended_at_ms":1,"listened_ms":1,
        "rule_version":1})
    .as_object()
    .unwrap()
    {
        object.insert(key.clone(), item.clone());
    }
    let event: ListeningEvent = serde_json::from_value(value)
        .map_err(|_| StatsError::Invalid("invalid counter metadata fields".into()))?;
    validate_event(&event, "snapshot").map_err(StatsError::Invalid)?;
    Ok(event)
}
fn add(
    txn: &redb::WriteTransaction,
    user: &str,
    value: &Value,
    contribution: &Contribution,
) -> Result<(), StatsError> {
    let id = &contribution.metadata_id;
    {
        let mut table = txn.open_table(SNAPSHOTS)?;
        let key = scoped_key(user, id);
        if let Some(previous) = table.get(key.as_str())? {
            if serde_json::from_slice::<Value>(previous.value())? != *value {
                return Err(StatsError::Conflict("immutable metadata changed".into()));
            }
        }
        let bytes = serde_json::to_vec(value)?;
        table.insert(key.as_str(), bytes.as_slice())?;
    }
    let period = period_key(user, contribution.year, contribution.month);
    {
        let mut table = txn.open_table(ROWS)?;
        let key = format!("{period}:{id}");
        let mut totals: Totals = table
            .get(key.as_str())?
            .map(|v| serde_json::from_slice(v.value()))
            .transpose()?
            .unwrap_or_default();
        totals.listened_ms = totals
            .listened_ms
            .checked_add(contribution.listened_ms)
            .ok_or_else(|| StatsError::Invalid("counter overflow".into()))?;
        totals.plays = totals
            .plays
            .checked_add(contribution.plays)
            .ok_or_else(|| StatsError::Invalid("counter overflow".into()))?;
        for (genre, ms) in &contribution.genre_ms {
            let target = totals.genre_ms.entry(genre.clone()).or_default();
            *target = target
                .checked_add(*ms)
                .ok_or_else(|| StatsError::Invalid("counter overflow".into()))?;
        }
        let bytes = serde_json::to_vec(&totals)?;
        table.insert(key.as_str(), bytes.as_slice())?;
    }
    bump(txn, &period)?;
    Ok(())
}
fn bump(txn: &redb::WriteTransaction, key: &str) -> Result<(), StatsError> {
    let mut table = txn.open_table(REVISIONS)?;
    let previous: u64 = table
        .get(key)?
        .map(|v| serde_json::from_slice(v.value()))
        .transpose()?
        .unwrap_or(0);
    let next = previous
        .checked_add(1)
        .ok_or_else(|| StatsError::Invalid("revision exhausted".into()))?;
    let bytes = serde_json::to_vec(&next)?;
    table.insert(key, bytes.as_slice())?;
    Ok(())
}
pub(super) fn add_legacy(
    txn: &redb::WriteTransaction,
    user: &str,
    event: &ListeningEvent,
    qualified: bool,
) -> Result<(), StatsError> {
    let value = metadata(event);
    let id = digest(&value)?;
    let genres = canonical_genres(&event.genres);
    let mut offset = 0;
    for (year, month, listened_ms) in split_duration(event)? {
        let genre_ms = genre_duration_shares(&genres, offset, offset + listened_ms)
            .into_iter()
            .map(|(g, ms)| (g.into(), ms))
            .collect();
        offset += listened_ms;
        add(
            txn,
            user,
            &value,
            &Contribution {
                year,
                month,
                metadata_id: id.clone(),
                listened_ms,
                plays: 0,
                genre_ms,
            },
        )?;
    }
    if qualified {
        let at = datetime(event.session_started_at_ms)?;
        add(
            txn,
            user,
            &value,
            &Contribution {
                year: at.year(),
                month: at.month() as u8,
                metadata_id: id,
                listened_ms: 0,
                plays: 1,
                genre_ms: BTreeMap::new(),
            },
        )?;
    }
    Ok(())
}
pub(super) fn reset(txn: &redb::WriteTransaction, user: &str) -> Result<(), StatsError> {
    let prefix = account_prefix(user);
    for definition in [ROWS, SNAPSHOTS, DEVICES, REVISIONS, STATE] {
        let mut table = txn.open_table(definition)?;
        for removed in table.drain_filter::<&str, _>(.., |key, _| key.starts_with(&prefix))? {
            removed?;
        }
    }
    Ok(())
}
pub(super) fn genres_changed(txn: &redb::WriteTransaction, user: &str) -> Result<(), StatsError> {
    let prefix = account_prefix(user);
    let keys: Vec<String> = txn
        .open_table(REVISIONS)?
        .range(prefix.as_str()..)?
        .take_while(|r| {
            r.as_ref()
                .is_ok_and(|(k, _)| k.value().starts_with(&prefix))
        })
        .map(|r| r.map(|(k, _)| k.value().to_owned()))
        .collect::<Result<_, _>>()?;
    for key in keys {
        bump(txn, &key)?;
    }
    Ok(())
}
pub(super) fn legacy_retired(txn: &redb::ReadTransaction, user: &str) -> Result<bool, StatsError> {
    Ok(txn
        .open_table(STATE)?
        .get(scoped_key(user, "retired").as_str())?
        .is_some())
}
pub(super) fn check_legacy(
    txn: &redb::WriteTransaction,
    user: &str,
    device: &str,
) -> Result<(), StatsError> {
    let retired = txn
        .open_table(STATE)?
        .get(scoped_key(user, "retired").as_str())?
        .is_some();
    let checkpoint: CounterCheckpoint = txn
        .open_table(DEVICES)?
        .get(scoped_key(user, device).as_str())?
        .map(|v| serde_json::from_slice(v.value()))
        .transpose()?
        .unwrap_or_default();
    if retired || checkpoint.upgraded {
        return Err(StatsError::ProtocolRetired);
    }
    Ok(())
}
fn account_read(txn: &redb::ReadTransaction, user: &str) -> Result<(String, u64), StatsError> {
    let table = txn.open_table(META)?;
    let server = table
        .get("server_id")?
        .map(|v| String::from_utf8_lossy(v.value()).into_owned())
        .ok_or_else(|| StatsError::Invalid("missing stats identity".into()))?;
    let generation = table
        .get(scoped_key(user, "account").as_str())?
        .map(|v| serde_json::from_slice::<AccountState>(v.value()).map(|a| a.generation))
        .transpose()?
        .unwrap_or(1);
    Ok((server, generation))
}
fn checkpoint_read(
    txn: &redb::ReadTransaction,
    user: &str,
    device: &str,
) -> Result<CounterCheckpoint, StatsError> {
    Ok(txn
        .open_table(DEVICES)?
        .get(scoped_key(user, device).as_str())?
        .map(|v| serde_json::from_slice(v.value()))
        .transpose()?
        .unwrap_or_default())
}

impl StatsStore {
    pub(in super::super) fn init_counters(&self) -> Result<(), StatsError> {
        let txn = self.db.begin_write()?;
        for definition in [ROWS, SNAPSHOTS, DEVICES, REVISIONS, STATE] {
            txn.open_table(definition)?;
        }
        if txn.open_table(STATE)?.get("migrated")?.is_none() {
            // One transaction makes interruption/restart safe. Iterate database rows;
            // memory is independent of event count.
            let events = txn.open_table(EVENTS)?;
            for item in events.iter()? {
                let (key, value) = item?;
                let user = stored_event_user(key.value())?;
                let event: ListeningEvent = serde_json::from_slice(value.value())?;
                add_legacy(&txn, user, &event, false)?;
            }
            drop(events);
            let sessions = txn.open_table(SESSIONS)?;
            for item in sessions.iter()? {
                let (key, value) = item?;
                let session: SessionState = serde_json::from_slice(value.value())?;
                if !session.qualified {
                    continue;
                }
                let fields: Vec<Value> = serde_json::from_str(&session.metadata)?;
                let mut value = json!({"track_id":session.track_id,"track_duration_ms":session.track_duration_ms});
                for (key, index) in [
                    ("title", 0),
                    ("artist_id", 1),
                    ("artist_name", 2),
                    ("album_id", 3),
                    ("album_title", 4),
                    ("artists", 5),
                    ("genres", 6),
                ] {
                    value[key] = fields[index].clone();
                }
                let at = datetime(session.started_at_ms)?;
                add(
                    &txn,
                    stored_event_user(key.value())?,
                    &value,
                    &Contribution {
                        year: at.year(),
                        month: at.month() as u8,
                        metadata_id: digest(&value)?,
                        listened_ms: 0,
                        plays: 1,
                        genre_ms: BTreeMap::new(),
                    },
                )?;
            }
            drop(sessions);
            txn.open_table(STATE)?.insert("migrated", &b"1"[..])?;
        }
        txn.commit()?;
        Ok(())
    }
    pub fn counter_state(
        &self,
        user: &str,
        device: &str,
        enabled: bool,
    ) -> Result<CounterState, StatsError> {
        validate_id(device)?;
        let txn = self.db.begin_read()?;
        let (server_id, history_generation) = account_read(&txn, user)?;
        let pref: Option<DevicePreference> = txn
            .open_table(PREFERENCES)?
            .get(scoped_key(user, device).as_str())?
            .map(|v| serde_json::from_slice(v.value()))
            .transpose()?;
        let prefix = account_prefix(user);
        let mut months = Vec::new();
        for item in txn.open_table(REVISIONS)?.range(prefix.as_str()..)? {
            let (key, value) = item?;
            let Some(suffix) = key.value().strip_prefix(&prefix) else {
                break;
            };
            let (year, month) = suffix
                .split_once(':')
                .ok_or_else(|| StatsError::Invalid("invalid counter period".into()))?;
            months.push(CounterMonth {
                year: year
                    .parse()
                    .map_err(|_| StatsError::Invalid("invalid year".into()))?,
                month: month
                    .parse()
                    .map_err(|_| StatsError::Invalid("invalid month".into()))?,
                revision: serde_json::from_slice(value.value())?,
            });
        }
        Ok(CounterState {
            server_id,
            user_id: user.into(),
            history_generation,
            collection_enabled: enabled,
            device_enabled: pref.as_ref().is_none_or(|p| p.enabled),
            preference_revision: pref.map(|p| p.revision).unwrap_or(0),
            checkpoint: checkpoint_read(&txn, user, device)?,
            months,
        })
    }
    /// Called only after this installation has reconciled every legacy event ID.
    pub fn activate_counters(
        &self,
        user: &str,
        device: &str,
        generation: u64,
    ) -> Result<CounterCheckpoint, StatsError> {
        validate_id(device)?;
        let txn = self.db.begin_write()?;
        let current = txn
            .open_table(META)?
            .get(scoped_key(user, "account").as_str())?
            .map(|v| serde_json::from_slice::<AccountState>(v.value()).map(|a| a.generation))
            .transpose()?
            .unwrap_or(1);
        if current != generation {
            return Err(StatsError::GenerationMismatch);
        }
        let checkpoint;
        {
            let mut devices = txn.open_table(DEVICES)?;
            let key = scoped_key(user, device);
            let mut state: CounterCheckpoint = devices
                .get(key.as_str())?
                .map(|v| serde_json::from_slice(v.value()))
                .transpose()?
                .unwrap_or_default();
            state.upgraded = true;
            let bytes = serde_json::to_vec(&state)?;
            devices.insert(key.as_str(), bytes.as_slice())?;
            checkpoint = state;
        }
        let prefix = account_prefix(user);
        let mut all_upgraded = true;
        {
            let events = txn.open_table(EVENTS)?;
            let devices = txn.open_table(DEVICES)?;
            let mut checked = BTreeSet::new();
            for item in events.range(prefix.as_str()..)? {
                let (key, value) = item?;
                if !key.value().starts_with(&prefix) {
                    break;
                }
                let event: ListeningEvent = serde_json::from_slice(value.value())?;
                if !checked.insert(event.device_id.clone()) {
                    continue;
                }
                let upgraded = devices
                    .get(scoped_key(user, &event.device_id).as_str())?
                    .map(|v| {
                        serde_json::from_slice::<CounterCheckpoint>(v.value()).map(|c| c.upgraded)
                    })
                    .transpose()?
                    .unwrap_or(false);
                if !upgraded {
                    all_upgraded = false;
                    break;
                }
            }
        }
        if all_upgraded {
            for definition in [EVENTS, CHANGES, SESSIONS] {
                let mut table = txn.open_table(definition)?;
                for removed in
                    table.drain_filter::<&str, _>(.., |key, _| key.starts_with(&prefix))?
                {
                    removed?;
                }
            }
            let mut periods = txn.open_table(PERIODS)?;
            for removed in periods
                .drain_filter::<&str, _>(.., |key, _| stored_period_user(key).ok() == Some(user))?
            {
                removed?;
            }
            drop(periods);
            txn.open_table(STATE)?
                .insert(scoped_key(user, "retired").as_str(), &b"1"[..])?;
        }
        txn.commit()?;
        Ok(checkpoint)
    }
    pub fn ingest_counters(
        &self,
        user: &str,
        batch: &CounterBatch,
    ) -> Result<CounterCheckpoint, StatsError> {
        validate_id(&batch.device_id)?;
        if batch.rule_version != 3
            || batch.batch_sequence == 0
            || batch.contributions.is_empty()
            || batch.contributions.len() > 200
            || batch.metadata.len() > 200
            || serde_json::to_vec(batch)?.len() > MAX_BATCH_BYTES
        {
            return Err(StatsError::Invalid("invalid counter batch or size".into()));
        }
        let hash = digest(batch)?;
        let txn = self.db.begin_write()?;
        let generation = txn
            .open_table(META)?
            .get(scoped_key(user, "account").as_str())?
            .map(|v| serde_json::from_slice::<AccountState>(v.value()).map(|a| a.generation))
            .transpose()?
            .unwrap_or(1);
        if generation != batch.history_generation {
            return Err(StatsError::GenerationMismatch);
        }
        let pref: Option<DevicePreference> = txn
            .open_table(PREFERENCES)?
            .get(scoped_key(user, &batch.device_id).as_str())?
            .map(|v| serde_json::from_slice(v.value()))
            .transpose()?;
        if pref.as_ref().is_some_and(|p| !p.enabled) {
            return Err(StatsError::DeviceDisabled);
        }
        if pref.map(|p| p.revision).unwrap_or(0) != batch.collection_revision {
            return Err(StatsError::Conflict("collection revision changed".into()));
        }
        let key = scoped_key(user, &batch.device_id);
        let mut checkpoint: CounterCheckpoint = txn
            .open_table(DEVICES)?
            .get(key.as_str())?
            .map(|v| serde_json::from_slice(v.value()))
            .transpose()?
            .unwrap_or_default();
        if !checkpoint.upgraded {
            return Err(StatsError::Conflict("legacy handoff required".into()));
        }
        if batch.batch_sequence <= checkpoint.last_sequence {
            if batch.batch_sequence == checkpoint.last_sequence && checkpoint.last_hash != hash {
                return Err(StatsError::Conflict("last batch payload changed".into()));
            }
            return Ok(checkpoint);
        }
        if checkpoint.last_sequence.checked_add(1) != Some(batch.batch_sequence) {
            return Err(StatsError::Conflict("batch sequence gap".into()));
        }
        let first = &batch.contributions[0];
        let mut seen = BTreeSet::new();
        for row in &batch.contributions {
            if !(1970..=9998).contains(&row.year)
                || !(1..=12).contains(&row.month)
                || (row.year, row.month) != (first.year, first.month)
                || !seen.insert(&row.metadata_id)
                || row.listened_ms.checked_add(row.plays).is_none()
                || (row.listened_ms == 0 && row.plays == 0)
            {
                return Err(StatsError::Invalid("invalid monthly contribution".into()));
            }
            let value = batch
                .metadata
                .get(&row.metadata_id)
                .ok_or_else(|| StatsError::Invalid("missing metadata".into()))?;
            let event = event_from_metadata(value)?;
            if digest(value)? != row.metadata_id {
                return Err(StatsError::Invalid("metadata hash mismatch".into()));
            }
            let genres = canonical_genres(&event.genres);
            if row.genre_ms.keys().any(|g| !genres.contains(g))
                || row
                    .genre_ms
                    .values()
                    .try_fold(0_u64, |sum, ms| sum.checked_add(*ms))
                    != Some(if genres.is_empty() {
                        0
                    } else {
                        row.listened_ms
                    })
            {
                return Err(StatsError::Invalid(
                    "genre durations must conserve listening time".into(),
                ));
            }
            add(&txn, user, value, row)?;
            let bytes = serde_json::to_vec(&event)?;
            txn.open_table(TRACK_METADATA)?
                .insert(scoped_key(user, &event.track_id).as_str(), bytes.as_slice())?;
            if let Some((id, name)) = primary_artist(&event) {
                let bytes = serde_json::to_vec(name)?;
                txn.open_table(ARTIST_METADATA)?
                    .insert(scoped_key(user, id).as_str(), bytes.as_slice())?;
            }
        }
        checkpoint.last_sequence = batch.batch_sequence;
        checkpoint.last_hash = hash;
        let bytes = serde_json::to_vec(&checkpoint)?;
        txn.open_table(DEVICES)?
            .insert(key.as_str(), bytes.as_slice())?;
        txn.commit()?;
        Ok(checkpoint)
    }
    pub fn counter_snapshot(
        &self,
        user: &str,
        device: &str,
        year: i32,
        month: u8,
        expected: Option<u64>,
        after: &str,
        limit: usize,
    ) -> Result<CounterSnapshot, StatsError> {
        let txn = self.db.begin_read()?;
        self.counter_snapshot_in(&txn, user, device, year, month, expected, after, limit)
    }
    fn counter_snapshot_in(
        &self,
        txn: &redb::ReadTransaction,
        user: &str,
        device: &str,
        year: i32,
        month: u8,
        expected: Option<u64>,
        after: &str,
        limit: usize,
    ) -> Result<CounterSnapshot, StatsError> {
        validate_id(device)?;
        if !(1970..=9998).contains(&year) || !(1..=12).contains(&month) || after.len() > 64 {
            return Err(StatsError::Invalid("invalid snapshot query".into()));
        }
        let (_, history_generation) = account_read(&txn, user)?;
        let key = period_key(user, year, month);
        let revision = txn
            .open_table(REVISIONS)?
            .get(key.as_str())?
            .map(|v| serde_json::from_slice(v.value()))
            .transpose()?
            .unwrap_or(0);
        if expected.is_some_and(|expected| expected != revision) {
            return Err(StatsError::Conflict(
                "month changed; restart snapshot".into(),
            ));
        }
        let prefix = format!("{key}:");
        let start = format!("{prefix}{after}");
        let rows = txn.open_table(ROWS)?;
        let snapshots = txn.open_table(SNAPSHOTS)?;
        let genre_table = txn.open_table(GENRE_METADATA)?;
        let mut page = CounterSnapshot {
            history_generation,
            year,
            month,
            revision,
            checkpoint: checkpoint_read(&txn, user, device)?,
            contributions: Vec::new(),
            metadata: BTreeMap::new(),
            genre_metadata: BTreeMap::new(),
            next_key: after.into(),
            has_more: false,
        };
        let mut size = 0;
        for item in rows.range(start.as_str()..)? {
            let (key, value) = item?;
            let Some(id) = key.value().strip_prefix(&prefix) else {
                break;
            };
            if id <= after {
                continue;
            }
            if page.contributions.len() >= limit.clamp(1, 200) || size >= MAX_BATCH_BYTES / 2 {
                page.has_more = true;
                break;
            }
            let totals: Totals = serde_json::from_slice(value.value())?;
            let value: Value = snapshots
                .get(scoped_key(user, id).as_str())?
                .map(|v| serde_json::from_slice(v.value()))
                .transpose()?
                .ok_or_else(|| StatsError::Invalid("missing counter metadata".into()))?;
            size += serde_json::to_vec(&value)?.len() + 2048;
            let track = value["track_id"].as_str().unwrap_or_default();
            if let Some(genres) = genre_table.get(scoped_key(user, track).as_str())? {
                page.genre_metadata
                    .insert(track.into(), serde_json::from_slice(genres.value())?);
            }
            page.metadata.insert(id.into(), value);
            page.contributions.push(Contribution {
                year,
                month,
                metadata_id: id.into(),
                listened_ms: totals.listened_ms,
                plays: totals.plays,
                genre_ms: totals.genre_ms,
            });
            page.next_key = id.into();
        }
        Ok(page)
    }
    pub(in super::super) fn get_counter_period(
        &self,
        user: &str,
        year: i32,
        month: Option<u8>,
    ) -> Result<Option<UserPeriodStats>, StatsError> {
        let txn = self.db.begin_read()?;
        let mut result = UserPeriodStats::default();
        let mut found = false;
        for month in month.map(|m| vec![m]).unwrap_or_else(|| (1..=12).collect()) {
            let mut after = String::new();
            loop {
                let page =
                    self.counter_snapshot_in(&txn, user, "report", year, month, None, &after, 200)?;
                for row in &page.contributions {
                    found = true;
                    let event = event_from_metadata(&page.metadata[&row.metadata_id])?;
                    result.total_ms = result.total_ms.saturating_add(row.listened_ms);
                    if row.listened_ms > 0 {
                        *result.track_ms.entry(event.track_id.clone()).or_default() +=
                            row.listened_ms;
                    }
                    if row.plays > 0 {
                        *result
                            .track_plays
                            .entry(event.track_id.clone())
                            .or_default() += row.plays;
                    }
                    if row.listened_ms > 0 {
                        if let Some((id, _)) = primary_artist(&event) {
                            *result.artist_ms.entry(id.into()).or_default() += row.listened_ms;
                        }
                    }
                    let genres = page
                        .genre_metadata
                        .get(&event.track_id)
                        .cloned()
                        .unwrap_or_else(|| canonical_genres(&event.genres));
                    let shares = if genres == canonical_genres(&event.genres) {
                        row.genre_ms.clone()
                    } else {
                        genre_duration_shares(&genres, 0, row.listened_ms)
                            .into_iter()
                            .map(|(g, ms)| (g.into(), ms))
                            .collect()
                    };
                    for (genre, ms) in shares {
                        if ms > 0 {
                            *result.genre_ms.entry(genre).or_default() += ms;
                        }
                    }
                }
                if !page.has_more {
                    break;
                }
                after = page.next_key;
            }
        }
        Ok(found.then_some(result))
    }
}
