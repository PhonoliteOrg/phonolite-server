// crates/server/src/admin/users.rs
use std::collections::HashMap;

use axum::{
    extract::{Form, Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};

use crate::admin::presentation::{
    apply_template, escape_html, html_error, load_template, render_admin_response, PageLayout,
};
use crate::auth::{self, AuthUser, UserActivity, UserLogin, UserRole};
use crate::http::{json_error_response, redirect_to, wants_json};
use crate::state::{AppState, BulkDeleteForm, NewUserForm, PasswordForm, UpdateUserForm};

use super::auth::{ensure_auth_mutation_running, run_auth_commit};
use super::{admin_user_from_headers, is_admin};

pub async fn admin_users(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !state.auth.has_admin().unwrap_or(false) {
        return redirect_to("/setup");
    }
    let user = match admin_user_from_headers(&state, &headers) {
        Ok(Some(user)) => user,
        Ok(None) => return redirect_to("/login"),
        Err(err) => {
            return html_error(
                &state,
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("auth error: {}", err),
            )
        }
    };
    if !is_admin(&user) {
        return html_error(&state, StatusCode::FORBIDDEN, "forbidden".to_string());
    }
    admin_users_page(&state, &user, None)
}

pub async fn admin_user_activity(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match admin_user_from_headers(&state, &headers) {
        Ok(Some(user)) if is_admin(&user) => {}
        _ => return json_error_response(StatusCode::UNAUTHORIZED, "unauthorized"),
    }
    match state.auth.user_activity() {
        Ok(activity) => {
            let sessions: HashMap<_, _> = activity
                .into_iter()
                .map(|(id, activity)| (id, activity.active_sessions))
                .collect();
            Json(serde_json::json!({ "sessions": sessions })).into_response()
        }
        Err(error) => json_error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

pub async fn admin_create_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<NewUserForm>,
) -> Response {
    if let Err(response) = ensure_auth_mutation_running(&state) {
        return response;
    }
    let user = match admin_user_from_headers(&state, &headers) {
        Ok(Some(user)) => user,
        Ok(None) => return redirect_to("/login"),
        Err(err) => {
            return html_error(
                &state,
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("auth error: {}", err),
            )
        }
    };
    if !is_admin(&user) {
        return html_error(&state, StatusCode::FORBIDDEN, "forbidden".to_string());
    }

    let role = match form.role.as_deref() {
        Some("admin") => UserRole::Admin,
        Some("user") | None => UserRole::User,
        Some("superadmin") => {
            if wants_json(&headers) {
                return json_error_response(StatusCode::FORBIDDEN, "superadmin role is reserved");
            }
            return admin_users_page(
                &state,
                &user,
                Some("superadmin role is reserved".to_string()),
            );
        }
        Some(_) => UserRole::User,
    };

    let result = match run_auth_commit(&state, "admin_user_create", move |store| {
        store.create_user(&form.username, &form.password, role)
    })
    .await
    {
        Ok(result) => result,
        Err(response) => return response,
    };
    match result {
        Ok(_) => {
            if wants_json(&headers) {
                users_update_response(&state, &user)
            } else {
                redirect_to("/users")
            }
        }
        Err(err) => {
            let message = match err {
                auth::AuthError::UserExists => "username already exists".to_string(),
                _ => format!("create user failed: {}", err),
            };
            if wants_json(&headers) {
                let status = match err {
                    auth::AuthError::UserExists => StatusCode::CONFLICT,
                    auth::AuthError::InvalidUsername | auth::AuthError::InvalidPassword => {
                        StatusCode::BAD_REQUEST
                    }
                    auth::AuthError::SuperAdminProtected => StatusCode::FORBIDDEN,
                    _ => StatusCode::INTERNAL_SERVER_ERROR,
                };
                json_error_response(status, message)
            } else {
                admin_users_page(&state, &user, Some(message))
            }
        }
    }
}

pub async fn admin_set_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(user_id): AxumPath<String>,
    Form(form): Form<PasswordForm>,
) -> Response {
    if let Err(response) = ensure_auth_mutation_running(&state) {
        return response;
    }
    let user = match admin_user_from_headers(&state, &headers) {
        Ok(Some(user)) => user,
        Ok(None) => return redirect_to("/login"),
        Err(err) => {
            return html_error(
                &state,
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("auth error: {}", err),
            )
        }
    };
    if !is_admin(&user) {
        return html_error(&state, StatusCode::FORBIDDEN, "forbidden".to_string());
    }

    let target = match state.auth.get_user(&user_id) {
        Ok(Some(target)) => target,
        Ok(None) => {
            if wants_json(&headers) {
                return json_error_response(StatusCode::NOT_FOUND, "user not found");
            }
            return admin_users_page(&state, &user, Some("user not found".to_string()));
        }
        Err(err) => {
            if wants_json(&headers) {
                return json_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("password update failed: {}", err),
                );
            }
            return admin_users_page(
                &state,
                &user,
                Some(format!("password update failed: {}", err)),
            );
        }
    };

    if target.role == UserRole::SuperAdmin && target.id != user.id {
        if wants_json(&headers) {
            return json_error_response(
                StatusCode::FORBIDDEN,
                "superadmin can only edit its own account",
            );
        }
        return admin_users_page(
            &state,
            &user,
            Some("superadmin can only edit its own account".to_string()),
        );
    }

    let result = match run_auth_commit(&state, "admin_password_update", move |store| {
        store.update_password(&user_id, &form.password)
    })
    .await
    {
        Ok(result) => result,
        Err(response) => return response,
    };
    match result {
        Ok(_) => {
            if wants_json(&headers) {
                users_update_response(&state, &user)
            } else {
                redirect_to("/users")
            }
        }
        Err(err) => {
            if wants_json(&headers) {
                json_error_response(
                    StatusCode::BAD_REQUEST,
                    format!("password update failed: {}", err),
                )
            } else {
                admin_users_page(
                    &state,
                    &user,
                    Some(format!("password update failed: {}", err)),
                )
            }
        }
    }
}

pub async fn admin_delete_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(user_id): AxumPath<String>,
) -> Response {
    if let Err(response) = ensure_auth_mutation_running(&state) {
        return response;
    }
    let user = match admin_user_from_headers(&state, &headers) {
        Ok(Some(user)) => user,
        Ok(None) => return redirect_to("/login"),
        Err(err) => {
            return html_error(
                &state,
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("auth error: {}", err),
            )
        }
    };
    if !is_admin(&user) {
        return html_error(&state, StatusCode::FORBIDDEN, "forbidden".to_string());
    }

    let personal = state.user_data.clone();
    let shuffle = state.shuffle_sessions.clone();
    let result = match run_auth_commit(&state, "admin_user_delete", move |store| {
        store.delete_user(&user_id)?;
        personal
            .purge_playback(&user_id)
            .map_err(|err| auth::AuthError::DbError(err.to_string()))?;
        shuffle.purge_user(&user_id);
        Ok(())
    })
    .await
    {
        Ok(result) => result,
        Err(response) => return response,
    };
    match result {
        Ok(_) => {
            if wants_json(&headers) {
                users_update_response(&state, &user)
            } else {
                redirect_to("/users")
            }
        }
        Err(err) => {
            if wants_json(&headers) {
                let status = match err {
                    auth::AuthError::SuperAdminProtected => StatusCode::FORBIDDEN,
                    auth::AuthError::LastAdmin => StatusCode::CONFLICT,
                    auth::AuthError::UserNotFound => StatusCode::NOT_FOUND,
                    _ => StatusCode::BAD_REQUEST,
                };
                json_error_response(status, format!("delete failed: {}", err))
            } else {
                admin_users_page(&state, &user, Some(format!("delete failed: {}", err)))
            }
        }
    }
}

pub async fn admin_revoke_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(user_id): AxumPath<String>,
) -> Response {
    if let Err(response) = ensure_auth_mutation_running(&state) {
        return response;
    }
    let actor = match admin_user_from_headers(&state, &headers) {
        Ok(Some(actor)) if is_admin(&actor) => actor,
        _ => return json_error_response(StatusCode::UNAUTHORIZED, "unauthorized"),
    };
    let target = match state.auth.get_user(&user_id) {
        Ok(Some(target)) => target,
        Ok(None) => return json_error_response(StatusCode::NOT_FOUND, "user not found"),
        Err(err) => return json_error_response(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    };
    if target.role == UserRole::SuperAdmin && target.id != actor.id {
        return json_error_response(
            StatusCode::FORBIDDEN,
            "superadmin can only edit its own account",
        );
    }
    match run_auth_commit(&state, "admin_revoke_sessions", move |store| {
        store.revoke_account_sessions(&user_id)
    })
    .await
    {
        Ok(Ok(_)) if wants_json(&headers) => users_update_response(&state, &actor),
        Ok(Ok(_)) => redirect_to("/users"),
        Ok(Err(err)) => json_error_response(StatusCode::BAD_REQUEST, err.to_string()),
        Err(response) => response,
    }
}

pub async fn admin_update_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(user_id): AxumPath<String>,
    Form(form): Form<UpdateUserForm>,
) -> Response {
    if let Err(response) = ensure_auth_mutation_running(&state) {
        return response;
    }
    let actor = match admin_user_from_headers(&state, &headers) {
        Ok(Some(user)) => user,
        Ok(None) => return redirect_to("/login"),
        Err(err) => {
            return html_error(
                &state,
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("auth error: {}", err),
            )
        }
    };
    if !is_admin(&actor) {
        return html_error(&state, StatusCode::FORBIDDEN, "forbidden".to_string());
    }

    let target = match state.auth.get_user(&user_id) {
        Ok(Some(target)) => target,
        Ok(None) => {
            if wants_json(&headers) {
                return json_error_response(StatusCode::NOT_FOUND, "user not found");
            }
            return admin_users_page(&state, &actor, Some("user not found".to_string()));
        }
        Err(err) => {
            if wants_json(&headers) {
                return json_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("update failed: {}", err),
                );
            }
            return admin_users_page(&state, &actor, Some(format!("update failed: {}", err)));
        }
    };

    if target.role == UserRole::SuperAdmin && target.id != actor.id {
        if wants_json(&headers) {
            return json_error_response(
                StatusCode::FORBIDDEN,
                "superadmin can only edit its own account",
            );
        }
        return admin_users_page(
            &state,
            &actor,
            Some("superadmin can only edit its own account".to_string()),
        );
    }

    let role = match form.role.as_str() {
        "admin" => UserRole::Admin,
        "user" => UserRole::User,
        "superadmin" => {
            if target.role == UserRole::SuperAdmin {
                UserRole::SuperAdmin
            } else {
                if wants_json(&headers) {
                    return json_error_response(
                        StatusCode::FORBIDDEN,
                        "superadmin role is reserved",
                    );
                }
                return admin_users_page(
                    &state,
                    &actor,
                    Some("superadmin role is reserved".to_string()),
                );
            }
        }
        _ => UserRole::User,
    };
    let role = if target.role == UserRole::SuperAdmin {
        UserRole::SuperAdmin
    } else {
        role
    };
    let password = form.password.trim();
    let password = if password.is_empty() {
        None
    } else {
        Some(password.to_string())
    };

    let result = match run_auth_commit(&state, "admin_user_update", move |store| {
        store.update_user(&user_id, &form.username, password.as_deref(), role)
    })
    .await
    {
        Ok(result) => result,
        Err(response) => return response,
    };
    match result {
        Ok(_) => {
            if wants_json(&headers) {
                users_update_response(&state, &actor)
            } else {
                redirect_to("/users")
            }
        }
        Err(err) => {
            let message = match err {
                auth::AuthError::UserExists => "username already exists".to_string(),
                _ => format!("update failed: {}", err),
            };
            if wants_json(&headers) {
                let status = match err {
                    auth::AuthError::UserExists => StatusCode::CONFLICT,
                    auth::AuthError::InvalidUsername | auth::AuthError::InvalidPassword => {
                        StatusCode::BAD_REQUEST
                    }
                    auth::AuthError::SuperAdminProtected => StatusCode::FORBIDDEN,
                    auth::AuthError::UserNotFound => StatusCode::NOT_FOUND,
                    _ => StatusCode::INTERNAL_SERVER_ERROR,
                };
                json_error_response(status, message)
            } else {
                admin_users_page(&state, &actor, Some(message))
            }
        }
    }
}

pub async fn admin_bulk_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<BulkDeleteForm>,
) -> Response {
    if let Err(response) = ensure_auth_mutation_running(&state) {
        return response;
    }
    let actor = match admin_user_from_headers(&state, &headers) {
        Ok(Some(user)) => user,
        Ok(None) => return redirect_to("/login"),
        Err(err) => {
            return html_error(
                &state,
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("auth error: {}", err),
            )
        }
    };
    if !is_admin(&actor) {
        return html_error(&state, StatusCode::FORBIDDEN, "forbidden".to_string());
    }

    let ids: Vec<String> = form
        .user_ids
        .split(',')
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .collect();
    if ids.is_empty() {
        if wants_json(&headers) {
            return json_error_response(StatusCode::BAD_REQUEST, "no users selected");
        }
        return admin_users_page(&state, &actor, Some("no users selected".to_string()));
    }

    let personal = state.user_data.clone();
    let shuffle = state.shuffle_sessions.clone();
    let result = match run_auth_commit(&state, "admin_users_bulk_delete", move |store| {
        for user_id in ids {
            store.delete_user(&user_id)?;
            personal
                .purge_playback(&user_id)
                .map_err(|err| auth::AuthError::DbError(err.to_string()))?;
            shuffle.purge_user(&user_id);
        }
        Ok(())
    })
    .await
    {
        Ok(result) => result,
        Err(response) => return response,
    };
    if let Err(err) = result {
        if wants_json(&headers) {
            let status = match err {
                auth::AuthError::SuperAdminProtected => StatusCode::FORBIDDEN,
                auth::AuthError::LastAdmin => StatusCode::CONFLICT,
                auth::AuthError::UserNotFound => StatusCode::NOT_FOUND,
                _ => StatusCode::BAD_REQUEST,
            };
            return json_error_response(status, format!("delete failed: {}", err));
        }
        return admin_users_page(&state, &actor, Some(format!("delete failed: {}", err)));
    }

    if wants_json(&headers) {
        users_update_response(&state, &actor)
    } else {
        redirect_to("/users")
    }
}

pub(crate) fn admin_users_page(
    state: &AppState,
    current_user: &AuthUser,
    message: Option<String>,
) -> Response {
    let user_rows = match render_user_rows(state, current_user) {
        Ok(rows) => rows,
        Err(error) => return html_error(state, StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    let template = match load_template(state, "templates/users.html") {
        Ok(template) => template,
        Err(err) => {
            return html_error(
                state,
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("template error: {}", err),
            )
        }
    };

    let modals = [
        load_template(state, "templates/modals/user_add.html").unwrap_or_default(),
        load_template(state, "templates/modals/user_edit.html").unwrap_or_default(),
        load_template(state, "templates/modals/user_delete.html").unwrap_or_default(),
        load_template(state, "templates/modals/user_sessions.html").unwrap_or_default(),
    ]
    .join("");

    let error = message.as_deref().unwrap_or_default();

    let body = apply_template(
        template,
        &[
            ("error", escape_html(error)),
            (
                "error_class",
                if error.is_empty() { "" } else { " visible" }.to_string(),
            ),
            ("current_user_id", escape_html(&current_user.id)),
            (
                "current_user_role",
                escape_html(match current_user.role {
                    UserRole::SuperAdmin => "superadmin",
                    UserRole::Admin => "admin",
                    UserRole::User => "user",
                }),
            ),
            ("user_rows", user_rows),
            ("modals", modals),
        ],
    );
    render_admin_response(
        state,
        StatusCode::OK,
        "Users",
        &body,
        PageLayout::standard(),
    )
}

fn users_update_response(state: &AppState, current_user: &AuthUser) -> Response {
    match render_user_rows(state, current_user) {
        Ok(html) => Json(serde_json::json!({ "status": "ok", "html": html })).into_response(),
        Err(error) => json_error_response(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

fn render_user_rows(state: &AppState, current_user: &AuthUser) -> Result<String, String> {
    let users = state.auth.list_users().map_err(|error| error.to_string())?;
    let activity = state
        .auth
        .user_activity()
        .map_err(|error| error.to_string())?;
    let row_template = load_template(state, "templates/partials/user_row.html")?;
    Ok(render_user_rows_html(
        &row_template,
        &users,
        &activity,
        current_user,
    ))
}

fn render_last_login(login: Option<&UserLogin>) -> String {
    let Some(login) = login else {
        return "Not recorded".to_string();
    };
    let timestamp = i64::try_from(login.logged_in_at)
        .ok()
        .and_then(|seconds| time::OffsetDateTime::from_unix_timestamp(seconds).ok());
    let Some(timestamp) = timestamp else {
        return format!("Unknown time — {}", escape_html(&login.device));
    };
    let datetime = timestamp
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    format!(
        "<time datetime=\"{}\">{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC</time> — {}",
        escape_html(&datetime),
        timestamp.year(),
        u8::from(timestamp.month()),
        timestamp.day(),
        timestamp.hour(),
        timestamp.minute(),
        timestamp.second(),
        escape_html(&login.device),
    )
}

fn render_user_rows_html(
    row_template: &str,
    users: &[AuthUser],
    activity: &HashMap<String, UserActivity>,
    current_user: &AuthUser,
) -> String {
    let mut user_rows = String::new();
    for user in users {
        let username = escape_html(&user.username);
        let role = match user.role {
            UserRole::SuperAdmin => "superadmin",
            UserRole::Admin => "admin",
            UserRole::User => "user",
        };
        let role_label = escape_html(role);
        let is_superadmin = matches!(user.role, UserRole::SuperAdmin);
        let can_edit = !is_superadmin || user.id == current_user.id;
        let can_delete = !is_superadmin;
        let checkbox = if can_delete {
            format!(
                r#"<input class="row-select" type="checkbox" aria-label="Select {}" />"#,
                username
            )
        } else {
            format!(
                r#"<input class="row-select" type="checkbox" aria-label="Select {} (protected)" disabled />"#,
                username
            )
        };
        let edit_icon = r#"<svg viewBox="0 0 24 24" aria-hidden="true" focusable="false"><path d="M3 17.25V21h3.75L17.81 9.94l-3.75-3.75L3 17.25zm2.92 2.83H5v-.92l9.06-9.06.92.92L5.92 20.08zM20.71 7.04a1.003 1.003 0 0 0 0-1.42l-2.34-2.34a1.003 1.003 0 0 0-1.42 0l-1.83 1.83 3.75 3.75 1.84-1.82z"/></svg>"#;
        let delete_icon = r#"<svg viewBox="0 0 24 24" aria-hidden="true" focusable="false"><path d="M9 3h6l1 2h4v2h-2v12a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2V7H4V5h4l1-2zm1 6h2v9h-2V9zm4 0h2v9h-2V9z"/></svg>"#;
        let edit_button = if can_edit {
            format!(
                r#"<button type="button" class="icon-button" data-action="edit" title="Edit user" aria-label="Edit user">{}</button>"#,
                edit_icon
            )
        } else {
            format!(
                r#"<button type="button" class="icon-button" data-action="edit" title="Locked" aria-label="Edit user (locked)" disabled>{}</button>"#,
                edit_icon
            )
        };
        let sessions_button = if can_edit {
            r#"<button type="button" class="icon-button" data-action="revoke-sessions" title="Sign out all app sessions" aria-label="Sign out all app sessions"><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M3 3h10v2H5v14h8v2H3V3zm13 4 5 5-5 5-1.4-1.4 2.6-2.6H9v-2h8.2l-2.6-2.6L16 7z"/></svg></button>"#.to_string()
        } else {
            String::new()
        };
        let delete_button = if can_delete {
            format!(
                r#"<button type="button" class="icon-button danger" data-action="delete" title="Delete user" aria-label="Delete user">{}</button>"#,
                delete_icon
            )
        } else {
            format!(
                r#"<button type="button" class="icon-button danger" data-action="delete" title="Protected" aria-label="Delete user (protected)" disabled>{}</button>"#,
                delete_icon
            )
        };

        user_rows.push_str(&apply_template(
            row_template.to_string(),
            &[
                ("id", escape_html(&user.id)),
                ("username_raw", escape_html(&user.username)),
                ("role", escape_html(role)),
                (
                    "disabled",
                    if user.disabled {
                        "1".to_string()
                    } else {
                        "0".to_string()
                    },
                ),
                (
                    "protected",
                    if is_superadmin {
                        "1".to_string()
                    } else {
                        "0".to_string()
                    },
                ),
                ("checkbox", checkbox),
                ("username", username),
                ("role_label", role_label),
                (
                    "active_sessions",
                    activity
                        .get(&user.id)
                        .map_or(0, |entry| entry.active_sessions)
                        .to_string(),
                ),
                (
                    "last_login",
                    render_last_login(
                        activity
                            .get(&user.id)
                            .and_then(|entry| entry.last_login.as_ref()),
                    ),
                ),
                ("edit_button", edit_button),
                ("delete_button", delete_button),
                ("sessions_button", sessions_button),
            ],
        ));
    }

    if user_rows.is_empty() {
        user_rows.push_str("<tr><td colspan=\"6\">No users found.</td></tr>");
    }

    user_rows
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROW_TEMPLATE: &str = include_str!("../../../../web/templates/partials/user_row.html");

    fn user(id: &str, username: &str, role: UserRole) -> AuthUser {
        AuthUser {
            id: id.to_string(),
            username: username.to_string(),
            role,
            disabled: false,
        }
    }

    #[test]
    fn refreshed_rows_preserve_protection_and_escape_account_fields() {
        let actor = user("actor", "Current admin", UserRole::Admin);
        let protected = user("super", "Superadmin", UserRole::SuperAdmin);
        let mut ordinary = user("user<&", "A \"B\" & <C>", UserRole::User);
        ordinary.disabled = true;
        let html = render_user_rows_html(
            ROW_TEMPLATE,
            &[protected, ordinary],
            &HashMap::new(),
            &actor,
        );
        assert_eq!(html.matches("class=\"user-row\"").count(), 2);
        assert!(html.contains("data-user-id=\"user&lt;&amp;\""));
        assert!(html.contains("data-username=\"A &quot;B&quot; &amp; &lt;C&gt;\""));
        assert!(html.contains("data-protected=\"1\""));
        assert!(html.contains("aria-label=\"Edit user (locked)\" disabled"));
        assert!(html.contains("aria-label=\"Delete user (protected)\" disabled"));
        assert!(html.contains("data-disabled=\"1\""));
        assert!(!html.contains("user-status"));
        assert!(!html.contains("private-password-hash"));
    }

    #[test]
    fn refreshed_superadmin_rows_allow_self_edit_and_empty_lists_have_a_table_row() {
        let actor = user("super", "Superadmin", UserRole::SuperAdmin);
        let html = render_user_rows_html(
            ROW_TEMPLATE,
            std::slice::from_ref(&actor),
            &HashMap::new(),
            &actor,
        );
        assert!(html.contains("aria-label=\"Edit user\""));
        assert!(!html.contains("Edit user (locked)"));
        assert!(html.contains("Delete user (protected)"));
        let empty = render_user_rows_html(ROW_TEMPLATE, &[], &HashMap::new(), &actor);
        assert!(empty.starts_with("<tr>"));
        assert!(empty.contains("No users found."));
        assert!(empty.contains("colspan=\"6\""));
    }

    #[test]
    fn activity_columns_show_plain_counts_and_escaped_login_time_and_device() {
        let actor = user("admin", "Admin", UserRole::Admin);
        let listener = user("listener", "Listener", UserRole::User);
        let activity = HashMap::from([(
            listener.id.clone(),
            UserActivity {
                active_sessions: 2,
                last_login: Some(UserLogin {
                    logged_in_at: 1_700_000_000,
                    device: "Phonolite on <Android> & \"tablet\"".to_string(),
                }),
            },
        )]);
        let html =
            render_user_rows_html(ROW_TEMPLATE, &[listener, actor.clone()], &activity, &actor);
        assert!(html.contains("<td class=\"user-sessions\">2</td>"));
        assert!(html.contains("<time datetime=\"2023-11-14T22:13:20Z\">2023-11-14 22:13:20 UTC</time> — Phonolite on &lt;Android&gt; &amp; &quot;tablet&quot;"));
        assert!(html.contains("<td class=\"user-sessions\">0</td>"));
        assert!(html.contains("<td class=\"user-last-login\">Not recorded</td>"));
        assert!(!html.contains("{{active_sessions}}"));
        assert!(!html.contains("{{last_login}}"));
    }
}
