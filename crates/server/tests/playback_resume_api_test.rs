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
        pub available: std::sync::Arc<std::sync::atomic::AtomicBool>,
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
#[path = "../src/api/playback.rs"]
mod playback_api;
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
    if !state.available.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(http::json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "library unavailable",
        ));
    }
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
            available: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let app = Router::new()
            .route(
                "/account/playback",
                get(playback_api::get)
                    .post(playback_api::save)
                    .delete(playback_api::clear),
            )
            .route("/account/playback/decks/:id", get(playback_api::deck_page))
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

fn source(kind: &str, id: Option<&str>) -> Value {
    let mut source = json!({"kind":kind,"artist_ids":[],"genres":[]});
    if let Some(id) = id {
        source["id"] = id.into();
    }
    source
}
fn start(track: &str, source: Value, mode: &str, deck: Option<&str>) -> Value {
    let mut checkpoint = json!({"track_id":track,"source":source,"shuffle_mode":mode,"shuffle_context":source,"cursor":0});
    if let Some(id) = deck {
        checkpoint["deck_id"] = id.into();
    }
    json!({"expected_revision":0,"session_id":"session-phone","sequence":1,"request_id":"request-one","intent":"start","client_id":"phone","checkpoint":checkpoint})
}
async fn save(f: &Fixture, user: &str, input: Value) -> Value {
    let (status, response) = f
        .request(Some(user), Method::POST, "/account/playback", input)
        .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    response
}

#[tokio::test]
async fn checkpoints_use_atomic_compare_and_swap_retry_and_generation_fences() {
    let f = Fixture::new();
    let input = start(&f.tracks[0].id, source("library", None), "off", None);
    let first = save(&f, "alice", input.clone()).await;
    assert_eq!(first["revision"], 1);
    assert!(first["generation"].is_string());
    assert_eq!(save(&f, "alice", input.clone()).await, first);
    assert_eq!(
        f.state()
            .user_data
            .last_track("alice")
            .unwrap()
            .track_id
            .as_deref(),
        Some(f.tracks[0].id.as_str())
    );

    let mut other = start(&f.tracks[1].id, source("standalone", None), "off", None);
    other["session_id"] = "laptop".into();
    other["request_id"] = "other".into();
    assert_eq!(
        f.request(
            Some("alice"),
            Method::POST,
            "/account/playback",
            other.clone()
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    other["expected_revision"] = 1.into();
    let second = save(&f, "alice", other).await;
    assert_ne!(second["generation"], first["generation"]);

    let mut update = input;
    update["intent"] = "update".into();
    update["generation"] = first["generation"].clone();
    update["sequence"] = 2.into();
    update["request_id"] = "paused-toggle".into();
    update["expected_revision"] = 2.into();
    assert_eq!(
        f.request(Some("alice"), Method::POST, "/account/playback", update)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.state()
            .user_data
            .playback_snapshot("alice")
            .unwrap()
            .checkpoint
            .unwrap()
            .track_id,
        f.tracks[1].id
    );
    assert_eq!(
        f.request(Some("bob"), Method::GET, "/account/playback", Value::Null)
            .await
            .1["checkpoint"],
        Value::Null
    );
}

#[tokio::test]
async fn exact_deck_and_source_survive_runtime_release_and_database_reopen() {
    let mut f = Fixture::new();
    let first = f
        .create(
            "alice",
            json!({"mode":"all","client_id":"phone","request_id":"deck","limit":50}),
        )
        .await;
    let deck = first["session_id"].as_str().unwrap();
    let ids = item_ids(&first);
    let snapshot = save(
        &f,
        "alice",
        start(&ids[0], source("library", None), "all", Some(deck)),
    )
    .await;
    f.state()
        .shuffle_sessions
        .remove("alice".into(), "phone".into(), deck.into())
        .await
        .unwrap();
    let uri = format!("/account/playback/decks/{deck}?offset=0&limit=50");
    let page = f
        .request(Some("alice"), Method::GET, &uri, Value::Null)
        .await;
    assert_eq!(page.0, StatusCode::OK);
    assert_eq!(item_ids(&page.1), ids);
    assert_eq!(
        f.request(Some("bob"), Method::GET, &uri, Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.request(None, Method::GET, &uri, Value::Null).await.0,
        StatusCode::UNAUTHORIZED
    );

    // Close all handles to the personal DB and reopen it, simulating restart.
    f.app.take();
    f.state.take();
    let store = user_data::UserDataStore::new(Arc::new(
        redb::Database::open(f.directory.join("personalization.redb")).unwrap(),
    ));
    store.init_tables().unwrap();
    assert_eq!(
        serde_json::to_value(store.playback_snapshot("alice").unwrap()).unwrap(),
        snapshot
    );
    assert_eq!(
        store
            .playback_deck_ids("alice", deck, 0, 50)
            .unwrap()
            .unwrap()
            .1,
        ids
    );
    assert!(store.playback_deck("bob", deck).unwrap().is_none());
    drop(store);
}

#[tokio::test]
async fn playlist_removals_keep_deck_offsets_and_deletion_detaches_the_resume_source() {
    let f = Fixture::new();
    let playlist = f
        .state()
        .user_data
        .create_playlist(
            "alice",
            "Private".into(),
            None,
            f.tracks[..3].iter().map(|t| t.id.clone()).collect(),
        )
        .unwrap();
    let first = f.create("alice", json!({"mode":"playlist","playlist_id":playlist.id,"client_id":"phone","request_id":"playlist","limit":10})).await;
    let ids = item_ids(&first);
    let deck = first["session_id"].as_str().unwrap();
    let written = save(
        &f,
        "alice",
        start(
            &ids[0],
            source("playlist", Some(&playlist.id)),
            "current_playlist",
            Some(deck),
        ),
    )
    .await;
    f.state()
        .user_data
        .change_playlist_tracks("alice", &playlist.id, vec![ids[0].clone()], false)
        .unwrap();
    let restored = f
        .request(Some("alice"), Method::GET, "/account/playback", Value::Null)
        .await;
    assert_eq!(restored.0, StatusCode::OK);
    assert_eq!(restored.1["checkpoint"]["track_id"], ids[1]);
    assert_eq!(restored.1["checkpoint"]["cursor"], 1);
    assert_eq!(restored.1["reason"], "track_removed");
    let page = f
        .request(
            Some("alice"),
            Method::GET,
            &format!("/account/playback/decks/{deck}?limit=10"),
            Value::Null,
        )
        .await;
    assert_eq!(item_ids(&page.1), ids[1..]);
    assert_eq!(page.1["item_offsets"], json!([1, 2]));
    assert_eq!(page.1["next_offset"], 3);
    assert_eq!(
        serde_json::to_value(f.state().user_data.playback_snapshot("alice").unwrap()).unwrap(),
        written
    );

    f.state()
        .user_data
        .delete_playlist("alice", &playlist.id)
        .unwrap();
    let detached = f
        .request(Some("alice"), Method::GET, "/account/playback", Value::Null)
        .await
        .1;
    assert_eq!(detached["reason"], "source_missing");
    assert_eq!(detached["checkpoint"]["source"]["kind"], "standalone");
    assert_eq!(detached["checkpoint"]["shuffle_mode"], "off");
    assert_eq!(detached["checkpoint"]["track_id"], ids[0]);
    assert_eq!(
        f.state()
            .user_data
            .playback_snapshot("alice")
            .unwrap()
            .revision,
        1
    );
}

#[tokio::test]
async fn an_accepted_retry_survives_source_deletion_and_temporary_catalog_unavailability() {
    let f = Fixture::new();
    let playlist = f
        .state()
        .user_data
        .create_playlist(
            "alice",
            "Private".into(),
            None,
            vec![f.tracks[0].id.clone()],
        )
        .unwrap();
    let input = start(
        &f.tracks[0].id,
        source("playlist", Some(&playlist.id)),
        "off",
        None,
    );
    let written = save(&f, "alice", input.clone()).await;
    f.state()
        .user_data
        .delete_playlist("alice", &playlist.id)
        .unwrap();
    assert_eq!(save(&f, "alice", input.clone()).await, written);
    f.state()
        .available
        .store(false, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(save(&f, "alice", input.clone()).await, written);
    assert_eq!(
        f.request(
            Some("bob"),
            Method::POST,
            "/account/playback",
            input.clone()
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    f.state().user_data.purge_playback("alice").unwrap();
    assert_ne!(
        f.request(Some("alice"), Method::POST, "/account/playback", input)
            .await
            .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn deleted_catalog_entries_leave_holes_and_unavailable_libraries_do_not_clear_state() {
    let f = Fixture::new();
    let first = f
        .create(
            "alice",
            json!({"mode":"all","client_id":"phone","request_id":"delete","limit":50}),
        )
        .await;
    let ids = item_ids(&first);
    let deck = first["session_id"].as_str().unwrap();
    save(
        &f,
        "alice",
        start(&ids[0], source("library", None), "all", Some(deck)),
    )
    .await;
    f.state()
        .available
        .store(false, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        f.request(Some("alice"), Method::GET, "/account/playback", Value::Null)
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        f.state()
            .user_data
            .playback_snapshot("alice")
            .unwrap()
            .revision,
        1
    );
    f.state()
        .available
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let deleted = f.tracks.iter().find(|track| track.id == ids[0]).unwrap();
    std::fs::remove_file(
        f.state()
            .library
            .resolve_relpath(&deleted.file_relpath)
            .unwrap(),
    )
    .unwrap();
    f.state().library.incremental_scan().unwrap();
    let restored = f
        .request(Some("alice"), Method::GET, "/account/playback", Value::Null)
        .await
        .1;
    assert_eq!(restored["checkpoint"]["track_id"], ids[1]);
    assert_eq!(restored["checkpoint"]["cursor"], 1);
    let page = f
        .request(
            Some("alice"),
            Method::GET,
            &format!("/account/playback/decks/{deck}?limit=50"),
            Value::Null,
        )
        .await
        .1;
    assert_eq!(item_ids(&page), ids[1..]);
    assert_eq!(page["item_offsets"][0], 1);
}

#[tokio::test]
async fn paused_updates_accept_only_the_authoritative_reconciled_replacement_track() {
    let f = Fixture::new();
    let ids = f.tracks[..3]
        .iter()
        .map(|track| track.id.clone())
        .collect::<Vec<_>>();
    let playlist = f
        .state()
        .user_data
        .create_playlist("alice", "Resume".into(), None, ids.clone())
        .unwrap();
    let written = save(
        &f,
        "alice",
        start(&ids[0], source("playlist", Some(&playlist.id)), "off", None),
    )
    .await;
    f.state()
        .user_data
        .change_playlist_tracks("alice", &playlist.id, vec![ids[0].clone()], false)
        .unwrap();
    let restored = f
        .request(Some("alice"), Method::GET, "/account/playback", Value::Null)
        .await
        .1;
    assert_eq!(restored["checkpoint"]["track_id"], ids[1]);
    let mut update = json!({
        "expected_revision": written["revision"], "generation": written["generation"],
        "session_id": "restored-phone", "sequence": 1, "request_id": "paused-change", "intent": "update",
        "checkpoint": restored["checkpoint"],
    });
    update["checkpoint"]["track_id"] = ids[2].clone().into();
    assert_eq!(
        f.request(
            Some("alice"),
            Method::POST,
            "/account/playback",
            update.clone()
        )
        .await
        .0,
        StatusCode::CONFLICT
    );

    update["checkpoint"]["track_id"] = ids[1].clone().into();
    let accepted = save(&f, "alice", update).await;
    assert_eq!(accepted["checkpoint"]["track_id"], ids[1]);
    assert_eq!(accepted["generation"], written["generation"]);
    assert_eq!(
        f.state()
            .user_data
            .last_track("alice")
            .unwrap()
            .track_id
            .as_deref(),
        Some(ids[0].as_str())
    );
}

#[tokio::test]
async fn forged_decks_wrong_cursors_and_foreign_playlist_sources_cannot_be_saved() {
    let f = Fixture::new();
    let first = f
        .create(
            "alice",
            json!({"mode":"all","client_id":"phone","request_id":"private"}),
        )
        .await;
    let ids = item_ids(&first);
    let deck = first["session_id"].as_str().unwrap();
    let input = start(&ids[0], source("library", None), "all", Some(deck));
    assert_eq!(
        f.request(
            Some("bob"),
            Method::POST,
            "/account/playback",
            input.clone()
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let mut wrong = input.clone();
    wrong["checkpoint"]["cursor"] = 1.into();
    assert_eq!(
        f.request(Some("alice"), Method::POST, "/account/playback", wrong)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let foreign = f
        .state()
        .user_data
        .create_playlist("bob", "secret".into(), None, vec![ids[0].clone()])
        .unwrap();
    let wrong = start(&ids[0], source("playlist", Some(&foreign.id)), "off", None);
    assert_eq!(
        f.request(Some("alice"), Method::POST, "/account/playback", wrong)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.state()
            .user_data
            .playback_snapshot("alice")
            .unwrap()
            .revision,
        0
    );
    assert!(f
        .state()
        .user_data
        .playback_deck("alice", deck)
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn paused_mode_updates_are_durable_and_legacy_starts_safely_replace_context() {
    let f = Fixture::new();
    let first = f
        .create(
            "alice",
            json!({"mode":"all","client_id":"phone","request_id":"toggle"}),
        )
        .await;
    let ids = item_ids(&first);
    let deck = first["session_id"].as_str().unwrap();
    let input = start(&ids[0], source("library", None), "all", Some(deck));
    let accepted = save(&f, "alice", input.clone()).await;
    let mut update = input;
    update["intent"] = "update".into();
    update["generation"] = accepted["generation"].clone();
    update["expected_revision"] = 1.into();
    update["sequence"] = 2.into();
    update["request_id"] = "off".into();
    update["checkpoint"]["shuffle_mode"] = "off".into();
    update["checkpoint"]
        .as_object_mut()
        .unwrap()
        .remove("deck_id");
    let updated = save(&f, "alice", update).await;
    assert_eq!(updated["revision"], 2);
    assert_eq!(updated["generation"], accepted["generation"]);
    f.state()
        .user_data
        .record_track_start("alice", &ids[1], "old-client", 1, "old-start")
        .unwrap();
    let legacy = f.state().user_data.playback_snapshot("alice").unwrap();
    assert_eq!(legacy.revision, 3);
    assert_eq!(legacy.checkpoint.unwrap().source.kind, "standalone");
    assert_ne!(
        legacy.generation.as_deref(),
        accepted["generation"].as_str()
    );
    assert_eq!(
        f.request(
            Some("alice"),
            Method::DELETE,
            "/account/playback?expected_revision=2",
            Value::Null
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.request(
            Some("alice"),
            Method::DELETE,
            "/account/playback?expected_revision=3",
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(f
        .state()
        .user_data
        .last_track("alice")
        .unwrap()
        .track_id
        .is_none());
    assert!(f
        .state()
        .user_data
        .playback_deck("alice", deck)
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn out_of_deck_paused_toggles_keep_preferences_and_use_effective_shuffle_membership() {
    let f = Fixture::new();
    let current = &f.tracks[0].id;
    let next = &f.tracks[1].id;
    let origin = f
        .state()
        .user_data
        .create_playlist("alice", "Origin".into(), None, vec![current.clone()])
        .unwrap();
    f.state().user_data.add_like("alice", next).unwrap();
    let original = save(
        &f,
        "alice",
        start(current, source("playlist", Some(&origin.id)), "off", None),
    )
    .await;
    let deck = f
        .create(
            "alice",
            json!({"mode":"liked","client_id":"phone","request_id":"out-of-deck"}),
        )
        .await;
    let mut update = start(
        current,
        source("playlist", Some(&origin.id)),
        "artist",
        Some(deck["session_id"].as_str().unwrap()),
    );
    update["intent"] = "update".into();
    update["generation"] = original["generation"].clone();
    update["expected_revision"] = 1.into();
    update["sequence"] = 2.into();
    update["request_id"] = "toggle".into();
    update["checkpoint"]["shuffle_context"] = source("liked", None);
    let accepted = save(&f, "alice", update).await;
    assert_eq!(accepted["checkpoint"]["track_id"], *current);
    let restored = f
        .request(Some("alice"), Method::GET, "/account/playback", Value::Null)
        .await
        .1;
    assert_eq!(restored["checkpoint"]["track_id"], *next);
    assert_eq!(restored["checkpoint"]["shuffle_mode"], "artist");
    assert_eq!(restored["checkpoint"]["source"]["id"], origin.id);
    assert_eq!(restored["checkpoint"]["shuffle_context"]["kind"], "liked");
}

#[tokio::test]
async fn restoration_wraps_once_after_removed_tail_and_empty_likes_never_broaden_scope() {
    let f = Fixture::new();
    for track in &f.tracks[..3] {
        f.state().user_data.add_like("alice", &track.id).unwrap();
    }
    let deck = f
        .create(
            "alice",
            json!({"mode":"liked","client_id":"phone","request_id":"wrap","limit":10}),
        )
        .await;
    let ids = item_ids(&deck);
    let mut input = start(
        &ids[2],
        source("liked", None),
        "liked",
        Some(deck["session_id"].as_str().unwrap()),
    );
    input["checkpoint"]["cursor"] = 2.into();
    save(&f, "alice", input).await;
    f.state().user_data.remove_like("alice", &ids[2]).unwrap();
    let restored = f
        .request(Some("alice"), Method::GET, "/account/playback", Value::Null)
        .await
        .1;
    assert_eq!(restored["checkpoint"]["track_id"], ids[0]);
    assert_eq!(restored["checkpoint"]["cursor"], 0);
    for id in &ids[..2] {
        f.state().user_data.remove_like("alice", id).unwrap();
    }
    let empty = f
        .request(Some("alice"), Method::GET, "/account/playback", Value::Null)
        .await
        .1;
    assert_eq!(empty["checkpoint"], Value::Null);
    assert_eq!(empty["reason"], "source_empty");
    assert_eq!(
        f.state()
            .user_data
            .playback_snapshot("alice")
            .unwrap()
            .revision,
        1
    );
    let mut custom = source("custom", None);
    let invalid = start(&f.tracks[0].id, custom.clone(), "off", None);
    assert_eq!(
        f.request(Some("bob"), Method::POST, "/account/playback", invalid)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    custom["artist_ids"] = json!(["deleted-artist"]);
    let missing = save(&f, "bob", start(&f.tracks[0].id, custom, "off", None)).await;
    assert_eq!(missing["revision"], 1);
    let empty = f
        .request(Some("bob"), Method::GET, "/account/playback", Value::Null)
        .await
        .1;
    assert_eq!(empty["checkpoint"], Value::Null);
    assert_eq!(empty["reason"], "source_empty");
}

#[tokio::test]
async fn deleted_account_fence_blocks_already_authenticated_late_writers() {
    let f = Fixture::new();
    let input = start(&f.tracks[0].id, source("library", None), "off", None);
    save(&f, "alice", input.clone()).await;
    f.state().user_data.purge_playback("alice").unwrap();
    // Fixed auth in this listener-free fixture deliberately simulates a request
    // that passed authentication before the account deletion committed.
    assert_eq!(
        f.request(Some("alice"), Method::POST, "/account/playback", input)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert!(f
        .state()
        .user_data
        .record_track_start("alice", &f.tracks[0].id, "late", 1, "late")
        .is_err());
    assert!(f
        .state()
        .user_data
        .playback_snapshot("alice")
        .unwrap()
        .checkpoint
        .is_none());
    assert!(f
        .state()
        .user_data
        .last_track("alice")
        .unwrap()
        .track_id
        .is_none());
    save(
        &f,
        "bob",
        start(&f.tracks[1].id, source("library", None), "off", None),
    )
    .await;
}

#[test]
fn durable_chunks_page_across_boundaries_and_cleanup_keeps_current_account_deck() {
    let f = Fixture::new();
    let ids: Vec<String> = (0..700).map(|i| format!("id-{i}")).collect();
    for index in 0..6 {
        let id = format!("deck-{index}");
        let mut input = start(&ids[0], source("library", None), "all", Some(&id));
        input["expected_revision"] = index.into();
        input["sequence"] = (index + 1).into();
        input["request_id"] = format!("request-{index}").into();
        let input: user_data::PlaybackWrite = serde_json::from_value(input).unwrap();
        let deck = user_data::PlaybackDeckImport {
            id: id.clone(),
            source: input.checkpoint.shuffle_context.clone(),
            track_ids: ids.clone(),
        };
        f.state()
            .user_data
            .save_playback("alice", &input, Some(deck))
            .unwrap();
    }
    let page = f
        .state()
        .user_data
        .playback_deck_ids("alice", "deck-5", 250, 256)
        .unwrap()
        .unwrap();
    assert_eq!(page.1, ids[250..506]);
    assert!(f
        .state()
        .user_data
        .playback_deck("alice", "deck-0")
        .unwrap()
        .is_none());
    assert!(f
        .state()
        .user_data
        .playback_deck("alice", "deck-1")
        .unwrap()
        .is_none());
    for index in 2..6 {
        assert!(f
            .state()
            .user_data
            .playback_deck("alice", &format!("deck-{index}"))
            .unwrap()
            .is_some());
    }
    f.state().user_data.purge_playback("alice").unwrap();
    assert!(f
        .state()
        .user_data
        .playback_deck("alice", "deck-5")
        .unwrap()
        .is_none());
}
