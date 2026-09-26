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
//! - [`ai`] (T5.1.1, design §11): the AI module — engine ladder, call
//!   records, and the structural gate that keeps it from ever writing
//!   `sin90.db` directly. Depends on NOTHING but `core` and itself — see
//!   [`ai`]'s module doc.
//! - [`store`]: SQLite persistence over `core`; implements `ai`'s two I/O
//!   traits in `store::ai_port`.
//! - [`http`]: business routes over `store`. Knows nothing about Agent24 —
//!   only an [`http::EventSink`] trait for "someone may collect what I emit".
//! - [`adapter_agent24`]: the ONLY module that knows Agent24 exists — the
//!   `initialize` handshake, `A24_*` env vars, and the `EventSink` that
//!   forwards to `_a24/events/emit`.

// T3.2.3 review (M2): `test-hooks` gates real, security-relevant surface
// (`adapter_agent24::kernel_roundtrip`'s debug HTTP route, `store::test_hooks`'s
// raw DB backdoors) that must never ship. `debug_assertions` is off in a
// `--release` build unless a `Cargo.toml` profile override lies about it —
// cheap, load-bearing insurance against `--release --features test-hooks`
// ever producing a binary anyone could mistake for a real install.
#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!("test-hooks must not be enabled in release builds");

// T5.1.2 (2026-09-26 review M1): `remote-allowed-manifest` (test package B,
// §11.3.2/§2 #26) swaps `ai::MANIFEST_YAML`/`ai::MODEL_ACCESS` to a manifest
// that declares `model_access: remote_allowed` — the one thing the hard
// constraint says a real install must never do. Same insurance as
// `test-hooks` above, same reason: a `--release --features
// remote-allowed-manifest` binary must never be buildable, so it can never
// be mistaken for (or accidentally shipped as) a real install.
#[cfg(all(feature = "remote-allowed-manifest", not(debug_assertions)))]
compile_error!("remote-allowed-manifest must not be enabled in release builds");

pub mod adapter_agent24;
pub mod ai;
pub mod core;
pub mod http;
pub mod store;
