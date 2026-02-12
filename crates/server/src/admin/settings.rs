// crates/server/src/admin/settings.rs
use std::time::Duration;

use axum::{
    extract::{Form, Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use tracing::warn;

use crate::config::{resolve_path, save_config, MetadataSourceConfig};
use crate::external::{self, ExternalSource};
use crate::scan::{new_source_id, parse_provider, source_fields_from_parts};
use crate::state::{
    AppState, MetadataSourceForm, MetadataTestForm, MetadataToggleForm, SettingsForm, SettingsQuery,
};
use crate::utils::{
    apply_template, escape_html, html_error, html_response, json_error_response, json_ok_response,
    load_template, redirect_to, render_admin_page, truncate_text, wants_json, PageLayout,
};

use super::{admin_user_from_headers, is_admin, render_message};

pub async fn admin_restart(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let user = match admin_user_from_headers(&state, &headers) {
        Ok(Some(user)) => user,
        Ok(None) => return redirect_to("/login"),
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

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if let Err(err) = crate::utils::spawn_restart() {
            warn!("Restart failed: {}", err);
        }
        std::process::exit(0);
    });

    if wants_json(&headers) {
        json_ok_response()
    } else {
        redirect_to("/")
    }
}

pub async fn admin_shutdown(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let user = match admin_user_from_headers(&state, &headers) {
        Ok(Some(user)) => user,
        Ok(None) => return redirect_to("/login"),
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

    if let Err(err) = state.auth.clear_sessions() {
        if wants_json(&headers) {
            return json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("shutdown failed: {}", err),
            );
        }
        // Redirect to home with error if possible, or just error page
        return html_error(
            &state,
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("shutdown failed: {}", err),
        );
    }

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        std::process::exit(0);
    });

    if wants_json(&headers) {
        json_ok_response()
    } else {
        redirect_to("/")
    }
}

pub async fn admin_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<SettingsQuery>,
) -> Response {
    if !state.auth.has_admin().unwrap_or(false) {
        return redirect_to("/setup");
    }
    let user = match admin_user_from_headers(&state, &headers) {
        Ok(Some(user)) => user,
        Ok(None) => return redirect_to("/login"),
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
    let message = query
        .message
        .as_deref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string());
    admin_settings_page(&state, message, query.music_root)
}

pub async fn admin_update_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<SettingsForm>,
) -> Response {
    let user = match admin_user_from_headers(&state, &headers) {
        Ok(Some(user)) => user,
        Ok(None) => return redirect_to("/login"),
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

    let music_root = form.music_root.trim();
    let previous_music_root = state.config.read().music_root.clone();
    let index_path = form.index_path.trim();
    if index_path.is_empty() {
        return admin_settings_page(&state, Some("index_path is required".to_string()), None);
    }
    let metadata_path = form.metadata_path.trim();
    if metadata_path.is_empty() {
        return admin_settings_page(&state, Some("metadata_path is required".to_string()), None);
    }
    let port = match form.port.trim().parse::<u16>() {
        Ok(value) if value > 0 => value,
        _ => {
            return admin_settings_page(
                &state,
                Some("port must be a valid number".to_string()),
                None,
            )
        }
    };
    let quic_port = match form.quic_port.trim().parse::<u16>() {
        Ok(value) if value > 0 => value,
        _ => {
            return admin_settings_page(
                &state,
                Some("quic_port must be a valid number".to_string()),
                None,
            )
        }
    };
    if quic_port == port {
        return admin_settings_page(
            &state,
            Some("quic_port must be different from port".to_string()),
            None,
        );
    }
    let watch_debounce_secs = match form.watch_debounce_secs.trim().parse::<u64>() {
        Ok(value) if value > 0 => value,
        _ => {
            return admin_settings_page(
                &state,
                Some("watch_debounce_secs must be a positive number".to_string()),
                None,
            )
        }
    };
    let session_ttl_secs = match form.session_ttl_secs.trim().parse::<u64>() {
        Ok(value) if value > 0 => value,
        _ => {
            return admin_settings_page(
                &state,
                Some("session_ttl_secs must be a positive number".to_string()),
                None,
            )
        }
    };
    if !music_root.is_empty() {
        let resolved = resolve_path(&state.config_path, music_root);
        if !resolved.exists() {
            return admin_settings_page(
                &state,
                Some(format!(
                    "music_root not found: {}",
                    resolved.to_string_lossy()
                )),
                None,
            );
        }
    }
    let external_min_interval_secs = match form
        .external_metadata_min_interval_secs
        .trim()
        .parse::<u64>()
    {
        Ok(value) if value > 0 => value,
        _ => {
            return admin_settings_page(
                &state,
                Some("external_metadata_min_interval_secs must be a positive number".to_string()),
                None,
            )
        }
    };
    let external_timeout_secs = match form.external_metadata_timeout_secs.trim().parse::<u64>() {
        Ok(value) if value > 0 => value,
        _ => {
            return admin_settings_page(
                &state,
                Some("external_metadata_timeout_secs must be a positive number".to_string()),
                None,
            )
        }
    };
    let external_scan_limit = match form.external_metadata_scan_limit.trim().parse::<usize>() {
        Ok(value) => value,
        _ => {
            return admin_settings_page(
                &state,
                Some("external_metadata_scan_limit must be a number".to_string()),
                None,
            )
        }
    };

    let mut config = state.config.read().clone();
    config.music_root = music_root.to_string();
    config.index_path = index_path.to_string();
    config.metadata_path = metadata_path.to_string();
    config.port = port;
    config.quic_port = quic_port;
    config.watch_music = form.watch_music.is_some();
    config.watch_debounce_secs = watch_debounce_secs;
    config.session_ttl_secs = session_ttl_secs;
    config.stats_collection_enabled = form.stats_collection_enabled.is_some();
    config.external_metadata_min_interval_secs = external_min_interval_secs;
    config.external_metadata_timeout_secs = external_timeout_secs;
    config.external_metadata_scan_limit = external_scan_limit;
    config.external_metadata_on_tag_error = form.external_metadata_on_tag_error.is_some();
    let sources_enabled = has_enabled_sources(&config.external_metadata_sources);
    config.external_metadata_enabled = sources_enabled;
    config.external_metadata_enrich_on_scan = sources_enabled;

    if let Err(err) = save_config(&state.config_path, &config) {
        return admin_settings_page(&state, Some(format!("save failed: {}", err)), None);
    }

    *state.config.write() = config;
    let music_root_changed = previous_music_root.trim() != music_root;
    let mut message = "info: Saved.".to_string();
    if music_root_changed {
        let root_message = crate::scan::apply_music_root_update(state.clone(), music_root, true);
        message.push(' ');
        message.push_str(&root_message);
    }
    message.push_str(" Restart the server to apply port or index path changes.");
    admin_settings_page(&state, Some(message), None)
}

pub async fn admin_add_metadata_source(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<MetadataSourceForm>,
) -> Response {
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

    let provider = match parse_provider(&form.provider) {
        Ok(provider) => provider,
        Err(err) => return json_error_response(StatusCode::BAD_REQUEST, err),
    };
    let (api_key, user_agent) = match source_fields_from_parts(
        provider,
        form.api_key.as_deref().unwrap_or(""),
        form.user_agent.as_deref().unwrap_or(""),
    ) {
        Ok(values) => values,
        Err(err) => return json_error_response(StatusCode::BAD_REQUEST, err),
    };

    let mut config = state.config.read().clone();
    let source = MetadataSourceConfig {
        id: new_source_id(),
        provider,
        enabled: true,
        api_key,
        user_agent,
    };
    config.external_metadata_sources.push(source.clone());
    let sources_enabled = has_enabled_sources(&config.external_metadata_sources);
    config.external_metadata_enabled = sources_enabled;
    config.external_metadata_enrich_on_scan = sources_enabled;
    if let Err(err) = save_config(&state.config_path, &config) {
        return json_error_response(StatusCode::INTERNAL_SERVER_ERROR, format!("{}", err));
    }
    *state.config.write() = config;
    let html = render_metadata_sources(&state, &[source]);
    html_response(StatusCode::OK, html)
}

pub async fn admin_update_metadata_source(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(source_id): AxumPath<String>,
    Form(form): Form<MetadataSourceForm>,
) -> Response {
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

    let provider = match parse_provider(&form.provider) {
        Ok(provider) => provider,
        Err(err) => return json_error_response(StatusCode::BAD_REQUEST, err),
    };
    let (api_key, user_agent) = match source_fields_from_parts(
        provider,
        form.api_key.as_deref().unwrap_or(""),
        form.user_agent.as_deref().unwrap_or(""),
    ) {
        Ok(values) => values,
        Err(err) => return json_error_response(StatusCode::BAD_REQUEST, err),
    };

    let mut config = state.config.read().clone();
    let target = match config
        .external_metadata_sources
        .iter_mut()
        .find(|source| source.id == source_id)
    {
        Some(source) => source,
        None => {
            return json_error_response(StatusCode::NOT_FOUND, "metadata source not found");
        }
    };
    target.provider = provider;
    target.api_key = api_key;
    target.user_agent = user_agent;

    if let Err(err) = save_config(&state.config_path, &config) {
        return json_error_response(StatusCode::INTERNAL_SERVER_ERROR, format!("{}", err));
    }
    *state.config.write() = config;
    json_ok_response()
}

pub async fn admin_delete_metadata_source(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(source_id): AxumPath<String>,
) -> Response {
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

    let mut config = state.config.read().clone();
    let initial_len = config.external_metadata_sources.len();
    config
        .external_metadata_sources
        .retain(|source| source.id != source_id);

    if config.external_metadata_sources.len() == initial_len {
        return json_error_response(StatusCode::NOT_FOUND, "metadata source not found");
    }
    let sources_enabled = has_enabled_sources(&config.external_metadata_sources);
    config.external_metadata_enabled = sources_enabled;
    config.external_metadata_enrich_on_scan = sources_enabled;

    if let Err(err) = save_config(&state.config_path, &config) {
        return json_error_response(StatusCode::INTERNAL_SERVER_ERROR, format!("{}", err));
    }
    *state.config.write() = config;
    json_ok_response()
}

pub async fn admin_toggle_metadata_source(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(source_id): AxumPath<String>,
    Form(form): Form<MetadataToggleForm>,
) -> Response {
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
    let enabled = form.enabled.is_some();
    let mut config = state.config.read().clone();
    let target = match config
        .external_metadata_sources
        .iter_mut()
        .find(|source| source.id == source_id)
    {
        Some(source) => source,
        None => {
            return json_error_response(StatusCode::NOT_FOUND, "metadata source not found");
        }
    };
    target.enabled = enabled;
    let sources_enabled = has_enabled_sources(&config.external_metadata_sources);
    config.external_metadata_enabled = sources_enabled;
    config.external_metadata_enrich_on_scan = sources_enabled;

    if let Err(err) = save_config(&state.config_path, &config) {
        return json_error_response(StatusCode::INTERNAL_SERVER_ERROR, format!("{}", err));
    }
    *state.config.write() = config;
    json_ok_response()
}

pub async fn admin_test_metadata_source(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<MetadataTestForm>,
) -> Response {
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
    let provider = match parse_provider(&form.provider) {
        Ok(provider) => provider,
        Err(err) => return json_error_response(StatusCode::BAD_REQUEST, err),
    };
    let (api_key, user_agent) = match source_fields_from_parts(
        provider,
        form.api_key.as_deref().unwrap_or(""),
        form.user_agent.as_deref().unwrap_or(""),
    ) {
        Ok(values) => values,
        Err(err) => return json_error_response(StatusCode::BAD_REQUEST, err),
    };
    let timeout = state
        .config
        .read()
        .external_metadata_timeout_secs
        .max(1);
    let source = ExternalSource {
        provider,
        api_key: if api_key.is_empty() {
            None
        } else {
            Some(api_key)
        },
        user_agent: if user_agent.is_empty() {
            None
        } else {
            Some(user_agent)
        },
        timeout: Duration::from_secs(timeout),
    };
    match external::test_source(&state.external_client, &source).await {
        Ok(()) => json_ok_response(),
        Err(err) => json_error_response(StatusCode::BAD_REQUEST, err),
    }
}

fn admin_settings_page(
    state: &AppState,
    message: Option<String>,
    music_root_override: Option<String>,
) -> Response {
    let config = state.config.read().clone();
    let template = match load_template(state, "templates/settings.html") {
        Ok(template) => template,
        Err(err) => {
            return html_error(
                state,
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("template error: {}", err),
            )
        }
    };

    let config_path = state.config_path.to_string_lossy().to_string();
    let music_root = music_root_override.unwrap_or_else(|| config.music_root.clone());
    let index_full_path = resolve_path(&state.config_path, &config.index_path)
        .to_string_lossy()
        .to_string();
    let metadata_full_path = resolve_path(&state.config_path, &config.metadata_path)
        .to_string_lossy()
        .to_string();
    let watch_checked = if config.watch_music {
        "checked".to_string()
    } else {
        String::new()
    };
    let metadata_sources = render_metadata_sources(state, &config.external_metadata_sources);
    let sources_enabled = has_enabled_sources(&config.external_metadata_sources);
    let message_html = render_message(message);
    let status_block = super::library::render_status_block_for_library(state);
    let modals = [
        load_template(state, "templates/modals/metadata_add.html").unwrap_or_default(),
        load_template(state, "templates/modals/metadata_edit.html").unwrap_or_default(),
        load_template(state, "templates/modals/reindex_confirm.html").unwrap_or_default(),
        load_template(state, "templates/modals/unsaved_changes.html").unwrap_or_default(),
    ]
    .join("");

    let body = apply_template(
        template,
        &[
            ("message", message_html),
            ("status_block", status_block),
            ("config_path", escape_html(&config_path)),
            ("music_root", escape_html(&music_root)),
            ("index_path", escape_html(&config.index_path)),
            ("index_full_path", escape_html(&index_full_path)),
            ("metadata_path", escape_html(&config.metadata_path)),
            ("metadata_full_path", escape_html(&metadata_full_path)),
            ("port", config.port.to_string()),
            ("quic_port", config.quic_port.to_string()),
            ("watch_checked", watch_checked),
            (
                "watch_debounce_secs",
                config.watch_debounce_secs.to_string(),
            ),
            ("session_ttl_secs", config.session_ttl_secs.to_string()),
            (
                "stats_collection_checked",
                if config.stats_collection_enabled {
                    "checked".to_string()
                } else {
                    String::new()
                },
            ),
            (
                "external_enabled_checked",
                if sources_enabled {
                    "checked".to_string()
                } else {
                    String::new()
                },
            ),
            ("metadata_sources", metadata_sources),
            (
                "external_min_interval_secs",
                config.external_metadata_min_interval_secs.to_string(),
            ),
            (
                "external_timeout_secs",
                config.external_metadata_timeout_secs.to_string(),
            ),
            (
                "external_enrich_checked",
                if sources_enabled {
                    "checked".to_string()
                } else {
                    String::new()
                },
            ),
            (
                "external_tag_error_checked",
                if config.external_metadata_on_tag_error {
                    "checked".to_string()
                } else {
                    String::new()
                },
            ),
            (
                "external_scan_limit",
                config.external_metadata_scan_limit.to_string(),
            ),
            ("modals", modals),
        ],
    );
    html_response(
        StatusCode::OK,
        render_admin_page(state, "Settings", &body, PageLayout::standard()),
    )
}

fn render_metadata_sources(state: &AppState, sources: &[MetadataSourceConfig]) -> String {
    if sources.is_empty() {
        return "<p class=\"muted\" id=\"no-metadata-sources\">No metadata sources configured.</p>".to_string();
    }
    let template = load_template(state, "templates/partials/source_row.html").unwrap_or_default();
    let mut out = String::new();
    for source in sources {
        let provider_label = provider_label(source.provider);
        let provider_value = provider_value(source.provider);
        let detail = match source.provider {
            crate::external::Provider::TheAudioDb => {
                if source.api_key.trim().is_empty() {
                    "API key missing".to_string()
                } else {
                    "API key set".to_string()
                }
            }
            crate::external::Provider::MusicBrainz => {
                let ua = source.user_agent.trim();
                if ua.is_empty() {
                    "User agent missing".to_string()
                } else {
                    format!("User agent: {}", truncate_text(ua, 60))
                }
            }
        };
        let checked = if source.enabled { "checked" } else { "" };
        out.push_str(&apply_template(template.clone(), &[
            ("id", escape_html(&source.id)),
            ("provider", escape_html(provider_value)),
            ("api_key", escape_html(&source.api_key)),
            ("user_agent", escape_html(&source.user_agent)),
            ("label", escape_html(provider_label)),
            ("detail", escape_html(&detail)),
            ("checked", checked.to_string()),
        ]));
    }
    out
}

fn provider_label(provider: crate::external::Provider) -> &'static str {
    match provider {
        crate::external::Provider::TheAudioDb => "TheAudioDB",
        crate::external::Provider::MusicBrainz => "MusicBrainz",
    }
}

fn provider_value(provider: crate::external::Provider) -> &'static str {
    match provider {
        crate::external::Provider::TheAudioDb => "theaudiodb",
        crate::external::Provider::MusicBrainz => "musicbrainz",
    }
}

fn has_enabled_sources(sources: &[MetadataSourceConfig]) -> bool {
    sources.iter().any(|source| source.enabled)
}
