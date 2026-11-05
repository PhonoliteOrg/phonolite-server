//! Production router, auth middleware, catalogue cache and file responses,
//! exercised by in-process requests only. No listener or provider is started.
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use axum::response::Response;
use parking_lot::RwLock;
use redb::{Database, TableDefinition};
use serde_json::Value;
use tower::ServiceExt;

use crate::auth::{AuthStore, UserRole};
use crate::config::ServerConfig;
use crate::state::{AppState, LibraryState, LibraryStatus};

fn memory_database() -> Arc<Database> {
    Arc::new(
        Database::builder()
            .set_cache_size(1024 * 1024)
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .unwrap(),
    )
}

struct Authentication {
    store: AuthStore,
    alice: (String, String),
    bob: (String, String),
    security: Arc<crate::security::SecurityState>,
}

fn authentication() -> &'static Authentication {
    // Enroll each real account once. All tests still exercise production token
    // creation/validation; no process-global logging is initialized.
    static AUTH: OnceLock<Authentication> = OnceLock::new();
    AUTH.get_or_init(|| {
        let store = AuthStore::new(memory_database(), Duration::from_secs(3600));
        store.init_tables().unwrap();
        let alice = store
            .create_user("catalogue-alice", "test-password", UserRole::User)
            .unwrap();
        let bob = store
            .create_user("catalogue-bob", "test-password", UserRole::User)
            .unwrap();
        let alice_token = store.create_session(&alice.id).unwrap().token;
        let bob_token = store.create_session(&bob.id).unwrap().token;
        let security = crate::security::SecurityState::new(store.opaque_setup().unwrap());
        Authentication {
            store,
            alice: (alice.id, alice_token),
            bob: (bob.id, bob_token),
            security,
        }
    })
}

struct Fixture {
    state: AppState,
    root: PathBuf,
    reference: String,
}

impl Fixture {
    fn new() -> Self {
        let auth = authentication();
        let root = std::env::temp_dir().join(format!(
            "phonolite-catalogue-router-{}",
            uuid::Uuid::new_v4()
        ));
        let artwork = root.join("metadata/logos");
        std::fs::create_dir_all(&artwork).unwrap();
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            2,
            2,
            image::Rgba([40, 80, 120, 255]),
        ));
        let mut encoded = Cursor::new(Vec::new());
        image
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let image = encoded.into_inner();
        let reference = format!("logos/artist.{}.png", blake3::hash(&image).to_hex());
        std::fs::write(root.join("metadata").join(&reference), image).unwrap();

        let db = memory_database();
        let library = library::Library::load_or_scan_with_db(Vec::new(), db.clone())
            .unwrap()
            .0;
        // These are authoritative catalogue fixtures, encoded with the public
        // stable common records. The real library builds its disposable index.
        let artist = common::Artist {
            id: "artist".into(),
            name: "Artist".into(),
            genres: vec!["rock".into()],
            summary: Some("Private detail excluded from the compact list".into()),
            logo_ref: Some(reference.clone()),
            banner_ref: None,
        };
        let album = common::Album {
            id: "album".into(),
            artist_id: artist.id.clone(),
            artist_ids: Vec::new(),
            artist_names: Vec::new(),
            title: "Album".into(),
            year: None,
            folder_relpath: String::new(),
            cover_ref: None,
            genres: Vec::new(),
            summary: None,
        };
        let write = db.begin_write().unwrap();
        write
            .open_table(TableDefinition::<&str, &[u8]>::new("artists"))
            .unwrap()
            .insert("artist", bincode::serialize(&artist).unwrap().as_slice())
            .unwrap();
        write
            .open_table(TableDefinition::<&str, &[u8]>::new("artists_by_name"))
            .unwrap()
            .insert("artist\u{1f}artist", &[1u8][..])
            .unwrap();
        write
            .open_table(TableDefinition::<&str, &[u8]>::new("albums"))
            .unwrap()
            .insert("album", bincode::serialize(&album).unwrap().as_slice())
            .unwrap();
        write
            .open_table(TableDefinition::<&str, &[u8]>::new("artist_albums"))
            .unwrap()
            .insert("artist\u{1f}9999\u{1f}album\u{1f}album", &[1u8][..])
            .unwrap();
        write.commit().unwrap();

        let user_data = crate::user_data::UserDataStore::new(memory_database());
        user_data.init_tables().unwrap();
        let stats = crate::stats_store::StatsStore::new(memory_database());
        stats.init_tables().unwrap();
        let limits = crate::resources::ResourceLimits::default();
        let config = ServerConfig {
            metadata_path: root.join("metadata").to_string_lossy().into_owned(),
            ..Default::default()
        };
        let state = AppState {
            shutdown: crate::shutdown::Shutdown::new(),
            process_metrics: Default::default(),
            library_state: Arc::new(RwLock::new(LibraryState {
                library: Some(library),
                status: LibraryStatus::Ready(library::LibraryStats {
                    artists: 1,
                    albums: 1,
                    tracks: 0,
                }),
                metadata_work: Default::default(),
            })),
            auth: auth.store.clone(),
            security: auth.security.clone(),
            config_path: root.join("config.yaml"),
            config: Arc::new(RwLock::new(config)),
            db: db.clone(),
            user_data,
            stats,
            activity: crate::activity_store::ActivityStore::new(db),
            log_control: crate::logging::LogControl::for_tests(),
            watcher: Default::default(),
            external_client: reqwest::Client::new(),
            stream_cache: crate::stream_cache::StreamCache::new(root.join("streams"), false),
            metadata_events: crate::metadata_events::MetadataEventBus::new(8),
            metadata_coordinator: Default::default(),
            shuffle_sessions: Default::default(),
            download_jobs: crate::download_jobs::DownloadJobStore::new(memory_database()),
            backup_jobs: Default::default(),
            resources: Arc::new(crate::resources::Resources::new(&limits)),
            catalogue_cache: Arc::new(Default::default()),
            artwork_cache: Arc::new(crate::artwork_cache::ArtworkCache::new(&limits)),
        };
        Self {
            state,
            root,
            reference,
        }
    }

    async fn request(&self, uri: &str, token: Option<&str>, etag: Option<&str>) -> Response {
        let mut request = Request::builder().uri(uri);
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        if let Some(etag) = etag {
            request = request.header(header::IF_NONE_MATCH, etag);
        }
        super::api_router(self.state.clone())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.shutdown.cancel();
        self.state.catalogue_cache.close();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn json(response: Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap()
}

async fn post_json(fixture: &Fixture, uri: &str, token: Option<&str>, payload: Value) -> Response {
    let mut request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    super::api_router(fixture.state.clone())
        .oneshot(request.body(Body::from(payload.to_string())).unwrap())
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_download_profiles_prepare_authenticated_resumable_opus_files() {
    use sha2::{Digest, Sha256};
    let fixture = Fixture::new();
    let auth = authentication();
    let music = fixture.root.join("music");
    std::fs::create_dir_all(&music).unwrap();
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/formats/tone.wav"),
        music.join("tone.wav"),
    )
    .unwrap();
    let library = library::Library::load_or_scan_with_db(
        vec![library::LibraryRoot::new("music".into(), music)],
        memory_database(),
    )
    .unwrap()
    .0;
    let album = library.list_albums(None, 10, 0).unwrap().0.remove(0);
    let track = library.get_album_tracks(&album.id).unwrap().remove(0);
    fixture.state.library_state.write().library = Some(library);
    fixture.state.download_jobs.init_tables().unwrap();
    let capabilities = json(fixture.request("/server/capabilities", None, None).await).await;
    assert_eq!(
        capabilities["download_profiles"],
        serde_json::json!(["original", "opus_64", "opus_96", "opus_128", "opus_192"])
    );
    let payload = |request: &str, profile: &str| {
        serde_json::json!({
            "client_id": "offline-phone", "client_request_id": request,
            "scope": {"kind":"track", "id":track.id}, "profile":profile
        })
    };
    assert_eq!(
        post_json(
            &fixture,
            "/download/jobs",
            None,
            payload("unauthorized", "opus_64")
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    let mut legacy_request = payload("original", "original");
    legacy_request.as_object_mut().unwrap().remove("profile");
    let original = json(
        post_json(
            &fixture,
            "/download/jobs",
            Some(&auth.alice.1),
            legacy_request,
        )
        .await,
    )
    .await;
    assert_eq!(original["profile"], "original");
    assert_eq!(original["items"][0]["status"], "ready_to_download");
    let response = post_json(
        &fixture,
        "/download/jobs",
        Some(&auth.alice.1),
        payload("compressed", "opus_64"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let created = json(response).await;
    assert_eq!(created["profile"], "opus_64");
    assert_eq!(created["items"].as_array().unwrap().len(), 1);
    assert_eq!(created["items"][0]["status"], "queued");
    assert_ne!(created["job_id"], original["job_id"]);
    let job_url = format!("/download/jobs/{}", created["job_id"].as_str().unwrap());
    let ready = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let value = json(fixture.request(&job_url, Some(&auth.alice.1), None).await).await;
            if value["items"][0]["status"] == "ready_to_download" {
                break value;
            }
            assert_ne!(value["items"][0]["status"], "failed", "{value}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let item = &ready["items"][0];
    assert_eq!(item["profile"], "opus_64");
    let file_url = item["download_url"]
        .as_str()
        .unwrap()
        .strip_prefix("/api")
        .unwrap();
    assert_eq!(
        fixture
            .request(&job_url, Some(&auth.bob.1), None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        fixture
            .request(file_url, Some(&auth.bob.1), None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let file = fixture.request(file_url, Some(&auth.alice.1), None).await;
    assert_eq!(file.status(), StatusCode::OK);
    assert_eq!(file.headers()["x-phonolite-download-profile"], "opus_64");
    assert_eq!(
        file.headers()[header::ETAG].to_str().unwrap(),
        item["etag"].as_str().unwrap()
    );
    assert!(file.headers()[header::CONTENT_DISPOSITION]
        .to_str()
        .unwrap()
        .contains(".opus"));
    let bytes = to_bytes(file.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(&bytes[..4], b"OggS");
    assert_eq!(bytes.len() as u64, item["byte_length"].as_u64().unwrap());
    assert_eq!(
        format!("{:x}", Sha256::digest(&bytes)),
        item["sha256"].as_str().unwrap()
    );
    let request = Request::builder()
        .uri(file_url)
        .header(header::AUTHORIZATION, format!("Bearer {}", auth.alice.1))
        .header(header::RANGE, "bytes=17-")
        .header(header::IF_RANGE, item["etag"].as_str().unwrap())
        .body(Body::empty())
        .unwrap();
    let range = super::api_router(fixture.state.clone())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(range.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        to_bytes(range.into_body(), 1024 * 1024)
            .await
            .unwrap()
            .as_ref(),
        &bytes[17..]
    );
    // Eviction is recovered through the job protocol, preserving the profile
    // and exact encoded bytes rather than redirecting to a source download.
    let path = crate::download_renditions::file_path(
        &fixture.state,
        item["rendition_id"].as_str().unwrap(),
    )
    .unwrap();
    std::fs::remove_file(path).unwrap();
    assert_eq!(
        fixture
            .request(file_url, Some(&auth.alice.1), None)
            .await
            .status(),
        StatusCode::GONE
    );
    let regenerated = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let value = json(fixture.request(&job_url, Some(&auth.alice.1), None).await).await;
            if value["items"][0]["status"] == "ready_to_download" {
                break value;
            }
            assert_ne!(value["items"][0]["status"], "failed", "{value}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(regenerated["items"][0]["profile"], "opus_64");
    assert_eq!(regenerated["items"][0]["sha256"], item["sha256"]);
    assert_eq!(
        post_json(
            &fixture,
            "/download/jobs",
            Some(&auth.alice.1),
            payload("compressed", "opus_128")
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    let deleted = format!("{job_url}/actions");
    assert_eq!(
        post_json(
            &fixture,
            &deleted,
            Some(&auth.alice.1),
            serde_json::json!({"action":"cancel"})
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        fixture
            .request(file_url, Some(&auth.alice.1), None)
            .await
            .status(),
        StatusCode::GONE
    );
    // A metadata failure can be retried after its source is restored.
    let source = fixture
        .state
        .library_state
        .read()
        .library
        .as_ref()
        .unwrap()
        .resolve_relpath(&track.file_relpath)
        .unwrap();
    std::fs::remove_file(&source).unwrap();
    let failed = json(
        post_json(
            &fixture,
            "/download/jobs",
            Some(&auth.alice.1),
            payload("retry-metadata", "opus_128"),
        )
        .await,
    )
    .await;
    assert_eq!(failed["items"][0]["status"], "metadata_failed");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/formats/tone.wav"),
        source,
    )
    .unwrap();
    let retry_url = format!("/download/jobs/{}", failed["job_id"].as_str().unwrap());
    assert_eq!(
        post_json(
            &fixture,
            &format!("{retry_url}/actions"),
            Some(&auth.alice.1),
            serde_json::json!({"action":"retry_failed"})
        )
        .await
        .status(),
        StatusCode::OK
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let value = json(fixture.request(&retry_url, Some(&auth.alice.1), None).await).await;
            if value["items"][0]["status"] == "ready_to_download" {
                assert_eq!(value["items"][0]["profile"], "opus_128");
                assert!(value["items"][0]["offline_metadata"].is_object());
                break;
            }
            assert_ne!(value["items"][0]["status"], "failed", "{value}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(fixture.state.shutdown.drain(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn playback_presence_counts_playing_devices_between_transfers_without_listening_statistics() {
    let fixture = Fixture::new();
    let auth = authentication();
    let uri = "/account/playback/presence";
    let payload = |client: &str, sequence, playing| {
        serde_json::json!({
            "client_id": client, "sequence": sequence, "playing": playing,
        })
    };
    let capabilities = json(fixture.request("/server/capabilities", None, None).await).await;
    assert_eq!(capabilities["playback_presence"], true);
    assert_eq!(
        post_json(&fixture, uri, None, payload("desktop", 1, true))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let network = fixture.state.process_metrics.network();
    for client in ["desktop", "phone"] {
        let response =
            post_json(&fixture, uri, Some(&auth.alice.1), payload(client, 1, true)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json(response).await["accepted"], true);
    }
    assert_eq!(network.playing_devices(), 2);
    assert_eq!(network.active_streams(), 0);
    let response = post_json(
        &fixture,
        uri,
        Some(&auth.alice.1),
        payload("desktop", 3, false),
    )
    .await;
    assert_eq!(json(response).await["accepted"], true);
    assert_eq!(network.playing_devices(), 1);
    let response = post_json(
        &fixture,
        uri,
        Some(&auth.alice.1),
        payload("desktop", 2, true),
    )
    .await;
    assert_eq!(json(response).await["accepted"], false);
    assert_eq!(network.playing_devices(), 1);
    assert_eq!(
        post_json(&fixture, uri, Some(&auth.alice.1), payload("", 4, true))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    // A console cookie cannot report a playing client.
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(
            header::COOKIE,
            format!("phonolite_session={}", auth.alice.1),
        )
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(payload("browser", 1, true).to_string()))
        .unwrap();
    assert_eq!(
        super::api_router(fixture.state.clone())
            .oneshot(request)
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn playback_presence_clears_immediately_for_logout_and_account_session_revocation() {
    let mut fixture = Fixture::new();
    fixture.state.auth = AuthStore::new(memory_database(), Duration::from_secs(3600));
    let auth = &fixture.state.auth;
    auth.init_tables().unwrap();
    let user = auth
        .create_user("presence-alice", "test-password", UserRole::User)
        .unwrap();
    let current = auth.create_session(&user.id).unwrap().token;
    let other = auth.create_session(&user.id).unwrap().token;
    let logout = auth.create_session(&user.id).unwrap().token;
    let network = fixture.state.process_metrics.network();
    let payload = serde_json::json!({"client_id":"client", "sequence":1, "playing":true});
    for token in [&current, &other, &logout] {
        assert_eq!(
            post_json(
                &fixture,
                "/account/playback/presence",
                Some(token),
                payload.clone()
            )
            .await
            .status(),
            StatusCode::OK
        );
    }
    assert_eq!(network.playing_devices(), 3);
    assert_eq!(
        post_json(
            &fixture,
            "/auth/logout",
            Some(&logout),
            serde_json::json!({})
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(network.playing_devices(), 2);
    assert_eq!(
        post_json(
            &fixture,
            "/account/playback/presence",
            Some(&logout),
            payload.clone()
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        post_json(
            &fixture,
            "/account/sessions/revoke-others",
            Some(&current),
            serde_json::json!({})
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(network.playing_devices(), 1);
    assert_eq!(
        post_json(
            &fixture,
            "/account/playback/presence",
            Some(&other),
            payload
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        post_json(
            &fixture,
            "/account/sessions/revoke-all",
            Some(&current),
            serde_json::json!({})
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(network.playing_devices(), 0);
}

#[tokio::test]
async fn admin_performance_requires_a_current_admin_cookie_and_supports_both_views() {
    async fn request(state: &AppState, uri: &str, token: Option<&str>, view: bool) -> Response {
        let mut request = Request::builder()
            .uri(uri)
            .header(header::HOST, "127.0.0.1:3002");
        if let Some(token) = token {
            request = request.header(header::COOKIE, format!("phonolite_session={token}"));
        }
        if view {
            request = request.header("X-Console-View", "true");
        }
        crate::admin::admin_router(state.clone(), "127.0.0.1:3002".parse().unwrap())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    let mut fixture = Fixture::new();
    fixture.state.auth = AuthStore::new(memory_database(), Duration::from_secs(3600));
    let auth = &fixture.state.auth;
    auth.init_tables().unwrap();
    let setup = request(&fixture.state, "/performance", None, false).await;
    assert_eq!(setup.headers()[header::LOCATION], "/setup");
    assert_eq!(
        request(&fixture.state, "/performance/data", None, false)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );

    let owner = auth
        .create_superadmin("performance-owner", "test-password")
        .unwrap();
    let owner_token = auth.create_console_session(&owner.id).unwrap().token;
    let admin = auth
        .create_user("performance-admin", "test-password", UserRole::Admin)
        .unwrap();
    let admin_token = auth.create_console_session(&admin.id).unwrap().token;
    let user = auth
        .create_user("performance-user", "test-password", UserRole::User)
        .unwrap();
    let user_token = auth.create_console_session(&user.id).unwrap().token;
    assert_eq!(
        json(request(&fixture.state, "/performance", None, true).await).await["redirect"],
        "/login"
    );
    for uri in ["/performance", "/performance/data"] {
        assert_eq!(
            request(&fixture.state, uri, Some(&user_token), false)
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    let network = fixture.state.process_metrics.network();
    network.record_sent(512);
    network.record_received(256);
    let active_stream = network.stream_started();
    let playback_token = auth.create_session(&user.id).unwrap().token;
    for client in ["desktop", "phone"] {
        assert_eq!(
            post_json(
                &fixture,
                "/account/playback/presence",
                Some(&playback_token),
                serde_json::json!({"client_id":client, "sequence":1, "playing":true}),
            )
            .await
            .status(),
            StatusCode::OK
        );
    }
    for token in [&owner_token, &admin_token] {
        let response = request(&fixture.state, "/performance/data", Some(token), false).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, no-store"
        );
        let snapshot = json(response).await;
        assert_eq!(snapshot["pid"], std::process::id());
        assert_eq!(snapshot["interval_ms"], 2000);
        assert_eq!(snapshot["sent_bytes"], 512);
        assert_eq!(snapshot["received_bytes"], 256);
        assert_eq!(snapshot["active_streams"], 2);
        assert!(snapshot["logical_cpus"].as_u64().unwrap() > 0);
        assert!(!snapshot["history"].as_array().unwrap().is_empty());
        assert!(snapshot["history"].as_array().unwrap().len() <= 150);
        let view = json(request(&fixture.state, "/performance", Some(token), true).await).await;
        assert_eq!(view["page"], "performance");
        assert!(view["content"].as_str().unwrap().contains("process-memory"));
        assert!(!view["content"].as_str().unwrap().contains("{{"));
        assert!(view["nav"]
            .as_str()
            .unwrap()
            .contains("href=\"/performance\" aria-current=\"page\""));
    }
    drop(active_stream);
    // Buffered playback remains active after the last network transfer closes.
    assert_eq!(network.active_streams(), 0);
    assert_eq!(fixture.state.process_metrics.snapshot().active_streams, 2);
    for client in ["desktop", "phone"] {
        assert_eq!(
            post_json(
                &fixture,
                "/account/playback/presence",
                Some(&playback_token),
                serde_json::json!({"client_id":client, "sequence":2, "playing":false}),
            )
            .await
            .status(),
            StatusCode::OK
        );
    }
    assert_eq!(
        json(
            request(
                &fixture.state,
                "/performance/data",
                Some(&admin_token),
                false
            )
            .await
        )
        .await["active_streams"],
        0,
    );
    let response = request(&fixture.state, "/performance", Some(&admin_token), false).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers()[header::CONTENT_TYPE]
        .to_str()
        .unwrap()
        .starts_with("text/html"));
    let html = String::from_utf8(
        to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains("Resident memory"));
    assert!(!html.contains("{{"));
    auth.revoke_session(&admin_token).unwrap();
    assert_eq!(
        request(
            &fixture.state,
            "/performance/data",
            Some(&admin_token),
            false
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn summary_route_authenticates_cached_hits_checks_caps_and_separates_revisions() {
    let fixture = Fixture::new();
    let auth = authentication();
    let uri = "/browse/artist-summaries?limit=60&offset=0";
    assert_eq!(
        fixture.request(uri, None, None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let first = fixture.request(uri, Some(&auth.alice.1), None).await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(first.headers()[header::CACHE_CONTROL], "private, no-store");
    let first = json(first).await;
    assert_eq!(first["total"], 1);
    assert!(first["next_offset"].is_null());
    assert!(first["items"][0].get("summary").is_none());
    let hits = fixture.state.catalogue_cache.hit_count();
    let second = fixture.request(uri, Some(&auth.bob.1), None).await;
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(json(second).await, first);
    assert_eq!(fixture.state.catalogue_cache.hit_count(), hits + 1);
    assert_eq!(
        fixture.request(uri, None, Some("*")).await.status(),
        StatusCode::UNAUTHORIZED
    );
    for invalid in [
        "/browse/artist-summaries?limit=0",
        "/browse/artist-summaries?limit=201",
    ] {
        assert_eq!(
            fixture
                .request(invalid, Some(&auth.alice.1), None)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    let library = fixture.state.library_state.read().library.clone().unwrap();
    library
        .update_artist_enrichment("artist", None, &["jazz".into()], None, None, false)
        .unwrap();
    let revision = first["browse_revision"].as_str().unwrap();
    let updated = json(
        fixture
            .request(
                &format!("{uri}&revision={revision}"),
                Some(&auth.alice.1),
                None,
            )
            .await,
    )
    .await;
    assert_eq!(updated["browse_revision"], first["browse_revision"]);
    assert_ne!(
        updated["items"][0]["metadata_revision"],
        first["items"][0]["metadata_revision"]
    );
    assert!(updated["items"][0]["genres"]
        .as_array()
        .unwrap()
        .iter()
        .any(|genre| genre == "Jazz"));
    library.rescan().unwrap();
    assert_eq!(
        fixture
            .request(
                &format!("{uri}&revision={revision}"),
                Some(&auth.alice.1),
                None
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn personal_responses_remain_no_store_and_are_isolated_after_shared_cache_hits() {
    let fixture = Fixture::new();
    let auth = authentication();
    fixture
        .state
        .user_data
        .create_playlist(&auth.alice.0, "Alice only".into(), None, Vec::new())
        .unwrap();
    fixture
        .state
        .user_data
        .create_playlist(&auth.bob.0, "Bob only".into(), None, Vec::new())
        .unwrap();
    let warm = fixture
        .request("/browse/artist-summaries", Some(&auth.alice.1), None)
        .await;
    assert_eq!(warm.status(), StatusCode::OK);
    drop(json(warm).await);
    for (token, expected) in [(&auth.alice.1, "Alice only"), (&auth.bob.1, "Bob only")] {
        let response = fixture
            .request("/library/playlists", Some(token), Some("*"))
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, no-store"
        );
        let value = json(response).await;
        assert_eq!(value.as_array().unwrap().len(), 1);
        assert_eq!(value[0]["name"], expected);
    }
}

#[tokio::test]
async fn artwork_conditional_cache_hits_still_require_authentication() {
    let fixture = Fixture::new();
    let auth = authentication();
    let uri = format!(
        "/library/artists/artist/cover?kind=logo&size=128&rendition=1&v={}",
        fixture.reference
    );
    let response = fixture.request(&uri, Some(&auth.alice.1), None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, max-age=31536000, immutable"
    );
    let etag = response.headers()[header::ETAG]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(!to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap()
        .is_empty());
    let cached = fixture.request(&uri, Some(&auth.bob.1), Some(&etag)).await;
    assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(
        cached.headers()[header::CACHE_CONTROL],
        "private, max-age=31536000, immutable"
    );
    assert_eq!(
        fixture.request(&uri, None, Some(&etag)).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        fixture
            .request(&uri, Some("invalid"), Some(&etag))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let stale = fixture
        .request(
            "/library/artists/artist/cover?kind=logo&size=128&v=logos/old.png",
            Some(&auth.alice.1),
            None,
        )
        .await;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    assert_eq!(stale.headers()[header::CACHE_CONTROL], "private, no-store");
    assert_eq!(
        fixture
            .request(
                "/library/artists/artist/cover?kind=logo&size=129",
                Some(&auth.alice.1),
                None
            )
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
}
