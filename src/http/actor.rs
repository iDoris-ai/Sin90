//! The "AI does not write directly" gate (design §7.1), enforced by Sin90
//! itself — not by Agent24. Two distinct keys: a **human** key that may hit
//! any direct-write route, and an **automation** key that may only reach the
//! Proposal routes (`POST /proposals`, `POST /proposals/{id}/accept`).
//!
//! This is a real, checked distinction — not the self-reported
//! `Sin90Proposal.source` field the old (kernel) implementation relied on
//! alone. It does not attempt cryptographic strength (no rotation, no scopes
//! beyond the two); design §7.1 explicitly scoped M0 to "at least a key that
//! distinguishes caller type", with anything stronger deferred until a real
//! misuse is observed.

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    Human,
    Automation,
}

#[derive(Debug, Clone)]
pub struct ActorKeys {
    pub human: String,
    pub automation: String,
}

impl ActorKeys {
    /// Load from `SIN90_HUMAN_KEY` / `SIN90_AUTOMATION_KEY`, generating and
    /// printing (once, to stderr) any that are unset — a fresh install has
    /// nowhere else to learn them from. Regenerating on every restart when
    /// unset would silently invalidate whatever the operator copied down, so
    /// an unset key always warns loudly rather than rotating quietly.
    pub fn from_env_or_generate() -> Self {
        let human = std::env::var("SIN90_HUMAN_KEY").unwrap_or_else(|_| {
            let k = random_key();
            eprintln!(
                "sin90: SIN90_HUMAN_KEY not set — generated for this run: {k}\n\
                 sin90: set SIN90_HUMAN_KEY to keep this stable across restarts."
            );
            k
        });
        let automation = std::env::var("SIN90_AUTOMATION_KEY").unwrap_or_else(|_| {
            let k = random_key();
            eprintln!(
                "sin90: SIN90_AUTOMATION_KEY not set — generated for this run: {k}\n\
                 sin90: set SIN90_AUTOMATION_KEY to keep this stable across restarts."
            );
            k
        });
        Self { human, automation }
    }

    /// Identify the actor behind the `x-sin90-actor-key` header. `None` means
    /// no recognized key was presented at all (missing header, or a key that
    /// matches neither).
    pub fn identify(&self, headers: &HeaderMap) -> Option<Actor> {
        let key = bearer_token(headers)?;
        if constant_time_eq(key.as_bytes(), self.human.as_bytes()) {
            Some(Actor::Human)
        } else if constant_time_eq(key.as_bytes(), self.automation.as_bytes()) {
            Some(Actor::Automation)
        } else {
            None
        }
    }
}

/// Bearer-token comparison that takes the same time regardless of where the
/// first mismatching byte falls, so a timing side channel can't be used to
/// recover `human`/`automation` one byte at a time. A length mismatch is not
/// itself timed — comparing against a fixed-length local secret leaks
/// nothing an attacker doesn't already get from trying keys of every length.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// `x-sin90-actor-key`, deliberately NOT the standard `Authorization` header:
/// `agent24-os-proto::proxy` strips `Authorization`/`Cookie` (and anything
/// `X-A24-*`) from every request before it reaches an out-of-process module —
/// a key read off `Authorization` would always be `None` for every request
/// that actually arrived through Agent24's real proxy, only ever working in
/// tests that call `router()` directly. Caught by
/// `tests/agent24_mount_blackbox.rs` against a real daemon.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("x-sin90-actor-key")
        .and_then(|v| v.to_str().ok())
}

fn random_key() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 24];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Standard rejection body for a missing/wrong/insufficiently-privileged key —
/// deliberately vague about WHICH is wrong (missing vs. wrong vs. wrong kind),
/// same reasoning `axum`'s own auth examples use: don't tell an attacker which
/// half of "bearer token" they got right.
pub fn forbidden(why: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        axum::Json(serde_json::json!({
            "error": { "code": "forbidden", "message": why }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn keys() -> ActorKeys {
        ActorKeys {
            human: "human-secret".into(),
            automation: "auto-secret".into(),
        }
    }

    #[test]
    fn identifies_human_and_automation_by_distinct_keys() {
        let k = keys();
        let mut h = HeaderMap::new();
        h.insert(
            "x-sin90-actor-key",
            HeaderValue::from_static("human-secret"),
        );
        assert_eq!(k.identify(&h), Some(Actor::Human));

        let mut a = HeaderMap::new();
        a.insert("x-sin90-actor-key", HeaderValue::from_static("auto-secret"));
        assert_eq!(k.identify(&a), Some(Actor::Automation));
    }

    #[test]
    fn unrecognized_or_missing_key_identifies_as_none() {
        let k = keys();
        assert_eq!(k.identify(&HeaderMap::new()), None);

        let mut wrong = HeaderMap::new();
        wrong.insert("x-sin90-actor-key", HeaderValue::from_static("nope"));
        assert_eq!(k.identify(&wrong), None);

        // Regression: `Authorization` must NOT be recognized — Agent24's real
        // proxy strips it before a request reaches this module, so a key read
        // off it would only ever work in a test that bypasses the proxy.
        let mut auth_header = HeaderMap::new();
        auth_header.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer human-secret"),
        );
        assert_eq!(k.identify(&auth_header), None);
    }

    #[test]
    fn generated_keys_are_not_reused_between_two_calls() {
        // Regression pin: a copy-paste bug that returned the same random buffer
        // for both keys would silently erase the human/automation distinction.
        // Calls `random_key()` directly rather than `from_env_or_generate` so
        // this assertion holds regardless of the test process's environment.
        let h1 = random_key();
        let a1 = random_key();
        let h2 = random_key();
        assert_ne!(h1, a1, "human and automation keys must differ");
        assert_ne!(h1, h2, "two generation calls must not collide");
    }
}
