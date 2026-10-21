//! Password-confirmed rotation of the identity shared by HTTPS and QUIC.
use axum::{
    extract::{rejection::FormRejection, DefaultBodyLimit, Form, FromRef, State},
    http::{header, HeaderMap, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::post,
    Extension, Json, Router,
};
use serde::{Deserialize, Deserializer};
use std::{path::PathBuf, sync::Arc, time::Duration};
use zeroize::Zeroizing;

use crate::{
    auth::{AuthStore, UserRole},
    http::{extract_session_cookie, json_error_response},
    security::{SecurityState, TlsConnection},
    state::AppState,
};

static ROTATION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone)]
pub(crate) struct RotationState {
    auth: AuthStore,
    security: Arc<SecurityState>,
    config_path: PathBuf,
    events: crate::metadata_events::MetadataEventBus,
    restart: fn(),
}

impl FromRef<AppState> for RotationState {
    fn from_ref(state: &AppState) -> Self {
        Self {
            auth: state.auth.clone(),
            security: state.security.clone(),
            config_path: state.config_path.clone(),
            events: state.metadata_events.clone(),
            restart: restart_after_response,
        }
    }
}

// Never echo or log password fields, including form decoding failures.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RotationForm {
    #[serde(deserialize_with = "secret_string")]
    password: Zeroizing<String>,
    confirm: String,
}

fn secret_string<'de, D: Deserializer<'de>>(value: D) -> Result<Zeroizing<String>, D::Error> {
    String::deserialize(value).map(Zeroizing::new)
}

pub(crate) fn router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    RotationState: FromRef<S>,
{
    Router::new()
        .route("/settings/security/rotate-keys", post(rotate_keys))
        .layer(DefaultBodyLimit::max(8 * 1024))
        .layer(middleware::from_fn(crate::security::deadline))
}

async fn rotate_keys(
    State(state): State<RotationState>,
    tls: Option<Extension<TlsConnection>>,
    headers: HeaderMap,
    payload: Result<Form<RotationForm>, FormRejection>,
) -> Response {
    let Some(Extension(tls)) = tls else {
        return failure(StatusCode::UPGRADE_REQUIRED, "HTTPS is required.");
    };
    let Some(token) = extract_session_cookie(&headers).map(Zeroizing::new) else {
        return failure(StatusCode::UNAUTHORIZED, "Sign in as an administrator.");
    };
    let user = match state.auth.user_from_token(&token) {
        Ok(Some(user)) => user,
        Ok(None) => return failure(StatusCode::UNAUTHORIZED, "Sign in as an administrator."),
        Err(_) => {
            return failure(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Key rotation is unavailable.",
            )
        }
    };
    if !matches!(user.role, UserRole::Admin | UserRole::SuperAdmin) {
        return failure(StatusCode::FORBIDDEN, "Administrator access is required.");
    }
    let payload = match payload {
        Ok(Form(payload))
            if payload.confirm == "rotate"
                && !payload.password.is_empty()
                && payload.password.len() <= 1024 =>
        {
            payload
        }
        _ => {
            return failure(
                StatusCode::BAD_REQUEST,
                "Enter your current password and confirm key regeneration.",
            )
        }
    };
    let permit = match state
        .security
        .reserve_password_change(tls.peer, &user.username.trim().to_ascii_lowercase())
    {
        Ok(permit) => permit,
        Err(status) => return failure(status, "Too many attempts. Try again later."),
    };
    let lock = match ROTATION.try_lock() {
        Ok(lock) => lock,
        Err(_) => return failure(StatusCode::CONFLICT, "Key rotation is already in progress."),
    };
    let worker = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        let (_permit, _lock) = (permit, lock);
        let authenticated = worker
            .auth
            .authenticate(&user.username, &payload.password)
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Key rotation is unavailable.".to_string(),
                )
            })?;
        if authenticated.as_ref().map(|current| current.id.as_str()) != Some(user.id.as_str()) {
            return Err((
                StatusCode::FORBIDDEN,
                "Current password is incorrect.".to_string(),
            ));
        }
        // Recheck authorization after the password KDF, so an expired/revoked
        // session or demoted administrator cannot commit a rotation.
        let current = worker.auth.user_from_token(&token).map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Key rotation is unavailable.".to_string(),
            )
        })?;
        if !current.is_some_and(|current| {
            current.id == user.id && matches!(current.role, UserRole::Admin | UserRole::SuperAdmin)
        }) {
            return Err((
                StatusCode::UNAUTHORIZED,
                "Your session has ended. Sign in again.".to_string(),
            ));
        }
        let fingerprint = crate::tls::prepare_rotation(&worker.config_path, || {
            worker
                .auth
                .clear_sessions()
                .map_err(|error| error.to_string())
        })
        .map_err(|message| (StatusCode::CONFLICT, message))?;
        worker.security.invalidate_all_attempts();
        worker.events.emit_identity_rotation();
        // Scheduling belongs to the worker: cancellation/disposal cannot leave
        // a committed rotation waiting for the HTTP handler to be polled again.
        (worker.restart)();
        Ok::<_, (StatusCode, String)>(fingerprint)
    })
    .await;
    match result {
        Ok(Ok(fingerprint)) => {
            let mut response = Json(serde_json::json!({
                "ok": true,
                "fingerprint": fingerprint,
                "message": "New server keys generated. The server is restarting and everyone has been signed out. Reload this page, verify the new browser certificate, and sign in again. Apps will refresh the server key securely when users sign back in."
            })).into_response();
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
            response
        }
        Ok(Err((status, message))) => failure(status, &message),
        Err(_) => failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not confirm key rotation. Check the server before trying again.",
        ),
    }
}

fn failure(status: StatusCode, message: &str) -> Response {
    let mut response = json_error_response(status, message);
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

fn restart_after_response() {
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Err(error) = crate::process_control::spawn_restart() {
            tracing::error!("Key rotation restart failed; start the server manually: {error}");
        }
        // Stop using the old identity even if launching the replacement fails.
        std::process::exit(0);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    static RESTARTS: AtomicUsize = AtomicUsize::new(0);
    fn record_restart() {
        RESTARTS.fetch_add(1, Ordering::SeqCst);
    }
    struct Directory(PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn request(
        state: &RotationState,
        token: Option<&str>,
        tls: bool,
        body: &str,
    ) -> Response {
        let mut app = router().with_state(state.clone());
        if tls {
            app = app.layer(Extension(TlsConnection {
                binding: [7; 32],
                peer: "127.0.0.1".parse().unwrap(),
            }));
        }
        let mut request = Request::post("/settings/security/rotate-keys")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(token) = token {
            request = request.header(header::COOKIE, format!("phonolite_session={token}"));
        }
        app.oneshot(request.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn rotation_requires_https_admin_password_and_confirmation_then_revokes_every_session() {
        let root =
            std::env::temp_dir().join(format!("phonolite-rotation-api-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let _directory = Directory(root.clone());
        let auth = AuthStore::new(
            Arc::new(redb::Database::create(root.join("auth.redb")).unwrap()),
            Duration::from_secs(3600),
        );
        auth.init_tables().unwrap();
        let admin = auth.create_superadmin("admin", "correct-password").unwrap();
        let listener = auth
            .create_user("listener", "listener-password", UserRole::User)
            .unwrap();
        let admin_token = auth.create_session(&admin.id).unwrap().token;
        let listener_token = auth.create_session(&listener.id).unwrap().token;
        let state = RotationState {
            security: SecurityState::new(auth.opaque_setup().unwrap()),
            auth: auth.clone(),
            config_path: root.join("config.yaml"),
            events: crate::metadata_events::MetadataEventBus::new(16),
            restart: record_restart,
        };
        let body = "password=correct-password&confirm=rotate";
        assert_eq!(
            request(&state, Some(&admin_token), false, body)
                .await
                .status(),
            StatusCode::UPGRADE_REQUIRED
        );
        assert_eq!(
            request(&state, None, true, body).await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            request(&state, Some(&listener_token), true, body)
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(
                &state,
                Some(&admin_token),
                true,
                "password=correct-password"
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
        let rejected = request(
            &state,
            Some(&admin_token),
            true,
            "password=wrong-private-value&confirm=rotate",
        )
        .await;
        assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
        let rejected = to_bytes(rejected.into_body(), 4096).await.unwrap();
        assert!(!String::from_utf8_lossy(&rejected).contains("wrong-private-value"));
        assert!(auth.user_from_token(&admin_token).unwrap().is_some());
        assert!(auth.user_from_token(&listener_token).unwrap().is_some());
        assert!(!root.join("config.yaml.tls-rotation.json").exists());
        let mut notifications = state.events.subscribe();
        let response = request(&state, Some(&admin_token), true, body).await;
        let notification = notifications.try_recv().unwrap();
        assert_eq!(notification.kind, "session_invalidated");
        assert_eq!(notification.id, "server_keys_rotated");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let response = to_bytes(response.into_body(), 8192).await.unwrap();
        assert!(!String::from_utf8_lossy(&response).contains("PRIVATE KEY"));
        let response: serde_json::Value = serde_json::from_slice(&response).unwrap();
        assert_eq!(response["fingerprint"].as_str().unwrap().len(), 64);
        assert!(root.join("config.yaml.tls-rotation.json").exists());
        assert!(auth.user_from_token(&admin_token).unwrap().is_none());
        assert!(auth.user_from_token(&listener_token).unwrap().is_none());
        assert_eq!(
            RESTARTS.load(Ordering::SeqCst),
            1,
            "restart is recorded without launching a process"
        );
        // A lost reply/retry after signing back in cannot overwrite the staged pair.
        let pending = std::fs::read(root.join("config.yaml.tls-rotation.json")).unwrap();
        let new_token = auth.create_session(&admin.id).unwrap().token;
        assert_eq!(
            request(&state, Some(&new_token), true, body).await.status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            pending,
            std::fs::read(root.join("config.yaml.tls-rotation.json")).unwrap()
        );
        assert!(auth.user_from_token(&new_token).unwrap().is_some());
        assert_eq!(RESTARTS.load(Ordering::SeqCst), 1);
    }
}
