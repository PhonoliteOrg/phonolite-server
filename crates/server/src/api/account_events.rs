//! Live invalidation vectors. Durable snapshots remain the source of truth.
use crate::{
    auth::{AuthStore, AuthUser, UserRole},
    http::{extract_token, json_error, json_error_response},
    shutdown::Shutdown,
    state::{AppState, AuthContext, JsonResult},
    stats_store::StatsStore,
    user_data::UserDataStore,
};
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
use std::{convert::Infallible, time::Duration};

#[derive(Clone)]
pub(crate) struct AccountState {
    user_data: UserDataStore,
    stats: StatsStore,
    auth: AuthStore,
    shutdown: Shutdown,
}
impl FromRef<AppState> for AccountState {
    fn from_ref(s: &AppState) -> Self {
        Self {
            user_data: s.user_data.clone(),
            stats: s.stats.clone(),
            auth: s.auth.clone(),
            shutdown: s.shutdown.clone(),
        }
    }
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Revisions {
    collections: u64,
    presets: u64,
    statistics: u64,
    playback: u64,
}
#[derive(Serialize)]
pub struct Snapshot {
    user_id: String,
    username: String,
    role: UserRole,
    revisions: Revisions,
}
#[derive(Serialize)]
struct AccountEvent {
    kind: &'static str,
    revisions: Revisions,
}
impl AccountState {
    fn revisions(&self, user: &str) -> Result<Revisions, String> {
        Ok(Revisions {
            collections: self
                .user_data
                .collection_revision(user)
                .map_err(|e| e.to_string())?,
            presets: self
                .user_data
                .presets_revision(user)
                .map_err(|e| e.to_string())?,
            statistics: self
                .stats
                .notification_revision(user)
                .map_err(|e| e.to_string())?,
            playback: self
                .user_data
                .playback_revision(user)
                .map_err(|e| e.to_string())?,
        })
    }
}
pub async fn snapshot(
    State(state): State<AccountState>,
    Extension(ctx): Extension<AuthContext>,
) -> JsonResult<Snapshot> {
    let user = ctx.user;
    let revisions = tokio::task::spawn_blocking(move || {
        state.revisions(&user.id).map(|revisions| Snapshot {
            user_id: user.id,
            username: user.username,
            role: user.role,
            revisions,
        })
    })
    .await
    .map_err(|e| json_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| json_error(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(revisions))
}
pub async fn events(
    State(state): State<AccountState>,
    Extension(ctx): Extension<AuthContext>,
    headers: HeaderMap,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>, Response> {
    open_events(state, ctx.user, headers, Duration::from_secs(15))
}
fn open_events(
    state: AccountState,
    user: AuthUser,
    headers: HeaderMap,
    period: Duration,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>, Response> {
    let token = extract_token(&headers)
        .ok_or_else(|| json_error_response(StatusCode::UNAUTHORIZED, "unauthorized"))?;
    // Subscribe before ready: mutations racing the first snapshot stay observable.
    let subscription = state.user_data.subscribe_collections(&user.id);
    let stats = state.stats.subscribe_changes(&user.id);
    let access = state.auth.subscribe_access_changes(&user.id);
    let timer = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    let stream = futures_util::stream::unfold(
        (
            state,
            user,
            token,
            subscription,
            stats,
            access,
            timer,
            None::<Revisions>,
        ),
        |(state, mut user, token, mut data, mut stats, mut access, mut timer, mut previous)| async move {
            loop {
                let ready = previous.is_none();
                let periodic = if ready {
                    false
                } else {
                    tokio::select! {
                        biased;
                        _ = state.shutdown.cancelled() => return None,
                        result = access.changed() => { if result.is_err() { return None; } false },
                        result = data.receiver.recv() => { if matches!(result, Err(tokio::sync::broadcast::error::RecvError::Closed)) { return None; } false },
                        result = stats.changed() => { if result.is_err() { return None; } false },
                        _ = timer.tick() => true,
                    }
                };
                let authenticated = state.auth.user_from_token(&token).ok().flatten()?;
                if authenticated.id != user.id || state.shutdown.is_cancelled() {
                    return None;
                }
                let account_changed = authenticated != user;
                user = authenticated;
                let revisions = state.revisions(&user.id).ok()?;
                if !ready && !periodic && !account_changed && previous.as_ref() == Some(&revisions)
                {
                    continue;
                }
                let kind = if ready {
                    "ready"
                } else if account_changed {
                    "account_changed"
                } else {
                    "revision"
                };
                let event = Event::default()
                    .event("account")
                    .json_data(AccountEvent {
                        kind,
                        revisions: revisions.clone(),
                    })
                    .ok()?;
                previous = Some(revisions);
                return Some((
                    Ok(event),
                    (state, user, token, data, stats, access, timer, previous),
                ));
            }
        },
    );
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        http::{header, HeaderValue},
        response::IntoResponse,
    };
    use futures_util::StreamExt;
    use std::sync::Arc;
    struct Fixture {
        state: Option<AccountState>,
        user: AuthUser,
        token: String,
        dbs: Vec<std::sync::Arc<redb::Database>>,
        paths: Vec<std::path::PathBuf>,
    }
    impl Fixture {
        fn new() -> Self {
            let paths = (0..2)
                .map(|_| {
                    std::env::temp_dir().join(format!(
                        "phonolite-account-vector-{}.redb",
                        uuid::Uuid::new_v4()
                    ))
                })
                .collect::<Vec<_>>();
            let db = Arc::new(redb::Database::create(&paths[0]).unwrap());
            let auth = AuthStore::new(db.clone(), Duration::from_secs(60));
            auth.init_tables().unwrap();
            let user = auth
                .create_user("listener", "password", UserRole::User)
                .unwrap();
            let token = auth.create_session(&user.id).unwrap().token;
            let user_data = UserDataStore::new(db.clone());
            user_data.init_tables().unwrap();
            let stats_db = Arc::new(redb::Database::create(&paths[1]).unwrap());
            let stats = StatsStore::new(stats_db.clone());
            stats.init_tables().unwrap();
            Self {
                state: Some(AccountState {
                    user_data,
                    stats,
                    auth,
                    shutdown: Default::default(),
                }),
                user,
                token,
                dbs: vec![db, stats_db],
                paths,
            }
        }
        fn state(&self) -> &AccountState {
            self.state.as_ref().unwrap()
        }
        fn events(&self, period: Duration) -> axum::body::BodyDataStream {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {}", self.token)).unwrap(),
            );
            open_events(self.state().clone(), self.user.clone(), headers, period)
                .unwrap()
                .into_response()
                .into_body()
                .into_data_stream()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.state.take();
            self.dbs.clear();
            for path in &self.paths {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    async fn event(stream: &mut axum::body::BodyDataStream) -> serde_json::Value {
        let chunk = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let text = std::str::from_utf8(&chunk).unwrap();
        serde_json::from_str(
            text.lines()
                .find_map(|l| l.strip_prefix("data:"))
                .unwrap()
                .trim(),
        )
        .unwrap()
    }
    #[tokio::test]
    async fn independent_revisions_wake_all_account_sessions_after_commit() {
        let f = Fixture::new();
        let s = f.state();
        let mut one = f.events(Duration::from_secs(60));
        let mut two = f.events(Duration::from_secs(60));
        s.user_data.add_like(&f.user.id, "track").unwrap();
        assert_eq!(
            event(&mut one).await["revisions"],
            serde_json::json!({"collections":1,"presets":0,"statistics":0,"playback":0})
        );
        event(&mut two).await;
        // Drain the pre-ready queued collection wake (the vector is unchanged).
        s.user_data
            .save_shuffle_preset(&f.user.id, None, "Evening".into(), vec![], vec![], None)
            .unwrap();
        assert_eq!(
            event(&mut one).await["revisions"],
            serde_json::json!({"collections":1,"presets":1,"statistics":0,"playback":0})
        );
        assert_eq!(event(&mut two).await["revisions"]["presets"], 1);
        s.stats.reset_history(&f.user.id, 1).unwrap();
        assert_eq!(
            event(&mut one).await["revisions"],
            serde_json::json!({"collections":1,"presets":1,"statistics":1,"playback":0})
        );
        assert_eq!(event(&mut two).await["revisions"]["statistics"], 1);
        s.user_data
            .record_track_start(&f.user.id, "track", "player", 1, "started")
            .unwrap();
        assert_eq!(event(&mut one).await["revisions"]["playback"], 1);
        assert_eq!(event(&mut two).await["revisions"]["playback"], 1);
    }
    #[tokio::test]
    async fn foreign_mutations_are_silent_and_revocation_closes_without_heartbeat() {
        let f = Fixture::new();
        let mut body = f.events(Duration::from_secs(60));
        event(&mut body).await;
        f.state().user_data.add_like("foreign", "private").unwrap();
        f.state().stats.reset_history("foreign", 1).unwrap();
        f.state()
            .user_data
            .record_track_start("foreign", "private", "player", 1, "start")
            .unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(20), body.next())
            .await
            .is_err());
        f.state().auth.revoke_session(&f.token).unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(2), body.next())
            .await
            .unwrap()
            .is_none());
    }
    #[tokio::test]
    async fn durable_periodic_vectors_repair_missed_notifications_and_restart() {
        let f = Fixture::new();
        let mut body = f.events(Duration::from_millis(5));
        event(&mut body).await;
        let detached = StatsStore::new(f.dbs[1].clone());
        detached.reset_history(&f.user.id, 1).unwrap();
        assert_eq!(event(&mut body).await["revisions"]["statistics"], 1);
        let reopened = UserDataStore::new(f.dbs[0].clone());
        reopened
            .save_shuffle_preset(&f.user.id, None, "Restart".into(), vec![], vec![], None)
            .unwrap();
        assert_eq!(event(&mut body).await["revisions"]["presets"], 1);
    }
}
