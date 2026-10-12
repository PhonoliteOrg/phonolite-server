use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use reqwest::Client;
use tracing::{info, warn};

use crate::activity_store::ActivityStore;
use crate::assets::{
    clear_metadata_assets, image_ext_from_mime, image_ext_from_url, metadata_root_path,
    prune_stale_metadata_assets, resolve_cover_source, warm_cover_cache, CoverCacheKey,
};
use crate::config::{resolve_music_roots, resolve_path, MusicRootConfig};
use crate::external::{self, ExternalConfig, ExternalSource, Provider};
use crate::metadata_events::MetadataEventBus;
use crate::metadata_progress::{MetadataPhase, MetadataWorkHandle};
use crate::metadata_sources::external_config_from_settings;
use crate::musicbrainz_album_artists::{
    identity_source_fingerprint, AlbumIdentityOutcome, MusicBrainzAlbumArtistResolver,
    ResolveOptions,
};
use crate::performance::MetadataTiming;
use crate::state::{AppState, LibraryStatus};
use crate::watch::configure_watcher;
use common::{Album, Artist};
use library::{AlbumSourceIdentity, Library, LibraryRoot, LibraryStats, ScanProgress};

pub fn start_index(state: AppState, roots: Vec<LibraryRoot>, force_rescan: bool) {
    let progress = Arc::new(parking_lot::RwLock::new(ScanProgress::Discovering));
    {
        let mut guard = state.library_state.write();
        guard.library = None;
        guard.metadata_work = Default::default();
        guard.status = LibraryStatus::Scanning {
            started: SystemTime::now(),
            progress: Arc::clone(&progress),
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
            let update_progress = |value| {
                *progress.write() = value;
            };
            let (library, mut scanned) =
                Library::load_or_scan_with_db_and_progress(roots_clone, db, &update_progress)?;
            // Policy repairs reparse unchanged source tags. An explicit reindex
            // also resets provider metadata and must reread every source file.
            let stats = if force_rescan {
                scanned = true;
                library.scan_with_progress(true, None, &update_progress)?
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
                if scanned && !force_rescan {
                    match prune_stale_metadata_assets(&state, &library).await {
                        Ok(pruned) if pruned.removed_files > 0 || pruned.removed_dirs > 0 => {
                            info!(
                                "Metadata asset GC removed {} files and {} directories",
                                pruned.removed_files, pruned.removed_dirs
                            );
                        }
                        Ok(_) => {}
                        Err(e) => warn!("Metadata asset GC failed: {}", e),
                    }
                }
                if scanned {
                    // Startup policy repairs rebuild from local metadata. Use
                    // normal enrichment so source descriptions survive; only
                    // an explicitly requested reindex uses replacement mode.
                    start_enrichment_sweep(state.clone(), library.clone(), force_rescan);
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

pub fn start_rescan(
    state: AppState,
    library: Library,
    replace_complete: bool,
) -> tokio::task::JoinHandle<()> {
    start_rescan_inner(state, library, replace_complete, None)
}

pub fn start_changed_scan(
    state: AppState,
    library: Library,
    paths: Option<Vec<PathBuf>>,
) -> tokio::task::JoinHandle<()> {
    start_rescan_inner(state, library, false, paths)
}

fn start_rescan_inner(
    state: AppState,
    library: Library,
    replace_complete: bool,
    changed_paths: Option<Vec<PathBuf>>,
) -> tokio::task::JoinHandle<()> {
    let progress = Arc::new(parking_lot::RwLock::new(ScanProgress::Discovering));
    {
        let mut guard = state.library_state.write();
        guard.metadata_work = Default::default();
        guard.status = LibraryStatus::Scanning {
            started: SystemTime::now(),
            progress: Arc::clone(&progress),
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
        let result = tokio::task::spawn_blocking(move || {
            library.scan_with_progress(replace_complete, changed_paths.as_deref(), &|value| {
                *progress.write() = value;
            })
        })
        .await;
        match result {
            Ok(Ok(stats)) => {
                {
                    let mut guard = state.library_state.write();
                    guard.status = LibraryStatus::Ready(stats.clone());
                }
                info!(
                    "Library rescan complete: {} artists, {} albums, {} tracks",
                    stats.artists, stats.albums, stats.tracks
                );
                let _ = state.activity.add_activity(format!(
                    "Library scan finished: {} artists, {} albums, {} tracks.",
                    stats.artists, stats.albums, stats.tracks
                ));
                if !replace_complete {
                    match prune_stale_metadata_assets(&state, &library_clone).await {
                        Ok(pruned) if pruned.removed_files > 0 || pruned.removed_dirs > 0 => {
                            info!(
                                "Metadata asset GC removed {} files and {} directories",
                                pruned.removed_files, pruned.removed_dirs
                            );
                        }
                        Ok(_) => {}
                        Err(e) => warn!("Metadata asset GC failed: {}", e),
                    }
                }
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
    })
}

pub fn set_library_missing(state: &AppState, path: PathBuf) {
    let mut guard = state.library_state.write();
    guard.library = None;
    guard.metadata_work = Default::default();
    guard.status = LibraryStatus::Missing(path);
    *state.watcher.write() = None;
}

pub fn set_library_unconfigured(state: &AppState) {
    let mut guard = state.library_state.write();
    guard.library = None;
    guard.metadata_work = Default::default();
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
    let mut cursor = None;
    let mut count = 0;
    loop {
        let (albums, total) = match library.page_albums(&mut cursor, page_size) {
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
    let budget = SweepBudget::new(replace_complete, config.external_metadata_scan_limit);
    if budget.is_disabled() {
        info!("External metadata sweep skipped (scan_limit=0)");
        return;
    }
    let min_interval = Duration::from_secs(config.external_metadata_min_interval_secs.max(60));
    info!(
        "External metadata sweep starting: sources={}, phase_limit={}, min_interval_secs={}, replace_complete={}",
        fetch_config.sources.len(),
        budget.label(),
        min_interval.as_secs(),
        replace_complete
    );
    let client = state.external_client.clone();
    let tag_error_first = config.external_metadata_on_tag_error;
    let metadata_root = resolve_path(&state.config_path, &config.metadata_path);
    let activity = state.activity.clone();
    let metadata_events = state.metadata_events.clone();
    let _ = activity.add_activity("Metadata scan started.");
    let progress = state
        .library_state
        .read()
        .metadata_work
        .start(MetadataPhase::Artists);
    tokio::spawn(async move {
        run_enrichment_sweep(
            library,
            client,
            fetch_config,
            min_interval,
            budget,
            tag_error_first,
            metadata_root,
            replace_complete,
            activity,
            metadata_events,
            progress,
        )
        .await;
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SweepBudget {
    replace_complete: bool,
    per_phase_limit: usize,
}

impl SweepBudget {
    fn new(replace_complete: bool, scan_limit: usize) -> Self {
        let per_phase_limit = if replace_complete {
            usize::MAX
        } else {
            scan_limit
        };
        Self {
            replace_complete,
            per_phase_limit,
        }
    }

    fn is_disabled(self) -> bool {
        !self.replace_complete && self.per_phase_limit == 0
    }

    fn label(self) -> String {
        if self.per_phase_limit == usize::MAX {
            "unlimited".to_string()
        } else {
            format!("{} per phase", self.per_phase_limit)
        }
    }

    fn artist_limit(self) -> usize {
        self.per_phase_limit
    }

    fn album_artist_limit(self) -> usize {
        self.per_phase_limit
    }

    fn artist_catchup_limit(self) -> usize {
        self.per_phase_limit
    }

    fn album_limit(self) -> usize {
        self.per_phase_limit
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PhaseStats {
    attempted: usize,
    skipped: usize,
    failed: usize,
    updated: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PhaseProgress {
    remaining: usize,
    stats: PhaseStats,
}

impl PhaseProgress {
    fn new(limit: usize) -> Self {
        Self {
            remaining: limit,
            stats: PhaseStats::default(),
        }
    }

    fn record_fetch(&mut self, result: FetchResult) {
        if result.attempted {
            self.record_attempt(result.updated);
            if result.failed {
                self.stats.failed = self.stats.failed.saturating_add(1);
            }
        } else {
            self.record_skip();
        }
    }

    fn record_attempt(&mut self, updated: bool) {
        self.consume_budget();
        self.stats.attempted = self.stats.attempted.saturating_add(1);
        if updated {
            self.stats.updated = self.stats.updated.saturating_add(1);
        }
    }

    fn record_skip(&mut self) {
        self.stats.skipped = self.stats.skipped.saturating_add(1);
    }

    fn record_failed(&mut self) {
        self.stats.failed = self.stats.failed.saturating_add(1);
    }

    fn record_failed_attempt(&mut self) {
        self.consume_budget();
        self.stats.attempted = self.stats.attempted.saturating_add(1);
        self.stats.failed = self.stats.failed.saturating_add(1);
    }

    fn consume_budget(&mut self) {
        if self.remaining != usize::MAX {
            self.remaining = self.remaining.saturating_sub(1);
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MetadataSweepStats {
    artists: PhaseStats,
    album_artists: PhaseStats,
    artist_catchup: PhaseStats,
    tag_error_albums: PhaseStats,
    albums: PhaseStats,
}

impl MetadataSweepStats {
    fn total_attempted(self) -> usize {
        self.artists.attempted
            + self.album_artists.attempted
            + self.artist_catchup.attempted
            + self.tag_error_albums.attempted
            + self.albums.attempted
    }

    fn total_skipped(self) -> usize {
        self.artists.skipped
            + self.album_artists.skipped
            + self.artist_catchup.skipped
            + self.tag_error_albums.skipped
            + self.albums.skipped
    }

    fn total_failed(self) -> usize {
        self.artists.failed
            + self.album_artists.failed
            + self.artist_catchup.failed
            + self.tag_error_albums.failed
            + self.albums.failed
    }
}

async fn run_enrichment_sweep(
    library: Library,
    client: Client,
    config: ExternalConfig,
    min_interval: Duration,
    budget: SweepBudget,
    tag_error_first: bool,
    metadata_root: PathBuf,
    replace_complete: bool,
    activity: ActivityStore,
    metadata_events: MetadataEventBus,
    progress: MetadataWorkHandle,
) {
    let mut stats = MetadataSweepStats::default();

    let artist_phase = run_artist_enrichment_sweep(
        &library,
        &client,
        &config,
        min_interval,
        &metadata_root,
        budget.artist_limit(),
        replace_complete,
        &activity,
        &metadata_events,
        &progress,
    )
    .await;
    log_metadata_phase("artists", artist_phase.stats);
    stats.artists = artist_phase.stats;

    progress.set_phase(MetadataPhase::AlbumArtists);
    let album_artist_phase = run_musicbrainz_album_artist_sweep(
        &library,
        &client,
        &config,
        min_interval,
        budget.album_artist_limit(),
        replace_complete,
        &activity,
        &metadata_events,
        &progress,
    )
    .await;
    log_metadata_phase("album artists", album_artist_phase.stats);
    stats.album_artists = album_artist_phase.stats;

    if album_artist_phase.stats.updated > 0 {
        progress.set_phase(MetadataPhase::Artists);
        let artist_catchup_phase = run_artist_enrichment_sweep(
            &library,
            &client,
            &config,
            min_interval,
            &metadata_root,
            budget.artist_catchup_limit(),
            false,
            &activity,
            &metadata_events,
            &progress,
        )
        .await;
        log_metadata_phase("artist catch-up", artist_catchup_phase.stats);
        stats.artist_catchup = artist_catchup_phase.stats;
    }

    progress.set_phase(MetadataPhase::Albums);
    let mut album_remaining = budget.album_limit();
    if tag_error_first {
        let tag_error_album_phase = run_tag_error_album_enrichment(
            &library,
            &client,
            &config,
            min_interval,
            album_remaining,
            replace_complete,
            &activity,
            &metadata_events,
            &progress,
        )
        .await;
        log_metadata_phase("tag-error albums", tag_error_album_phase.stats);
        album_remaining = tag_error_album_phase.remaining;
        stats.tag_error_albums = tag_error_album_phase.stats;
    }

    // The tag-error pass can check the same albums as the catalog pass.
    // Give each pass its own counter rather than double-counting checks.
    progress.set_phase(MetadataPhase::Albums);
    let album_phase = run_album_enrichment_sweep(
        &library,
        &client,
        &config,
        min_interval,
        album_remaining,
        replace_complete,
        &activity,
        &metadata_events,
        &progress,
    )
    .await;
    log_metadata_phase("albums", album_phase.stats);
    stats.albums = album_phase.stats;

    log_persisted_album_metadata_totals(&library);
    finish_metadata_scan(&activity, stats);
}

async fn run_artist_enrichment_sweep(
    library: &Library,
    client: &Client,
    config: &ExternalConfig,
    min_interval: Duration,
    metadata_root: &Path,
    limit: usize,
    replace_complete: bool,
    activity: &ActivityStore,
    metadata_events: &MetadataEventBus,
    work: &MetadataWorkHandle,
) -> PhaseProgress {
    let mut progress = PhaseProgress::new(limit);
    if progress.remaining == 0 {
        return progress;
    }
    let page_size = 100;
    let billed_artists = match library.billed_artist_ids() {
        Ok(ids) => ids,
        Err(err) => {
            warn!(
                "External artist sweep failed to load billed artists: {}",
                err
            );
            return progress;
        }
    };
    work.set_total(billed_artists.len());
    let mut offset = 0usize;
    let mut cursor = None;
    loop {
        let (items, total) = match library.page_artists(&mut cursor, page_size) {
            Ok(result) => result,
            Err(err) => {
                warn!("External artist sweep failed: {}", err);
                return progress;
            }
        };
        for artist in items {
            if !billed_artists.contains(&artist.id) {
                continue;
            }
            if progress.remaining == 0 {
                break;
            }
            let _item = work.item(&artist.name, None);
            let result = fetch_artist_enrichment(
                library,
                client,
                config,
                min_interval,
                metadata_root,
                &artist,
                replace_complete,
                Some(activity),
                Some(metadata_events),
            )
            .await;
            progress.record_fetch(result);
        }
        if progress.remaining == 0 || offset + page_size >= total {
            break;
        }
        offset += page_size;
    }
    progress
}

async fn run_album_enrichment_sweep(
    library: &Library,
    client: &Client,
    config: &ExternalConfig,
    min_interval: Duration,
    limit: usize,
    replace_complete: bool,
    activity: &ActivityStore,
    metadata_events: &MetadataEventBus,
    work: &MetadataWorkHandle,
) -> PhaseProgress {
    let mut progress = PhaseProgress::new(limit);
    if progress.remaining == 0 {
        return progress;
    }
    let page_size = 100;
    let mut offset = 0usize;
    let mut cursor = None;
    loop {
        let (items, total) = match library.page_albums(&mut cursor, page_size) {
            Ok(result) => result,
            Err(err) => {
                warn!("External album sweep failed: {}", err);
                return progress;
            }
        };
        work.set_total(total);
        for album in items {
            if progress.remaining == 0 {
                break;
            }
            let artist_name = library
                .get_artist(&album.artist_id)
                .ok()
                .and_then(|artist| artist.map(|value| value.name))
                .unwrap_or_else(|| "Unknown Artist".to_string());
            let _item = work.item(&artist_name, Some(&album.title));
            let result = fetch_album_enrichment(
                library,
                client,
                config,
                min_interval,
                &album,
                &artist_name,
                replace_complete,
                Some(activity),
                Some(metadata_events),
            )
            .await;
            progress.record_fetch(result);
        }
        if progress.remaining == 0 || offset + page_size >= total {
            break;
        }
        offset += page_size;
    }
    progress
}

fn finish_metadata_scan(activity: &ActivityStore, stats: MetadataSweepStats) {
    let artist_updates = stats.artists.updated + stats.artist_catchup.updated;
    let album_updates = stats.tag_error_albums.updated + stats.albums.updated;
    let attempts = stats.total_attempted();
    let skipped = stats.total_skipped();
    let failed = stats.total_failed();
    let summary = format!(
        "Metadata scan finished. Updated {} artists, {} albums, and {} collaborative album links. Attempts={}, skipped={}, failed={}.",
        artist_updates, album_updates, stats.album_artists.updated, attempts, skipped, failed
    );
    let _ = activity.add_activity(summary);
}

fn log_metadata_phase(label: &str, stats: PhaseStats) {
    info!(
        "Metadata scan phase '{}' finished: attempted={}, skipped={}, failed={}, updated={}",
        label, stats.attempted, stats.skipped, stats.failed, stats.updated
    );
}

fn log_persisted_album_metadata_totals(library: &Library) {
    match library.album_metadata_stats() {
        Ok(stats) => info!(
            event = "album_metadata_completeness",
            complete = stats.completed,
            total = stats.total,
            incomplete = stats.total.saturating_sub(stats.completed),
            "Persisted album metadata completeness after sweep"
        ),
        Err(err) => warn!(
            event = "album_metadata_completeness",
            error = %err,
            "Failed to read persisted album metadata completeness after sweep"
        ),
    }
}

fn album_metadata_missing_fields(album: &Album) -> &'static str {
    let description_present = album
        .summary
        .as_ref()
        .is_some_and(|summary| !summary.trim().is_empty());
    match (description_present, album.genres.is_empty()) {
        (true, false) => "none",
        (false, false) => "description",
        (true, true) => "genres",
        (false, true) => "description, genres",
    }
}

fn log_album_metadata_storage_outcome(
    library: &Library,
    album: &Album,
    artist_name: &str,
    replace: bool,
    received_description_present: bool,
    received_genre_count: usize,
    updated: bool,
) {
    // The provider payload can be partial, and replacing metadata preserves
    // previously stored fields when those fields are absent from the response.
    // Read the committed record rather than interpreting the payload as storage.
    match library.get_album(&album.id) {
        Ok(Some(persisted)) => info!(
            event = "album_metadata_storage",
            album_id = %album.id,
            artist = artist_name,
            album = %album.title,
            replace,
            received_description_present,
            received_genre_count,
            updated,
            persisted_description_present = persisted
                .summary
                .as_ref()
                .is_some_and(|summary| !summary.trim().is_empty()),
            persisted_genre_count = persisted.genres.len(),
            persisted_complete = persisted.has_enriched_metadata(),
            missing_fields = album_metadata_missing_fields(&persisted),
            "Album metadata storage result"
        ),
        Ok(None) => warn!(
            event = "album_metadata_storage",
            album_id = %album.id,
            artist = artist_name,
            album = %album.title,
            replace,
            received_description_present,
            received_genre_count,
            updated,
            outcome = "missing_record",
            "Album metadata storage could not be verified: album record is missing"
        ),
        Err(err) => warn!(
            event = "album_metadata_storage",
            album_id = %album.id,
            artist = artist_name,
            album = %album.title,
            replace,
            received_description_present,
            received_genre_count,
            updated,
            outcome = "read_failed",
            error = %err,
            "Album metadata storage could not be verified: persisted record read failed"
        ),
    }
}

async fn run_musicbrainz_album_artist_sweep(
    library: &Library,
    client: &Client,
    config: &ExternalConfig,
    min_interval: Duration,
    limit: usize,
    replace_complete: bool,
    activity: &ActivityStore,
    metadata_events: &MetadataEventBus,
    work: &MetadataWorkHandle,
) -> PhaseProgress {
    let mut progress = PhaseProgress::new(limit);
    let Some(source) = musicbrainz_source(config) else {
        return progress;
    };
    if progress.remaining == 0 {
        return progress;
    }

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
            progress.record_failed();
            return progress;
        }
    };

    let page_size = 100usize;
    let mut offset = 0usize;
    let mut cursor = None;
    let mut seen_albums = 0usize;
    let mut skipped_albums = 0usize;
    let mut matched_albums = 0usize;
    let mut unmatched_albums = 0usize;
    let mut failed_albums = 0usize;
    let mut updated_albums = 0usize;
    let mut total_albums = None;
    loop {
        let (albums, total) = match library.page_albums(&mut cursor, page_size) {
            Ok(result) => result,
            Err(err) => {
                warn!("MusicBrainz album-artist sweep failed: {}", err);
                progress.record_failed();
                return progress;
            }
        };
        work.set_total(total);
        if total_albums.is_none() {
            total_albums = Some(total);
            info!(
                "MusicBrainz album-artist sweep starting: total_albums={}, replace_complete={}, limit={}",
                total,
                replace_complete,
                if progress.remaining == usize::MAX {
                    "unlimited".to_string()
                } else {
                    progress.remaining.to_string()
                }
            );
        }

        for album in albums {
            if progress.remaining == 0 {
                break;
            }
            seen_albums = seen_albums.saturating_add(1);
            let total = total_albums.unwrap_or(seen_albums);
            let progress_label = format!("[{}/{}]", seen_albums, total);
            let artist_name = album.artist_display_name();
            let artist_name = if artist_name.trim().is_empty() {
                "Unknown Artist"
            } else {
                artist_name.as_str()
            };
            let _item = work.item(artist_name, Some(&album.title));

            let key = external_attempt_key("album_artists", &album.id);
            if !replace_complete {
                match library.should_attempt_external(&key, min_interval) {
                    Ok(true) => {}
                    Ok(false) => {
                        skipped_albums = skipped_albums.saturating_add(1);
                        progress.record_skip();
                        info!(
                            album_id = %album.id,
                            min_interval_secs = min_interval.as_secs(),
                            "MusicBrainz album-artist sweep {} skipping '{}' (artist='{}', min interval not reached)",
                            progress_label,
                            album.title,
                            artist_name
                        );
                        continue;
                    }
                    Err(err) => {
                        failed_albums = failed_albums.saturating_add(1);
                        progress.record_failed();
                        warn!(
                            album_id = %album.id,
                            artist = %artist_name,
                            album = %album.title,
                            "MusicBrainz album-artist check failed: {}", err
                        );
                        continue;
                    }
                }
            }

            let tracks = match library.get_album_tracks(&album.id) {
                Ok(tracks) if !tracks.is_empty() => tracks,
                Ok(_) => {
                    progress.record_skip();
                    continue;
                }
                Err(err) => {
                    failed_albums = failed_albums.saturating_add(1);
                    progress.record_failed();
                    warn!(
                        "MusicBrainz album-artist sweep {} failed to load tracks for '{}' (artist='{}'): {}",
                        progress_label,
                        album.title,
                        artist_name,
                        err
                    );
                    continue;
                }
            };

            info!(
                "MusicBrainz album-artist sweep {} resolving '{}' (artist='{}', tracks={})",
                progress_label,
                album.title,
                artist_name,
                tracks.len()
            );
            let _ = library.record_external_attempt(&key, false);
            match resolver
                .resolve_album_artists(library, &album, &tracks, &known_artists)
                .await
            {
                Ok(Some(resolution)) => {
                    let resolved_artists = resolution.artists;
                    matched_albums = matched_albums.saturating_add(1);
                    match library.merge_album_artists(&album.id, &resolved_artists) {
                        Ok(updated) => {
                            let updated = updated || resolution.source_credit_repaired;
                            progress.record_attempt(updated);
                            let _ = library.record_external_attempt(&key, true);
                            let associated_artists = resolved_artists
                                .iter()
                                .map(|artist| artist.name.clone())
                                .collect::<Vec<_>>();
                            let artist_names = associated_artists.join(", ");
                            if updated {
                                updated_albums = updated_albums.saturating_add(1);
                                merge_known_artists(&mut known_artists, &resolved_artists);
                                let artist_id = library
                                    .get_album(&album.id)
                                    .ok()
                                    .flatten()
                                    .map(|album| album.artist_id)
                                    .filter(|value| !value.trim().is_empty());
                                metadata_events.emit_album_artists(&album.id, artist_id);
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
                                progress_label,
                                album.title,
                                associated_artists,
                                updated
                            );
                        }
                        Err(err) => {
                            failed_albums = failed_albums.saturating_add(1);
                            progress.record_failed_attempt();
                            warn!(
                                "MusicBrainz album-artist sweep {} merge failed for '{}' (artist='{}'): {}",
                                progress_label,
                                album.title,
                                artist_name,
                                err
                            );
                        }
                    }
                }
                Ok(None) => {
                    unmatched_albums = unmatched_albums.saturating_add(1);
                    progress.record_attempt(false);
                    let _ = library.record_external_attempt(&key, false);
                    info!(
                        "MusicBrainz album-artist sweep {} found no usable match for '{}' (artist='{}')",
                        progress_label,
                        album.title,
                        artist_name
                    );
                }
                Err(err) => {
                    failed_albums = failed_albums.saturating_add(1);
                    progress.record_failed_attempt();
                    warn!(
                        "MusicBrainz album-artist sweep {} resolution failed for '{}' (artist='{}'): {}",
                        progress_label,
                        album.title,
                        artist_name,
                        err
                    );
                }
            }
        }

        if progress.remaining == 0 || offset + page_size >= total {
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

    progress
}

async fn run_tag_error_album_enrichment(
    library: &Library,
    client: &Client,
    config: &ExternalConfig,
    min_interval: Duration,
    limit: usize,
    replace_complete: bool,
    activity: &ActivityStore,
    metadata_events: &MetadataEventBus,
    work: &MetadataWorkHandle,
) -> PhaseProgress {
    let mut progress = PhaseProgress::new(limit);
    if progress.remaining == 0 {
        return progress;
    }
    let page_size = 100;
    let mut offset = 0usize;
    loop {
        let (items, total) = match library.list_tag_errors(page_size, offset) {
            Ok(result) => result,
            Err(err) => {
                warn!("External tag error sweep failed: {}", err);
                progress.record_failed();
                return progress;
            }
        };
        work.set_total(total);
        for info in items {
            if progress.remaining == 0 {
                break;
            }
            let _item = work.item(&info.artist_name, Some(&info.album_title));
            match library.get_album(&info.album_id) {
                Ok(Some(album)) => {
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
                        Some(metadata_events),
                    )
                    .await;
                    progress.record_fetch(result);
                }
                Ok(None) => progress.record_skip(),
                Err(err) => {
                    progress.record_failed();
                    warn!("External tag error album lookup failed: {}", err);
                }
            }
        }
        if progress.remaining == 0 || offset + page_size >= total {
            break;
        }
        offset += page_size;
    }
    progress
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
    let mut cursor = None;
    let limit = 200usize;
    loop {
        let (mut batch, total) = library.page_artists(&mut cursor, limit)?;
        items.append(&mut batch);
        if items.len() >= total {
            break;
        }
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
    pub failed: bool,
}

impl FetchResult {
    fn skipped() -> Self {
        Self {
            attempted: false,
            updated: false,
            failed: false,
        }
    }

    fn attempted(updated: bool) -> Self {
        Self {
            attempted: true,
            updated,
            failed: false,
        }
    }

    fn failed() -> Self {
        Self {
            attempted: true,
            updated: false,
            failed: true,
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
    metadata_events: Option<&MetadataEventBus>,
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
    let mut timing = MetadataTiming::new("artist");
    let _ = library.record_external_attempt(&key, false);
    if let Some(timing) = timing.as_mut() {
        timing.stage("store_attempt");
    }
    info!("Fetching external artist metadata for '{}'", artist.name);
    let fetched = external::fetch_artist(client, config, &artist.name).await;
    if let Some(timing) = timing.as_mut() {
        timing.stage("fetch");
    }
    match fetched {
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
            let (logo_ref, banner_ref) = store_artist_assets(
                metadata_root,
                client,
                config,
                &artist.id,
                &metadata,
                replace,
            )
            .await;
            if let Some(timing) = timing.as_mut() {
                timing.stage("artwork");
            }
            let mut failed = false;
            let updated = match library.update_artist_enrichment(
                &artist.id,
                metadata.summary,
                &metadata.genres,
                logo_ref,
                banner_ref,
                replace,
            ) {
                Ok(updated) => updated,
                Err(err) => {
                    warn!(
                        "External artist metadata update failed for {}: {}",
                        artist.name, err
                    );
                    failed = true;
                    false
                }
            };
            if updated {
                if let Some(metadata_events) = metadata_events {
                    metadata_events.emit_artist(&artist.id);
                }
            }
            if let Some(activity) = activity {
                let _ = activity.add_event(
                    "metadata",
                    format!("Fetched metadata for artist '{}'.", artist.name),
                );
            }
            let _ = library.record_external_attempt(&key, true);
            if let Some(mut timing) = timing {
                timing.stage("store");
                timing.finish(if failed { "error" } else { "ok" }, updated);
            }
            if failed {
                FetchResult::failed()
            } else {
                FetchResult::attempted(updated)
            }
        }
        Ok(None) => {
            info!("External artist metadata not found for '{}'", artist.name);
            // Keep the cooldown anchored at completion, including slow sources.
            let _ = library.record_external_attempt(&key, false);
            if let Some(timing) = timing {
                timing.finish("not_found", false);
            }
            FetchResult::attempted(false)
        }
        Err(err) => {
            warn!("External artist fetch failed for {}: {}", artist.name, err);
            if let Some(timing) = timing {
                timing.finish("error", false);
            }
            FetchResult::failed()
        }
    }
}

#[derive(Default)]
struct AlbumMetadataIdentity {
    identity: Option<AlbumSourceIdentity>,
    ambiguous: bool,
}

async fn resolve_album_metadata_identity(
    library: &Library,
    client: &Client,
    config: &ExternalConfig,
    album: &Album,
) -> AlbumMetadataIdentity {
    let result = async {
        let tracks = library
            .get_album_tracks(&album.id)
            .map_err(|err| err.to_string())?;
        let fingerprint = identity_source_fingerprint(library, album, &tracks)?;
        if let Some(identity) = library
            .get_album_source_identity(&album.id, &fingerprint)
            .map_err(|err| err.to_string())?
        {
            info!(
                event = "metadata_source_identity_cache",
                album_id = album.id,
                release_group_id = identity.musicbrainz_release_group_id,
                outcome = "hit"
            );
            return Ok::<_, String>(AlbumMetadataIdentity {
                identity: Some(identity),
                ambiguous: false,
            });
        }
        info!(
            event = "metadata_source_identity_cache",
            album_id = album.id,
            outcome = "miss"
        );
        let Some(source) = musicbrainz_source(config) else {
            return Ok(AlbumMetadataIdentity::default());
        };
        let user_agent = source.user_agent.as_deref().unwrap_or_default().trim();
        if user_agent.is_empty() {
            return Ok(AlbumMetadataIdentity::default());
        }
        let resolver = MusicBrainzAlbumArtistResolver::new(
            client,
            user_agent,
            source.timeout,
            ResolveOptions::default(),
        );
        let verified = match resolver
            .resolve_album_identity_outcome(library, album, &tracks)
            .await?
        {
            AlbumIdentityOutcome::Verified(identity) => identity,
            AlbumIdentityOutcome::NoMatch => return Ok(AlbumMetadataIdentity::default()),
            AlbumIdentityOutcome::Ambiguous => {
                return Ok(AlbumMetadataIdentity {
                    identity: None,
                    ambiguous: true,
                })
            }
        };
        let identity = verified.to_source_identity();
        if let Err(err) = library.set_album_source_identity(&album.id, &identity) {
            // Verified proof remains usable for this request if optional persistence fails.
            warn!(event = "metadata_source_identity_cache", album_id = album.id,
                outcome = "write_failed", error = %err);
        }
        info!(
            event = "metadata_source_identity_cache",
            album_id = album.id,
            release_group_id = identity.musicbrainz_release_group_id,
            outcome = "verified"
        );
        Ok(AlbumMetadataIdentity {
            identity: Some(identity),
            ambiguous: false,
        })
    }
    .await;
    match result {
        Ok(identity) => identity,
        Err(err) => {
            warn!(event = "metadata_album_identity_failed", album_id = album.id,
                album = album.title, error = %err,
                "Album identity resolution failed; literal provider matching remains available");
            AlbumMetadataIdentity::default()
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
    metadata_events: Option<&MetadataEventBus>,
) -> FetchResult {
    if !replace && !needs_album_enrichment(album) {
        info!(
            album_id = %album.id,
            artist = artist_name,
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
                    album_id = %album.id,
                    artist = artist_name,
                    min_interval_secs = min_interval.as_secs(),
                    "External metadata: skipping album '{}' (min interval not reached)",
                    album.title
                );
                return FetchResult::skipped();
            }
            Err(err) => {
                warn!(
                    album_id = %album.id,
                    artist = artist_name,
                    album = %album.title,
                    "External metadata check failed: {}", err
                );
                return FetchResult::skipped();
            }
        }
    }
    let mut timing = MetadataTiming::new("album");
    let _ = library.record_external_attempt(&key, false);
    if let Some(timing) = timing.as_mut() {
        timing.stage("store_attempt");
    }
    info!(
        album_id = %album.id,
        "Fetching external album metadata for '{}' - '{}'",
        artist_name, album.title
    );
    let identity = resolve_album_metadata_identity(library, client, config, album).await;
    let fetched = if identity.ambiguous {
        info!(
            event = "metadata_lookup_skipped",
            album_id = album.id,
            reason = "ambiguous_verified_album_groups",
            "Conflicting verified album groups prevent literal metadata fallback"
        );
        Ok(None)
    } else {
        external::fetch_album_with_identity(
            client,
            config,
            artist_name,
            &album.title,
            identity.identity.as_ref(),
        )
        .await
    };
    if let Some(timing) = timing.as_mut() {
        timing.stage("fetch");
    }
    match fetched {
        Ok(Some(metadata)) => {
            let received_description_present = metadata
                .summary
                .as_ref()
                .is_some_and(|summary| !summary.trim().is_empty());
            let received_genre_count = metadata.genres.len();
            let summary = metadata
                .summary
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or("<none>");
            info!(
                album_id = %album.id,
                "External album metadata received for '{} - {}': summary='{}', genres={:?}",
                artist_name, album.title, summary, metadata.genres
            );
            let mut failed = false;
            let updated = match library.update_album_enrichment(
                &album.id,
                metadata.summary,
                &metadata.genres,
                replace,
            ) {
                Ok(updated) => {
                    log_album_metadata_storage_outcome(
                        library,
                        album,
                        artist_name,
                        replace,
                        received_description_present,
                        received_genre_count,
                        updated,
                    );
                    updated
                }
                Err(err) => {
                    warn!(
                        event = "album_metadata_storage",
                        album_id = %album.id,
                        artist = artist_name,
                        album = %album.title,
                        replace,
                        received_description_present,
                        received_genre_count,
                        outcome = "write_failed",
                        "External album metadata update failed for {} - {}: {}",
                        artist_name, album.title, err
                    );
                    failed = true;
                    false
                }
            };
            if updated {
                if let Some(metadata_events) = metadata_events {
                    metadata_events.emit_album(&album.id, Some(album.artist_id.clone()));
                }
            }
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
            if let Some(mut timing) = timing {
                timing.stage("store");
                timing.finish(if failed { "error" } else { "ok" }, updated);
            }
            if failed {
                FetchResult::failed()
            } else {
                FetchResult::attempted(updated)
            }
        }
        Ok(None) => {
            info!(
                album_id = %album.id,
                "External album metadata not found for '{} - {}'",
                artist_name, album.title
            );
            let _ = library.record_external_attempt(&key, false);
            if let Some(timing) = timing {
                timing.finish("not_found", false);
            }
            FetchResult::attempted(false)
        }
        Err(err) => {
            warn!(
                album_id = %album.id,
                "External album fetch failed for {} - {}: {}",
                artist_name, album.title, err
            );
            if let Some(timing) = timing {
                timing.finish("error", false);
            }
            FetchResult::failed()
        }
    }
}

pub fn schedule_artist_enrichment(state: &AppState, library: &Library, artist: &Artist) {
    if !needs_artist_enrichment(artist) {
        return;
    }
    match library.billed_artist_ids() {
        Ok(ids) if ids.contains(&artist.id) => {}
        Ok(_) => return,
        Err(err) => {
            warn!("Cannot determine artist billing for metadata: {}", err);
            return;
        }
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
    let metadata_events = state.metadata_events.clone();
    let progress = state
        .library_state
        .read()
        .metadata_work
        .start(MetadataPhase::Artists);
    tokio::spawn(async move {
        let _progress = progress;
        _progress.set_total(1);
        let _item = _progress.item(&artist.name, None);
        let _ = fetch_artist_enrichment(
            &library,
            &client,
            &config,
            min_interval,
            &metadata_root,
            &artist,
            false,
            Some(&activity),
            Some(&metadata_events),
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
    let metadata_events = state.metadata_events.clone();
    let progress = state
        .library_state
        .read()
        .metadata_work
        .start(MetadataPhase::Albums);
    tokio::spawn(async move {
        let _progress = progress;
        _progress.set_total(1);
        let _item = _progress.item(&artist_name, Some(&album.title));
        let _ = fetch_album_enrichment(
            &library,
            &client,
            &config,
            min_interval,
            &album,
            &artist_name,
            false,
            Some(&activity),
            Some(&metadata_events),
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
    !album.has_enriched_metadata()
}

async fn store_artist_assets(
    metadata_root: &Path,
    client: &Client,
    config: &ExternalConfig,
    artist_id: &str,
    metadata: &external::ExternalMetadata,
    replace: bool,
) -> (Option<String>, Option<String>) {
    // Independent assets use separate directories; one slow download need not
    // delay starting the other. Verified source-URL cache checks stay first.
    tokio::join!(
        async {
            match metadata.logo_url.as_deref() {
                Some(url) => {
                    fetch_and_store_asset(
                        metadata_root,
                        client,
                        config,
                        artist_id,
                        "logo",
                        url,
                        replace,
                    )
                    .await
                }
                None => None,
            }
        },
        async {
            match metadata.banner_url.as_deref() {
                Some(url) => {
                    fetch_and_store_asset(
                        metadata_root,
                        client,
                        config,
                        artist_id,
                        "banner",
                        url,
                        replace,
                    )
                    .await
                }
                None => None,
            }
        }
    )
}

async fn fetch_and_store_asset(
    metadata_root: &Path,
    client: &Client,
    config: &ExternalConfig,
    artist_id: &str,
    label: &str,
    url: &str,
    replace: bool,
) -> Option<String> {
    fetch_and_store_asset_with(metadata_root, artist_id, label, url, replace, || {
        download_image(client, config, url)
    })
    .await
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ArtistAssetSource {
    url: String,
    filename: String,
}

async fn fetch_and_store_asset_with<F, Fut>(
    metadata_root: &Path,
    artist_id: &str,
    label: &str,
    url: &str,
    replace: bool,
    download: F,
) -> Option<String>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Option<(Bytes, String)>>,
{
    let started = crate::performance::enabled().then(std::time::Instant::now);
    let (dir_name, _legacy_name) = match label {
        "logo" => ("logos", "logo"),
        "banner" => ("banners", "banner"),
        _ => ("artists", label),
    };
    let base_dir = metadata_root.join(dir_name);
    tokio::fs::create_dir_all(&base_dir).await.ok()?;
    let source_record = base_dir.join(format!("{artist_id}.source"));
    if !replace {
        if let Ok(bytes) = tokio::fs::read(&source_record).await {
            if let Ok(cached) = serde_json::from_slice::<ArtistAssetSource>(&bytes) {
                // Older unversioned and legacy assets have no known provider
                // URL. Refresh them once instead of assuming the current URL
                // refers to those bytes.
                if cached.url == url
                    && cached.filename.starts_with(&format!("{artist_id}."))
                    && Path::new(&cached.filename).components().count() == 1
                {
                    if let Ok(metadata) = tokio::fs::metadata(base_dir.join(&cached.filename)).await
                    {
                        record_asset_timing(started, label, "cache", "ok", Some(metadata.len()));
                        return Some(format!("{dir_name}/{}", cached.filename));
                    }
                }
            }
        }
    }

    let (bytes, ext) = match download().await {
        Some(value) => value,
        None => {
            record_asset_timing(started, label, "network", "error", None);
            return None;
        }
    };
    let byte_count = bytes.len() as u64;
    // A content version changes the stored reference, so metadata listeners
    // can observe replacement artwork even when its provider URL is unchanged.
    let version = blake3::hash(&bytes).to_hex();
    let filename = format!("{artist_id}.{version}.{ext}");
    let path = base_dir.join(&filename);
    if tokio::fs::write(&path, &bytes).await.is_err() {
        record_asset_timing(started, label, "network", "store_error", Some(byte_count));
        return None;
    }
    if let Ok(record) = serde_json::to_vec(&ArtistAssetSource {
        url: url.to_owned(),
        filename: filename.clone(),
    }) {
        let _ = tokio::fs::write(source_record, record).await;
    }
    record_asset_timing(started, label, "network", "ok", Some(byte_count));
    Some(format!("{}/{}", dir_name, filename))
}

fn record_asset_timing(
    started: Option<std::time::Instant>,
    asset: &str,
    source: &'static str,
    outcome: &'static str,
    bytes: Option<u64>,
) {
    if let Some(started) = started {
        tracing::info!(
            target: "performance",
            event = "metadata_artwork",
            asset,
            source,
            outcome,
            bytes,
            elapsed_ms = crate::performance::milliseconds(started.elapsed()),
        );
    }
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

#[cfg(test)]
mod tests {
    use super::{FetchResult, PhaseProgress, SweepBudget};

    fn metadata_album(summary: Option<&str>, genres: &[&str]) -> common::Album {
        serde_json::from_value(serde_json::json!({
            "id": "diagnostic-album", "artist_id": "artist", "artist_names": ["Artist"],
            "title": "Diagnostic Album", "year": null, "folder_relpath": "Artist/Album",
            "cover_ref": null, "summary": summary, "genres": genres
        }))
        .unwrap()
    }

    fn metadata_test_library(album: Option<&common::Album>) -> library::Library {
        let db = std::sync::Arc::new(
            redb::Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        );
        let library = library::Library::load_or_scan_with_db(Vec::new(), db.clone())
            .unwrap()
            .0;
        if let Some(album) = album {
            let transaction = db.begin_write().unwrap();
            {
                let mut albums = transaction
                    .open_table(redb::TableDefinition::<&str, &[u8]>::new("albums"))
                    .unwrap();
                let encoded = bincode::serialize(album).unwrap();
                albums
                    .insert(album.id.as_str(), encoded.as_slice())
                    .unwrap();
                let mut meta = transaction
                    .open_table(redb::TableDefinition::<&str, &[u8]>::new("meta"))
                    .unwrap();
                let counts = bincode::serialize(&library::AlbumMetadataStats {
                    completed: usize::from(album.has_enriched_metadata()),
                    total: 1,
                })
                .unwrap();
                meta.insert("album_metadata_stats_v1", counts.as_slice())
                    .unwrap();
            }
            transaction.commit().unwrap();
        }
        library
    }

    #[tokio::test]
    async fn metadata_identity_cache_reuses_proof_without_enabling_musicbrainz_requests() {
        let album = metadata_album(None, &[]);
        let library = metadata_test_library(Some(&album));
        let fingerprint = super::identity_source_fingerprint(&library, &album, &[]).unwrap();
        let identity = library::AlbumSourceIdentity {
            schema_version: library::ALBUM_SOURCE_IDENTITY_SCHEMA_VERSION,
            context_fingerprint: fingerprint,
            musicbrainz_release_id: "c547b57a-0cf7-4fbe-85d4-759a1193a6f8".into(),
            musicbrainz_release_group_id: "4587e021-3f9d-43a6-81f3-50505e05fbd3".into(),
            canonical_title: "Diagnostic Album".into(),
            artist_names: vec!["Artist".into()],
            artist_ids: vec!["070d193a-845c-479f-980e-bef15710653e".into()],
            verified_at_ms: 1,
            evidence: "previously verified test proof".into(),
        };
        library
            .set_album_source_identity(&album.id, &identity)
            .unwrap();
        // No providers configured: this must be a cache-only path.
        let config = crate::external::ExternalConfig {
            sources: Vec::new(),
        };
        let client = reqwest::Client::new();
        assert_eq!(
            super::resolve_album_metadata_identity(&library, &client, &config, &album,)
                .await
                .identity,
            Some(identity)
        );
        let mut changed = album;
        changed.title = "A different source album".into();
        assert!(
            super::resolve_album_metadata_identity(&library, &client, &config, &changed,)
                .await
                .identity
                .is_none()
        );
    }

    #[derive(Clone)]
    struct MetadataLogWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for MetadataLogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn capture_metadata_logs(action: impl FnOnce()) -> String {
        let output = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = MetadataLogWriter(output.clone());
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, action);
        let bytes = output.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn metadata_missing_field_diagnostics_distinguish_each_incomplete_state() {
        for (summary, genres, expected) in [
            (None, Vec::new(), "description, genres"),
            (Some(" \n\t"), vec!["Rock"], "description"),
            (Some("Description"), Vec::new(), "genres"),
            (Some("Description"), vec!["Rock"], "none"),
        ] {
            let album = metadata_album(summary, &genres);
            assert_eq!(super::album_metadata_missing_fields(&album), expected);
            assert_eq!(album.has_enriched_metadata(), expected == "none");
        }
    }

    #[test]
    fn metadata_storage_diagnostics_read_committed_fields_and_report_cached_completeness() {
        let album = metadata_album(Some("Existing description"), &["Rock"]);
        let library = metadata_test_library(Some(&album));
        let updated = library
            .update_album_enrichment(&album.id, None, &["Jazz".into()], true)
            .unwrap();
        assert!(updated);
        let logs = capture_metadata_logs(|| {
            super::log_album_metadata_storage_outcome(
                &library, &album, "Artist", true, false, 1, updated,
            );
            super::log_persisted_album_metadata_totals(&library);
        });
        for field in [
            "album_id=diagnostic-album",
            "received_description_present=false",
            "received_genre_count=1",
            "updated=true",
            "persisted_description_present=true",
            "persisted_genre_count=1",
            "persisted_complete=true",
            "missing_fields=\"none\"",
            "complete=1 total=1 incomplete=0",
        ] {
            assert!(logs.contains(field), "{logs}");
        }
        assert_eq!(
            library
                .get_album(&album.id)
                .unwrap()
                .unwrap()
                .summary
                .as_deref(),
            Some("Existing description")
        );

        // A received genre can normalize away. The committed record and cached
        // count, rather than raw payload size or updated=true, define completion.
        let updated = library
            .update_album_enrichment(&album.id, None, &[" \t".into()], true)
            .unwrap();
        assert!(updated);
        let logs = capture_metadata_logs(|| {
            super::log_album_metadata_storage_outcome(
                &library, &album, "Artist", true, false, 1, updated,
            );
            super::log_persisted_album_metadata_totals(&library);
        });
        for field in [
            "received_genre_count=1",
            "updated=true",
            "persisted_genre_count=0",
            "persisted_complete=false",
            "missing_fields=\"genres\"",
            "complete=0 total=1 incomplete=1",
        ] {
            assert!(logs.contains(field), "{logs}");
        }
    }

    #[test]
    fn metadata_storage_diagnostics_do_not_claim_completeness_when_verification_fails() {
        let album = metadata_album(None, &[]);
        let library = metadata_test_library(None);
        let missing = capture_metadata_logs(|| {
            super::log_album_metadata_storage_outcome(
                &library, &album, "Artist", false, true, 1, false,
            );
        });
        assert!(missing.contains("outcome=\"missing_record\""), "{missing}");
        assert!(!missing.contains("persisted_complete="), "{missing}");

        let db = library.db();
        let transaction = db.begin_write().unwrap();
        {
            let mut albums = transaction
                .open_table(redb::TableDefinition::<&str, &[u8]>::new("albums"))
                .unwrap();
            albums.insert(album.id.as_str(), &[0xff_u8][..]).unwrap();
        }
        transaction.commit().unwrap();
        let failed = capture_metadata_logs(|| {
            super::log_album_metadata_storage_outcome(
                &library, &album, "Artist", false, true, 1, false,
            );
        });
        assert!(failed.contains("outcome=\"read_failed\""), "{failed}");
        assert!(!failed.contains("persisted_complete="), "{failed}");
    }

    struct AssetTestDirectory(std::path::PathBuf);

    impl AssetTestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("phonolite-artist-assets-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for AssetTestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn metadata_sweep_checks_advance_without_claiming_unmatched_or_deferred_albums_complete()
    {
        let dir = AssetTestDirectory::new();
        for title in ["First", "Second"] {
            let folder = dir.0.join("Artist").join(title);
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(folder.join("Track.mp3"), b"untagged audio fixture").unwrap();
        }
        let db = std::sync::Arc::new(
            redb::Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        );
        let library = library::Library::load_or_scan_with_db(
            vec![library::LibraryRoot {
                id: String::new(),
                path: dir.0.clone(),
            }],
            db.clone(),
        )
        .unwrap()
        .0;
        let albums = library.list_albums(None, 10, 0).unwrap().0;
        assert_eq!(albums.len(), 2);
        library
            .record_external_attempt(&super::external_attempt_key("album", &albums[0].id), false)
            .unwrap();
        let activity = crate::activity_store::ActivityStore::new(db);
        activity.init_tables().unwrap();
        let events = crate::metadata_events::MetadataEventBus::new(16);
        let work = std::sync::Arc::new(crate::metadata_progress::MetadataWork::default());
        let job = work.start(crate::metadata_progress::MetadataPhase::Albums);
        // No providers: the other album is unmatched without any network requests.
        let phase = super::run_album_enrichment_sweep(
            &library,
            &reqwest::Client::new(),
            &crate::external::ExternalConfig {
                sources: Vec::new(),
            },
            std::time::Duration::from_secs(3600),
            usize::MAX,
            false,
            &activity,
            &events,
            &job,
        )
        .await;
        assert_eq!(phase.stats.skipped, 1);
        assert_eq!(phase.stats.attempted, 1);
        let progress = work.snapshot().unwrap();
        assert_eq!(progress.checked, 2);
        assert_eq!(progress.total, Some(2));
        let stored = library.album_metadata_stats().unwrap();
        assert_eq!(stored.completed, 0);
        assert_eq!(stored.total, 2);
    }

    #[tokio::test]
    async fn artist_artwork_reuses_verified_url_cache_and_refreshes_changed_urls_or_replacements() {
        let dir = AssetTestDirectory::new();
        let first = super::fetch_and_store_asset_with(
            &dir.0,
            "artist",
            "logo",
            "https://example.test/old",
            false,
            || async { Some((bytes::Bytes::from_static(b"first logo"), "png".to_owned())) },
        )
        .await
        .unwrap();
        assert_eq!(
            tokio::fs::read(dir.0.join(&first)).await.unwrap(),
            b"first logo"
        );

        let reused = super::fetch_and_store_asset_with(
            &dir.0,
            "artist",
            "logo",
            "https://example.test/old",
            false,
            || async { panic!("unchanged provider URL should reuse verified artwork") },
        )
        .await
        .unwrap();
        assert_eq!(reused, first);

        let new_url = super::fetch_and_store_asset_with(
            &dir.0,
            "artist",
            "logo",
            "https://example.test/new",
            false,
            || async {
                Some((
                    bytes::Bytes::from_static(b"new provider logo"),
                    "webp".to_owned(),
                ))
            },
        )
        .await
        .unwrap();
        assert_ne!(new_url, first);
        assert_eq!(
            tokio::fs::read(dir.0.join(&new_url)).await.unwrap(),
            b"new provider logo"
        );

        let replacement = super::fetch_and_store_asset_with(
            &dir.0,
            "artist",
            "logo",
            "https://example.test/new",
            true,
            || async {
                Some((
                    bytes::Bytes::from_static(b"corrected same URL logo"),
                    "webp".to_owned(),
                ))
            },
        )
        .await
        .unwrap();
        assert_ne!(replacement, new_url);
        assert_eq!(
            tokio::fs::read(dir.0.join(&replacement)).await.unwrap(),
            b"corrected same URL logo"
        );
        let unchanged_bytes = super::fetch_and_store_asset_with(
            &dir.0,
            "artist",
            "logo",
            "https://example.test/new",
            true,
            || async {
                Some((
                    bytes::Bytes::from_static(b"corrected same URL logo"),
                    "webp".to_owned(),
                ))
            },
        )
        .await
        .unwrap();
        assert_eq!(unchanged_bytes, replacement);
    }

    #[tokio::test]
    async fn artwork_refresh_rejects_unverified_legacy_cache_and_failed_download_keeps_previous_asset(
    ) {
        let dir = AssetTestDirectory::new();
        let logos = dir.0.join("logos");
        tokio::fs::create_dir_all(&logos).await.unwrap();
        tokio::fs::write(logos.join("artist.jpg"), b"unverified wrong artist")
            .await
            .unwrap();
        let corrected = super::fetch_and_store_asset_with(
            &dir.0,
            "artist",
            "logo",
            "https://example.test/correct",
            false,
            || async {
                Some((
                    bytes::Bytes::from_static(b"verified new artist"),
                    "jpg".to_owned(),
                ))
            },
        )
        .await
        .unwrap();
        assert_eq!(
            tokio::fs::read(dir.0.join(&corrected)).await.unwrap(),
            b"verified new artist"
        );

        assert!(super::fetch_and_store_asset_with(
            &dir.0,
            "artist",
            "logo",
            "https://example.test/correct",
            true,
            || async { None }
        )
        .await
        .is_none());
        assert_eq!(
            tokio::fs::read(dir.0.join(&corrected)).await.unwrap(),
            b"verified new artist"
        );
        let retained = super::fetch_and_store_asset_with(
            &dir.0,
            "artist",
            "logo",
            "https://example.test/correct",
            false,
            || async { panic!("a failed refresh must leave the previous verified cache usable") },
        )
        .await
        .unwrap();
        assert_eq!(retained, corrected);
    }

    #[test]
    fn normal_sweep_budget_applies_limit_per_phase() {
        let budget = SweepBudget::new(false, 7);

        assert!(!budget.is_disabled());
        assert_eq!(budget.artist_limit(), 7);
        assert_eq!(budget.album_artist_limit(), 7);
        assert_eq!(budget.artist_catchup_limit(), 7);
        assert_eq!(budget.album_limit(), 7);
        assert_eq!(budget.label(), "7 per phase");
    }

    #[test]
    fn zero_normal_sweep_budget_disables_external_scan() {
        let budget = SweepBudget::new(false, 0);

        assert!(budget.is_disabled());
        assert_eq!(budget.artist_limit(), 0);
        assert_eq!(budget.album_limit(), 0);
    }

    #[test]
    fn full_reindex_sweep_budget_is_unlimited() {
        let budget = SweepBudget::new(true, 0);

        assert!(!budget.is_disabled());
        assert_eq!(budget.artist_limit(), usize::MAX);
        assert_eq!(budget.album_artist_limit(), usize::MAX);
        assert_eq!(budget.artist_catchup_limit(), usize::MAX);
        assert_eq!(budget.album_limit(), usize::MAX);
        assert_eq!(budget.label(), "unlimited");
    }

    #[test]
    fn album_artist_attempts_do_not_consume_album_phase_budget() {
        let budget = SweepBudget::new(false, 2);
        let mut album_artist_phase = PhaseProgress::new(budget.album_artist_limit());
        album_artist_phase.record_attempt(false);
        album_artist_phase.record_attempt(true);

        assert_eq!(album_artist_phase.remaining, 0);
        assert_eq!(budget.album_limit(), 2);
    }

    #[test]
    fn failed_fetches_consume_phase_budget_and_are_counted() {
        let mut progress = PhaseProgress::new(2);

        progress.record_fetch(FetchResult::failed());

        assert_eq!(progress.remaining, 1);
        assert_eq!(progress.stats.attempted, 1);
        assert_eq!(progress.stats.failed, 1);
        assert_eq!(progress.stats.skipped, 0);
    }
}
