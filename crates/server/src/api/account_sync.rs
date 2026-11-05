//! Account collection notifications are invalidations, not a replay log. Every
//! new connection establishes a subscription before the client reads a snapshot.
use std::convert::Infallible;
use std::time::Duration;

use axum::{
    extract::{FromRef, State},
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        Response,
    },
    Extension, Json,
};
use serde::Serialize;
use tokio::sync::broadcast;

use crate::http::{extract_token, json_error, json_error_response};
use crate::state::{AppState, AuthContext, JsonResult};
use crate::user_data::CollectionSnapshot;

#[derive(Clone)]
pub(crate) struct AccountSyncState {
    user_data: crate::user_data::UserDataStore,
    auth: crate::auth::AuthStore,
    shutdown: crate::shutdown::Shutdown,
}

impl FromRef<AppState> for AccountSyncState {
    fn from_ref(state: &AppState) -> Self {
        Self {
            user_data: state.user_data.clone(),
            auth: state.auth.clone(),
            shutdown: state.shutdown.clone(),
        }
    }
}

#[derive(Serialize)]
struct CollectionEvent {
    kind: &'static str,
    revision: u64,
}

pub async fn collection_snapshot(
    State(state): State<AccountSyncState>,
    Extension(ctx): Extension<AuthContext>,
) -> JsonResult<CollectionSnapshot> {
    let store = state.user_data.clone();
    let snapshot = tokio::task::spawn_blocking(move || store.collection_snapshot(&ctx.user.id))
        .await
        .map_err(|err| {
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("collection read failed: {err}"),
            )
        })?
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    Ok(Json(snapshot))
}

pub async fn collection_events(
    State(state): State<AccountSyncState>,
    Extension(ctx): Extension<AuthContext>,
    headers: HeaderMap,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>, Response> {
    open_collection_events(state, ctx, headers, Duration::from_secs(15))
}

fn open_collection_events(
    state: AccountSyncState,
    ctx: AuthContext,
    headers: HeaderMap,
    period: Duration,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>, Response> {
    let token = extract_token(&headers)
        .ok_or_else(|| json_error_response(StatusCode::UNAUTHORIZED, "unauthorized"))?;
    let subscription = state.user_data.subscribe_collections(&ctx.user.id);
    let access = state.auth.subscribe_access_changes(&ctx.user.id);
    let timer = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    let stream = futures_util::stream::unfold(
        (subscription, state, ctx.user.id, token, true, timer, access),
        |(mut subscription, state, user_id, token, ready, mut timer, mut access)| async move {
            let kind = if ready {
                "ready"
            } else {
                tokio::select! {
                    biased;
                    _ = state.shutdown.cancelled() => return None,
                    _ = access.changed() => "revision",
                    notification = subscription.receiver.recv() => match notification {
                        Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => "collections_changed",
                        Err(broadcast::error::RecvError::Closed) => return None,
                    },
                    _ = timer.tick() => "revision",
                }
            };
            // Authentication at request admission alone does not protect a
            // long-lived response after logout, expiry or account deletion.
            if state.shutdown.is_cancelled()
                || !matches!(state.auth.user_from_token(&token), Ok(Some(user)) if user.id == user_id)
            {
                return None;
            }
            let revision = match state.user_data.collection_revision(&user_id) {
                Ok(revision) => revision,
                Err(err) => {
                    tracing::warn!("Collection revision read failed: {err}");
                    return None;
                }
            };
            let event = Event::default()
                .event("collections")
                .id(revision.to_string())
                .json_data(CollectionEvent { kind, revision })
                .ok()?;
            Some((
                Ok(event),
                (subscription, state, user_id, token, false, timer, access),
            ))
        },
    );
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{header, HeaderValue};
    use axum::response::IntoResponse;
    use futures_util::StreamExt;
    use std::path::PathBuf;
    use std::sync::Arc;

    struct TemporaryDatabase(PathBuf);
    impl Drop for TemporaryDatabase {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    struct Fixture {
        state: AccountSyncState,
        user: crate::auth::AuthUser,
        token: String,
        db: Arc<redb::Database>,
        _cleanup: TemporaryDatabase,
    }

    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "phonolite-account-sync-{}.redb",
                uuid::Uuid::new_v4()
            ));
            let db = Arc::new(redb::Database::create(&path).unwrap());
            let auth = crate::auth::AuthStore::new(db.clone(), Duration::from_secs(60));
            auth.init_tables().unwrap();
            let user = auth
                .create_user("listener", "password", crate::auth::UserRole::User)
                .unwrap();
            let token = auth.create_session(&user.id).unwrap().token;
            let user_data = crate::user_data::UserDataStore::new(db.clone());
            user_data.init_tables().unwrap();
            Self {
                state: AccountSyncState {
                    user_data,
                    auth,
                    shutdown: Default::default(),
                },
                user,
                token,
                db,
                _cleanup: TemporaryDatabase(path),
            }
        }

        fn events(&self, period: Duration) -> axum::body::BodyDataStream {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {}", self.token)).unwrap(),
            );
            open_collection_events(
                self.state.clone(),
                AuthContext {
                    user: self.user.clone(),
                },
                headers,
                period,
            )
            .unwrap()
            .into_response()
            .into_body()
            .into_data_stream()
        }
    }

    async fn event(body: &mut axum::body::BodyDataStream) -> serde_json::Value {
        let chunk = tokio::time::timeout(Duration::from_secs(2), body.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let text = std::str::from_utf8(&chunk).unwrap();
        let data = text
            .lines()
            .find_map(|line| line.strip_prefix("data:"))
            .unwrap();
        serde_json::from_str(data.trim()).unwrap()
    }

    #[tokio::test]
    async fn subscription_precedes_ready_and_notifies_every_session_of_the_same_account() {
        let fixture = Fixture::new();
        let mut first = fixture.events(Duration::from_secs(15));
        let mut second = fixture.events(Duration::from_secs(15));
        fixture
            .state
            .user_data
            .add_like(&fixture.user.id, "before-ready")
            .unwrap();
        assert_eq!(
            event(&mut first).await,
            serde_json::json!({"kind":"ready", "revision":1})
        );
        assert_eq!(event(&mut second).await["kind"], "ready");
        let snapshot = collection_snapshot(
            State(fixture.state.clone()),
            Extension(AuthContext {
                user: fixture.user.clone(),
            }),
        )
        .await
        .unwrap_or_else(|(status, _)| panic!("snapshot failed: {status}"))
        .0;
        assert_eq!(snapshot.revision, 1);
        assert_eq!(snapshot.likes[0].track_id, "before-ready");
        assert_eq!(event(&mut first).await["kind"], "collections_changed");
        assert_eq!(event(&mut second).await["revision"], 1);
    }

    #[tokio::test]
    async fn periodic_revision_repairs_a_commit_without_a_live_notification() {
        let fixture = Fixture::new();
        let mut body = fixture.events(Duration::from_millis(5));
        assert_eq!(event(&mut body).await["revision"], 0);
        // A separate store has no shared in-memory subscriber registry, modelling
        // a missed publish. The persistent account revision still repairs it.
        let detached_store = crate::user_data::UserDataStore::new(fixture.db.clone());
        detached_store
            .add_like(&fixture.user.id, "missed-notification")
            .unwrap();
        assert_eq!(
            event(&mut body).await,
            serde_json::json!({"kind":"revision", "revision":1})
        );
        let snapshot = fixture
            .state
            .user_data
            .collection_snapshot(&fixture.user.id)
            .unwrap();
        assert_eq!(snapshot.likes[0].track_id, "missed-notification");
    }

    #[tokio::test]
    async fn foreign_changes_do_not_reach_the_account_and_revocation_closes_the_stream() {
        let fixture = Fixture::new();
        let mut body = fixture.events(Duration::from_millis(5));
        event(&mut body).await;
        fixture
            .state
            .user_data
            .add_like("another-account", "private-track")
            .unwrap();
        assert_eq!(
            event(&mut body).await,
            serde_json::json!({"kind":"revision", "revision":0})
        );
        fixture.state.auth.revoke_session(&fixture.token).unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(2), body.next())
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn shutdown_ends_an_idle_stream_without_waiting_for_the_heartbeat() {
        let fixture = Fixture::new();
        let mut body = fixture.events(Duration::from_secs(15));
        event(&mut body).await;
        fixture.state.shutdown.cancel();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), body.next())
                .await
                .unwrap()
                .is_none()
        );
    }
}
