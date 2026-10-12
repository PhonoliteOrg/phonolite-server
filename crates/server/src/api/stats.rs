use axum::{
    extract::{Query, State},
    http::StatusCode,
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use time::OffsetDateTime;

use crate::http::json_error;
use crate::library_views::build_track_views;
use crate::state::{AppState, AuthContext, JsonResult};
use crate::stats_store::{
    CounterBatch, CounterCheckpoint, CounterSnapshot, CounterState, DevicePreference,
    EventBatchResult, ListeningEvent, StatsError, StatsResetResult, StatsStore, StatsSyncPage,
    MAX_EVENT_BATCH,
};

#[derive(Debug, Deserialize)]
pub struct StatsQuery {
    pub year: Option<i32>,
    pub month: Option<u8>,
}

#[derive(Serialize)]
pub struct StatsResponse {
    pub year: i32,
    pub month: Option<u8>,
    pub total_minutes: u64,
    pub total_listening_ms: u64,
    pub total_plays: u64,
    pub unique_tracks: usize,
    pub unique_artists: usize,
    pub other_genre_listening_ms: u64,
    pub unclassified_listening_ms: u64,
    pub top_tracks: Vec<StatsTrack>,
    pub top_artists: Vec<StatsItem>,
    pub top_genres: Vec<StatsItem>,
}

#[derive(Serialize)]
pub struct StatsItem {
    pub id: String,
    pub name: String,
    pub minutes: u64,
    pub listening_ms: u64,
}

#[derive(Serialize)]
pub struct StatsTrack {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub minutes: u64,
    pub plays: u64,
}

pub async fn get_stats(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Query(query): Query<StatsQuery>,
) -> JsonResult<StatsResponse> {
    let library = state.library_state.read().library.clone();
    let (year, month) = resolve_stats_period(query.year, query.month)?;
    let store = state.stats.clone();
    let user_id = ctx.user.id.clone();
    let catalog = library.clone();
    let stats = tokio::task::spawn_blocking(move || {
        let track_ids = store.listening_track_ids(&user_id)?;
        refresh_genre_metadata(&store, &user_id, catalog.as_ref(), track_ids)?;
        store.get_period(&user_id, year, month)
    })
    .await
    .map_err(stats_task_error)?
    .map_err(stats_error)?;

    let Some(stats) = stats else {
        return Ok(Json(StatsResponse {
            year,
            month,
            total_minutes: 0,
            total_listening_ms: 0,
            total_plays: 0,
            unique_tracks: 0,
            unique_artists: 0,
            other_genre_listening_ms: 0,
            unclassified_listening_ms: 0,
            top_tracks: Vec::new(),
            top_artists: Vec::new(),
            top_genres: Vec::new(),
        }));
    };

    let mut top_tracks_source: Vec<_> = stats
        .track_ms
        .keys()
        .chain(stats.track_plays.keys())
        .cloned()
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    top_tracks_source.sort_by(|a, b| {
        stats
            .track_ms
            .get(b)
            .unwrap_or(&0)
            .cmp(stats.track_ms.get(a).unwrap_or(&0))
            .then_with(|| {
                stats
                    .track_plays
                    .get(b)
                    .unwrap_or(&0)
                    .cmp(stats.track_plays.get(a).unwrap_or(&0))
            })
            .then_with(|| a.cmp(b))
    });
    top_tracks_source.truncate(10);

    let top_tracks = top_tracks_source
        .into_iter()
        .map(|track_id| {
            let plays = stats.track_plays.get(&track_id).copied().unwrap_or(0);
            let snapshot = state
                .stats
                .track_snapshot(&ctx.user.id, &track_id)
                .ok()
                .flatten();
            let (title, artist_name) = match library
                .as_ref()
                .and_then(|library| library.get_track(&track_id).ok().flatten())
            {
                Some(track) => {
                    let artist_name = library
                        .as_ref()
                        .and_then(|library| library.get_artist(&track.artist_id).ok().flatten())
                        .map(|artist| artist.name)
                        .or_else(|| snapshot.as_ref().map(|event| event.artist_name.clone()))
                        .unwrap_or_else(|| "Unknown Artist".to_string());
                    (track.title, artist_name)
                }
                _ => snapshot
                    .map(|event| (event.title, event.artist_name))
                    .unwrap_or_else(|| ("Unknown Track".to_string(), "Unknown Artist".to_string())),
            };
            let minutes = stats
                .track_ms
                .get(&track_id)
                .map(|ms| ms_to_minutes(*ms))
                .unwrap_or(0);
            StatsTrack {
                id: track_id,
                title,
                artist: artist_name,
                minutes,
                plays,
            }
        })
        .collect();

    let top_artists = top_n(&stats.artist_ms, 5)
        .into_iter()
        .map(|(artist_id, ms)| {
            let name = library
                .as_ref()
                .and_then(|library| library.get_artist(&artist_id).ok().flatten())
                .map(|artist| artist.name)
                .or_else(|| {
                    state
                        .stats
                        .artist_snapshot(&ctx.user.id, &artist_id)
                        .ok()
                        .flatten()
                })
                .unwrap_or_else(|| "Unknown Artist".to_string());
            StatsItem {
                id: artist_id,
                name,
                minutes: ms_to_minutes(ms),
                listening_ms: ms,
            }
        })
        .collect();

    let (top_genres, other_genre_listening_ms, unclassified_listening_ms) =
        genre_breakdown(&stats.genre_ms, stats.total_ms);

    Ok(Json(StatsResponse {
        year,
        month,
        total_minutes: ms_to_minutes(stats.total_ms),
        total_listening_ms: stats.total_ms,
        total_plays: stats
            .track_plays
            .values()
            .fold(0_u64, |total, plays| total.saturating_add(*plays)),
        unique_tracks: stats
            .track_ms
            .values()
            .filter(|milliseconds| **milliseconds > 0)
            .count(),
        unique_artists: stats
            .artist_ms
            .values()
            .filter(|milliseconds| **milliseconds > 0)
            .count(),
        other_genre_listening_ms,
        unclassified_listening_ms,
        top_tracks,
        top_artists,
        top_genres,
    }))
}

#[derive(Debug, Deserialize)]
pub struct StatsSyncQuery {
    #[serde(default)]
    pub after: u64,
    pub limit: Option<usize>,
    pub device_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListeningBatchRequest {
    pub device_id: String,
    pub history_generation: u64,
    pub events: Vec<ListeningEvent>,
}

pub async fn sync_stats(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Query(query): Query<StatsSyncQuery>,
) -> JsonResult<StatsSyncPage> {
    // Reading history and identity remains available when recording is disabled.
    let collection_enabled = state.config.read().stats_collection_enabled;
    let library = state.library_state.read().library.clone();
    let store = state.stats.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut page = store.sync_events(
            &ctx.user.id,
            query.after,
            query.limit.unwrap_or(MAX_EVENT_BATCH),
            collection_enabled,
            query.device_id.as_deref(),
        )?;
        // Keep repair payloads bounded by this history page. Already-synced
        // client history can refresh its remaining tracks when opening stats.
        let track_ids = page
            .events
            .iter()
            .map(|change| change.event.track_id.clone());
        let resolved = refresh_genre_metadata(&store, &ctx.user.id, library.as_ref(), track_ids)?;
        page.genre_metadata.extend(resolved);
        Ok::<_, StatsError>(page)
    })
    .await
    .map_err(stats_task_error)?;
    result.map(Json).map_err(stats_error)
}

fn refresh_genre_metadata(
    store: &StatsStore,
    user_id: &str,
    library: Option<&::library::Library>,
    track_ids: impl IntoIterator<Item = String>,
) -> Result<HashMap<String, Vec<String>>, StatsError> {
    let Some(library) = library else {
        return Ok(HashMap::new());
    };
    let mut tracks = Vec::new();
    for track_id in track_ids.into_iter().collect::<HashSet<_>>() {
        match library.get_track(&track_id) {
            Ok(Some(track)) => tracks.push(track),
            // Missing catalog records retain the original saved genre snapshot.
            Ok(None) => {}
            Err(error) => tracing::warn!("stats genre lookup failed: {}", error),
        }
    }
    if tracks.is_empty() {
        return Ok(HashMap::new());
    }
    // Build one batch so album and performer metadata is loaded once per
    // identity, independent of how many listening intervals reference it.
    let views = match build_track_views(library, &tracks, &HashSet::new(), &HashSet::new()) {
        Ok(views) => views,
        Err(error) => {
            // Optional catalog enrichment must not block history replication.
            tracing::warn!("stats genre projection failed: {}", error);
            return Ok(HashMap::new());
        }
    };
    let metadata = views
        .into_iter()
        .map(|track| (track.id, track.listening_genres))
        .collect();
    store.update_genre_metadata(user_id, &metadata)?;
    Ok(metadata)
}

pub async fn ingest_stats(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(request): Json<ListeningBatchRequest>,
) -> JsonResult<EventBatchResult> {
    if !state.config.read().stats_collection_enabled {
        return Err(json_error(
            StatusCode::FORBIDDEN,
            "stats collection disabled".to_string(),
        ));
    }
    if request.events.len() > MAX_EVENT_BATCH {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "at most 200 listening events per batch".to_string(),
        ));
    }
    let store = state.stats.clone();
    let result = tokio::task::spawn_blocking(move || {
        store.ingest_events(
            &ctx.user.id,
            &request.device_id,
            request.history_generation,
            &request.events,
        )
    })
    .await
    .map_err(stats_task_error)?;
    result.map(Json).map_err(stats_error)
}

pub async fn update_stats_preference(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(request): Json<DevicePreference>,
) -> JsonResult<DevicePreference> {
    // Disabling a device is always allowed, including when the administrator has
    // disabled ingestion. It never changes another installation's preference.
    let store = state.stats.clone();
    let result =
        tokio::task::spawn_blocking(move || store.set_device_preference(&ctx.user.id, request))
            .await
            .map_err(stats_task_error)?;
    result.map(Json).map_err(stats_error)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatsResetRequest {
    pub history_generation: u64,
}

#[derive(Deserialize)]
pub struct CounterQuery {
    device_id: String,
    year: Option<i32>,
    month: Option<u8>,
    revision: Option<u64>,
    #[serde(default)]
    after: String,
    limit: Option<usize>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivateRequest {
    device_id: String,
    history_generation: u64,
}

pub async fn counter_state(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Query(query): Query<CounterQuery>,
) -> JsonResult<CounterState> {
    let enabled = state.config.read().stats_collection_enabled;
    let store = state.stats.clone();
    let library = state.library_state.read().library.clone();
    tokio::task::spawn_blocking(move || {
        let tracks = store.listening_track_ids(&ctx.user.id)?;
        refresh_genre_metadata(&store, &ctx.user.id, library.as_ref(), tracks)?;
        store.counter_state(&ctx.user.id, &query.device_id, enabled)
    })
    .await
    .map_err(stats_task_error)?
    .map(Json)
    .map_err(stats_error)
}
pub async fn counter_snapshot(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Query(query): Query<CounterQuery>,
) -> JsonResult<CounterSnapshot> {
    let store = state.stats.clone();
    let library = state.library_state.read().library.clone();
    tokio::task::spawn_blocking(move || {
        let year = query
            .year
            .ok_or_else(|| StatsError::Invalid("year is required".into()))?;
        let month = query
            .month
            .ok_or_else(|| StatsError::Invalid("month is required".into()))?;
        // Repair before the first page; later pages use the same revision fence.
        if query.after.is_empty() {
            let tracks = store.listening_track_ids(&ctx.user.id)?;
            refresh_genre_metadata(&store, &ctx.user.id, library.as_ref(), tracks)?;
        }
        store.counter_snapshot(
            &ctx.user.id,
            &query.device_id,
            year,
            month,
            query.revision,
            &query.after,
            query.limit.unwrap_or(200),
        )
    })
    .await
    .map_err(stats_task_error)?
    .map(Json)
    .map_err(stats_error)
}
pub async fn ingest_counters(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(batch): Json<CounterBatch>,
) -> JsonResult<CounterCheckpoint> {
    if !state.config.read().stats_collection_enabled {
        return Err(json_error(
            StatusCode::FORBIDDEN,
            "stats collection disabled",
        ));
    }
    let store = state.stats.clone();
    tokio::task::spawn_blocking(move || store.ingest_counters(&ctx.user.id, &batch))
        .await
        .map_err(stats_task_error)?
        .map(Json)
        .map_err(stats_error)
}
pub async fn activate_counters(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(request): Json<ActivateRequest>,
) -> JsonResult<CounterCheckpoint> {
    let store = state.stats.clone();
    tokio::task::spawn_blocking(move || {
        store.activate_counters(&ctx.user.id, &request.device_id, request.history_generation)
    })
    .await
    .map_err(stats_task_error)?
    .map(Json)
    .map_err(stats_error)
}

pub async fn reset_stats(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(request): Json<StatsResetRequest>,
) -> JsonResult<StatsResetResult> {
    // Deletion remains available when device or server collection is disabled.
    let store = state.stats.clone();
    let result = tokio::task::spawn_blocking(move || {
        store.reset_history(&ctx.user.id, request.history_generation)
    })
    .await
    .map_err(stats_task_error)?;
    result.map(Json).map_err(stats_error)
}

fn stats_task_error(
    err: tokio::task::JoinError,
) -> (StatusCode, Json<crate::state::ErrorResponse>) {
    tracing::error!("stats worker failed: {}", err);
    json_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "stats worker failed".to_string(),
    )
}

fn stats_error(err: StatsError) -> (StatusCode, Json<crate::state::ErrorResponse>) {
    let status = match &err {
        StatsError::DeviceDisabled => StatusCode::FORBIDDEN,
        StatsError::GenerationMismatch => StatusCode::CONFLICT,
        StatsError::Conflict(_) => StatusCode::CONFLICT,
        StatsError::ProtocolRetired => StatusCode::GONE,
        StatsError::Invalid(_) => StatusCode::BAD_REQUEST,
        _ => {
            tracing::error!("stats store failed: {}", err);
            StatusCode::INTERNAL_SERVER_ERROR
        }
    };
    let message = if status == StatusCode::INTERNAL_SERVER_ERROR {
        "stats storage error".to_string()
    } else {
        err.to_string()
    };
    json_error(status, message)
}

fn resolve_stats_period(
    year: Option<i32>,
    month: Option<u8>,
) -> Result<(i32, Option<u8>), (StatusCode, Json<crate::state::ErrorResponse>)> {
    let (current_year, current_month) = current_year_month();
    let year_out = year.unwrap_or(current_year);
    if !(1970..=9998).contains(&year_out) {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "year must be 1970-9998".to_string(),
        ));
    }
    let month_out = match month {
        Some(value) if (1..=12).contains(&value) => Some(value),
        Some(_) => {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "month must be 1-12".to_string(),
            ))
        }
        None => {
            if year.is_none() {
                Some(current_month)
            } else {
                None
            }
        }
    };
    Ok((year_out, month_out))
}

fn current_year_month() -> (i32, u8) {
    let now = OffsetDateTime::now_utc();
    (now.year(), now.month() as u8)
}

fn top_n(map: &std::collections::HashMap<String, u64>, limit: usize) -> Vec<(String, u64)> {
    let mut items: Vec<(String, u64)> = map
        .iter()
        .map(|(key, value)| (key.clone(), *value))
        .collect();
    items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    items.truncate(limit);
    items
}

fn genre_breakdown(map: &HashMap<String, u64>, total_ms: u64) -> (Vec<StatsItem>, u64, u64) {
    let classified_ms = map
        .values()
        .fold(0_u64, |total, value| total.saturating_add(*value));
    let ranked = top_n(map, 5);
    let visible_ms = ranked
        .iter()
        .fold(0_u64, |total, (_, value)| total.saturating_add(*value));
    let genres = ranked
        .into_iter()
        .map(|(genre, ms)| StatsItem {
            id: genre.clone(),
            name: common::genre_display_label(&genre),
            minutes: ms_to_minutes(ms),
            listening_ms: ms,
        })
        .collect();
    (
        genres,
        classified_ms.saturating_sub(visible_ms),
        total_ms.saturating_sub(classified_ms),
    )
}

fn ms_to_minutes(value: u64) -> u64 {
    value / 60000
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf, sync::Arc};

    struct TestDirectory(PathBuf);

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn catalog_repairs_exact_recordings_using_album_genres_and_keeps_raw_history() {
        let directory = TestDirectory(
            std::env::temp_dir().join(format!("phonolite-stats-catalog-{}", uuid::Uuid::new_v4())),
        );
        let music = directory.0.join("music");
        fs::create_dir_all(&music).unwrap();
        // A minimal indexed FLAC with the same broad file tags as the reported
        // Jamiroquai recording. No audio device or server is involved.
        let mut stream = [0_u8; 34];
        stream[..2].copy_from_slice(&4096_u16.to_be_bytes());
        stream[2..4].copy_from_slice(&4096_u16.to_be_bytes());
        stream[10..18]
            .copy_from_slice(&((44_100_u64 << 44) | (1 << 41) | (15 << 36) | 44_100).to_be_bytes());
        let mut comments = Vec::new();
        comments.extend_from_slice(&4_u32.to_le_bytes());
        comments.extend_from_slice(b"test");
        let tags = [
            "ARTIST=Jamiroquai",
            "ALBUMARTIST=Jamiroquai",
            "ALBUM=Travelling Without Moving",
            "TITLE=Virtual Insanity",
            "GENRE=Rock;Pop",
        ];
        comments.extend_from_slice(&(tags.len() as u32).to_le_bytes());
        for tag in tags {
            comments.extend_from_slice(&(tag.len() as u32).to_le_bytes());
            comments.extend_from_slice(tag.as_bytes());
        }
        let mut bytes = b"fLaC".to_vec();
        bytes.extend_from_slice(&[0, 0, 0, 34]);
        bytes.extend_from_slice(&stream);
        bytes.push(0x84);
        bytes.extend_from_slice(&(comments.len() as u32).to_be_bytes()[1..]);
        bytes.extend_from_slice(&comments);
        fs::write(music.join("track.flac"), bytes).unwrap();
        let library = ::library::Library::load_or_scan(
            vec![::library::LibraryRoot::new(String::new(), music)],
            directory.0.join("library.redb"),
        )
        .unwrap()
        .0;
        let track = library.list_tracks(None, 1, 0).unwrap().0.remove(0);
        library
            .update_album_enrichment(
                &track.album_id,
                None,
                &["acid jazz".into(), "Electronic".into(), "funk".into()],
                true,
            )
            .unwrap();
        let store = StatsStore::new(Arc::new(
            redb::Database::create(directory.0.join("stats.redb")).unwrap(),
        ));
        store.init_tables().unwrap();
        let start = 1_791_446_400_000;
        let event = ListeningEvent {
            event_id: "event".into(),
            session_id: "session".into(),
            device_id: "device".into(),
            track_id: track.id.clone(),
            title: track.title.clone(),
            artist_id: track.artist_id.clone(),
            artist_name: "Jamiroquai".into(),
            album_id: track.album_id.clone(),
            album_title: "Travelling Without Moving".into(),
            artists: Vec::new(),
            genres: vec!["Rock".into(), "Pop".into()],
            track_duration_ms: 180_000,
            session_started_at_ms: start,
            started_at_ms: start,
            ended_at_ms: start + 60_000,
            listened_ms: 60_000,
            rule_version: 1,
            end_reason: None,
        };
        store
            .ingest_events("user", "device", 1, &[event.clone()])
            .unwrap();
        let metadata = refresh_genre_metadata(
            &store,
            "user",
            Some(&library),
            [track.id.clone(), "missing-catalog-recording".into()],
        )
        .unwrap();
        assert_eq!(metadata.len(), 1);
        assert_eq!(metadata[&track.id], ["Acid Jazz", "Electronic", "Funk"]);
        let period = store.get_period("user", 2026, Some(10)).unwrap().unwrap();
        assert_eq!(period.total_ms, 60_000);
        assert_eq!(period.genre_ms.len(), 3);
        assert!(period.genre_ms.values().all(|duration| *duration == 20_000));
        let synced = store.sync_events("user", 0, 200, true, None).unwrap();
        assert_eq!(synced.events[0].event, event);
        assert_eq!(synced.genre_metadata[&track.id].len(), 3);
        // An unavailable catalog leaves the previous derived projection alone.
        assert!(refresh_genre_metadata(&store, "user", None, [track.id])
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .get_period("user", 2026, Some(10))
                .unwrap()
                .unwrap()
                .genre_ms,
            period.genre_ms
        );
    }

    #[test]
    fn rankings_preserve_subminute_listening_duration() {
        let response = serde_json::to_value(StatsItem {
            id: "Jazz".into(),
            name: "Jazz".into(),
            minutes: ms_to_minutes(15_000),
            listening_ms: 15_000,
        })
        .unwrap();
        assert_eq!(response["minutes"], 0);
        assert_eq!(response["listening_ms"], 15_000);
    }

    #[test]
    fn genre_breakdown_accounts_for_hidden_and_untagged_listening() {
        let genres = [
            ("Acid Jazz".into(), 20_001),
            ("Electronic".into(), 15_000),
            ("Funk".into(), 10_000),
            ("Metal".into(), 8_000),
            ("Rock".into(), 5_000),
            ("Dance-pop".into(), 4_000),
            ("Motown".into(), 2_000),
        ]
        .into_iter()
        .collect();
        let total_ms = 70_123;
        let (top, other, unclassified) = genre_breakdown(&genres, total_ms);
        assert_eq!(top.len(), 5);
        assert_eq!(top[0].name, "Acid Jazz");
        assert_eq!(other, 6_000);
        assert_eq!(unclassified, 6_122);
        assert_eq!(
            top.iter().map(|item| item.listening_ms).sum::<u64>() + other + unclassified,
            total_ms
        );
        // Legacy aggregate baselines may overlap genres. Unknown duration must
        // never become negative while reporting their preserved old totals.
        let (_, _, unknown) = genre_breakdown(&genres, 1_000);
        assert_eq!(unknown, 0);
    }

    #[test]
    fn genre_display_labels_do_not_change_report_keys_or_durations() {
        let genres = [("edm".into(), 20_000), ("acid jazz".into(), 10_000)]
            .into_iter()
            .collect();
        let (top, other, unclassified) = genre_breakdown(&genres, 30_000);
        assert_eq!((top[0].id.as_str(), top[0].name.as_str()), ("edm", "EDM"));
        assert_eq!(
            (top[1].id.as_str(), top[1].name.as_str()),
            ("acid jazz", "Acid Jazz")
        );
        assert_eq!(top[0].listening_ms, 20_000);
        assert_eq!((other, unclassified), (0, 0));
    }
}
