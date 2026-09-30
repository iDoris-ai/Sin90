//! ME4-5.2.1 migration (`docs/design/ME4-S3-os-sdk.md` §5.1): the
//! `_a24/scheduler/{upsert,delete,list}` wire client moved into
//! `agent24-os-sdk` (`ScheduleSpec`/`ScheduleState`/`UpsertResult`/etc. are
//! field-for-field identical to this file's OLD `ModuleSpec`/
//! `ModuleScheduleState`/`UpsertResponse` — transported over here as `pub
//! use … as …` under the old local names, so nothing outside this module
//! needs to change a type name).
//!
//! [`SchedulerClient`] itself stays a NEWTYPE around
//! `agent24_os_sdk::SchedulerClient` rather than a straight re-export,
//! specifically so [`SchedulerClient::upsert`] can keep this crate's OLD
//! five-positional-argument call shape (`key, spec, enabled, label,
//! request_id`) instead of the SDK's `&UpsertRequest` struct — `reconciler.
//! rs`'s own `#[cfg(test)] mod tests` (which must survive this migration
//! BYTE-IDENTICAL, §5.2 point 3) calls it exactly that way
//! (`fake_kernel_rejects_an_uppercase_key`, among others). `request_id`'s
//! type does change, from `Option<&str>` to `Option<&agent24_os_sdk::
//! RequestId>` (§5.1's own note) — every call site in this crate, test and
//! production alike, only ever passes the literal `None` there, so that
//! type change needs no source edits anywhere it is actually called.

use std::sync::Arc;

use agent24_os_proto::module::Connection;

use super::error::ClientError;
use agent24_os_sdk::RequestId;
pub use agent24_os_sdk::{
    DeleteOutcome, DeleteResult as DeleteResponse, LastFire, LastFires, ListResult as ListResponse,
    ScheduleSpec as ModuleSpec, ScheduleState as ModuleScheduleState, UpsertOutcome, UpsertRequest,
    UpsertResult as UpsertResponse,
};

/// The prefix `Offer.provides` must cover for [`SchedulerClient::new`] to
/// return `Some`.
pub const PREFIX: &str = "_a24/scheduler/";

/// Newtype around `agent24_os_sdk::SchedulerClient` — see this module's own
/// doc for why. Wraps it in an `Arc` (not bare) specifically so this type
/// stays cheaply `#[derive(Clone)]`: the SDK's own `SchedulerClient` does
/// not implement `Clone` itself (its internal `Core` field, cheap to clone
/// INSIDE the SDK, is private to that crate — this crate cannot reach in and
/// clone it by hand), and `reconciler.rs`'s protected test module clones a
/// `SchedulerClient` extensively (`scheduler.clone()`, dozens of call sites)
/// to hand a fresh handle to each `only_scheduler(...)`/`scheduler_and_memory(...)`
/// call while keeping the original for the next one.
#[derive(Clone)]
pub struct SchedulerClient(Arc<agent24_os_sdk::SchedulerClient>);

impl SchedulerClient {
    /// `None` unless the handshake's `Offer` granted [`PREFIX`]
    /// (architecture.md 不可破边界 #7) — same posture the pre-migration type
    /// had, now delegated to the SDK's own constructor.
    #[must_use]
    pub fn new(conn: &Arc<Connection>) -> Option<Self> {
        agent24_os_sdk::SchedulerClient::new(conn)
            .map(Arc::new)
            .map(Self)
    }

    /// Wraps an already-built SDK client (`agent24_os_sdk::Module::
    /// scheduler()`) — the production path, which never has a bare
    /// `Arc<Connection>` to hand [`Self::new`] (the SDK's own `Module`
    /// never exposes one, by design).
    #[must_use]
    pub fn from_sdk(inner: agent24_os_sdk::SchedulerClient) -> Self {
        Self(Arc::new(inner))
    }

    /// `_a24/scheduler/upsert`, in this crate's OLD five-positional-argument
    /// shape (module doc) — internally builds the SDK's `UpsertRequest`.
    pub async fn upsert(
        &self,
        key: &str,
        spec: &ModuleSpec,
        enabled: bool,
        label: Option<&str>,
        request_id: Option<&RequestId>,
    ) -> Result<UpsertResponse, ClientError> {
        let req = UpsertRequest {
            key,
            spec,
            enabled,
            label,
        };
        self.0.upsert(&req, request_id).await
    }

    /// `_a24/scheduler/delete`.
    pub async fn delete(
        &self,
        key: &str,
        request_id: Option<&RequestId>,
    ) -> Result<DeleteResponse, ClientError> {
        self.0.delete(key, request_id).await
    }

    /// `_a24/scheduler/list`.
    pub async fn list(&self, request_id: Option<&RequestId>) -> Result<ListResponse, ClientError> {
        self.0.list(request_id).await
    }
}
