//! T3.2.1 — the CLOSED error set every typed kernel client
//! (`scheduler`/`memory`/`approval`) maps onto, so business code never
//! matches on a raw `transport::TransportError` or a kernel `error.data.kind`
//! string. Two independent sources collapse into this one enum:
//!
//! - `transport::TransportError`: this end's own connection-level failures
//!   (busy, not sent, connection lost, ...).
//! - `RpcErrorInfo`: the kernel's application-level failures, carried as
//!   `-32000` + `error.data.kind` (SPEC-ME3-OUT-OF-PROCESS.md §3's closed
//!   `ErrorKind` set) or the protocol-level `-32602 invalid params`.
//!
//! L-6: this is where the merge happens — this end's own
//! [`transport::TransportError::Busy`] (65th concurrent call, no slot) and
//! the kernel's `-32000 {kind: "busy"}` (its own token bucket empty) both
//! become [`ClientError::Busy`]. A caller cannot tell which side ran out of
//! room, and — per spec.md's M3 错误处理 table — does not need to: both are
//! retried the same way (backoff).
//!
//! [`ClientError::is_permanent`] / [`ClientError::is_retryable`] mirror
//! `spec.md`'s M3 分类 exactly: permanent = `forbidden`/`quota_exceeded`/
//! `invalid_params` (outbox rows built on top of this, T3.3.1/T3.3.2, flip to
//! `failed` on these); retryable = `rate_limited`/`busy`/`timeout`/
//! `not_ready`/`draining`/`not_sent` (backoff and try again). `connection_lost`
//! is neither — architecture.md 不可破边界 #5: the outcome is genuinely
//! unknown, and whether to resend is the CALLER's call, made on whatever
//! idempotence that caller has (outbox's `upsert`/`delete` are naturally
//! idempotent; a one-shot `approval/gate` submit is not, which is exactly why
//! its wire params carry `request_id`/`approval_token` for the kernel's own
//! idempotent lookup — see T7b's `find_existing`).

use crate::adapter_agent24::transport::{RpcErrorInfo, TransportError};

/// The closed set. Every variant carries the kernel's or this end's own
/// message, for logs — nothing in this crate matches on the STRING, only on
/// the variant.
#[derive(Debug, Clone, thiserror::Error, PartialEq)]
pub enum ClientError {
    /// The capability was not granted, or a gate action is outside the
    /// kernel's closed set. Permanent.
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// This end's token bucket, or the kernel's, is empty. Retryable
    /// (backoff).
    #[error("rate limited: {0}")]
    RateLimited(String),
    /// No in-flight slot — merged from both sides (L-6, module docs).
    /// Retryable (backoff).
    #[error("busy: {0}")]
    Busy(String),
    /// A per-module resource limit (e.g. 256 schedule rows) was hit.
    /// Permanent — retrying the SAME request changes nothing; the caller has
    /// to free a slot first (spec.md M3: outbox row → `failed`).
    #[error("quota exceeded: {0}")]
    QuotaExceeded(String),
    /// `-32602` or a wire-shape validation failure this client caught
    /// itself. Permanent — the params were wrong, not the timing.
    #[error("invalid params: {0}")]
    InvalidParams(String),
    /// No response in time, a bound `request_id` was not actually in
    /// flight, or a lifecycle budget ran out. Retryable.
    #[error("timeout: {0}")]
    Timeout(String),
    /// The generation has not finished its handshake yet. Retryable.
    #[error("not ready: {0}")]
    NotReady(String),
    /// The generation is draining. Retryable.
    #[error("draining: {0}")]
    Draining(String),
    /// The generation has been revoked — this connection's whole life is
    /// ending (architecture.md 不可破边界 #5: "回调连接断了 = 这一代结束").
    /// spec.md M3's own 分类 table does not mention `revoked` at all, and
    /// this crate does not guess: `is_permanent`/`is_retryable` both say
    /// `false` for it, same as [`Self::ConnectionLost`]. Retrying on THIS
    /// `Transport` cannot help either way (D1: no reconnect — the connection
    /// is dying with the generation), but that is a fact about the
    /// connection, not a classification this closed set makes for the
    /// caller.
    #[error("revoked: {0}")]
    Revoked(String),
    /// The kernel has no record of the id asked about (e.g.
    /// `_a24/approval/status` on an unknown `approval_id`). Also outside
    /// spec.md M3's table (which only classifies the errors a scheduler
    /// outbox row can hit) — `is_permanent`/`is_retryable` both say `false`.
    /// A future caller that needs "will re-querying the same id ever
    /// succeed" (almost certainly not) should classify it itself rather than
    /// rely on a guess baked in here.
    #[error("not found: {0}")]
    NotFound(String),
    /// The call was in flight when the connection died. Outcome UNKNOWN —
    /// see module docs: neither permanent nor retryable, the caller decides.
    #[error("connection lost; outcome unknown")]
    ConnectionLost,
    /// The call never reached the wire (connection already known dead when
    /// it was attempted). Retryable — but only ever on a NEW connection
    /// (`transport::TransportError::NotSent`'s own doc): this generation's
    /// `Transport` never reconnects, so "retry" here means "the next
    /// generation's business code tries again," not a loop in this one.
    #[error("call was not sent: {0}")]
    NotSent(String),
    /// Anything outside the closed set above: an unrecognised `error.data.kind`
    /// (e.g. `auth_failed`, `token_invalid` — handshake-only kinds that
    /// should never reach a business call), `-32601`/`-32603`, an oversized
    /// frame this client tried to send, an internal id collision, or a
    /// response that did not deserialize into the expected shape. Neither
    /// permanent nor retryable by default — an unclassified failure gets no
    /// free pass to either loop forever or give up silently.
    #[error("other: {0}")]
    Other(String),
}

impl ClientError {
    /// spec.md M3: retrying the exact same request can never turn this into
    /// success. Callers (T3.3.1's outbox) flip the row to `failed` and
    /// surface it, rather than retrying forever.
    #[must_use]
    pub fn is_permanent(&self) -> bool {
        matches!(
            self,
            ClientError::Forbidden(_)
                | ClientError::QuotaExceeded(_)
                | ClientError::InvalidParams(_)
        )
    }

    /// spec.md M3: a transient condition — backoff and try again is the
    /// right response.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            ClientError::RateLimited(_)
                | ClientError::Busy(_)
                | ClientError::Timeout(_)
                | ClientError::NotReady(_)
                | ClientError::Draining(_)
                | ClientError::NotSent(_)
        )
    }
}

/// The one place `transport::TransportError` becomes a [`ClientError`] —
/// every typed client (`scheduler`/`memory`/`approval`) routes through this,
/// so the merge decision (L-6) and the kind mapping live in exactly one spot.
///
/// T3.2.1a (this branch, stacked below `feat/t3.2.1b-scheduler-client`): no
/// typed client exists yet in THIS commit to call it outside this file's own
/// `#[cfg(test)]` module — `SchedulerClient` lands in the next branch of the
/// stack and calls this on every request. `#[cfg_attr(not(test), ...)]`
/// (not a bare `#[allow(dead_code)]`, matching `transport.rs`'s own
/// existing convention for "no production caller as of this commit") keeps
/// the lint live for the test build, where it already has callers.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn map_transport_error(err: TransportError) -> ClientError {
    // Computed before the match moves `err` into the `Rpc(info)` arm —
    // unused by that arm (it builds its own message from `info` instead via
    // `map_rpc_error`/`Display`), but every other arm wants it.
    let message = err.to_string();
    match err {
        TransportError::NotSent => ClientError::NotSent(message),
        TransportError::ConnectionLost => ClientError::ConnectionLost,
        TransportError::Busy => ClientError::Busy(message),
        TransportError::Timeout => ClientError::Timeout(message),
        TransportError::FrameTooLarge => ClientError::Other(message),
        TransportError::IdCollision => ClientError::Other(message),
        TransportError::Rpc(info) => map_rpc_error(&info),
    }
}

/// The kernel's own application error → [`ClientError`]. `-32602` (invalid
/// params, a protocol-level code, not an `error.data.kind`) maps first; every
/// other code — in practice always `-32000` (SPEC's "全部应用层错误用
/// -32000") — is classified by `data.kind`. A `-32000` with no `kind` at all,
/// or a kind outside SPEC's closed set (e.g. `auth_failed`/`token_invalid`,
/// which SPEC scopes to the handshake, not business calls), falls through to
/// [`ClientError::Other`] — deny by default, same posture the kernel's own
/// `map_memory_error` takes (Agent24 `os_memory_page.rs`).
///
/// Same T3.2.1a note as [`map_transport_error`]'s doc: no caller outside
/// this file's own tests until `feat/t3.2.1b-scheduler-client` lands.
#[cfg_attr(not(test), allow(dead_code))]
fn map_rpc_error(info: &RpcErrorInfo) -> ClientError {
    if info.code == -32602 {
        return ClientError::InvalidParams(info.to_string());
    }
    match info.kind.as_deref() {
        Some("forbidden") => ClientError::Forbidden(info.to_string()),
        Some("rate_limited") => ClientError::RateLimited(info.to_string()),
        Some("busy") => ClientError::Busy(info.to_string()),
        Some("quota_exceeded") => ClientError::QuotaExceeded(info.to_string()),
        Some("timeout") => ClientError::Timeout(info.to_string()),
        Some("not_ready") => ClientError::NotReady(info.to_string()),
        Some("draining") => ClientError::Draining(info.to_string()),
        Some("revoked") => ClientError::Revoked(info.to_string()),
        Some("not_found") => ClientError::NotFound(info.to_string()),
        _ => ClientError::Other(info.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rpc(kind: &str) -> TransportError {
        TransportError::Rpc(RpcErrorInfo {
            code: -32000,
            kind: Some(kind.to_string()),
            message: "test".to_string(),
            raw: serde_json::json!({}),
        })
    }

    /// The full classification table, one row per variant, each with its own
    /// positive control (the variant it must NOT be). spec.md M3: permanent
    /// = forbidden/quota_exceeded/invalid_params; retryable =
    /// rate_limited/busy/timeout/not_ready/draining/not_sent;
    /// connection_lost is neither.
    #[test]
    fn classification_matches_spec_md_m3_exactly() {
        let cases: &[(ClientError, bool, bool)] = &[
            (ClientError::Forbidden("x".into()), true, false),
            (ClientError::QuotaExceeded("x".into()), true, false),
            (ClientError::InvalidParams("x".into()), true, false),
            (ClientError::RateLimited("x".into()), false, true),
            (ClientError::Busy("x".into()), false, true),
            (ClientError::Timeout("x".into()), false, true),
            (ClientError::NotReady("x".into()), false, true),
            (ClientError::Draining("x".into()), false, true),
            (ClientError::NotSent("x".into()), false, true),
            (ClientError::ConnectionLost, false, false),
            (ClientError::Revoked("x".into()), false, false),
            (ClientError::NotFound("x".into()), false, false),
            (ClientError::Other("x".into()), false, false),
        ];
        for (err, want_permanent, want_retryable) in cases {
            assert_eq!(
                err.is_permanent(),
                *want_permanent,
                "{err:?}.is_permanent()"
            );
            assert_eq!(
                err.is_retryable(),
                *want_retryable,
                "{err:?}.is_retryable()"
            );
            // No variant is ever BOTH — the two predicates must stay
            // mutually exclusive as the table grows.
            assert!(
                !(err.is_permanent() && err.is_retryable()),
                "{err:?} is both"
            );
        }
    }

    /// Mutation this test exists to catch: if `is_retryable` accidentally
    /// classified `quota_exceeded` as retryable (e.g. someone "fixes" a
    /// perceived gap by adding it to the retryable `matches!` arm), this
    /// goes red. Written as its own test — not folded into the table above —
    /// so it survives even if the table's shape changes later.
    #[test]
    fn quota_exceeded_must_never_be_retryable() {
        assert!(!ClientError::QuotaExceeded("x".into()).is_retryable());
        assert!(ClientError::QuotaExceeded("x".into()).is_permanent());
    }

    #[test]
    fn l6_this_ends_busy_and_the_kernels_busy_both_map_to_client_error_busy() {
        assert!(matches!(
            map_transport_error(TransportError::Busy),
            ClientError::Busy(_)
        ));
        assert!(matches!(
            map_transport_error(rpc("busy")),
            ClientError::Busy(_)
        ));
    }

    #[test]
    fn invalid_params_code_is_recognised_even_without_a_kind() {
        let err = TransportError::Rpc(RpcErrorInfo {
            code: -32602,
            kind: None,
            message: "bad key".to_string(),
            raw: serde_json::json!({}),
        });
        assert!(matches!(
            map_transport_error(err),
            ClientError::InvalidParams(_)
        ));
    }

    #[test]
    fn an_unrecognised_kind_falls_through_to_other_not_silently_ignored() {
        // e.g. `auth_failed`/`token_invalid` — real SPEC kinds, just not ones
        // a business call after a successful handshake should ever see.
        assert!(matches!(
            map_transport_error(rpc("auth_failed")),
            ClientError::Other(_)
        ));
    }

    #[test]
    fn connection_lost_and_not_sent_map_distinctly() {
        assert_eq!(
            map_transport_error(TransportError::ConnectionLost),
            ClientError::ConnectionLost
        );
        assert!(matches!(
            map_transport_error(TransportError::NotSent),
            ClientError::NotSent(_)
        ));
    }
}
