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

    /// Identify the actor behind `Authorization: Bearer <key>`. `None` means
    /// no recognized key was presented at all (missing header, or a key that
    /// matches neither).
    pub fn identify(&self, headers: &HeaderMap) -> Option<Actor> {
        let key = bearer_token(headers)?;
        if key == self.human {
            Some(Actor::Human)
        } else if key == self.automation {
            Some(Actor::Automation)
        } else {
            None
        }
    }
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
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
            "authorization",
            HeaderValue::from_static("Bearer human-secret"),
        );
        assert_eq!(k.identify(&h), Some(Actor::Human));

        let mut a = HeaderMap::new();
        a.insert(
            "authorization",
            HeaderValue::from_static("Bearer auto-secret"),
        );
        assert_eq!(k.identify(&a), Some(Actor::Automation));
    }

    #[test]
    fn unrecognized_or_missing_key_identifies_as_none() {
        let k = keys();
        assert_eq!(k.identify(&HeaderMap::new()), None);

        let mut wrong = HeaderMap::new();
        wrong.insert("authorization", HeaderValue::from_static("Bearer nope"));
        assert_eq!(k.identify(&wrong), None);
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
