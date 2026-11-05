//! Private durable resume suggestions. Restoration is read-only reconciliation;
//! a transient catalogue failure never erases a saved checkpoint.
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::{Deserialize, Serialize};

use super::views::{build_track_views, TrackView};
use crate::{
    http::json_error,
    state::{AppState, AuthContext, JsonResult},
    user_data::{
        PlaybackCheckpoint, PlaybackDeckImport, PlaybackSnapshot, PlaybackSource,
        PlaybackStoreError, PlaybackWrite, UserDataError, UserDataStore,
    },
};

#[derive(Serialize)]
pub struct PlaybackDeckPage {
    pub session_id: String,
    pub offset: usize,
    pub next_offset: usize,
    pub total: usize,
    pub has_more: bool,
    pub items: Vec<TrackView>,
    pub item_offsets: Vec<usize>,
}

pub async fn get(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> JsonResult<PlaybackSnapshot> {
    let library = super::library_or_json_error(&state)?;
    let store = state.user_data.clone();
    let user = ctx.user.id;
    let read_store = store.clone();
    let read_user = user.clone();
    let snapshot = tokio::task::spawn_blocking(move || read_store.playback_snapshot(&read_user))
        .await
        .map_err(worker_error)?
        .map_err(data_error)?;
    let sources = snapshot
        .checkpoint
        .as_ref()
        .map(|cp| vec![cp.source.clone(), cp.shuffle_context.clone()])
        .unwrap_or_default();
    let custom = prepare_custom(&state, &library, &sources).await?;
    let permit = state
        .shuffle_sessions
        .acquire_work_permit()
        .await
        .map_err(|_| unavailable())?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        restore(&store, &library, &user, snapshot, &custom)
    })
    .await
    .map_err(worker_error)?
    .map(Json)
}

pub async fn save(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(input): Json<PlaybackWrite>,
) -> Response {
    if let Err(err) = input.validate() {
        return data_error(err).into_response();
    }
    if state.shutdown.is_cancelled() {
        return unavailable().into_response();
    }
    let retry_store = state.user_data.clone();
    let retry_user = ctx.user.id.clone();
    let retry_input = input.clone();
    match tokio::task::spawn_blocking(move || retry_store.playback_retry(&retry_user, &retry_input))
        .await
    {
        Ok(Ok(Some(snapshot))) => return Json(snapshot).into_response(),
        Ok(Ok(None)) => {}
        Ok(Err(err)) => return data_error(err).into_response(),
        Err(err) => return worker_error(err).into_response(),
    }
    let library = match super::library_or_json_error(&state) {
        Ok(value) => value,
        Err(err) => return err.into_response(),
    };
    let store = state.user_data.clone();
    let reconciliation =
        match prepare_update_reconciliation(&state, &library, &ctx.user.id, &input).await {
            Ok(value) => value,
            Err(err) => return err.into_response(),
        };
    let sessions = state.shuffle_sessions.clone();
    let permit = match sessions.acquire_work_permit().await {
        Ok(permit) => permit,
        Err(_) => return unavailable().into_response(),
    };
    let result = state
        .shutdown
        .spawn_blocking_commit("playback_checkpoint", move || {
            let _permit = permit;
            let reconciled_from = if let Some((snapshot, custom)) = reconciliation {
                let original = snapshot.checkpoint.as_ref().map(|cp| cp.track_id.clone());
                let resolved = restore(&store, &library, &ctx.user.id, snapshot, &custom)?;
                if resolved
                    .checkpoint
                    .as_ref()
                    .is_some_and(|cp| cp.track_id == input.checkpoint.track_id)
                {
                    original
                } else {
                    None
                }
            } else {
                None
            };
            // Import only from an authenticated account/client-owned runtime deck.
            // A persisted deck reference deliberately does not require its old client.
            let import = if let Some(id) = &input.checkpoint.deck_id {
                match store.playback_deck(&ctx.user.id, id).map_err(data_error)? {
                    Some(_) => None,
                    None => {
                        let client = input.client_id.as_deref().ok_or_else(|| {
                            json_error(
                                StatusCode::BAD_REQUEST,
                                "client_id required to persist a live shuffle",
                            )
                        })?;
                        let deck =
                            sessions
                                .export_deck(&ctx.user.id, client, id)
                                .map_err(|_| {
                                    json_error(StatusCode::NOT_FOUND, "shuffle deck not found")
                                })?;
                        Some(PlaybackDeckImport {
                            id: id.clone(),
                            source: PlaybackSource {
                                kind: deck.mode,
                                id: deck.source_id,
                                artist_ids: deck.artist_ids,
                                genres: deck.genres,
                            },
                            track_ids: deck.track_ids,
                        })
                    }
                }
            } else {
                None
            };
            if library
                .get_track(&input.checkpoint.track_id)
                .map_err(|_| unavailable())?
                .is_none()
            {
                return Err(json_error(StatusCode::NOT_FOUND, "track not found"));
            }
            // Foreign playlist IDs and missing sources have the same result. No
            // client-supplied account identity participates in this lookup.
            for source in [&input.checkpoint.source, &input.checkpoint.shuffle_context] {
                if source.kind == "playlist"
                    && store
                        .get_playlist(&ctx.user.id, source.id.as_deref().unwrap())
                        .map_err(data_error)?
                        .is_none()
                {
                    return Err(json_error(
                        StatusCode::NOT_FOUND,
                        "playback source not found",
                    ));
                }
            }
            Ok(match reconciled_from {
                Some(original) => {
                    store.save_reconciled_playback(&ctx.user.id, &input, import, Some(&original))
                }
                None => store.save_playback(&ctx.user.id, &input, import),
            })
        })
        .await;
    match result {
        Ok(Ok(Ok(snapshot))) => Json(snapshot).into_response(),
        Ok(Ok(Err(err))) => store_error(err),
        Ok(Err(err)) => err.into_response(),
        Err(err) => worker_error(err).into_response(),
    }
}

async fn prepare_update_reconciliation(
    state: &AppState,
    library: &library::Library,
    user: &str,
    input: &PlaybackWrite,
) -> Result<Option<(PlaybackSnapshot, CustomCandidates)>, ApiError> {
    if input.intent != "update" {
        return Ok(None);
    }

    let store = state.user_data.clone();
    let user = user.to_owned();
    let snapshot = tokio::task::spawn_blocking(move || store.playback_snapshot(&user))
        .await
        .map_err(worker_error)?
        .map_err(data_error)?;
    if snapshot.revision != input.expected_revision
        || snapshot.generation != input.generation
        || snapshot
            .checkpoint
            .as_ref()
            .is_none_or(|cp| cp.track_id == input.checkpoint.track_id)
    {
        return Ok(None);
    }

    let checkpoint = snapshot.checkpoint.as_ref().unwrap();
    let sources = [
        checkpoint.source.clone(),
        checkpoint.shuffle_context.clone(),
    ];
    let custom = prepare_custom(state, library, &sources).await?;
    Ok(Some((snapshot, custom)))
}

#[derive(Deserialize)]
pub struct ClearQuery {
    pub expected_revision: u64,
}
pub async fn clear(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Query(query): Query<ClearQuery>,
) -> Response {
    if state.shutdown.is_cancelled() {
        return unavailable().into_response();
    }
    let store = state.user_data.clone();
    match state
        .shutdown
        .spawn_blocking_commit("clear_playback", move || {
            store.clear_playback(&ctx.user.id, query.expected_revision)
        })
        .await
    {
        Ok(Ok(snapshot)) => Json(snapshot).into_response(),
        Ok(Err(err)) => store_error(err),
        Err(err) => worker_error(err).into_response(),
    }
}

#[derive(Deserialize)]
pub struct DeckQuery {
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}
pub async fn deck_page(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
    Query(query): Query<DeckQuery>,
) -> JsonResult<PlaybackDeckPage> {
    let offset = query.offset.unwrap_or(0);
    let limit = query.limit.unwrap_or(10);
    if !(1..=50).contains(&limit) {
        return Err(json_error(StatusCode::BAD_REQUEST, "limit must be 1 to 50"));
    }
    let library = super::library_or_json_error(&state)?;
    let store = state.user_data.clone();
    let deck_store = store.clone();
    let deck_user = ctx.user.id.clone();
    let deck_id = id.clone();
    let deck = tokio::task::spawn_blocking(move || deck_store.playback_deck(&deck_user, &deck_id))
        .await
        .map_err(worker_error)?
        .map_err(data_error)?
        .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "shuffle deck not found"))?;
    deck.source.validate().map_err(data_error)?;
    let custom = prepare_custom(&state, &library, &[deck.source]).await?;
    let permit = state
        .shuffle_sessions
        .acquire_work_permit()
        .await
        .map_err(|_| unavailable())?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let (deck, ids) = store
            .playback_deck_ids(&ctx.user.id, &id, offset, limit)
            .map_err(data_error)?
            .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "shuffle deck not found"))?;
        let candidates = source_candidates(&store, &library, &ctx.user.id, &deck.source, &custom)?;
        let tracks = library.tracks_by_ids(&ids).map_err(|_| unavailable())?;
        let mut items = Vec::new();
        let mut offsets = Vec::new();
        for (index, id) in ids.iter().enumerate() {
            if candidates.contains(id) {
                if let Some(track) = tracks.iter().find(|track| &track.id == id) {
                    items.push(track.clone());
                    offsets.push(offset + index);
                }
            }
        }
        let (liked, playlists) = store
            .track_membership(&ctx.user.id, &ids)
            .map_err(data_error)?;
        let items = build_track_views(&library, &items, &liked, &playlists)
            .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
        let next = offset.saturating_add(limit).min(deck.total);
        Ok(Json(PlaybackDeckPage {
            session_id: id,
            offset,
            next_offset: next,
            total: deck.total,
            has_more: next < deck.total,
            items,
            item_offsets: offsets,
        }))
    })
    .await
    .map_err(worker_error)?
}

type CustomCandidates = HashMap<String, Arc<Vec<String>>>;
fn custom_key(source: &PlaybackSource) -> String {
    serde_json::to_string(source).unwrap_or_default()
}
async fn prepare_custom(
    state: &AppState,
    library: &library::Library,
    sources: &[PlaybackSource],
) -> Result<CustomCandidates, ApiError> {
    let mut out = HashMap::new();
    for source in sources.iter().filter(|source| source.kind == "custom") {
        let key = custom_key(source);
        if !out.contains_key(&key) {
            let ids = state
                .shuffle_sessions
                .resume_custom_candidates(
                    library.clone(),
                    state.metadata_events.revision(),
                    source.artist_ids.clone(),
                    source.genres.clone(),
                )
                .await
                .map_err(|_| unavailable())?;
            out.insert(key, ids);
        }
    }
    Ok(out)
}
struct Candidates {
    exists: bool,
    ids: Option<Vec<String>>,
    membership: Option<HashSet<String>>,
    sorted: Option<Arc<Vec<String>>>,
}
impl Candidates {
    fn contains(&self, id: &str) -> bool {
        self.exists
            && self.membership.as_ref().is_none_or(|ids| ids.contains(id))
            && self
                .sorted
                .as_ref()
                .is_none_or(|ids| ids.binary_search_by(|item| item.as_str().cmp(id)).is_ok())
    }
    fn ordered_ids(&self) -> Option<&[String]> {
        self.ids
            .as_deref()
            .or_else(|| self.sorted.as_ref().map(|ids| ids.as_slice()))
    }
}
fn source_candidates(
    store: &UserDataStore,
    library: &library::Library,
    user: &str,
    source: &PlaybackSource,
    custom: &CustomCandidates,
) -> Result<Candidates, ApiError> {
    if source.kind == "custom" {
        let ids = custom
            .get(&custom_key(source))
            .ok_or_else(unavailable)?
            .clone();
        return Ok(Candidates {
            exists: true,
            ids: None,
            membership: None,
            sorted: Some(ids),
        });
    }
    let ids = match source.kind.as_str() {
        "standalone" | "library" => None,
        "liked" => Some(store.list_likes(user).map_err(data_error)?),
        "playlist" => match store
            .get_playlist(user, source.id.as_deref().unwrap())
            .map_err(data_error)?
        {
            Some(playlist) => Some(playlist.track_ids),
            None => {
                return Ok(Candidates {
                    exists: false,
                    ids: None,
                    membership: None,
                    sorted: None,
                })
            }
        },
        "album" => {
            let id = source.id.as_deref().unwrap();
            if library.get_album(id).map_err(|_| unavailable())?.is_none() {
                return Ok(Candidates {
                    exists: false,
                    ids: None,
                    membership: None,
                    sorted: None,
                });
            }
            Some(
                library
                    .get_album_tracks(id)
                    .map_err(|_| unavailable())?
                    .into_iter()
                    .map(|track| track.id)
                    .collect(),
            )
        }
        "artist" => {
            let id = source.id.as_deref().unwrap();
            if library.get_artist(id).map_err(|_| unavailable())?.is_none() {
                return Ok(Candidates {
                    exists: false,
                    ids: None,
                    membership: None,
                    sorted: None,
                });
            }
            Some(
                crate::shuffle::artist_tracks(library, id)
                    .map_err(|_| unavailable())?
                    .into_iter()
                    .map(|track| track.id)
                    .collect(),
            )
        }
        _ => {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "invalid playback source",
            ))
        }
    };
    let membership = ids.as_ref().map(|ids| ids.iter().cloned().collect());
    Ok(Candidates {
        exists: true,
        ids,
        membership,
        sorted: None,
    })
}

type ApiError = (StatusCode, Json<crate::state::ErrorResponse>);
fn source_exists(
    store: &UserDataStore,
    library: &library::Library,
    user: &str,
    source: &PlaybackSource,
) -> Result<bool, ApiError> {
    match source.kind.as_str() {
        "playlist" => Ok(store
            .get_playlist(user, source.id.as_deref().unwrap())
            .map_err(data_error)?
            .is_some()),
        "album" => Ok(library
            .get_album(source.id.as_deref().unwrap())
            .map_err(|_| unavailable())?
            .is_some()),
        "artist" => Ok(library
            .get_artist(source.id.as_deref().unwrap())
            .map_err(|_| unavailable())?
            .is_some()),
        _ => Ok(true),
    }
}
fn restore(
    store: &UserDataStore,
    library: &library::Library,
    user: &str,
    mut snapshot: PlaybackSnapshot,
    custom: &CustomCandidates,
) -> Result<PlaybackSnapshot, ApiError> {
    // Additive migration: old accounts retain their last track without inventing
    // a source or changing durable revisions just because a client reads it.
    if snapshot.revision == 0 && snapshot.checkpoint.is_none() {
        if let Some(id) = store.last_track(user).map_err(data_error)?.track_id {
            snapshot.checkpoint = Some(PlaybackCheckpoint::standalone(id));
        }
    }
    let Some(mut checkpoint) = snapshot.checkpoint.take() else {
        return Ok(snapshot);
    };
    if checkpoint.validate().is_err() {
        snapshot.reason = Some("invalid_checkpoint".into());
        return Ok(snapshot);
    }
    if !source_exists(store, library, user, &checkpoint.source)? {
        snapshot.reason = Some("source_missing".into());
        if library
            .get_track(&checkpoint.track_id)
            .map_err(|_| unavailable())?
            .is_some()
        {
            snapshot.checkpoint = Some(PlaybackCheckpoint::standalone(checkpoint.track_id));
        }
        return Ok(snapshot);
    }
    if checkpoint.shuffle_mode != "off"
        && !source_exists(store, library, user, &checkpoint.shuffle_context)?
    {
        checkpoint.shuffle_mode = "off".into();
        checkpoint.shuffle_context = PlaybackSource::standalone();
        checkpoint.deck_id = None;
        checkpoint.cursor = 0;
        snapshot.reason = Some("shuffle_source_missing".into());
    }
    let effective = if checkpoint.shuffle_mode == "off" {
        &checkpoint.source
    } else {
        &checkpoint.shuffle_context
    };
    let mut candidates = source_candidates(store, library, user, effective, custom)?;
    let current_exists = library
        .get_track(&checkpoint.track_id)
        .map_err(|_| unavailable())?
        .is_some();
    let mut valid = current_exists && candidates.contains(&checkpoint.track_id);
    if let Some(id) = checkpoint.deck_id.clone() {
        if let Some(deck) = store.playback_deck(user, &id).map_err(data_error)? {
            // A paused shuffle toggle can retain an out-of-deck current anchor.
            // On a cold restore the saved cursor identifies the first next item.
            let anchor = store
                .playback_deck_ids(user, &id, checkpoint.cursor.min(deck.total), 1)
                .map_err(data_error)?;
            valid &= anchor
                .as_ref()
                .is_some_and(|(_, ids)| ids.first() == Some(&checkpoint.track_id));
            if !valid {
                let start = checkpoint.cursor.min(deck.total);
                // Search the suffix and then the prefix exactly once. A removed
                // last item must not hide surviving earlier deck entries.
                for (begin, end) in [(start, deck.total), (0, start)] {
                    let mut cursor = begin;
                    while cursor < end {
                        let (_, ids) = store
                            .playback_deck_ids(user, &id, cursor, (end - cursor).min(256))
                            .map_err(data_error)?
                            .ok_or_else(|| {
                                json_error(StatusCode::NOT_FOUND, "shuffle deck not found")
                            })?;
                        let tracks = library.tracks_by_ids(&ids).map_err(|_| unavailable())?;
                        for (index, track_id) in ids.iter().enumerate() {
                            if candidates.contains(track_id)
                                && tracks.iter().any(|track| &track.id == track_id)
                            {
                                checkpoint.track_id = track_id.clone();
                                checkpoint.cursor = cursor + index;
                                snapshot.reason = Some("track_removed".into());
                                snapshot.checkpoint = Some(checkpoint);
                                return Ok(snapshot);
                            }
                        }
                        cursor += ids.len();
                        if ids.is_empty() {
                            break;
                        }
                    }
                }
                snapshot.reason = Some("source_empty".into());
                return Ok(snapshot);
            }
        } else {
            checkpoint.deck_id = None;
            checkpoint.cursor = 0;
            checkpoint.shuffle_mode = "off".into();
            checkpoint.shuffle_context = PlaybackSource::standalone();
            snapshot.reason = Some("deck_missing".into());
            candidates = source_candidates(store, library, user, &checkpoint.source, custom)?;
            valid = current_exists && candidates.contains(&checkpoint.track_id);
        }
    }
    if !valid {
        if let Some(ids) = candidates.ordered_ids() {
            for chunk in ids.chunks(256) {
                let tracks = library.tracks_by_ids(chunk).map_err(|_| unavailable())?;
                if let Some(track) = tracks.first() {
                    checkpoint.track_id = track.id.clone();
                    checkpoint.cursor = 0;
                    snapshot.reason = Some("track_removed".into());
                    snapshot.checkpoint = Some(checkpoint);
                    return Ok(snapshot);
                }
            }
        }
        snapshot.reason = Some(
            if candidates.ordered_ids().is_some() {
                "source_empty"
            } else {
                "track_missing"
            }
            .into(),
        );
        return Ok(snapshot);
    }
    snapshot.checkpoint = Some(checkpoint);
    Ok(snapshot)
}
fn unavailable() -> ApiError {
    json_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "library temporarily unavailable",
    )
}
fn data_error(err: UserDataError) -> ApiError {
    let status = if matches!(&err, UserDataError::Invalid(_)) {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    json_error(status, err.to_string())
}
fn worker_error(err: tokio::task::JoinError) -> ApiError {
    json_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}
fn store_error(err: PlaybackStoreError) -> Response {
    match err {
        PlaybackStoreError::Conflict(snapshot) => {
            (StatusCode::CONFLICT, Json(snapshot)).into_response()
        }
        PlaybackStoreError::NotFound => {
            json_error(StatusCode::NOT_FOUND, "shuffle deck not found").into_response()
        }
        PlaybackStoreError::Data(err) => data_error(err).into_response(),
    }
}
