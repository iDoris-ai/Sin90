//! ME4-5.2.1 migration (`docs/design/ME4-S3-os-sdk.md` §5.1): the
//! `_a24/memory/private/{remember,recall,recent}` client moved into
//! `agent24-os-sdk` verbatim (same wire shapes, same "omit, don't null"
//! optional-field convention, same `Deserialize` field names). `remember`'s
//! own signature only changed in one respect (`request_id: Option<&str>` ->
//! `Option<&RequestId>`), and every call site in this crate passes the
//! untyped literal `None` there, so no call site needs editing for that.
//!
//! [`MemoryClient`] itself stays a thin NEWTYPE — wrapping `Arc<agent24_os_sdk::
//! MemoryClient>` rather than re-exporting the SDK's type directly — for the
//! same reason `clients::scheduler::SchedulerClient` does: the SDK's own
//! `MemoryClient` does not implement `Clone` (its `Core` field is private to
//! that crate), and `reconciler.rs`'s protected test module clones a
//! `MemoryClient` (`memory.clone()`, several call sites) to hand a fresh
//! handle to each `scheduler_and_memory(...)` call. Every method below
//! forwards straight through with the EXACT same signature the SDK exposes,
//! so no call site anywhere in this crate needed to change.
//!
//! `remember_once` (H3, §2.4/§3.5) is new here: the `recall`-pre-check
//! algorithm `reconciler.rs`'s own `memory_recall_finds_dedup_key`/
//! `remember_review_summary` used to hand-roll, ported into the SDK
//! verbatim (same `RECALL_PRECHECK_MAX_PAGES`/`RECALL_PRECHECK_PAGE_SIZE`,
//! same `body.dedup_key` marker field) — `reconciler.rs`'s own
//! `remember_review_summary` calls it directly now instead of re-deriving
//! the same algorithm.
use std::sync::Arc;

use agent24_os_proto::module::Connection;
use agent24_os_sdk::RequestId;
use serde_json::{Map, Value};

use super::error::ClientError;
pub use agent24_os_sdk::{
    RecallPage, Recollection, RememberOnce, Remembered, DEDUP_KEY_FIELD, RECALL_PRECHECK_MAX_PAGES,
    RECALL_PRECHECK_PAGE_SIZE,
};

/// The prefix `Offer.provides` must cover for [`MemoryClient::new`] to
/// return `Some`.
pub const PREFIX: &str = "_a24/memory/private/";

/// Newtype around `agent24_os_sdk::MemoryClient` — see this module's own
/// doc for why.
#[derive(Clone)]
pub struct MemoryClient(Arc<agent24_os_sdk::MemoryClient>);

impl MemoryClient {
    /// `None` unless the handshake's `Offer` granted [`PREFIX`]
    /// (architecture.md 不可破边界 #7).
    #[must_use]
    pub fn new(conn: &Arc<Connection>) -> Option<Self> {
        agent24_os_sdk::MemoryClient::new(conn)
            .map(Arc::new)
            .map(Self)
    }

    /// Wraps an already-built SDK client (`agent24_os_sdk::Module::
    /// memory()`) — the production path, which never has a bare
    /// `Arc<Connection>` to hand [`Self::new`].
    #[must_use]
    pub fn from_sdk(inner: agent24_os_sdk::MemoryClient) -> Self {
        Self(Arc::new(inner))
    }

    /// `_a24/memory/private/remember`.
    pub async fn remember(
        &self,
        kind: &str,
        body: Map<String, Value>,
        request_id: Option<&RequestId>,
    ) -> Result<Remembered, ClientError> {
        self.0.remember(kind, body, request_id).await
    }

    /// `_a24/memory/private/recall`.
    pub async fn recall(
        &self,
        query: &str,
        page_size: usize,
        cursor: Option<&str>,
        request_id: Option<&RequestId>,
    ) -> Result<RecallPage, ClientError> {
        self.0.recall(query, page_size, cursor, request_id).await
    }

    /// `_a24/memory/private/recent`.
    pub async fn recent(
        &self,
        page_size: usize,
        cursor: Option<&str>,
        request_id: Option<&RequestId>,
    ) -> Result<RecallPage, ClientError> {
        self.0.recent(page_size, cursor, request_id).await
    }

    /// `remember`, at most once per `dedup_key` (H3) — see [`RememberOnce`]'s
    /// own doc.
    pub async fn remember_once(
        &self,
        kind: &str,
        dedup_key: &str,
        body: Map<String, Value>,
        request_id: Option<&RequestId>,
    ) -> Result<RememberOnce, ClientError> {
        self.0
            .remember_once(kind, dedup_key, body, request_id)
            .await
    }
}
