// crates/server/src/admin/users.rs
use axum::{
    extract::{Form, Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};

use crate::auth::{self, AuthUser, UserRole};
use crate::state::{AppState, BulkDeleteForm, NewUserForm, PasswordForm, UpdateUserForm};
use crate::utils::{
    apply_template, escape_html, html_error, html_response, json_error_response, json_ok_response,
    load_template, redirect_to, render_admin_page, wants_json, PageLayout,
};

use super::{admin_user_from_headers, is_admin, render_message};

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

pub async fn admin_create_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<NewUserForm>,
) -> Response {
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

    match state.auth.create_user(&form.username, &form.password, role) {
        Ok(_) => {
            if wants_json(&headers) {
                json_ok_response()
            } else {
                redirect_to("/")
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

    match state.auth.update_password(&user_id, &form.password) {
        Ok(_) => {
            if wants_json(&headers) {
                json_ok_response()
            } else {
                redirect_to("/")
            }
        }
        Err(err) => {
            if wants_json(&headers) {
                json_error_response(
                    StatusCode::BAD_REQUEST,
                    format!("password update failed: {}", err),
                )
            } else {
                admin_users_page(&state, &user, Some(format!("password update failed: {}", err)))
            }
        }
    }
}

pub async fn admin_delete_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(user_id): AxumPath<String>,
) -> Response {
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

    match state.auth.delete_user(&user_id) {
        Ok(_) => {
            if wants_json(&headers) {
                json_ok_response()
            } else {
                redirect_to("/")
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

pub async fn admin_update_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(user_id): AxumPath<String>,
    Form(form): Form<UpdateUserForm>,
) -> Response {
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
        Some(password)
    };

    match state
        .auth
        .update_user(&user_id, &form.username, password, role)
    {
        Ok(_) => {
            if wants_json(&headers) {
                json_ok_response()
            } else {
                redirect_to("/")
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

    for user_id in ids {
        if let Err(err) = state.auth.delete_user(&user_id) {
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
    }

    if wants_json(&headers) {
        json_ok_response()
    } else {
        redirect_to("/")
    }
}

pub(crate) fn admin_users_page(
    state: &AppState,
    current_user: &AuthUser,
    message: Option<String>,
) -> Response {
    let users = match state.auth.list_users() {
        Ok(users) => users,
        Err(err) => return html_error(state, StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
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
    let row_template = load_template(state, "templates/partials/user_row.html").unwrap_or_default();
    let modals = [
        load_template(state, "templates/modals/user_add.html").unwrap_or_default(),
        load_template(state, "templates/modals/user_edit.html").unwrap_or_default(),
        load_template(state, "templates/modals/user_delete.html").unwrap_or_default(),
    ]
    .join("");

    let message_html = render_message(message);

    let mut user_rows = String::new();
    for user in users {
        let username = escape_html(&user.username);
        let role = match user.role {
            UserRole::SuperAdmin => "superadmin",
            UserRole::Admin => "admin",
            UserRole::User => "user",
        };
        let role_label = escape_html(role);
        let status = if user.disabled { "disabled" } else { "active" };
        let is_superadmin = matches!(user.role, UserRole::SuperAdmin);
        let can_edit = !is_superadmin || user.id == current_user.id;
        let can_delete = !is_superadmin;
        let checkbox = if can_delete {
            r#"<input class="row-select" type="checkbox" />"#.to_string()
        } else {
            r#"<input class="row-select" type="checkbox" disabled />"#.to_string()
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

        user_rows.push_str(&apply_template(row_template.clone(), &[
            ("id", escape_html(&user.id)),
            ("username_raw", escape_html(&user.username)),
            ("role", escape_html(role)),
            ("disabled", if user.disabled { "1".to_string() } else { "0".to_string() }),
            ("protected", if is_superadmin { "1".to_string() } else { "0".to_string() }),
            ("checkbox", checkbox),
            ("username", username),
            ("role_label", role_label),
            ("status", status.to_string()),
            ("edit_button", edit_button),
            ("delete_button", delete_button),
        ]));
    }

    if user_rows.is_empty() {
        user_rows.push_str("<tr><td colspan=\"5\">No users found.</td></tr>");
    }

    let body = apply_template(
        template,
        &[
            ("message", message_html),
            ("current_user_id", escape_html(&current_user.id)),
            ("current_user_role", escape_html(match current_user.role {
                UserRole::SuperAdmin => "superadmin",
                UserRole::Admin => "admin",
                UserRole::User => "user",
            })),
            ("user_rows", user_rows),
            ("modals", modals),
        ],
    );
    html_response(
        StatusCode::OK,
        render_admin_page(state, "Users", &body, PageLayout::standard()),
    )
}
