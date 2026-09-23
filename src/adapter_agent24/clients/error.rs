//! T3.2.1 — the CLOSED error set every typed kernel client
//! (`scheduler`/`memory`/`approval`) maps onto, so business code never
//! matches on a raw `transport::TransportError` or a kernel `error.data.kind`
//! string. Two independent sources collapse into this one enum:
//!
//! - `transport::TransportError`: this end's own connection-level failures
//!   (busy, not sent, connection lost, ...).
//! - `RpcErrorInfo`: the kernel's application-level failures, carried as
//!   `-32000` + `error.data.kind` (SPEC-ME3-OUT-OF-PROCESS.md §3's closed
//!   `ErrorKind` set — exactly 17 members today: `forbidden`/`busy`/
//!   `cancelled`/`timeout`/`quota_exceeded`/`invalid_lease`/
//!   `unknown_capability`/`version_mismatch`/`auth_failed`/
//!   `manifest_mismatch`/`not_ready`/`draining`/`revoked`/`rate_limited`/
//!   `payload_too_large`/`token_invalid`/`not_found` — this file's own
//!   `tests::every_spec_error_kind_maps_to_its_documented_variant` pins the
//!   count) or the protocol-level `-32602 invalid params`.
//!
//! Only `auth_failed` and `manifest_mismatch` are handshake-only (SPEC §3:
//! "认证失败...并断连"/"manifest 摘要不符...并断连", both under the
//! `initialize` row) — every other kind, INCLUDING `token_invalid` and
//! `payload_too_large`, is reachable from an ordinary post-handshake business
//! call and is mapped to its own variant below, not left to fall through to
//! [`ClientError::Other`] (a review round on this file's first cut got that
//! wrong for both; fixed here — see [`ClientError::TokenInvalid`] /
//! [`ClientError::PayloadTooLarge`]'s own docs for where each is actually
//! reachable).
//!
//! L-6: this is where the merge happens — this end's own
//! [`transport::TransportError::Busy`] (65th concurrent call, no slot) and
//! the kernel's `-32000 {kind: "busy"}` (its own token bucket empty) both
//! become [`ClientError::Busy`]. A caller cannot tell which side ran out of
//! room, and — per spec.md's M3 错误处理 table — does not need to: both are
//! retried the same way (backoff). The same merge happens for oversized
//! payloads: this end's own outgoing-frame size check
//! (`transport::TransportError::FrameTooLarge`) and the kernel's own
//! `payload_too_large` (`rpc.rs`'s dispatch-wide params budget) both become
//! [`ClientError::PayloadTooLarge`] — both mean "shrink the request before
//! sending it again," regardless of which side would have rejected it.
//!
//! [`ClientError::is_permanent`] / [`ClientError::is_retryable`] mirror
//! `spec.md`'s M3 分类 for the errors it enumerates: permanent =
//! `forbidden`/`quota_exceeded`/`invalid_params` (outbox rows built on top of
//! this, T3.3.1/T3.3.2, flip to `failed` on these) plus, as of this review
//! round, `token_invalid`/`payload_too_large` (the same "retrying the exact
//! same request cannot help" reasoning — see each variant's own doc);
//! retryable = `rate_limited`/`busy`/`timeout`/`not_ready`/`draining`/
//! `not_sent` (backoff and try again). `connection_lost` and the new
//! [`ClientError::RequestNotInFlight`] are neither — see their own docs for
//! why each is a genuinely different kind of "don't just retry this."

use serde_json::Value;

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
    /// No response within this end's own deadline, or (on the kernel side) a
    /// lifecycle budget ran out while this call was bound to it. Retryable —
    /// but see [`Self::RequestNotInFlight`] for the specific sub-case that
    /// looks identical on the wire (`kind: "timeout"`, no other field) yet
    /// must NOT be retried with the same `request_id`: `map_rpc_error` only
    /// returns THIS variant when the kernel did not also say
    /// `data.retryable: false`.
    #[error("timeout: {0}")]
    Timeout(String),
    /// The kernel said `kind: "timeout"` AND `data.retryable: false`
    /// (SPEC-ME3-OUT-OF-PROCESS.md §3 / `ME4-S1-scheduler-callback.md` §6.4
    /// step 3b): the `request_id` this call carried was never — or is no
    /// longer — in flight on this connection (`admit_callback_bound` returns
    /// `Ok(None)`). Retrying with the SAME `request_id` can never succeed:
    /// id-bound calls only admit while bound to a still-live proxied request
    /// (a `fired` handler in progress, an in-flight gated action, ...), and
    /// that window has already closed. Neither permanent (the underlying
    /// action was never attempted, let alone rejected) nor retryable
    /// (repeating the exact same call is guaranteed to fail the same way) —
    /// the design doc's own prescribed fix is to drop the `request_id` and
    /// resend as an UNBOUND, background call instead (T3.3.2's reconciler
    /// always does this: outbox retries never carry a proxied request's
    /// `request_id`).
    #[error("request_id not in flight (do not retry with the same id): {0}")]
    RequestNotInFlight(String),
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
    /// A one-time secret (`approval_token`) was rejected: already consumed,
    /// expired (SPEC: injected tokens are invalidated the instant the
    /// delivery that carried them ends), or simply wrong. Reachable from an
    /// ordinary `_a24/approval/{gate,advise}` submit — NOT handshake-only
    /// (Agent24 `approval_callback.rs`'s `ApprovalCallbackRefused::TokenInvalid`
    /// is exactly this, on a normal post-handshake call). Permanent: the
    /// token that failed is gone either way, and resubmitting with the SAME
    /// stale token cannot succeed — the caller needs a fresh token from a
    /// new proxied request (`_a24/approval/*`'s own doc: submit BEFORE the
    /// delivery window that carried the token ends, and dedupe by `fire_id`
    /// first so a retry never needs a second token at all).
    #[error("token invalid: {0}")]
    TokenInvalid(String),
    /// The kernel's dispatch-wide params size budget (`rpc.rs`) rejected this
    /// request before it reached any method handler — reachable for any
    /// method, not handshake-only. Permanent, same reasoning as
    /// [`Self::InvalidParams`]: the request itself is the problem, not the
    /// timing; merged with this end's own outgoing-frame size check
    /// (`transport::TransportError::FrameTooLarge`, module docs) since both
    /// mean the same thing to a caller.
    #[error("payload too large: {0}")]
    PayloadTooLarge(String),
    /// The kernel has no record of the id asked about (e.g.
    /// `_a24/approval/status` on an unknown `approval_id`). Also outside
    /// spec.md M3's table (which only classifies the errors a scheduler
    /// outbox row can hit) — `is_permanent`/`is_retryable` both say `false`
    /// in THIS closed set's own predicates, but a caller with a narrower
    /// question can classify it further itself: for `approval/status`
    /// specifically, re-querying the exact same `approval_id` can never
    /// start succeeding (the id either exists or it does not), which makes
    /// it PERMANENT for that one call site even though this shared enum does
    /// not bake that in generically for every possible future user of
    /// `not_found`.
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
    /// Anything outside the closed set above: an unrecognised
    /// `error.data.kind` (`cancelled`/`invalid_lease`/`unknown_capability`/
    /// `version_mismatch`, or the two genuinely handshake-only kinds
    /// `auth_failed`/`manifest_mismatch` somehow reaching a business call,
    /// which should never happen post-handshake), `-32601`, an internal id
    /// collision, or a response that did not deserialize into the expected
    /// shape. `-32603` internal/storage errors also land here (Agent24's own
    /// examples: approval's "the approval backend is temporarily
    /// unavailable" and memory's generic storage-error message,
    /// `os_memory_page.rs`'s `map_memory_error`'s deny-by-default fallback)
    /// — SPEC keeps `-32603`'s message fixed and carries no `data.kind` at
    /// all, so there is nothing to classify on. Those two ARE typically
    /// transient in practice (a storage hiccup, not a permanent rejection),
    /// but this variant does not promise that by returning `is_retryable() ==
    /// true` for them — a caller that wants to treat `-32603` as
    /// backoff-and-retry should decide that explicitly, not rely on a guess
    /// baked in here. Neither permanent nor retryable by default — an
    /// unclassified failure gets no free pass to either loop forever or give
    /// up silently.
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
                | ClientError::TokenInvalid(_)
                | ClientError::PayloadTooLarge(_)
        )
    }

    /// spec.md M3: a transient condition — backoff and try again is the
    /// right response, PROVIDED the call being retried is itself idempotent.
    /// This predicate does not and cannot check that — it only says "the
    /// KERNEL-side or connection-side condition that failed this call is the
    /// kind that goes away on its own," not "it is safe for THIS caller to
    /// resend THIS request." A caller with a non-idempotent action (nothing
    /// in this crate's own typed clients is one: `scheduler::upsert`/`delete`
    /// are naturally idempotent, `approval::gate`/`advise` dedupe by
    /// `(module, request_id, kind)` server-side) must establish its own
    /// idempotence before treating `true` here as "go ahead and resend."
    ///
    /// [`Self::Timeout`] deserves a specific callout even though it IS
    /// `true` here: a timeout means the outcome is genuinely UNKNOWN (the
    /// kernel may have already executed the side effect before the response
    /// was lost) — the exact same uncertainty [`Self::ConnectionLost`]
    /// carries (which this predicate returns `false` for, precisely so a
    /// caller cannot mistake it for an unconditional green light). Both are
    /// "safe to retry only because a well-formed retry is idempotent," never
    /// "definitely did not happen the first time."
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
        // Merged with the kernel's own `payload_too_large` — module docs.
        TransportError::FrameTooLarge => ClientError::PayloadTooLarge(message),
        TransportError::IdCollision => ClientError::Other(message),
        TransportError::Rpc(info) => map_rpc_error(&info),
    }
}

/// `raw["data"]["retryable"] == false`, the one signal that distinguishes
/// [`ClientError::RequestNotInFlight`] from a plain [`ClientError::Timeout`]
/// — both carry `kind: "timeout"` on the wire (H1: `RpcErrorInfo::kind`
/// alone cannot tell them apart, only the sibling `data.retryable` field
/// can). Absent entirely (a plain timeout never sets it) is treated as "not
/// `false`," i.e. the ordinary retryable case — this is deliberately NOT
/// `.unwrap_or(true)`-style guessing on a present-but-unexpected-type value;
/// anything other than a literal JSON `false` falls through to the ordinary
/// `Timeout` classification.
fn request_id_explicitly_not_in_flight(info: &RpcErrorInfo) -> bool {
    info.raw
        .get("data")
        .and_then(|d| d.get("retryable"))
        .and_then(Value::as_bool)
        == Some(false)
}

/// The kernel's own application error → [`ClientError`]. `-32602` (invalid
/// params, a protocol-level code, not an `error.data.kind`) maps first; every
/// other code — in practice always `-32000` (SPEC's "全部应用层错误用
/// -32000") — is classified by `data.kind`. A `-32000` with no `kind` at all,
/// or a kind outside SPEC's closed set, falls through to
/// [`ClientError::Other`] — deny by default, same posture the kernel's own
/// `map_memory_error` takes (Agent24 `os_memory_page.rs`). See the module
/// docs for which of the 17 real SPEC kinds get their own variant and which
/// fall through on purpose.
fn map_rpc_error(info: &RpcErrorInfo) -> ClientError {
    if info.code == -32602 {
        return ClientError::InvalidParams(info.to_string());
    }
    match info.kind.as_deref() {
        Some("forbidden") => ClientError::Forbidden(info.to_string()),
        Some("rate_limited") => ClientError::RateLimited(info.to_string()),
        Some("busy") => ClientError::Busy(info.to_string()),
        Some("quota_exceeded") => ClientError::QuotaExceeded(info.to_string()),
        Some("timeout") if request_id_explicitly_not_in_flight(info) => {
            ClientError::RequestNotInFlight(info.to_string())
        }
        Some("timeout") => ClientError::Timeout(info.to_string()),
        Some("not_ready") => ClientError::NotReady(info.to_string()),
        Some("draining") => ClientError::Draining(info.to_string()),
        Some("revoked") => ClientError::Revoked(info.to_string()),
        Some("token_invalid") => ClientError::TokenInvalid(info.to_string()),
        Some("payload_too_large") => ClientError::PayloadTooLarge(info.to_string()),
        Some("not_found") => ClientError::NotFound(info.to_string()),
        _ => ClientError::Other(info.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// No `data.retryable` field at all — the ordinary shape for every kind
    /// except the H1 timeout/retryable:false sub-case.
    fn rpc(kind: &str) -> TransportError {
        TransportError::Rpc(RpcErrorInfo {
            code: -32000,
            kind: Some(kind.to_string()),
            message: "test".to_string(),
            raw: json!({}),
        })
    }

    /// Same shape `rpc` builds, plus a `data.retryable` field — used for the
    /// H1 timeout/retryable:false distinction, which `kind` alone cannot
    /// express.
    fn rpc_with_retryable(kind: &str, retryable: bool) -> TransportError {
        TransportError::Rpc(RpcErrorInfo {
            code: -32000,
            kind: Some(kind.to_string()),
            message: "test".to_string(),
            raw: json!({"code": -32000, "message": "test", "data": {"kind": kind, "retryable": retryable}}),
        })
    }

    /// The full classification table, one row per variant, each with its own
    /// positive control (the variant it must NOT be). spec.md M3 plus this
    /// review round's additions: permanent = forbidden/quota_exceeded/
    /// invalid_params/token_invalid/payload_too_large; retryable =
    /// rate_limited/busy/timeout/not_ready/draining/not_sent;
    /// connection_lost/revoked/not_found/request_not_in_flight/other are
    /// neither.
    #[test]
    fn classification_matches_spec_md_m3_exactly() {
        let cases: &[(ClientError, bool, bool)] = &[
            (ClientError::Forbidden("x".into()), true, false),
            (ClientError::QuotaExceeded("x".into()), true, false),
            (ClientError::InvalidParams("x".into()), true, false),
            (ClientError::TokenInvalid("x".into()), true, false),
            (ClientError::PayloadTooLarge("x".into()), true, false),
            (ClientError::RateLimited("x".into()), false, true),
            (ClientError::Busy("x".into()), false, true),
            (ClientError::Timeout("x".into()), false, true),
            (ClientError::NotReady("x".into()), false, true),
            (ClientError::Draining("x".into()), false, true),
            (ClientError::NotSent("x".into()), false, true),
            (ClientError::ConnectionLost, false, false),
            (ClientError::Revoked("x".into()), false, false),
            (ClientError::NotFound("x".into()), false, false),
            (ClientError::RequestNotInFlight("x".into()), false, false),
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

    /// L-6's payload-size counterpart: this end's own outgoing size check
    /// and the kernel's own `payload_too_large` must map the same way.
    #[test]
    fn this_ends_frame_too_large_and_the_kernels_payload_too_large_map_the_same() {
        let from_this_end = map_transport_error(TransportError::FrameTooLarge);
        let from_kernel = map_transport_error(rpc("payload_too_large"));
        assert!(matches!(from_this_end, ClientError::PayloadTooLarge(_)));
        assert!(matches!(from_kernel, ClientError::PayloadTooLarge(_)));
        assert!(from_this_end.is_permanent());
        assert!(from_kernel.is_permanent());
    }

    #[test]
    fn invalid_params_code_is_recognised_even_without_a_kind() {
        let err = TransportError::Rpc(RpcErrorInfo {
            code: -32602,
            kind: None,
            message: "bad key".to_string(),
            raw: json!({}),
        });
        assert!(matches!(
            map_transport_error(err),
            ClientError::InvalidParams(_)
        ));
    }

    /// M1: `token_invalid` is reachable from an ordinary business call
    /// (`approval_callback.rs`'s `TokenInvalid` refusal) — it must NOT fall
    /// through to `Other`, and must be permanent (a stale token cannot be
    /// redeemed by resubmitting).
    #[test]
    fn token_invalid_is_a_dedicated_permanent_variant_not_other() {
        let err = map_transport_error(rpc("token_invalid"));
        assert!(matches!(err, ClientError::TokenInvalid(_)));
        assert!(err.is_permanent());
        assert!(!err.is_retryable());
    }

    /// H1: the specific sub-case — `kind: "timeout"` PLUS
    /// `data.retryable: false` — must map to `RequestNotInFlight`, not
    /// `Timeout`, and must be neither permanent nor retryable.
    #[test]
    fn timeout_with_retryable_false_maps_to_request_not_in_flight_not_plain_timeout() {
        let err = map_transport_error(rpc_with_retryable("timeout", false));
        assert!(matches!(err, ClientError::RequestNotInFlight(_)));
        assert!(!err.is_permanent());
        assert!(!err.is_retryable());
    }

    /// Positive control for the test above: a plain `timeout` with no
    /// `data.retryable` field at all (the ordinary shape) still maps to
    /// `Timeout` and is still retryable — the H1 fix must not have widened
    /// to swallow the common case.
    #[test]
    fn timeout_without_a_retryable_field_is_still_plain_timeout_and_retryable() {
        let err = map_transport_error(rpc("timeout"));
        assert!(matches!(err, ClientError::Timeout(_)));
        assert!(err.is_retryable());
        assert!(!err.is_permanent());
    }

    /// Same wire shape, `data.retryable: true` (explicit, not just absent) —
    /// must not be misread as the H1 sub-case either.
    #[test]
    fn timeout_with_retryable_true_is_still_plain_timeout() {
        let err = map_transport_error(rpc_with_retryable("timeout", true));
        assert!(matches!(err, ClientError::Timeout(_)));
    }

    #[test]
    fn an_unrecognised_kind_falls_through_to_other_not_silently_ignored() {
        // `auth_failed` — a real SPEC kind, but one of only two that are
        // genuinely handshake-only (module docs); it should never reach a
        // business call, and if it somehow does, this crate does not invent
        // a dedicated variant for it.
        assert!(matches!(
            map_transport_error(rpc("auth_failed")),
            ClientError::Other(_)
        ));
    }

    /// M4: every one of SPEC's 17 real `error.data.kind` strings, fed
    /// through `map_transport_error`, lands on its documented variant — the
    /// ones this file gives a dedicated variant to, and an explicit assertion
    /// of `Other` for the rest, so "I forgot to classify this one" and "I
    /// deliberately fall through" read identically different in the table.
    /// Mutation: change any one expected variant below to a wrong one — the
    /// corresponding `assert!` goes red (verified by hand for
    /// `payload_too_large`; see PR notes).
    #[test]
    fn every_spec_error_kind_maps_to_its_documented_variant() {
        // Named so clippy's `type_complexity` lint doesn't flag the table's
        // own type — purely a readability alias, not a semantic type.
        type Case = (&'static str, fn(&ClientError) -> bool);
        let cases: &[Case] = &[
            ("forbidden", |e| matches!(e, ClientError::Forbidden(_))),
            ("busy", |e| matches!(e, ClientError::Busy(_))),
            ("cancelled", |e| matches!(e, ClientError::Other(_))),
            ("timeout", |e| matches!(e, ClientError::Timeout(_))),
            ("quota_exceeded", |e| {
                matches!(e, ClientError::QuotaExceeded(_))
            }),
            ("invalid_lease", |e| matches!(e, ClientError::Other(_))),
            ("unknown_capability", |e| matches!(e, ClientError::Other(_))),
            ("version_mismatch", |e| matches!(e, ClientError::Other(_))),
            ("auth_failed", |e| matches!(e, ClientError::Other(_))),
            ("manifest_mismatch", |e| matches!(e, ClientError::Other(_))),
            ("not_ready", |e| matches!(e, ClientError::NotReady(_))),
            ("draining", |e| matches!(e, ClientError::Draining(_))),
            ("revoked", |e| matches!(e, ClientError::Revoked(_))),
            ("rate_limited", |e| matches!(e, ClientError::RateLimited(_))),
            ("payload_too_large", |e| {
                matches!(e, ClientError::PayloadTooLarge(_))
            }),
            ("token_invalid", |e| {
                matches!(e, ClientError::TokenInvalid(_))
            }),
            ("not_found", |e| matches!(e, ClientError::NotFound(_))),
        ];
        assert_eq!(
            cases.len(),
            17,
            "SPEC-ME3-OUT-OF-PROCESS.md §3's closed error.data.kind set has exactly 17 members \
             as of this writing — update this count (and the table) if SPEC grows it"
        );
        for (kind, expect_variant) in cases {
            let mapped = map_transport_error(rpc(kind));
            assert!(
                expect_variant(&mapped),
                "kind {kind:?} mapped to {mapped:?}, not the documented variant"
            );
        }
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
