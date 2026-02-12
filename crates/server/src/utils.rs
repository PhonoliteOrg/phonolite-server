use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use std::path::PathBuf;
use std::process::Command;

use crate::state::{AppState, ErrorResponse, HealthResponse};

pub fn json_error(
    status: StatusCode,
    message: impl Into<String>,
) -> (StatusCode, Json<ErrorResponse>) {
    (
        status,
        Json(ErrorResponse {
            error: message.into(),
        }),
    )
}

pub fn json_error_response(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorResponse {
            error: message.into(),
        }),
    )
    .into_response()
}

pub fn json_ok_response() -> Response {
    Json(HealthResponse { status: "ok" }).into_response()
}

pub fn wants_json(headers: &HeaderMap) -> bool {
    if let Some(value) = headers.get(header::ACCEPT) {
        if let Ok(value) = value.to_str() {
            if value.contains("application/json") {
                return true;
            }
        }
    }
    if let Some(value) = headers.get("X-Requested-With") {
        if let Ok(value) = value.to_str() {
            if value.eq_ignore_ascii_case("fetch") {
                return true;
            }
        }
    }
    false
}

pub fn redirect_to(path: &str) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::SEE_OTHER;
    let location = HeaderValue::from_str(path).unwrap_or_else(|_| HeaderValue::from_static("/"));
    response.headers_mut().insert(header::LOCATION, location);
    response
}

pub fn html_response(status: StatusCode, body: String) -> Response {
    let mut response = Html(body).into_response();
    *response.status_mut() = status;
    response
}

pub fn html_error(state: &AppState, status: StatusCode, message: String) -> Response {
    let template = load_template(state, "templates/partials/error.html").unwrap_or_else(|_| {
        "<div class=\"error-page\"><h2>Error {{status}}</h2><p>{{message}}</p></div>".to_string()
    });
    let body = apply_template(template, &[
        ("status", status.as_u16().to_string()),
        ("message", escape_html(&message)),
    ]);
    let html = render_admin_page(state, "Error", &body, PageLayout::centered());
    html_response(status, html)
}

pub fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

pub fn format_track_position(disc_no: Option<u16>, track_no: Option<u16>) -> String {
    match (disc_no, track_no) {
        (Some(d), Some(t)) => format!("{}.{:02}", d, t),
        (None, Some(t)) => format!("{:02}", t),
        _ => "-".to_string(),
    }
}

#[derive(Clone, Copy)]
pub struct PageLayout {
    pub nav: bool,
    pub centered: bool,
}

impl PageLayout {
    pub fn standard() -> Self {
        Self {
            nav: true,
            centered: false,
        }
    }

    pub fn centered() -> Self {
        Self {
            nav: false,
            centered: true,
        }
    }
}

pub fn render_admin_page(
    state: &AppState,
    title: &str,
    content: &str,
    layout: PageLayout,
) -> String {
    match load_template(state, "templates/layout.html") {
        Ok(template) => {
            let mut out = template.replace("{{title}}", &escape_html(title));
            let page_class = if layout.centered { "centered" } else { "" };
            let nav_class = if layout.nav { "" } else { "hidden" };
            let nav_template =
                load_template(state, "templates/partials/nav.html").unwrap_or_default();
            let footer_template =
                load_template(state, "templates/partials/footer.html").unwrap_or_default();
            let artist_detail_template =
                load_template(state, "templates/partials/artist_detail_modal.html")
                    .unwrap_or_default();
            let nav_html = apply_template(nav_template, &[("nav_class", nav_class.to_string())]);
            out = out.replace("{{page_class}}", page_class);
            out = out.replace("{{nav}}", &nav_html);
            out = out.replace("{{footer}}", &footer_template);
            out = out.replace("{{artist_detail}}", &artist_detail_template);
            out = out.replace("{{content}}", content);
            out
        }
        Err(err) => format!(
            "<!DOCTYPE html><html><head><meta charset=\"utf-8\" /><title>{}</title></head><body><h1>Phonolite admin</h1><div>{}</div><pre>{}</pre></body></html>",
            escape_html(title),
            content,
            escape_html(&err)
        ),
    }
}

pub fn apply_template(mut template: String, replacements: &[(&str, String)]) -> String {
    for (key, value) in replacements {
        let token = format!("{{{{{}}}}}", key);
        template = template.replace(&token, value);
    }
    template
}

pub fn load_template(state: &AppState, name: &str) -> Result<String, String> {
    let path = web_root(state).join(name);
    std::fs::read_to_string(&path)
        .map_err(|err| format!("failed to read {}: {}", path.display(), err))
}

pub fn web_root(state: &AppState) -> PathBuf {
    let base = state
        .config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let primary = base.join("web");
    if primary.exists() {
        return primary;
    }
    if let Ok(cwd) = std::env::current_dir() {
        let candidate = cwd.join("web");
        if candidate.exists() {
            return candidate;
        }
        let candidate = cwd.join("server").join("web");
        if candidate.exists() {
            return candidate;
        }
    }
    primary
}

pub fn spawn_restart() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|err| err.to_string())?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    Command::new(exe)
        .env("PHONOLITE_START_DELAY_MS", "1000")
        .args(args)
        .spawn()
        .map_err(|err| err.to_string())?;
    Ok(())
}

pub fn url_escape(input: &str) -> String {
    let mut out = String::new();
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~' => out.push(*byte as char),
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

pub fn format_duration_ms(duration_ms: u32) -> String {
    if duration_ms == 0 {
        return "-".to_string();
    }
    let total_secs = duration_ms / 1000;
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    if hours > 0 {
        format!("{}:{:02}:{:02}", hours, minutes, seconds)
    } else {
        format!("{}:{:02}", minutes, seconds)
    }
}

pub fn truncate_text(value: &str, max: usize) -> String {
    let mut out = String::new();
    let mut count = 0usize;
    for ch in value.chars() {
        if count >= max {
            out.push_str("...");
            return out;
        }
        out.push(ch);
        count += 1;
    }
    out
}
