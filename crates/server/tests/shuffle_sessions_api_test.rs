//! Listener-free requests against the production paged-shuffle API and store.
//! Authentication supplies two fixed server identities; it does not replace
//! production authentication tests. Library/catalog/personalization are real.
#![allow(dead_code, unused_imports)]

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::extract::Request;
use axum::http::{header, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tower::ServiceExt;

mod state {
    use axum::http::StatusCode;
    use axum::Json;
    use serde::{Deserialize, Serialize};

    #[derive(Clone)]
    pub struct AppState {
        pub shutdown: crate::shutdown::Shutdown,
        pub user_data: crate::user_data::UserDataStore,
        pub shuffle_sessions: crate::shuffle_sessions::ShuffleSessionStore,
        pub metadata_events: crate::metadata_events::MetadataEventBus,
        pub library: library::Library,
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
    ) -> (StatusCode, Json<state::ErrorResponse>) {
        (
            status,
            Json(state::ErrorResponse {
                error: message.into(),
            }),
        )
    }
}

#[path = "../src/library_views.rs"]
mod library_views;
#[path = "../src/metadata_events.rs"]
mod metadata_events;
#[path = "../src/offline_metadata.rs"]
mod offline_metadata;
#[path = "../src/shuffle.rs"]
mod shuffle;
#[path = "../src/shuffle_sessions.rs"]
mod shuffle_sessions;
#[path = "../src/api/shuffle_sessions.rs"]
mod shuffle_sessions_api;
#[path = "../src/shutdown.rs"]
mod shutdown;
#[path = "../src/user_data.rs"]
mod user_data;
#[path = "../src/api/views.rs"]
mod views;

fn library_or_json_error(
    state: &state::AppState,
) -> Result<library::Library, (StatusCode, Json<state::ErrorResponse>)> {
    Ok(state.library.clone())
}

async fn fixture_auth(mut request: Request, next: Next) -> Response {
    let user = match request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        Some("Bearer alice") => "alice",
        Some("Bearer bob") => "bob",
        _ => return StatusCode::UNAUTHORIZED.into_response(),
    };
    request.extensions_mut().insert(state::AuthContext {
        user: state::AuthUser { id: user.into() },
    });
    next.run(request).await
}

struct Fixture {
    app: Option<Router>,
    state: Option<state::AppState>,
    directory: PathBuf,
    tracks: Vec<common::Track>,
}

impl Fixture {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("phonolite-shuffle-api-{}", uuid::Uuid::new_v4()));
        let music = directory.join("music");
        let album = music.join("Artist").join("Album");
        std::fs::create_dir_all(&album).unwrap();
        for number in 0..27 {
            std::fs::write(
                album.join(format!("Track {number:02}.mp3")),
                b"shuffle session fixture",
            )
            .unwrap();
        }
        let (library, _) = library::Library::load_or_scan(
            vec![library::LibraryRoot::new(String::new(), music)],
            directory.join("library.redb"),
        )
        .unwrap();
        let tracks = library.list_tracks(None, 100, 0).unwrap().0;
        assert_eq!(tracks.len(), 27);
        let db = Arc::new(redb::Database::create(directory.join("personalization.redb")).unwrap());
        let user_data = user_data::UserDataStore::new(db);
        user_data.init_tables().unwrap();
        let state = state::AppState {
            shutdown: Default::default(),
            user_data,
            shuffle_sessions: shuffle_sessions::ShuffleSessionStore::new(),
            metadata_events: metadata_events::MetadataEventBus::new(8),
            library,
        };
        let app = Router::new()
            .route(
                "/library/shuffle/sessions",
                post(shuffle_sessions_api::create_session),
            )
            .route(
                "/library/shuffle/sessions/:id",
                get(shuffle_sessions_api::get_page).delete(shuffle_sessions_api::delete_session),
            )
            .layer(middleware::from_fn(fixture_auth))
            .with_state(state.clone());
        Self {
            app: Some(app),
            state: Some(state),
            directory,
            tracks,
        }
    }

    fn state(&self) -> &state::AppState {
        self.state.as_ref().unwrap()
    }

    async fn request(
        &self,
        user: Option<&str>,
        method: Method,
        uri: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-user-id", "bob");
        if let Some(user) = user {
            request = request.header(header::AUTHORIZATION, format!("Bearer {user}"));
        }
        let response = self
            .app
            .as_ref()
            .unwrap()
            .clone()
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn create(&self, user: &str, body: Value) -> Value {
        let (status, value) = self
            .request(Some(user), Method::POST, "/library/shuffle/sessions", body)
            .await;
        assert_eq!(status, StatusCode::OK, "{value}");
        value
    }

    async fn page(
        &self,
        user: &str,
        session_id: &str,
        client_id: &str,
        offset: usize,
        limit: usize,
    ) -> (StatusCode, Value) {
        self.request(Some(user), Method::GET, &format!("/library/shuffle/sessions/{session_id}?client_id={client_id}&offset={offset}&limit={limit}"), Value::Null).await
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        drop(self.app.take());
        drop(self.state.take());
        std::fs::remove_dir_all(&self.directory).unwrap();
    }
}

fn item_ids(value: &Value) -> Vec<String> {
    value["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn shutdown_rejects_new_shuffle_work_and_pages() {
    let fixture = Fixture::new();
    let first = fixture
        .create(
            "alice",
            json!({"mode":"all", "request_id":"before-stop", "client_id":"phone"}),
        )
        .await;
    fixture.state().shutdown.cancel();
    assert_eq!(
        fixture
            .request(
                Some("alice"),
                Method::POST,
                "/library/shuffle/sessions",
                json!({"mode":"all", "request_id":"after-stop", "client_id":"phone"})
            )
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        fixture
            .page(
                "alice",
                first["session_id"].as_str().unwrap(),
                "phone",
                10,
                10
            )
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(
        fixture
            .state()
            .shutdown
            .drain(std::time::Duration::from_secs(1))
            .await
    );
}

#[tokio::test]
async fn every_catalog_rule_uses_paged_decks_and_preserves_its_scope() {
    let fixture = Fixture::new();
    let library = &fixture.state().library;
    let other_path = library.roots()[0].path.join("Other/Other Album/Other.mp3");
    std::fs::create_dir_all(other_path.parent().unwrap()).unwrap();
    std::fs::write(other_path, b"other artist fixture").unwrap();
    library.incremental_scan().unwrap();
    let artist_id = &fixture.tracks[0].artist_id;
    let album_id = &fixture.tracks[0].album_id;
    let other = library
        .list_tracks(None, 100, 0)
        .unwrap()
        .0
        .into_iter()
        .find(|track| &track.artist_id != artist_id)
        .unwrap();
    library
        .update_artist_enrichment(&other.artist_id, None, &["Jazz".into()], None, None, false)
        .unwrap();
    fixture
        .state()
        .metadata_events
        .emit_artist(&other.artist_id);
    let expected = fixture
        .tracks
        .iter()
        .map(|track| track.id.clone())
        .collect::<HashSet<_>>();
    for mut body in [
        json!({"mode":"artist", "artist_id":artist_id}),
        json!({"mode":"album", "album_id":album_id}),
        json!({"mode":"custom", "artist_ids":[artist_id]}),
    ] {
        body["request_id"] = json!(format!("scope-{}", body["mode"].as_str().unwrap()));
        body["client_id"] = json!("phone");
        let first = fixture.create("alice", body).await;
        assert_eq!(first["total"], 27);
        assert_eq!(first["items"].as_array().unwrap().len(), 10);
        let mut actual = item_ids(&first);
        for offset in [10, 20] {
            let (status, page) = fixture
                .page(
                    "alice",
                    first["session_id"].as_str().unwrap(),
                    "phone",
                    offset,
                    10,
                )
                .await;
            assert_eq!(status, StatusCode::OK);
            actual.extend(item_ids(&page));
        }
        assert_eq!(actual.len(), expected.len());
        assert_eq!(actual.into_iter().collect::<HashSet<_>>(), expected);
    }
    let jazz = fixture.create("alice", json!({"mode":"custom", "genres":[" jazz "], "request_id":"genre-scope", "client_id":"phone"})).await;
    assert_eq!(item_ids(&jazz), vec![other.id]);
    assert_eq!(jazz["total"], 1);
    assert_eq!(jazz["has_more"], false);
}

#[tokio::test]
async fn retries_and_default_pages_keep_one_order_without_repeats_and_reject_invalid_limits() {
    let fixture = Fixture::new();
    let body = json!({"mode":"all", "request_id":"retry", "client_id":"phone"});
    let first = fixture.create("alice", body.clone()).await;
    let session_id = first["session_id"].as_str().unwrap();
    assert_eq!(first["total"], 27);
    assert_eq!(first["offset"], 0);
    assert_eq!(first["next_offset"], 10);
    assert_eq!(first["has_more"], true);
    assert_eq!(item_ids(&first).len(), 10);
    let retry = fixture.create("alice", body).await;
    assert_eq!(retry, first);
    let mut ordered = item_ids(&first);
    for offset in [10, 20] {
        let (status, value) = fixture.page("alice", session_id, "phone", offset, 10).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(value["offset"], offset);
        assert!(item_ids(&value).len() <= 10);
        let retry = fixture.page("alice", session_id, "phone", offset, 10).await;
        assert_eq!(retry, (status, value.clone()));
        ordered.extend(item_ids(&value));
        assert_eq!(value["next_offset"], std::cmp::min(offset + 10, 27));
        assert_eq!(value["has_more"], offset + 10 < 27);
    }
    assert_eq!(ordered.len(), 27);
    assert_eq!(ordered.iter().collect::<HashSet<_>>().len(), 27);
    assert_eq!(
        ordered.into_iter().collect::<HashSet<_>>(),
        fixture
            .tracks
            .iter()
            .map(|track| track.id.clone())
            .collect()
    );
    for limit in [0, 100] {
        assert_eq!(
            fixture.page("alice", session_id, "phone", 0, limit).await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(fixture.request(Some("alice"), Method::POST, "/library/shuffle/sessions", json!({"mode":"all", "request_id":format!("invalid-limit-{limit}"), "client_id":"phone", "limit":limit})).await.0, StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn sessions_are_bound_to_authenticated_user_and_client_and_can_be_released() {
    let fixture = Fixture::new();
    let body = json!({"mode":"all", "request_id":"same", "client_id":"phone", "limit":10, "user_id":"bob", "owner_id":"bob"});
    let alice = fixture.create("alice", body.clone()).await;
    let bob = fixture.create("bob", body).await;
    let alice_id = alice["session_id"].as_str().unwrap();
    assert_ne!(alice["session_id"], bob["session_id"]);
    assert_eq!(
        fixture.page("bob", alice_id, "phone", 0, 10).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        fixture.page("alice", alice_id, "laptop", 0, 10).await.0,
        StatusCode::NOT_FOUND
    );
    let uri = format!("/library/shuffle/sessions/{alice_id}?client_id=phone");
    assert_eq!(
        fixture
            .request(Some("bob"), Method::DELETE, &uri, Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        fixture.page("alice", alice_id, "phone", 0, 10).await.0,
        StatusCode::OK
    );
    let status = fixture
        .request(Some("alice"), Method::DELETE, &uri, Value::Null)
        .await
        .0;
    assert!(status.is_success());
    assert_eq!(
        fixture.page("alice", alice_id, "phone", 0, 10).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        fixture
            .page("bob", bob["session_id"].as_str().unwrap(), "phone", 0, 10)
            .await
            .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn liked_and_playlist_sessions_only_use_the_authenticated_accounts_collections() {
    let fixture = Fixture::new();
    let alice_track = &fixture.tracks[0].id;
    let bob_track = &fixture.tracks[1].id;
    fixture
        .state()
        .user_data
        .add_like("alice", alice_track)
        .unwrap();
    fixture
        .state()
        .user_data
        .add_like("bob", bob_track)
        .unwrap();
    let alice_playlist = fixture
        .state()
        .user_data
        .create_playlist("alice", "Alice".into(), None, vec![alice_track.clone()])
        .unwrap();
    let bob_playlist = fixture
        .state()
        .user_data
        .create_playlist("bob", "Bob".into(), None, vec![bob_track.clone()])
        .unwrap();
    for (user, id) in [("alice", alice_track), ("bob", bob_track)] {
        let value = fixture.create(user, json!({"mode":"liked", "request_id":"likes", "client_id":"phone", "limit":10, "user_id":"bob"})).await;
        assert_eq!(item_ids(&value), vec![id.clone()]);
        assert_eq!(value["items"][0]["liked"], true);
        assert_eq!(value["items"][0]["in_playlists"], true);
    }
    for (user, playlist, id) in [
        ("alice", &alice_playlist, alice_track),
        ("bob", &bob_playlist, bob_track),
    ] {
        let value = fixture.create(user, json!({"mode":"playlist", "request_id":"playlist", "client_id":"phone", "playlist_id":playlist.id, "limit":10})).await;
        assert_eq!(item_ids(&value), vec![id.clone()]);
    }
    for (request_id, playlist_id) in [
        ("foreign", bob_playlist.id.as_str()),
        ("missing", "nonexistent"),
    ] {
        assert_eq!(fixture.request(Some("alice"), Method::POST, "/library/shuffle/sessions", json!({"mode":"playlist", "request_id":request_id, "client_id":"phone", "playlist_id":playlist_id})).await.0, StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn playlist_create_retries_keep_the_deck_after_the_original_playlist_is_deleted() {
    let fixture = Fixture::new();
    let playlist = fixture
        .state()
        .user_data
        .create_playlist(
            "alice",
            "Snapshot".into(),
            None,
            vec![fixture.tracks[0].id.clone()],
        )
        .unwrap();
    let body = json!({"mode":"playlist", "request_id":"retry-deleted-playlist", "client_id":"phone", "playlist_id":playlist.id, "limit":10});
    let before_delete = fixture.create("alice", body.clone()).await;
    fixture
        .state()
        .user_data
        .delete_playlist("alice", &playlist.id)
        .unwrap();
    let retry = fixture.create("alice", body).await;
    assert_eq!(retry["session_id"], before_delete["session_id"]);
    assert_eq!(item_ids(&retry), item_ids(&before_delete));
    assert_eq!(retry["items"][0]["in_playlists"], false);
}

#[tokio::test]
async fn deleted_tracks_leave_stable_offsets_and_pages_refresh_account_flags() {
    let fixture = Fixture::new();
    let first = fixture
        .create(
            "alice",
            json!({"mode":"all", "request_id":"missing-track", "client_id":"phone", "limit":10}),
        )
        .await;
    let session_id = first["session_id"].as_str().unwrap();
    let ids = item_ids(&first);
    let deleted = fixture
        .tracks
        .iter()
        .find(|track| track.id == ids[0])
        .unwrap();
    std::fs::remove_file(
        fixture
            .state()
            .library
            .resolve_relpath(&deleted.file_relpath)
            .unwrap(),
    )
    .unwrap();
    fixture.state().library.incremental_scan().unwrap();
    fixture
        .state()
        .user_data
        .add_like("alice", &ids[1])
        .unwrap();
    fixture
        .state()
        .user_data
        .create_playlist("bob", "Bob only".into(), None, vec![ids[1].clone()])
        .unwrap();
    let (status, page) = fixture.page("alice", session_id, "phone", 0, 10).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["total"], 27);
    assert_eq!(page["next_offset"], 10);
    assert_eq!(page["has_more"], true);
    assert_eq!(item_ids(&page), ids[1..].to_vec());
    assert_eq!(page["item_offsets"], json!([1, 2, 3, 4, 5, 6, 7, 8, 9]));
    let item = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == ids[1])
        .unwrap();
    assert_eq!(item["liked"], true);
    assert_eq!(item["in_playlists"], false);

    // Scans mutate the same Library clone held by AppState. No metadata event
    // is emitted here: catalog generation alone must expire shared candidates.
    let after_delete = fixture
        .create(
            "alice",
            json!({"mode":"all", "request_id":"after-delete", "client_id":"phone"}),
        )
        .await;
    assert_eq!(after_delete["total"], 26);
    let new_path = fixture
        .state()
        .library
        .resolve_relpath(&deleted.file_relpath)
        .unwrap()
        .with_file_name("Added later.mp3");
    std::fs::write(new_path, b"new catalog track").unwrap();
    fixture.state().library.incremental_scan().unwrap();
    let catalog_ids: HashSet<_> = fixture
        .state()
        .library
        .list_tracks(None, 100, 0)
        .unwrap()
        .0
        .into_iter()
        .map(|track| track.id)
        .collect();
    let new_id = catalog_ids
        .iter()
        .find(|id| !fixture.tracks.iter().any(|track| &track.id == *id))
        .unwrap();
    let after_add = fixture
        .create(
            "alice",
            json!({"mode":"all", "request_id":"after-add", "client_id":"phone"}),
        )
        .await;
    assert_eq!(after_add["total"], 27);
    let new_session = after_add["session_id"].as_str().unwrap();
    let mut new_deck = item_ids(&after_add);
    for offset in [10, 20] {
        let (status, page) = fixture
            .page("alice", new_session, "phone", offset, 10)
            .await;
        assert_eq!(status, StatusCode::OK);
        new_deck.extend(item_ids(&page));
    }
    assert!(new_deck.contains(new_id));
    assert!(!new_deck.contains(&deleted.id));
    assert_eq!(new_deck.into_iter().collect::<HashSet<_>>(), catalog_ids);
}

#[tokio::test]
async fn missing_identity_is_rejected_for_create_page_and_delete() {
    let fixture = Fixture::new();
    let created = fixture
        .create(
            "alice",
            json!({"mode":"all", "request_id":"auth", "client_id":"phone"}),
        )
        .await;
    let uri = format!(
        "/library/shuffle/sessions/{}?client_id=phone",
        created["session_id"].as_str().unwrap()
    );
    for (method, route) in [
        (Method::POST, "/library/shuffle/sessions"),
        (Method::GET, uri.as_str()),
        (Method::DELETE, uri.as_str()),
    ] {
        assert_eq!(
            fixture
                .request(
                    None,
                    method,
                    route,
                    json!({"mode":"all", "request_id":"anonymous", "client_id":"phone"})
                )
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
}
