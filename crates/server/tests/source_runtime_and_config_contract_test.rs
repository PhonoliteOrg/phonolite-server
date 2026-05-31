mod support;

use support::{assert_contains_all, read_repo_file};

#[test]
fn main_bootstrap_wires_startup_state_and_http_stack() {
    let main_rs = read_repo_file("crates/server/src/main.rs");

    assert_contains_all(
        &main_rs,
        &[
            "#[tokio::main]",
            "load_or_create_config(&config_path)",
            "logging::init_logging(&config_path, &config)",
            "Library::open_db(&index_path)?",
            "AuthStore::new(Arc::clone(&db), session_ttl)",
            "UserDataStore::new(Arc::clone(&user_db))",
            "StatsStore::new(Arc::clone(&stats_db))",
            "StreamCache::new(stream_cache_dir, config.stream_cache_enabled)",
            ".nest(\"/api/v1\", api_router(state.clone()))",
            ".merge(admin_router(state.clone()))",
            "SetRequestIdLayer::x_request_id(MakeRequestUuid)",
            "TraceLayer::new_for_http()",
            "LatencyUnit::Millis",
        ],
    );
}

#[test]
fn main_handles_music_root_state_quic_launch_and_graceful_shutdown() {
    let main_rs = read_repo_file("crates/server/src/main.rs");

    assert_contains_all(
        &main_rs,
        &[
            "resolve_music_roots(&state.config_path, &config.music_roots)",
            "set_library_missing(&state, missing.path.clone());",
            "start_index(state.clone(), roots, false);",
            "std::env::var(\"PHONOLITE_START_DELAY_MS\")",
            "quic::run(quic_state).await",
            "tokio::signal::ctrl_c()",
            "SignalKind::terminate()",
        ],
    );
}

#[test]
fn config_defaults_migrations_and_bind_normalization_are_encoded_in_source() {
    let config_rs = read_repo_file("crates/server/src/config.rs");

    assert_contains_all(
        &config_rs,
        &[
            "pub const CONFIG_VERSION: u32 = 8;",
            "port: 3000",
            "bind_addr: None",
            "quic_enabled: true",
            "quic_port: 3001",
            "quic_cert_path: \"quic_cert.pem\".to_string()",
            "quic_key_path: \"quic_key.pem\".to_string()",
            "watch_debounce_secs: 2",
            "session_ttl_secs: 60 * 60 * 24 * 7",
            "stats_collection_enabled: false",
            "external_metadata_scan_limit: 50",
            "external_metadata_timeout_secs: 8",
            "stream_cache_enabled: true",
            "stream_cache_dir: \"stream_cache\".to_string()",
            "match env::var(\"PHONOLITE_CONFIG\")",
            "if config.external_metadata_sources.is_empty()",
            "config.quic_port = config.port.saturating_add(1);",
            "if config.quic_port == config.port {",
            "normalize_music_roots(&mut config);",
            "format!(\"root-{}\", source_id_suffix())",
            "\"0.0.0.0\".to_string()",
            "format!(\"[{}]:{}\", host, port)",
        ],
    );
}
