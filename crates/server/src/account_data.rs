use super::*;

const PRESETS: TableDefinition<&str, &[u8]> = TableDefinition::new("shuffle_presets_v1");
const PRESET_REVISIONS: TableDefinition<&str, u64> =
    TableDefinition::new("shuffle_preset_revisions_v1");
const LAST_TRACK: TableDefinition<&str, &[u8]> = TableDefinition::new("account_last_track_v1");
const TRACK_SEQUENCES: TableDefinition<&str, &[u8]> =
    TableDefinition::new("account_track_sequences_v1");

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShufflePreset {
    pub id: String,
    pub name: String,
    pub artist_ids: Vec<String>,
    pub genres: Vec<String>,
    pub revision: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ShufflePresets {
    pub revision: u64,
    pub items: Vec<ShufflePreset>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LastTrack {
    pub track_id: Option<String>,
    pub revision: u64,
}

#[derive(Serialize, Deserialize)]
struct TrackSequence {
    sequence: u64,
    request_id: String,
    track_id: String,
    #[serde(default)]
    updated_at: u64,
}

pub(super) fn init_tables(txn: &redb::WriteTransaction) -> Result<(), UserDataError> {
    txn.open_table(PRESETS)?;
    txn.open_table(PRESET_REVISIONS)?;
    txn.open_table(LAST_TRACK)?;
    txn.open_table(TRACK_SEQUENCES)?;
    Ok(())
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, UserDataError> {
    serde_json::to_vec(value).map_err(|err| UserDataError::Invalid(err.to_string()))
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, UserDataError> {
    serde_json::from_slice(bytes).map_err(|err| UserDataError::Invalid(err.to_string()))
}

fn next_revision(txn: &redb::WriteTransaction, user: &str) -> Result<u64, UserDataError> {
    let mut table = txn.open_table(PRESET_REVISIONS)?;
    let previous = table.get(user)?.map(|value| value.value()).unwrap_or(0);
    let revision = previous
        .checked_add(1)
        .ok_or(UserDataError::RevisionExhausted)?;
    table.insert(user, revision)?;
    Ok(revision)
}

fn normalized(values: Vec<String>, lower: bool) -> Result<Vec<String>, UserDataError> {
    if values.len() > 20_000 {
        return Err(UserDataError::Invalid("too many preset filters".into()));
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for value in values {
        let value = if lower {
            value.trim().to_lowercase()
        } else {
            value.trim().to_owned()
        };
        if value.len() > 1024 {
            return Err(UserDataError::Invalid("preset filter too long".into()));
        }
        if !value.is_empty() && seen.insert(value.clone()) {
            out.push(value);
        }
    }
    Ok(out)
}

impl UserDataStore {
    pub fn presets_revision(&self, user: &str) -> Result<u64, UserDataError> {
        let txn = self.db.begin_read()?;
        let revision = txn
            .open_table(PRESET_REVISIONS)?
            .get(user)?
            .map(|value| value.value())
            .unwrap_or(0);
        Ok(revision)
    }

    pub fn shuffle_presets(&self, user: &str) -> Result<ShufflePresets, UserDataError> {
        let txn = self.db.begin_read()?;
        let revision = txn
            .open_table(PRESET_REVISIONS)?
            .get(user)?
            .map(|value| value.value())
            .unwrap_or(0);
        let table = txn.open_table(PRESETS)?;
        let prefix = user_prefix(user);
        let mut items = Vec::new();
        for row in table.range(prefix.as_str()..)? {
            let (key, bytes) = row?;
            if !key.value().starts_with(&prefix) {
                break;
            }
            items.push(decode::<ShufflePreset>(bytes.value())?);
        }
        items.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then(a.id.cmp(&b.id))
        });
        Ok(ShufflePresets { revision, items })
    }

    pub fn save_shuffle_preset(
        &self,
        user: &str,
        id: Option<&str>,
        name: String,
        artists: Vec<String>,
        genres: Vec<String>,
        expected: Option<u64>,
    ) -> Result<Option<ShufflePreset>, UserDataError> {
        let name = name.trim().to_owned();
        if name.is_empty() || name.chars().count() > 80 {
            return Err(UserDataError::Invalid(
                "preset name must be 1 to 80 characters".into(),
            ));
        }
        let artists = normalized(artists, false)?;
        let genres = normalized(genres, true)?;
        let txn = ::library::begin_write(&self.db)?;
        let id = match id {
            Some(id) => {
                let key = user_key(user, id);
                let table = txn.open_table(PRESETS)?;
                let existing = match table.get(key.as_str())? {
                    Some(value) => decode::<ShufflePreset>(value.value())?,
                    None => return Ok(None),
                };
                if expected != Some(existing.revision) {
                    return Err(UserDataError::PresetConflict);
                }
                if existing.name == name
                    && existing.artist_ids == artists
                    && existing.genres == genres
                {
                    return Ok(Some(existing));
                }
                id.to_owned()
            }
            None => Uuid::new_v4().to_string(),
        };
        let revision = next_revision(&txn, user)?;
        let preset = ShufflePreset {
            id,
            name,
            artist_ids: artists,
            genres,
            revision,
        };
        txn.open_table(PRESETS)?.insert(
            user_key(user, &preset.id).as_str(),
            encode(&preset)?.as_slice(),
        )?;
        txn.commit()?;
        self.notify_account_change(user, revision);
        Ok(Some(preset))
    }

    pub fn delete_shuffle_preset(
        &self,
        user: &str,
        id: &str,
        expected: u64,
    ) -> Result<bool, UserDataError> {
        let txn = ::library::begin_write(&self.db)?;
        let key = user_key(user, id);
        {
            let mut table = txn.open_table(PRESETS)?;
            let existing = match table.get(key.as_str())? {
                Some(value) => decode::<ShufflePreset>(value.value())?,
                None => return Ok(false),
            };
            if existing.revision != expected {
                return Err(UserDataError::PresetConflict);
            }
            table.remove(key.as_str())?;
        }
        let revision = next_revision(&txn, user)?;
        txn.commit()?;
        self.notify_account_change(user, revision);
        Ok(true)
    }

    pub fn last_track(&self, user: &str) -> Result<LastTrack, UserDataError> {
        let txn = self.db.begin_read()?;
        let value = txn
            .open_table(LAST_TRACK)?
            .get(user)?
            .map(|value| decode(value.value()))
            .transpose()?
            .unwrap_or_default();
        Ok(value)
    }

    /// Live successful starts only. Client clocks, positions, stops and pauses do
    /// not order this record. The server's commit order resolves concurrent starts.
    pub fn record_track_start(
        &self,
        user: &str,
        track: &str,
        session: &str,
        sequence: u64,
        request: &str,
    ) -> Result<LastTrack, UserDataError> {
        let txn = ::library::begin_write(&self.db)?;
        let (latest, changed) =
            record_track_start_in(&txn, user, track, session, sequence, request)?;
        let playback_revision = if changed {
            Some(super::playback_data::legacy_start_in(&txn, user, track)?)
        } else {
            None
        };
        txn.commit()?;
        if let Some(revision) = playback_revision {
            self.notify_account_change(user, revision);
        }
        Ok(latest)
    }
}

pub(super) fn purge_playback_in(
    txn: &redb::WriteTransaction,
    user: &str,
) -> Result<(), UserDataError> {
    txn.open_table(LAST_TRACK)?.remove(user)?;
    let prefix = user_prefix(user);
    let mut table = txn.open_table(TRACK_SEQUENCES)?;
    let mut keys = Vec::new();
    for row in table.range(prefix.as_str()..)? {
        let (key, _) = row?;
        if !key.value().starts_with(&prefix) {
            break;
        }
        keys.push(key.value().to_owned());
    }
    for key in keys {
        table.remove(key.as_str())?;
    }
    Ok(())
}

pub(super) fn clear_last_track_in(
    txn: &redb::WriteTransaction,
    user: &str,
) -> Result<(), UserDataError> {
    let mut latest: LastTrack = txn
        .open_table(LAST_TRACK)?
        .get(user)?
        .map(|value| decode(value.value()))
        .transpose()?
        .unwrap_or_default();
    latest.track_id = None;
    latest.revision = latest
        .revision
        .checked_add(1)
        .ok_or(UserDataError::RevisionExhausted)?;
    txn.open_table(LAST_TRACK)?
        .insert(user, encode(&latest)?.as_slice())?;
    Ok(())
}

pub(super) fn record_track_start_in(
    txn: &redb::WriteTransaction,
    user: &str,
    track: &str,
    session: &str,
    sequence: u64,
    request: &str,
) -> Result<(LastTrack, bool), UserDataError> {
    super::playback_data::ensure_live_in(txn, user)?;
    if sequence == 0
        || [track, session, request]
            .iter()
            .any(|value| value.is_empty() || value.len() > 256)
    {
        return Err(UserDataError::Invalid("invalid track start report".into()));
    }
    let key = user_key(user, session);
    let mut latest: LastTrack = txn
        .open_table(LAST_TRACK)?
        .get(user)?
        .map(|value| decode(value.value()))
        .transpose()?
        .unwrap_or_default();
    if let Some(previous) = txn.open_table(TRACK_SEQUENCES)?.get(key.as_str())? {
        let previous: TrackSequence = decode(previous.value())?;
        if sequence <= previous.sequence {
            if sequence == previous.sequence
                && (request != previous.request_id || track != previous.track_id)
            {
                return Err(UserDataError::Invalid(
                    "track start sequence already used".into(),
                ));
            }
            return Ok((latest, false));
        }
    }
    latest.track_id = Some(track.to_owned());
    latest.revision = latest
        .revision
        .checked_add(1)
        .ok_or(UserDataError::RevisionExhausted)?;
    txn.open_table(LAST_TRACK)?
        .insert(user, encode(&latest)?.as_slice())?;
    txn.open_table(TRACK_SEQUENCES)?.insert(
        key.as_str(),
        encode(&TrackSequence {
            sequence,
            request_id: request.to_owned(),
            track_id: track.to_owned(),
            updated_at: now_millis(),
        })?
        .as_slice(),
    )?;
    // Runtime session nonces are intentionally fresh. Keep bounded retry
    // history while preserving the current writer even on timestamp ties.
    let prefix = user_prefix(user);
    let mut writers = Vec::new();
    for row in txn.open_table(TRACK_SEQUENCES)?.range(prefix.as_str()..)? {
        let (candidate, bytes) = row?;
        if !candidate.value().starts_with(&prefix) {
            break;
        }
        writers.push((
            candidate.value().to_owned(),
            decode::<TrackSequence>(bytes.value())?.updated_at,
        ));
    }
    writers.sort_by_key(|(candidate, updated)| (candidate == &key, *updated));
    let remove = writers.len().saturating_sub(128);
    let mut sequences = txn.open_table(TRACK_SEQUENCES)?;
    for (candidate, _) in writers.into_iter().take(remove) {
        sequences.remove(candidate.as_str())?;
    }
    Ok((latest, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        store: Option<UserDataStore>,
        path: std::path::PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("phonolite-account-data-{}.redb", Uuid::new_v4()));
            let store = UserDataStore::new(Arc::new(redb::Database::create(&path).unwrap()));
            store.init_tables().unwrap();
            Self {
                store: Some(store),
                path,
            }
        }
        fn store(&self) -> &UserDataStore {
            self.store.as_ref().unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.store.take();
            let _ = std::fs::remove_file(&self.path);
        }
    }
    #[test]
    fn presets_are_scoped_versioned_and_do_not_touch_collection_records() {
        let f = Fixture::new();
        let store = f.store();
        let preset = store
            .save_shuffle_preset(
                "alice",
                None,
                " Evening ".into(),
                vec!["artist".into(), "artist".into()],
                vec![" Rock ".into()],
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(preset.name, "Evening");
        assert_eq!(preset.artist_ids, ["artist"]);
        assert_eq!(store.presets_revision("alice").unwrap(), 1);
        assert_eq!(store.collection_revision("alice").unwrap(), 0);
        assert!(store.shuffle_presets("bob").unwrap().items.is_empty());
        assert!(store
            .save_shuffle_preset(
                "bob",
                Some(&preset.id),
                "Stolen".into(),
                vec![],
                vec![],
                Some(preset.revision)
            )
            .unwrap()
            .is_none());
        assert!(!store
            .delete_shuffle_preset("bob", &preset.id, preset.revision)
            .unwrap());
        let saved = store
            .save_shuffle_preset(
                "alice",
                Some(&preset.id),
                "Changed".into(),
                vec![],
                vec![],
                Some(preset.revision),
            )
            .unwrap()
            .unwrap();
        assert_eq!(saved.revision, 2);
        assert!(matches!(
            store.save_shuffle_preset(
                "alice",
                Some(&preset.id),
                "Stale".into(),
                vec![],
                vec![],
                Some(preset.revision)
            ),
            Err(UserDataError::PresetConflict)
        ));
        assert!(matches!(
            store.delete_shuffle_preset("alice", &preset.id, preset.revision),
            Err(UserDataError::PresetConflict)
        ));
        assert_eq!(
            store.shuffle_presets("alice").unwrap().items[0].name,
            "Changed"
        );
        assert!(store
            .delete_shuffle_preset("alice", &preset.id, saved.revision)
            .unwrap());
        assert_eq!(store.presets_revision("alice").unwrap(), 3);
    }
    #[test]
    fn track_start_retries_stale_requests_and_concurrent_sessions_use_commit_order() {
        let f = Fixture::new();
        let s = f.store();
        assert_eq!(s.last_track("alice").unwrap(), LastTrack::default());
        assert_eq!(
            s.record_track_start("alice", "one", "session-a", 1, "a1")
                .unwrap()
                .revision,
            1
        );
        assert_eq!(
            s.record_track_start("alice", "two", "session-a", 2, "a2")
                .unwrap()
                .revision,
            2
        );
        assert_eq!(
            s.record_track_start("alice", "other-device", "session-b", 1, "b1")
                .unwrap()
                .revision,
            3
        );
        assert_eq!(
            s.record_track_start("alice", "two", "session-a", 2, "a2")
                .unwrap()
                .track_id
                .as_deref(),
            Some("other-device")
        );
        assert_eq!(
            s.record_track_start("alice", "one", "session-a", 1, "a1")
                .unwrap()
                .revision,
            3
        );
        assert!(s
            .record_track_start("alice", "wrong", "session-a", 2, "a2")
            .is_err());
        assert_eq!(
            s.record_track_start("alice", "new", "session-a", 3, "a3")
                .unwrap()
                .revision,
            4
        );
        assert_eq!(s.last_track("bob").unwrap(), LastTrack::default());
        assert_eq!(s.collection_revision("alice").unwrap(), 0);
        assert_eq!(s.presets_revision("alice").unwrap(), 0);
    }
    #[test]
    fn invalid_filters_and_starts_never_advance_durable_state() {
        let f = Fixture::new();
        let s = f.store();
        assert!(s
            .save_shuffle_preset("alice", None, "  ".into(), vec![], vec![], None)
            .is_err());
        assert!(s
            .record_track_start("alice", "track", "session", 0, "request")
            .is_err());
        assert_eq!(s.presets_revision("alice").unwrap(), 0);
        assert_eq!(s.last_track("alice").unwrap().revision, 0);
    }
}
