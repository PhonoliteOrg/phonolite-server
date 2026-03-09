use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::config::resolve_path;
use crate::logging::{
    LOG_ACTIVITY_FILE, LOG_ALL_FILE, LOG_DEBUG_FILE, LOG_ERROR_FILE, LOG_INFO_FILE, LOG_ISSUE_FILE,
    LOG_MAX_LINES, LOG_WARN_FILE,
};
use crate::state::AppState;
use crate::utils::{
    apply_template, escape_html, html_error, html_response, json_error_response, json_ok_response,
    load_template, redirect_to, render_admin_page, wants_json, PageLayout,
};

use super::{admin_user_from_headers, is_admin};

#[derive(Deserialize)]
pub struct LogTailQuery {
    pub lines: Option<usize>,
    pub view: Option<String>,
}

#[derive(Serialize)]
struct LogTailResponse {
    lines: Vec<String>,
    total: usize,
    updated_at: u64,
}

pub async fn admin_logs(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !state.auth.has_admin().unwrap_or(false) {
        return html_error(
            &state,
            StatusCode::UNAUTHORIZED,
            "admin access required".to_string(),
        );
    }
    let user = match admin_user_from_headers(&state, &headers) {
        Ok(Some(user)) => user,
        Ok(None) => {
            return html_error(
                &state,
                StatusCode::UNAUTHORIZED,
                "admin access required".to_string(),
            )
        }
        Err(err) => {
            return html_error(
                &state,
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("auth error: {}", err),
            )
        }
    };
    if !is_admin(&user) {
        return html_error(&state, StatusCode::FORBIDDEN, "forbidden".to_string());
    }

    let template = match load_template(&state, "templates/logs.html") {
        Ok(template) => template,
        Err(err) => {
            return html_error(
                &state,
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("template error: {}", err),
            )
        }
    };

    let log_dir = resolve_path(&state.config_path, &state.config.read().log_dir);
    let log_debug_checked = if state.config.read().log_debug_enabled {
        "checked".to_string()
    } else {
        String::new()
    };
    let body = apply_template(
        template,
        &[
            ("log_dir", escape_html(&log_dir.to_string_lossy())),
            ("log_limit", LOG_MAX_LINES.to_string()),
            ("log_debug_checked", log_debug_checked),
        ],
    );

    html_response(
        StatusCode::OK,
        render_admin_page(&state, "Logs", &body, PageLayout::standard()),
    )
}

pub async fn admin_logs_tail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<LogTailQuery>,
) -> Response {
    if !state.auth.has_admin().unwrap_or(false) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(LogTailResponse {
                lines: Vec::new(),
                total: 0,
                updated_at: 0,
            }),
        )
            .into_response();
    }
    let user = match admin_user_from_headers(&state, &headers) {
        Ok(Some(user)) => user,
        Ok(None) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(LogTailResponse {
                    lines: Vec::new(),
                    total: 0,
                    updated_at: 0,
                }),
            )
                .into_response()
        }
        Err(_err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(LogTailResponse {
                    lines: Vec::new(),
                    total: 0,
                    updated_at: 0,
                }),
            )
                .into_response()
        }
    };
    if !is_admin(&user) {
        return (
            StatusCode::FORBIDDEN,
            Json(LogTailResponse {
                lines: Vec::new(),
                total: 0,
                updated_at: 0,
            }),
        )
            .into_response();
    }

    let log_dir = resolve_path(&state.config_path, &state.config.read().log_dir);
    let limit = query.lines.unwrap_or(LOG_MAX_LINES).min(LOG_MAX_LINES);
    let view = query.view.as_deref().unwrap_or("all");
    let log_path = log_file_for_view(&log_dir, view);
    match read_log_tail(&log_path, limit) {
        Ok((lines, total, updated_at)) => (
            StatusCode::OK,
            Json(LogTailResponse {
                lines,
                total,
                updated_at,
            }),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(LogTailResponse {
                lines: vec![format!("Failed to read logs: {}", err)],
                total: 1,
                updated_at: 0,
            }),
        )
            .into_response(),
    }
}

pub async fn admin_logs_clear(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !state.auth.has_admin().unwrap_or(false) {
        return json_error_response(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let user = match admin_user_from_headers(&state, &headers) {
        Ok(Some(user)) => user,
        Ok(None) => return json_error_response(StatusCode::UNAUTHORIZED, "unauthorized"),
        Err(err) => {
            return json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("auth error: {}", err),
            )
        }
    };
    if !is_admin(&user) {
        return json_error_response(StatusCode::FORBIDDEN, "forbidden");
    }

    if let Err(err) = state.log_control.clear_all() {
        return json_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to clear logs: {}", err),
        );
    }

    if wants_json(&headers) {
        json_ok_response()
    } else {
        redirect_to("/logs")
    }
}

fn read_log_tail(path: &PathBuf, limit: usize) -> Result<(Vec<String>, usize, u64), String> {
    if !path.exists() {
        return Ok((Vec::new(), 0, 0));
    }
    let contents = std::fs::read_to_string(path).map_err(|err| err.to_string())?;
    let mut lines: Vec<String> = contents.lines().map(|line| line.to_string()).collect();
    let total = lines.len();
    if lines.len() > limit {
        lines = lines.split_off(lines.len().saturating_sub(limit));
    }
    let updated_at = std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    Ok((lines, total, updated_at))
}

fn log_file_for_view(log_dir: &PathBuf, view: &str) -> PathBuf {
    let name = match view {
        "info" => LOG_INFO_FILE,
        "warnings" => LOG_WARN_FILE,
        "errors" => LOG_ERROR_FILE,
        "issues" => LOG_ISSUE_FILE,
        "activities" => LOG_ACTIVITY_FILE,
        "debug" => LOG_DEBUG_FILE,
        _ => LOG_ALL_FILE,
    };
    log_dir.join(name)
}
