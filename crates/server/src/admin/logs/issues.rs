//! Indexing issue details shared by the Logs page and its refresh endpoint.
use std::path::Path;

use serde::Serialize;

use crate::admin::library_for_admin;
use crate::admin::presentation::escape_html;
use crate::config::resolve_path;
use crate::logging::LOG_ISSUE_FILE;
use crate::state::AppState;

const ISSUE_UI_LIMIT: usize = 200;

#[derive(Serialize)]
pub(super) struct IndexingIssuesResponse {
    pub html: String,
    pub total: usize,
}

pub(super) fn load_indexing_issues(state: &AppState) -> Result<IndexingIssuesResponse, String> {
    match read_issue_log(state, ISSUE_UI_LIMIT) {
        Ok((items, total)) => Ok(IndexingIssuesResponse {
            html: render_issue_log(&items),
            total,
        }),
        Err(_) => {
            let library = library_for_admin(state)
                .map_err(|message| format!("Library unavailable: {}", message))?;
            let (items, total) = library
                .list_tag_error_files(ISSUE_UI_LIMIT, 0)
                .map_err(|err| err.to_string())?;
            Ok(IndexingIssuesResponse {
                html: render_tag_error_files(&library, &items),
                total,
            })
        }
    }
}

struct IssueLogItem {
    file: String,
    folder: Option<String>,
    error: String,
}

fn read_issue_log(state: &AppState, limit: usize) -> Result<(Vec<IssueLogItem>, usize), String> {
    let log_dir = resolve_path(&state.config_path, &state.config.read().log_dir);
    read_issue_log_file(&log_dir.join(LOG_ISSUE_FILE), limit)
}

fn read_issue_log_file(path: &Path, limit: usize) -> Result<(Vec<IssueLogItem>, usize), String> {
    // A missing/unreadable log falls back to persisted tag errors. A readable,
    // empty log stays empty so clearing Logs does not restore cleared entries.
    let (mut items, total) = super::read_log_records(path, limit, parse_issue_line)?;
    items.reverse();
    Ok((items, total))
}

fn parse_issue_line(line: &str) -> Option<IssueLogItem> {
    let (_, rest) = line.split_once(char::is_whitespace)?;
    let (level, rest) = rest.trim_start().split_once(char::is_whitespace)?;
    if level != "ISSUE" {
        return None;
    }
    let rest = rest.trim();
    let message = decode_log_value(rest.strip_prefix("message=").unwrap_or(rest));
    let mut file = None;
    let mut folder = None;
    let mut error = None;
    for part in message.split(" | ") {
        if let Some((key, value)) = part.split_once('=') {
            let value = decode_log_value(value);
            match key.trim() {
                "file" => file = Some(value),
                "folder" => folder = Some(value),
                "error" => error = Some(value),
                _ => {}
            }
        }
    }
    Some(IssueLogItem {
        file: file?,
        folder,
        error: error.unwrap_or_else(|| "Unknown error".to_string()),
    })
}

fn decode_log_value(value: &str) -> String {
    let value = value.trim();
    serde_json::from_str::<String>(value).unwrap_or_else(|_| {
        value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .unwrap_or(value)
            .to_string()
    })
}

fn render_issue_log(items: &[IssueLogItem]) -> String {
    if items.is_empty() {
        return "<p class=\"muted\">No indexing issues detected.</p>".to_string();
    }

    let mut rows = String::new();
    for item in items {
        let filename = item.file.rsplit(['/', '\\']).next().unwrap_or(&item.file);
        rows.push_str(&format!(
            "<tr><td>{}</td><td><code>{}</code></td><td>{}</td><td>{}</td></tr>",
            escape_html(filename),
            escape_html(&item.file),
            escape_html(item.folder.as_deref().unwrap_or("-")),
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

#[cfg(test)]
mod tests {
    use super::{parse_issue_line, read_issue_log_file, render_issue_log};

    #[test]
    fn issue_fields_preserve_quoted_errors_and_paths() {
        let message = "file=Music/song.flac | folder=Music | error=\"Missing \\\"title\\\" tag\"";
        for value in [message.to_string(), serde_json::to_string(message).unwrap()] {
            let line = format!("2026-09-30T12:00:00Z ISSUE    message={value}");
            let issue = parse_issue_line(&line).unwrap();
            assert_eq!(issue.file, "Music/song.flac");
            assert_eq!(issue.folder.as_deref(), Some("Music"));
            assert_eq!(issue.error, "Missing \"title\" tag");
        }
        assert!(parse_issue_line("2026-09-30T12:00:00Z ISSUE album=Album | files=2").is_none());
        assert!(parse_issue_line("2026-09-30T12:00:00Z INFO file=song.flac").is_none());
    }

    #[test]
    fn issues_escape_external_text_and_handle_windows_filenames() {
        let issue = parse_issue_line(
            "2026-09-30T12:00:00Z ISSUE file=C:\\Music\\<song>.flac | folder=A&B | error=<script>bad</script>",
        )
        .unwrap();
        let html = render_issue_log(&[issue]);
        assert!(html.contains("<td>&lt;song&gt;.flac</td>"));
        assert!(html.contains("C:\\Music\\&lt;song&gt;.flac"));
        assert!(html.contains("A&amp;B"));
        assert!(html.contains("&lt;script&gt;bad&lt;/script&gt;"));
        assert!(!html.contains("<script>"));
    }

    #[test]
    fn issue_log_counts_file_records_and_returns_newest_first() {
        let path =
            std::env::temp_dir().join(format!("phonolite-issues-{}.log", uuid::Uuid::new_v4()));
        std::fs::write(
            &path,
            "2026-09-30T12:00:00Z ISSUE file=first.flac | error=bad\n\
             2026-09-30T12:00:01Z ISSUE album=Album | files=2\n\
             2026-09-30T12:00:02Z ISSUE file=second.flac | error=bad\n",
        )
        .unwrap();
        let (items, total) = read_issue_log_file(&path, 1).unwrap();
        assert_eq!(total, 2);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].file, "second.flac");
        std::fs::write(&path, "").unwrap();
        let (items, total) = read_issue_log_file(&path, 200).unwrap();
        assert!(items.is_empty());
        assert_eq!(total, 0);
        std::fs::remove_file(&path).unwrap();
        assert!(read_issue_log_file(&path, 200).is_err());
    }
}
