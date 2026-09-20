//! Sin90 — 个人助理的领域 OS，同时是「怎么写一个 Agent24 领域 OS」的参考实现。
//!
//! Four layers, dependency arrow pointing one way (design §5.2):
//!
//! ```text
//! core  <-  store  <-  http  <-  adapter_agent24
//! ```
//!
//! - [`core`]: pure domain — entities, state machines, Proposal validation.
//!   Zero Agent24 dependency, zero I/O.
//! - [`store`]: SQLite persistence over `core`.
//! - [`http`]: business routes over `store`. Knows nothing about Agent24 —
//!   only an [`http::EventSink`] trait for "someone may collect what I emit".
//! - [`adapter_agent24`]: the ONLY module that knows Agent24 exists — the
//!   `initialize` handshake, `A24_*` env vars, and the `EventSink` that
//!   forwards to `_a24/events/emit`.

pub mod adapter_agent24;
pub mod core;
pub mod http;
pub mod store;
