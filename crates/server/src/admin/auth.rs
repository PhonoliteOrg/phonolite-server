// crates/server/src/admin/auth.rs
use axum::{
    extract::{Form, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
};

use crate::admin::presentation::{
    apply_template, escape_html, html_error, load_template, render_admin_response, PageLayout,
};
use crate::http::{json_error_response, redirect_to, wants_json};
use crate::state::{AppState, LoginForm, SetupForm};

use super::is_admin;
use crate::http::{clear_session_cookie, extract_session_cookie, session_cookie_header};

pub async fn admin_setup_form(State(state): State<AppState>) -> Response {
    if state.auth.has_admin().unwrap_or(false) {
        return redirect_to("/login");
    }
    admin_setup_page(&state, None)
}

pub async fn admin_setup_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<SetupForm>,
) -> Response {
    if let Err(response) = ensure_auth_mutation_running(&state) {
        return response;
    }
    if state.auth.has_admin().unwrap_or(false) {
        return redirect_to("/login");
    }
    if form.password != form.confirm {
        if wants_json(&headers) {
            return json_error_response(StatusCode::BAD_REQUEST, "passwords do not match");
        }
        return admin_setup_page(&state, Some("passwords do not match".to_string()));
    }

    let result = match run_auth_commit(&state, "admin_setup", move |store| {
        store.create_superadmin(&form.username, &form.password)
    })
    .await
    {
        Ok(result) => result,
        Err(response) => return response,
    };
    let user = match result {
        Ok(user) => user,
        Err(err) => {
            let message = format!("setup failed: {}", err);
            if wants_json(&headers) {
                let status = match err {
                    crate::auth::AuthError::InvalidUsername
                    | crate::auth::AuthError::InvalidPassword => StatusCode::BAD_REQUEST,
                    crate::auth::AuthError::UserExists => StatusCode::CONFLICT,
                    _ => StatusCode::INTERNAL_SERVER_ERROR,
                };
                return json_error_response(status, message);
            }
            return admin_setup_page(&state, Some(message));
        }
    };

    let user_id = user.id.clone();
    let result = match run_auth_commit(&state, "admin_session_create", move |store| {
        store.create_console_session(&user_id)
    })
    .await
    {
        Ok(result) => result,
        Err(response) => return response,
    };
    let session = match result {
        Ok(session) => session,
        Err(err) => {
            return html_error(
                &state,
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("auth error: {}", err),
            )
        }
    };

    let mut response = redirect_to("/");
    response.headers_mut().insert(
        header::SET_COOKIE,
        session_cookie_header(&session, state.auth.session_ttl()),
    );
    response
}

pub async fn admin_login_form(State(state): State<AppState>) -> Response {
    if !state.auth.has_admin().unwrap_or(false) {
        return redirect_to("/setup");
    }
    admin_login_page(&state, None)
}

pub async fn admin_login_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    if let Err(response) = ensure_auth_mutation_running(&state) {
        return response;
    }
    if !state.auth.has_admin().unwrap_or(false) {
        return redirect_to("/setup");
    }

    let result = match run_auth_commit(&state, "admin_authenticate", move |store| {
        store.authenticate(&form.username, &form.password)
    })
    .await
    {
        Ok(result) => result,
        Err(response) => return response,
    };
    let user = match result {
        Ok(Some(user)) if is_admin(&user) => user,
        Ok(Some(_)) => {
            if wants_json(&headers) {
                return json_error_response(StatusCode::FORBIDDEN, "admin access required");
            }
            return admin_login_page(&state, Some("admin access required".to_string()));
        }
        Ok(None) => {
            if wants_json(&headers) {
                return json_error_response(StatusCode::UNAUTHORIZED, "invalid credentials");
            }
            return admin_login_page(&state, Some("invalid credentials".to_string()));
        }
        Err(err) => {
            return html_error(
                &state,
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("auth error: {}", err),
            )
        }
    };

    let user_id = user.id.clone();
    let result = match run_auth_commit(&state, "admin_session_create", move |store| {
        store.create_console_session(&user_id)
    })
    .await
    {
        Ok(result) => result,
        Err(response) => return response,
    };
    let session = match result {
        Ok(session) => session,
        Err(err) => {
            return html_error(
                &state,
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("auth error: {}", err),
            )
        }
    };

    let mut response = redirect_to("/");
    response.headers_mut().insert(
        header::SET_COOKIE,
        session_cookie_header(&session, state.auth.session_ttl()),
    );
    response
}

pub async fn admin_logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = ensure_auth_mutation_running(&state) {
        return response;
    }
    if let Some(token) = extract_session_cookie(&headers) {
        if let Err(response) = run_auth_commit(&state, "admin_session_revoke", move |store| {
            store.revoke_session(&token)
        })
        .await
        {
            return response;
        }
    }
    let mut response = redirect_to("/login");
    response
        .headers_mut()
        .insert(header::SET_COOKIE, clear_session_cookie());
    response
}

pub(crate) fn admin_setup_page(state: &AppState, message: Option<String>) -> Response {
    let template = match load_template(state, "templates/setup.html") {
        Ok(template) => template,
        Err(err) => {
            return html_error(
                state,
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("template error: {}", err),
            )
        }
    };
    let error = message.as_deref().unwrap_or_default();
    let body = apply_template(
        template,
        &[
            ("error", escape_html(error)),
            (
                "error_class",
                if error.is_empty() { "" } else { " visible" }.to_string(),
            ),
        ],
    );
    render_admin_response(
        state,
        StatusCode::OK,
        "Setup",
        &body,
        PageLayout::centered(),
    )
}

pub(crate) fn admin_login_page(state: &AppState, message: Option<String>) -> Response {
    let template = match load_template(state, "templates/login.html") {
        Ok(template) => template,
        Err(err) => {
            return html_error(
                state,
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("template error: {}", err),
            )
        }
    };
    let error = message.as_deref().unwrap_or_default();
    let body = apply_template(
        template,
        &[
            ("error", escape_html(error)),
            (
                "error_class",
                if error.is_empty() { "" } else { " visible" }.to_string(),
            ),
        ],
    );
    render_admin_response(
        state,
        StatusCode::OK,
        "Login",
        &body,
        PageLayout::centered(),
    )
}

pub(super) fn ensure_auth_mutation_running(state: &AppState) -> Result<(), Response> {
    if state.shutdown.is_cancelled() {
        Err(json_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "server shutting down",
        ))
    } else {
        Ok(())
    }
}
pub(super) async fn run_auth_commit<R, F>(
    state: &AppState,
    name: &str,
    work: F,
) -> Result<Result<R, crate::auth::AuthError>, Response>
where
    R: Send + 'static,
    F: FnOnce(crate::auth::AuthStore) -> Result<R, crate::auth::AuthError> + Send + 'static,
{
    ensure_auth_mutation_running(state)?;
    let store = state.auth.clone();
    state
        .shutdown
        .spawn_blocking_commit(name, move || work(store))
        .await
        .map_err(|err| {
            json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("authentication task failed: {err}"),
            )
        })
}
