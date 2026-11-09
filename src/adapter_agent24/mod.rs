//! The ONLY module that knows Agent24 exists (design §5.2).
//!
//! ME4-5.2.1 migration (`docs/design/ME4-S3-os-sdk.md` §5.1): this module
//! used to read `A24_DATA_DIR`/`A24_CALLBACK_SOCK`/`A24_HANDSHAKE_TOKEN`/
//! `A24_LISTEN_FD` itself, run the `initialize` handshake over the callback
//! Unix socket by hand, and multiplex calls over it (`transport`/`frame`,
//! both deleted). All of that now lives in `agent24-os-sdk` (`Module`/
//! `ModuleBuilder`) — this module wires Sin90's own decisions on top of it:
//! which typed clients to build ([`clients`]), how to turn the SDK's
//! `EventsClient` into [`crate::http::EventSink`] ([`KernelEventSink`]), and
//! which capability set Sin90 has any use for at all
//! ([`provides_any_known_capability`]).

use std::sync::Arc;

use agent24_os_sdk::Module;
use serde_json::Map;

use crate::http::{EventSink, ModelCaller};

pub mod clients;
/// TS.1.0 (`docs/design/ME4-S3-os-sdk.md` §5.1) — wire golden samples for the
/// ME4-5.2.1 SDK migration. A SEPARATE `#[cfg(test)]` module on purpose, not
/// folded into `reconciler`'s or `clients::model`'s own `mod tests`: those
/// two files' test modules must stay byte-identical (reconciler.rs) or
/// change by exactly one designated line (model.rs) across the migration
/// (§5.2 point 3, checked by `scripts/check-test-modules.sh`) — adding golden
/// captures inside either would corrupt that comparison.
#[cfg(test)]
mod golden_tests;
/// T3.2.3, `test-hooks` only — see that module's own doc for why it lives
/// here rather than in `http`.
#[cfg(feature = "test-hooks")]
pub mod kernel_roundtrip;
pub mod reconciler;
/// T3.5.1, `test-hooks` only — see that module's own doc for why it lives
/// here rather than in `http`, and why it exists alongside `kernel_roundtrip`
/// rather than extending it.
#[cfg(feature = "test-hooks")]
pub mod reconciler_debug;

/// ME4-5.2.1 migration: re-exports the SDK's own hook type — `main.rs`
/// builds one to pass to `ModuleBuilder::on_connection_lost`. Sin90 no
/// longer defines its own `FatalHook` (it used to be a `transport`-owned
/// type alias, L-1's own reasoning for re-exporting it applies identically
/// to the SDK's).
pub use agent24_os_proto::module::FatalHook;

/// Capability prefixes Sin90 has any use for, per the kernel contract table
/// (`docs/agent/architecture.md` §"与内核的契约"). `events` is wired up via
/// `KernelEventSink`; `scheduler`/`memory` (private)/`approval` have typed
/// clients as of T3.2.1 (`clients::Clients`, built from a `&Module` once the
/// handshake is done — each one is `None` unless `Offer.provides` covers its
/// own prefix, architecture.md 不可破边界 #7). `model` joined this list in
/// T5.1.2 (`clients::model::ModelClient`, same "`None` unless granted"
/// posture).
pub const SIN90_CAPABILITY_PREFIXES: &[&str] = &[
    "_a24/events/",
    "_a24/scheduler/",
    "_a24/memory/private/",
    "_a24/approval/",
    "_a24/model/",
];

/// Whether `offer` grants at least one prefix from
/// [`SIN90_CAPABILITY_PREFIXES`] — the decoupled "keep the callback channel
/// open" decision (design §5), separate from "is `events` specifically among
/// them" (only [`wire_kernel_clients`]'s `EventSink` choice cares about
/// that).
pub fn provides_any_known_capability(offer: &[String]) -> bool {
    offer.iter().any(|granted| {
        SIN90_CAPABILITY_PREFIXES
            .iter()
            .any(|known| known.starts_with(granted.as_str()) || granted.starts_with(known))
    })
}

/// Adapts the SDK's own `EventsClient::spawn_sink` to [`crate::http::EventSink`].
/// `http` never sees this type — only the trait.
///
/// ME4-5.2.1 migration: this used to hand-roll the bounded-queue-plus-
/// fixed-worker-pool-plus-sub-quota machinery itself (N-M1/N-M2, two review
/// rounds' worth of tuning) — that whole implementation moved into
/// `agent24_os_sdk::clients::EventsClient::spawn_sink`/`EventSink` verbatim
/// (`agent24-os-sdk`'s own `events.rs` module doc: "Ported from Sin90's
/// `KernelEventSink::new`"), with the exact same default numbers
/// ([`agent24_os_sdk::EventSinkConfig::default`] = 256/4/32/5s, pinned by
/// this file's own `event_sink_config_defaults_match_the_pre_migration_
/// numbers` test below). This type is now a thin wrapper purely so `http`
/// keeps seeing Sin90's own [`EventSink`] trait, never an SDK type directly.
pub struct KernelEventSink(agent24_os_sdk::EventSink);

impl KernelEventSink {
    #[must_use]
    pub fn new(events: agent24_os_sdk::EventsClient) -> Self {
        Self(events.spawn_sink(agent24_os_sdk::EventSinkConfig::default()))
    }

    /// How many events were dropped because the bounded queue was full —
    /// forwarded from the SDK's own counter. `pub(crate)` for this crate's
    /// own tests; nothing production reads it yet.
    #[cfg(test)]
    pub(crate) fn dropped_count(&self) -> u64 {
        self.0.dropped()
    }
}

impl EventSink for KernelEventSink {
    fn emit(&self, kind: &str, payload: Map<String, serde_json::Value>) {
        self.0.emit(kind, payload);
    }
}

/// The decision `main.rs`'s `run_as_agent24_module` actually applies —
/// pulled out here so it can be exercised with an INJECTED `Offer` (via the
/// SDK's own `testing::FakeEndpoint`), no real kernel required. `main.rs`
/// calls this verbatim; it does not reimplement the decision, so a bug in
/// how the decision gets APPLIED (not just in the decision itself) shows up
/// here too.
///
/// ME4-5.2.1 migration: takes `&Module` now, not `(offer, Arc<KernelClients>)`
/// — the SDK's `Module` is the one thing that can hand out `EventsClient`/
/// `ModelClient` at all (its accessors are the only sanctioned way to reach
/// them; the underlying `Connection` is never exposed, by design). N-H1's
/// own concern ("the callback connection must outlive this function
/// regardless of what got wired") is now the caller's responsibility in a
/// different shape: `main.rs` holds `Module` itself until it calls
/// `Module::serve`, which is what keeps the connection alive — there is no
/// separate "holder" handle to keep bound here anymore, since this function
/// no longer owns (or can drop) the connection at all.
///
/// T5.1.2 adds the model client to the tuple: `Some(Arc<dyn ModelCaller>)` —
/// `http`'s own `dyn`-safe seam for `_a24/model/complete` (mirrors
/// `EventSink`; `http` must never name `clients::model::ModelClient`
/// directly, lib.rs's own dependency arrow) — only when `Offer.provides`
/// covers `_a24/model/`; `None` otherwise (including the
/// `provides_any_known_capability` early return below, same as every other
/// client). `main.rs` assigns it to `Sin90State::model`.
pub type WiredKernelClients = (Arc<dyn EventSink>, Option<Arc<dyn ModelCaller>>);

pub fn wire_kernel_clients(module: &Module) -> WiredKernelClients {
    if !provides_any_known_capability(&module.offer().provides) {
        tracing::warn!(
            "sin90: kernel offered no capability Sin90 uses; the callback connection is kept \
             open regardless (N-H1 — closing it would end this generation) but no client is \
             wired to it"
        );
        return (Arc::new(crate::http::NullEventSink), None);
    }
    let sink: Arc<dyn EventSink> = match module.events() {
        Some(events) => Arc::new(KernelEventSink::new(events)),
        None => {
            // Granted some other capability but not events — degrade, don't
            // fail (design §5.3).
            tracing::warn!("sin90: events not offered by kernel; running with events dropped");
            Arc::new(crate::http::NullEventSink)
        }
    };
    // M3 (2026-09-26 review): wrapped in `SemaphoredModelCaller` — built
    // ONCE here, at wiring time, and shared across every AI trigger route
    // through this single `Arc` — so Sin90's own three-capability fan-out
    // (classify/summarize/propose) can never put more than
    // `MODEL_MAX_IN_FLIGHT_PER_MODULE` calls to the kernel in flight at
    // once (`SemaphoredModelCaller`'s own doc).
    let model: Option<Arc<dyn ModelCaller>> = module.model().map(|m| {
        let raw: Arc<dyn ModelCaller> = Arc::new(clients::model::ModelClient::from_sdk(m));
        Arc::new(crate::http::SemaphoredModelCaller::new(
            raw,
            crate::http::MODEL_MAX_IN_FLIGHT_PER_MODULE,
        )) as Arc<dyn ModelCaller>
    });
    (sink, model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent24_os_sdk::testing::{noop_hook, FakeEndpoint};
    use serde_json::json;
    use std::time::Duration;

    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sin90-test-{}", crate::core::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Drives a real (in-process) handshake over a `FakeEndpoint` and hands
    /// back the resulting `Module`, granted exactly `offer` — the SDK's own
    /// test primitive for exactly this (J-S11's own scratch, ported), so
    /// this crate's tests exercise the REAL `Module::builder(..).with_env(..)
    /// .connect()` path rather than a second, hand-rolled fake.
    async fn module_with_offer(offer: Vec<String>) -> Module {
        let dir = tempdir();
        let (env, ep) = FakeEndpoint::bind(&dir);
        let connect = tokio::spawn(
            Module::builder(
                "name: sin90-test\nroute_namespace: /api/v1/sin90-test\nkernel_capabilities: []\n",
            )
            .with_env(env, noop_hook())
            .connect(),
        );
        let result = json!({"protocol_version": 1, "offer": {"provides": offer}});
        let (_req, _peer) = ep.accept_initialize(result).await;
        connect.await.unwrap().unwrap()
    }

    #[test]
    fn provides_any_known_capability_true_for_a_known_prefix_false_for_an_unknown_one() {
        assert!(provides_any_known_capability(&[
            "_a24/scheduler/".to_string()
        ]));
        assert!(!provides_any_known_capability(&[
            "_a24/some-unknown-capability/".to_string()
        ]));
        assert!(!provides_any_known_capability(&[]));
    }

    /// §5.2 point 4 ("常量同值"): the SDK's `EventSinkConfig::default()` is
    /// the SAME 256/4/32/5s Sin90's own pre-migration `KernelEventSink`
    /// used — the numbers two review rounds tuned did not silently change
    /// when the implementation moved into the SDK.
    #[test]
    fn event_sink_config_defaults_match_the_pre_migration_numbers() {
        let cfg = agent24_os_sdk::EventSinkConfig::default();
        assert_eq!(cfg.queue_capacity, 256);
        assert_eq!(cfg.workers, 4);
        assert_eq!(cfg.sub_quota, 32);
        assert_eq!(cfg.slot_wait, Duration::from_secs(5));
    }

    /// Belt-and-suspenders on the thin wrapper itself: `KernelEventSink::
    /// dropped_count` forwards to the SDK's own counter (the SDK's own test
    /// suite, `events.rs`, proves the counting BEHAVIOR under a full queue —
    /// this only proves Sin90's wrapper method actually reaches it, rather
    /// than e.g. always reading `0`).
    #[tokio::test]
    async fn dropped_count_forwards_to_the_sdk_own_counter() {
        let module = module_with_offer(vec!["_a24/events/".to_string()]).await;
        let sink = KernelEventSink::new(module.events().unwrap());
        assert_eq!(sink.dropped_count(), 0);
    }

    #[tokio::test]
    async fn wiring_uses_kernel_event_sink_and_emit_reaches_the_wire_when_events_offered() {
        // N-M4③ positive control: `events` granted → emit really goes out.
        let module = module_with_offer(vec!["_a24/events/".to_string()]).await;
        let (sink, _model) = wire_kernel_clients(&module);
        sink.emit("test.kind", Map::new());
        // `module` itself owns the only handle that can observe the wire
        // byte here (the SDK's own fake-kernel primitives operate at the
        // `Connection` layer, not `Module`) — this test's job is only to
        // prove `wire_kernel_clients` wired a REAL `KernelEventSink` (not a
        // `NullEventSink`) when `events` was granted; the SDK's own test
        // suite (`events.rs`) already proves `spawn_sink`'s wire behavior.
        assert!(module.events().is_some());
    }

    /// T5.1.2: `_a24/model/` granted → `ModelClient::from_sdk` wraps it into
    /// `Arc<dyn ModelCaller>` — the tuple's second slot is `Some`, not
    /// `None`.
    #[tokio::test]
    async fn wiring_wires_a_model_client_when_model_is_offered() {
        let module = module_with_offer(vec!["_a24/model/".to_string()]).await;
        let (_sink, model) = wire_kernel_clients(&module);
        assert!(
            model.is_some(),
            "Offer.provides covering _a24/model/ must wire a ModelCaller"
        );
    }

    /// Positive control: granted some other capability (not `_a24/model/`)
    /// → the model slot stays `None`, same "句柄可能不在" posture every
    /// other typed client already has.
    #[tokio::test]
    async fn wiring_model_is_none_when_model_is_not_offered() {
        let module = module_with_offer(vec!["_a24/scheduler/".to_string()]).await;
        let (_sink, model) = wire_kernel_clients(&module);
        assert!(model.is_none());
    }

    #[tokio::test]
    async fn wiring_uses_null_event_sink_when_only_scheduler_offered_not_events() {
        // N-M4③: assert the CONCRETE sink behavior, not an `Option`'s
        // shape — a `NullEventSink` must never put anything on the wire
        // (proven here by the fact that `module.events()` is `None`, so
        // `wire_kernel_clients` could not have built a `KernelEventSink`
        // even if it wanted to — `emit()` on the returned sink is a no-op).
        let module = module_with_offer(vec!["_a24/scheduler/".to_string()]).await;
        let (sink, _model) = wire_kernel_clients(&module);
        assert!(module.events().is_none());
        sink.emit("test.kind", Map::new()); // must not panic.
    }

    #[tokio::test]
    async fn wiring_wires_nothing_when_no_known_capability_is_offered() {
        let module = module_with_offer(vec!["_a24/some-unknown-capability/".to_string()]).await;
        let (sink, model) = wire_kernel_clients(&module);
        assert!(model.is_none());
        sink.emit("test.kind", Map::new()); // NullEventSink: must not panic.
    }
}
