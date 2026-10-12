mod support;

use support::{assert_contains_all, read_repo_files};

#[test]
fn logging_and_activity_source_define_expected_logs_and_views() {
    let source = read_repo_files(&[
        "crates/server/src/logging.rs",
        "crates/server/src/admin/logs.rs",
        "crates/server/src/activity_store.rs",
    ]);

    assert_contains_all(
        &source,
        &[
            "pub const LOG_MAX_LINES: usize = 10_000;",
            "pub const LOG_TRIM_TO: usize = 5_000;",
            "pub const LOG_ALL_FILE: &str = \"all.log\";",
            "pub const LOG_INFO_FILE: &str = \"info.log\";",
            "pub const LOG_WARN_FILE: &str = \"warnings.log\";",
            "pub const LOG_ERROR_FILE: &str = \"errors.log\";",
            "pub const LOG_ISSUE_FILE: &str = \"issues.log\";",
            "pub const LOG_ACTIVITY_FILE: &str = \"activities.log\";",
            "pub const LOG_DEBUG_FILE: &str = \"debug.log\";",
            "reload::Layer::new(filter)",
            "let view = query.view.as_deref().unwrap_or(\"all\");",
            "\"issues\" => LOG_ISSUE_FILE,",
            "\"activities\" => LOG_ACTIVITY_FILE,",
            "\"debug\" => LOG_DEBUG_FILE,",
            "info!(target: \"activity\", \"{}\", message);",
            "self.add_event(\"ACTIVITY\", message)",
            "pub fn clear_events(&self)",
            "write_txn.delete_table(ACTIVITY_TABLE)",
            ".open_table(ACTIVITY_TABLE)",
        ],
    );
}

#[test]
fn logs_page_renders_indexing_issues_with_persisted_tag_error_fallback() {
    let logs = read_repo_files(&[
        "crates/server/src/admin/logs.rs",
        "crates/server/src/admin/logs/issues.rs",
    ]);

    assert_contains_all(
        &logs,
        &[
            "pub async fn admin_logs_issues",
            "\"issue_count\"",
            "\"tag_errors\"",
            "load_indexing_issues(&state)",
            "read_issue_log",
            "render_issue_log",
            "list_tag_error_files",
            "render_tag_error_files",
            "log_dir.join(LOG_ISSUE_FILE)",
            "No indexing issues detected.",
        ],
    );
}

#[test]
fn quic_streaming_and_playback_stat_contracts_are_encoded() {
    let source = read_repo_files(&[
        "crates/server/src/quic/mod.rs",
        "crates/server/src/stream_cache.rs",
        "crates/server/src/stats_ledger.rs",
    ]);

    assert_contains_all(
        &source,
        &[
            "const ALPN_QUIC: &[&[u8]] = &[b\"phonolite-quic\"];",
            "const SEEK_RESET_MARKER: u16 = 0xFFFF;",
            "const SEEK_RECOVERY_TARGET_MS: u32 = 400;",
            "pub const MAX_EVENT_BATCH: usize = 200;",
            "txn.commit()?;",
            "#[serde(rename = \"auth\")]",
            "#[serde(rename = \"open\")]",
            "#[serde(rename = \"advance\")]",
            "#[serde(rename = \"buffer\")]",
            "#[serde(rename = \"seek\")]",
            "#[serde(rename = \"playback\")]",
            "#[serde(rename = \"ping\")]",
            "#[serde(rename = \"auth_ok\")]",
            "#[serde(rename = \"open_ok\")]",
            "config.set_max_idle_timeout(90_000);",
            "const BUFFER_REPORT_STALE_AFTER: Duration = Duration::from_secs(2);",
            "last_buffer_report: Instant,",
            "fn active_backpressure_paused(session: &SessionState, now: Instant) -> bool",
            "now.duration_since(session.last_buffer_report) <= BUFFER_REPORT_STALE_AFTER",
            "buffer_report_age_ms={}",
            "producer_failure: Option<String>,",
            "\"QUIC stream producer failed track={}",
            "let mut active_failures = Vec::new();",
            "request_id,",
            "session.buffer_target_ms = SEEK_RECOVERY_TARGET_MS;",
            "marker.extend_from_slice(&SEEK_RESET_MARKER.to_le_bytes());",
            "ControlMessage::Playback { .. } => {}",
            "30_000.min((session.track_duration_ms.saturating_add(1) / 2).max(1))",
            "const CACHE_RANGE_MS: u32 = 5_000;",
            "const STREAM_QUEUE_MS: u32 = 400;",
            "const MAX_RANGE_PRODUCERS: usize = 4;",
            "range_end_ms: u32,",
            "end_of_track: bool,",
            "source_version:",
        ],
    );
}

#[test]
fn raw_opus_transcode_and_cache_contracts_are_encoded() {
    let source = read_repo_files(&[
        "crates/server/src/transcode.rs",
        "crates/server/src/stream_cache.rs",
        "crates/codecs_ffi/src/opus.rs",
    ]);

    assert_contains_all(
        &source,
        &[
            "const TARGET_SAMPLE_RATE: u32 = 48_000;",
            ".lookahead()",
            "Auto",
            "TranscodeMode::Fixed",
            "if decoded_channels == 0 || decoded_channels > 2 {",
            "LinearResampler::new(",
            "buf.extend_from_slice(b\"OPUSR01\\0\");",
            "if header_len > u16::MAX as usize {",
            "2 | 5 | 10 | 20 | 40 | 60 => Ok(frame_ms),",
            "TranscodeQuality::High => 160_000,",
            "TranscodeQuality::Medium => 96_000,",
            "TranscodeQuality::Low => 48_000,",
            "const MAX_CACHE_BYTES: usize = 4 * 1024 * 1024;",
            "pub const MEMORY_CACHE_MAX_BYTES: usize = MAX_CACHE_BYTES * MAX_CACHE_ENTRIES;",
            "if !matches!(sample_rate, 8000 | 12000 | 16000 | 24000 | 48000) {",
        ],
    );
}
