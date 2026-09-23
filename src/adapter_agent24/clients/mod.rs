//! T3.2.1 — typed kernel clients: business code calls `scheduler`/`memory`/
//! `approval` methods and sees `serde` structs and a closed [`error::ClientError`]
//! set, never a raw JSON-RPC method name or `transport::TransportError`.
//!
//! Each client is built from an `Arc<KernelClients>` and the `Offer` fixed at
//! handshake time (`adapter_agent24` module docs: no reconnect, no `Offer`
//! change after construction). Per architecture.md 不可破边界 #7 ("只声明真
//! 正用到的能力；代码按「句柄可能不在」写"), a client's constructor returns
//! `None` — not a client that always fails — when `Offer.provides` does not
//! cover that client's own prefix; there is no fallible "call it anyway"
//! path to accidentally reach for.
//!
//! T3.2.1 itself adds no business caller (outbox reconciliation is T3.3.2,
//! `fired` routing is T3.2.2, the real-mount acceptance test is T3.2.3) —
//! this module is the typed surface those tasks build on.

pub mod approval;
pub mod error;
pub mod memory;
pub mod scheduler;
#[cfg(test)]
mod test_support;

pub use approval::ApprovalClient;
pub use error::ClientError;
pub use memory::MemoryClient;
pub use scheduler::SchedulerClient;

use std::sync::Arc;

use serde_json::Value;

use crate::adapter_agent24::KernelClients;

/// One bundle of typed clients, built once against one handshake's `Offer`.
/// Each field is `None` exactly when that capability's prefix was not
/// granted — see the module docs.
pub struct Clients {
    pub scheduler: Option<SchedulerClient>,
    pub memory: Option<MemoryClient>,
    pub approval: Option<ApprovalClient>,
}

impl Clients {
    /// Builds all three from one `Arc<KernelClients>` — cheap (each
    /// constructor is just an `Offer.provides` prefix check plus an `Arc`
    /// clone), safe to call more than once if a caller ever needs to.
    #[must_use]
    pub fn build(clients: &Arc<KernelClients>) -> Self {
        Self {
            scheduler: SchedulerClient::new(clients),
            memory: MemoryClient::new(clients),
            approval: ApprovalClient::new(clients),
        }
    }
}

/// Sets `params[key] = value.into()` only when `value` is `Some` — every
/// optional wire field across the three clients omits itself from the
/// request entirely rather than sending `null`, matching each method's own
/// "absent means default" semantics (design §6.1 for scheduler; the same
/// convention carried over to memory/approval for consistency). Shared here
/// so the "omit, don't null" rule is written down in exactly one place.
pub(crate) fn set_optional(params: &mut Value, key: &str, value: Option<impl Into<Value>>) {
    if let Some(v) = value {
        params[key] = v.into();
    }
}
