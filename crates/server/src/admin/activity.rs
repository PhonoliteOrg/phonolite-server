use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::config::resolve_path;
use crate::logging::{LOG_ACTIVITY_FILE, LOG_ISSUE_FILE};
use crate::state::AppState;
use crate::utils::{
    apply_template, escape_html, html_error, html_response, json_error_response, json_ok_response,
    load_template, redirect_to, render_admin_page, wants_json, PageLayout,
};

use super::library::render_status_block_for_library;
use super::{admin_user_from_headers, is_admin, library_for_admin};

#[derive(Serialize)]
struct ActivityStatusResponse {
    count: usize,
    status: String,
    issues: usize,
}

pub async fn admin_activity(State(state): State<AppState>, headers: HeaderMap) -> Response {
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

    let status_block = render_status_block_for_library(&state);
    let (stored_events, stored_total) = read_activity_log(&state, 200);
    let (events_html, active_count, status_label) = build_events(&state, &stored_events);
    let (tag_errors_html, issue_count) = match read_issue_log(&state, 200) {
        Ok((items, total)) => (render_issue_log(&items), total),
        Err(_) => match library_for_admin(&state) {
            Ok(library) => match library.list_tag_error_files(200, 0) {
                Ok((items, total)) => (render_tag_error_files(&library, &items), total),
                Err(err) => (
                    format!(
                        "<p class=\"muted\">Failed to load indexing issues: {}</p>",
                        escape_html(&err.to_string())
                    ),
                    0,
                ),
            },
            Err(message) => (
                format!(
                    "<p class=\"muted\">Library unavailable: {}</p>",
                    escape_html(&message)
                ),
                0,
            ),
        },
    };

    let template = match load_template(&state, "templates/activity.html") {
        Ok(template) => template,
        Err(err) => {
            return html_error(
                &state,
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("template error: {}", err),
            )
        }
    };

    let body = apply_template(
        template,
        &[
            ("status_block", status_block),
            ("events", events_html),
            ("event_count", (active_count + stored_total).to_string()),
            ("stored_count", stored_total.to_string()),
            ("issue_count", issue_count.to_string()),
            ("status_label", status_label),
            ("tag_errors", tag_errors_html),
        ],
    );

    html_response(
        StatusCode::OK,
        render_admin_page(&state, "Activity", &body, PageLayout::standard()),
    )
}

pub async fn admin_activity_clear(State(state): State<AppState>, headers: HeaderMap) -> Response {
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

    let _ = state.activity.clear_events();
    let log_dir = resolve_path(&state.config_path, &state.config.read().log_dir);
    let _ = std::fs::write(log_dir.join(LOG_ACTIVITY_FILE), "");
    if wants_json(&headers) {
        json_ok_response()
    } else {
        redirect_to("/activity")
    }
}

pub async fn admin_activity_status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !state.auth.has_admin().unwrap_or(false) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(ActivityStatusResponse {
                count: 0,
                status: "unauthorized".to_string(),
                issues: 0,
            }),
        )
            .into_response();
    }
    let user = match admin_user_from_headers(&state, &headers) {
        Ok(Some(user)) => user,
        Ok(None) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(ActivityStatusResponse {
                    count: 0,
                    status: "unauthorized".to_string(),
                    issues: 0,
                }),
            )
                .into_response()
        }
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ActivityStatusResponse {
                    count: 0,
                    status: format!("auth error: {}", err),
                    issues: 0,
                }),
            )
                .into_response()
        }
    };
    if !is_admin(&user) {
        return (
            StatusCode::FORBIDDEN,
            Json(ActivityStatusResponse {
                count: 0,
                status: "forbidden".to_string(),
                issues: 0,
            }),
        )
            .into_response();
    }

    let (_, stored_total) = read_activity_log(&state, 1);
    let (_, active_count, status_label) = build_events(&state, &[]);
    let issues = read_issue_log(&state, 1)
        .map(|(_, total)| total)
        .unwrap_or(0);
    let count = active_count + stored_total + issues;

    (
        StatusCode::OK,
        Json(ActivityStatusResponse {
            count,
            status: status_label,
            issues,
        }),
    )
        .into_response()
}

fn build_events(state: &AppState, stored: &[ActivityLogItem]) -> (String, usize, String) {
    use crate::state::LibraryStatus;
    let guard = state.library_state.read();
    let mut events: Vec<(String, String, Option<u64>, bool)> = Vec::new();
    let status_label = match &guard.status {
        LibraryStatus::Unconfigured => {
            events.push((
                "ACTIVITY".to_string(),
                "Library not configured yet.".to_string(),
                None,
                true,
            ));
            "unconfigured".to_string()
        }
        LibraryStatus::Missing(path) => {
            events.push((
                "ERROR".to_string(),
                format!("Music directory missing: {}", path.display()),
                None,
                true,
            ));
            "missing".to_string()
        }
        LibraryStatus::Scanning { started } => {
            let since = started
                .elapsed()
                .map(|elapsed| format!("{}s", elapsed.as_secs()))
                .unwrap_or_else(|_| "unknown".to_string());
            events.push((
                "ACTIVITY".to_string(),
                format!("Library scan in progress (started {}).", since),
                None,
                true,
            ));
            "scanning".to_string()
        }
        LibraryStatus::Ready(_) => "ready".to_string(),
        LibraryStatus::Error(message) => {
            events.push((
                "ERROR".to_string(),
                format!("Library error: {}", message),
                None,
                true,
            ));
            "error".to_string()
        }
    };

    if events.is_empty() && stored.is_empty() {
        return (
            "<p class=\"muted\">No activity yet.</p>".to_string(),
            0,
            status_label,
        );
    }

    let mut out = String::new();
    for (kind, message, created_at, active) in &events {
        let (label, class_name) = format_kind(kind);
        let time = created_at
            .map(format_relative_time)
            .unwrap_or_else(|| "now".to_string());
        let active_class = if *active { " active" } else { "" };
        out.push_str(&format!(
            "<div class=\"activity-item{}\"><span class=\"activity-tag {}\">{}</span><div class=\"activity-body\"><div class=\"activity-message\">{}</div><div class=\"activity-meta\">{}</div></div></div>",
            active_class,
            class_name,
            escape_html(&label),
            escape_html(message),
            escape_html(&time)
        ));
    }
    for item in stored {
        let (label, class_name) = format_kind(&item.kind);
        let time = item
            .created_at
            .map(format_relative_time)
            .unwrap_or_else(|| "recent".to_string());
        out.push_str(&format!(
            "<div class=\"activity-item\"><span class=\"activity-tag {}\">{}</span><div class=\"activity-body\"><div class=\"activity-message\">{}</div><div class=\"activity-meta\">{}</div></div></div>",
            class_name,
            escape_html(&label),
            escape_html(&item.message),
            escape_html(&time)
        ));
    }
    (out, events.len(), status_label)
}

fn format_kind(kind: &str) -> (String, &'static str) {
    let label = kind.trim().to_uppercase();
    if label.is_empty() {
        return ("INFO".to_string(), "info");
    }
    let class = match label.as_str() {
        "ACTIVITY" => "activity",
        "ERROR" => "error",
        "WARN" | "WARNING" => "warn",
        "METADATA" => "meta",
        "INDEX" => "index",
        "SCAN" => "scan",
        _ => "info",
    };
    (label, class)
}

fn format_relative_time(created_at: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(created_at);
    let delta = now.saturating_sub(created_at);
    if delta < 10 {
        return "just now".to_string();
    }
    if delta < 60 {
        return format!("{}s ago", delta);
    }
    let mins = delta / 60;
    if mins < 60 {
        return format!("{}m ago", mins);
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{}h ago", hours);
    }
    let days = hours / 24;
    format!("{}d ago", days)
}

struct ActivityLogItem {
    message: String,
    created_at: Option<u64>,
    kind: String,
}

fn read_activity_log(state: &AppState, limit: usize) -> (Vec<ActivityLogItem>, usize) {
    let log_dir = resolve_path(&state.config_path, &state.config.read().log_dir);
    let log_path = log_dir.join(LOG_ACTIVITY_FILE);
    match read_log_tail(&log_path, limit) {
        Ok((lines, total)) => {
            let mut out = Vec::new();
            for line in lines.into_iter().rev() {
                if let Some(item) = parse_activity_line(&line) {
                    out.push(item);
                }
            }
            (out, total)
        }
        Err(_) => (Vec::new(), 0),
    }
}

#[derive(Clone)]
struct IssueLogItem {
    file: String,
    folder: Option<String>,
    error: String,
}

fn read_issue_log(state: &AppState, limit: usize) -> Result<(Vec<IssueLogItem>, usize), String> {
    let log_dir = resolve_path(&state.config_path, &state.config.read().log_dir);
    let log_path = log_dir.join(LOG_ISSUE_FILE);
    let (lines, total) = read_log_tail(&log_path, limit)?;
    let mut out = Vec::new();
    for line in lines.into_iter().rev() {
        if let Some(item) = parse_issue_line(&line) {
            out.push(item);
        }
    }
    Ok((out, total))
}

fn read_log_tail(path: &PathBuf, limit: usize) -> Result<(Vec<String>, usize), String> {
    if !path.exists() {
        return Ok((Vec::new(), 0));
    }
    let contents = std::fs::read_to_string(path).map_err(|err| err.to_string())?;
    let mut lines: Vec<String> = contents.lines().map(|line| line.to_string()).collect();
    let total = lines.len();
    if lines.len() > limit {
        lines = lines.split_off(lines.len().saturating_sub(limit));
    }
    Ok((lines, total))
}

fn parse_activity_line(line: &str) -> Option<ActivityLogItem> {
    let (ts, level, rest) = split_log_line(line)?;
    if level != "ACTIVITY" {
        return None;
    }
    let created_at = parse_log_timestamp(ts);
    let message = clean_message(rest);
    Some(ActivityLogItem {
        message,
        created_at,
        kind: "ACTIVITY".to_string(),
    })
}

fn parse_issue_line(line: &str) -> Option<IssueLogItem> {
    let (_, level, rest) = split_log_line(line)?;
    if level != "ISSUE" {
        return None;
    }
    let message = clean_message(rest);
    let mut file = None;
    let mut folder = None;
    let mut error = None;
    for part in message.split(" | ") {
        if let Some((key, value)) = part.split_once('=') {
            let value = value.trim().trim_matches('"');
            match key.trim() {
                "file" => file = Some(value.to_string()),
                "folder" => folder = Some(value.to_string()),
                "error" => error = Some(value.to_string()),
                _ => {}
            }
        }
    }
    let file = file?;
    let error = error.unwrap_or_else(|| "Unknown error".to_string());
    Some(IssueLogItem {
        file,
        folder,
        error,
    })
}

fn split_log_line(line: &str) -> Option<(&str, &str, &str)> {
    let mut parts = line.splitn(3, ' ');
    let ts = parts.next()?;
    let level = parts.next()?;
    let rest = parts.next().unwrap_or("");
    Some((ts, level, rest))
}

fn clean_message(input: &str) -> String {
    let trimmed = input.trim();
    if let Some(value) = trimmed.strip_prefix("message=") {
        let value = value.trim();
        if let Some(stripped) = value.strip_prefix('"') {
            if let Some(end) = stripped.find('"') {
                return stripped[..end].to_string();
            }
        }
        return value.trim_matches('"').to_string();
    }
    trimmed.to_string()
}

fn parse_log_timestamp(value: &str) -> Option<u64> {
    let parsed = OffsetDateTime::parse(value, &Rfc3339).ok()?;
    let ts = parsed.unix_timestamp();
    if ts < 0 {
        None
    } else {
        Some(ts as u64)
    }
}

fn render_issue_log(items: &[IssueLogItem]) -> String {
    if items.is_empty() {
        return "<p class=\"muted\">No indexing issues detected.</p>".to_string();
    }

    let mut rows = String::new();
    for item in items {
        let filename = item.file.split('/').last().unwrap_or(item.file.as_str());
        let folder = item.folder.clone().unwrap_or_else(|| "-".to_string());
        rows.push_str(&format!(
            "<tr><td>{}</td><td><code>{}</code></td><td>{}</td><td>{}</td></tr>",
            escape_html(filename),
            escape_html(&item.file),
            escape_html(&folder),
            escape_html(&item.error),
        ));
    }

    format!(
        "<table><thead><tr><th>File</th><th>Location</th><th>Folder</th><th>Error</th></tr></thead><tbody>{}</tbody></table>",
        rows
    )
}

fn render_tag_error_files(library: &library::Library, items: &[library::TagErrorFile]) -> String {
    if items.is_empty() {
        return "<p class=\"muted\">No indexing issues detected.</p>".to_string();
    }

    let mut rows = String::new();
    for item in items {
        let display_path = library
            .resolve_relpath(&item.file_relpath)
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| item.file_relpath.clone());
        let filename = Path::new(&display_path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(display_path.as_str());
        rows.push_str(&format!(
            "<tr><td>{}</td><td><code>{}</code></td><td>{}</td></tr>",
            escape_html(filename),
            escape_html(&display_path),
            escape_html(&item.error),
        ));
    }

    format!(
        "<table><thead><tr><th>File</th><th>Location</th><th>Error</th></tr></thead><tbody>{}</tbody></table>",
        rows
    )
}
