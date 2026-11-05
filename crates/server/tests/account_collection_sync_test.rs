//! Production collection stream/store/auth checks with no listening socket or
//! audio codec. The imported modules also provide focused store unit tests.
#![allow(dead_code, unused_imports)]

#[path = "../src/api/account.rs"]
mod account_api;
#[path = "../src/api/account_events.rs"]
mod account_events;
#[path = "support/account_http_cases.rs"]
mod account_http_cases;
#[path = "../src/api/account_sync.rs"]
mod account_sync;
#[path = "../src/auth.rs"]
mod auth;
#[path = "../src/http.rs"]
mod http;
#[path = "../src/network_metrics.rs"]
mod network_metrics;
#[path = "../src/api/playback_presence.rs"]
mod playback_presence_api;
#[path = "../src/presence.rs"]
mod presence;
#[path = "../src/process_metrics.rs"]
mod process_metrics;
#[path = "../src/security_protocol.rs"]
mod security_protocol;
#[path = "../src/shutdown.rs"]
mod shutdown;
#[path = "../src/stats_store.rs"]
mod stats_store;
#[path = "../src/user_data.rs"]
mod user_data;

mod state {
    use serde::{Deserialize, Serialize};

    #[derive(Clone)]
    pub struct AppState {
        pub library_state: std::sync::Arc<parking_lot::RwLock<LibraryState>>,
        pub stats: crate::stats_store::StatsStore,
        pub user_data: crate::user_data::UserDataStore,
        pub auth: crate::auth::AuthStore,
        pub shutdown: crate::shutdown::Shutdown,
        pub process_metrics: crate::process_metrics::ProcessMetrics,
    }

    pub struct LibraryState {
        pub library: Option<library::Library>,
    }

    #[derive(Clone)]
    pub struct AuthContext {
        pub user: crate::auth::AuthUser,
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct Playlist {
        pub id: String,
        pub name: String,
        #[serde(default)]
        pub track_ids: Vec<String>,
        #[serde(default)]
        pub description: Option<String>,
        #[serde(default)]
        pub image_ref: Option<String>,
    }

    #[derive(Serialize)]
    pub struct ErrorResponse {
        pub error: String,
    }

    #[derive(Serialize)]
    pub struct HealthResponse {
        pub status: &'static str,
    }

    pub type JsonResult<T> =
        Result<axum::Json<T>, (axum::http::StatusCode, axum::Json<ErrorResponse>)>;
}

fn library_or_json_error(
    state: &state::AppState,
) -> Result<library::Library, (axum::http::StatusCode, axum::Json<state::ErrorResponse>)> {
    state.library_state.read().library.clone().ok_or_else(|| {
        http::json_error(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "library unavailable",
        )
    })
}
