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

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

/// File name of the persisted keys, inside the module's data directory.
pub const KEY_FILE: &str = "actor-keys.json";
const ENV_HUMAN: &str = "SIN90_HUMAN_KEY";
const ENV_AUTOMATION: &str = "SIN90_AUTOMATION_KEY";
/// Generated keys are 48 hex chars; 32 is the floor for operator-supplied ones.
const MIN_KEY_LEN: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("{0} key is too short (minimum {MIN_KEY_LEN} characters)")]
    TooShort(&'static str),
    #[error("{0} key must be printable ASCII with no spaces (it travels in an HTTP header)")]
    NotHeaderSafe(&'static str),
    #[error("human and automation keys must differ")]
    Identical,
    #[error("set both {ENV_HUMAN} and {ENV_AUTOMATION}, or neither")]
    PartialEnv,
    #[error("no key source: set {ENV_HUMAN}/{ENV_AUTOMATION}, or give a data directory")]
    NoSource,
    #[error(
        "{path} is readable or writable by group/other (mode {mode:o}); run `chmod 600` on it"
    )]
    TooOpen { path: String, mode: u32 },
    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not a valid key file: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyFile {
    human: String,
    automation: String,
}

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
    /// Build from two explicit keys, rejecting any pair that would weaken the
    /// human/automation boundary: a blank or short key (an empty
    /// `x-sin90-actor-key` would otherwise authenticate), a key that cannot
    /// travel in an HTTP header, or two identical keys (the human comparison
    /// runs first, so identical keys silently promote automation to human).
    pub fn new(human: String, automation: String) -> Result<Self, KeyError> {
        validate_key("human", &human)?;
        validate_key("automation", &automation)?;
        if human == automation {
            return Err(KeyError::Identical);
        }
        Ok(Self { human, automation })
    }

    /// Resolve the two keys for a real process. Order:
    ///
    /// 1. `SIN90_HUMAN_KEY` + `SIN90_AUTOMATION_KEY` — both or neither.
    /// 2. `<data_dir>/actor-keys.json`, created on first start with mode
    ///    `0600` and reused afterwards. This is the only path that works under
    ///    Agent24: its module launch passes a fixed env allowlist, never
    ///    arbitrary `SIN90_*` variables, so env keys cannot reach a mounted
    ///    module.
    ///
    /// Raw keys are never written to stdout/stderr — Agent24 re-logs every
    /// module output line, so anything printed there is readable by anyone
    /// who can read the daemon log. Only the key file's path is reported.
    pub fn load(data_dir: Option<&Path>) -> Result<Self, KeyError> {
        let human = std::env::var(ENV_HUMAN).ok();
        let automation = std::env::var(ENV_AUTOMATION).ok();
        match (human, automation) {
            (Some(h), Some(a)) => return Self::new(h, a),
            (None, None) => {}
            _ => return Err(KeyError::PartialEnv),
        }
        let dir = data_dir.ok_or(KeyError::NoSource)?;
        load_or_create_file(&dir.join(KEY_FILE))
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

fn validate_key(which: &'static str, key: &str) -> Result<(), KeyError> {
    if key.len() < MIN_KEY_LEN {
        return Err(KeyError::TooShort(which));
    }
    if !key.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err(KeyError::NotHeaderSafe(which));
    }
    Ok(())
}

/// Create-if-absent without a window where the file exists half-written: the
/// keys go to a private temp file first, then a hard link publishes it under
/// the real name (atomic, and fails with `AlreadyExists` if another process
/// won the race — in which case its keys are the ones to use).
fn load_or_create_file(path: &Path) -> Result<ActorKeys, KeyError> {
    if path.exists() {
        return read_file(path);
    }
    let io = |p: &Path| {
        let p = p.display().to_string();
        move |source| KeyError::Io { path: p, source }
    };
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(io(dir))?;
    let tmp = dir.join(format!(".{KEY_FILE}.{}.tmp", std::process::id()));
    let keys = KeyFile {
        human: random_key(),
        automation: random_key(),
    };
    let body = serde_json::to_vec(&keys).expect("KeyFile serializes");
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(&body).and_then(|()| f.sync_all()));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(io(&tmp)(e));
    }
    let linked = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => {
            eprintln!(
                "sin90: created actor keys at {} (mode 0600)",
                path.display()
            );
            ActorKeys::new(keys.human, keys.automation)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => read_file(path),
        Err(e) => Err(io(path)(e)),
    }
}

fn read_file(path: &Path) -> Result<ActorKeys, KeyError> {
    let io = |source| KeyError::Io {
        path: path.display().to_string(),
        source,
    };
    let mode = std::fs::metadata(path).map_err(io)?.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(KeyError::TooOpen {
            path: path.display().to_string(),
            mode,
        });
    }
    let bytes = std::fs::read(path).map_err(io)?;
    let keys: KeyFile = serde_json::from_slice(&bytes).map_err(|source| KeyError::Parse {
        path: path.display().to_string(),
        source,
    })?;
    ActorKeys::new(keys.human, keys.automation)
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
        // Calls `random_key()` directly rather than `load` so
        // this assertion holds regardless of the test process's environment.
        let h1 = random_key();
        let a1 = random_key();
        let h2 = random_key();
        assert_ne!(h1, a1, "human and automation keys must differ");
        assert_ne!(h1, h2, "two generation calls must not collide");
    }

    const H: &str = "hhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhh";
    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "sin90-actor-{name}-{}-{}",
            std::process::id(),
            random_key()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn weak_or_identical_keys_are_refused() {
        assert!(matches!(
            ActorKeys::new(String::new(), A.into()),
            Err(KeyError::TooShort("human"))
        ));
        assert!(matches!(
            ActorKeys::new(H.into(), "short".into()),
            Err(KeyError::TooShort("automation"))
        ));
        let spaced = format!("{} x", &H[..MIN_KEY_LEN]);
        assert!(matches!(
            ActorKeys::new(spaced, A.into()),
            Err(KeyError::NotHeaderSafe("human"))
        ));
        assert!(matches!(
            ActorKeys::new(H.into(), H.into()),
            Err(KeyError::Identical)
        ));
        // Positive control: a valid distinct pair is accepted.
        assert!(ActorKeys::new(H.into(), A.into()).is_ok());
    }

    #[test]
    fn key_file_is_created_owner_only_and_reused_across_restarts() {
        let dir = scratch_dir("reuse");
        let path = dir.join(KEY_FILE);
        let first = load_or_create_file(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "key file must be owner-only");
        let second = load_or_create_file(&path).unwrap();
        assert_eq!(first.human, second.human, "restart must not rotate keys");
        assert_eq!(first.automation, second.automation);
        assert_ne!(first.human, first.automation);
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n != KEY_FILE)
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_group_readable_key_file_is_refused() {
        let dir = scratch_dir("open");
        let path = dir.join(KEY_FILE);
        load_or_create_file(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(matches!(
            load_or_create_file(&path),
            Err(KeyError::TooOpen { .. })
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_key_file_with_identical_keys_is_refused() {
        let dir = scratch_dir("same");
        let path = dir.join(KEY_FILE);
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap()
            .write_all(format!(r#"{{"human":"{H}","automation":"{H}"}}"#).as_bytes())
            .unwrap();
        assert!(matches!(
            load_or_create_file(&path),
            Err(KeyError::Identical)
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
