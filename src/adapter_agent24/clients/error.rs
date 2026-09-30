//! ME4-5.2.1 migration (`docs/design/ME4-S3-os-sdk.md` §5.1): this crate's
//! own hand-written `ClientError` closed set (and its two mappings —
//! transport-level failures and the kernel's `error.data.kind`) moved into
//! `agent24-os-sdk`, which SDK ports it VERBATIM (`agent24_os_sdk::error`'s
//! own module doc: "Ported verbatim from Sin90 `adapter_agent24/clients/
//! error.rs`"), including `is_permanent()`/`is_retryable()` and the exact
//! same 18-kernel-kind classification table. Every call site in this crate
//! only ever matched on `ClientError`'s variant (never on anything
//! Sin90-specific), so re-exporting the SDK's type needs no further mapping
//! layer here — a `pub use` is the whole file.
//!
//! The one thing that did NOT move: `ClientError::Unavailable.cause` was
//! `crate::ai::UnavailableCause` here (an explicit dependency this file's
//! own old doc comment called out — "`adapter_agent24` may depend on `ai`");
//! the SDK owns an equivalent, but domain-decoupled, four-value closed set
//! (`agent24_os_sdk::UnavailableCause`) instead, so as not to make the SDK
//! depend on any one module's domain types. The one call site that cared
//! about the SIN90 domain type — `clients::model`'s `ModelPort` impl, which
//! hands `ai::ModelFailure::Unavailable` a `crate::ai::UnavailableCause` —
//! converts explicitly at that one boundary
//! (`clients::model::map_client_error_to_model_failure`); nothing else in
//! this crate matches on `cause` at all.
pub use agent24_os_sdk::ClientError;
