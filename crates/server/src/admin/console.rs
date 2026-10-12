//! Content negotiation for the persistent console shell.
use axum::{
    extract::Request,
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

use crate::http::wants_json;

#[derive(Clone, Serialize)]
pub(crate) struct ConsoleView {
    pub title: String,
    pub page: String,
    pub page_class: String,
    pub content: String,
    pub nav: String,
    pub footer: String,
}

#[derive(Clone)]
pub(crate) struct ConsoleError(pub String);

pub(crate) fn wants_console_view(headers: &HeaderMap) -> bool {
    headers
        .get("X-Console-View")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("true"))
}

pub(crate) async fn console_response(request: Request, next: Next) -> Response {
    let view = wants_console_view(request.headers());
    let mutation_json =
        !matches!(*request.method(), Method::GET | Method::HEAD) && wants_json(request.headers());
    let asset = request.uri().path().starts_with("/assets/")
        || request.uri().path().starts_with("/covers/");
    let response = next.run(request).await;
    if asset {
        return response;
    }
    negotiate_response(response, view, mutation_json)
}

fn negotiate_response(mut response: Response, view: bool, mutation_json: bool) -> Response {
    if mutation_json {
        if let Some(error) = response.extensions_mut().remove::<ConsoleError>() {
            response = json_response(response, serde_json::json!({ "error": error.0 }));
        }
    }
    if view || mutation_json {
        if response.status().is_redirection() {
            if let Some(location) = response.headers().get(header::LOCATION).cloned() {
                if let Ok(location) = location.to_str() {
                    let redirect = location.to_string();
                    response.headers_mut().remove(header::LOCATION);
                    *response.status_mut() = StatusCode::OK;
                    response = json_response(response, serde_json::json!({ "redirect": redirect }));
                }
            }
        } else if view && !mutation_json {
            if let Some(page) = response.extensions_mut().remove::<ConsoleView>() {
                // The page metadata is a typed struct of strings. Keep its
                // fields available while exposing a useful failed-load message.
                let mut value = serde_json::to_value(page)
                    .expect("console views contain only serializable strings");
                if !response.status().is_success() {
                    if let Some(error) = response.extensions_mut().remove::<ConsoleError>() {
                        value["error"] = serde_json::Value::String(error.0);
                    }
                }
                response = json_response(response, value);
            }
        }
    }
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response.headers_mut().append(
        header::VARY,
        HeaderValue::from_static("X-Console-View, Accept, X-Requested-With"),
    );
    response
}

fn json_response(response: Response, value: impl Serialize) -> Response {
    let (mut parts, _) = response.into_parts();
    let (json_parts, body) = Json(value).into_response().into_parts();
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(
        header::CONTENT_TYPE,
        json_parts.headers[header::CONTENT_TYPE].clone(),
    );
    Response::from_parts(parts, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{html_response, redirect_to};

    async fn json_body(response: Response) -> serde_json::Value {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn view_responses_preserve_status_and_private_headers() {
        let mut response = html_response(StatusCode::FORBIDDEN, "document".to_string());
        response.extensions_mut().insert(ConsoleView {
            title: "Error".to_string(),
            page: "error".to_string(),
            page_class: "centered".to_string(),
            content: "<p>Forbidden</p>".to_string(),
            nav: String::new(),
            footer: "<footer>Phonolite</footer>".to_string(),
        });
        response
            .extensions_mut()
            .insert(ConsoleError("Forbidden".to_string()));
        let response = negotiate_response(response, true, false);
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, no-store"
        );
        let view = json_body(response).await;
        assert_eq!(view["page"], "error");
        assert_eq!(view["content"], "<p>Forbidden</p>");
        assert_eq!(view["nav"], "");
        assert_eq!(view["error"], "Forbidden");
    }

    #[tokio::test]
    async fn spa_redirects_keep_session_cookies_and_documents_keep_redirects() {
        let mut response = redirect_to("/login");
        response.headers_mut().insert(
            header::SET_COOKIE,
            HeaderValue::from_static("phonolite_session=; Path=/; HttpOnly; Max-Age=0"),
        );
        let response = negotiate_response(response, true, true);
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key(header::SET_COOKIE));
        assert!(!response.headers().contains_key(header::LOCATION));
        assert_eq!(json_body(response).await["redirect"], "/login");

        let response = negotiate_response(redirect_to("/setup"), false, false);
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], "/setup");
    }

    #[tokio::test]
    async fn existing_fragment_json_is_not_wrapped_as_a_page() {
        let response =
            Json(serde_json::json!({ "content": "tiles", "pagination": "next" })).into_response();
        let result = json_body(negotiate_response(response, true, false)).await;
        assert_eq!(result["pagination"], "next");
        assert!(result.get("page").is_none());
    }

    #[tokio::test]
    async fn mutation_errors_use_json_without_private_page_markup() {
        let mut response = html_response(StatusCode::UNAUTHORIZED, "private markup".to_string());
        response
            .extensions_mut()
            .insert(ConsoleError("admin access required".to_string()));
        response.extensions_mut().insert(ConsoleView {
            title: "Error".to_string(),
            page: "error".to_string(),
            page_class: "centered".to_string(),
            content: "error page markup".to_string(),
            nav: String::new(),
            footer: String::new(),
        });
        let response = negotiate_response(response, true, true);
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            json_body(response).await,
            serde_json::json!({ "error": "admin access required" })
        );
    }

    #[test]
    fn explicit_console_header_selects_page_views() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
        assert!(!wants_console_view(&headers));
        headers.insert("X-Fragment", HeaderValue::from_static("true"));
        assert!(!wants_console_view(&headers));
        headers.insert("X-Console-View", HeaderValue::from_static("TRUE"));
        assert!(wants_console_view(&headers));
        headers.insert("X-Console-View", HeaderValue::from_static("false"));
        assert!(!wants_console_view(&headers));
    }
}
