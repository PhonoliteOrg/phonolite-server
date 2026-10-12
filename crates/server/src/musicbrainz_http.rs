//! Shared MusicBrainz request policy; provider payloads and matching stay with callers.

use std::time::Duration;

use reqwest::{Client, StatusCode, Url};
use serde::de::DeserializeOwned;

use crate::musicbrainz_rate_limit;

const MAX_ATTEMPTS: usize = 6;

pub(crate) async fn get_json<T: DeserializeOwned>(
    client: &Client,
    url: &str,
    user_agent: &str,
    timeout: Duration,
) -> Result<T, String> {
    let mut last_error = String::from("request failed");
    let endpoint = endpoint_path(url);

    for attempt in 0..MAX_ATTEMPTS {
        musicbrainz_rate_limit::wait_for_slot().await;
        let response = client
            .get(url)
            .timeout(timeout)
            .header("User-Agent", user_agent)
            .send()
            .await;

        let failure = match response {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    match response.bytes().await {
                        Ok(bytes) => match serde_json::from_slice::<T>(&bytes) {
                            Ok(payload) => return Ok(payload),
                            Err(err) => RequestFailure {
                                stage: "decode",
                                status: Some(status),
                                diagnostic_reason: decode_diagnostic_reason(&err),
                                error: format_musicbrainz_decode_error(err, &bytes),
                                can_retry: true,
                            },
                        },
                        Err(err) => {
                            let error = err.without_url().to_string();
                            RequestFailure {
                                stage: "body",
                                status: Some(status),
                                diagnostic_reason: error.clone(),
                                error,
                                can_retry: true,
                            }
                        }
                    }
                } else {
                    let error = format!("http {status}");
                    RequestFailure {
                        stage: "http",
                        status: Some(status),
                        diagnostic_reason: error.clone(),
                        error,
                        can_retry: retryable_status(status),
                    }
                }
            }
            Err(err) => {
                let error = err.without_url().to_string();
                RequestFailure {
                    stage: "send",
                    status: None,
                    diagnostic_reason: error.clone(),
                    error,
                    can_retry: true,
                }
            }
        };

        let diagnostic = retry_diagnostic(attempt, failure.can_retry);
        tracing::warn!(
            endpoint = %endpoint,
            stage = failure.stage,
            attempt = attempt + 1,
            max_attempts = MAX_ATTEMPTS,
            http_status = ?failure.status.map(|status| status.as_u16()),
            reason = %failure.diagnostic_reason,
            retrying = diagnostic.delay.is_some(),
            retry_delay_ms = diagnostic.delay.map(|delay| delay.as_millis()).unwrap_or(0) as u64,
            attempts_exhausted = diagnostic.exhausted,
            "MusicBrainz request attempt failed"
        );
        last_error = format!(
            "MusicBrainz {} failed for {} (attempt {}/{}): {}",
            failure.stage,
            endpoint,
            attempt + 1,
            MAX_ATTEMPTS,
            failure.error
        );
        if diagnostic.delay.is_none() {
            return Err(last_error);
        }

        tokio::time::sleep(retry_delay(attempt)).await;
    }

    Err(last_error)
}

struct RequestFailure {
    stage: &'static str,
    status: Option<StatusCode>,
    diagnostic_reason: String,
    error: String,
    can_retry: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct RetryDiagnostic {
    delay: Option<Duration>,
    exhausted: bool,
}

fn retry_diagnostic(attempt: usize, can_retry: bool) -> RetryDiagnostic {
    let exhausted = can_retry && attempt + 1 >= MAX_ATTEMPTS;
    RetryDiagnostic {
        delay: if can_retry && !exhausted {
            Some(retry_delay(attempt))
        } else {
            None
        },
        exhausted,
    }
}

fn endpoint_path(url: &str) -> String {
    Url::parse(url)
        .map(|url| url.path().to_string())
        .unwrap_or_else(|_| "<invalid URL>".into())
}

fn decode_diagnostic_reason(error: &serde_json::Error) -> String {
    // A deserializer error can contain response values. Log only its category
    // and location; the returned error retains the existing body excerpt.
    format!(
        "JSON {:?} error at line {} column {}",
        error.classify(),
        error.line(),
        error.column()
    )
}

fn format_musicbrainz_decode_error(err: serde_json::Error, body: &[u8]) -> String {
    let snippet = String::from_utf8_lossy(body);
    let snippet = snippet.trim();
    if snippet.is_empty() {
        return format!("error decoding response body: {}", err);
    }
    let snippet = if snippet.len() > 240 {
        // Provider error pages can contain multibyte text. Keep the existing
        // byte limit without slicing through a UTF-8 character.
        let mut end = 240;
        while !snippet.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &snippet[..end])
    } else {
        snippet.to_string()
    };
    format!("error decoding response body: {} | body={}", err, snippet)
}

fn retryable_status(status: StatusCode) -> bool {
    status.as_u16() == 429 || status.is_server_error()
}

fn retry_delay(attempt: usize) -> Duration {
    Duration::from_millis(1000 + (attempt as u64 * 500))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn musicbrainz_decode_error_trims_and_preserves_short_response_body() {
        let error = serde_json::from_str::<Value>("{").unwrap_err();
        let expected = format!("error decoding response body: {} | body=unavailable", error);
        assert_eq!(
            format_musicbrainz_decode_error(error, b" \n unavailable \t"),
            expected
        );
    }

    #[test]
    fn musicbrainz_decode_error_omits_empty_response_body() {
        let error = serde_json::from_str::<Value>("{").unwrap_err();
        let expected = format!("error decoding response body: {}", error);
        assert_eq!(format_musicbrainz_decode_error(error, b" \n\t"), expected);
    }

    #[test]
    fn musicbrainz_decode_error_limits_ascii_excerpt_to_240_bytes() {
        let error = serde_json::from_str::<Value>("{").unwrap_err();
        let body = "a".repeat(241);
        let expected = format!(
            "error decoding response body: {} | body={}...",
            error,
            "a".repeat(240)
        );
        assert_eq!(
            format_musicbrainz_decode_error(error, body.as_bytes()),
            expected
        );
    }

    #[test]
    fn musicbrainz_decode_error_keeps_multibyte_characters_intact_at_excerpt_boundary() {
        let error = serde_json::from_str::<Value>("{").unwrap_err();
        let body = format!("{}é provider error", "a".repeat(239));
        let expected = format!(
            "error decoding response body: {} | body={}...",
            error,
            "a".repeat(239)
        );
        assert_eq!(
            format_musicbrainz_decode_error(error, body.as_bytes()),
            expected
        );
    }

    #[test]
    fn musicbrainz_decode_error_handles_invalid_utf8_without_panicking() {
        let error = serde_json::from_str::<Value>("{").unwrap_err();
        let expected = format!(
            "error decoding response body: {} | body=bad � response",
            error
        );
        assert_eq!(
            format_musicbrainz_decode_error(error, b"bad \xff response"),
            expected
        );
    }

    #[test]
    fn retry_policy_retries_only_rate_limit_and_server_statuses() {
        for code in [429, 500, 502, 503, 504, 599] {
            assert!(
                retryable_status(StatusCode::from_u16(code).unwrap()),
                "{code}"
            );
        }
        for code in [200, 301, 400, 401, 403, 404, 408, 422] {
            assert!(
                !retryable_status(StatusCode::from_u16(code).unwrap()),
                "{code}"
            );
        }
    }

    #[test]
    fn retry_policy_keeps_six_attempts_and_existing_linear_backoff() {
        assert_eq!(MAX_ATTEMPTS, 6);
        let delays: Vec<u128> = (0..MAX_ATTEMPTS - 1)
            .map(|attempt| retry_delay(attempt).as_millis())
            .collect();
        assert_eq!(delays, vec![1000, 1500, 2000, 2500, 3000]);
    }

    #[test]
    fn retry_diagnostics_distinguish_retry_exhaustion_and_terminal_http_status() {
        for attempt in 0..MAX_ATTEMPTS - 1 {
            assert_eq!(
                retry_diagnostic(attempt, true),
                RetryDiagnostic {
                    delay: Some(retry_delay(attempt)),
                    exhausted: false,
                }
            );
        }
        assert_eq!(
            retry_diagnostic(MAX_ATTEMPTS - 1, true),
            RetryDiagnostic {
                delay: None,
                exhausted: true,
            }
        );
        for attempt in [0, MAX_ATTEMPTS - 1] {
            assert_eq!(
                retry_diagnostic(attempt, false),
                RetryDiagnostic {
                    delay: None,
                    exhausted: false,
                }
            );
        }
    }

    #[test]
    fn endpoint_diagnostics_exclude_credentials_query_and_fragment() {
        assert_eq!(
            endpoint_path(
                "https://private-user:private-password@musicbrainz.org/ws/2/release/?query=private-album&fmt=json#private-fragment"
            ),
            "/ws/2/release/"
        );
        assert_eq!(endpoint_path("invalid private URL"), "<invalid URL>");
    }

    #[test]
    fn decode_diagnostics_exclude_response_values_while_returned_errors_retain_excerpt() {
        let body = br#""private response value""#;
        let error = serde_json::from_slice::<u64>(body).unwrap_err();
        assert!(error.to_string().contains("private response value"));
        let diagnostic = decode_diagnostic_reason(&error);
        assert!(diagnostic.starts_with("JSON Data error at line 1 column "));
        assert!(!diagnostic.contains("private response value"));
        assert!(format_musicbrainz_decode_error(error, body)
            .contains("body=\"private response value\""));
    }
}
