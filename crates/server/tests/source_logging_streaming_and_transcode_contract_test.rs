mod support;

use support::{assert_contains_all, read_repo_files};

#[test]
fn logging_and_activity_source_define_expected_logs_and_views() {
    let source = read_repo_files(&[
        "crates/server/src/logging.rs",
        "crates/server/src/admin/logs.rs",
        "crates/server/src/admin/activity.rs",
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
            "state.activity.clear_events()",
            "std::fs::write(log_dir.join(LOG_ACTIVITY_FILE), \"\")",
            "read_issue_log(&state, 1)",
        ],
    );
}

#[test]
fn quic_streaming_and_playback_stat_contracts_are_encoded() {
    let source = read_repo_files(&[
        "crates/server/src/quic/mod.rs",
        "crates/server/src/stream_sessions.rs",
    ]);

    assert_contains_all(
        &source,
        &[
            "const ALPN_QUIC: &[&[u8]] = &[b\"phonolite-quic\"];",
            "const SEEK_RESET_MARKER: u16 = 0xFFFF;",
            "const SEEK_RECOVERY_TARGET_MS: u32 = 400;",
            "const STATS_FLUSH_INTERVAL: Duration = Duration::from_secs(5);",
            "const STATS_MAX_PLAYBACK_DELTA_MS: u64 = 15_000;",
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
            "producer_failure: Option<String>,",
            "\"QUIC stream producer failed track={}",
            "let mut active_failures = Vec::new();",
            "ControlResponse::Error { message: &message },",
            "session.buffer_target_ms = SEEK_RECOVERY_TARGET_MS;",
            "marker.extend_from_slice(&SEEK_RESET_MARKER.to_le_bytes());",
            "if duration_ms > 0 && position_ms >= duration_ms / 2 {",
            "const SESSION_TTL: Duration = Duration::from_secs(90);",
            "const DOWN_SHIFT_MS: u64 = 2000;",
            "const UP_SHIFT_MS: u64 = 8000;",
            "StreamQuality::High => 160_000,",
            "StreamQuality::Medium => 96_000,",
            "StreamQuality::Low => 48_000,",
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
            "const MAX_SEEK_SKIP_MS: u32 = 250;",
            "TranscodeMode::Auto",
            "TranscodeMode::Fixed",
            "if decoded_channels == 0 || decoded_channels > 2 {",
            "LinearResampler::new(",
            "buf.extend_from_slice(b\"OPUSR01\\0\");",
            "if header_len > u16::MAX as usize {",
            "2 | 5 | 10 | 20 | 40 | 60 => Ok(frame_ms),",
            "TranscodeQuality::High => 160_000,",
            "TranscodeQuality::Medium => 96_000,",
            "TranscodeQuality::Low => 48_000,",
            "const MAX_CACHE_BYTES: usize = 256 * 1024 * 1024;",
            "pub const MEMORY_CACHE_MAX_BYTES: usize = MAX_CACHE_BYTES;",
            "if !matches!(sample_rate, 8000 | 12000 | 16000 | 24000 | 48000) {",
        ],
    );
}
