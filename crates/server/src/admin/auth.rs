// crates/server/src/admin/auth.rs
use axum::{
    extract::{Form, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
};

use crate::state::{AppState, LoginForm, SetupForm};
use crate::utils::{
    apply_template, html_error, html_response, load_template, redirect_to, render_admin_page,
    PageLayout,
};

use super::{
    clear_session_cookie, extract_session_cookie, is_admin, render_message, session_cookie_header,
};

pub async fn admin_setup_form(State(state): State<AppState>) -> Response {
    if state.auth.has_admin().unwrap_or(false) {
        return redirect_to("/login");
    }
    admin_setup_page(&state, None)
}

pub async fn admin_setup_submit(
    State(state): State<AppState>,
    Form(form): Form<SetupForm>,
) -> Response {
    if state.auth.has_admin().unwrap_or(false) {
        return redirect_to("/login");
    }
    if form.password != form.confirm {
        return admin_setup_page(&state, Some("passwords do not match".to_string()));
    }

    let user = match state.auth.create_superadmin(&form.username, &form.password) {
        Ok(user) => user,
        Err(err) => return admin_setup_page(&state, Some(format!("setup failed: {}", err))),
    };

    let session = match state.auth.create_session(&user.id) {
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
    Form(form): Form<LoginForm>,
) -> Response {
    if !state.auth.has_admin().unwrap_or(false) {
        return redirect_to("/setup");
    }

    let user = match state.auth.authenticate(&form.username, &form.password) {
        Ok(Some(user)) if is_admin(&user) => user,
        Ok(Some(_)) => {
            return admin_login_page(&state, Some("admin access required".to_string()))
        }
        Ok(None) => return admin_login_page(&state, Some("invalid credentials".to_string())),
        Err(err) => {
            return html_error(
                &state,
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("auth error: {}", err),
            )
        }
    };

    let session = match state.auth.create_session(&user.id) {
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
    if let Some(token) = extract_session_cookie(&headers) {
        let _ = state.auth.revoke_session(&token);
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
    let message_html = render_message(message);
    let body = apply_template(template, &[("message", message_html)]);
    html_response(
        StatusCode::OK,
        render_admin_page(state, "Setup", &body, PageLayout::centered()),
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
    let message_html = render_message(message);
    let body = apply_template(template, &[("message", message_html)]);
    html_response(
        StatusCode::OK,
        render_admin_page(state, "Login", &body, PageLayout::standard()),
    )
}
