use axum::{
    extract::{Path as AxumPath, Query, State},
    http::StatusCode,
    response::Response,
    Extension, Json,
};
use std::sync::Arc;

use crate::artist_artwork::recover_artist_artwork;
use crate::assets::metadata_root_path;
use crate::catalogue_cache::{json_response, CacheError};
use crate::http::json_error;
use crate::library_views::{ArtistSummaryPage, ArtistSummaryView};
use crate::state::{AppState, ArtistQuery, AuthContext, JsonResult, ListResponse};

use super::{
    library_or_json_error,
    views::{
        build_album_views, build_artist_view, build_artist_views, build_track_views, liked_set,
        playlist_set, AlbumView, TrackView,
    },
};

#[derive(Debug, serde::Deserialize)]
pub struct ArtistSummaryQuery {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub revision: Option<String>,
}

/// Additive compact browse contract. Ordering revisions change only when
/// catalogue membership/order changes; metadata freshness is separate.
pub async fn list_artist_summaries(
    State(state): State<AppState>,
    Extension(_ctx): Extension<AuthContext>,
    Query(params): Query<ArtistSummaryQuery>,
    work: Option<Extension<Arc<crate::resources::WorkPermit>>>,
) -> Response {
    let jobs = state.shutdown.clone();
    let work = work.map(|Extension(work)| work);
    let library = match super::library_or_response(&state) {
        Ok(library) => library,
        Err(response) => return response,
    };
    let limit = params.limit.unwrap_or(60);
    if limit == 0
        || limit > 200
        || params
            .revision
            .as_ref()
            .is_some_and(|value| value.len() > 128)
    {
        return crate::http::json_error_response(
            StatusCode::BAD_REQUEST,
            "limit must be between 1 and 200 and revision must be valid",
        );
    }
    let offset = params.offset.unwrap_or(0);
    let generation = library.catalog_generation();
    let metadata = library.metadata_revision();
    let revision = library.browse_revision();
    if params
        .revision
        .as_ref()
        .is_some_and(|value| value != &revision)
    {
        return CacheError::Stale.response();
    }
    let key = format!("summaries:{generation}:{metadata}:{limit}:{offset}");
    let current_library = library.clone();
    let shutdown = state.shutdown.clone();
    let metadata_root = metadata_root_path(&state);
    let bytes = state
        .catalogue_cache
        .get_or_fill(
            key,
            || {
                !shutdown.is_cancelled()
                    && current_library.catalog_generation() == generation
                    && current_library.metadata_revision() == metadata
            },
            || async move {
                jobs.spawn_cancellable_blocking("catalogue_read", move || {
                    let _work = work;
                    let (summaries, total) = library
                        .artist_browse_page(limit, offset)
                        .map_err(|error| error.to_string())?;
                    let items = summaries
                        .into_iter()
                        .map(|summary| {
                            let logo_ref = summary.logo_ref.or_else(|| {
                                crate::artist_artwork::stored_artist_artwork_ref(
                                    &metadata_root,
                                    &summary.id,
                                    crate::artist_artwork::ArtistArtworkKind::Logo,
                                )
                            });
                            ArtistSummaryView {
                                id: summary.id,
                                name: summary.name,
                                genres: summary
                                    .genres
                                    .iter()
                                    .map(|genre| common::genre_display_label(genre))
                                    .collect(),
                                logo_ref,
                                metadata_revision: metadata.to_string(),
                                album_count: summary.album_count,
                                release_count: summary.release_count,
                                collab_album_count: summary.collab_album_count,
                                appearance_count: summary.appearance_count,
                                other_credit_count: summary.other_credit_count,
                            }
                        })
                        .collect::<Vec<_>>();
                    let next = offset.saturating_add(items.len());
                    let page = ArtistSummaryPage {
                        items,
                        total,
                        next_offset: (next < total).then_some(next),
                        browse_revision: revision,
                    };
                    serde_json::to_vec(&page)
                        .map(bytes::Bytes::from)
                        .map_err(|error| error.to_string())
                })
                .await
                .map_err(|error| error.to_string())?
            },
        )
        .await;
    match bytes {
        Ok(bytes) => json_response(bytes),
        // Metadata-only races are retryable work, not an ordering reset.
        Err(CacheError::Stale) if current_library.catalog_generation() == generation => {
            CacheError::Busy.response()
        }
        Err(error) => error.response(),
    }
}

pub async fn list_artists(
    State(state): State<AppState>,
    Extension(_ctx): Extension<AuthContext>,
    Query(params): Query<ArtistQuery>,
    work: Option<Extension<Arc<crate::resources::WorkPermit>>>,
) -> Response {
    let jobs = state.shutdown.clone();
    let work = work.map(|Extension(work)| work);
    let library = match super::library_or_response(&state) {
        Ok(library) => library,
        Err(response) => return response,
    };
    let limit = params.limit.unwrap_or(200).max(1);
    if limit > 1000
        || params
            .search
            .as_ref()
            .is_some_and(|value| value.len() > 512)
    {
        return crate::http::json_error_response(
            StatusCode::BAD_REQUEST,
            "artist page limit is 1000 and search is limited to 512 bytes",
        );
    }
    let offset = params.offset.unwrap_or(0);
    let metadata_root = metadata_root_path(&state);
    let generation = library.catalog_generation();
    let metadata = library.metadata_revision();
    let key = format!(
        "artists:{generation}:{metadata}:{limit}:{offset}:{}:{:?}",
        params.include_contributors, params.search
    );
    let current = library.clone();
    let shutdown = state.shutdown.clone();
    let result = state
        .catalogue_cache
        .get_or_fill(
            key,
            || {
                !shutdown.is_cancelled()
                    && current.catalog_generation() == generation
                    && current.metadata_revision() == metadata
            },
            || async move {
                jobs.spawn_cancellable_blocking("catalogue_read", move || {
                    let _work = work;
                    // Recover only this page's saved artwork. Legacy callers retain their
                    // complete projection; shared serialized payloads avoid repeat work.
                    let (mut artists, total) = library
                        .list_artists_for_browse(
                            params.search.as_deref(),
                            limit,
                            offset,
                            params.include_contributors,
                        )
                        .map_err(|err| format!("library error: {err}"))?;
                    for artist in &mut artists {
                        recover_artist_artwork(&metadata_root, artist);
                    }
                    let items = build_artist_views(&library, artists)?;
                    serde_json::to_vec(&ListResponse { items, total })
                        .map(bytes::Bytes::from)
                        .map_err(|error| error.to_string())
                })
                .await
                .map_err(|error| error.to_string())?
            },
        )
        .await;
    match result {
        Ok(bytes) => json_response(bytes),
        Err(CacheError::Stale) if current.catalog_generation() == generation => {
            CacheError::Busy.response()
        }
        Err(error) => error.response(),
    }
}

pub async fn list_artist_releases(
    State(state): State<AppState>,
    Extension(_ctx): Extension<AuthContext>,
    AxumPath(artist_id): AxumPath<String>,
) -> JsonResult<crate::library_views::ArtistReleasesView> {
    let library = library_or_json_error(&state)?;
    if library
        .get_artist(&artist_id)
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?
        .is_none()
    {
        return Err(json_error(StatusCode::NOT_FOUND, "artist not found"));
    }
    crate::library_views::build_artist_releases_view(&library, &artist_id)
        .map(Json)
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err))
}

pub async fn list_artist_albums(
    State(state): State<AppState>,
    Extension(_ctx): Extension<AuthContext>,
    AxumPath(artist_id): AxumPath<String>,
) -> JsonResult<Vec<AlbumView>> {
    let library = library_or_json_error(&state)?;
    let _artist = match library.get_artist(&artist_id) {
        Ok(Some(artist)) => artist,
        Ok(None) => {
            return Err(json_error(
                StatusCode::NOT_FOUND,
                "artist not found".to_string(),
            ))
        }
        Err(err) => {
            return Err(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("library error: {}", err),
            ))
        }
    };
    let albums = match library.list_artist_albums(&artist_id) {
        Ok(albums) => albums,
        Err(err) => {
            return Err(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("library error: {}", err),
            ))
        }
    };

    let items = build_album_views(&library, albums)
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    Ok(Json(items))
}

pub async fn get_artist(
    State(state): State<AppState>,
    Extension(_ctx): Extension<AuthContext>,
    AxumPath(artist_id): AxumPath<String>,
    work: Option<Extension<Arc<crate::resources::WorkPermit>>>,
) -> Response {
    let jobs = state.shutdown.clone();
    let work = work.map(|Extension(work)| work);
    let library = match super::library_or_response(&state) {
        Ok(library) => library,
        Err(response) => return response,
    };
    let generation = library.catalog_generation();
    let metadata = library.metadata_revision();
    let key = format!("artist:{generation}:{metadata}:{artist_id}");
    // Eligible lookup scheduling is independent of response-cache lifetime.
    // The small source read remains; expensive release projection is shared.
    let source_library = library.clone();
    let source_state = state.clone();
    let source_work = work.clone();
    let source = jobs
        .spawn_cancellable_blocking(
            "catalogue_read",
            move || -> Result<Option<common::Artist>, String> {
                let _work = source_work;
                match source_library
                    .get_artist(&artist_id)
                    .map_err(|error| error.to_string())?
                {
                    Some(mut artist) => {
                        let metadata_root = metadata_root_path(&source_state);
                        recover_artist_artwork(&metadata_root, &mut artist);
                        crate::scan::schedule_artist_enrichment(
                            &source_state,
                            &source_library,
                            &artist,
                        );
                        Ok(Some(artist))
                    }
                    None => Ok(None),
                }
            },
        )
        .await;
    let artist = match source {
        Ok(Ok(Some(artist))) => artist,
        Ok(Ok(None)) => {
            return crate::http::json_error_response(StatusCode::NOT_FOUND, "artist not found")
        }
        Ok(Err(error)) => {
            return crate::http::json_error_response(StatusCode::INTERNAL_SERVER_ERROR, error)
        }
        Err(error) => {
            return crate::http::json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                error.to_string(),
            )
        }
    };
    let current = library.clone();
    let shutdown = state.shutdown.clone();
    let result = state
        .catalogue_cache
        .get_or_fill(
            key,
            || {
                !shutdown.is_cancelled()
                    && current.catalog_generation() == generation
                    && current.metadata_revision() == metadata
            },
            || async move {
                jobs.spawn_cancellable_blocking("catalogue_read", move || {
                    let _work = work;
                    let view = build_artist_view(&library, artist)?;
                    serde_json::to_vec(&view)
                        .map(bytes::Bytes::from)
                        .map_err(|error| error.to_string())
                })
                .await
                .map_err(|error| error.to_string())?
            },
        )
        .await;
    match result {
        Ok(bytes) => json_response(bytes),
        Err(CacheError::Stale) if current.catalog_generation() == generation => {
            CacheError::Busy.response()
        }
        Err(error) => error.response(),
    }
}

pub async fn list_album_tracks(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    AxumPath(album_id): AxumPath<String>,
) -> JsonResult<Vec<TrackView>> {
    let library = library_or_json_error(&state)?;
    let mut tracks = match library.get_album_tracks(&album_id) {
        Ok(tracks) => tracks,
        Err(err) => {
            return Err(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("library error: {}", err),
            ))
        }
    };
    if tracks.is_empty() {
        return Err(json_error(
            StatusCode::NOT_FOUND,
            "album not found".to_string(),
        ));
    }
    tracks.sort_by(|a, b| {
        a.disc_no
            .unwrap_or(0)
            .cmp(&b.disc_no.unwrap_or(0))
            .then_with(|| a.track_no.unwrap_or(0).cmp(&b.track_no.unwrap_or(0)))
            .then_with(|| {
                a.title
                    .to_ascii_lowercase()
                    .cmp(&b.title.to_ascii_lowercase())
            })
    });

    let liked_set = liked_set(&state, &ctx.user.id)?;
    let playlist_set = playlist_set(&state, &ctx.user.id)?;

    let items = build_track_views(&library, &tracks, &liked_set, &playlist_set)
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    Ok(Json(items))
}

pub async fn get_track(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    AxumPath(track_id): AxumPath<String>,
) -> JsonResult<TrackView> {
    let library = library_or_json_error(&state)?;
    let track = match library.get_track(&track_id) {
        Ok(Some(track)) => track,
        Ok(None) => {
            return Err(json_error(
                StatusCode::NOT_FOUND,
                "track not found".to_string(),
            ))
        }
        Err(err) => {
            return Err(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("library error: {}", err),
            ))
        }
    };
    let liked_set = liked_set(&state, &ctx.user.id)?;
    let playlist_set = playlist_set(&state, &ctx.user.id)?;
    let mut items = build_track_views(
        &library,
        std::slice::from_ref(&track),
        &liked_set,
        &playlist_set,
    )
    .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    let view = items
        .pop()
        .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "track not found".to_string()))?;
    Ok(Json(view))
}

pub async fn list_playlist_tracks(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    AxumPath(playlist_id): AxumPath<String>,
) -> JsonResult<Vec<TrackView>> {
    let playlist = state
        .user_data
        .get_playlist(&ctx.user.id, &playlist_id)
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{:?}", err)))?;
    let Some(playlist) = playlist else {
        return Err(json_error(
            StatusCode::NOT_FOUND,
            "playlist not found".to_string(),
        ));
    };
    let library = library_or_json_error(&state)?;
    let liked_set = liked_set(&state, &ctx.user.id)?;
    let playlist_set = playlist_set(&state, &ctx.user.id)?;
    let tracks = library.tracks_by_ids_best_effort(&playlist.track_ids);
    let items = build_track_views(&library, &tracks, &liked_set, &playlist_set)
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    Ok(Json(items))
}

pub async fn list_liked_tracks(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> JsonResult<Vec<TrackView>> {
    let track_ids = state
        .user_data
        .list_likes(&ctx.user.id)
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{:?}", err)))?;
    let library = library_or_json_error(&state)?;
    let liked_set = liked_set(&state, &ctx.user.id)?;
    let playlist_set = playlist_set(&state, &ctx.user.id)?;
    let tracks = library.tracks_by_ids_best_effort(&track_ids);
    let items = build_track_views(&library, &tracks, &liked_set, &playlist_set)
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    Ok(Json(items))
}
