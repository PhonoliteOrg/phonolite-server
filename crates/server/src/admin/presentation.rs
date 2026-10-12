//! Console templates, escaping, and display formatting.
use axum::http::StatusCode;
use axum::response::Response;
use std::path::PathBuf;

use super::console::{ConsoleError, ConsoleView};
use crate::http::html_response;
use crate::state::AppState;

pub fn html_error(state: &AppState, status: StatusCode, message: String) -> Response {
    let template = load_template(state, "templates/partials/error.html").unwrap_or_else(|_| {
        "<div class=\"error-page\"><h2>Error {{status}}</h2><p>{{message}}</p></div>".to_string()
    });
    let body = apply_template(
        template,
        &[
            ("status", status.as_u16().to_string()),
            ("message", escape_html(&message)),
        ],
    );
    let mut response = render_admin_response(state, status, "Error", &body, PageLayout::centered());
    response.extensions_mut().insert(ConsoleError(message));
    response
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

pub fn render_admin_response(
    state: &AppState,
    status: StatusCode,
    title: &str,
    content: &str,
    layout: PageLayout,
) -> Response {
    let nav_template = if layout.nav && !layout.centered {
        load_template(state, "templates/partials/nav.html").unwrap_or_default()
    } else {
        String::new()
    };
    let footer_template = if layout.centered {
        load_template(state, "templates/partials/footer.html").unwrap_or_default()
    } else {
        String::new()
    };
    let view = build_page_view(title, content, layout, nav_template, &footer_template);
    let html = match load_template(state, "templates/layout.html") {
        Ok(template) => render_view_template(template, &view),
        Err(err) => format!(
            "<!DOCTYPE html><html><head><meta charset=\"utf-8\" /><title>{}</title></head><body><h1>Phonolite admin</h1><div>{}</div><pre>{}</pre></body></html>",
            escape_html(title),
            content,
            escape_html(&err)
        ),
    };
    let mut response = html_response(status, html);
    response.extensions_mut().insert(view);
    response
}

#[cfg(test)]
fn render_page_template(
    template: String,
    title: &str,
    content: &str,
    layout: PageLayout,
    nav_template: String,
    footer: &str,
) -> String {
    let view = build_page_view(title, content, layout, nav_template, footer);
    render_view_template(template, &view)
}

fn build_page_view(
    title: &str,
    content: &str,
    layout: PageLayout,
    nav_template: String,
    footer: &str,
) -> ConsoleView {
    let page = title.to_ascii_lowercase();
    let current = |name: &str| {
        if page == name {
            "aria-current=\"page\"".to_string()
        } else {
            String::new()
        }
    };
    let nav = if layout.nav && !layout.centered {
        apply_template(
            nav_template,
            &[
                ("nav_class", String::new()),
                ("nav_library_current", current("library")),
                ("nav_users_current", current("users")),
                ("nav_logs_current", current("logs")),
                ("nav_settings_current", current("settings")),
            ],
        )
    } else {
        String::new()
    };
    ConsoleView {
        title: title.to_string(),
        page,
        page_class: if layout.centered { "centered" } else { "" }.to_string(),
        nav,
        footer: if layout.centered {
            footer.to_string()
        } else {
            String::new()
        },
        content: content.to_string(),
    }
}

fn render_view_template(template: String, view: &ConsoleView) -> String {
    apply_template(
        template,
        &[
            ("title", escape_html(&view.title)),
            ("page", escape_html(&view.page)),
            ("page_class", escape_html(&view.page_class)),
            ("nav", view.nav.clone()),
            ("footer", view.footer.clone()),
            ("content", view.content.clone()),
        ],
    )
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
    if let Ok(value) = std::env::var("PHONOLITE_WEB_ROOT") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            let candidate = PathBuf::from(trimmed);
            if candidate.is_dir() {
                return candidate;
            }
        }
    }

    let mut candidates = Vec::new();
    if let Some(base) = state
        .config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        candidates.push(base.join("web"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(base) = exe.parent().filter(|p| !p.as_os_str().is_empty()) {
            candidates.push(base.join("web"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("web"));
        candidates.push(cwd.join("server").join("web"));
    }
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("web"),
    );

    for candidate in &candidates {
        if candidate.is_dir() {
            return candidate.clone();
        }
    }

    candidates
        .into_iter()
        .next()
        .unwrap_or_else(|| PathBuf::from("web"))
}

pub fn url_escape(input: &str) -> String {
    let mut out = String::new();
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
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

#[cfg(test)]
mod tests {
    use super::{render_page_template, PageLayout};

    #[test]
    fn rendering_preserves_escaping_and_unicode_boundaries() {
        assert_eq!(super::escape_html("<&>\"'é"), "&lt;&amp;&gt;&quot;&#39;é");
        assert_eq!(super::url_escape("A é/&"), "A%20%C3%A9%2F%26");
        assert_eq!(super::truncate_text("é中ab", 2), "é中...");
        assert_eq!(super::format_duration_ms(3_661_000), "1:01:01");
        assert_eq!(super::format_track_position(Some(2), Some(3)), "2.03");
    }

    fn render_page(title: &str, layout: PageLayout) -> String {
        render_page_template(
            include_str!("../../../../web/templates/layout.html").to_string(),
            title,
            "<article>Page content</article>",
            layout,
            include_str!("../../../../web/templates/partials/nav.html").to_string(),
            include_str!("../../../../web/templates/partials/footer.html"),
        )
    }

    #[test]
    fn console_shell_marks_only_the_current_page() {
        for title in ["Library", "Users", "Logs", "Settings"] {
            let html = render_page(title, PageLayout::standard());
            let page = title.to_ascii_lowercase();
            assert!(html.contains(&format!("data-page=\"{page}\"")));
            assert_eq!(html.matches("aria-current=\"page\"").count(), 1);
            let current_link = html
                .split("<a ")
                .filter_map(|link| link.split_once('>'))
                .map(|(attributes, _)| attributes)
                .find(|attributes| attributes.contains("aria-current=\"page\""))
                .expect("active navigation link");
            assert!(current_link.contains(&format!("href=\"/{page}\"")));
            assert!(html.contains("<article>Page content</article>"));
            assert!(!html.contains("console-loading"));
            assert!(!html.contains("{{"), "unresolved template token on {title}");
        }
    }

    #[test]
    fn centered_pages_omit_private_navigation() {
        for title in ["Login", "Setup", "Error"] {
            let html = render_page(title, PageLayout::centered());
            assert!(html.contains("centered"));
            assert!(!html.contains("action=\"/logout\""));
            assert!(!html.contains("href=\"/settings\""));
            assert!(!html.contains("data-action=\"restart\""));
            assert!(!html.contains("data-action=\"shutdown\""));
            assert!(!html.contains("{{"));
        }
    }

    #[test]
    fn shell_escapes_title_and_page_attributes() {
        let html = render_page("<Error>\"", PageLayout::centered());
        assert!(html.contains("&lt;Error&gt;&quot;"));
        assert!(html.contains("data-page=\"&lt;error&gt;&quot;\""));
        assert!(!html.contains("<Error>"));
    }

    #[test]
    fn console_view_metadata_stays_separate_from_page_markup() {
        let view = super::build_page_view(
            "Settings",
            "<article data-page=\"login\">{{page}} {{title}}</article>",
            PageLayout::standard(),
            include_str!("../../../../web/templates/partials/nav.html").to_string(),
            "<footer>Phonolite</footer>",
        );
        let json = serde_json::to_value(view).unwrap();
        assert_eq!(json["title"], "Settings");
        assert_eq!(json["page"], "settings");
        assert_eq!(json["page_class"], "");
        assert!(json["content"].as_str().unwrap().contains("{{page}}"));
        assert_eq!(
            json["nav"]
                .as_str()
                .unwrap()
                .matches("aria-current=\"page\"")
                .count(),
            1
        );

        let view = super::build_page_view(
            "Login",
            "form",
            PageLayout::centered(),
            "private nav".to_string(),
            "footer",
        );
        assert_eq!(view.page_class, "centered");
        assert!(view.nav.is_empty());
    }
}
