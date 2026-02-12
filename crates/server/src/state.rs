use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use axum::http::StatusCode;
use axum::Json;
use notify::RecommendedWatcher;
use parking_lot::RwLock;
use redb::Database;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::auth::{AuthStore, AuthUser};
use crate::activity_store::ActivityStore;
use crate::config::ServerConfig;
use crate::stats_store::StatsStore;
use crate::user_data::UserDataStore;
use crate::stream_sessions::StreamSessions;
use library::{Library, LibraryStats};

#[derive(Clone)]
pub struct AppState {
    pub library_state: Arc<RwLock<LibraryState>>,
    pub auth: AuthStore,
    pub config_path: PathBuf,
    pub config: Arc<RwLock<ServerConfig>>,
    pub db: Arc<Database>,
    pub user_data: UserDataStore,
    pub stats: StatsStore,
    pub activity: ActivityStore,
    pub watcher: Arc<RwLock<Option<RecommendedWatcher>>>,
    pub external_client: Client,
    pub stream_sessions: StreamSessions,
}

#[derive(Clone)]
pub struct LibraryState {
    pub library: Option<Library>,
    pub status: LibraryStatus,
}

#[derive(Clone, Debug)]
pub enum LibraryStatus {
    Unconfigured,
    Missing(PathBuf),
    Scanning { started: SystemTime },
    Ready(LibraryStats),
    Error(String),
}

#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
}

#[derive(Serialize)]
pub struct ServerPortsResponse {
    pub http_port: u16,
    pub quic_port: u16,
    pub quic_enabled: bool,
}

#[derive(Serialize)]
pub struct ListResponse<T> {
    pub items: Vec<T>,
    pub total: usize,
}

#[derive(Serialize)]
pub struct LibraryStatusResponse {
    pub status: String,
    pub message: Option<String>,
    pub artists: Option<usize>,
    pub albums: Option<usize>,
    pub tracks: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct ArtistQuery {
    pub search: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct ShuffleQuery {
    pub mode: String,
    pub artist_id: Option<String>,
    pub album_id: Option<String>,
    pub artist_ids: Option<String>,
    pub genres: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub query: String,
    pub limit: Option<usize>,
}

#[derive(Serialize)]
pub struct SearchResult {
    pub kind: String,
    pub id: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub score: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub track_ids: Vec<String>,
}


#[derive(Debug, Deserialize)]
pub struct CreatePlaylistRequest {
    pub name: String,
    #[serde(default)]
    pub track_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdatePlaylistRequest {
    pub name: Option<String>,
    pub track_ids: Option<Vec<String>>,
}

#[derive(Clone)]
pub struct AuthContext {
    pub user: AuthUser,
}

#[derive(Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Serialize)]
pub struct LoginResponse {
    pub token: String,
    pub expires_at: u64,
    pub token_type: &'static str,
}

#[derive(Deserialize)]
pub struct SetupForm {
    pub username: String,
    pub password: String,
    pub confirm: String,
}

#[derive(Deserialize)]
pub struct LoginForm {
    pub username: String,
    pub password: String,
}

#[derive(Deserialize)]
pub struct NewUserForm {
    pub username: String,
    pub password: String,
    pub role: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateUserForm {
    pub username: String,
    pub password: String,
    pub role: String,
}

#[derive(Deserialize)]
pub struct BulkDeleteForm {
    pub user_ids: String,
}

#[derive(Deserialize)]
pub struct PasswordForm {
    pub password: String,
}

#[derive(Deserialize)]
pub struct SettingsForm {
    pub music_root: String,
    pub index_path: String,
    pub metadata_path: String,
    pub port: String,
    pub quic_port: String,
    pub watch_music: Option<String>,
    pub watch_debounce_secs: String,
    pub session_ttl_secs: String,
    pub stats_collection_enabled: Option<String>,
    pub external_metadata_enabled: Option<String>,
    pub external_metadata_min_interval_secs: String,
    pub external_metadata_timeout_secs: String,
    pub external_metadata_enrich_on_scan: Option<String>,
    pub external_metadata_scan_limit: String,
    pub external_metadata_on_tag_error: Option<String>,
}

#[derive(Deserialize)]
pub struct SettingsQuery {
    pub music_root: Option<String>,
    pub message: Option<String>,
}

#[derive(Deserialize)]
pub struct AdminLibraryQuery {
    pub search: Option<String>,
    pub filter: Option<String>,
    pub offset: Option<usize>,
    pub album_id: Option<String>,
    pub artist_id: Option<String>,
}

#[derive(Deserialize)]
pub struct ArtistCoverQuery {
    pub kind: Option<String>,
}

#[derive(Deserialize)]
pub struct MetadataSourceForm {
    pub provider: String,
    pub api_key: Option<String>,
    pub user_agent: Option<String>,
}

#[derive(Deserialize)]
pub struct MetadataTestForm {
    pub provider: String,
    pub api_key: Option<String>,
    pub user_agent: Option<String>,
}

#[derive(Deserialize)]
pub struct MetadataToggleForm {
    pub enabled: Option<String>,
}

pub type JsonResult<T> = Result<Json<T>, (StatusCode, Json<ErrorResponse>)>;
