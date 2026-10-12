//! Shared HTTP response and session-header behavior used by both transports.
use axum::body::Body;
use axum::http::{header, uri::Authority, HeaderMap, HeaderValue, Request, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use std::time::Duration;

use crate::auth::SessionToken;
use crate::state::{ErrorResponse, HealthResponse};

pub(crate) fn request_authority<B>(
    request: &Request<B>,
    default_port: u16,
) -> Option<(String, u16)> {
    let mut hosts = request.headers().get_all(header::HOST).iter();
    let raw = hosts.next()?.to_str().ok()?;
    if hosts.next().is_some() || raw.contains('@') {
        return None;
    }
    let authority: Authority = raw.parse().ok()?;
    let host = authority.host().to_ascii_lowercase();
    let valid_host = if let Some(ipv6) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        ipv6.parse::<std::net::Ipv6Addr>().is_ok()
    } else {
        !host.is_empty()
            && host.len() <= 253
            && host
                .strip_suffix('.')
                .unwrap_or(&host)
                .split('.')
                .all(|label| {
                    !label.is_empty()
                        && label.len() <= 63
                        && label
                            .as_bytes()
                            .first()
                            .is_some_and(u8::is_ascii_alphanumeric)
                        && label
                            .as_bytes()
                            .last()
                            .is_some_and(u8::is_ascii_alphanumeric)
                        && label
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
    };
    if !valid_host {
        return None;
    }
    let suffix = authority.as_str().strip_prefix(authority.host())?;
    let port = if suffix.is_empty() {
        default_port
    } else {
        let digits = suffix.strip_prefix(':')?;
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        digits.parse::<u16>().ok()?
    };
    (port != 0).then_some((host, port))
}

pub fn json_error(
    status: StatusCode,
    message: impl Into<String>,
) -> (StatusCode, Json<ErrorResponse>) {
    (
        status,
        Json(ErrorResponse {
            error: message.into(),
        }),
    )
}

pub fn json_error_response(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorResponse {
            error: message.into(),
        }),
    )
        .into_response()
}

pub fn json_ok_response() -> Response {
    Json(HealthResponse { status: "ok" }).into_response()
}

pub fn wants_json(headers: &HeaderMap) -> bool {
    if let Some(value) = headers.get(header::ACCEPT) {
        if let Ok(value) = value.to_str() {
            if value.contains("application/json") {
                return true;
            }
        }
    }
    if let Some(value) = headers.get("X-Requested-With") {
        if let Ok(value) = value.to_str() {
            if value.eq_ignore_ascii_case("fetch") {
                return true;
            }
        }
    }
    false
}

pub fn redirect_to(path: &str) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::SEE_OTHER;
    let location = HeaderValue::from_str(path).unwrap_or_else(|_| HeaderValue::from_static("/"));
    response.headers_mut().insert(header::LOCATION, location);
    response
}

pub fn html_response(status: StatusCode, body: String) -> Response {
    let mut response = Html(body).into_response();
    *response.status_mut() = status;
    response
}

pub fn extract_token(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get(header::AUTHORIZATION) {
        if let Ok(value) = value.to_str() {
            if let Some(token) = value.strip_prefix("Bearer ") {
                return Some(token.trim().to_string());
            }
        }
    }
    extract_session_cookie(headers)
}

pub fn extract_session_cookie(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    parse_cookie_value(cookie, "phonolite_session")
}

fn parse_cookie_value(cookie: &str, name: &str) -> Option<String> {
    for part in cookie.split(';') {
        let part = part.trim();
        let mut iter = part.splitn(2, '=');
        let key = iter.next()?.trim();
        let value = iter.next()?.trim();
        if key == name && !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

pub fn session_cookie_header(session: &SessionToken, ttl: Duration) -> HeaderValue {
    let value = format!(
        "phonolite_session={}; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age={}",
        session.token,
        ttl.as_secs()
    );
    HeaderValue::from_str(&value).unwrap_or_else(|_| {
        HeaderValue::from_static(
            "phonolite_session=invalid; Path=/; Secure; HttpOnly; SameSite=Strict",
        )
    })
}

pub fn clear_session_cookie() -> HeaderValue {
    HeaderValue::from_static(
        "phonolite_session=; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=0",
    )
}

#[cfg(test)]
mod session_tests {
    use super::*;

    #[test]
    fn bearer_precedes_cookie_and_cookie_only_auth_stays_separate() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("other=x; phonolite_session=cookie=token"),
        );
        assert_eq!(extract_token(&headers).as_deref(), Some("cookie=token"));
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer   bearer-token  "),
        );
        assert_eq!(extract_token(&headers).as_deref(), Some("bearer-token"));
        assert_eq!(
            extract_session_cookie(&headers).as_deref(),
            Some("cookie=token")
        );
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("bearer lower-case"),
        );
        assert_eq!(extract_token(&headers).as_deref(), Some("cookie=token"));
    }

    #[test]
    fn cookie_parsing_retains_empty_and_malformed_segment_behavior() {
        assert_eq!(
            parse_cookie_value("phonolite_session=", "phonolite_session"),
            None
        );
        assert_eq!(
            parse_cookie_value("malformed; phonolite_session=value", "phonolite_session"),
            None
        );
        assert_eq!(
            parse_cookie_value(
                "phonolite_session=first; phonolite_session=second",
                "phonolite_session"
            )
            .as_deref(),
            Some("first")
        );
        assert_eq!(
            clear_session_cookie().to_str().unwrap(),
            "phonolite_session=; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=0"
        );
    }
}

#[cfg(test)]
mod response_tests {
    #[test]
    fn json_negotiation_preserves_accept_and_fetch_rules() {
        use axum::http::{header, HeaderMap, HeaderValue};
        let mut headers = HeaderMap::new();
        assert!(!super::wants_json(&headers));
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("text/html, application/json"),
        );
        assert!(super::wants_json(&headers));
        headers.insert(header::ACCEPT, HeaderValue::from_static("Application/JSON"));
        assert!(!super::wants_json(&headers));
        headers.insert("X-Requested-With", HeaderValue::from_static("FeTcH"));
        assert!(super::wants_json(&headers));
    }

    #[test]
    fn redirects_preserve_status_and_reject_invalid_locations() {
        use axum::http::{header, StatusCode};
        let response = super::redirect_to("/library?search=a%20b");
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers()[header::LOCATION],
            "/library?search=a%20b"
        );
        assert_eq!(
            super::redirect_to("/bad\r\nvalue").headers()[header::LOCATION],
            "/"
        );
    }
}
