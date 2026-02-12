mod admin;
mod activity_store;
mod api;
mod assets;
mod auth;
mod config;
mod external;
mod quic;
mod range;
mod scan;
mod shuffle;
mod streaming;
mod stats_store;
mod state;
mod stream_sessions;
mod transcode;
mod user_data;
mod utils;
mod watch;

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Router,
};
use admin::admin_router;
use api::api_router;
use activity_store::ActivityStore;
use auth::AuthStore;
use config::{
    config_path_from_env, load_or_create_config, resolve_music_root, resolve_path,
};
use library::Library;
use parking_lot::RwLock;
use reqwest::Client;
use scan::{set_library_missing, start_index};
use stats_store::StatsStore;
use state::{AppState, LibraryState, LibraryStatus};
use user_data::{open_or_create_db as open_user_db, UserDataStore};
use tower_http::request_id::{MakeRequestUuid, SetRequestIdLayer};
use tower_http::trace::TraceLayer;
use tracing::{info, warn};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info".into());
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let config_path = config_path_from_env();
    let (config, created) = load_or_create_config(&config_path)?;
    let config_store = Arc::new(RwLock::new(config.clone()));

    if created {
        info!("Created default config at {:?}", config_path);
    } else {
        info!("Loaded config from {:?}", config_path);
    }

    let index_path_value = config.index_path.trim();
    let index_path_value = if index_path_value.is_empty() {
        "library.redb"
    } else {
        index_path_value
    };
    let port = if config.port == 0 { 3000 } else { config.port };
    let bind_addr = format!("0.0.0.0:{}", port);
    let session_ttl_secs = if config.session_ttl_secs == 0 {
        60 * 60 * 24 * 7
    } else {
        config.session_ttl_secs
    };
    let session_ttl = Duration::from_secs(session_ttl_secs);

    let index_path = resolve_path(&config_path, index_path_value);
    if let Some(parent) = index_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let db = Library::open_db(&index_path)?;
    let auth = AuthStore::new(Arc::clone(&db), session_ttl);
    if let Err(err) = auth.init_tables() {
        warn!("Failed to create initial tables: {}", err);
    }
    if let Err(err) = auth.ensure_superadmin() {
        warn!("Failed to ensure superadmin: {}", err);
    }
    let activity = ActivityStore::new(Arc::clone(&db));
    if let Err(err) = activity.init_tables() {
        warn!("Failed to create activity table: {}", err);
    }

    let user_db_path = resolve_path(&config_path, "user_data.redb");
    if let Some(parent) = user_db_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let user_db = Arc::new(open_user_db(&user_db_path)?);
    let user_data = UserDataStore::new(Arc::clone(&user_db));
    if let Err(err) = user_data.init_tables() {
        warn!("Failed to create user data tables: {:?}", err);
    }

    let stats_db_path = resolve_path(&config_path, "stats.redb");
    if let Some(parent) = stats_db_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let stats_db = Arc::new(open_user_db(&stats_db_path)?);
    let stats = StatsStore::new(Arc::clone(&stats_db));
    if let Err(err) = stats.init_tables() {
        warn!("Failed to create stats tables: {:?}", err);
    }
    let external_client = Client::builder()
        .user_agent("phonolite/0.1")
        .build()?;
    let library_state = Arc::new(RwLock::new(LibraryState {
        library: None,
        status: LibraryStatus::Unconfigured,
    }));
    let watcher = Arc::new(RwLock::new(None));
    let state = AppState {
        library_state,
        auth,
        config_path,
        config: config_store,
        db,
        user_data,
        stats,
        activity,
        watcher,
        external_client,
        stream_sessions: stream_sessions::StreamSessions::new(),
    };
    if let Some(music_root) = resolve_music_root(&state.config_path, &config.music_root) {
        if music_root.exists() {
            start_index(state.clone(), music_root, false);
        } else {
            set_library_missing(&state, music_root);
        }
    } else {
        info!("Music directory not configured yet; open the admin settings to select one.");
    }

    if let Ok(delay_ms) = std::env::var("PHONOLITE_START_DELAY_MS") {
        if let Ok(delay_ms) = delay_ms.parse::<u64>() {
            if delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
        }
    }

    let app = Router::new()
        .nest("/api/v1", api_router(state.clone()))
        // .nest("/api/v1", api::api_router(state.clone())) // Removed duplicate
        .merge(admin_router(state.clone()))
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(TraceLayer::new_for_http());

    if state.config.read().quic_enabled {
        let quic_state = state.clone();
        tokio::spawn(async move {
            if let Err(err) = quic::run(quic_state).await {
                tracing::error!("QUIC server failed: {}", err);
            }
        });
    }

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    info!("Listening on {}", bind_addr);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(err) => {
                warn!("Failed to install terminate signal handler: {}", err);
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    }

    #[cfg(not(unix))]
    {
        if let Err(err) = tokio::signal::ctrl_c().await {
            warn!("Failed to listen for ctrl-c: {}", err);
        }
    }

    info!("Shutdown signal received.");
}
