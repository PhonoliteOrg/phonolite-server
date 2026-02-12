use axum::{
    extract::{Query, State},
    http::StatusCode,
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::state::{AppState, AuthContext, JsonResult};
use crate::utils::json_error;

use super::library_or_json_error;

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
    pub top_tracks: Vec<StatsTrack>,
    pub top_artists: Vec<StatsItem>,
    pub top_genres: Vec<StatsItem>,
}

#[derive(Serialize)]
pub struct StatsItem {
    pub id: String,
    pub name: String,
    pub minutes: u64,
}

#[derive(Serialize)]
pub struct StatsTrack {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub minutes: u64,
}

pub async fn get_stats(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Query(query): Query<StatsQuery>,
) -> JsonResult<StatsResponse> {
    if !state.config.read().stats_collection_enabled {
        return Err(json_error(
            StatusCode::FORBIDDEN,
            "stats collection disabled".to_string(),
        ));
    }
    let library = library_or_json_error(&state)?;
    let (year, month) = resolve_stats_period(query.year, query.month)?;
    let stats = match state.stats.get_period(&ctx.user.id, year, month) {
        Ok(stats) => stats,
        Err(err) => {
            return Err(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("stats error: {}", err),
            ))
        }
    };

    let Some(stats) = stats else {
        return Ok(Json(StatsResponse {
            year,
            month,
            total_minutes: 0,
            top_tracks: Vec::new(),
            top_artists: Vec::new(),
            top_genres: Vec::new(),
        }));
    };

    let top_tracks = top_n(&stats.track_ms, 5)
        .into_iter()
        .map(|(track_id, ms)| {
            let (title, artist_name) = match library.get_track(&track_id) {
                Ok(Some(track)) => {
                    let artist_name = library
                        .get_artist(&track.artist_id)
                        .ok()
                        .flatten()
                        .map(|artist| artist.name)
                        .unwrap_or_else(|| "Unknown Artist".to_string());
                    (track.title, artist_name)
                }
                _ => ("Unknown Track".to_string(), "Unknown Artist".to_string()),
            };
            StatsTrack {
                id: track_id,
                title,
                artist: artist_name,
                minutes: ms_to_minutes(ms),
            }
        })
        .collect();

    let top_artists = top_n(&stats.artist_ms, 5)
        .into_iter()
        .map(|(artist_id, ms)| {
            let name = library
                .get_artist(&artist_id)
                .ok()
                .flatten()
                .map(|artist| artist.name)
                .unwrap_or_else(|| "Unknown Artist".to_string());
            StatsItem {
                id: artist_id,
                name,
                minutes: ms_to_minutes(ms),
            }
        })
        .collect();

    let top_genres = top_n(&stats.genre_ms, 5)
        .into_iter()
        .map(|(genre, ms)| StatsItem {
            id: genre.clone(),
            name: genre,
            minutes: ms_to_minutes(ms),
        })
        .collect();

    Ok(Json(StatsResponse {
        year,
        month,
        total_minutes: ms_to_minutes(stats.total_ms),
        top_tracks,
        top_artists,
        top_genres,
    }))
}

fn resolve_stats_period(
    year: Option<i32>,
    month: Option<u8>,
) -> Result<(i32, Option<u8>), (StatusCode, Json<crate::state::ErrorResponse>)> {
    let (current_year, current_month) = current_year_month();
    let year_out = year.unwrap_or(current_year);
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

fn ms_to_minutes(value: u64) -> u64 {
    value / 60000
}
