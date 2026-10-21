use axum::{
    extract::{rejection::JsonRejection, DefaultBodyLimit, Extension, FromRef, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::{Deserialize, Deserializer};
use std::sync::Arc;
use zeroize::Zeroizing;

use crate::auth::{AuthError, AuthStore};
use crate::http::extract_token;
use crate::http::json_error_response;
use crate::security::{SecurityState, TlsConnection};
use crate::state::{AppState, HealthResponse};

#[derive(Clone)]
pub(crate) struct PasswordChangeState {
    auth: AuthStore,
    security: Arc<SecurityState>,
}

impl FromRef<AppState> for PasswordChangeState {
    fn from_ref(state: &AppState) -> Self {
        Self {
            auth: state.auth.clone(),
            security: state.security.clone(),
        }
    }
}

// Never derive Debug or Serialize for a request containing passwords.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PasswordChangeRequest {
    #[serde(deserialize_with = "secret_string")]
    current_password: Zeroizing<String>,
    #[serde(deserialize_with = "secret_string")]
    new_password: Zeroizing<String>,
}

fn secret_string<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Zeroizing<String>, D::Error> {
    String::deserialize(deserializer).map(Zeroizing::new)
}

pub(crate) fn password_router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    PasswordChangeState: FromRef<S>,
{
    Router::new()
        .route("/auth/password", post(auth_password))
        // Two 1024-byte strings can use six bytes per escaped ASCII character.
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(middleware::from_fn(crate::security::deadline))
        .layer(middleware::from_fn(password_no_store))
}

async fn password_no_store(request: axum::extract::Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn password_bearer(headers: &HeaderMap) -> Option<Zeroizing<String>> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    let token = value.strip_prefix("Bearer ")?.trim();
    if token.is_empty() || token.len() > 128 || token.chars().any(char::is_whitespace) {
        return None;
    }
    Some(Zeroizing::new(token.to_owned()))
}

async fn auth_password(
    State(state): State<PasswordChangeState>,
    tls: Option<Extension<TlsConnection>>,
    headers: HeaderMap,
    payload: Result<Json<PasswordChangeRequest>, JsonRejection>,
) -> Response {
    let Some(Extension(tls)) = tls else {
        return json_error_response(StatusCode::UPGRADE_REQUIRED, "authenticated HTTPS required");
    };
    // Cookie sessions intentionally cannot authorize this endpoint: an explicit
    // bearer header keeps browser cross-site requests from changing passwords.
    let Some(token) = password_bearer(&headers) else {
        return json_error_response(StatusCode::UNAUTHORIZED, "unauthorized");
    };
    let user = match state.auth.user_from_token(&token) {
        Ok(Some(user)) => user,
        Ok(None) => return json_error_response(StatusCode::UNAUTHORIZED, "unauthorized"),
        Err(_) => {
            return json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "password change unavailable",
            )
        }
    };
    let payload = match payload {
        Ok(Json(payload)) => payload,
        // Never forward deserialization errors, which can contain submitted data.
        Err(error) => {
            let status = if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
                StatusCode::PAYLOAD_TOO_LARGE
            } else {
                StatusCode::BAD_REQUEST
            };
            return json_error_response(status, "invalid password change request");
        }
    };
    let permit = match state
        .security
        .reserve_password_change(tls.peer, &user.username.trim().to_ascii_lowercase())
    {
        Ok(permit) => permit,
        Err(status) => return json_error_response(status, "too many password change attempts"),
    };
    let auth = state.auth.clone();
    let result = tokio::task::spawn_blocking(move || {
        // The permit outlives the HTTP request if a timeout/cancellation occurs.
        let _permit = permit;
        auth.change_own_password(&token, &payload.current_password, &payload.new_password)
    })
    .await;
    match result {
        Ok(Ok(())) => {
            state.security.invalidate_user_attempts(&user.id);
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(Err(AuthError::CurrentPasswordMismatch)) => {
            json_error_response(StatusCode::FORBIDDEN, "current password is incorrect")
        }
        Ok(Err(AuthError::InvalidPassword)) => json_error_response(
            StatusCode::BAD_REQUEST,
            "password must be nonempty and contain at most 1024 UTF-8 bytes",
        ),
        Ok(Err(AuthError::InvalidToken | AuthError::TokenExpired | AuthError::UserNotFound)) => {
            json_error_response(StatusCode::UNAUTHORIZED, "unauthorized")
        }
        _ => json_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "password change unavailable",
        ),
    }
}

pub async fn auth_logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let token = match extract_token(&headers) {
        Some(token) => token,
        None => return json_error_response(StatusCode::BAD_REQUEST, "missing token"),
    };

    if let Err(err) = state.auth.revoke_session(&token) {
        return json_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("auth error: {}", err),
        );
    }

    Json(HealthResponse { status: "ok" }).into_response()
}

#[cfg(test)]
mod password_tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use serde_json::json;
    use std::{path::PathBuf, time::Duration};
    use tower::ServiceExt;

    struct Fixture {
        state: PasswordChangeState,
        user: crate::auth::AuthUser,
        token: String,
        _cleanup: TemporaryDatabase,
    }
    struct TemporaryDatabase(PathBuf);
    impl Drop for TemporaryDatabase {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "phonolite-password-api-{}.redb",
                uuid::Uuid::new_v4()
            ));
            let auth = AuthStore::new(
                Arc::new(redb::Database::create(&path).unwrap()),
                Duration::from_secs(60),
            );
            auth.init_tables().unwrap();
            let user = auth
                .create_user("listener", "old-password", crate::auth::UserRole::User)
                .unwrap();
            let token = auth.create_session(&user.id).unwrap().token;
            let security = SecurityState::new(auth.opaque_setup().unwrap());
            Self {
                state: PasswordChangeState { auth, security },
                user,
                token,
                _cleanup: TemporaryDatabase(path),
            }
        }
        fn router(&self, tls: bool) -> Router {
            let router = password_router().with_state(self.state.clone());
            if tls {
                router.layer(Extension(TlsConnection {
                    binding: [1; 32],
                    peer: "127.0.0.1".parse().unwrap(),
                }))
            } else {
                router
            }
        }
    }
    fn request(token: Option<&str>, body: impl Into<Body>) -> Request<Body> {
        let mut request = Request::builder()
            .method("POST")
            .uri("/auth/password")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        request.body(body.into()).unwrap()
    }
    fn body(current: &str, new: &str) -> String {
        json!({"current_password": current, "new_password": new}).to_string()
    }
    async fn check_response(response: Response, status: StatusCode) {
        assert_eq!(response.status(), status);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let bytes = axum::body::to_bytes(response.into_body(), 2048)
            .await
            .unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        for secret in [
            "old-password",
            "replacement password",
            "do-not-echo-this-secret",
        ] {
            assert!(!text.contains(secret));
        }
        if status == StatusCode::NO_CONTENT {
            assert!(bytes.is_empty());
        }
    }

    #[tokio::test]
    async fn password_endpoint_keeps_current_session_and_revokes_other_owner_sessions() {
        let f = Fixture::new();
        let other = f
            .state
            .auth
            .create_user("other", "other-old-password", crate::auth::UserRole::User)
            .unwrap();
        let other_session = f.state.auth.create_session(&other.id).unwrap();
        let owner_second = f.state.auth.create_session(&f.user.id).unwrap();
        let router = f.router(true);
        check_response(
            router
                .clone()
                .oneshot(request(
                    Some(&f.token),
                    body("incorrect", "replacement password"),
                ))
                .await
                .unwrap(),
            StatusCode::FORBIDDEN,
        )
        .await;
        check_response(
            router
                .clone()
                .oneshot(request(
                    Some(&f.token),
                    body("old-password", "replacement password"),
                ))
                .await
                .unwrap(),
            StatusCode::NO_CONTENT,
        )
        .await;
        assert!(f
            .state
            .auth
            .authenticate("listener", "replacement password")
            .unwrap()
            .is_some());
        assert!(f
            .state
            .auth
            .authenticate("listener", "old-password")
            .unwrap()
            .is_none());
        assert!(f
            .state
            .auth
            .user_from_token(&owner_second.token)
            .unwrap()
            .is_none());
        assert!(f
            .state
            .auth
            .user_from_token(&other_session.token)
            .unwrap()
            .is_some());
        assert!(f.state.auth.user_from_token(&f.token).unwrap().is_some());
        check_response(
            router
                .clone()
                .oneshot(request(
                    Some(&f.token),
                    body("old-password", "another replacement password"),
                ))
                .await
                .unwrap(),
            StatusCode::FORBIDDEN,
        )
        .await;
        check_response(
            router
                .oneshot(request(
                    Some(&owner_second.token),
                    body("replacement password", "another replacement password"),
                ))
                .await
                .unwrap(),
            StatusCode::UNAUTHORIZED,
        )
        .await;
    }

    #[tokio::test]
    async fn password_endpoint_requires_tls_and_explicit_single_bearer_header() {
        let f = Fixture::new();
        check_response(
            f.router(false)
                .oneshot(request(
                    Some(&f.token),
                    body("old-password", "replacement password"),
                ))
                .await
                .unwrap(),
            StatusCode::UPGRADE_REQUIRED,
        )
        .await;
        let mut cookie = request(None, body("old-password", "replacement password"));
        cookie.headers_mut().insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("phonolite_session={}", f.token)).unwrap(),
        );
        check_response(
            f.router(true).oneshot(cookie).await.unwrap(),
            StatusCode::UNAUTHORIZED,
        )
        .await;
        let mut duplicate = request(Some(&f.token), body("old-password", "replacement password"));
        duplicate.headers_mut().append(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", f.token)).unwrap(),
        );
        check_response(
            f.router(true).oneshot(duplicate).await.unwrap(),
            StatusCode::UNAUTHORIZED,
        )
        .await;
    }

    #[tokio::test]
    async fn password_endpoint_accepts_single_character_and_unchanged_passwords() {
        for replacement in ["a", "界", "old-password"] {
            let f = Fixture::new();
            let second_session = f.state.auth.create_session(&f.user.id).unwrap();
            check_response(
                f.router(true)
                    .oneshot(request(Some(&f.token), body("old-password", replacement)))
                    .await
                    .unwrap(),
                StatusCode::NO_CONTENT,
            )
            .await;
            assert!(f
                .state
                .auth
                .authenticate("listener", replacement)
                .unwrap()
                .is_some());
            assert_eq!(
                f.state
                    .auth
                    .authenticate("listener", "old-password")
                    .unwrap()
                    .is_some(),
                replacement == "old-password"
            );
            assert!(f.state.auth.user_from_token(&f.token).unwrap().is_some());
            assert!(f
                .state
                .auth
                .user_from_token(&second_session.token)
                .unwrap()
                .is_none());
        }
    }

    #[tokio::test]
    async fn password_endpoint_rejects_target_accounts_malformed_payloads_and_oversized_body() {
        let f = Fixture::new();
        let router = f.router(true);
        for payload in [
            json!({"current_password":"old-password", "new_password":"replacement password", "user_id": "other"}).to_string(),
            json!({"current_password":"old-password", "new_password":"replacement password", "do-not-echo-this-secret": true}).to_string(),
            "{\"current_password\":\"do-not-echo-this-secret\",\"new_password\": [}".into(),
            body("old-password", ""),
            body("old-password", &"界".repeat(342)),
        ] {
            check_response(router.clone().oneshot(request(Some(&f.token), payload)).await.unwrap(), StatusCode::BAD_REQUEST).await;
        }
        check_response(
            router
                .oneshot(request(Some(&f.token), "x".repeat(16 * 1024 + 1)))
                .await
                .unwrap(),
            StatusCode::PAYLOAD_TOO_LARGE,
        )
        .await;
        assert!(f
            .state
            .auth
            .authenticate("listener", "old-password")
            .unwrap()
            .is_some());
        assert!(f.state.auth.user_from_token(&f.token).unwrap().is_some());
    }
}
