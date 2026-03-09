use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use reqwest::Client;
use tracing::{info, warn};

use crate::activity_store::ActivityStore;
use crate::assets::{
    clear_metadata_assets, image_ext_from_mime, image_ext_from_url, metadata_root_path,
    resolve_cover_source, warm_cover_cache, CoverCacheKey,
};
use crate::config::{resolve_music_roots, resolve_path, MusicRootConfig, ServerConfig};
use crate::external::{self, ExternalConfig, ExternalSource, Provider};
use crate::musicbrainz_album_artists::{MusicBrainzAlbumArtistResolver, ResolveOptions};
use crate::state::{AppState, LibraryStatus};
use crate::watch::configure_watcher;
use common::{Album, Artist};
use library::{Library, LibraryRoot, LibraryStats};

pub fn start_index(state: AppState, roots: Vec<LibraryRoot>, force_rescan: bool) {
    {
        let mut guard = state.library_state.write();
        guard.library = None;
        guard.status = LibraryStatus::Scanning {
            started: SystemTime::now(),
        };
    }
    *state.watcher.write() = None;

    tokio::spawn(async move {
        let _ = state.activity.add_activity("Library rescan started.");
        if force_rescan {
            if let Err(e) = clear_metadata_assets(&state).await {
                warn!("Failed to clear metadata: {}", e);
            }
        }

        let db = Arc::clone(&state.db);
        let roots_clone = roots.clone();
        let result = tokio::task::spawn_blocking(move || {
            let (library, mut scanned) = Library::load_or_scan_with_db(roots_clone, db)?;
            let stats = if force_rescan {
                scanned = true;
                library.rescan()?
            } else {
                library.stats()?
            };
            Ok::<(Library, LibraryStats, bool), library::LibraryError>((library, stats, scanned))
        })
        .await;

        match result {
            Ok(Ok((library, stats, scanned))) => {
                {
                    let mut guard = state.library_state.write();
                    guard.library = Some(library.clone());
                    guard.status = LibraryStatus::Ready(stats.clone());
                }
                info!(
                    "Library ready: {} artists, {} albums, {} tracks",
                    stats.artists, stats.albums, stats.tracks
                );
                let _ = state.activity.add_activity(format!(
                    "Library scan finished: {} artists, {} albums, {} tracks.",
                    stats.artists, stats.albums, stats.tracks
                ));
                configure_watcher(&state, &library);
                if scanned {
                    start_enrichment_sweep(state.clone(), library.clone(), true);
                } else {
                    info!("External metadata sweep skipped (no new scan)");
                }
                start_cover_sweep(state.clone(), library);
            }
            Ok(Err(err)) => {
                let message = err.to_string();
                {
                    let mut guard = state.library_state.write();
                    guard.library = None;
                    guard.status = LibraryStatus::Error(message.clone());
                }
                warn!("Library scan failed: {}", message);
                let _ = state
                    .activity
                    .add_event("ERROR", format!("Library scan failed: {}", message));
            }
            Err(err) => {
                let message = err.to_string();
                {
                    let mut guard = state.library_state.write();
                    guard.library = None;
                    guard.status = LibraryStatus::Error(message.clone());
                }
                warn!("Library scan join error: {}", message);
                let _ = state
                    .activity
                    .add_event("ERROR", format!("Library scan failed: {}", message));
            }
        }
    });
}

pub fn start_rescan(state: AppState, library: Library, replace_complete: bool) {
    {
        let mut guard = state.library_state.write();
        guard.status = LibraryStatus::Scanning {
            started: SystemTime::now(),
        };
    }
    let library_clone = library.clone();
    tokio::spawn(async move {
        let _ = state.activity.add_activity("Library scan started.");
        if replace_complete {
            if let Err(e) = clear_metadata_assets(&state).await {
                warn!("Failed to clear metadata: {}", e);
            }
        }
        let result = tokio::task::spawn_blocking(move || library.rescan()).await;
        match result {
            Ok(Ok(stats)) => {
                let mut guard = state.library_state.write();
                guard.status = LibraryStatus::Ready(stats.clone());
                info!(
                    "Library rescan complete: {} artists, {} albums, {} tracks",
                    stats.artists, stats.albums, stats.tracks
                );
                let _ = state.activity.add_activity(format!(
                    "Library scan finished: {} artists, {} albums, {} tracks.",
                    stats.artists, stats.albums, stats.tracks
                ));
                start_enrichment_sweep(state.clone(), library_clone.clone(), replace_complete);
                start_cover_sweep(state.clone(), library_clone);
            }
            Ok(Err(err)) => {
                let message = err.to_string();
                let mut guard = state.library_state.write();
                guard.status = LibraryStatus::Error(message.clone());
                warn!("Library rescan failed: {}", message);
                let _ = state
                    .activity
                    .add_event("ERROR", format!("Library scan failed: {}", message));
            }
            Err(err) => {
                let message = err.to_string();
                let mut guard = state.library_state.write();
                guard.status = LibraryStatus::Error(message.clone());
                warn!("Library rescan join error: {}", message);
                let _ = state
                    .activity
                    .add_event("ERROR", format!("Library scan failed: {}", message));
            }
        }
    });
}

pub fn set_library_missing(state: &AppState, path: PathBuf) {
    let mut guard = state.library_state.write();
    guard.library = None;
    guard.status = LibraryStatus::Missing(path);
    *state.watcher.write() = None;
}

pub fn set_library_unconfigured(state: &AppState) {
    let mut guard = state.library_state.write();
    guard.library = None;
    guard.status = LibraryStatus::Unconfigured;
    *state.watcher.write() = None;
}

pub fn apply_music_roots_update(state: AppState, roots: &[MusicRootConfig], force: bool) -> String {
    let resolved = resolve_music_roots(&state.config_path, roots);
    if resolved.is_empty() {
        set_library_unconfigured(&state);
        return "Music directory not configured.".to_string();
    }
    if let Some(missing) = resolved.iter().find(|root| !root.path.exists()) {
        set_library_missing(&state, missing.path.clone());
        return format!("Music directory not found: {}", missing.path.display());
    }
    start_index(state, resolved, force);
    "Scanning started.".to_string()
}

pub fn start_cover_sweep(state: AppState, library: Library) {
    tokio::spawn(async move {
        run_cover_sweep(state, library).await;
    });
}

async fn run_cover_sweep(state: AppState, library: Library) {
    let page_size = 50;
    let mut offset = 0;
    let mut count = 0;
    loop {
        let (albums, total) = match library.list_albums(None, page_size, offset) {
            Ok(res) => res,
            Err(e) => {
                warn!("Cover sweep failed to list albums: {}", e);
                break;
            }
        };

        if albums.is_empty() {
            break;
        }

        for album in albums {
            if let Some(cover_ref) = album.cover_ref {
                if let Ok(Some(source)) = resolve_cover_source(&library, &cover_ref) {
                    let key = CoverCacheKey::Album(album.id);
                    if let Err(e) = warm_cover_cache(&state, &key, source).await {
                        warn!("Failed to warm cover cache: {}", e);
                    } else {
                        count += 1;
                    }
                }
            }
        }

        if offset + page_size >= total {
            break;
        }
        offset += page_size;
    }
    if count > 0 {
        info!("Cover sweep completed: {} covers processed", count);
    }
}

pub fn start_enrichment_sweep(state: AppState, library: Library, replace_complete: bool) {
    let config = state.config.read().clone();
    let fetch_config = match external_config_from_settings(&config) {
        Some(config) => config,
        None => {
            info!("External metadata sweep skipped (no enabled sources)");
            return;
        }
    };
    let max_items = config.external_metadata_scan_limit;
    if max_items == 0 {
        info!("External metadata sweep skipped (scan_limit=0)");
        return;
    }
    let min_interval = Duration::from_secs(config.external_metadata_min_interval_secs.max(60));
    info!(
        "External metadata sweep starting: sources={}, max_items={}, min_interval_secs={}, replace_complete={}",
        fetch_config.sources.len(),
        max_items,
        min_interval.as_secs(),
        replace_complete
    );
    let client = state.external_client.clone();
    let tag_error_first = config.external_metadata_on_tag_error;
    let metadata_root = resolve_path(&state.config_path, &config.metadata_path);
    let activity = state.activity.clone();
    let _ = activity.add_activity("Metadata scan started.");
    tokio::spawn(async move {
        run_enrichment_sweep(
            library,
            client,
            fetch_config,
            min_interval,
            max_items,
            tag_error_first,
            metadata_root,
            replace_complete,
            activity,
        )
        .await;
    });
}

async fn run_enrichment_sweep(
    library: Library,
    client: Client,
    config: ExternalConfig,
    min_interval: Duration,
    max_items: usize,
    tag_error_first: bool,
    metadata_root: PathBuf,
    replace_complete: bool,
    activity: ActivityStore,
) {
    let mut remaining = max_items;
    let mut artist_updates = 0usize;
    let mut album_updates = 0usize;
    let mut album_artist_updates = 0usize;
    if remaining == 0 {
        return;
    }
    let album_artist_limit = if replace_complete {
        usize::MAX
    } else {
        remaining
    };
    let album_artist_remaining = run_musicbrainz_album_artist_sweep(
        &library,
        &client,
        &config,
        min_interval,
        album_artist_limit,
        replace_complete,
        &activity,
        &mut album_artist_updates,
    )
    .await;
    if !replace_complete {
        remaining = album_artist_remaining;
    }
    if remaining == 0 {
        let summary = format!(
            "Metadata scan finished. Updated {} artists, {} albums, and {} collaborative album links.",
            artist_updates, album_updates, album_artist_updates
        );
        let _ = activity.add_activity(summary);
        return;
    }
    if tag_error_first {
        remaining = run_tag_error_enrichment(
            &library,
            &client,
            &config,
            min_interval,
            &metadata_root,
            remaining,
            replace_complete,
            &activity,
            &mut artist_updates,
            &mut album_updates,
        )
        .await;
        if remaining == 0 {
            let summary = format!(
                "Metadata scan finished. Updated {} artists, {} albums, and {} collaborative album links.",
                artist_updates, album_updates, album_artist_updates
            );
            let _ = activity.add_activity(summary);
            return;
        }
    }
    let page_size = 100;
    let mut offset = 0usize;
    loop {
        let (items, total) = match library.list_artists(None, page_size, offset) {
            Ok(result) => result,
            Err(err) => {
                warn!("External artist sweep failed: {}", err);
                break;
            }
        };
        for artist in items {
            if remaining == 0 {
                break;
            }
            let result = fetch_artist_enrichment(
                &library,
                &client,
                &config,
                min_interval,
                &metadata_root,
                &artist,
                replace_complete,
                Some(&activity),
            )
            .await;
            if result.attempted {
                remaining = remaining.saturating_sub(1);
            }
            if result.updated {
                artist_updates = artist_updates.saturating_add(1);
            }
        }
        if remaining == 0 || offset + page_size >= total {
            break;
        }
        offset += page_size;
    }

    if remaining == 0 {
        let summary = format!(
            "Metadata scan finished. Updated {} artists, {} albums, and {} collaborative album links.",
            artist_updates, album_updates, album_artist_updates
        );
        let _ = activity.add_activity(summary);
        return;
    }
    let mut offset = 0usize;
    loop {
        let (items, total) = match library.list_albums(None, page_size, offset) {
            Ok(result) => result,
            Err(err) => {
                warn!("External album sweep failed: {}", err);
                break;
            }
        };
        for album in items {
            if remaining == 0 {
                break;
            }
            let artist_name = library
                .get_artist(&album.artist_id)
                .ok()
                .and_then(|artist| artist.map(|value| value.name))
                .unwrap_or_else(|| "Unknown Artist".to_string());
            let result = fetch_album_enrichment(
                &library,
                &client,
                &config,
                min_interval,
                &album,
                &artist_name,
                replace_complete,
                Some(&activity),
            )
            .await;
            if result.attempted {
                remaining = remaining.saturating_sub(1);
            }
            if result.updated {
                album_updates = album_updates.saturating_add(1);
            }
        }
        if remaining == 0 || offset + page_size >= total {
            break;
        }
        offset += page_size;
    }

    let summary = format!(
        "Metadata scan finished. Updated {} artists, {} albums, and {} collaborative album links.",
        artist_updates, album_updates, album_artist_updates
    );
    let _ = activity.add_activity(summary);
}

async fn run_musicbrainz_album_artist_sweep(
    library: &Library,
    client: &Client,
    config: &ExternalConfig,
    min_interval: Duration,
    mut remaining: usize,
    replace_complete: bool,
    activity: &ActivityStore,
    album_artist_updates: &mut usize,
) -> usize {
    let Some(source) = musicbrainz_source(config) else {
        return remaining;
    };

    let resolver = MusicBrainzAlbumArtistResolver::new(
        client,
        source.user_agent.as_deref().unwrap_or(""),
        source.timeout,
        ResolveOptions::default(),
    );
    let mut known_artists = match load_all_artists(library) {
        Ok(items) => items,
        Err(err) => {
            warn!(
                "MusicBrainz album-artist sweep failed to load artists: {}",
                err
            );
            return remaining;
        }
    };

    let page_size = 100usize;
    let mut offset = 0usize;
    let mut seen_albums = 0usize;
    let mut skipped_albums = 0usize;
    let mut matched_albums = 0usize;
    let mut unmatched_albums = 0usize;
    let mut failed_albums = 0usize;
    let mut updated_albums = 0usize;
    let mut total_albums = None;
    loop {
        let (albums, total) = match library.list_albums(None, page_size, offset) {
            Ok(result) => result,
            Err(err) => {
                warn!("MusicBrainz album-artist sweep failed: {}", err);
                return remaining;
            }
        };
        if total_albums.is_none() {
            total_albums = Some(total);
            info!(
                "MusicBrainz album-artist sweep starting: total_albums={}, replace_complete={}, limit={}",
                total,
                replace_complete,
                if remaining == usize::MAX {
                    "unlimited".to_string()
                } else {
                    remaining.to_string()
                }
            );
        }

        for album in albums {
            if remaining == 0 {
                break;
            }
            seen_albums = seen_albums.saturating_add(1);
            let total = total_albums.unwrap_or(seen_albums);
            let progress = format!("[{}/{}]", seen_albums, total);
            let artist_name = album.artist_display_name();
            let artist_name = if artist_name.trim().is_empty() {
                "Unknown Artist"
            } else {
                artist_name.as_str()
            };

            let key = external_attempt_key("album_artists", &album.id);
            if !replace_complete {
                match library.should_attempt_external(&key, min_interval) {
                    Ok(true) => {}
                    Ok(false) => {
                        skipped_albums = skipped_albums.saturating_add(1);
                        info!(
                            "MusicBrainz album-artist sweep {} skipping '{}' (artist='{}', min interval not reached)",
                            progress,
                            album.title,
                            artist_name
                        );
                        continue;
                    }
                    Err(err) => {
                        failed_albums = failed_albums.saturating_add(1);
                        warn!("MusicBrainz album-artist check failed: {}", err);
                        continue;
                    }
                }
            }

            let tracks = match library.get_album_tracks(&album.id) {
                Ok(tracks) if !tracks.is_empty() => tracks,
                Ok(_) => continue,
                Err(err) => {
                    failed_albums = failed_albums.saturating_add(1);
                    warn!(
                        "MusicBrainz album-artist sweep {} failed to load tracks for '{}' (artist='{}'): {}",
                        progress,
                        album.title,
                        artist_name,
                        err
                    );
                    continue;
                }
            };

            info!(
                "MusicBrainz album-artist sweep {} resolving '{}' (artist='{}', tracks={})",
                progress,
                album.title,
                artist_name,
                tracks.len()
            );
            let _ = library.record_external_attempt(&key, false);
            match resolver
                .resolve_album_artists(library, &album, &tracks, &known_artists)
                .await
            {
                Ok(Some(resolved_artists)) => {
                    matched_albums = matched_albums.saturating_add(1);
                    match library.merge_album_artists(&album.id, &resolved_artists) {
                        Ok(updated) => {
                            remaining = remaining.saturating_sub(1);
                            let _ = library.record_external_attempt(&key, true);
                            let associated_artists = resolved_artists
                                .iter()
                                .map(|artist| artist.name.clone())
                                .collect::<Vec<_>>();
                            let artist_names = associated_artists.join(", ");
                            if updated {
                                *album_artist_updates = album_artist_updates.saturating_add(1);
                                updated_albums = updated_albums.saturating_add(1);
                                merge_known_artists(&mut known_artists, &resolved_artists);
                                let _ = activity.add_event(
                                    "metadata",
                                    format!(
                                        "Resolved album artists for '{}' to {}.",
                                        album.title, artist_names
                                    ),
                                );
                            }
                            info!(
                                "MusicBrainz album-artist sweep {} resolved '{}' associated_artists={:?} (updated={})",
                                progress,
                                album.title,
                                associated_artists,
                                updated
                            );
                        }
                        Err(err) => {
                            failed_albums = failed_albums.saturating_add(1);
                            warn!(
                                "MusicBrainz album-artist sweep {} merge failed for '{}' (artist='{}'): {}",
                                progress,
                                album.title,
                                artist_name,
                                err
                            );
                        }
                    }
                }
                Ok(None) => {
                    unmatched_albums = unmatched_albums.saturating_add(1);
                    remaining = remaining.saturating_sub(1);
                    let _ = library.record_external_attempt(&key, false);
                    info!(
                        "MusicBrainz album-artist sweep {} found no usable match for '{}' (artist='{}')",
                        progress,
                        album.title,
                        artist_name
                    );
                }
                Err(err) => {
                    failed_albums = failed_albums.saturating_add(1);
                    warn!(
                        "MusicBrainz album-artist sweep {} resolution failed for '{}' (artist='{}'): {}",
                        progress,
                        album.title,
                        artist_name,
                        err
                    );
                }
            }
        }

        if remaining == 0 || offset + page_size >= total {
            break;
        }
        offset += page_size;
    }

    info!(
        "MusicBrainz album-artist sweep finished: seen={}, skipped={}, matched={}, unmatched={}, failed={}, updated={}",
        seen_albums,
        skipped_albums,
        matched_albums,
        unmatched_albums,
        failed_albums,
        updated_albums
    );

    remaining
}

async fn run_tag_error_enrichment(
    library: &Library,
    client: &Client,
    config: &ExternalConfig,
    min_interval: Duration,
    metadata_root: &Path,
    mut remaining: usize,
    replace_complete: bool,
    activity: &ActivityStore,
    artist_updates: &mut usize,
    album_updates: &mut usize,
) -> usize {
    let page_size = 100;
    let mut offset = 0usize;
    loop {
        let (items, total) = match library.list_tag_errors(page_size, offset) {
            Ok(result) => result,
            Err(err) => {
                warn!("External tag error sweep failed: {}", err);
                return remaining;
            }
        };
        for info in items {
            if remaining == 0 {
                break;
            }
            if let Ok(Some(artist)) = library.get_artist(&info.artist_id) {
                let result = fetch_artist_enrichment(
                    library,
                    client,
                    config,
                    min_interval,
                    metadata_root,
                    &artist,
                    replace_complete,
                    Some(activity),
                )
                .await;
                if result.attempted {
                    remaining = remaining.saturating_sub(1);
                }
                if result.updated {
                    *artist_updates = artist_updates.saturating_add(1);
                }
            }
            if remaining == 0 {
                break;
            }
            if let Ok(Some(album)) = library.get_album(&info.album_id) {
                let artist_name = if !info.artist_name.trim().is_empty() {
                    info.artist_name.clone()
                } else {
                    library
                        .get_artist(&info.artist_id)
                        .ok()
                        .and_then(|artist| artist.map(|value| value.name))
                        .unwrap_or_else(|| "Unknown Artist".to_string())
                };
                let result = fetch_album_enrichment(
                    library,
                    client,
                    config,
                    min_interval,
                    &album,
                    &artist_name,
                    replace_complete,
                    Some(activity),
                )
                .await;
                if result.attempted {
                    remaining = remaining.saturating_sub(1);
                }
                if result.updated {
                    *album_updates = album_updates.saturating_add(1);
                }
            }
        }
        if remaining == 0 || offset + page_size >= total {
            break;
        }
        offset += page_size;
    }
    remaining
}

fn musicbrainz_source(config: &ExternalConfig) -> Option<ExternalSource> {
    config
        .sources
        .iter()
        .find(|source| matches!(source.provider, Provider::MusicBrainz))
        .cloned()
}

fn load_all_artists(library: &Library) -> Result<Vec<Artist>, library::LibraryError> {
    let mut items = Vec::new();
    let mut offset = 0usize;
    let limit = 200usize;
    loop {
        let (mut batch, total) = library.list_artists(None, limit, offset)?;
        items.append(&mut batch);
        if items.len() >= total {
            break;
        }
        offset = items.len();
    }
    Ok(items)
}

fn merge_known_artists(target: &mut Vec<Artist>, incoming: &[Artist]) {
    for artist in incoming {
        if target.iter().any(|existing| existing.id == artist.id) {
            continue;
        }
        target.push(artist.clone());
    }
}

#[derive(Clone, Copy)]
pub struct FetchResult {
    pub attempted: bool,
    pub updated: bool,
}

impl FetchResult {
    fn skipped() -> Self {
        Self {
            attempted: false,
            updated: false,
        }
    }

    fn attempted(updated: bool) -> Self {
        Self {
            attempted: true,
            updated,
        }
    }
}

pub async fn fetch_artist_enrichment(
    library: &Library,
    client: &Client,
    config: &ExternalConfig,
    min_interval: Duration,
    metadata_root: &Path,
    artist: &Artist,
    replace: bool,
    activity: Option<&ActivityStore>,
) -> FetchResult {
    if !replace && !needs_artist_enrichment(artist) {
        info!(
            "External metadata: skipping artist '{}' (already enriched)",
            artist.name
        );
        return FetchResult::skipped();
    }
    let key = external_attempt_key("artist", &artist.id);
    if !replace {
        match library.should_attempt_external(&key, min_interval) {
            Ok(true) => {}
            Ok(false) => {
                info!(
                    "External metadata: skipping artist '{}' (min interval not reached)",
                    artist.name
                );
                return FetchResult::skipped();
            }
            Err(err) => {
                warn!("External metadata check failed: {}", err);
                return FetchResult::skipped();
            }
        }
    }
    let _ = library.record_external_attempt(&key, false);
    info!("Fetching external artist metadata for '{}'", artist.name);
    match external::fetch_artist(client, config, &artist.name).await {
        Ok(Some(metadata)) => {
            let summary = metadata
                .summary
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or("<none>");
            info!(
                "External artist metadata received for '{}': summary='{}', genres={:?}, logo_url={}, banner_url={}",
                artist.name,
                summary,
                metadata.genres,
                metadata.logo_url.as_deref().unwrap_or("<none>"),
                metadata.banner_url.as_deref().unwrap_or("<none>")
            );
            let (logo_ref, banner_ref) =
                store_artist_assets(metadata_root, client, config, &artist.id, &metadata).await;
            let _ = library.update_artist_enrichment(
                &artist.id,
                metadata.summary,
                &metadata.genres,
                logo_ref,
                banner_ref,
                replace,
            );
            if let Some(activity) = activity {
                let _ = activity.add_event(
                    "metadata",
                    format!("Fetched metadata for artist '{}'.", artist.name),
                );
            }
            let _ = library.record_external_attempt(&key, true);
            FetchResult::attempted(true)
        }
        Ok(None) => {
            info!("External artist metadata not found for '{}'", artist.name);
            let _ = library.record_external_attempt(&key, false);
            FetchResult::attempted(false)
        }
        Err(err) => {
            warn!("External artist fetch failed for {}: {}", artist.name, err);
            FetchResult::skipped()
        }
    }
}

pub async fn fetch_album_enrichment(
    library: &Library,
    client: &Client,
    config: &ExternalConfig,
    min_interval: Duration,
    album: &Album,
    artist_name: &str,
    replace: bool,
    activity: Option<&ActivityStore>,
) -> FetchResult {
    if !replace && !needs_album_enrichment(album) {
        info!(
            "External metadata: skipping album '{}' (already enriched)",
            album.title
        );
        return FetchResult::skipped();
    }
    let key = external_attempt_key("album", &album.id);
    if !replace {
        match library.should_attempt_external(&key, min_interval) {
            Ok(true) => {}
            Ok(false) => {
                info!(
                    "External metadata: skipping album '{}' (min interval not reached)",
                    album.title
                );
                return FetchResult::skipped();
            }
            Err(err) => {
                warn!("External metadata check failed: {}", err);
                return FetchResult::skipped();
            }
        }
    }
    let _ = library.record_external_attempt(&key, false);
    info!(
        "Fetching external album metadata for '{}' - '{}'",
        artist_name, album.title
    );
    match external::fetch_album(client, config, artist_name, &album.title).await {
        Ok(Some(metadata)) => {
            let summary = metadata
                .summary
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or("<none>");
            info!(
                "External album metadata received for '{} - {}': summary='{}', genres={:?}",
                artist_name, album.title, summary, metadata.genres
            );
            let _ = library.update_album_enrichment(&album.id, metadata.summary, &metadata.genres);
            if let Some(activity) = activity {
                let _ = activity.add_event(
                    "metadata",
                    format!(
                        "Fetched metadata for album '{}' ({artist}).",
                        album.title,
                        artist = artist_name
                    ),
                );
            }
            let _ = library.record_external_attempt(&key, true);
            FetchResult::attempted(true)
        }
        Ok(None) => {
            info!(
                "External album metadata not found for '{} - {}'",
                artist_name, album.title
            );
            let _ = library.record_external_attempt(&key, false);
            FetchResult::attempted(false)
        }
        Err(err) => {
            warn!(
                "External album fetch failed for {} - {}: {}",
                artist_name, album.title, err
            );
            FetchResult::skipped()
        }
    }
}

pub fn schedule_artist_enrichment(state: &AppState, library: &Library, artist: &Artist) {
    if !needs_artist_enrichment(artist) {
        return;
    }
    let config = match external_config(state) {
        Some(config) => config,
        None => return,
    };
    let min_interval = external_min_interval(state);
    let artist = artist.clone();
    let library = library.clone();
    let client = state.external_client.clone();
    let metadata_root = metadata_root_path(state);
    let activity = state.activity.clone();
    tokio::spawn(async move {
        let _ = fetch_artist_enrichment(
            &library,
            &client,
            &config,
            min_interval,
            &metadata_root,
            &artist,
            false,
            Some(&activity),
        )
        .await;
    });
}

pub fn schedule_album_enrichment(
    state: &AppState,
    library: &Library,
    album: &Album,
    artist_name: &str,
) {
    if !needs_album_enrichment(album) {
        return;
    }
    let config = match external_config(state) {
        Some(config) => config,
        None => return,
    };
    let min_interval = external_min_interval(state);
    let album = album.clone();
    let artist_name = artist_name.to_string();
    let library = library.clone();
    let client = state.external_client.clone();
    let activity = state.activity.clone();
    tokio::spawn(async move {
        let _ = fetch_album_enrichment(
            &library,
            &client,
            &config,
            min_interval,
            &album,
            &artist_name,
            false,
            Some(&activity),
        )
        .await;
    });
}

fn needs_artist_enrichment(artist: &Artist) -> bool {
    artist
        .summary
        .as_ref()
        .map(|value| value.trim().is_empty())
        .unwrap_or(true)
        || artist.genres.is_empty()
        || artist.logo_ref.is_none()
        || artist.banner_ref.is_none()
}

fn needs_album_enrichment(album: &Album) -> bool {
    album
        .summary
        .as_ref()
        .map(|value| value.trim().is_empty())
        .unwrap_or(true)
        || album.genres.is_empty()
}

async fn store_artist_assets(
    metadata_root: &Path,
    client: &Client,
    config: &ExternalConfig,
    artist_id: &str,
    metadata: &external::ExternalMetadata,
) -> (Option<String>, Option<String>) {
    let mut logo_ref = None;
    let mut banner_ref = None;

    if let Some(url) = metadata.logo_url.as_deref() {
        logo_ref =
            fetch_and_store_asset(metadata_root, client, config, artist_id, "logo", url).await;
    }
    if let Some(url) = metadata.banner_url.as_deref() {
        banner_ref =
            fetch_and_store_asset(metadata_root, client, config, artist_id, "banner", url).await;
    }

    (logo_ref, banner_ref)
}

async fn fetch_and_store_asset(
    metadata_root: &Path,
    client: &Client,
    config: &ExternalConfig,
    artist_id: &str,
    label: &str,
    url: &str,
) -> Option<String> {
    let (dir_name, legacy_name) = match label {
        "logo" => ("logos", "logo"),
        "banner" => ("banners", "banner"),
        _ => ("artists", label),
    };
    let base_dir = metadata_root.join(dir_name);
    let _ = tokio::fs::create_dir_all(&base_dir).await;

    for ext in ["jpg", "jpeg", "png"] {
        let filename = format!("{}.{}", artist_id, ext);
        let path = base_dir.join(&filename);
        if tokio::fs::metadata(&path).await.is_ok() {
            return Some(format!("{}/{}", dir_name, filename));
        }
    }

    let legacy_dir = metadata_root.join("artists").join(artist_id);
    for ext in ["jpg", "jpeg", "png"] {
        let legacy_path = legacy_dir.join(format!("{}.{}", legacy_name, ext));
        if tokio::fs::metadata(&legacy_path).await.is_ok() {
            let filename = format!("{}.{}", artist_id, ext);
            let target_path = base_dir.join(&filename);
            if tokio::fs::metadata(&target_path).await.is_err() {
                if tokio::fs::copy(&legacy_path, &target_path).await.is_err() {
                    return Some(format!("artists/{}/{}.{}", artist_id, legacy_name, ext));
                }
            }
            return Some(format!("{}/{}", dir_name, filename));
        }
    }

    let (bytes, ext) = match download_image(client, config, url).await {
        Some(value) => value,
        None => return None,
    };
    let filename = format!("{}.{}", artist_id, ext);
    let path = base_dir.join(&filename);
    if tokio::fs::write(&path, bytes).await.is_err() {
        return None;
    }
    Some(format!("{}/{}", dir_name, filename))
}

async fn download_image(
    client: &Client,
    config: &ExternalConfig,
    url: &str,
) -> Option<(Bytes, String)> {
    let response = client
        .get(url)
        .timeout(config_timeout(config))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let headers = response.headers().clone();
    let bytes = response.bytes().await.ok()?;
    let mut ext = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(image_ext_from_mime);
    if ext.is_none() {
        ext = image_ext_from_url(url);
    }
    let ext = ext.unwrap_or("jpg").to_string();
    Some((bytes, ext))
}

fn config_timeout(config: &ExternalConfig) -> Duration {
    config
        .sources
        .first()
        .map(|source| source.timeout)
        .unwrap_or_else(|| Duration::from_secs(8))
}

pub fn external_config(state: &AppState) -> Option<ExternalConfig> {
    let config = state.config.read().clone();
    external_config_from_settings(&config)
}

pub fn external_min_interval(state: &AppState) -> Duration {
    let config = state.config.read();
    let secs = config.external_metadata_min_interval_secs.max(60);
    Duration::from_secs(secs)
}

pub fn external_attempt_key(kind: &str, id: &str) -> String {
    format!("{}:{}", kind, id)
}

fn external_config_from_settings(config: &ServerConfig) -> Option<ExternalConfig> {
    let timeout = config.external_metadata_timeout_secs.max(1);
    let mut sources = Vec::new();
    for source in &config.external_metadata_sources {
        if !source.enabled {
            continue;
        }
        match source.provider {
            Provider::TheAudioDb => {
                let api_key = source.api_key.trim();
                if api_key.is_empty() {
                    continue;
                }
                sources.push(ExternalSource {
                    provider: Provider::TheAudioDb,
                    api_key: Some(api_key.to_string()),
                    user_agent: None,
                    timeout: Duration::from_secs(timeout),
                });
            }
            Provider::MusicBrainz => {
                let user_agent = source.user_agent.trim();
                if user_agent.is_empty() {
                    continue;
                }
                sources.push(ExternalSource {
                    provider: Provider::MusicBrainz,
                    api_key: None,
                    user_agent: Some(user_agent.to_string()),
                    timeout: Duration::from_secs(timeout),
                });
            }
        }
    }
    if sources.is_empty() {
        return None;
    }
    Some(ExternalConfig { sources })
}

pub fn parse_provider(value: &str) -> Result<Provider, String> {
    let provider =
        external::provider_from_str(value).ok_or_else(|| "unsupported provider".to_string())?;
    Ok(provider)
}

pub fn source_fields_from_parts(
    provider: Provider,
    api_key: &str,
    user_agent: &str,
) -> Result<(String, String), String> {
    let api_key = api_key.trim();
    let user_agent = user_agent.trim();
    match provider {
        Provider::TheAudioDb => {
            if api_key.is_empty() {
                return Err("api key is required".to_string());
            }
            Ok((api_key.to_string(), String::new()))
        }
        Provider::MusicBrainz => {
            if user_agent.is_empty() {
                return Err("user agent is required".to_string());
            }
            Ok((String::new(), user_agent.to_string()))
        }
    }
}

pub fn new_source_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    format!("src-{}", nanos)
}
