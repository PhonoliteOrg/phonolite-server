//! Durable account resume suggestions. These records never control a player.
use super::*;

const PLAYBACK: TableDefinition<&str, &[u8]> = TableDefinition::new("account_playback_v1");
const WRITERS: TableDefinition<&str, &[u8]> = TableDefinition::new("account_playback_writers_v1");
const DECKS: TableDefinition<&str, &[u8]> = TableDefinition::new("account_playback_decks_v1");
const CHUNKS: TableDefinition<&str, &[u8]> =
    TableDefinition::new("account_playback_deck_chunks_v1");
const DELETED: TableDefinition<&str, u64> =
    TableDefinition::new("account_playback_deleted_users_v1");
const CHUNK_SIZE: usize = 256;
const MAX_DECK_IDS: usize = 1_000_000;
const MAX_ACCOUNT_IDS: usize = 2_000_000;
const MAX_DECKS: usize = 4;
const MAX_WRITERS: usize = 64;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlaybackSource {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default)]
    pub artist_ids: Vec<String>,
    #[serde(default)]
    pub genres: Vec<String>,
}
impl PlaybackSource {
    fn normalize(&mut self) {
        self.artist_ids.sort();
        self.artist_ids.dedup();
        self.genres = self
            .genres
            .iter()
            .map(|s| s.trim().to_lowercase())
            .collect();
        self.genres.sort();
        self.genres.dedup();
    }
    pub fn standalone() -> Self {
        Self {
            kind: "standalone".into(),
            id: None,
            artist_ids: vec![],
            genres: vec![],
        }
    }
    pub fn validate(&self) -> Result<(), UserDataError> {
        if !matches!(
            self.kind.as_str(),
            "standalone" | "library" | "liked" | "playlist" | "album" | "artist" | "custom"
        ) {
            return Err(invalid("invalid playback source"));
        }
        let needs_id = matches!(self.kind.as_str(), "playlist" | "album" | "artist");
        if needs_id != self.id.is_some() || self.id.as_ref().is_some_and(|id| !identifier(id)) {
            return Err(invalid("invalid playback source identifier"));
        }
        if self.artist_ids.len() > 128
            || self.genres.len() > 128
            || self
                .artist_ids
                .iter()
                .chain(&self.genres)
                .any(|id| !identifier(id))
            || (self.kind != "custom" && (!self.artist_ids.is_empty() || !self.genres.is_empty()))
        {
            return Err(invalid("invalid playback source filters"));
        }
        if self.kind == "custom" && self.artist_ids.is_empty() && self.genres.is_empty() {
            return Err(invalid("custom playback source requires filters"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlaybackCheckpoint {
    pub track_id: String,
    pub source: PlaybackSource,
    pub shuffle_mode: String,
    pub shuffle_context: PlaybackSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deck_id: Option<String>,
    #[serde(default)]
    pub cursor: usize,
}
impl PlaybackCheckpoint {
    pub fn standalone(track_id: String) -> Self {
        Self {
            track_id,
            source: PlaybackSource::standalone(),
            shuffle_mode: "off".into(),
            shuffle_context: PlaybackSource::standalone(),
            deck_id: None,
            cursor: 0,
        }
    }
    pub fn validate(&self) -> Result<(), UserDataError> {
        self.source.validate()?;
        self.shuffle_context.validate()?;
        let active = match self.shuffle_mode.as_str() {
            "off" => false,
            "all" | "artist" | "album" | "custom" | "liked" | "current_playlist" => true,
            _ => return Err(invalid("invalid shuffle mode")),
        };
        if !identifier(&self.track_id)
            || self.cursor >= MAX_DECK_IDS
            || self.deck_id.as_ref().is_some_and(|id| !identifier(id))
            || (active && self.shuffle_context.kind == "standalone")
            || (!active && self.deck_id.is_some())
            || (active && self.deck_id.is_none())
        {
            return Err(invalid("invalid playback checkpoint"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlaybackSnapshot {
    pub revision: u64,
    pub generation: Option<String>,
    pub checkpoint: Option<PlaybackCheckpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlaybackWrite {
    pub expected_revision: u64,
    pub session_id: String,
    pub sequence: u64,
    pub request_id: String,
    pub intent: String,
    #[serde(default)]
    pub generation: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    pub checkpoint: PlaybackCheckpoint,
}
impl PlaybackWrite {
    fn normalized(&self) -> Result<Self, UserDataError> {
        let mut input = self.clone();
        input.checkpoint.source.normalize();
        input.checkpoint.shuffle_context.normalize();
        input.validate()?;
        Ok(input)
    }

    pub fn validate(&self) -> Result<(), UserDataError> {
        self.checkpoint.validate()?;
        if !identifier(&self.session_id)
            || !identifier(&self.request_id)
            || self.sequence == 0
            || !matches!(self.intent.as_str(), "start" | "update")
            || self.client_id.as_ref().is_some_and(|id| !identifier(id))
            || (self.intent == "update"
                && self.generation.as_ref().is_none_or(|id| !identifier(id)))
        {
            return Err(invalid("invalid playback write"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DurablePlaybackDeck {
    pub id: String,
    pub source: PlaybackSource,
    pub total: usize,
    pub created_at: u64,
}
#[derive(Clone)]
pub struct PlaybackDeckImport {
    pub id: String,
    pub source: PlaybackSource,
    pub track_ids: Vec<String>,
}
#[derive(Serialize, Deserialize)]
struct Writer {
    sequence: u64,
    request_id: String,
    fingerprint: String,
    updated_at: u64,
}
#[derive(Debug)]
pub enum PlaybackStoreError {
    Data(UserDataError),
    Conflict(PlaybackSnapshot),
    NotFound,
}
impl From<UserDataError> for PlaybackStoreError {
    fn from(value: UserDataError) -> Self {
        Self::Data(value)
    }
}
type StoreResult<T> = Result<T, PlaybackStoreError>;
fn invalid(message: &str) -> UserDataError {
    UserDataError::Invalid(message.into())
}
fn identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && value.trim() == value
}
fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, UserDataError> {
    serde_json::to_vec(value).map_err(|e| invalid(&e.to_string()))
}
fn decode<T: serde::de::DeserializeOwned>(value: &[u8]) -> Result<T, UserDataError> {
    serde_json::from_slice(value).map_err(|e| invalid(&e.to_string()))
}
pub(super) fn init_tables(txn: &redb::WriteTransaction) -> Result<(), UserDataError> {
    txn.open_table(PLAYBACK)?;
    txn.open_table(WRITERS)?;
    txn.open_table(DECKS)?;
    txn.open_table(CHUNKS)?;
    txn.open_table(DELETED)?;
    Ok(())
}
pub(super) fn ensure_live_in(
    txn: &redb::WriteTransaction,
    user: &str,
) -> Result<(), UserDataError> {
    if txn.open_table(DELETED)?.get(user)?.is_some() {
        return Err(invalid("account playback was deleted"));
    }
    Ok(())
}
fn snapshot_in(
    txn: &redb::WriteTransaction,
    user: &str,
) -> Result<PlaybackSnapshot, UserDataError> {
    txn.open_table(PLAYBACK)?
        .get(user)?
        .map(|v| decode(v.value()))
        .transpose()
        .map(|v| v.unwrap_or_default())
}
pub(super) fn legacy_start_in(
    txn: &redb::WriteTransaction,
    user: &str,
    track: &str,
) -> Result<u64, UserDataError> {
    let mut state = snapshot_in(txn, user)?;
    state.revision = state
        .revision
        .checked_add(1)
        .ok_or(UserDataError::RevisionExhausted)?;
    state.generation = Some(Uuid::new_v4().to_string());
    state.checkpoint = Some(PlaybackCheckpoint::standalone(track.to_owned()));
    state.reason = None;
    txn.open_table(PLAYBACK)?
        .insert(user, encode(&state)?.as_slice())?;
    Ok(state.revision)
}

impl UserDataStore {
    pub fn playback_snapshot(&self, user: &str) -> Result<PlaybackSnapshot, UserDataError> {
        let txn = self.db.begin_read()?;
        let result = txn
            .open_table(PLAYBACK)?
            .get(user)?
            .map(|v| decode(v.value()))
            .transpose()?
            .unwrap_or_default();
        Ok(result)
    }
    pub fn playback_revision(&self, user: &str) -> Result<u64, UserDataError> {
        Ok(self.playback_snapshot(user)?.revision)
    }

    /// Acknowledges a committed retry before catalog/source validation. A source
    /// may have disappeared since the original successful transaction.
    pub fn playback_retry(
        &self,
        user: &str,
        input: &PlaybackWrite,
    ) -> Result<Option<PlaybackSnapshot>, UserDataError> {
        let input = input.normalized()?;
        let fingerprint = serde_json::to_string(&input).map_err(|e| invalid(&e.to_string()))?;
        let txn = self.db.begin_read()?;
        if txn.open_table(DELETED)?.get(user)?.is_some() {
            return Err(invalid("account playback was deleted"));
        }
        let previous = txn
            .open_table(WRITERS)?
            .get(user_key(user, &input.session_id).as_str())?
            .map(|value| decode::<Writer>(value.value()))
            .transpose()?;
        if previous.is_none_or(|previous| {
            previous.sequence != input.sequence
                || previous.request_id != input.request_id
                || previous.fingerprint != fingerprint
        }) {
            return Ok(None);
        }
        let snapshot = txn
            .open_table(PLAYBACK)?
            .get(user)?
            .map(|value| decode(value.value()))
            .transpose()?
            .unwrap_or_default();
        Ok(Some(snapshot))
    }
    pub fn playback_deck(
        &self,
        user: &str,
        id: &str,
    ) -> Result<Option<DurablePlaybackDeck>, UserDataError> {
        let txn = self.db.begin_read()?;
        let result = txn
            .open_table(DECKS)?
            .get(user_key(user, id).as_str())?
            .map(|v| decode(v.value()))
            .transpose()?;
        Ok(result)
    }
    pub fn playback_deck_ids(
        &self,
        user: &str,
        id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Option<(DurablePlaybackDeck, Vec<String>)>, UserDataError> {
        let txn = self.db.begin_read()?;
        let Some(deck) = txn
            .open_table(DECKS)?
            .get(user_key(user, id).as_str())?
            .map(|v| decode::<DurablePlaybackDeck>(v.value()))
            .transpose()?
        else {
            return Ok(None);
        };
        if offset > deck.total || limit > 256 {
            return Err(invalid("invalid durable deck page"));
        }
        let end = offset.saturating_add(limit).min(deck.total);
        let table = txn.open_table(CHUNKS)?;
        let mut ids = Vec::with_capacity(end - offset);
        let mut cursor = offset;
        while cursor < end {
            let key = chunk_key(user, id, cursor / CHUNK_SIZE);
            let bytes = table
                .get(key.as_str())?
                .ok_or_else(|| invalid("incomplete playback deck"))?;
            let chunk: Vec<String> = decode(bytes.value())?;
            let start = cursor % CHUNK_SIZE;
            let count = (end - cursor).min(chunk.len().saturating_sub(start));
            if count == 0 {
                return Err(invalid("incomplete playback deck"));
            }
            ids.extend_from_slice(&chunk[start..start + count]);
            cursor += count;
        }
        Ok(Some((deck, ids)))
    }
    pub fn save_playback(
        &self,
        user: &str,
        input: &PlaybackWrite,
        import: Option<PlaybackDeckImport>,
    ) -> StoreResult<PlaybackSnapshot> {
        self.save_reconciled_playback(user, input, import, None)
    }

    /// Only the restore resolver may authorize replacing an unavailable anchor.
    /// The original revision and generation are still compared in this transaction.
    pub fn save_reconciled_playback(
        &self,
        user: &str,
        input: &PlaybackWrite,
        import: Option<PlaybackDeckImport>,
        reconciled_from: Option<&str>,
    ) -> StoreResult<PlaybackSnapshot> {
        let normalized = input.normalized()?;
        let input = &normalized;
        // Full immutable payload identifies retries, including source and shuffle changes.
        let fingerprint = serde_json::to_string(input).map_err(|e| invalid(&e.to_string()))?;
        let txn = ::library::begin_write(&self.db).map_err(UserDataError::from)?;
        ensure_live_in(&txn, user)?;
        let mut latest = snapshot_in(&txn, user)?;
        let key = user_key(user, &input.session_id);
        let previous = txn
            .open_table(WRITERS)
            .map_err(UserDataError::from)?
            .get(key.as_str())
            .map_err(UserDataError::from)?
            .map(|v| decode::<Writer>(v.value()))
            .transpose()?;
        if let Some(previous) = previous {
            if input.sequence <= previous.sequence {
                if input.sequence == previous.sequence
                    && input.request_id == previous.request_id
                    && fingerprint == previous.fingerprint
                {
                    return Ok(latest);
                }
                return Err(PlaybackStoreError::Conflict(latest));
            }
        }
        if latest.revision != input.expected_revision
            || (input.intent == "update" && latest.generation != input.generation)
        {
            return Err(PlaybackStoreError::Conflict(latest));
        }
        if input.intent == "update"
            && latest.checkpoint.as_ref().is_none_or(|cp| {
                cp.track_id != input.checkpoint.track_id
                    && Some(cp.track_id.as_str()) != reconciled_from
            })
        {
            return Err(PlaybackStoreError::Conflict(latest));
        }
        if let Some(id) = &input.checkpoint.deck_id {
            let existing = txn
                .open_table(DECKS)
                .map_err(UserDataError::from)?
                .get(user_key(user, id).as_str())
                .map_err(UserDataError::from)?
                .map(|v| decode::<DurablePlaybackDeck>(v.value()))
                .transpose()?;
            let deck = if let Some(deck) = existing {
                deck
            } else {
                let mut import = import.ok_or(PlaybackStoreError::NotFound)?;
                import.source.normalize();
                if import.id != *id
                    || import.source != input.checkpoint.shuffle_context
                    || import.track_ids.is_empty()
                    || import.track_ids.len() > MAX_DECK_IDS
                    || import.track_ids.iter().any(|id| !identifier(id))
                {
                    return Err(invalid("invalid durable shuffle import").into());
                }
                if input.intent == "start"
                    && import.track_ids.get(input.checkpoint.cursor)
                        != Some(&input.checkpoint.track_id)
                {
                    return Err(invalid("shuffle cursor does not identify current track").into());
                }
                let deck = DurablePlaybackDeck {
                    id: id.clone(),
                    source: import.source,
                    total: import.track_ids.len(),
                    created_at: now_millis(),
                };
                txn.open_table(DECKS)
                    .map_err(UserDataError::from)?
                    .insert(user_key(user, id).as_str(), encode(&deck)?.as_slice())
                    .map_err(UserDataError::from)?;
                let mut chunks = txn.open_table(CHUNKS).map_err(UserDataError::from)?;
                for (index, ids) in import.track_ids.chunks(CHUNK_SIZE).enumerate() {
                    chunks
                        .insert(
                            chunk_key(user, id, index).as_str(),
                            encode(&ids)?.as_slice(),
                        )
                        .map_err(UserDataError::from)?;
                }
                deck
            };
            if deck.source != input.checkpoint.shuffle_context
                || input.checkpoint.cursor >= deck.total
            {
                return Err(invalid("shuffle context does not match durable deck").into());
            }
            let bytes = txn
                .open_table(CHUNKS)
                .map_err(UserDataError::from)?
                .get(chunk_key(user, id, input.checkpoint.cursor / CHUNK_SIZE).as_str())
                .map_err(UserDataError::from)?
                .map(|v| decode::<Vec<String>>(v.value()))
                .transpose()?
                .ok_or(PlaybackStoreError::NotFound)?;
            if input.intent == "start"
                && bytes.get(input.checkpoint.cursor % CHUNK_SIZE)
                    != Some(&input.checkpoint.track_id)
            {
                return Err(invalid("shuffle cursor does not identify current track").into());
            }
        }
        if input.intent == "start" {
            let (_, changed) = account_data::record_track_start_in(
                &txn,
                user,
                &input.checkpoint.track_id,
                &input.session_id,
                input.sequence,
                &input.request_id,
            )?;
            if !changed {
                return Err(PlaybackStoreError::Conflict(latest));
            }
            latest.generation = Some(Uuid::new_v4().to_string());
        }
        latest.revision = latest
            .revision
            .checked_add(1)
            .ok_or(UserDataError::RevisionExhausted)?;
        latest.checkpoint = Some(input.checkpoint.clone());
        latest.reason = None;
        txn.open_table(PLAYBACK)
            .map_err(UserDataError::from)?
            .insert(user, encode(&latest)?.as_slice())
            .map_err(UserDataError::from)?;
        txn.open_table(WRITERS)
            .map_err(UserDataError::from)?
            .insert(
                key.as_str(),
                encode(&Writer {
                    sequence: input.sequence,
                    request_id: input.request_id.clone(),
                    fingerprint,
                    updated_at: now_millis(),
                })?
                .as_slice(),
            )
            .map_err(UserDataError::from)?;
        cleanup(&txn, user, input.checkpoint.deck_id.as_deref(), &key)?;
        txn.commit().map_err(UserDataError::from)?;
        self.notify_account_change(user, latest.revision);
        Ok(latest)
    }
    pub fn clear_playback(
        &self,
        user: &str,
        expected_revision: u64,
    ) -> StoreResult<PlaybackSnapshot> {
        let txn = ::library::begin_write(&self.db).map_err(UserDataError::from)?;
        ensure_live_in(&txn, user)?;
        let mut latest = snapshot_in(&txn, user)?;
        if expected_revision != latest.revision {
            return Err(PlaybackStoreError::Conflict(latest));
        }
        latest.revision = latest
            .revision
            .checked_add(1)
            .ok_or(UserDataError::RevisionExhausted)?;
        latest.generation = None;
        latest.checkpoint = None;
        latest.reason = None;
        txn.open_table(PLAYBACK)
            .map_err(UserDataError::from)?
            .insert(user, encode(&latest)?.as_slice())
            .map_err(UserDataError::from)?;
        remove_prefix(&txn, DECKS, &user_prefix(user))?;
        remove_prefix(&txn, CHUNKS, &user_prefix(user))?;
        account_data::clear_last_track_in(&txn, user)?;
        txn.commit().map_err(UserDataError::from)?;
        self.notify_account_change(user, latest.revision);
        Ok(latest)
    }
    /// Called after auth deletion. UUID account IDs are never reassigned.
    pub fn purge_playback(&self, user: &str) -> Result<(), UserDataError> {
        let txn = ::library::begin_write(&self.db)?;
        // Serialize the permanent fence with cleanup. Already authenticated
        // requests that arrive later cannot recreate a deleted account's rows.
        txn.open_table(DELETED)?.insert(user, now_millis())?;
        txn.open_table(PLAYBACK)?.remove(user)?;
        remove_prefix(&txn, WRITERS, &user_prefix(user))?;
        remove_prefix(&txn, DECKS, &user_prefix(user))?;
        remove_prefix(&txn, CHUNKS, &user_prefix(user))?;
        account_data::purge_playback_in(&txn, user)?;
        txn.commit()?;
        Ok(())
    }
}
fn chunk_key(user: &str, deck: &str, index: usize) -> String {
    user_key(user, &format!("{}:{}:{index:08}", deck.len(), deck))
}
fn remove_prefix(
    txn: &redb::WriteTransaction,
    definition: TableDefinition<&str, &[u8]>,
    prefix: &str,
) -> Result<(), UserDataError> {
    let mut table = txn.open_table(definition)?;
    let keys = table
        .range(prefix..)?
        .take_while(|row| {
            row.as_ref()
                .map(|(key, _)| key.value().starts_with(prefix))
                .unwrap_or(true)
        })
        .map(|row| row.map(|(key, _)| key.value().to_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    for key in keys {
        table.remove(key.as_str())?;
    }
    Ok(())
}
fn cleanup(
    txn: &redb::WriteTransaction,
    user: &str,
    keep: Option<&str>,
    writer_key: &str,
) -> Result<(), UserDataError> {
    let prefix = user_prefix(user);
    let mut decks = Vec::new();
    for row in txn.open_table(DECKS)?.range(prefix.as_str()..)? {
        let (key, bytes) = row?;
        if !key.value().starts_with(&prefix) {
            break;
        }
        decks.push(decode::<DurablePlaybackDeck>(bytes.value())?);
    }
    decks.sort_by_key(|d| (Some(d.id.as_str()) == keep, d.created_at));
    let mut total: usize = decks.iter().map(|d| d.total).sum();
    while decks.len() > MAX_DECKS || total > MAX_ACCOUNT_IDS {
        let deck = decks.remove(0);
        total -= deck.total;
        txn.open_table(DECKS)?
            .remove(user_key(user, &deck.id).as_str())?;
        remove_prefix(
            txn,
            CHUNKS,
            &user_key(user, &format!("{}:{}:", deck.id.len(), deck.id)),
        )?;
    }
    let mut writers = Vec::new();
    for row in txn.open_table(WRITERS)?.range(prefix.as_str()..)? {
        let (key, bytes) = row?;
        if !key.value().starts_with(&prefix) {
            break;
        }
        writers.push((
            key.value().to_owned(),
            decode::<Writer>(bytes.value())?.updated_at,
        ));
    }
    writers.sort_by_key(|(key, time)| (key == writer_key, *time));
    let remove = writers.len().saturating_sub(MAX_WRITERS);
    let mut table = txn.open_table(WRITERS)?;
    for (key, _) in writers.into_iter().take(remove) {
        table.remove(key.as_str())?;
    }
    Ok(())
}
