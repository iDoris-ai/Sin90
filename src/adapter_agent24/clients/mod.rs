//! T3.2.1 — typed kernel clients: business code calls `scheduler`/`memory`/
//! `approval` methods and sees `serde` structs and a closed [`error::ClientError`]
//! set, never a raw JSON-RPC method name or `transport::TransportError`.
//!
//! **T3.2.1a (this slice of the stack)**: only [`error`] — the closed error
//! set both this end's `transport::TransportError` and the kernel's
//! `error.data.kind` collapse onto (L-6's merge lives there). The typed
//! clients themselves (`SchedulerClient`/`MemoryClient`/`ApprovalClient`) and
//! the `Clients` bundle land in the next two branches of this stack
//! (`feat/t3.2.1b-scheduler-client`, then `feat/t3.2.1-kernel-clients`) — see
//! those for the rest of this module doc's original scope.

pub mod error;

pub use error::ClientError;
