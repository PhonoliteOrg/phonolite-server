// crates/server/src/admin/library.rs
use std::collections::HashMap;

use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use common::{Album, Artist, Track};
pub use library::{Library, LibraryStats};
use crate::state::LibraryStatus;
use tracing::warn;

use crate::assets::{
    clear_metadata_assets, cover_response, fetch_cover, fetch_cover_cached, metadata_root_path,
    resolve_artist_banner_source, resolve_artist_cover_source, resolve_artist_logo_source,
    resolve_cover_source, CoverCacheKey,
};
use crate::scan::{start_cover_sweep, start_enrichment_sweep, start_rescan};
use crate::state::{
    AdminLibraryQuery, AppState, ArtistCoverQuery, HealthResponse, LibraryStatusResponse,
};
use crate::utils::{
    apply_template, escape_html, format_duration_ms, format_track_position, html_error,
    html_response, json_error_response, load_template, redirect_to, render_admin_page, truncate_text,
    url_escape, wants_json, PageLayout,
};

use super::auth::{admin_login_page, admin_setup_page};
use super::{
    admin_user_from_headers, is_admin, library_for_admin, library_or_response, render_message,
};

pub async fn admin_library_status(State(state): State<AppState>, headers: HeaderMap) -> Response {
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

    let guard = state.library_state.read();
    let (status, message, artists, albums, tracks) = match &guard.status {
        LibraryStatus::Unconfigured => (
            "unconfigured".to_string(),
            Some("music directory must be set".to_string()),
            None,
            None,
            None,
        ),
        LibraryStatus::Missing(path) => (
            "missing".to_string(),
            Some(format!("music directory not found: {}", path.display())),
            None,
            None,
            None,
        ),
        LibraryStatus::Scanning { started } => {
            let since = started
                .elapsed()
                .map(|elapsed: std::time::Duration| format!("library scan in progress ({}s)", elapsed.as_secs()))
                .unwrap_or_else(|_| "library scan in progress".to_string());
            ("scanning".to_string(), Some(since), None, None, None)
        }
        LibraryStatus::Ready(stats) => (
            "ready".to_string(),
            None,
            Some(stats.artists),
            Some(stats.albums),
            Some(stats.tracks),
        ),
        LibraryStatus::Error(message) => (
            "error".to_string(),
            Some(message.clone()),
            None,
            None,
            None,
        ),
    };

    Json(LibraryStatusResponse {
        status,
        message,
        artists,
        albums,
        tracks,
    })
    .into_response()
}

pub async fn admin_reindex(State(state): State<AppState>, headers: HeaderMap) -> Response {
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

    let message = match library_for_admin(&state) {
        Ok(library) => {
            if let Err(err) = clear_metadata_assets(&state).await {
                warn!("Failed to clear metadata on reindex: {}", err);
            }
            start_rescan(state.clone(), library, true);
            "info: Reindex started. Check status below for progress.".to_string()
        }
        Err(message) => message,
    };

    if wants_json(&headers) {
        if message.starts_with("info:") {
            (StatusCode::ACCEPTED, Json(HealthResponse { status: "indexing" })).into_response()
        } else {
            json_error_response(StatusCode::SERVICE_UNAVAILABLE, message)
        }
    } else {
        let redirect = format!("/settings?message={}", url_escape(&message));
        redirect_to(&redirect)
    }
}

pub async fn admin_library(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AdminLibraryQuery>,
) -> Response {
    if !state.auth.has_admin().unwrap_or(false) {
        return admin_setup_page(&state, None);
    }
    let user = match admin_user_from_headers(&state, &headers) {
        Ok(Some(user)) => user,
        Ok(None) => return admin_login_page(&state, None),
        Err(err) => {
            return html_error(
                &state,
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("auth error: {}", err),
            )
        }
    };
    if !is_admin(&user) {
        return admin_login_page(&state, Some("admin access required".to_string()));
    }
    admin_library_page(&state, query, None, &headers)
}

pub async fn admin_album_cover(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(album_id): AxumPath<String>,
) -> Response {
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

    let library = match library_or_response(&state) {
        Ok(library) => library,
        Err(response) => return response,
    };
    let album = match library.get_album(&album_id) {
        Ok(Some(album)) => album,
        Ok(None) => return json_error_response(StatusCode::NOT_FOUND, "album not found"),
        Err(err) => {
            return json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("library error: {}", err),
            )
        }
    };
    let cover_ref = match album.cover_ref {
        Some(cover) => cover,
        None => return json_error_response(StatusCode::NOT_FOUND, "cover not found"),
    };
    let source = match resolve_cover_source(&library, &cover_ref) {
        Ok(Some(source)) => source,
        Ok(None) => return json_error_response(StatusCode::NOT_FOUND, "cover not found"),
        Err(err) => {
            return json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("library error: {}", err),
            )
        }
    };

    let key = CoverCacheKey::Album(album_id.clone());
    match fetch_cover_cached(&state, key, source).await {
        Ok((bytes, mime)) => cover_response(bytes, &mime),
        Err(err) => json_error_response(StatusCode::NOT_FOUND, err),
    }
}

pub async fn admin_artist_cover(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ArtistCoverQuery>,
    AxumPath(artist_id): AxumPath<String>,
) -> Response {
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

    let library = match library_or_response(&state) {
        Ok(library) => library,
        Err(response) => return response,
    };
    let metadata_root = metadata_root_path(&state);
    let source = match query.kind.as_deref() {
        Some("logo") => resolve_artist_logo_source(&library, &metadata_root, &artist_id),
        Some("banner") => resolve_artist_banner_source(&library, &metadata_root, &artist_id),
        _ => resolve_artist_cover_source(&library, &metadata_root, &artist_id),
    };
    let source = match source {
        Ok(Some(source)) => source,
        Ok(None) => return json_error_response(StatusCode::NOT_FOUND, "cover not found"),
        Err(err) => {
            return json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("library error: {}", err),
            )
        }
    };

    match fetch_cover(source).await {
        Ok((bytes, mime)) => cover_response(bytes, &mime),
        Err(err) => json_error_response(StatusCode::NOT_FOUND, err),
    }
}

fn admin_library_page(
    state: &AppState,
    query: AdminLibraryQuery,
    mut message: Option<String>,
    headers: &HeaderMap,
) -> Response {
    let template = match load_template(state, "templates/library.html") {
        Ok(template) => template,
        Err(err) => {
            return html_error(
                state,
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("template error: {}", err),
            )
        }
    };

    let is_fragment = headers.get("X-Fragment").map(|v| v == "true").unwrap_or(false);

    let status_block = render_status_block_for_library(state);
    let search = query.search.clone().unwrap_or_default();
    let search_trimmed = search.trim().to_string();
    let filter = normalize_filter(query.filter.as_deref());
    let offset = query.offset.unwrap_or(0);
    let limit = 24;
    let mut search_block = render_library_search_block(state, &search_trimmed, filter);

    let content: String;
    let mut pagination = String::new();

    let library = match library_for_admin(state) {
        Ok(library) => Some(library),
        Err(err) => {
            if message.is_none() {
                message = Some(err);
            }
            None
        }
    };

    let guard = state.library_state.read();
    if let LibraryStatus::Ready(stats) = &guard.status {
        search_block.push_str(&render_library_snapshot(
            state,
            stats,
            &search_trimmed,
            filter,
        ));
    }

    if is_fragment {
        if let Some(library) = library {
            if let Some(album_id) = query.album_id {
                content = render_album_detail(state, &library, &album_id, &mut message);
            } else if let Some(artist_id) = query.artist_id {
                content = render_artist_detail(state, &library, &artist_id, &mut message);
            } else {
                let (block, total) = render_search_results(
                    state,
                    &library,
                    &search_trimmed,
                    filter,
                    limit,
                    offset,
                    &mut message,
                );
                content = block;
                if let Some(total) = total {
                    let base = search_base_query(&search_trimmed, filter);
                    pagination = render_pagination(state, &base, offset, limit, total);
                }
            }
        } else {
            content = "<p class=\"muted\">Library not ready yet.</p>".to_string();
        }
        return Json(json!({
            "content": content,
            "pagination": pagination,
            "message": render_message(message),
            "search_block": search_block,
        })).into_response();
    }

    // Initial load - empty content to be fetched by JS
    content = String::new();
    if library.is_none() {
        // If library is not ready, we might want to show that immediately
        // But the status block handles that.
    }

    let message_html = render_message(message);
    let body = apply_template(
        template,
        &[
            ("message", message_html),
            ("status_block", status_block),
            ("search_block", search_block),
            ("content", content),
            ("pagination", pagination),
        ],
    );
    html_response(
        StatusCode::OK,
        render_admin_page(state, "Library", &body, PageLayout::standard()),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SearchFilter {
    Artists,
    Albums,
    Tracks,
}

pub async fn admin_scan(State(state): State<AppState>, headers: HeaderMap) -> Response {
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

    let message = match library_for_admin(&state) {
        Ok(library) => {
            start_enrichment_sweep(state.clone(), library.clone(), false);
            start_cover_sweep(state.clone(), library);
            "info: Scan started. Missing metadata will be refreshed in the background.".to_string()
        }
        Err(message) => message,
    };

    if wants_json(&headers) {
        if message.starts_with("info:") {
            (StatusCode::ACCEPTED, Json(HealthResponse { status: "indexing" })).into_response()
        } else {
            json_error_response(StatusCode::SERVICE_UNAVAILABLE, message)
        }
    } else {
        let redirect = format!("/settings?message={}", url_escape(&message));
        redirect_to(&redirect)
    }
}

fn normalize_filter(value: Option<&str>) -> SearchFilter {
    match value.map(|v| v.trim().to_ascii_lowercase()) {
        Some(ref v) if v == "artists" => SearchFilter::Artists,
        Some(ref v) if v == "albums" => SearchFilter::Albums,
        Some(ref v) if v == "tracks" => SearchFilter::Tracks,
        _ => SearchFilter::Artists,
    }
}

fn filter_label(filter: SearchFilter) -> &'static str {
    match filter {
        SearchFilter::Artists => "artists",
        SearchFilter::Albums => "albums",
        SearchFilter::Tracks => "tracks",
    }
}

fn search_base_query(search: &str, filter: SearchFilter) -> String {
    let mut out = format!("/library?search={}", url_escape(search));
    out.push_str(&format!("&filter={}", filter_label(filter)));
    out
}

fn render_library_search_block(state: &AppState, search: &str, filter: SearchFilter) -> String {
    let value = escape_html(search);
    let hidden_filter = format!(
        "<input type=\"hidden\" name=\"filter\" value=\"{}\" />",
        filter_label(filter)
    );

    let clear_icon = r#"<svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="18" y1="6" x2="6" y2="18"></line><line x1="6" y1="6" x2="18" y2="18"></line></svg>"#;

    let clear_button = if !value.is_empty() {
        format!(r#"<a href="/library" class="icon-button" title="Clear search" style="opacity: 0.6; margin-right: 4px;">{}</a>"#, clear_icon)
    } else {
        String::new()
    };

    let template = load_template(state, "templates/partials/library_search.html").unwrap_or_default();
    apply_template(template, &[
        ("value", value),
        ("clear_button", clear_button),
        ("hidden_filter", hidden_filter),
    ])
}

fn render_library_tabs(search: &str, filter: SearchFilter) -> String {
    let options = [
        (SearchFilter::Artists, "Artists"),
        (SearchFilter::Albums, "Albums"),
        (SearchFilter::Tracks, "Tracks"),
    ];
    let mut tabs_html = String::new();
    for (option, label) in options {
        let href = search_base_query(search, option);
        let class = if option == filter {
            "tab-chip active"
        } else {
            "tab-chip"
        };
        tabs_html.push_str(&format!(
            "<a class=\"{}\" href=\"{}\">{}</a>",
            class,
            escape_html(&href),
            label
        ));
    }
    tabs_html
}

fn render_search_results(
    state: &AppState,
    library: &Library,
    search: &str,
    filter: SearchFilter,
    limit: usize,
    offset: usize,
    message: &mut Option<String>,
) -> (String, Option<usize>) {
    match filter {
        SearchFilter::Artists => {
            let (items, total) =
                render_artist_tiles_section(state, library, search, limit, offset, message);
            (items, Some(total))
        }
        SearchFilter::Albums => {
            let (items, total) =
                render_album_tiles_section(state, library, search, limit, offset, message);
            (items, Some(total))
        }
        SearchFilter::Tracks => {
            let (items, total) =
                render_track_tiles_section(state, library, search, limit, offset, message);
            (items, Some(total))
        }
    }
}

fn render_artist_tiles_section(
    state: &AppState,
    library: &Library,
    search: &str,
    limit: usize,
    offset: usize,
    message: &mut Option<String>,
) -> (String, usize) {
    let search_opt = search.trim();
    let search_opt = if search_opt.is_empty() {
        None
    } else {
        Some(search_opt)
    };

    let (items, total) = match library.list_artists(search_opt, limit, offset) {
        Ok(result) => result,
        Err(err) => {
            *message = Some(format!("library error: {}", err));
            return ("<p class=\"muted\">Failed to load artists.</p>".to_string(), 0);
        }
    };

    let tiles = render_artist_tiles(state, library, &items);
    (tiles, total)
}

fn render_album_tiles_section(
    state: &AppState,
    library: &Library,
    search: &str,
    limit: usize,
    offset: usize,
    message: &mut Option<String>,
) -> (String, usize) {
    let search_opt = search.trim();
    let search_opt = if search_opt.is_empty() {
        None
    } else {
        Some(search_opt)
    };

    let (items, total) = match library.list_albums(search_opt, limit, offset) {
        Ok(result) => result,
        Err(err) => {
            *message = Some(format!("library error: {}", err));
            return ("<p class=\"muted\">Failed to load albums.</p>".to_string(), 0);
        }
    };

    let tiles = render_album_tiles(state, library, &items);
    (tiles, total)
}

fn render_track_tiles_section(
    state: &AppState,
    library: &Library,
    search: &str,
    limit: usize,
    offset: usize,
    message: &mut Option<String>,
) -> (String, usize) {
    let search_opt = search.trim();
    let search_opt = if search_opt.is_empty() {
        None
    } else {
        Some(search_opt)
    };

    let (items, total) = match library.list_tracks(search_opt, limit, offset) {
        Ok(result) => result,
        Err(err) => {
            *message = Some(format!("library error: {}", err));
            return ("<p class=\"muted\">Failed to load tracks.</p>".to_string(), 0);
        }
    };

    let tiles = render_track_tiles(state, library, &items);
    (tiles, total)
}

fn render_artist_tiles(state: &AppState, library: &Library, artists: &[Artist]) -> String {
    if artists.is_empty() {
        return "<p class=\"muted\">No artists found.</p>".to_string();
    }
    let template = load_template(state, "templates/partials/tile_artist.html").unwrap_or_default();
    let mut tiles = String::new();
    for artist in artists {
        let logo_url = artist_cover_url(library, &artist.id);
        let cover_html = if let Some(url) = &logo_url {
            format!("<img class=\"tile-cover artist-cover-img\" src=\"{}\" alt=\"{}\" />", escape_html(url), escape_html(&artist.name))
        } else {
            "<div class=\"tile-cover artist-cover-placeholder\"></div>".to_string()
        };
        let link = format!("/library?artist_id={}", url_escape(&artist.id));
        let genres = format_genres_html(&artist.genres);
        tiles.push_str(&apply_template(template.clone(), &[
            ("link", escape_html(&link)),
            ("cover_html", cover_html),
            ("name", escape_html(&artist.name)),
            ("genres", genres),
        ]));
    }
    format!("<div class=\"tile-grid\">{}</div>", tiles)
}

fn render_album_tiles(state: &AppState, library: &Library, albums: &[Album]) -> String {
    if albums.is_empty() {
        return "<p class=\"muted\">No albums found.</p>".to_string();
    }
    let template = load_template(state, "templates/partials/tile_album.html").unwrap_or_default();
    let mut artist_cache: HashMap<String, Artist> = HashMap::new();
    let mut tiles = String::new();
    for album in albums {
        let link = format!("/library?album_id={}", url_escape(&album.id));
        let cover_url = if album.cover_ref.is_some() {
            Some(format!("/covers/albums/{}", url_escape(&album.id)))
        } else {
            None
        };
        let cover = render_tile_cover(cover_url.as_deref());
        let artist = artist_cache
            .get(&album.artist_id)
            .cloned()
            .or_else(|| {
                library
                    .get_artist(&album.artist_id)
                    .ok()
                    .flatten()
                    .map(|artist| {
                        artist_cache.insert(album.artist_id.clone(), artist.clone());
                        artist
                    })
            });
        let artist_name = artist
            .as_ref()
            .map(|artist| artist.name.clone())
            .unwrap_or_else(|| "Unknown Artist".to_string());
        let year = album
            .year
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string());
        let summary = render_tile_summary(album.summary.as_deref());
        let genres = format_genres_html(&album.genres);
        tiles.push_str(&apply_template(template.clone(), &[
            ("link", escape_html(&link)),
            ("cover", cover),
            ("title", escape_html(&album.title)),
            ("artist", escape_html(&artist_name)),
            ("year", escape_html(&year)),
            ("genres", genres),
            ("summary", summary),
        ]));
    }
    format!("<div class=\"tile-grid\">{}</div>", tiles)
}

fn render_track_tiles(state: &AppState, library: &Library, tracks: &[Track]) -> String {
    if tracks.is_empty() {
        return "<p class=\"muted\">No tracks found.</p>".to_string();
    }
    let template = load_template(state, "templates/partials/tile_track.html").unwrap_or_default();
    let mut album_cache: HashMap<String, Album> = HashMap::new();
    let mut artist_cache: HashMap<String, Artist> = HashMap::new();
    let mut tiles = String::new();
    for track in tracks {
        let album_link = format!("/library?album_id={}", url_escape(&track.album_id));
        let album = album_cache
            .get(&track.album_id)
            .cloned()
            .or_else(|| {
                library
                    .get_album(&track.album_id)
                    .ok()
                    .flatten()
                    .map(|album| {
                        album_cache.insert(track.album_id.clone(), album.clone());
                        album
                    })
            });
        let album_title = album
            .as_ref()
            .map(|album| album.title.clone())
            .unwrap_or_else(|| "Unknown Album".to_string());
        let artist = artist_cache
            .get(&track.artist_id)
            .cloned()
            .or_else(|| {
                library
                    .get_artist(&track.artist_id)
                    .ok()
                    .flatten()
                    .map(|artist| {
                        artist_cache.insert(track.artist_id.clone(), artist.clone());
                        artist
                    })
            });
        let artist_name = artist
            .as_ref()
            .map(|artist| artist.name.clone())
            .unwrap_or_else(|| "Unknown Artist".to_string());
        let cover_url = album
            .as_ref()
            .and_then(|album| album.cover_ref.as_ref())
            .map(|_| format!("/covers/albums/{}", url_escape(&track.album_id)));
        let cover = render_tile_cover(cover_url.as_deref());
        let summary_source = album
            .as_ref()
            .and_then(|album| album.summary.as_deref())
            .or_else(|| artist.as_ref().and_then(|artist| artist.summary.as_deref()));
        let summary = render_tile_summary(summary_source);
        let duration = format_duration_ms(track.duration_ms);
        let position = format_track_position(track.disc_no, track.track_no);
        let genres = format_genres_html(&track.genres);
        tiles.push_str(&apply_template(template.clone(), &[
            ("link", escape_html(&album_link)),
            ("cover", cover),
            ("title", escape_html(&track.title)),
            ("position", escape_html(&position)),
            ("duration", escape_html(&duration)),
            ("artist", escape_html(&artist_name)),
            ("album", escape_html(&album_title)),
            ("genres", genres),
            ("summary", summary),
        ]));
    }
    format!("<div class=\"tile-grid\">{}</div>", tiles)
}

fn render_tile_cover(url: Option<&str>) -> String {
    match url {
        Some(url) => format!(
            "<img class=\"tile-cover\" src=\"{}\" alt=\"cover\" />",
            escape_html(url)
        ),
        None => "<div class=\"tile-cover\"></div>".to_string(),
    }
}

fn render_tile_summary(summary: Option<&str>) -> String {
    let summary = summary
        .map(|value| value.trim())
        .filter(|value| !value.is_empty());
    let summary = match summary {
        Some(value) => value,
        None => return String::new(),
    };
    let truncated = truncate_text(summary, 160);
    format!("<div class=\"tile-summary\">{}</div>", escape_html(&truncated))
}

fn artist_cover_url(library: &Library, artist_id: &str) -> Option<String> {
    if let Ok(Some(artist)) = library.get_artist(artist_id) {
        if artist.logo_ref.is_some() {
            return Some(format!(
                "/covers/artists/{}?kind=logo",
                url_escape(artist_id)
            ));
        }
        if artist.banner_ref.is_some() {
            return Some(format!("/covers/artists/{}", url_escape(artist_id)));
        }
    }
    let albums = library.list_artist_albums(artist_id).ok()?;
    for album in albums {
        if album.cover_ref.is_some() {
            return Some(format!("/covers/artists/{}", url_escape(artist_id)));
        }
    }
    None
}

fn render_artist_detail(
    state: &AppState,
    library: &Library,
    artist_id: &str,
    message: &mut Option<String>,
) -> String {
    let artist = match library.get_artist(artist_id) {
        Ok(Some(artist)) => artist,
        Ok(None) => return "<p class=\"muted\">Artist not found.</p>".to_string(),
        Err(err) => {
            *message = Some(format!("library error: {}", err));
            return "<p class=\"muted\">Failed to load artist.</p>".to_string();
        }
    };

    let albums = match library.list_artist_albums(&artist.id) {
        Ok(items) => items,
        Err(err) => {
            *message = Some(format!("library error: {}", err));
            Vec::new()
        }
    };

    let summary = artist
        .summary
        .unwrap_or_else(|| "No summary available.".to_string());
    let genres = format_genres_html(&artist.genres);
    let back = "/library?filter=artists".to_string();

    let banner_url = if artist.banner_ref.is_some() {
        Some(format!(
            "/covers/artists/{}?kind=banner",
            url_escape(&artist.id)
        ))
    } else {
        None
    };
    let logo_url = if artist.logo_ref.is_some() {
        Some(format!(
            "/covers/artists/{}?kind=logo",
            url_escape(&artist.id)
        ))
    } else {
        None
    };

    let (header_style, header_class) = if let Some(banner) = banner_url {
        (format!("background-image: linear-gradient(rgba(0,0,0,0.7), rgba(0,0,0,0.7)), url('{}');", banner), "artist-header-dynamic")
    } else {
        (String::new(), "artist-header-default")
    };

    let logo_img = if let Some(logo) = logo_url {
        format!("<img src=\"{}\" class=\"artist-logo\" alt=\"logo\" />", logo)
    } else {
        String::new()
    };

    let albums_html = render_album_tiles(state, library, &albums);

    let template = load_template(state, "templates/partials/artist_detail.html").unwrap_or_default();
    apply_template(template, &[
        ("header_style", header_style),
        ("header_class", header_class.to_string()),
        ("logo_img", logo_img),
        ("name", escape_html(&artist.name)),
        ("genres", genres),
        ("back_link", escape_html(&back)),
        ("summary", escape_html(&summary)),
        ("albums_html", albums_html),
    ])
}

fn render_album_detail(
    state: &AppState,
    library: &Library,
    album_id: &str,
    message: &mut Option<String>,
) -> String {
    let album = match library.get_album(album_id) {
        Ok(Some(album)) => album,
        Ok(None) => return "<p class=\"muted\">Album not found.</p>".to_string(),
        Err(err) => {
            *message = Some(format!("library error: {}", err));
            return "<p class=\"muted\">Failed to load album.</p>".to_string();
        }
    };

    let artist_name = library
        .get_artist(&album.artist_id)
        .ok()
        .and_then(|artist| artist.map(|value| value.name))
        .unwrap_or_else(|| "Unknown Artist".to_string());
    let artist_link = format!("/library?artist_id={}", url_escape(&album.artist_id));
    let cover = if album.cover_ref.is_some() {
        let cover_url = format!("/covers/albums/{}", url_escape(&album.id));
        format!(
            "<img class=\"cover-lg\" src=\"{}\" alt=\"cover\" />",
            escape_html(&cover_url)
        )
    } else {
        "<div class=\"cover-lg\"></div>".to_string()
    };
    let year = album
        .year
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_string());
    let genres = format_genres_html(&album.genres);
    let summary = album
        .summary
        .as_deref()
        .unwrap_or("No summary available.")
        .to_string();

    let tracks = match library.get_album_tracks(&album.id) {
        Ok(items) => items,
        Err(err) => {
            *message = Some(format!("library error: {}", err));
            Vec::new()
        }
    };

    let mut rows = String::new();
    for track in tracks {
        let pos = format_track_position(track.disc_no, track.track_no);
        let duration = format_duration_ms(track.duration_ms);
        let genres = format_genres_html(&track.genres);
        rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape_html(&pos),
            escape_html(&track.title),
            escape_html(&duration),
            genres
        ));
    }
    if rows.is_empty() {
        rows.push_str("<tr><td colspan=\"4\">No tracks found.</td></tr>");
    }

    let track_table = format!(
        "<table><thead><tr><th>#</th><th>Track</th><th>Duration</th><th>Genres</th></tr></thead><tbody>{}</tbody></table>",
        rows
    );

    let template = load_template(state, "templates/partials/album_detail.html").unwrap_or_default();
    apply_template(template, &[
        ("title", escape_html(&album.title)),
        ("artist_link", escape_html(&artist_link)),
        ("artist_name", escape_html(&artist_name)),
        ("cover", cover),
        ("year", escape_html(&year)),
        ("genres", genres),
        ("folder", escape_html(&album.folder_relpath)),
        ("summary", escape_html(&summary)),
        ("track_table", track_table),
    ])
}

fn render_pagination(state: &AppState, base: &str, offset: usize, limit: usize, total: usize) -> String {
    if total <= limit {
        return String::new();
    }
    let start = offset + 1;
    let end = (offset + limit).min(total);
    let total_pages = (total + limit - 1) / limit;
    let current_page = offset / limit;
    let mut controls = String::new();
    if offset > 0 {
        let prev = offset.saturating_sub(limit);
        let href = format!("{}&offset={}", base, prev);
        controls.push_str(&format!(
            "<a class=\"button ghost\" href=\"{}\">Prev</a>",
            escape_html(&href)
        ));
    }
    let window = 2usize;
    let start_page = current_page.saturating_sub(window);
    let end_page = (current_page + window).min(total_pages.saturating_sub(1));
    for page in start_page..=end_page {
        let page_offset = page * limit;
        let href = format!("{}&offset={}", base, page_offset);
        let class = if page == current_page {
            "page-link active"
        } else {
            "page-link"
        };
        controls.push_str(&format!(
            "<a class=\"{}\" href=\"{}\">{}</a>",
            class,
            escape_html(&href),
            page + 1
        ));
    }
    if offset + limit < total {
        let next = offset + limit;
        let href = format!("{}&offset={}", base, next);
        controls.push_str(&format!(
            "<a class=\"button ghost\" href=\"{}\">Next</a>",
            escape_html(&href)
        ));
    }
    let template = load_template(state, "templates/partials/pagination.html").unwrap_or_default();
    apply_template(template, &[
        ("start", start.to_string()),
        ("end", end.to_string()),
        ("total", total.to_string()),
        ("controls", controls),
    ])
}

fn format_genres_html(genres: &[String]) -> String {
    if genres.is_empty() {
        "<span class=\"muted\">-</span>".to_string()
    } else {
        escape_html(&genres.join(", "))
    }
}

pub fn render_status_block_for_library(state: &AppState) -> String {
    let guard = state.library_state.read();
    render_status_notice(state, &guard.status).unwrap_or_default()
}

fn render_status_notice(state: &AppState, status: &LibraryStatus) -> Option<String> {
    let template = load_template(state, "templates/partials/status_notice.html").unwrap_or_default();
    match status {
        LibraryStatus::Unconfigured => Some(apply_template(template, &[
            ("class", "warn".to_string()),
            ("attrs", "".to_string()),
            ("content", "<strong>Setup needed</strong> Music directory not set. Choose a folder to start indexing.".to_string()),
        ])),
        LibraryStatus::Missing(path) => Some(apply_template(template, &[
            ("class", "error".to_string()),
            ("attrs", "".to_string()),
            ("content", format!("<strong>Missing</strong> Music directory not found: <code>{}</code>", escape_html(&path.display().to_string()))),
        ])),
        LibraryStatus::Scanning { started } => {
            let since = started
                .elapsed()
                .map(|elapsed: std::time::Duration| format!(" (started {}s ago)", elapsed.as_secs()))
                .unwrap_or_default();
            Some(apply_template(template, &[
                ("class", "".to_string()),
                ("attrs", "data-status=\"scanning\"".to_string()),
                ("content", format!("<strong>Indexing</strong> Library scan in progress{}. Refresh this page for updates.", escape_html(&since))),
            ]))
        }
        LibraryStatus::Ready(_) => None,
        LibraryStatus::Error(message) => Some(apply_template(template, &[
            ("class", "error".to_string()),
            ("attrs", "".to_string()),
            ("content", format!("<strong>Error</strong> {}", escape_html(message))),
        ])),
    }
}

fn render_library_snapshot(
    state: &AppState,
    stats: &LibraryStats,
    search: &str,
    filter: SearchFilter,
) -> String {
    let template = load_template(state, "templates/partials/library_snapshot.html").unwrap_or_default();
    let tabs_html = render_library_tabs(search, filter);
    apply_template(template, &[
        ("artists", stats.artists.to_string()),
        ("albums", stats.albums.to_string()),
        ("tracks", stats.tracks.to_string()),
        ("tabs_html", tabs_html),
    ])
}
