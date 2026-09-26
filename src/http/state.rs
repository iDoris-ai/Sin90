//! Handler state + the seam that decouples `http` from Agent24 (design §5.2).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::http::HeaderMap;
use axum::response::Response;

use crate::ai::{ModelFailure, ModelPort, ModelReply, ModelRequest};
use crate::http::actor::{forbidden, Actor, ActorKeys};
use crate::http::ai_runs::SharedRunRegistry;
use crate::store::Sin90Store;

/// Where emitted events go. `adapter_agent24` implements this by forwarding to
/// `_a24/events/emit`; a `standalone`/test harness can implement it with one
/// that just records what it saw, or drops everything. Sin90's `http` layer
/// only ever sees this trait — never Agent24's actual wire format.
pub trait EventSink: Send + Sync {
    fn emit(&self, kind: &str, payload: serde_json::Map<String, serde_json::Value>);
}

/// An `EventSink` that drops every event. Used by `standalone` mode when no
/// collector is wired — the same "kernel didn't grant events → degrade, don't
/// fail" posture the ported handlers already assume (design §5.3: `events()`
/// returning `None` is fine, not an error).
pub struct NullEventSink;
impl EventSink for NullEventSink {
    fn emit(&self, _kind: &str, _payload: serde_json::Map<String, serde_json::Value>) {}
}

/// T5.1.2 — the SAME "`http` knows nothing about Agent24" seam [`EventSink`]
/// already gives `_a24/events/emit`, applied to `_a24/model/complete`.
/// `adapter_agent24::clients::model::ModelClient` implements this (mirroring
/// `KernelEventSink: EventSink`); [`Sin90State::model`] holds
/// `Option<Arc<dyn ModelCaller>>`, never the concrete adapter type.
///
/// `dyn`-safe on purpose, unlike [`crate::ai::ModelPort`] itself (whose
/// `complete` returns a bare `impl Future`, which return-position `impl
/// Trait` cannot make into a trait object) — a boxed future crosses that gap.
/// [`HttpModelPort`] is the other half: it wraps `Arc<dyn ModelCaller>` back
/// into something that DOES implement [`ModelPort`], so
/// `ai::classify::run_classify`/`summarize::run_summarize`/
/// `propose::run_propose`'s generic `M: ModelPort` parameter has a concrete,
/// `Sized` type to bind to without `http` ever naming `ModelClient`.
pub trait ModelCaller: Send + Sync {
    fn complete<'a>(
        &'a self,
        req: ModelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ModelReply, ModelFailure>> + Send + 'a>>;
}

/// See [`ModelCaller`]'s own doc — the `Sized`, `ModelPort`-implementing
/// wrapper every AI trigger route actually passes as its generic `M`
/// parameter.
#[derive(Clone)]
pub struct HttpModelPort(pub Arc<dyn ModelCaller>);

impl ModelPort for HttpModelPort {
    async fn complete(&self, req: ModelRequest) -> Result<ModelReply, ModelFailure> {
        self.0.complete(req).await
    }
}

/// M3 (2026-09-26 review): the kernel's own per-module in-flight cap for
/// `_a24/model/complete` — ME4-S2 §5.2's `MODEL_MAX_IN_FLIGHT_PER_MODULE`.
/// Sin90 has no admission control of its own on this side, and it has
/// THREE independent AI capabilities (classify/summarize/propose) that can
/// each have a background run mid-model-call at once — three concurrent
/// HTTP triggers reach three concurrent `_a24/model/complete` calls, and a
/// third one would get bounced with the kernel's own `busy` purely from
/// Sin90's own fan-out, not genuine contention from some OTHER module.
/// [`SemaphoredModelCaller`] enforces the same cap in-process instead.
pub const MODEL_MAX_IN_FLIGHT_PER_MODULE: usize = 2;

/// Wraps any [`ModelCaller`] with a process-local `Semaphore(
/// MODEL_MAX_IN_FLIGHT_PER_MODULE)` — a third concurrent caller queues (in
/// this process, before ever reaching the kernel) instead of spending a
/// call just to be told `busy`. Built ONCE, at wiring time
/// (`adapter_agent24::wire_kernel_clients`), and shared across all three AI
/// capabilities via the same `Arc<dyn ModelCaller>` that
/// [`Sin90State::model`] already threads through every trigger route — the
/// semaphore only does its job if every caller acquires from the SAME
/// instance, not one recreated per request.
pub struct SemaphoredModelCaller {
    inner: Arc<dyn ModelCaller>,
    semaphore: tokio::sync::Semaphore,
}

impl SemaphoredModelCaller {
    #[must_use]
    pub fn new(inner: Arc<dyn ModelCaller>, max_in_flight: usize) -> Self {
        Self {
            inner,
            semaphore: tokio::sync::Semaphore::new(max_in_flight),
        }
    }
}

impl ModelCaller for SemaphoredModelCaller {
    fn complete<'a>(
        &'a self,
        req: ModelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ModelReply, ModelFailure>> + Send + 'a>> {
        Box::pin(async move {
            // The semaphore is never closed (nothing ever calls
            // `close()`), so `acquire()` never returns `Err`.
            let _permit = self
                .semaphore
                .acquire()
                .await
                .expect("SemaphoredModelCaller's semaphore is never closed");
            // L-2 (2026-09-26 review round 2): `_permit` is released the
            // INSTANT this whole future is dropped — e.g. by
            // `ai_runs::with_hard_deadline`'s outer `tokio::time::timeout`
            // cancelling a run mid-call — which is immediate and LOCAL to
            // this process. The kernel's own cancellation of the underlying
            // `_a24/model/complete` call (via the `$/cancelRequest` the
            // dropped future's `CallGuard` sends, `adapter_agent24::
            // transport`) is only BEST-EFFORT — the kernel may still be
            // winding that call down for a brief moment after this permit
            // is already back in the pool. In that window Sin90 believes it
            // has a free slot and may admit a new call while the kernel, by
            // its own count, still has the old one in flight; a third
            // concurrent call landing in exactly that gap gets the kernel's
            // own `busy` instead of being admitted. Accepted cost, not a
            // bug: the alternative (holding the permit until the kernel
            // ITSELF confirms the cancellation landed) would mean a single
            // hung call can block this process's own admission indefinitely
            // — the one thing this semaphore exists to prevent.
            self.inner.complete(req).await
        })
    }
}

#[derive(Clone)]
pub struct Sin90State {
    pub store: Sin90Store,
    pub sink: Arc<dyn EventSink>,
    pub actor_keys: Arc<ActorKeys>,
    /// New (T5.2.1, design §11.4 公共): the in-process, in-memory observation
    /// window `GET /ai/runs/{run_id}` reads and `POST /ai/classify` (today
    /// the only trigger route) writes. Not a constructor parameter — nothing
    /// outside this module needs to configure it, and every `Sin90State`
    /// starts with an empty registry.
    pub ai_runs: SharedRunRegistry,
    /// New (T5.1.2, §11.3.2): `None` in `standalone` mode and whenever the
    /// kernel did not grant `_a24/model/` at handshake; `Some` only when
    /// `main.rs::run_as_agent24_module` wired a real
    /// `adapter_agent24::clients::model::ModelClient` in via
    /// [`crate::adapter_agent24::wire_kernel_clients`]. Not a constructor
    /// parameter, same posture as [`Self::ai_runs`] — every `Sin90State`
    /// starts with `None`; `main.rs` assigns this field directly (it is
    /// `pub`) once the handshake tells it whether a real client exists.
    pub model: Option<Arc<dyn ModelCaller>>,
    /// M-2 (2026-09-26 review round 2): the hard wall-clock ceiling every AI
    /// trigger route passes to [`super::ai_runs::with_hard_deadline`] —
    /// defaults to [`super::ai_runs::RUN_HARD_DEADLINE`], but a `pub` field
    /// (same posture as [`Self::model`]/[`Self::ai_runs`]) so a test can
    /// inject a millisecond-scale value instead of waiting out the real
    /// multi-minute ceiling to exercise the `Err(_elapsed)` branch (`GET
    /// /ai/runs/{id}` → `aborted`, single-flight slot released, a second
    /// trigger right after gets `202`).
    pub run_hard_deadline: std::time::Duration,
}

impl Sin90State {
    pub fn new(store: Sin90Store, sink: Arc<dyn EventSink>, actor_keys: ActorKeys) -> Self {
        Self {
            store,
            sink,
            actor_keys: Arc::new(actor_keys),
            ai_runs: SharedRunRegistry::default(),
            model: None,
            run_hard_deadline: super::ai_runs::RUN_HARD_DEADLINE,
        }
    }

    /// Emit `sin90.<kind>`. A non-object payload would be a bug in THIS crate
    /// (every call site passes `json!({...})`) — fails loud in debug, drops
    /// the event in release, same posture the ported kernel handlers used.
    pub fn emit(&self, kind: &str, payload: serde_json::Value) {
        let serde_json::Value::Object(map) = payload else {
            debug_assert!(false, "sin90 event payload must be an object");
            return;
        };
        self.sink.emit(kind, map);
    }

    /// Design §7.1: direct-write routes require the HUMAN actor key. The
    /// automation key identifies successfully but is still refused here — it
    /// is only good for the Proposal routes ([`Self::require_any_actor`]).
    // Err is the shared v1-envelope Response; the lint only fires because
    // Response itself is large, not because boxing it here is worthwhile.
    #[allow(clippy::result_large_err)]
    pub fn require_human(&self, headers: &HeaderMap) -> Result<(), Response> {
        match self.actor_keys.identify(headers) {
            Some(Actor::Human) => Ok(()),
            Some(Actor::Automation) => Err(forbidden(
                "the automation key may only submit/accept Proposals, not write directly",
            )),
            None => Err(forbidden("missing or unrecognized actor key")),
        }
    }

    /// The Proposal routes accept either key: a human client is free to build
    /// its own proposals too (design §7.1 does not forbid that, it only
    /// forbids AI from skipping the gate).
    #[allow(clippy::result_large_err)]
    pub fn require_any_actor(&self, headers: &HeaderMap) -> Result<(), Response> {
        match self.actor_keys.identify(headers) {
            Some(_) => Ok(()),
            None => Err(forbidden("missing or unrecognized actor key")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{Complexity, ServedTier};
    use serde_json::Map;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn sample_request() -> ModelRequest {
        ModelRequest {
            messages: vec![],
            schema_name: "t",
            schema: Map::new(),
            max_tokens: 16,
            complexity: Complexity::Simple,
        }
    }

    /// A `ModelCaller` that counts how many calls are concurrently past this
    /// point (i.e. past whatever admission control wraps it), records the
    /// high-water mark, and blocks on `release` (a semaphore the test drives
    /// by hand) until told to finish — lets the test observe "how many are
    /// in flight right now" independent of real wall-clock timing.
    struct CountingCaller {
        in_flight: Arc<AtomicUsize>,
        max_seen: Arc<AtomicUsize>,
        release: Arc<tokio::sync::Semaphore>,
    }
    impl ModelCaller for CountingCaller {
        fn complete<'a>(
            &'a self,
            _req: ModelRequest,
        ) -> Pin<Box<dyn Future<Output = Result<ModelReply, ModelFailure>> + Send + 'a>> {
            Box::pin(async move {
                let n = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_seen.fetch_max(n, Ordering::SeqCst);
                let _ = self.release.acquire().await.unwrap();
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                Ok(ModelReply {
                    text: String::new(),
                    model_id: None,
                    tier: ServedTier::Local,
                    prompt_tokens: None,
                    completion_tokens: None,
                })
            })
        }
    }

    /// M3 (2026-09-26 review): three concurrent callers against a
    /// `SemaphoredModelCaller::new(_, 2)` — only two are ever admitted past
    /// it into the inner caller at once (the third blocks at the SEMAPHORE,
    /// never even reaching `inner.complete`); all three eventually complete
    /// once released, proving this is a queue, not a rejection. Mirrors
    /// mounted Sin90's real shape: classify/summarize/propose share exactly
    /// ONE `SemaphoredModelCaller` instance (built once at wiring time), the
    /// same way this test shares one `Arc<dyn ModelCaller>` across three
    /// spawned tasks.
    #[tokio::test]
    async fn semaphored_model_caller_caps_at_two_concurrent_then_lets_the_third_through() {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let inner: Arc<dyn ModelCaller> = Arc::new(CountingCaller {
            in_flight: in_flight.clone(),
            max_seen: max_seen.clone(),
            release: release.clone(),
        });
        let capped: Arc<dyn ModelCaller> = Arc::new(SemaphoredModelCaller::new(inner, 2));

        let handles: Vec<_> = (0..3)
            .map(|_| {
                let capped = capped.clone();
                tokio::spawn(async move { capped.complete(sample_request()).await })
            })
            .collect();

        // Bounded poll (not a fixed sleep, N-M... same posture as the rest
        // of this crate's tests): wait until exactly two callers are
        // admitted past the semaphore and mid-flight in the inner caller.
        for _ in 0..1000 {
            if in_flight.load(Ordering::SeqCst) == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            in_flight.load(Ordering::SeqCst),
            2,
            "exactly two callers should be admitted past the cap of 2"
        );

        // Let everyone finish — the third only gets its turn once one of
        // the first two releases its semaphore permit by returning.
        release.add_permits(3);
        for h in handles {
            h.await.unwrap().unwrap();
        }
        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            2,
            "the in-flight count must never have exceeded the cap, even briefly"
        );
        assert_eq!(in_flight.load(Ordering::SeqCst), 0);
    }

    /// Positive control for the test above: with NO cap (`max_in_flight`
    /// large enough for all three), all three are admitted at once — proves
    /// `CountingCaller`'s own counting is real (it can actually observe 3
    /// concurrent callers), so the capped test's `max_seen == 2` is a real
    /// constraint the semaphore enforced, not an artifact of the fixture
    /// never being able to reach 3 in the first place.
    #[tokio::test]
    async fn semaphored_model_caller_with_a_high_cap_admits_all_three_at_once() {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let inner: Arc<dyn ModelCaller> = Arc::new(CountingCaller {
            in_flight: in_flight.clone(),
            max_seen: max_seen.clone(),
            release: release.clone(),
        });
        let capped: Arc<dyn ModelCaller> = Arc::new(SemaphoredModelCaller::new(inner, 3));

        let handles: Vec<_> = (0..3)
            .map(|_| {
                let capped = capped.clone();
                tokio::spawn(async move { capped.complete(sample_request()).await })
            })
            .collect();

        for _ in 0..1000 {
            if in_flight.load(Ordering::SeqCst) == 3 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(in_flight.load(Ordering::SeqCst), 3);

        release.add_permits(3);
        for h in handles {
            h.await.unwrap().unwrap();
        }
        assert_eq!(max_seen.load(Ordering::SeqCst), 3);
    }
}
