mod issues;

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::admin::presentation::{
    apply_template, escape_html, html_error, load_template, render_admin_response, PageLayout,
};
use crate::config::resolve_path;
use crate::http::{json_error_response, json_ok_response, redirect_to, wants_json};
use crate::logging::{
    LOG_ACTIVITY_FILE, LOG_ALL_FILE, LOG_DEBUG_FILE, LOG_ERROR_FILE, LOG_INFO_FILE, LOG_ISSUE_FILE,
    LOG_MAX_LINES, LOG_WARN_FILE,
};
use crate::state::AppState;

use super::console::wants_console_view;
use super::{admin_user_from_headers, is_admin};

const LOG_UI_DEFAULT_LINES: usize = 1_000;

#[derive(Deserialize)]
pub struct LogTailQuery {
    pub lines: Option<usize>,
    pub view: Option<String>,
    pub revision: Option<String>,
}

#[derive(Serialize)]
struct LogTailResponse {
    lines: Vec<String>,
    total: usize,
    updated_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    revision: Option<String>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum LogTailSnapshot {
    Full(LogTailResponse),
    Unchanged {
        revision: String,
        unchanged: bool,
        updated_at: u64,
    },
}

pub async fn admin_logs(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !state.auth.has_admin().unwrap_or(false) {
        if wants_console_view(&headers) {
            return redirect_to("/setup");
        }
        return html_error(
            &state,
            StatusCode::UNAUTHORIZED,
            "admin access required".to_string(),
        );
    }
    let user = match admin_user_from_headers(&state, &headers) {
        Ok(Some(user)) => user,
        Ok(None) => {
            if wants_console_view(&headers) {
                return redirect_to("/login");
            }
            return html_error(
                &state,
                StatusCode::UNAUTHORIZED,
                "admin access required".to_string(),
            );
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
    let log_path = |view| escape_html(&log_file_for_view(&log_dir, view).to_string_lossy());
    let log_debug_checked = if state.config.read().log_debug_enabled {
        "checked".to_string()
    } else {
        String::new()
    };
    let (tag_errors, issue_count) = match load_indexing_issues(&state).await {
        Ok(issues) => (issues.html, issues.total),
        Err(message) => (
            format!(
                "<p class=\"muted\">Failed to load indexing issues: {}</p>",
                escape_html(&message)
            ),
            0,
        ),
    };
    let body = apply_template(
        template,
        &[
            ("log_all_path", log_path("all")),
            ("log_info_path", log_path("info")),
            ("log_warnings_path", log_path("warnings")),
            ("log_errors_path", log_path("errors")),
            ("log_issues_path", log_path("issues")),
            ("log_activities_path", log_path("activities")),
            ("log_debug_path", log_path("debug")),
            ("log_limit", LOG_UI_DEFAULT_LINES.to_string()),
            ("log_debug_checked", log_debug_checked),
            ("tag_errors", tag_errors),
            ("issue_count", issue_count.to_string()),
        ],
    );

    render_admin_response(
        &state,
        StatusCode::OK,
        "Logs",
        &body,
        PageLayout::standard(),
    )
}

async fn load_indexing_issues(state: &AppState) -> Result<issues::IndexingIssuesResponse, String> {
    let state = state.clone();
    tokio::task::spawn_blocking(move || issues::load_indexing_issues(&state))
        .await
        .map_err(|err| err.to_string())?
}

pub async fn admin_logs_issues(State(state): State<AppState>, headers: HeaderMap) -> Response {
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
    match load_indexing_issues(&state).await {
        Ok(issues) => Json(issues).into_response(),
        Err(err) => json_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to load indexing issues: {}", err),
        ),
    }
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
                revision: None,
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
                    revision: None,
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
                    revision: None,
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
                revision: None,
            }),
        )
            .into_response();
    }

    let log_dir = resolve_path(&state.config_path, &state.config.read().log_dir);
    let limit = query
        .lines
        .unwrap_or(LOG_UI_DEFAULT_LINES)
        .min(LOG_MAX_LINES);
    let view = query.view.as_deref().unwrap_or("all");
    let log_path = log_file_for_view(&log_dir, view);
    let include_issues = view == "issues";
    let result = tokio::task::spawn_blocking(move || {
        read_log_tail_snapshot(&log_path, limit, include_issues, query.revision.as_deref())
    })
    .await;
    match result {
        Ok(Ok(snapshot)) => Json(snapshot).into_response(),
        Ok(Err(err)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(LogTailResponse {
                lines: vec![format!("Failed to read logs: {}", err)],
                total: 1,
                updated_at: 0,
                revision: None,
            }),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(LogTailResponse {
                lines: vec![format!("Failed to read logs: {}", err)],
                total: 1,
                updated_at: 0,
                revision: None,
            }),
        )
            .into_response(),
    }
}

fn log_revision(path: &Path, limit: usize, include_issues: bool) -> Option<(String, u64)> {
    let binding = format!("{}:{limit}:{include_issues}", path.to_string_lossy());
    let (fingerprint, updated_at) = match std::fs::metadata(path) {
        Ok(metadata) => {
            let modified = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
            (
                format!("{binding}:{}:{}", metadata.len(), modified.as_nanos()),
                modified.as_secs(),
            )
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            (format!("{binding}:missing"), 0)
        }
        Err(_) => return None,
    };
    Some((
        blake3::hash(fingerprint.as_bytes()).to_hex().to_string(),
        updated_at,
    ))
}

fn read_log_tail_snapshot(
    path: &PathBuf,
    limit: usize,
    include_issues: bool,
    previous_revision: Option<&str>,
) -> Result<LogTailSnapshot, String> {
    let before = log_revision(path, limit, include_issues);
    if let Some((revision, updated_at)) = &before {
        if previous_revision == Some(revision.as_str()) {
            return Ok(LogTailSnapshot::Unchanged {
                revision: revision.clone(),
                unchanged: true,
                updated_at: *updated_at,
            });
        }
    }
    let (lines, total, updated_at) = read_log_tail(path, limit, include_issues)?;
    let after = log_revision(path, limit, include_issues);
    // Concurrent writes during a read must not tag the older snapshot with a
    // newer revision, which could otherwise suppress its next refresh.
    let revision = if before == after {
        after.map(|(revision, _)| revision)
    } else {
        None
    };
    Ok(LogTailSnapshot::Full(LogTailResponse {
        lines,
        total,
        updated_at,
        revision,
    }))
}

fn read_log_tail(
    path: &PathBuf,
    limit: usize,
    include_issues: bool,
) -> Result<(Vec<String>, usize, u64), String> {
    if !path.exists() {
        return Ok((Vec::new(), 0, 0));
    }
    // Older all.log files may still contain issues from before their dedicated
    // routing. Keep those records out of the other views without rewriting logs.
    let (lines, total) = read_log_records(path, limit, |line| {
        (include_issues || line.split_whitespace().nth(1) != Some("ISSUE"))
            .then(|| line.to_string())
    })?;
    let updated_at = std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    Ok((lines, total, updated_at))
}

// Count the complete file while retaining only the requested tail. Polling
// must not allocate the full log plus an owned copy of every discarded row.
pub(super) fn read_log_records<T>(
    path: &Path,
    limit: usize,
    mut parse: impl FnMut(&str) -> Option<T>,
) -> Result<(Vec<T>, usize), String> {
    let file = File::open(path).map_err(|err| err.to_string())?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut tail = VecDeque::new();
    let mut total = 0usize;
    loop {
        line.clear();
        if reader.read_line(&mut line).map_err(|err| err.to_string())? == 0 {
            break;
        }
        let record = line.strip_suffix('\n').unwrap_or(&line);
        let record = record.strip_suffix('\r').unwrap_or(record);
        if let Some(item) = parse(record) {
            total = total.saturating_add(1);
            if limit > 0 {
                if tail.len() == limit {
                    tail.pop_front();
                }
                tail.push_back(item);
            }
        }
    }
    Ok((tail.into_iter().collect(), total))
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

#[cfg(test)]
mod tests {
    use super::{read_log_records, read_log_tail, read_log_tail_snapshot, LogTailSnapshot};

    #[test]
    fn log_revisions_skip_unchanged_reads_and_invalidate_for_append_clear_and_selection() {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!(
            "phonolite-tail-revision-{}.log",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(
            &path,
            "2026-09-30T12:00:00Z INFO first\n2026-09-30T12:00:01Z ISSUE file=bad.flac\n",
        )
        .unwrap();
        let LogTailSnapshot::Full(initial) = read_log_tail_snapshot(&path, 1, false, None).unwrap()
        else {
            panic!("initial full snapshot");
        };
        assert_eq!(initial.total, 1);
        let revision = initial.revision.unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        for _ in 0..100 {
            let snapshot = read_log_tail_snapshot(&path, 1, false, Some(&revision)).unwrap();
            let value = serde_json::to_value(snapshot).unwrap();
            assert_eq!(value["unchanged"], true);
            assert_eq!(value["revision"], revision);
            assert!(value.get("lines").is_none());
            assert!(value.get("total").is_none());
        }
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        assert!(matches!(
            read_log_tail_snapshot(&path, 2, false, Some(&revision)).unwrap(),
            LogTailSnapshot::Full(_)
        ));
        assert!(matches!(
            read_log_tail_snapshot(&path, 1, true, Some(&revision)).unwrap(),
            LogTailSnapshot::Full(_)
        ));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"2026-09-30T12:00:02Z INFO second\n")
            .unwrap();
        let LogTailSnapshot::Full(appended) =
            read_log_tail_snapshot(&path, 1, false, Some(&revision)).unwrap()
        else {
            panic!("append invalidates revision");
        };
        assert_eq!(appended.total, 2);
        assert!(appended.lines[0].ends_with("INFO second"));
        let appended_revision = appended.revision.unwrap();
        assert_ne!(appended_revision, revision);
        std::fs::write(&path, "").unwrap();
        let LogTailSnapshot::Full(cleared) =
            read_log_tail_snapshot(&path, 1, false, Some(&appended_revision)).unwrap()
        else {
            panic!("clear invalidates revision");
        };
        assert_eq!(cleared.total, 0);
        assert!(cleared.lines.is_empty());
        std::fs::remove_file(&path).unwrap();
        let LogTailSnapshot::Full(missing) =
            read_log_tail_snapshot(&path, 1, false, cleared.revision.as_deref()).unwrap()
        else {
            panic!("missing file invalidates revision");
        };
        assert_eq!(missing.total, 0);
        let missing_revision = missing.revision.unwrap();
        assert!(matches!(
            read_log_tail_snapshot(&path, 1, false, Some(&missing_revision)).unwrap(),
            LogTailSnapshot::Unchanged { .. }
        ));
        std::fs::write(&path, "new record\n").unwrap();
        assert!(matches!(
            read_log_tail_snapshot(&path, 1, false, Some(&missing_revision)).unwrap(),
            LogTailSnapshot::Full(_)
        ));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn log_tail_keeps_unicode_crlf_zero_limit_and_read_only_polling() {
        let path = std::env::temp_dir().join(format!(
            "phonolite-tail-bounded-{}.log",
            uuid::Uuid::new_v4()
        ));
        let original = "2026-09-30T12:00:00Z INFO é\r\n\r\n2026-09-30T12:00:01Z WARN 中\nfinal without newline";
        std::fs::write(&path, original).unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        for _ in 0..10 {
            let (lines, total, _) = read_log_tail(&path, 2, true).unwrap();
            assert_eq!(
                lines,
                ["2026-09-30T12:00:01Z WARN 中", "final without newline"]
            );
            assert_eq!(total, 4);
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        let (lines, total, _) = read_log_tail(&path, 0, true).unwrap();
        assert!(lines.is_empty());
        assert_eq!(total, 4);
        std::fs::write(&path, [0xff]).unwrap();
        assert!(read_log_tail(&path, 2, true).is_err());
        std::fs::write(&path, "").unwrap();
        assert!(read_log_tail(&path, 2, true).unwrap().0.is_empty());
        std::fs::remove_file(&path).unwrap();
        assert_eq!(read_log_tail(&path, 2, true).unwrap(), (Vec::new(), 0, 0));
    }

    #[test]
    fn log_records_retain_only_the_tail_while_counting_all_records() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        struct Item(usize, Arc<AtomicUsize>);
        impl Drop for Item {
            fn drop(&mut self) {
                self.1.fetch_sub(1, Ordering::Relaxed);
            }
        }
        let path = std::env::temp_dir().join(format!(
            "phonolite-record-bound-{}.log",
            uuid::Uuid::new_v4()
        ));
        let records = (0..1000)
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, records).unwrap();
        let live = Arc::new(AtomicUsize::new(0));
        let mut maximum = 0;
        let (tail, total) = read_log_records(&path, 3, |line| {
            let count = live.fetch_add(1, Ordering::Relaxed) + 1;
            maximum = maximum.max(count);
            Some(Item(line.parse().unwrap(), live.clone()))
        })
        .unwrap();
        assert_eq!(total, 1000);
        assert_eq!(
            tail.iter().map(|item| item.0).collect::<Vec<_>>(),
            [997, 998, 999]
        );
        assert_eq!(maximum, 4, "retention grew with the complete file");
        drop(tail);
        assert_eq!(live.load(Ordering::Relaxed), 0);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn other_log_views_exclude_historical_issues_before_counting_and_limiting() {
        let path =
            std::env::temp_dir().join(format!("phonolite-log-tail-{}.log", uuid::Uuid::new_v4()));
        std::fs::write(
            &path,
            "2026-09-30T12:00:00Z INFO first\n\
             2026-09-30T12:00:01Z ISSUE file=bad.flac | error=bad\n\
             2026-09-30T12:00:02Z WARN message=ISSUE mentioned in a normal warning\n\
             2026-09-30T12:00:03Z ISSUE file=other.flac | error=bad\n",
        )
        .unwrap();
        let (lines, total, _) = read_log_tail(&path, 1, false).unwrap();
        assert_eq!(total, 2);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("WARN message=ISSUE"));
        let (lines, total, _) = read_log_tail(&path, 1, true).unwrap();
        assert_eq!(total, 4);
        assert!(lines[0].contains("ISSUE file=other.flac"));
        std::fs::remove_file(&path).unwrap();
    }
}
