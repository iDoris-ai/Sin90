//! T3.2.1 — typed kernel clients: business code calls `scheduler`/`memory`/
//! `approval` methods and sees `serde` structs and a closed [`error::ClientError`]
//! set, never a raw JSON-RPC method name or `transport::TransportError`.
//!
//! **T3.2.1b (this slice of the stack)**: [`error`] (T3.2.1a, already landed)
//! plus [`scheduler`] — the first typed client. `memory`/`approval` and the
//! `Clients` bundle that holds all three land in the final branch of this
//! stack (`feat/t3.2.1-kernel-clients`).
//!
//! Each client is built from an `Arc<KernelClients>` and the `Offer` fixed at
//! handshake time (`adapter_agent24` module docs: no reconnect, no `Offer`
//! change after construction). Per architecture.md 不可破边界 #7 ("只声明真
//! 正用到的能力；代码按「句柄可能不在」写"), a client's constructor returns
//! `None` — not a client that always fails — when `Offer.provides` does not
//! cover that client's own prefix; there is no fallible "call it anyway"
//! path to accidentally reach for.

pub mod error;
pub mod scheduler;
#[cfg(test)]
mod test_support;

pub use error::ClientError;
pub use scheduler::SchedulerClient;

use serde_json::Value;

/// Sets `params[key] = value.into()` only when `value` is `Some` — every
/// optional wire field across the typed clients omits itself from the
/// request entirely rather than sending `null`, matching each method's own
/// "absent means default" semantics (design §6.1 for scheduler). Shared here
/// so the "omit, don't null" rule is written down in exactly one place.
pub(crate) fn set_optional(params: &mut Value, key: &str, value: Option<impl Into<Value>>) {
    if let Some(v) = value {
        params[key] = v.into();
    }
}
