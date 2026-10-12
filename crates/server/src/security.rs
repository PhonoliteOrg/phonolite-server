//! Connection-bound OPAQUE authentication. Pending state never survives a restart.
use crate::{
    auth::{AuthStore, SessionToken},
    http::json_error,
    security_protocol::{self as protocol, Suite},
    state::{AppState, JsonResult},
};
use axum::{
    extract::{Extension, State},
    http::StatusCode,
    Json,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use opaque_ke::{
    CredentialFinalization, CredentialRequest, Identifiers, ServerLogin, ServerLoginParameters,
    ServerRegistration, ServerSetup,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::Arc,
    time::{Duration, Instant},
};
type AuthResult<T> = Result<T, (StatusCode, Json<crate::state::ErrorResponse>)>;

pub async fn deadline(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    tokio::time::timeout(Duration::from_secs(15), next.run(request))
        .await
        .unwrap_or_else(|_| {
            crate::http::json_error_response(
                StatusCode::REQUEST_TIMEOUT,
                "authentication request timed out",
            )
        })
}

#[derive(Clone)]
pub struct TlsConnection {
    pub binding: [u8; 32],
    pub peer: IpAddr,
}

struct Attempt {
    login: ServerLogin<Suite>,
    user_id: Option<String>,
    record_hash: Vec<u8>,
    binding: [u8; 32],
    context: Vec<u8>,
    created: Instant,
}

#[derive(Default)]
struct Limits {
    attempts: HashMap<String, Attempt>,
    rates: HashMap<String, (Instant, u32)>,
}

pub struct SecurityState {
    pub setup: ServerSetup<Suite>,
    limits: Mutex<Limits>,
}

impl SecurityState {
    pub fn new(setup: ServerSetup<Suite>) -> Arc<Self> {
        Arc::new(Self {
            setup,
            limits: Mutex::new(Limits::default()),
        })
    }
    fn reserve(&self, peer: IpAddr, username: &str) -> Result<(), StatusCode> {
        let now = Instant::now();
        let mut limits = self.limits.lock();
        limits
            .attempts
            .retain(|_, a| now.duration_since(a.created) < Duration::from_secs(90));
        limits
            .rates
            .retain(|_, (time, _)| now.duration_since(*time) < Duration::from_secs(60));
        if limits.attempts.len() >= 256 || limits.rates.len() > 4096 {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
        for key in [
            format!("ip:{peer}"),
            format!("user:{username}"),
            "global".to_string(),
        ] {
            let rate = limits.rates.entry(key.clone()).or_insert((now, 0));
            rate.1 += 1;
            let max = if key == "global" { 512 } else { 20 };
            if rate.1 > max {
                return Err(StatusCode::TOO_MANY_REQUESTS);
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartRequest {
    pub profile: String,
    pub username: String,
    pub message: String,
}
#[derive(Serialize)]
pub struct StartResponse {
    profile: &'static str,
    attempt: String,
    message: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinishRequest {
    pub attempt: String,
    pub message: String,
}
#[derive(Serialize)]
pub struct FinishResponse {
    pub token: String,
    pub expires_at: u64,
    pub token_type: &'static str,
    pub quic_port: u16,
    pub quic_enabled: bool,
}

pub async fn start(
    State(state): State<AppState>,
    Extension(tls): Extension<TlsConnection>,
    Json(payload): Json<StartRequest>,
) -> JsonResult<StartResponse> {
    begin(&state.auth, &state.security, tls, payload).map(Json)
}

fn begin(
    auth: &AuthStore,
    security: &SecurityState,
    tls: TlsConnection,
    payload: StartRequest,
) -> AuthResult<StartResponse> {
    if payload.profile != protocol::PROFILE
        || payload.username.len() > 128
        || payload.username.trim().is_empty()
        || payload.message.len() > 2048
    {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "unsupported authentication request",
        ));
    }
    let username = payload.username.trim().to_ascii_lowercase();
    security
        .reserve(tls.peer, &username)
        .map_err(|s| json_error(s, "too many sign-in attempts"))?;
    let bytes = STANDARD
        .decode(&payload.message)
        .map_err(|_| json_error(StatusCode::BAD_REQUEST, "invalid handshake"))?;
    let request = CredentialRequest::<Suite>::deserialize(&bytes)
        .map_err(|_| json_error(StatusCode::BAD_REQUEST, "invalid handshake"))?;
    let user = auth.get_user_by_username(&username).map_err(|_| {
        json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "authentication unavailable",
        )
    })?;
    let user = user.filter(|u| !u.disabled);
    let record_bytes = user
        .as_ref()
        .map(|u| auth.opaque_record(&u.id))
        .transpose()
        .map_err(|_| {
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "authentication unavailable",
            )
        })?
        .flatten();
    let record = record_bytes
        .as_ref()
        .and_then(|b| ServerRegistration::<Suite>::deserialize(b).ok());
    let user_id = if record.is_some() {
        user.as_ref().map(|u| u.id.clone())
    } else {
        None
    };
    let id = user_id.as_deref().unwrap_or(&username);
    let context = protocol::context(&tls.binding, &username);
    let result = ServerLogin::start(
        &mut rand08::rngs::OsRng,
        &security.setup,
        record,
        request,
        id.as_bytes(),
        ServerLoginParameters {
            context: Some(&context),
            identifiers: Identifiers {
                client: None,
                server: Some(protocol::SERVER_IDENTITY),
            },
        },
    )
    .map_err(|_| json_error(StatusCode::UNAUTHORIZED, "unable to sign in"))?;
    let attempt = uuid::Uuid::new_v4().to_string();
    let response = StartResponse {
        profile: protocol::PROFILE,
        attempt: attempt.clone(),
        message: STANDARD.encode(result.message.serialize()),
    };
    let mut limits = security.limits.lock();
    if limits.attempts.len() >= 256 {
        return Err(json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "too many sign-in attempts",
        ));
    }
    limits.attempts.insert(
        attempt,
        Attempt {
            login: result.state,
            user_id,
            record_hash: Sha256::digest(record_bytes.unwrap_or_default()).to_vec(),
            binding: tls.binding,
            context,
            created: Instant::now(),
        },
    );
    Ok(response)
}

pub async fn finish(
    State(state): State<AppState>,
    Extension(tls): Extension<TlsConnection>,
    Json(payload): Json<FinishRequest>,
) -> JsonResult<FinishResponse> {
    let session = complete(&state.auth, &state.security, tls, payload)?;
    let config = state.config.read();
    Ok(Json(FinishResponse {
        token: session.token,
        expires_at: session.expires_at,
        token_type: "Bearer",
        quic_port: config.quic_port,
        quic_enabled: config.quic_enabled,
    }))
}

fn complete(
    auth: &AuthStore,
    security: &SecurityState,
    tls: TlsConnection,
    payload: FinishRequest,
) -> AuthResult<SessionToken> {
    let denied = || json_error(StatusCode::UNAUTHORIZED, "unable to sign in");
    if payload.attempt.len() > 64 || payload.message.len() > 2048 {
        return Err(denied());
    }
    let attempt = security
        .limits
        .lock()
        .attempts
        .remove(&payload.attempt)
        .ok_or_else(denied)?;
    if attempt.binding != tls.binding || attempt.created.elapsed() >= Duration::from_secs(90) {
        return Err(denied());
    }
    let bytes = STANDARD.decode(&payload.message).map_err(|_| denied())?;
    let finalization =
        CredentialFinalization::<Suite>::deserialize(&bytes).map_err(|_| denied())?;
    let _secret = attempt
        .login
        .finish(
            finalization,
            ServerLoginParameters {
                context: Some(&attempt.context),
                identifiers: Identifiers {
                    client: None,
                    server: Some(protocol::SERVER_IDENTITY),
                },
            },
        )
        .map_err(|_| denied())?;
    let id = attempt.user_id.ok_or_else(denied)?;
    auth.create_bound_session(&id, &attempt.record_hash)
        .map_err(|_| denied())
}

#[cfg(test)]
mod tests {
    use super::*;
    use opaque_ke::{ClientLogin, ClientLoginFinishParameters, CredentialResponse};
    struct Fixture {
        auth: AuthStore,
        security: Arc<SecurityState>,
        _cleanup: TemporaryDatabase,
    }
    struct TemporaryDatabase(std::path::PathBuf);
    impl Drop for TemporaryDatabase {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("phonolite-opaque-{}.redb", uuid::Uuid::new_v4()));
            let auth = AuthStore::new(
                Arc::new(redb::Database::create(&path).unwrap()),
                Duration::from_secs(60),
            );
            auth.init_tables().unwrap();
            auth.create_superadmin("listener", "correct password")
                .unwrap();
            let security = SecurityState::new(auth.opaque_setup().unwrap());
            Self {
                auth,
                security,
                _cleanup: TemporaryDatabase(path),
            }
        }
    }
    fn tls(byte: u8) -> TlsConnection {
        TlsConnection {
            binding: [byte; 32],
            peer: "127.0.0.1".parse().unwrap(),
        }
    }
    fn proof(
        f: &Fixture,
        username: &str,
        password: &str,
        client_binding: u8,
    ) -> Option<FinishRequest> {
        let mut rng = rand08::rngs::OsRng;
        let first = ClientLogin::<Suite>::start(&mut rng, password.as_bytes()).unwrap();
        let start = begin(
            &f.auth,
            &f.security,
            tls(1),
            StartRequest {
                profile: protocol::PROFILE.into(),
                username: username.into(),
                message: STANDARD.encode(first.message.serialize()),
            },
        )
        .unwrap_or_else(|_| panic!("start failed"));
        let message =
            CredentialResponse::<Suite>::deserialize(&STANDARD.decode(start.message).unwrap())
                .unwrap();
        let context = protocol::context(&[client_binding; 32], username);
        let hash = protocol::ksf();
        let result = first
            .state
            .finish(
                &mut rng,
                password.as_bytes(),
                message,
                ClientLoginFinishParameters::new(
                    Some(&context),
                    Identifiers {
                        client: None,
                        server: Some(protocol::SERVER_IDENTITY),
                    },
                    Some(&hash),
                ),
            )
            .ok()?;
        Some(FinishRequest {
            attempt: start.attempt,
            message: STANDARD.encode(result.message.serialize()),
        })
    }
    #[test]
    fn opaque_login_creates_session_and_replay_is_rejected() {
        let f = Fixture::new();
        let proof = proof(&f, "Listener", "correct password", 1).unwrap();
        let replay = FinishRequest {
            attempt: proof.attempt.clone(),
            message: proof.message.clone(),
        };
        let session = complete(&f.auth, &f.security, tls(1), proof)
            .unwrap_or_else(|_| panic!("finish failed"));
        assert!(f.auth.user_from_token(&session.token).unwrap().is_some());
        assert!(complete(&f.auth, &f.security, tls(1), replay).is_err());
    }
    #[test]
    fn incorrect_password_unknown_user_and_relay_binding_fail() {
        let f = Fixture::new();
        assert!(proof(&f, "listener", "wrong password", 1).is_none());
        assert!(proof(&f, "unknown", "correct password", 1).is_none());
        assert!(proof(&f, "listener", "correct password", 2).is_none());
    }
    #[test]
    fn switching_tls_connection_or_modifying_final_proof_fails() {
        let f = Fixture::new();
        let p = proof(&f, "listener", "correct password", 1).unwrap();
        assert!(complete(&f.auth, &f.security, tls(2), p).is_err());
        let mut p = proof(&f, "listener", "correct password", 1).unwrap();
        let mut bytes = STANDARD.decode(p.message).unwrap();
        bytes[0] ^= 1;
        p.message = STANDARD.encode(bytes);
        assert!(complete(&f.auth, &f.security, tls(1), p).is_err());
    }
    #[test]
    fn password_reset_invalidates_pending_proof_and_existing_session() {
        let f = Fixture::new();
        let p = proof(&f, "listener", "correct password", 1).unwrap();
        let user = f.auth.get_user_by_username("listener").unwrap().unwrap();
        let session = f.auth.create_session(&user.id).unwrap();
        f.auth
            .update_password(&user.id, "replacement password")
            .unwrap();
        assert!(complete(&f.auth, &f.security, tls(1), p).is_err());
        assert!(f.auth.user_from_token(&session.token).unwrap().is_none());
    }
    #[test]
    fn attempts_expire_and_online_guesses_are_bounded() {
        let f = Fixture::new();
        let p = proof(&f, "listener", "correct password", 1).unwrap();
        f.security
            .limits
            .lock()
            .attempts
            .get_mut(&p.attempt)
            .unwrap()
            .created -= Duration::from_secs(91);
        assert!(complete(&f.auth, &f.security, tls(1), p).is_err());
        for _ in 0..19 {
            assert!(f.security.reserve(tls(1).peer, "listener").is_ok());
        }
        assert_eq!(
            f.security.reserve(tls(1).peer, "listener"),
            Err(StatusCode::TOO_MANY_REQUESTS)
        );
    }
}
