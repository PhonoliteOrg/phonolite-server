mod support;

use support::{assert_contains_all, read_repo_file, read_repo_files};

#[test]
fn api_router_exposes_public_routes_and_protected_contracts() {
    let source = read_repo_file("crates/server/src/api/mod.rs");

    assert_contains_all(
        &source,
        &[
            ".route(\"/auth/login\", post(auth::auth_login))",
            ".route(\"/auth/logout\", post(auth::auth_logout))",
            ".route(\"/health\", get(health))",
            ".route(\"/server/ports\", get(server::get_ports))",
            ".route(\"/library/search\", get(library::search))",
            ".route(\"/library/shuffle\", get(library::shuffle_tracks))",
            ".route(",
            "\"/library/tracks/:track_id/offline-metadata\"",
            "get(library::get_offline_track_metadata)",
            ".route(\"/download/batches\", post(library::create_download_batch))",
            ".route(\"/download/tracks/:track_id\", get(library::download_track))",
            ".route(\"/browse/artists\", get(browse::list_artists))",
            ".route(\"/stats\", get(stats::get_stats))",
            ".route(\"/player/settings\", get(player::get_playback_settings))",
            ".route(\"/player/settings\", post(player::update_playback_settings))",
            ".layer(middleware::from_fn_with_state(state.clone(), require_auth));",
            "\"server not initialized\"",
            "\"unauthorized\"",
        ],
    );
}

#[test]
fn offline_metadata_route_and_response_shape_are_encoded() {
    let source = read_repo_files(&[
        "crates/server/src/api/mod.rs",
        "crates/server/src/api/library.rs",
        "crates/server/src/api/views.rs",
    ]);

    assert_contains_all(
        &source,
        &[
            "\"/library/tracks/:track_id/offline-metadata\"",
            "pub struct ArtistView",
            "pub struct AlbumView",
            "pub struct OfflineTrackMetadata",
            "pub schema_version: u32",
            "pub track: TrackView",
            "pub genres: Vec<String>",
            "pub album: AlbumView",
            "pub artist: ArtistView",
            "pub async fn get_offline_track_metadata",
            "library.get_track(&track_id)",
            "build_album_view(library, album)",
            "build_artist_view(library, artist)",
            "OFFLINE_METADATA_SCHEMA_VERSION",
        ],
    );
}

#[test]
fn download_batch_manifest_and_resume_rules_are_encoded() {
    let source = read_repo_files(&[
        "crates/server/src/api/mod.rs",
        "crates/server/src/api/library.rs",
    ]);

    assert_contains_all(
        &source,
        &[
            "\"/download/batches\"",
            "pub struct DownloadBatchRequest",
            "pub track_ids: Vec<String>",
            "pub client_batch_id: Option<String>",
            "pub struct DownloadBatchResponse",
            "pub struct DownloadBatchItem",
            "pub offline_metadata: OfflineTrackMetadata",
            "pub byte_length: u64",
            "pub sha256: String",
            "pub struct DownloadBatchUnavailable",
            "pub async fn create_download_batch",
            "checksum_for_file(&path, &metadata).await",
            "DOWNLOAD_CHECKSUM_CACHE",
            "header::IF_RANGE",
            "value.trim() == etag.as_str()",
            "filter(|_| if_range_matches)",
            "StatusCode::PARTIAL_CONTENT",
        ],
    );
}

#[test]
fn library_search_shuffle_player_and_stats_rules_are_encoded() {
    let source = read_repo_files(&[
        "crates/server/src/api/library.rs",
        "crates/server/src/api/browse.rs",
        "crates/server/src/api/player.rs",
        "crates/server/src/api/stats.rs",
        "crates/server/src/api/auth.rs",
    ]);

    assert_contains_all(
        &source,
        &[
            "if query.is_empty() {",
            "DEFAULT_SEARCH_LIMIT",
            "MAX_SEARCH_LIMIT",
            "results.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));",
            "split_list_param(params.artist_ids.as_deref())",
            "split_list_param(params.genres.as_deref())",
            "\"artist_id required for shuffle=artist\"",
            "\"album_id required for shuffle=album\"",
            "\"no tracks found\"",
            "let limit = params.limit.unwrap_or(200).max(1);",
            "tracks.sort_by(|a, b| {",
            "a.disc_no",
            "a.track_no.unwrap_or(0).cmp(&b.track_no.unwrap_or(0))",
            "const DEFAULT_REPEAT_MODE: &str = \"off\";",
            "\"invalid repeat_mode\"",
            "if !state.config.read().stats_collection_enabled {",
            "\"month must be 1-12\"",
            "token_type: \"Bearer\"",
            "state.auth.revoke_session(&token)",
            "header::ACCEPT_RANGES",
            "header::CONTENT_RANGE",
            "Body::from_stream(stream)",
        ],
    );
}

#[test]
fn admin_router_and_settings_validation_match_requirements() {
    let source = read_repo_files(&[
        "crates/server/src/admin/mod.rs",
        "crates/server/src/admin/settings.rs",
        "crates/server/src/admin/assets.rs",
    ]);

    assert_contains_all(
        &source,
        &[
            ".route(\"/\", get(admin_home))",
            "\"/setup\",",
            "\"/login\",",
            "\"/settings\",",
            ".route(\"/settings/reindex\", post(library::admin_reindex))",
            ".route(\"/settings/scan\", post(library::admin_scan))",
            "\"/users\",",
            "index_path is required",
            "metadata_path is required",
            "log_dir is required",
            "quic_port must be a valid number",
            "quic_port must be different from port",
            "watch_debounce_secs must be a positive number",
            "session_ttl_secs must be a positive number",
            "external_metadata_min_interval_secs must be a positive number",
            "external_metadata_timeout_secs must be a positive number",
            "external_metadata_scan_limit must be a number",
            "let duplicate = config.music_roots.iter().any(|root| {",
            "Component::Normal(value) => relpath.push(value)",
            "\"text/javascript\"",
            "\"text/css\"",
            "\"application/octet-stream\"",
        ],
    );
}

#[test]
fn admin_library_and_user_management_rules_are_encoded() {
    let source = read_repo_files(&[
        "crates/server/src/admin/library.rs",
        "crates/server/src/admin/users.rs",
        "crates/server/src/scan.rs",
    ]);

    assert_contains_all(
        &source,
        &[
            "let limit = 24;",
            "SearchFilter::Artists",
            "SearchFilter::Albums",
            "SearchFilter::Tracks",
            "start_rescan(state.clone(), library, true);",
            "prune_stale_metadata_assets(&state, &library_clone).await",
            "start_enrichment_sweep(state.clone(), library_clone.clone(), replace_complete);",
            "start_cover_sweep(state.clone(), library_clone);",
            "superadmin role is reserved",
            "superadmin can only edit its own account",
            "auth::AuthError::LastAdmin => StatusCode::CONFLICT",
            "admin_bulk_delete",
            "let is_superadmin = matches!(user.role, UserRole::SuperAdmin);",
        ],
    );
}
