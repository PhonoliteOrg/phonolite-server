//! Library page and fragment composition, independent of authenticated actions.
use crate::admin::library_for_admin;
use crate::admin::presentation::{
    apply_template, escape_html, format_duration_ms, format_track_position, html_error,
    load_template, render_admin_response, truncate_text, url_escape, PageLayout,
};
use crate::library_search::search_library;
use crate::state::{AdminLibraryQuery, AppState, LibraryStatus, SearchResult};
use ::library::{Library, LibraryStats};
use axum::{
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use common::{genre_display_label, Album, Artist, Track};
use serde::Serialize;
use std::collections::HashMap;

pub(super) fn admin_library_page(
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

    let is_fragment = headers
        .get("X-Fragment")
        .map(|v| v == "true")
        .unwrap_or(false);

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

    let status = state.library_state.read().status.clone();
    if let LibraryStatus::Ready(stats) = &status {
        search_block =
            render_library_snapshot(state, stats, &search_trimmed, filter, &search_block);
    }

    if let Some(library) = library {
        if let Some(album_id) = query.album_id {
            content = render_album_detail(state, &library, &album_id, &mut message);
        } else if let Some(artist_id) = query.artist_id {
            content = render_artist_detail(state, &library, &artist_id, &mut message);
        } else if !search_trimmed.is_empty() {
            content = render_ranked_search(state, &library, &search_trimmed, filter, &mut message);
        } else {
            let (block, total) = render_browse_results(
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
        content = render_empty_state(
            "No music found yet",
            "Artists, albums, and tracks will appear here once your music is indexed.",
        );
    }

    let view = LibraryPageView {
        content,
        pagination,
        message: message
            .map(|error| {
                format!(
                    r#"<p class="inline-error visible" role="alert">{}</p>"#,
                    escape_html(&error)
                )
            })
            .unwrap_or_default(),
        search_block,
    };
    if is_fragment {
        return Json(view).into_response();
    }

    let body = view.into_html(template, matches!(status, LibraryStatus::Scanning { .. }));
    render_admin_response(
        state,
        StatusCode::OK,
        "Library",
        &body,
        PageLayout::standard(),
    )
}

#[derive(Serialize)]
struct LibraryPageView {
    content: String,
    pagination: String,
    message: String,
    search_block: String,
}

impl LibraryPageView {
    fn into_html(self, template: String, scanning: bool) -> String {
        apply_template(
            template,
            &[
                ("message", self.message),
                ("library_scanning", scanning.to_string()),
                ("search_block", self.search_block),
                ("pagination", self.pagination),
                ("content", self.content),
            ],
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SearchFilter {
    Artists,
    Albums,
    Tracks,
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

    let clear_icon = include_str!("../../../../../web/static/icons/lucide/x.svg");

    let clear_link = search_base_query("", filter);
    let clear_button = format!(
        r#"<a href="{}" class="icon-button search-clear" data-library-link aria-label="Clear search"{}>{}</a>"#,
        escape_html(&clear_link),
        if value.is_empty() { " hidden" } else { "" },
        clear_icon
    );

    let template =
        load_template(state, "templates/partials/library_search.html").unwrap_or_default();
    apply_template(
        template,
        &[
            ("value", value),
            ("clear_button", clear_button),
            ("hidden_filter", hidden_filter),
        ],
    )
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
        let current = if option == filter {
            " aria-current=\"page\""
        } else {
            ""
        };
        tabs_html.push_str(&format!(
            "<a class=\"{}\" href=\"{}\" data-library-link{}>{}</a>",
            class,
            escape_html(&href),
            current,
            label
        ));
    }
    tabs_html
}

fn render_empty_state(title: &str, description: &str) -> String {
    format!(
        "<div class=\"empty-state\" role=\"status\"><p class=\"empty-state-title\">{}</p><p>{}</p></div>",
        escape_html(title),
        escape_html(description)
    )
}

fn render_search_empty_state(search: &str, filter: SearchFilter) -> String {
    let kind = filter_label(filter);
    if search.trim().is_empty() {
        render_empty_state(
            &format!("No {kind} yet"),
            "Add a music folder in Settings or scan your library to discover your collection.",
        )
    } else {
        render_empty_state(
            &format!("No {kind} found"),
            &format!(
                "No matches for \"{search}\". Try another search or browse a different category."
            ),
        )
    }
}

fn render_ranked_search(
    state: &AppState,
    library: &Library,
    search: &str,
    filter: SearchFilter,
    message: &mut Option<String>,
) -> String {
    if search.chars().count() < 2 {
        return render_empty_state("Search library", "Type at least 2 characters to search.");
    }
    let results = match search_library(library, search, 50) {
        Ok(results) => results,
        Err(err) => {
            *message = Some(format!("library error: {err}"));
            return render_empty_state("Search could not be loaded", "Try searching again.");
        }
    };
    let template =
        load_template(state, "templates/partials/search_result.html").unwrap_or_default();
    render_library_matches(library, &results, search, filter, &template)
}

fn render_library_matches(
    library: &Library,
    results: &[SearchResult],
    search: &str,
    filter: SearchFilter,
    template: &str,
) -> String {
    if results.is_empty() {
        return render_empty_state(
            "No results found",
            &format!("No matches for \"{search}\". Try another search term."),
        );
    }
    let mut rows = String::new();
    for result in results {
        let mut link = search_base_query(search, filter);
        let (kind, cover_url) = if result.kind == "artist" {
            link.push_str(&format!("&artist_id={}", url_escape(&result.id)));
            ("Artist", artist_cover_url(library, &result.id))
        } else {
            let album_id = result.album_id.as_deref().unwrap_or(&result.id);
            link.push_str(&format!("&album_id={}", url_escape(album_id)));
            if result.kind == "track" {
                link.push_str(&format!("#track-{}", url_escape(&result.id)));
            }
            let cover_url = library
                .get_album(album_id)
                .ok()
                .flatten()
                .filter(|album| album.cover_ref.is_some())
                .map(|_| format!("/covers/albums/{}", url_escape(album_id)));
            (
                if result.kind == "track" {
                    "Track"
                } else {
                    "Album"
                },
                cover_url,
            )
        };
        let kind_icon = match result.kind.as_str() {
            "artist" => include_str!("../../../../../web/static/icons/lucide/user-round.svg"),
            "track" => include_str!("../../../../../web/static/icons/lucide/music.svg"),
            _ => include_str!("../../../../../web/static/icons/lucide/disc-3.svg"),
        };
        rows.push_str(&apply_template(
            template.to_string(),
            &[
                ("link", escape_html(&link)),
                ("cover", render_tile_cover(cover_url.as_deref())),
                ("title", escape_html(&result.title)),
                (
                    "subtitle",
                    escape_html(result.subtitle.as_deref().unwrap_or_default()),
                ),
                ("kind", kind.to_string()),
                ("result_kind", escape_html(&result.kind)),
                ("kind_icon", kind_icon.to_string()),
                (
                    "chevron",
                    include_str!("../../../../../web/static/icons/lucide/chevron-right.svg")
                        .to_string(),
                ),
            ],
        ));
    }
    format!(
        r#"<section class="library-search-results" aria-labelledby="library-results-title"><div class="tile-section-header"><h2 id="library-results-title">Search results</h2><p class="muted" role="status">{} matches</p></div><div class="library-search-results-list" role="list" aria-label="Search results">{rows}</div></section>"#,
        results.len()
    )
}

fn render_browse_results(
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

    let (items, total) = match library.list_artists_for_browse(search_opt, limit, offset, false) {
        Ok(result) => result,
        Err(err) => {
            *message = Some(format!("library error: {}", err));
            return (
                render_empty_state(
                    "Artists could not be loaded",
                    "Refresh the page to try again.",
                ),
                0,
            );
        }
    };

    if items.is_empty() {
        return (
            render_search_empty_state(search, SearchFilter::Artists),
            total,
        );
    }
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
            return (
                render_empty_state(
                    "Albums could not be loaded",
                    "Refresh the page to try again.",
                ),
                0,
            );
        }
    };

    if items.is_empty() {
        return (
            render_search_empty_state(search, SearchFilter::Albums),
            total,
        );
    }
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
            return (
                render_empty_state(
                    "Tracks could not be loaded",
                    "Refresh the page to try again.",
                ),
                0,
            );
        }
    };

    if items.is_empty() {
        return (
            render_search_empty_state(search, SearchFilter::Tracks),
            total,
        );
    }
    let tiles = render_track_tiles(state, library, &items);
    (tiles, total)
}

fn render_artist_tiles(state: &AppState, library: &Library, artists: &[Artist]) -> String {
    if artists.is_empty() {
        return render_search_empty_state("", SearchFilter::Artists);
    }
    let template = load_template(state, "templates/partials/tile_artist.html").unwrap_or_default();
    let mut tiles = String::new();
    for artist in artists {
        let logo_url = artist_cover_url(library, &artist.id);
        let cover_html = if let Some(url) = &logo_url {
            format!(
                "<img class=\"tile-cover artist-cover-img\" src=\"{}\" alt=\"\" loading=\"lazy\" />",
                escape_html(url)
            )
        } else {
            render_artwork_placeholder("tile-cover artist-cover-placeholder")
        };
        let link = format!("/library?artist_id={}", url_escape(&artist.id));
        let genres = format_genres_html(&artist.genres);
        tiles.push_str(&apply_template(
            template.clone(),
            &[
                ("link", escape_html(&link)),
                ("cover_html", cover_html),
                ("name", escape_html(&artist.name)),
                ("genres", genres),
            ],
        ));
    }
    format!("<div class=\"tile-grid artist-grid\">{}</div>", tiles)
}

fn render_album_tiles(state: &AppState, library: &Library, albums: &[Album]) -> String {
    if albums.is_empty() {
        return render_empty_state(
            "No albums yet",
            "Albums will appear here when this artist has indexed tracks.",
        );
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
        let artist_name = album_artist_name(library, &mut artist_cache, album);
        let year = album
            .year
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string());
        let summary = render_tile_summary(album.summary.as_deref());
        let genres = format_genres_html(&album.genres);
        tiles.push_str(&apply_template(
            template.clone(),
            &[
                ("link", escape_html(&link)),
                ("cover", cover),
                ("title", escape_html(&album.title)),
                ("artist", escape_html(&artist_name)),
                ("year", escape_html(&year)),
                ("genres", genres),
                ("summary", summary),
            ],
        ));
    }
    format!("<div class=\"tile-grid album-grid\">{}</div>", tiles)
}

fn render_track_tiles(state: &AppState, library: &Library, tracks: &[Track]) -> String {
    if tracks.is_empty() {
        return render_search_empty_state("", SearchFilter::Tracks);
    }
    let template = load_template(state, "templates/partials/tile_track.html").unwrap_or_default();
    let mut album_cache: HashMap<String, Album> = HashMap::new();
    let mut artist_cache: HashMap<String, Artist> = HashMap::new();
    let mut tiles = String::new();
    for track in tracks {
        let album_link = format!("/library?album_id={}", url_escape(&track.album_id));
        let album = album_cache.get(&track.album_id).cloned().or_else(|| {
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
        let artist = artist_cache.get(&track.artist_id).cloned().or_else(|| {
            library
                .get_artist(&track.artist_id)
                .ok()
                .flatten()
                .map(|artist| {
                    artist_cache.insert(track.artist_id.clone(), artist.clone());
                    artist
                })
        });
        let artist_name = library
            .track_credits(&track.id)
            .ok()
            .map(|credits| credits.display_name)
            .filter(|name| !name.trim().is_empty())
            .or_else(|| artist.as_ref().map(|artist| artist.name.clone()))
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
        tiles.push_str(&apply_template(
            template.clone(),
            &[
                ("link", escape_html(&album_link)),
                ("cover", cover),
                ("title", escape_html(&track.title)),
                ("position", escape_html(&position)),
                ("duration", escape_html(&duration)),
                ("artist", escape_html(&artist_name)),
                ("album", escape_html(&album_title)),
                ("genres", genres),
                ("summary", summary),
            ],
        ));
    }
    format!("<div class=\"tile-grid track-grid\">{}</div>", tiles)
}

fn render_tile_cover(url: Option<&str>) -> String {
    match url {
        Some(url) => format!(
            "<img class=\"tile-cover\" src=\"{}\" alt=\"\" loading=\"lazy\" />",
            escape_html(url)
        ),
        None => render_artwork_placeholder("tile-cover"),
    }
}

fn render_artwork_placeholder(classes: &str) -> String {
    format!(
        r#"<div class="{} artwork-placeholder" aria-hidden="true"><svg viewBox="0 0 24 24" width="40" height="40" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" focusable="false"><path d="M9 18V5l12-2v13M9 8l12-2"/><ellipse cx="6" cy="18" rx="3" ry="2"/><ellipse cx="18" cy="16" rx="3" ry="2"/></svg></div>"#,
        escape_html(classes)
    )
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
    format!(
        "<div class=\"tile-summary\">{}</div>",
        escape_html(&truncated)
    )
}

fn album_artist_name(
    library: &Library,
    artist_cache: &mut HashMap<String, Artist>,
    album: &Album,
) -> String {
    if let Ok(credits) = library.album_credits(&album.id) {
        if !credits.display_name.trim().is_empty() {
            return credits.display_name;
        }
    }
    let display = album.artist_display_name();
    if !display.trim().is_empty() {
        return display;
    }

    artist_cache
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
        })
        .map(|artist| artist.name)
        .unwrap_or_else(|| "Unknown Artist".to_string())
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
        Ok(None) => {
            return render_empty_state(
                "Artist not found",
                "This artist may have been removed. Browse the library to find another artist.",
            )
        }
        Err(err) => {
            *message = Some(format!("library error: {}", err));
            return render_empty_state(
                "Artist could not be loaded",
                "Refresh the page to try again.",
            );
        }
    };

    let releases = match library.artist_release_relationships(&artist.id) {
        Ok(items) => items,
        Err(err) => {
            *message = Some(format!("library error: {}", err));
            ::library::ArtistReleaseRelationships::default()
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
        (
            escape_html(&format!("--artist-banner: url('{}');", banner)),
            "artist-header-dynamic",
        )
    } else {
        (String::new(), "artist-header-default")
    };

    let logo_img = if let Some(logo) = logo_url {
        format!(
            "<img src=\"{}\" class=\"artist-logo\" alt=\"\" />",
            escape_html(&logo)
        )
    } else {
        String::new()
    };

    let albums_html = if releases.albums.is_empty() {
        String::new()
    } else {
        format!("<section class=\"card detail-discography\"><div class=\"detail-section-title\"><h2>Albums</h2></div>{}</section>",render_album_tiles(state, library, &releases.albums))
    };
    let contributions_html = [
        render_artist_contributions(state, library, "Collab albums", &releases.collab_albums),
        render_artist_contributions(state, library, "Appears on", &releases.appears_on),
        render_artist_contributions(state, library, "Other credits", &releases.other_credits),
    ]
    .join("");

    let template =
        load_template(state, "templates/partials/artist_detail.html").unwrap_or_default();
    apply_template(
        template,
        &[
            ("header_style", header_style),
            ("header_class", header_class.to_string()),
            ("logo_img", logo_img),
            ("name", escape_html(&artist.name)),
            ("genres", genres),
            ("back_link", escape_html(&back)),
            ("summary", escape_html(&summary)),
            ("albums_html", albums_html),
            ("contributions_html", contributions_html),
        ],
    )
}

fn render_artist_contributions(
    _state: &AppState,
    library: &Library,
    heading: &str,
    contributions: &[::library::ArtistReleaseContribution],
) -> String {
    if contributions.is_empty() {
        return String::new();
    }
    let mut html = String::new();
    let mut artist_cache = HashMap::new();
    for contribution in contributions {
        let album = &contribution.album;
        let link = format!("/library?album_id={}", url_escape(&album.id));
        let roles = contribution
            .roles
            .iter()
            .map(|role| match role.as_str() {
                "featured_artist" => "Featured artist",
                "track_artist" => "Track artist",
                "composer" => "Composer",
                "conductor" => "Conductor",
                "performer" => "Performer",
                "producer" => "Producer",
                _ => role.as_str(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        let count = contribution.track_ids.len();
        let songs = if count == 1 { "song" } else { "songs" };
        let titles = contribution
            .track_titles
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join(" · ");
        let remaining = if count > 3 {
            format!(" · +{} more", count - 3)
        } else {
            String::new()
        };
        let artist_name = album_artist_name(library, &mut artist_cache, album);
        let cover = render_tile_cover(
            album
                .cover_ref
                .as_ref()
                .map(|_| format!("/covers/albums/{}", url_escape(&album.id)))
                .as_deref(),
        );
        html.push_str(&format!("<a class=\"tile tile-album\" data-library-link href=\"{}\">{}<div class=\"tile-body\"><div class=\"tile-title\">{}</div><div class=\"tile-meta\">{}</div><div class=\"tile-meta\">{} · {} {}</div><p class=\"tile-summary\">{}{}</p></div></a>",escape_html(&link),cover,escape_html(&album.title),escape_html(&artist_name),escape_html(&roles),count,songs,escape_html(&titles),escape_html(&remaining)));
    }
    format!("<section class=\"card detail-discography\"><div class=\"detail-section-title\"><h2>{}</h2></div><div class=\"tile-grid album-grid\">{}</div></section>",escape_html(heading),html)
}

fn render_album_detail(
    state: &AppState,
    library: &Library,
    album_id: &str,
    message: &mut Option<String>,
) -> String {
    let album = match library.get_album(album_id) {
        Ok(Some(album)) => album,
        Ok(None) => {
            return render_empty_state(
                "Album not found",
                "This album may have been removed. Browse the library to find another album.",
            )
        }
        Err(err) => {
            *message = Some(format!("library error: {}", err));
            return render_empty_state(
                "Album could not be loaded",
                "Refresh the page to try again.",
            );
        }
    };

    let mut artist_cache = HashMap::new();
    let artist_name = album_artist_name(library, &mut artist_cache, &album);
    let artist_link = format!("/library?artist_id={}", url_escape(&album.artist_id));
    let artist_credits_html = library
        .album_credits(&album.id)
        .ok()
        .map(|credits| {
            credits
                .credits
                .into_iter()
                .filter(|credit| credit.role == "album_artist")
                .map(|credit| {
                    format!(
                        "<a data-library-link href=\"/library?artist_id={}\">{}</a>{}",
                        url_escape(&credit.artist_id),
                        escape_html(&credit.credited_name),
                        escape_html(&credit.join_phrase)
                    )
                })
                .collect::<String>()
        })
        .filter(|html| !html.is_empty())
        .unwrap_or_else(|| {
            format!(
                "<a data-library-link href=\"{}\">{}</a>",
                escape_html(&artist_link),
                escape_html(&artist_name)
            )
        });
    let cover = if album.cover_ref.is_some() {
        let cover_url = format!("/covers/albums/{}", url_escape(&album.id));
        format!(
            "<img class=\"cover-lg\" src=\"{}\" alt=\"\" />",
            escape_html(&cover_url)
        )
    } else {
        render_artwork_placeholder("cover-lg")
    };
    let year = album
        .year
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_string());
    let genres = format_genres_html(&album.genres);
    let artist_genres = album
        .all_artist_ids()
        .iter()
        .filter_map(|artist_id| library.get_artist(artist_id).ok().flatten())
        .flat_map(|artist| artist.genres)
        .collect::<Vec<_>>();
    let artist_genres = format_genres_html(&artist_genres);
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
        let track_artist = library
            .track_credits(&track.id)
            .ok()
            .map(|credits| credits.display_name)
            .filter(|name| !name.trim().is_empty())
            .map(|name| format!("<div class=\"muted\">{}</div>", escape_html(&name)))
            .unwrap_or_default();
        rows.push_str(&format!(
            "<tr id=\"track-{}\"><td>{}</td><td>{}{}</td><td>{}</td><td>{}</td></tr>",
            escape_html(&track.id),
            escape_html(&pos),
            escape_html(&track.title),
            track_artist,
            escape_html(&duration),
            genres
        ));
    }
    if rows.is_empty() {
        rows.push_str(&format!(
            "<tr><td colspan=\"4\">{}</td></tr>",
            render_empty_state(
                "No tracks found",
                "Scan your library to refresh the tracks in this album.",
            )
        ));
    }

    let track_table = format!(
        "<table><thead><tr><th scope=\"col\">#</th><th scope=\"col\">Track</th><th scope=\"col\">Duration</th><th scope=\"col\">Genres</th></tr></thead><tbody>{}</tbody></table>",
        rows
    );

    let template = load_template(state, "templates/partials/album_detail.html").unwrap_or_default();
    let folder_path = library
        .resolve_relpath(&album.folder_relpath)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| album.folder_relpath.clone());
    apply_template(
        template,
        &[
            ("title", escape_html(&album.title)),
            ("artist_link", escape_html(&artist_link)),
            ("artist_name", escape_html(&artist_name)),
            ("artist_credits_html", artist_credits_html),
            ("cover", cover),
            ("year", escape_html(&year)),
            ("genres", genres),
            ("artist_genres", artist_genres),
            ("folder", escape_html(&folder_path)),
            ("summary", escape_html(&summary)),
            ("track_table", track_table),
        ],
    )
}

fn render_pagination(
    state: &AppState,
    base: &str,
    offset: usize,
    limit: usize,
    total: usize,
) -> String {
    if total == 0 || limit == 0 {
        return String::new();
    }
    let start = offset.saturating_add(1).min(total);
    let end = offset.saturating_add(limit).min(total);
    let total_pages = (total - 1) / limit + 1;
    let current_page = (offset / limit).min(total_pages - 1);
    let controls = render_pagination_controls(base, current_page, limit, total_pages);
    let template = load_template(state, "templates/partials/pagination.html").unwrap_or_default();
    apply_template(
        template,
        &[
            ("start", start.to_string()),
            ("end", end.to_string()),
            ("total", total.to_string()),
            ("controls", controls),
        ],
    )
}

fn render_pagination_controls(
    base: &str,
    current_page: usize,
    limit: usize,
    total_pages: usize,
) -> String {
    let mut controls = render_pagination_step(
        base,
        current_page.checked_sub(1).map(|page| page * limit),
        "prev",
        "Previous page",
        include_str!("../../../../../web/static/icons/lucide/chevron-left.svg"),
    );
    let visible_pages = total_pages.min(9);
    let start_page = current_page
        .saturating_sub(visible_pages / 2)
        .min(total_pages - visible_pages);
    for page in start_page..start_page + visible_pages {
        let page_offset = page * limit;
        let href = format!("{}&offset={}", base, page_offset);
        let class = if page == current_page {
            "page-link active"
        } else {
            "page-link"
        };
        let current = if page == current_page {
            " aria-current=\"page\""
        } else {
            ""
        };
        controls.push_str(&format!(
            "<a class=\"{}\" href=\"{}\" data-library-link aria-label=\"Page {}\"{}>{}</a>",
            class,
            escape_html(&href),
            page + 1,
            current,
            page + 1
        ));
    }
    controls.push_str(&render_pagination_step(
        base,
        current_page
            .checked_add(1)
            .filter(|page| *page < total_pages)
            .map(|page| page * limit),
        "next",
        "Next page",
        include_str!("../../../../../web/static/icons/lucide/chevron-right.svg"),
    ));
    controls
}

fn render_pagination_step(
    base: &str,
    offset: Option<usize>,
    relation: &str,
    label: &str,
    icon: &str,
) -> String {
    let label = escape_html(label);
    if let Some(offset) = offset {
        let href = escape_html(&format!("{base}&offset={offset}"));
        format!(
            r#"<a class="page-link page-step" href="{href}" data-library-link rel="{relation}" aria-label="{label}">{icon}</a>"#
        )
    } else {
        format!(
            r#"<button class="page-link page-step" type="button" disabled aria-label="{label}">{icon}</button>"#
        )
    }
}

fn format_genres_html(genres: &[String]) -> String {
    let genres = normalize_display_genres(genres);
    if genres.is_empty() {
        "<span class=\"muted\">-</span>".to_string()
    } else {
        escape_html(&genres.join(", "))
    }
}

fn normalize_display_genres(genres: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for genre in genres {
        let genre = normalize_genre_separators(genre);
        for part in genre.split(&[';', ',', '/', '|', '\0'][..]) {
            let trimmed = genre_display_label(part);
            if trimmed.is_empty() {
                continue;
            }
            if !out.iter().any(|existing: &String| existing == &trimmed) {
                out.push(trimmed);
            }
        }
    }
    out
}

fn normalize_genre_separators(value: &str) -> String {
    value
        .replace("&bull;", "|")
        .replace("&#8226;", "|")
        .replace("&#x2022;", "|")
        .replace("&middot;", "|")
        .replace("\u{2022}", "|")
        .replace("\u{00B7}", "|")
        .replace("\u{00E2}\u{20AC}\u{00A2}", "|")
        .replace("\u{00E2}\u{0080}\u{00A2}", "|")
        .replace("\u{00C2}\u{20AC}\u{00A2}", "|")
}

fn render_library_snapshot(
    state: &AppState,
    stats: &LibraryStats,
    search: &str,
    filter: SearchFilter,
    search_form: &str,
) -> String {
    let template =
        load_template(state, "templates/partials/library_snapshot.html").unwrap_or_default();
    let tabs_html = render_library_tabs(search, filter);
    apply_template(
        template,
        &[
            ("artists", stats.artists.to_string()),
            ("albums", stats.albums.to_string()),
            ("tracks", stats.tracks.to_string()),
            ("search_form", search_form.to_string()),
            (
                "tabs_hidden",
                if search.is_empty() { "" } else { " hidden" }.to_string(),
            ),
            ("tabs_html", tabs_html),
        ],
    )
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;
    use axum::Json;

    use super::{
        render_library_tabs, render_pagination_controls, render_search_empty_state,
        LibraryPageView, SearchFilter,
    };

    #[test]
    fn genre_display_capitalizes_legacy_labels_and_keeps_html_escaped() {
        let genres = [
            " alternative rock; ALTERNATIVE ROCK • edm ".to_string(),
            "r&b / acid jazz".to_string(),
            "<b>metal</b>".to_string(),
        ];
        assert_eq!(
            super::normalize_display_genres(&genres[..2]),
            ["Alternative Rock", "EDM", "R&B", "Acid Jazz"]
        );
        let html = super::format_genres_html(&genres);
        assert!(html.contains("R&amp;B"));
        assert!(!html.contains("<b>"));
        assert_eq!(
            super::format_genres_html(&[]),
            "<span class=\"muted\">-</span>"
        );
    }

    fn library_view() -> LibraryPageView {
        LibraryPageView {
            content: "<article>Selected album &amp; tracks</article>".to_string(),
            pagination: "<a href=\"/library?offset=24\" data-library-link>Next</a>".to_string(),
            message: "<p>Library ready</p>".to_string(),
            search_block: "<form data-library-search>Search</form>".to_string(),
        }
    }

    #[tokio::test]
    async fn full_library_html_and_json_fragment_share_rendered_content() {
        let response = Json(library_view()).into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let fragment: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(fragment.as_object().unwrap().len(), 4);

        let html = library_view().into_html(
            include_str!("../../../../../web/templates/library.html").to_string(),
            true,
        );
        for key in ["content", "pagination", "message", "search_block"] {
            assert!(
                html.contains(fragment[key].as_str().unwrap()),
                "initial HTML must contain the same {key} as the fragment"
            );
        }
        assert!(!html.contains("class=\"status"));
        assert!(html.contains("data-library-scanning=\"true\""));
        assert!(!html.contains("Loading library..."));
        assert!(!html.contains("{{"));
    }

    #[test]
    fn library_filter_links_keep_queries_and_explicit_navigation_hooks() {
        let html = render_library_tabs("R&B / <live>", SearchFilter::Albums);
        assert_eq!(html.matches("data-library-link").count(), 3);
        assert_eq!(html.matches("tab-chip active").count(), 1);
        assert_eq!(html.matches("aria-current=\"page\"").count(), 1);
        assert!(html.contains("&amp;filter=albums\" data-library-link aria-current=\"page\""));
        for filter in ["artists", "albums", "tracks"] {
            assert!(html.contains(&format!(
                "href=\"/library?search=R%26B%20%2F%20%3Clive%3E&amp;filter={filter}\""
            )));
        }
        assert!(!html.contains("<live>"));
    }

    #[test]
    fn pagination_keeps_up_to_nine_page_numbers_at_the_start_middle_and_end() {
        for (total_pages, current_page, expected) in [
            (1, 0, vec![1]),
            (4, 3, vec![1, 2, 3, 4]),
            (9, 4, vec![1, 2, 3, 4, 5, 6, 7, 8, 9]),
            (20, 0, vec![1, 2, 3, 4, 5, 6, 7, 8, 9]),
            (20, 4, vec![1, 2, 3, 4, 5, 6, 7, 8, 9]),
            (20, 9, vec![6, 7, 8, 9, 10, 11, 12, 13, 14]),
            (20, 18, vec![12, 13, 14, 15, 16, 17, 18, 19, 20]),
            (20, 19, vec![12, 13, 14, 15, 16, 17, 18, 19, 20]),
        ] {
            let html =
                render_pagination_controls("/library?filter=albums", current_page, 24, total_pages);
            let pages: Vec<usize> = html
                .split("aria-label=\"Page ")
                .skip(1)
                .map(|part| part.split('"').next().unwrap().parse().unwrap())
                .collect();
            assert_eq!(
                pages,
                expected,
                "page {} of {total_pages}",
                current_page + 1
            );
            assert_eq!(html.matches("aria-current=\"page\"").count(), 1);
            assert!(html.contains(&format!(
                "aria-label=\"Page {}\" aria-current=\"page\">{}",
                current_page + 1,
                current_page + 1
            )));
        }
    }

    #[test]
    fn pagination_chevrons_stay_visible_and_only_valid_directions_are_links() {
        for (total_pages, current_page, previous, next) in [
            (1, 0, None, None),
            (20, 0, None, Some(24)),
            (20, 9, Some(192), Some(240)),
            (20, 19, Some(432), None),
        ] {
            let html = render_pagination_controls(
                "/library?search=R%26B&filter=tracks",
                current_page,
                24,
                total_pages,
            );
            assert_eq!(html.matches("lucide-chevron-left").count(), 1);
            assert_eq!(html.matches("lucide-chevron-right").count(), 1);
            for (label, relation, offset) in [
                ("Previous page", "prev", previous),
                ("Next page", "next", next),
            ] {
                let tag = html
                    .split('>')
                    .find(|tag| tag.contains(&format!("aria-label=\"{label}\"")))
                    .unwrap();
                if let Some(offset) = offset {
                    assert!(tag.contains("<a "));
                    assert!(tag.contains("data-library-link"));
                    assert!(tag.contains(&format!("rel=\"{relation}\"")));
                    assert!(tag.contains(&format!(
                        "href=\"/library?search=R%26B&amp;filter=tracks&amp;offset={offset}\""
                    )));
                } else {
                    assert!(tag.contains("<button "));
                    assert!(tag.contains(" disabled "));
                    assert!(!tag.contains("href="));
                    assert!(!tag.contains("data-library-link"));
                }
            }
        }
    }

    #[test]
    fn web_search_uses_ranked_matches_across_categories_and_renders_linked_rows() {
        let temp_root = std::env::temp_dir();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = temp_root.join(format!(
            "phonolite-web-search-{}-{nonce}",
            std::process::id()
        ));
        assert!(root.starts_with(&temp_root));
        for file in [
            "Radiohead/OK Computer/Paranoid Android.mp3",
            "Björk/Debut/Human Behaviour.mp3",
        ] {
            let path = root.join("music").join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"not really audio").unwrap();
        }
        let (library, _) = library::Library::load_or_scan(
            vec![library::LibraryRoot::new(String::new(), root.join("music"))],
            root.join("library.redb"),
        )
        .unwrap();
        let template = include_str!("../../../../../web/templates/partials/search_result.html");
        for query in ["Radiohead", "raidohed", "radio head"] {
            let results = crate::library_search::search_library(&library, query, 50).unwrap();
            for kind in ["artist", "album", "track"] {
                assert!(
                    results.iter().any(|result| result.kind == kind),
                    "{query}: {kind}"
                );
            }
            let html = super::render_library_matches(
                &library,
                &results,
                query,
                SearchFilter::Tracks,
                template,
            );
            assert!(html.contains("Search results"));
            for (kind, icon) in [
                ("Artist", "user-round"),
                ("Album", "disc-3"),
                ("Track", "music"),
            ] {
                assert!(html.contains(&format!(
                    "class=\"library-search-result-badge\" role=\"img\" aria-label=\"{kind}\""
                )));
                assert!(html.contains(&format!("class=\"lucide lucide-{icon}\"")));
            }
            assert!(!html.contains("library-search-result-kind"));
            assert_eq!(html.matches("role=\"listitem\"").count(), results.len());
            assert!(html.contains("&amp;filter=tracks&amp;artist_id="));
            assert!(html.contains("&amp;filter=tracks&amp;album_id="));
            assert!(html.contains("#track-"));
            assert!(!html.contains("tile-grid"));
            assert!(!html.contains("pagination-controls"));
        }
        let best = crate::library_search::search_library(&library, "Radiohead", 1).unwrap();
        assert_eq!(best.len(), 1);
        assert_eq!(best[0].title, "Radiohead");
        let empty = super::render_library_matches(
            &library,
            &[],
            "<script>missing</script>",
            SearchFilter::Albums,
            template,
        );
        assert!(empty.contains("No results found"));
        assert!(empty.contains("&lt;script&gt;missing&lt;/script&gt;"));
        assert!(!empty.contains("<script>"));
        let results = [crate::state::SearchResult {
            kind: "artist".to_string(),
            id: "artist&1".to_string(),
            title: "<script>title</script>".to_string(),
            subtitle: Some("<img src=x>".to_string()),
            album_id: None,
            score: 100,
        }];
        let html = super::render_library_matches(
            &library,
            &results,
            "a&b",
            SearchFilter::Artists,
            template,
        );
        assert!(html.contains("&lt;script&gt;title&lt;/script&gt;"));
        assert!(html.contains("&lt;img src=x&gt;"));
        assert!(html.contains("search=a%26b&amp;filter=artists&amp;artist_id=artist%261"));
        assert!(!html.contains("<script>"));
        drop(library);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn empty_search_results_escape_the_query_and_distinguish_an_empty_library() {
        let html = render_search_empty_state("<script>alert(1)</script>", SearchFilter::Tracks);
        assert!(html.contains("No tracks found"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!html.contains("<script>"));

        let html = render_search_empty_state("", SearchFilter::Albums);
        assert!(html.contains("No albums yet"));
        assert!(html.contains("Add a music folder"));
        assert!(!html.contains("No matches"));
    }
}
