//! In-process requests against the production personalization handlers and redb store.
//!
//! Only AppState, response helpers, and authentication are reduced fixtures: the
//! two fixed bearer tokens stand in for already authenticated server identities.
//! No socket, server process, or codec is needed. Catalog projections use a tiny
//! temporary music library. This does not
//! replace the authentication/session tests for the production auth middleware.
#![allow(dead_code, unused_imports)]

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{to_bytes, Body, Bytes};
use axum::extract::Request;
use axum::http::{header, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tower::ServiceExt;

#[path = "../src/shutdown.rs"]
mod shutdown;

mod state {
    use axum::http::StatusCode;
    use axum::Json;
    use serde::{Deserialize, Serialize};

    #[derive(Clone)]
    pub struct AppState {
        pub user_data: crate::user_data::UserDataStore,
        pub shutdown: crate::shutdown::Shutdown,
    }

    #[derive(Clone)]
    pub struct AuthUser {
        pub id: String,
    }

    #[derive(Clone)]
    pub struct AuthContext {
        pub user: AuthUser,
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

    #[derive(Debug, Deserialize)]
    pub struct CreatePlaylistRequest {
        pub name: String,
        #[serde(default)]
        pub track_ids: Vec<String>,
        #[serde(default)]
        pub description: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    pub struct UpdatePlaylistRequest {
        pub name: Option<String>,
        pub track_ids: Option<Vec<String>>,
        pub description: Option<String>,
    }

    #[derive(Debug, Serialize)]
    pub struct ErrorResponse {
        pub error: String,
    }

    pub type JsonResult<T> = Result<Json<T>, (StatusCode, Json<ErrorResponse>)>;
}

mod http {
    use super::*;

    pub fn json_error(
        status: StatusCode,
        message: impl Into<String>,
    ) -> (StatusCode, Json<crate::state::ErrorResponse>) {
        (
            status,
            Json(crate::state::ErrorResponse {
                error: message.into(),
            }),
        )
    }

    pub fn json_error_response(status: StatusCode, message: impl Into<String>) -> Response {
        json_error(status, message).into_response()
    }
}

mod assets {
    use super::*;

    pub fn cover_response(data: Vec<u8>, mime: &str) -> Response {
        (
            [
                (header::CONTENT_TYPE, mime),
                (header::CACHE_CONTROL, "private, no-cache"),
            ],
            data,
        )
            .into_response()
    }
}

#[path = "../src/download_jobs.rs"]
mod download_jobs;
#[path = "../src/download_profile.rs"]
mod download_profile;
#[path = "../src/library_views.rs"]
mod library_views;
#[path = "../src/offline_metadata.rs"]
mod offline_metadata;
#[path = "../src/api/player.rs"]
mod player_api;
#[path = "../src/shuffle.rs"]
mod shuffle;
#[path = "../src/stats_store.rs"]
mod stats_store;
#[path = "../src/user_data.rs"]
mod user_data;
#[path = "../src/api/user_data.rs"]
mod user_data_api;
#[path = "../src/api/views.rs"]
mod views_api;

async fn fixture_auth(mut request: Request, next: Next) -> Response {
    let user_id = match request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        Some("Bearer alice") => "alice",
        Some("Bearer bob") => "bob",
        _ => return StatusCode::UNAUTHORIZED.into_response(),
    };
    request.extensions_mut().insert(state::AuthContext {
        user: state::AuthUser { id: user_id.into() },
    });
    next.run(request).await
}

#[tokio::test]
async fn shutdown_rejects_a_new_playlist_without_writing_data() {
    let db = Arc::new(
        redb::Database::builder()
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .unwrap(),
    );
    let store = user_data::UserDataStore::new(db);
    store.init_tables().unwrap();
    let shutdown = shutdown::Shutdown::new();
    shutdown.request_stop();
    let result = user_data_api::create_playlist(
        axum::extract::State(state::AppState {
            user_data: store.clone(),
            shutdown,
        }),
        axum::Extension(state::AuthContext {
            user: state::AuthUser { id: "alice".into() },
        }),
        Json(state::CreatePlaylistRequest {
            name: "discarded".into(),
            description: None,
            track_ids: vec![],
        }),
    )
    .await;
    assert_eq!(result.unwrap_err().0, StatusCode::SERVICE_UNAVAILABLE);
    assert!(store.list_playlists("alice").unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_admitted_playlist_write_survives_dropping_its_request_during_shutdown() {
    let db = Arc::new(
        redb::Database::builder()
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .unwrap(),
    );
    let store = user_data::UserDataStore::new(db.clone());
    store.init_tables().unwrap();
    let held = library::begin_write(&db).unwrap();
    let mut subscription = store.subscribe_collections("alice");
    let shutdown = shutdown::Shutdown::new();
    let state = state::AppState {
        user_data: store.clone(),
        shutdown: shutdown.clone(),
    };
    let request = tokio::spawn(user_data_api::create_playlist(
        axum::extract::State(state),
        axum::Extension(state::AuthContext {
            user: state::AuthUser { id: "alice".into() },
        }),
        Json(state::CreatePlaylistRequest {
            name: "committed".into(),
            description: None,
            track_ids: vec!["track".into()],
        }),
    ));
    let admitted = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !shutdown
            .pending_jobs()
            .iter()
            .any(|job| job.name == "playlist_create")
        {
            tokio::task::yield_now().await;
        }
    })
    .await;
    // Always release the writer, including if an admission assertion failed.
    request.abort();
    let _ = request.await;
    shutdown.cancel();
    drop(held);
    admitted.unwrap();
    assert!(shutdown.drain(std::time::Duration::from_secs(2)).await);
    let saved = store.list_playlists("alice").unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].name, "committed");
    assert_eq!(saved[0].track_ids, ["track"]);
    assert_eq!(subscription.receiver.try_recv().unwrap(), 1);
    assert_eq!(store.collection_snapshot("alice").unwrap().revision, 1);
}

struct Fixture {
    app: Option<Router>,
    store: Option<user_data::UserDataStore>,
    path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "phonolite-api-isolation-{}.redb",
            uuid::Uuid::new_v4()
        ));
        let store = user_data::UserDataStore::new(Arc::new(redb::Database::create(&path).unwrap()));
        store.init_tables().unwrap();
        let app = Router::new()
            .route(
                "/playlists",
                get(user_data_api::list_playlists).post(user_data_api::create_playlist),
            )
            .route(
                "/playlists/:id",
                post(user_data_api::update_playlist).delete(user_data_api::delete_playlist),
            )
            .route(
                "/playlists/:id/tracks",
                post(user_data_api::add_playlist_tracks)
                    .delete(user_data_api::remove_playlist_tracks),
            )
            .route(
                "/playlists/:id/cover",
                get(user_data_api::get_playlist_cover)
                    .put(user_data_api::upload_playlist_cover)
                    .delete(user_data_api::delete_playlist_cover),
            )
            .route("/likes/batch", post(user_data_api::batch_update_likes))
            .route(
                "/likes/:id",
                post(user_data_api::add_like).delete(user_data_api::remove_like),
            )
            .route(
                "/settings",
                get(player_api::get_playback_settings).post(player_api::update_playback_settings),
            )
            .layer(middleware::from_fn(fixture_auth))
            .with_state(state::AppState {
                shutdown: Default::default(),
                user_data: store.clone(),
            });
        Self {
            app: Some(app),
            store: Some(store),
            path,
        }
    }

    fn store(&self) -> &user_data::UserDataStore {
        self.store.as_ref().unwrap()
    }

    fn like_state(&self, user: &str, track_id: &str) -> Option<user_data::LikeState> {
        self.store()
            .list_like_states(user)
            .unwrap()
            .into_iter()
            .find(|(id, _)| id == track_id)
            .map(|(_, state)| state)
    }

    async fn request(
        &self,
        user: Option<&str>,
        method: Method,
        uri: &str,
        content_type: &str,
        body: impl Into<Body>,
    ) -> Response {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, content_type);
        if let Some(user) = user {
            request = request.header(header::AUTHORIZATION, format!("Bearer {user}"));
        }
        // Caller-controlled identity hints must not override AuthContext.
        request = request.header("x-user-id", "bob");
        self.app
            .as_ref()
            .unwrap()
            .clone()
            .oneshot(request.body(body.into()).unwrap())
            .await
            .unwrap()
    }

    async fn json(
        &self,
        user: &str,
        method: Method,
        uri: &str,
        value: Value,
    ) -> (StatusCode, Value) {
        let response = self
            .request(
                Some(user),
                method,
                uri,
                "application/json",
                value.to_string(),
            )
            .await;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn create(&self, user: &str, name: &str) -> String {
        let (status, value) = self
            .json(
                user,
                Method::POST,
                "/playlists",
                json!({
                    "name":name, "track_ids":["track-a"], "description":"private",
                    "user_id":"bob", "owner_id":"bob"
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{value}");
        value["id"].as_str().unwrap().to_string()
    }

    async fn playlists(&self, user: &str) -> Vec<Value> {
        let (status, value) = self
            .json(user, Method::GET, "/playlists", Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK);
        value.as_array().unwrap().clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        drop(self.app.take());
        drop(self.store.take());
        std::fs::remove_file(&self.path).unwrap();
    }
}

#[tokio::test]
async fn playlist_crud_uses_authenticated_owner_and_hides_foreign_ids() {
    let fixture = Fixture::new();
    let alice_id = fixture.create("alice", "Alice playlist").await;
    let bob_id = fixture.create("bob", "Bob playlist").await;
    assert_eq!(fixture.playlists("alice").await[0]["id"], alice_id);
    assert_eq!(fixture.playlists("bob").await[0]["id"], bob_id);
    assert_eq!(fixture.playlists("alice").await.len(), 1);
    assert_eq!(fixture.playlists("bob").await.len(), 1);

    let uri = format!("/playlists/{alice_id}");
    let update = json!({"name":"stolen", "track_ids":["foreign"], "description":"changed", "user_id":"alice"});
    assert_eq!(
        fixture.json("bob", Method::POST, &uri, update).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        fixture
            .json("bob", Method::DELETE, &uri, Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let alice = fixture.playlists("alice").await;
    assert_eq!(alice[0]["name"], "Alice playlist");
    assert_eq!(alice[0]["track_ids"], json!(["track-a"]));
    assert_eq!(alice[0]["description"], "private");

    let (status, updated) = fixture
        .json(
            "alice",
            Method::POST,
            &uri,
            json!({"name":"renamed", "track_ids":["track-b"], "description":"edited", "expected_revision":alice[0]["revision"]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["name"], "renamed");
    assert_eq!(updated["track_ids"], json!(["track-b"]));
    assert_eq!(updated["description"], "edited");
    assert_eq!(
        fixture
            .json("alice", Method::DELETE, &uri, Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    assert!(fixture.playlists("alice").await.is_empty());
    assert_eq!(fixture.playlists("bob").await[0]["id"], bob_id);
}

#[tokio::test]
async fn playlist_track_routes_merge_additions_and_require_a_current_version_for_replacement() {
    let fixture = Fixture::new();
    let id = fixture.create("alice", "Shared devices").await;
    let uri = format!("/playlists/{id}");
    let tracks_uri = format!("{uri}/tracks");
    let initial = fixture.playlists("alice").await[0]["revision"].clone();
    for track in ["phone", "desktop"] {
        let (status, playlist) = fixture
            .json(
                "alice",
                Method::POST,
                &tracks_uri,
                json!({"track_ids":[track]}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{playlist}");
    }
    let revision = fixture.store().collection_revision("alice").unwrap();
    assert_eq!(
        fixture
            .json(
                "alice",
                Method::POST,
                &tracks_uri,
                json!({"track_ids":["phone"]})
            )
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        fixture.store().collection_revision("alice").unwrap(),
        revision
    );
    assert_eq!(
        fixture
            .json(
                "bob",
                Method::POST,
                &tracks_uri,
                json!({"track_ids":["foreign"]})
            )
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        fixture
            .json("alice", Method::POST, &uri, json!({"track_ids":[]}))
            .await
            .0,
        StatusCode::PRECONDITION_REQUIRED
    );
    assert_eq!(
        fixture
            .json(
                "alice",
                Method::POST,
                &uri,
                json!({"track_ids":[], "expected_revision":initial})
            )
            .await
            .0,
        StatusCode::CONFLICT
    );
    let current = fixture.playlists("alice").await;
    assert_eq!(
        current[0]["track_ids"],
        json!(["track-a", "phone", "desktop"])
    );
    let (status, playlist) = fixture
        .json(
            "alice",
            Method::DELETE,
            &tracks_uri,
            json!({"track_ids":["phone"]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(playlist["track_ids"], json!(["track-a", "desktop"]));
    assert_eq!(fixture.json("alice", Method::POST, &uri, json!({"track_ids":["desktop", "track-a"], "expected_revision":playlist["revision"]})).await.0, StatusCode::OK);
}

#[tokio::test]
async fn foreign_playlist_cover_reads_replacements_and_deletes_are_rejected() {
    let fixture = Fixture::new();
    let alice_id = fixture.create("alice", "Alice artwork").await;
    let uri = format!("/playlists/{alice_id}/cover");
    let image = Bytes::from_static(b"alice-image");
    assert_eq!(
        fixture
            .request(Some("alice"), Method::PUT, &uri, "image/png", image.clone())
            .await
            .status(),
        StatusCode::OK
    );

    for method in [Method::GET, Method::PUT, Method::DELETE] {
        assert_eq!(
            fixture
                .request(
                    Some("bob"),
                    method,
                    &uri,
                    "image/png",
                    Bytes::from_static(b"bob-image")
                )
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    let response = fixture
        .request(Some("alice"), Method::GET, &uri, "image/png", Body::empty())
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        image
    );
    assert_eq!(
        fixture
            .json("alice", Method::DELETE, &uri, Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        fixture
            .request(Some("alice"), Method::GET, &uri, "image/png", Body::empty())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );

    // Deleting a playlist also removes its owner's image and leaves Bob's intact.
    let bob_id = fixture.create("bob", "Bob artwork").await;
    let bob_uri = format!("/playlists/{bob_id}/cover");
    assert_eq!(
        fixture
            .request(Some("bob"), Method::PUT, &bob_uri, "image/png", "bob-image")
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        fixture
            .request(Some("alice"), Method::PUT, &uri, "image/png", "replacement")
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        fixture
            .json(
                "alice",
                Method::DELETE,
                &format!("/playlists/{alice_id}"),
                Value::Null
            )
            .await
            .0,
        StatusCode::OK
    );
    assert!(fixture
        .store()
        .get_playlist_image("alice", &alice_id)
        .unwrap()
        .is_none());
    assert_eq!(
        fixture
            .request(
                Some("bob"),
                Method::GET,
                &bob_uri,
                "image/png",
                Body::empty()
            )
            .await
            .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn single_and_batch_likes_and_timestamps_are_account_scoped() {
    let fixture = Fixture::new();
    for user in ["alice", "bob"] {
        assert_eq!(
            fixture
                .json(user, Method::POST, "/likes/shared-track", Value::Null)
                .await
                .0,
            StatusCode::OK
        );
    }
    assert_eq!(
        fixture
            .json("alice", Method::DELETE, "/likes/shared-track", Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    assert!(fixture.store().list_likes("alice").unwrap().is_empty());
    assert_eq!(
        fixture.store().list_likes("bob").unwrap(),
        vec!["shared-track"]
    );

    let (status, batch) = fixture.json("alice", Method::POST, "/likes/batch", json!({
        "user_id":"bob", "items":[
            {"track_id":" alice-only ", "liked":true, "updated_at":9000000000000000_u64, "user_id":"bob"},
            {"track_id":"shared-track", "liked":false},
            {"track_id":"   ", "liked":true}
        ]
    })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(batch.as_array().unwrap().len(), 2);
    assert_eq!(batch[0]["track_id"], "alice-only");
    assert_eq!(
        fixture.store().list_likes("alice").unwrap(),
        vec!["alice-only"]
    );
    assert_eq!(
        fixture.store().list_likes("bob").unwrap(),
        vec!["shared-track"]
    );
    let bob_state = fixture.like_state("bob", "shared-track").unwrap();

    assert_eq!(fixture.json("alice", Method::POST, "/likes/batch", json!({"items":[{"track_id":"shared-track", "liked":true, "updated_at":9000000000000001_u64}]})).await.0, StatusCode::OK);
    assert_eq!(
        fixture
            .like_state("bob", "shared-track")
            .unwrap()
            .updated_at,
        bob_state.updated_at
    );
    assert!(fixture.like_state("bob", "alice-only").is_none());
    assert_eq!(
        fixture
            .json("bob", Method::DELETE, "/likes/shared-track", Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    assert!(fixture.store().list_likes("bob").unwrap().is_empty());
    assert!(fixture.like_state("alice", "shared-track").unwrap().liked);
}

#[tokio::test]
async fn repeat_settings_use_auth_context_and_reject_invalid_values() {
    let fixture = Fixture::new();
    for user in ["alice", "bob"] {
        let (status, value) = fixture
            .json(user, Method::GET, "/settings", Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["repeat_mode"], "off");
    }
    assert_eq!(
        fixture
            .json(
                "alice",
                Method::POST,
                "/settings",
                json!({"repeat_mode":"one", "user_id":"bob"})
            )
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        fixture
            .json("bob", Method::GET, "/settings", Value::Null)
            .await
            .1["repeat_mode"],
        "off"
    );
    assert_eq!(
        fixture
            .json(
                "bob",
                Method::POST,
                "/settings",
                json!({"repeat_mode":"off", "user_id":"alice"})
            )
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        fixture
            .json("alice", Method::GET, "/settings", Value::Null)
            .await
            .1["repeat_mode"],
        "one"
    );
    assert_eq!(
        fixture
            .json(
                "alice",
                Method::POST,
                "/settings",
                json!({"repeat_mode":"all"})
            )
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        fixture
            .json("alice", Method::GET, "/settings", Value::Null)
            .await
            .1["repeat_mode"],
        "one"
    );
}

#[tokio::test]
async fn fixture_requires_an_authenticated_identity_before_personalization_handlers() {
    let fixture = Fixture::new();
    let alice_id = fixture.create("alice", "Private").await;
    for (method, uri, content_type, body) in [
        (
            Method::GET,
            "/playlists".to_string(),
            "application/json",
            "null",
        ),
        (
            Method::POST,
            "/playlists".to_string(),
            "application/json",
            r#"{"name":"anonymous"}"#,
        ),
        (
            Method::POST,
            format!("/playlists/{alice_id}"),
            "application/json",
            r#"{"name":"anonymous"}"#,
        ),
        (
            Method::DELETE,
            format!("/playlists/{alice_id}"),
            "application/json",
            "null",
        ),
        (
            Method::GET,
            format!("/playlists/{alice_id}/cover"),
            "image/png",
            "image",
        ),
        (
            Method::PUT,
            format!("/playlists/{alice_id}/cover"),
            "image/png",
            "image",
        ),
        (
            Method::DELETE,
            format!("/playlists/{alice_id}/cover"),
            "application/json",
            "null",
        ),
        (
            Method::POST,
            "/likes/track".to_string(),
            "application/json",
            "null",
        ),
        (
            Method::DELETE,
            "/likes/track".to_string(),
            "application/json",
            "null",
        ),
        (
            Method::POST,
            "/likes/batch".to_string(),
            "application/json",
            r#"{"items":[{"track_id":"track","liked":true}]}"#,
        ),
        (
            Method::GET,
            "/settings".to_string(),
            "application/json",
            "null",
        ),
        (
            Method::POST,
            "/settings".to_string(),
            "application/json",
            r#"{"repeat_mode":"one"}"#,
        ),
    ] {
        assert_eq!(
            fixture
                .request(None, method, &uri, content_type, body)
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(fixture.playlists("alice").await[0]["name"], "Private");
    assert!(fixture.store().list_likes("alice").unwrap().is_empty());
    assert!(fixture
        .store()
        .get_playback_settings("alice")
        .unwrap()
        .is_none());
}

#[test]
fn catalog_flags_liked_shuffle_and_offline_metadata_use_the_same_user_scope() {
    let fixture = Fixture::new();
    let directory = std::env::temp_dir().join(format!(
        "phonolite-catalog-isolation-{}",
        uuid::Uuid::new_v4()
    ));
    let music = directory.join("music");
    let album = music.join("Artist").join("Album");
    std::fs::create_dir_all(&album).unwrap();
    // The real library scanner's filename fallback is enough: no decoding runs.
    for name in ["First.mp3", "Second.mp3"] {
        std::fs::write(album.join(name), b"catalog isolation fixture").unwrap();
    }
    {
        let (library, _) = library::Library::load_or_scan(
            vec![library::LibraryRoot::new(String::new(), music)],
            directory.join("library.redb"),
        )
        .unwrap();
        let tracks = library.list_tracks(None, 10, 0).unwrap().0;
        assert_eq!(tracks.len(), 2);
        let first_id = tracks[0].id.clone();
        let second_id = tracks[1].id.clone();
        fixture.store().add_like("alice", &first_id).unwrap();
        fixture.store().add_like("bob", &second_id).unwrap();
        fixture
            .store()
            .create_playlist("alice", "Alice".into(), None, vec![second_id.clone()])
            .unwrap();
        fixture
            .store()
            .create_playlist("bob", "Bob".into(), None, vec![first_id.clone()])
            .unwrap();
        let state = state::AppState {
            shutdown: Default::default(),
            user_data: fixture.store().clone(),
        };

        for (user, liked_id, playlist_id) in [
            ("alice", &first_id, &second_id),
            ("bob", &second_id, &first_id),
        ] {
            let liked = views_api::liked_set(&state, user).unwrap();
            let playlists = views_api::playlist_set(&state, user).unwrap();
            assert_eq!(liked, [liked_id.clone()].into_iter().collect());
            assert_eq!(playlists, [playlist_id.clone()].into_iter().collect());
            let views =
                library_views::build_track_views(&library, &tracks, &liked, &playlists).unwrap();
            for view in views {
                assert_eq!(view.liked, &view.id == liked_id);
                assert_eq!(view.in_playlists, &view.id == playlist_id);
            }
            let queue = shuffle::build_shuffle_queue(
                &library,
                shuffle::ShuffleMode::Liked,
                None,
                None,
                &[],
                &[],
                &liked,
            )
            .unwrap();
            assert_eq!(
                queue.iter().map(|track| &track.id).collect::<Vec<_>>(),
                vec![liked_id]
            );
            for track in &tracks {
                let metadata = offline_metadata::offline_metadata_for_track(
                    &library, track, &liked, &playlists,
                )
                .unwrap();
                assert_eq!(metadata.track.liked, &track.id == liked_id);
                assert_eq!(metadata.track.in_playlists, &track.id == playlist_id);
            }
        }
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn statistics_keep_same_device_uploads_snapshots_preferences_and_resets_separate() {
    use sha2::{Digest, Sha256};
    use stats_store::{CounterBatch, DevicePreference, StatsError, StatsStore};

    let path = std::env::temp_dir().join(format!(
        "phonolite-statistics-isolation-{}.redb",
        uuid::Uuid::new_v4()
    ));
    {
        let store = StatsStore::new(Arc::new(redb::Database::create(&path).unwrap()));
        store.init_tables().unwrap();
        for user in ["alice", "bob"] {
            store.activate_counters(user, "same-device", 1).unwrap();
        }
        let metadata = json!({"track_id":"track", "title":"Song", "artist_id":"artist", "artist_name":"Artist", "album_id":"album", "album_title":"Album", "genres":[], "artists":[], "track_duration_ms":180000});
        let id = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&metadata).unwrap())
        );
        let batch = |sequence: u64, milliseconds: u64| -> CounterBatch {
            serde_json::from_value(json!({
                "device_id":"same-device", "history_generation":1, "collection_revision":0,
                "batch_sequence":sequence, "rule_version":3,
                "metadata":{(id.clone()):metadata.clone()},
                "contributions":[{"year":2026, "month":10, "metadata_id":id.clone(), "listened_ms":milliseconds, "plays":1, "genre_ms":{}}]
            })).unwrap()
        };
        store.ingest_counters("alice", &batch(1, 15001)).unwrap();
        assert!(store.get_period("bob", 2026, Some(10)).unwrap().is_none());
        store.ingest_counters("bob", &batch(1, 9001)).unwrap();
        assert_eq!(
            store
                .get_period("alice", 2026, Some(10))
                .unwrap()
                .unwrap()
                .total_ms,
            15001
        );
        assert_eq!(
            store
                .get_period("bob", 2026, Some(10))
                .unwrap()
                .unwrap()
                .total_ms,
            9001
        );
        store
            .set_device_preference(
                "alice",
                DevicePreference {
                    device_id: "same-device".into(),
                    enabled: false,
                    revision: 1,
                },
            )
            .unwrap();
        assert!(matches!(
            store.ingest_counters("alice", &batch(2, 1000)),
            Err(StatsError::DeviceDisabled)
        ));
        store.ingest_counters("bob", &batch(2, 2000)).unwrap();
        let alice = store
            .counter_snapshot("alice", "same-device", 2026, 10, None, "", 200)
            .unwrap();
        let bob = store
            .counter_snapshot("bob", "same-device", 2026, 10, None, "", 200)
            .unwrap();
        assert_eq!(alice.contributions[0].listened_ms, 15001);
        assert_eq!(bob.contributions[0].listened_ms, 11001);
        store.reset_history("alice", 1).unwrap();
        assert!(store.get_period("alice", 2026, Some(10)).unwrap().is_none());
        assert_eq!(
            store
                .get_period("bob", 2026, Some(10))
                .unwrap()
                .unwrap()
                .total_ms,
            11001
        );
    }
    std::fs::remove_file(path).unwrap();
}
