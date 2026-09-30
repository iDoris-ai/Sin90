//! ME4-5.2.1 migration (`docs/design/ME4-S3-os-sdk.md` §5.1): the
//! `_a24/approval/{gate,advise,status}` client moved into `agent24-os-sdk`
//! verbatim — a pure re-export, no newtype, unlike `scheduler`/`model`.
//! Reason for the difference: this crate has no BUSINESS caller of approval
//! today (§1.2 row 15 — "Sin90 没有业务调用方，只有 test-hooks 路由"), so
//! there is no pre-existing production call shape (and, unlike `reconciler.
//! rs`/`clients/model.rs`, no test module ME4-5.2.1 promises to keep
//! byte-identical) worth preserving behind a wrapper. `kernel_roundtrip.rs`
//! (`test-hooks` only) is this crate's one caller, and is updated in this
//! same migration to the SDK's own `ApprovalSubmit`/`RequestContext` shapes
//! directly.
//!
//! `ApprovalToken` in particular is no longer constructible by this crate at
//! all outside a real proxied request's headers (`agent24_os_sdk::
//! RequestContext::from_headers`) — the SDK dropped the old `ApprovalToken::
//! new(impl Into<String>)` escape hatch this crate's own type used to offer,
//! which is the stricter (and, per §2.4, intentional) posture: "a module
//! cannot mint one and bind it to a request it was not actually handling."
pub use agent24_os_sdk::{
    ApprovalAnswer, ApprovalClient, ApprovalDecision as ModuleApprovalDecision,
    ApprovalKind as ModuleApprovalKind, ApprovalSubmit,
};
pub use agent24_os_sdk::{ApprovalToken, RequestContext, RequestId};

/// The prefix `Offer.provides` must cover for `ApprovalClient::new` to
/// return `Some`.
pub const PREFIX: &str = "_a24/approval/";
