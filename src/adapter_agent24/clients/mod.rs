//! T3.2.1 — typed kernel clients: business code calls `scheduler`/`memory`/
//! `approval` methods and sees `serde` structs and a closed [`error::ClientError`]
//! set, never a raw JSON-RPC method name.
//!
//! ME4-5.2.1 migration (`docs/design/ME4-S3-os-sdk.md` §5.1): each client is
//! now built from an `Arc<agent24_os_proto::module::Connection>` (the SDK's
//! own connection handle) instead of this crate's own, now-deleted
//! `KernelClients` — the "no reconnect, no `Offer` change after
//! construction" contract these constructors document is unchanged, only
//! the type that holds the connection moved into the SDK. Per architecture.
//! md 不可破边界 #7 ("只声明真正用到的能力；代码按「句柄可能不在」写"), a
//! client's constructor returns `None` — not a client that always fails —
//! when `Offer.provides` does not cover that client's own prefix; there is
//! no fallible "call it anyway" path to accidentally reach for.
//!
//! `set_optional` — the pre-migration "omit, don't null" helper every client
//! here used to build its own wire `Value` with — is gone: every client's
//! wire construction now lives inside `agent24-os-sdk` itself (§2.2's own
//! structural judgement, J-S1: "the SDK... parses no text at all" / builds
//! the frames this crate used to build by hand), so nothing in this file
//! constructs a JSON-RPC `params` object anymore.

pub mod approval;
pub mod error;
pub mod memory;
pub mod model;
pub mod scheduler;
// `pub(crate)`, not private: T3.3.2's reconciler (`adapter_agent24::reconciler`,
// a SIBLING of this module, not a descendant) needs the same fake-kernel
// plumbing this file's own `scheduler`/`memory`/`approval` test suites use —
// promoting visibility here avoids a second hand-rolled copy of it (the kind
// of duplication this file's own doc comment already accepts once, for
// `adapter_agent24::mod`'s pre-existing `clients_over_a_socket_pair`/
// `noop_hook`; a third copy is not worth it). Still `#[cfg(test)]`-gated, so
// nothing here ships in a release build.
#[cfg(test)]
pub(crate) mod test_support;

pub use approval::ApprovalClient;
pub use error::ClientError;
pub use memory::MemoryClient;
pub use model::ModelClient;
pub use scheduler::SchedulerClient;

/// One bundle of typed clients, built once against one handshake's `Offer`.
/// Each field is `None` exactly when that capability's prefix was not
/// granted — see the module docs.
pub struct Clients {
    pub scheduler: Option<SchedulerClient>,
    pub memory: Option<MemoryClient>,
    pub approval: Option<ApprovalClient>,
}

impl Clients {
    /// Builds all three from one `agent24_os_sdk::Module` — cheap (each of
    /// `Module::scheduler()`/`memory()`/`approval()` is just an
    /// `Offer.provides` prefix check plus an `Arc` clone), safe to call more
    /// than once if a caller ever needs to.
    #[must_use]
    pub fn build(module: &agent24_os_sdk::Module) -> Self {
        Self {
            scheduler: module.scheduler().map(SchedulerClient::from_sdk),
            memory: module.memory().map(MemoryClient::from_sdk),
            approval: module.approval(),
        }
    }
}
