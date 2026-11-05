//! Exercise the production auth store and connection lifetimes without starting
//! listeners or linking the audio codec. Imported modules include their own
//! regressions for revocation, expiry, restart, deduplication, and timeouts.
#![allow(dead_code)]

#[path = "../src/auth.rs"]
mod auth;
#[path = "../src/http.rs"]
mod http;
#[path = "../src/presence.rs"]
mod presence;
#[path = "../src/security_protocol.rs"]
mod security_protocol;

// The shared HTTP helpers need only these serialized response types. Session
// creation, token parsing, and presence all use the production implementations.
mod state {
    #[derive(serde::Serialize)]
    pub struct ErrorResponse {
        pub error: String,
    }

    #[derive(serde::Serialize)]
    pub struct HealthResponse {
        pub status: &'static str,
    }
}
