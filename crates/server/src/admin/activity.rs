use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

use crate::state::AppState;
use crate::activity_store::ActivityEntry;
use crate::utils::{
    apply_template, escape_html, html_error, html_response, json_error_response, json_ok_response,
    load_template, redirect_to, render_admin_page, wants_json, PageLayout,
};

use super::{admin_user_from_headers, is_admin, library_for_admin};
use super::library::render_status_block_for_library;

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
    let (stored_events, stored_total) = state
        .activity
        .list_events(200, 0)
        .unwrap_or((Vec::new(), 0));
    let (events_html, active_count, status_label) = build_events(&state, &stored_events);
    let (tag_errors_html, issue_count) = match library_for_admin(&state) {
        Ok(library) => match library.list_tag_error_files(200, 0) {
            Ok((items, total)) => (render_tag_error_files(&items), total),
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

    let body = apply_template(template, &[
        ("status_block", status_block),
        ("events", events_html),
        ("event_count", (active_count + stored_total).to_string()),
        ("stored_count", stored_total.to_string()),
        ("issue_count", issue_count.to_string()),
        ("status_label", status_label),
        ("tag_errors", tag_errors_html),
    ]);

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
    if wants_json(&headers) {
        json_ok_response()
    } else {
        redirect_to("/activity")
    }
}

pub async fn admin_activity_status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !state.auth.has_admin().unwrap_or(false) {
        return (StatusCode::UNAUTHORIZED, Json(ActivityStatusResponse {
            count: 0,
            status: "unauthorized".to_string(),
            issues: 0,
        }))
        .into_response();
    }
    let user = match admin_user_from_headers(&state, &headers) {
        Ok(Some(user)) => user,
        Ok(None) => {
            return (StatusCode::UNAUTHORIZED, Json(ActivityStatusResponse {
                count: 0,
                status: "unauthorized".to_string(),
                issues: 0,
            }))
            .into_response()
        }
        Err(err) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(ActivityStatusResponse {
                count: 0,
                status: format!("auth error: {}", err),
                issues: 0,
            }))
            .into_response()
        }
    };
    if !is_admin(&user) {
        return (StatusCode::FORBIDDEN, Json(ActivityStatusResponse {
            count: 0,
            status: "forbidden".to_string(),
            issues: 0,
        }))
        .into_response();
    }

    let (stored_events, stored_total) = state
        .activity
        .list_events(1, 0)
        .unwrap_or((Vec::new(), 0));
    let (_, active_count, status_label) = build_events(&state, &stored_events);
    let issues = match library_for_admin(&state) {
        Ok(library) => library
            .list_tag_error_files(1, 0)
            .map(|(_, total)| total)
            .unwrap_or(0),
        Err(_) => 0,
    };
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

fn build_events(state: &AppState, stored: &[ActivityEntry]) -> (String, usize, String) {
    use crate::state::LibraryStatus;
    let guard = state.library_state.read();
    let mut events = Vec::new();
    let status_label = match &guard.status {
        LibraryStatus::Unconfigured => {
            events.push("Library not configured yet.".to_string());
            "unconfigured".to_string()
        }
        LibraryStatus::Missing(path) => {
            events.push(format!("Music directory missing: {}", path.display()));
            "missing".to_string()
        }
        LibraryStatus::Scanning { started } => {
            let since = started
                .elapsed()
                .map(|elapsed| format!("{}s", elapsed.as_secs()))
                .unwrap_or_else(|_| "unknown".to_string());
            events.push(format!("Library scan in progress (started {}).", since));
            "scanning".to_string()
        }
        LibraryStatus::Ready(_) => "ready".to_string(),
        LibraryStatus::Error(message) => {
            events.push(format!("Library error: {}", message));
            "error".to_string()
        }
    };

    if events.is_empty() && stored.is_empty() {
        return (
            "<p class=\"muted\">No active events.</p>".to_string(),
            0,
            status_label,
        );
    }

    let mut out = String::from("<ul>");
    for event in &events {
        out.push_str(&format!("<li>{}</li>", escape_html(event)));
    }
    for item in stored {
        out.push_str(&format!(
            "<li><span class=\"muted\">{}</span> {}</li>",
            escape_html(&item.kind),
            escape_html(&item.message)
        ));
    }
    out.push_str("</ul>");
    (out, events.len(), status_label)
}

fn render_tag_error_files(items: &[library::TagErrorFile]) -> String {
    if items.is_empty() {
        return "<p class=\"muted\">No indexing issues detected.</p>".to_string();
    }

    let mut rows = String::new();
    for item in items {
        let filename = item
            .file_relpath
            .split('/')
            .last()
            .unwrap_or(item.file_relpath.as_str());
        rows.push_str(&format!(
            "<tr><td>{}</td><td><code>{}</code></td><td>{}</td></tr>",
            escape_html(filename),
            escape_html(&item.file_relpath),
            escape_html(&item.error),
        ));
    }

    format!(
        "<table><thead><tr><th>File</th><th>Location</th><th>Error</th></tr></thead><tbody>{}</tbody></table>",
        rows
    )
}
