//! Request/response contracts against the actual account handlers, without TCP.
use super::*;
use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{header, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

async fn authenticate(
    State(state): State<state::AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let user = http::extract_token(request.headers())
        .and_then(|token| state.auth.user_from_token(&token).ok().flatten());
    match user {
        Some(user) => {
            request.extensions_mut().insert(state::AuthContext { user });
            next.run(request).await
        }
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}
struct Fixture {
    app: Option<Router>,
    state: Option<state::AppState>,
    alice: String,
    bob: String,
    first: String,
    root: std::path::PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("phonolite-account-api-{}", uuid::Uuid::new_v4()));
        let music = root.join("music");
        std::fs::create_dir_all(&music).unwrap();
        std::fs::write(music.join("first.mp3"), b"metadata fixture").unwrap();
        let (library, _) = library::Library::load_or_scan(
            vec![library::LibraryRoot::new(String::new(), music)],
            root.join("catalog.redb"),
        )
        .unwrap();
        let first = library.list_tracks(None, 10, 0).unwrap().0[0].id.clone();
        let database = || {
            Arc::new(
                redb::Database::builder()
                    .create_with_backend(redb::backends::InMemoryBackend::new())
                    .unwrap(),
            )
        };
        let db = database();
        let auth = auth::AuthStore::new(db.clone(), std::time::Duration::from_secs(60));
        auth.init_tables().unwrap();
        let alice_user = auth
            .create_user("alice", "password", auth::UserRole::User)
            .unwrap();
        let bob_user = auth
            .create_user("bob", "password", auth::UserRole::User)
            .unwrap();
        let alice = auth.create_session(&alice_user.id).unwrap().token;
        let bob = auth.create_session(&bob_user.id).unwrap().token;
        let user_data = user_data::UserDataStore::new(db);
        user_data.init_tables().unwrap();
        let stats = stats_store::StatsStore::new(database());
        stats.init_tables().unwrap();
        let state = state::AppState {
            library_state: Arc::new(parking_lot::RwLock::new(state::LibraryState {
                library: Some(library),
            })),
            auth,
            user_data,
            stats,
            shutdown: Default::default(),
            process_metrics: Default::default(),
        };
        let app = Router::new()
            .route("/state", get(account_events::snapshot))
            .route(
                "/presets",
                get(account_api::presets).post(account_api::create_preset),
            )
            .route(
                "/presets/:id",
                post(account_api::update_preset).delete(account_api::delete_preset),
            )
            .route(
                "/last-track",
                get(account_api::last_track).post(account_api::track_started),
            )
            .route("/revoke-others", post(account_api::revoke_others))
            .route("/revoke-all", post(account_api::revoke_all))
            .route("/presence", post(playback_presence_api::report))
            .layer(middleware::from_fn_with_state(state.clone(), authenticate))
            .with_state(state.clone());
        Self {
            app: Some(app),
            state: Some(state),
            alice,
            bob,
            first,
            root,
        }
    }
    async fn request(
        &self,
        token: &str,
        method: Method,
        path: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = self
            .app
            .as_ref()
            .unwrap()
            .clone()
            .oneshot(request)
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.app.take();
        self.state.take();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn playback_presence_http_counts_devices_and_rejects_delayed_playing_reports() {
    let fixture = Fixture::new();
    let network = fixture.state.as_ref().unwrap().process_metrics.network();
    let payload = |client, sequence, playing| {
        json!({
            "client_id":client, "sequence":sequence, "playing":playing,
        })
    };
    assert_eq!(
        fixture
            .request(
                "invalid",
                Method::POST,
                "/presence",
                payload("desktop", 1, true)
            )
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    for client in ["desktop", "phone"] {
        let (status, response) = fixture
            .request(
                &fixture.alice,
                Method::POST,
                "/presence",
                payload(client, 1, true),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response["accepted"], true);
    }
    assert_eq!(network.playing_devices(), 2);
    assert_eq!(network.active_streams(), 0);
    let (_, response) = fixture
        .request(
            &fixture.alice,
            Method::POST,
            "/presence",
            payload("desktop", 3, false),
        )
        .await;
    assert_eq!(response["accepted"], true);
    let (_, response) = fixture
        .request(
            &fixture.alice,
            Method::POST,
            "/presence",
            payload("desktop", 2, true),
        )
        .await;
    assert_eq!(response["accepted"], false);
    assert_eq!(network.playing_devices(), 1);
    assert_eq!(
        fixture
            .request(
                &fixture.alice,
                Method::POST,
                "/presence",
                payload("", 4, true)
            )
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
}
#[tokio::test]
async fn preset_http_contract_preconditions_and_account_scope() {
    let f = Fixture::new();
    let (status, preset) = f
        .request(
            &f.alice,
            Method::POST,
            "/presets",
            json!({"name":"Evening","artist_ids":["artist"],"genres":["Rock"]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let id = preset["id"].as_str().unwrap();
    assert_eq!(preset["revision"], 1);
    assert_eq!(
        f.request(&f.bob, Method::GET, "/presets", Value::Null)
            .await
            .1["items"],
        json!([])
    );
    assert_eq!(
        f.request(
            &f.bob,
            Method::POST,
            &format!("/presets/{id}"),
            json!({"name":"Steal","expected_revision":1})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.request(
            &f.alice,
            Method::POST,
            &format!("/presets/{id}"),
            json!({"name":"Overwrite"})
        )
        .await
        .0,
        StatusCode::PRECONDITION_REQUIRED
    );
    assert_eq!(
        f.request(
            &f.alice,
            Method::POST,
            &format!("/presets/{id}"),
            json!({"name":"Changed","expected_revision":1})
        )
        .await
        .1["revision"],
        2
    );
    assert_eq!(
        f.request(
            &f.alice,
            Method::DELETE,
            &format!("/presets/{id}?expected_revision=1"),
            Value::Null
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.request(
            &f.alice,
            Method::DELETE,
            &format!("/presets/{id}?expected_revision=2"),
            Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        f.request(&f.alice, Method::GET, "/state", Value::Null)
            .await
            .1["revisions"],
        json!({"collections":0,"presets":3,"statistics":0,"playback":0})
    );
}
#[tokio::test]
async fn start_reports_require_real_tracks_and_logout_is_scoped_to_current_account() {
    let f = Fixture::new();
    let state = f.state.as_ref().unwrap();
    let user = state.auth.user_from_token(&f.alice).unwrap().unwrap();
    let second = state.auth.create_session(&user.id).unwrap().token;
    let console = state.auth.create_console_session(&user.id).unwrap().token;
    let network = state.process_metrics.network();
    for token in [&f.alice, &second, &f.bob] {
        assert_eq!(
            f.request(
                token,
                Method::POST,
                "/presence",
                json!({"client_id":"client", "sequence":1, "playing":true})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    assert_eq!(network.playing_devices(), 3);
    assert_eq!(
        f.request(
            &f.alice,
            Method::POST,
            "/last-track",
            json!({"track_id":"unknown","session_id":"a","sequence":1,"request_id":"r1"})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.request(
            &f.alice,
            Method::POST,
            "/last-track",
            json!({"track_id":f.first,"session_id":"a","sequence":1,"request_id":"r1"})
        )
        .await
        .1,
        json!({"track_id":f.first,"revision":1})
    );
    assert_eq!(
        f.request(&f.bob, Method::GET, "/last-track", Value::Null)
            .await
            .1,
        json!({"track_id":null,"revision":0})
    );
    assert_eq!(
        f.request(&f.alice, Method::POST, "/revoke-others", Value::Null)
            .await
            .1["revoked"],
        1
    );
    assert_eq!(network.playing_devices(), 2);
    assert_eq!(
        f.request(&second, Method::GET, "/state", Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert!(state.auth.user_from_token(&console).unwrap().is_some());
    assert_eq!(
        f.request(&f.alice, Method::POST, "/revoke-all", Value::Null)
            .await
            .1["revoked"],
        1
    );
    assert_eq!(network.playing_devices(), 1);
    assert_eq!(
        f.request(&f.alice, Method::GET, "/state", Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert!(state.auth.user_from_token(&console).unwrap().is_some());
    assert_eq!(
        f.request(&f.bob, Method::GET, "/state", Value::Null)
            .await
            .0,
        StatusCode::OK
    );
}
