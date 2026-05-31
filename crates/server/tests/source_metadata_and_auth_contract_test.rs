mod support;

use support::{assert_contains_all, assert_contains_in_order, read_repo_file, read_repo_files};

#[test]
fn metadata_provider_support_retry_policy_and_rate_limit_are_encoded() {
    let source = read_repo_files(&[
        "crates/server/src/external.rs",
        "crates/server/src/musicbrainz_rate_limit.rs",
        "crates/server/src/musicbrainz_album_artists.rs",
    ]);

    assert_contains_all(
        &source,
        &[
            "TheAudioDb",
            "MusicBrainz",
            "\"theaudiodb\" | \"audio_db\" | \"audiodb\"",
            "\"musicbrainz\" | \"music_brainz\" | \"mb\"",
            "const MUSICBRAINZ_REQUEST_INTERVAL: Duration = Duration::from_secs(1);",
            "let tries = 6usize;",
            "status.as_u16() == 429 || status.is_server_error()",
            "tokio::time::sleep(Duration::from_millis(1000 + (attempt as u64 * 500))).await;",
            ".header(\"User-Agent\", user_agent)",
            "MusicBrainzAlbumArtistResolver",
            "wait_for_slot().await",
        ],
    );
}

#[test]
fn metadata_assets_cover_cache_and_long_lived_cache_headers_are_present() {
    let source = read_repo_files(&["crates/server/src/scan.rs", "crates/server/src/assets.rs"]);

    assert_contains_all(
        &source,
        &[
            "clear_metadata_assets",
            "start_cover_sweep",
            "warm_cover_cache",
            "prune_stale_metadata_assets",
            "MetadataAssetPruneStats",
            "prune_cover_cache",
            "prune_flat_artist_assets",
            "prune_legacy_artist_assets",
            "prune_stale_metadata_assets(&state, &library).await",
            "prune_stale_metadata_assets(&state, &library_clone).await",
            "\"logo\" => (\"logos\", \"logo\")",
            "\"banner\" => (\"banners\", \"banner\")",
            "metadata_root.join(\"covers\")",
            "HeaderValue::from_static(\"public, max-age=31536000\")",
            "CoverSource::Embedded",
            "CoverSource::File",
            "CoverSource::External",
        ],
    );
}

#[test]
fn metadata_update_events_are_exposed_and_emitted_per_object() {
    let source = read_repo_files(&[
        "crates/server/src/main.rs",
        "crates/server/src/state.rs",
        "crates/server/src/api/mod.rs",
        "crates/server/src/api/library.rs",
        "crates/server/src/scan.rs",
        "crates/server/src/metadata_events.rs",
    ]);

    assert_contains_all(
        &source,
        &[
            "MetadataEventBus::new(1024)",
            "pub metadata_events: MetadataEventBus",
            "\"/library/metadata-events\"",
            "get(library::metadata_events)",
            "pub async fn metadata_events",
            "Sse::new(stream).keep_alive(KeepAlive::default())",
            "metadata_events.emit_artist(&artist.id)",
            "metadata_events.emit_album(&album.id, Some(album.artist_id.clone()))",
            "metadata_events.emit_album_artists(&album.id, artist_id)",
            "pub struct MetadataUpdateEvent",
            "pub revision: u64",
            "pub kind: String",
            "pub album_id: Option<String>",
            "pub artist_id: Option<String>",
            "SweepBudget::new(replace_complete, config.external_metadata_scan_limit)",
            "usize::MAX",
            "\"unlimited\".to_string()",
        ],
    );
}

#[test]
fn metadata_sweep_order_and_per_phase_budget_are_encoded() {
    let scan = read_repo_file("crates/server/src/scan.rs");
    let settings = read_repo_file("web/templates/settings.html");

    assert_contains_all(
        &scan,
        &[
            "struct SweepBudget",
            "fn artist_limit(self) -> usize",
            "fn album_artist_limit(self) -> usize",
            "fn artist_catchup_limit(self) -> usize",
            "fn album_limit(self) -> usize",
            "run_tag_error_album_enrichment",
            "normal_sweep_budget_applies_limit_per_phase",
            "album_artist_attempts_do_not_consume_album_phase_budget",
            "log_metadata_phase(\"artists\", artist_phase.stats)",
            "log_metadata_phase(\"album artists\", album_artist_phase.stats)",
            "log_metadata_phase(\"artist catch-up\", artist_catchup_phase.stats)",
            "log_metadata_phase(\"albums\", album_phase.stats)",
        ],
    );
    assert_contains_in_order(
        &scan,
        &[
            "let artist_phase = run_artist_enrichment_sweep(",
            "let album_artist_phase = run_musicbrainz_album_artist_sweep(",
            "let artist_catchup_phase = run_artist_enrichment_sweep(",
            "let album_phase = run_album_enrichment_sweep(",
        ],
    );
    assert_contains_all(
        &settings,
        &[
            "Max items to enrich per scan phase",
            "Normal scans apply this limit separately to artists, MusicBrainz album-artist fixes, and albums.",
            "Reindex ignores the limit.",
        ],
    );
}

#[test]
fn auth_roles_session_tokens_and_admin_cookie_flows_match_requirements() {
    let source = read_repo_files(&[
        "crates/server/src/auth.rs",
        "crates/server/src/admin/mod.rs",
        "crates/server/src/admin/auth.rs",
    ]);

    assert_contains_all(
        &source,
        &[
            "pub enum UserRole {",
            "SuperAdmin",
            "Admin",
            "User",
            "pub disabled: bool,",
            "username.trim().is_empty()",
            "user.username.eq_ignore_ascii_case(username)",
            "if user.disabled {",
            "let expires_at = now + self.session_ttl.as_secs();",
            "let mut hasher = Sha256::new();",
            "phonolite_session",
            "HttpOnly; SameSite=Strict; Max-Age={}",
            "phonolite_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0",
            "state.auth.create_superadmin(&form.username, &form.password)",
            "state.auth.create_session(&user.id)",
            "clear_session_cookie()",
        ],
    );
}
