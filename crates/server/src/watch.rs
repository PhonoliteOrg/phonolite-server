use std::path::PathBuf;
use std::time::Duration;

use library::Library;
use notify::{
    Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{info, warn};

use crate::scan::{start_cover_sweep, start_enrichment_sweep};
use crate::state::AppState;

pub fn configure_watcher(state: &AppState, library: &Library) {
    let config = state.config.read().clone();
    if !config.watch_music {
        info!("Watcher disabled (watch_music=false)");
        *state.watcher.write() = None;
        return;
    }

    let roots: Vec<PathBuf> = library
        .roots()
        .iter()
        .map(|root| root.path.clone())
        .collect();
    if roots.is_empty() {
        info!("Watcher disabled (no music roots configured)");
        *state.watcher.write() = None;
        return;
    }

    let watch_debounce_secs = if config.watch_debounce_secs == 0 {
        2
    } else {
        config.watch_debounce_secs
    };
    let watch_debounce = Duration::from_secs(watch_debounce_secs);

    match setup_watcher(
        state.clone(),
        library.clone(),
        roots.clone(),
        watch_debounce,
    ) {
        Ok(watcher) => {
            let root_list = roots
                .iter()
                .map(|root| root.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            info!(
                "Watching {} for changes (debounce {}s)",
                root_list,
                watch_debounce.as_secs()
            );
            *state.watcher.write() = Some(watcher);
        }
        Err(err) => {
            warn!("Failed to start watcher: {}", err);
            *state.watcher.write() = None;
        }
    }
}

fn setup_watcher(
    state: AppState,
    library: Library,
    roots: Vec<PathBuf>,
    debounce: Duration,
) -> Result<RecommendedWatcher, Box<dyn std::error::Error>> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let mut watcher = RecommendedWatcher::new(
        move |res| {
            if let Ok(event) = res {
                let _ = tx.send(event);
            }
        },
        NotifyConfig::default(),
    )?;

    for root in roots {
        watcher.watch(&root, RecursiveMode::Recursive)?;
    }

    tokio::spawn(async move {
        watch_loop(state, library, rx, debounce).await;
    });

    Ok(watcher)
}

async fn watch_loop(
    state: AppState,
    library: Library,
    mut rx: UnboundedReceiver<Event>,
    debounce: Duration,
) {
    loop {
        let event = match rx.recv().await {
            Some(event) => event,
            None => break,
        };
        if !is_relevant_event(&event) {
            continue;
        }

        loop {
            tokio::select! {
                _ = tokio::time::sleep(debounce) => {
                    let _ = state
                        .activity
                        .add_activity("Library auto-scan started.");
                    let library = library.clone();
                    let rescan_library = library.clone();
                    match tokio::task::spawn_blocking(move || rescan_library.incremental_scan()).await {
                        Ok(Ok(stats)) => {
                            info!(
                                "Auto-scan complete: {} artists, {} albums, {} tracks",
                                stats.artists, stats.albums, stats.tracks
                            );
                            let _ = state.activity.add_activity(format!(
                                "Library auto-scan finished: {} artists, {} albums, {} tracks.",
                                stats.artists, stats.albums, stats.tracks
                            ));
                            start_enrichment_sweep(state.clone(), library.clone(), false);
                            start_cover_sweep(state.clone(), library.clone());
                        }
                        Ok(Err(err)) => warn!("Auto-rescan failed: {}", err),
                        Err(err) => warn!("Auto-rescan join error: {}", err),
                    }
                    break;
                }
                maybe_event = rx.recv() => {
                    if let Some(event) = maybe_event {
                        if !is_relevant_event(&event) {
                            continue;
                        }
                    } else {
                        return;
                    }
                }
            }
        }
    }
}

fn is_relevant_event(event: &Event) -> bool {
    matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}
