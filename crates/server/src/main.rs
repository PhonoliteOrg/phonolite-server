mod activity_store;
mod admin;
mod album_lookup;
mod api;
mod assets;
mod auth;
mod config;
mod download_files;
mod download_jobs;
mod external;
mod http;
mod library_search;
mod library_views;
mod logging;
mod metadata_events;
mod metadata_progress;
mod metadata_sources;
mod musicbrainz_album_artists;
mod musicbrainz_http;
mod musicbrainz_rate_limit;
mod offline_metadata;
mod performance;
mod process_control;
mod quic;
mod range;
mod scan;
mod security;
mod security_protocol;
mod shuffle;
mod state;
mod stats_store;
mod stream_cache;
mod streaming;
mod tls;
mod transcode;
mod user_data;
mod watch;

use std::sync::Arc;
use std::time::Duration;

use activity_store::ActivityStore;
use admin::admin_router;
use api::api_router;
use auth::AuthStore;
use axum::Router;
use config::{
    bind_target, config_path_from_env, load_or_create_config, resolve_music_roots, resolve_path,
};
use download_jobs::DownloadJobStore;
use library::Library;
use metadata_events::MetadataEventBus;
use parking_lot::RwLock;
use reqwest::Client;
use scan::{set_library_missing, start_index};
use state::{AppState, LibraryState, LibraryStatus};
use stats_store::StatsStore;
use stream_cache::StreamCache;
use tower_http::request_id::{MakeRequestUuid, SetRequestIdLayer};
use tower_http::trace::TraceLayer;
use tracing::{info, warn};
use user_data::{open_or_create_db as open_user_db, UserDataStore};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        None => {}
        Some("--setup-admin") if args.len() == 3 => {}
        Some("--renew-certificate") if args.len() == 2 => {}
        _ => return Err("usage: phonolite-server [--setup-admin USER | --renew-certificate]".into()),
    }
    let config_path = config_path_from_env();
    let (config, created) = load_or_create_config(&config_path)?;
    let (log_dir, log_control) = logging::init_logging(&config_path, &config)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))?;
    let config_store = Arc::new(RwLock::new(config.clone()));

    if args.get(1).map(String::as_str) == Some("--renew-certificate") {
        tls::renew(&config, &config_path)?;
        println!(
            "Certificate renewed with the existing server key. Restart the server when ready."
        );
        return Ok(());
    }

    if created {
        info!("Created default config at {:?}", config_path);
    } else {
        info!("Loaded config from {:?}", config_path);
    }
    info!("Logging to {}", log_dir.display());

    let index_path_value = config.index_path.trim();
    let index_path_value = if index_path_value.is_empty() {
        "library.redb"
    } else {
        index_path_value
    };
    let port = if config.port == 0 { 3000 } else { config.port };
    let bind_addr = bind_target(config.bind_addr.as_deref(), port);
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
    // A replacement process must wait before acquiring the previous process's
    // database locks, rather than delaying only after initialization.
    if let Ok(delay_ms) = std::env::var("PHONOLITE_START_DELAY_MS") {
        if let Ok(delay_ms) = delay_ms.parse::<u64>() {
            if delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
        }
    }
    let db = Library::open_db(&index_path)?;
    tls::harden_file(&index_path)?;
    let auth = AuthStore::new(Arc::clone(&db), session_ttl);
    auth.init_tables()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    let security = security::SecurityState::new(auth.opaque_setup()?);
    let enrolled = auth.has_any_user()?;
    if tls::activate_pending_rotation(&config, &config_path, || {
        auth.clear_sessions().map_err(|error| error.to_string())
    })? {
        info!("Server TLS identity rotated; all previous sessions revoked.");
    }
    let (cert_path, key_path) = tls::identity(&config, &config_path, enrolled)?;
    if args.get(1).map(String::as_str) == Some("--setup-admin") {
        let username = args.get(2).ok_or("provide a username")?;
        let password = zeroize::Zeroizing::new(rpassword::prompt_password("Password: ")?);
        let confirmation =
            zeroize::Zeroizing::new(rpassword::prompt_password("Confirm password: ")?);
        if *password != *confirmation {
            return Err("passwords do not match".into());
        }
        auth.create_superadmin(username, &password)?;
        println!("Initial account created. No server was started.");
        return Ok(());
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
    let download_jobs = DownloadJobStore::new(Arc::clone(&user_db));
    if let Err(err) = download_jobs.init_tables() {
        warn!("Failed to create download job tables: {}", err);
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
    let external_client = Client::builder().user_agent("phonolite/0.1").build()?;
    let stream_cache_dir = resolve_path(&config_path, &config.stream_cache_dir);
    let stream_cache = StreamCache::new(stream_cache_dir, config.stream_cache_enabled);
    let library_state = Arc::new(RwLock::new(LibraryState {
        library: None,
        status: LibraryStatus::Unconfigured,
        metadata_work: Default::default(),
    }));
    let watcher = Arc::new(RwLock::new(None));
    let state = AppState {
        library_state,
        auth,
        security,
        config_path,
        config: config_store,
        db,
        user_data,
        stats,
        activity,
        log_control,
        watcher,
        external_client,
        stream_cache,
        metadata_events: MetadataEventBus::new(1024),
        download_jobs,
        backup_jobs: Default::default(),
    };
    let roots = resolve_music_roots(&state.config_path, &config.music_roots);
    if roots.is_empty() {
        info!("Music directory not configured yet; open the admin settings to select one.");
    } else if let Some(missing) = roots.iter().find(|root| !root.path.exists()) {
        set_library_missing(&state, missing.path.clone());
    } else {
        start_index(state.clone(), roots, false);
    }

    let app = Router::new()
        .nest("/api", api_router(state.clone()))
        .layer(axum::middleware::from_fn(performance::http_request))
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(performance::trace_span)
                .on_request(performance::trace_request)
                .on_response(performance::trace_response)
                .on_eos(performance::trace_end),
        );

    if state.config.read().quic_enabled {
        let quic_state = state.clone();
        tokio::spawn(async move {
            if let Err(err) = quic::run(quic_state).await {
                tracing::error!("QUIC server failed: {}", err);
            }
        });
    }

    let admin_ip = config::parse_admin_bind_addr(&config.admin_bind_addr)?;
    let admin_listener = tokio::net::TcpListener::bind((admin_ip, config.admin_port)).await?;
    let admin_address = admin_listener.local_addr()?;
    let admin_app = admin_router(state.clone(), admin_address);
    let admin_cert = cert_path.clone();
    let admin_key = key_path.clone();
    tokio::spawn(async move {
        if let Err(error) = tls::serve(
            admin_listener,
            admin_app,
            &admin_cert,
            &admin_key,
            tls::Endpoint::Admin,
        )
        .await
        {
            tracing::error!("admin listener failed: {error}");
        }
    });
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    info!(
        "HTTPS listening on {}; administration listening on {}",
        bind_addr, admin_address
    );
    tls::serve(listener, app, &cert_path, &key_path, tls::Endpoint::Api).await?;
    Ok(())
}
