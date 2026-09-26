//! §11.3.3–§11.3.5 — the engine ladder itself: which steps a capability
//! tries in which order ([`plan`]), what happens when one fails
//! ([`run_item`]'s degrade/defer/abort handling, §11.3.4), the run-local
//! circuit breaker and budget ([`RunState`]), and the two small store-facing
//! derivations that decide from a served reply what the store should record
//! ([`source_for`], [`tripwire`]).
//!
//! Ported from the frozen design's scratch crate `t501-check/src/ports.rs`
//! (§11.12) — that file mixed vocabulary and ladder; here they are split per
//! §11.10's interface list (`ports.rs` vs `ladder.rs`).

use std::collections::HashSet;
use std::time::Instant;

use crate::core::ProposalSource;

use super::ports::{
    AiCallRecord, AiSettings, AiSink, Capability, Engine, LadderAction, ModelAccess, ModelFailure,
    ModelPort, ModelReply, ModelRequest, ReadError, ServedTier, SettingsRead, UnavailableCause,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    ReflexDecisive,
    Model(Engine),
    ReflexFallback,
}

/// `classify`: `[ReflexDecisive, Model(Executive)?, Model(Local)?,
/// ReflexFallback]`. `summarize`/`propose`: `[Model(Executive)?,
/// Model(Local)?, ReflexFallback]`. `Executive` appears ⇔ a model port is
/// present ∧ `access == RemoteAllowed` ∧ `settings.executive_enabled`;
/// `Local` appears ⇔ a model port is present (§11.3.3).
#[must_use]
pub fn plan(
    cap: Capability,
    access: ModelAccess,
    settings: AiSettings,
    model_port_present: bool,
) -> Vec<Step> {
    let mut v = Vec::new();
    if cap == Capability::Classify {
        v.push(Step::ReflexDecisive);
    }
    if model_port_present {
        if access == ModelAccess::RemoteAllowed && settings.executive_enabled {
            v.push(Step::Model(Engine::Executive));
        }
        v.push(Step::Model(Engine::Local));
    }
    v.push(Step::ReflexFallback);
    v
}

/// Store-side derivation of `ProposalSource` (M3): the SERVED tier decides,
/// not the requested engine — Sin90 cannot choose a provider (§11.3.1's
/// "关键事实": "executive" only means "ask the kernel to route `complex`";
/// the kernel may still serve it locally).
#[must_use]
pub fn source_for(engine: Engine, served: Option<ServedTier>) -> ProposalSource {
    match (engine, served) {
        (Engine::Reflex, _) => ProposalSource::Rule,
        (_, Some(ServedTier::Remote)) => ProposalSource::Executive,
        (_, _) => ProposalSource::LocalBrain,
    }
}

/// L4: re-read the switch right before trusting a remote reply — a user can
/// turn `ai.executive_enabled` off mid-run and that must still take effect.
/// "Detection, not protection" (§11.3.2): the bytes already left the
/// machine by the time this trips.
#[must_use]
pub fn tripwire(served: ServedTier, settings: AiSettings) -> bool {
    served == ServedTier::Remote && !settings.executive_enabled
}

/// ⚖️ per-run limits (M1). 20 ≤ kernel bucket capacity 30; a two-rung ladder
/// spends up to 2 calls per item, so a run may cover fewer than 20 items.
pub const MAX_MODEL_CALLS_PER_RUN: u32 = 20;
pub const RUN_DEADLINE_SECS: u64 = 600;

/// Mutable per-run state shared by every item of one run.
#[derive(Debug)]
pub struct RunState {
    pub calls_left: u32,
    pub deadline: Instant,
    /// Engines whose circuit opened this run.
    pub skip: HashSet<Engine>,
    /// Set by a Defer failure: no more model steps this run.
    pub models_off: bool,
    pub aborted: bool,
}

impl RunState {
    #[must_use]
    pub fn new(now: Instant) -> Self {
        Self {
            calls_left: MAX_MODEL_CALLS_PER_RUN,
            deadline: now + std::time::Duration::from_secs(RUN_DEADLINE_SECS),
            skip: HashSet::new(),
            models_off: false,
            aborted: false,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome<T> {
    /// `rec` is the `ok=1` row; the caller passes it to `AiSink::submit`
    /// together with the proposal (atomic, M3). Not yet written.
    Produced {
        value: T,
        engine: Engine,
        rec: AiCallRecord,
    },
    /// Every step ran, none produced (model said none, R2 no match…).
    ///
    /// `deterministic` (T5.7.2 review round 3, M2): true iff at least one
    /// step along the way got a genuine model REPLY that was then rejected
    /// as `bad_output` (`parse` returned `Err`) or blocked by the privacy
    /// `tripwire` — the model (or the served tier itself) actually looked at
    /// this item and produced something, it just wasn't usable. `false` when
    /// every step that ran never got that far at all: `ReflexDecisive`
    /// undecided, `ReflexFallback` no match, no model port configured, every
    /// `Model` step degraded/deferred on the TRANSPORT (`Timeout`,
    /// `Unavailable`, `Busy`, `RateLimited`, `NotReady`, circuit already
    /// open, …) before a reply ever came back. Callers use this to decide
    /// whether "the model looked at this and found nothing new" is true
    /// enough to record (classify's H2 `sin90_classify_evals` write) — a
    /// transport hiccup must not be laundered into "classify examined this
    /// task", but a malformed/blocked reply is a real (if unusable) look.
    Nothing {
        deterministic: bool,
    },
    /// Capacity/budget/deadline: item left for a later run; no R2.
    Deferred,
    Aborted,
}

/// One item through the ladder. Every NON-producing attempt is written via
/// `sink.record_call`; the producing attempt is returned for atomic submit
/// (§11.3.5's "原子性" — the caller, not this function, calls
/// `AiSink::submit` with the returned `rec`).
#[allow(clippy::too_many_arguments)]
pub async fn run_item<T, M, S, R>(
    run_id: &str,
    cap: Capability,
    steps: &[Step],
    st: &mut RunState,
    model: Option<&M>,
    sink: &S,
    settings_src: &R,
    build_request: impl Fn(Engine) -> ModelRequest,
    parse: impl Fn(&ModelReply) -> Result<T, &'static str>,
    reflex_decisive: impl Fn() -> Option<T>,
    reflex_fallback: impl Fn() -> Option<T>,
    now: impl Fn() -> String,
    mint_id: impl Fn() -> String,
) -> Outcome<T>
where
    M: ModelPort,
    S: AiSink,
    R: SettingsRead,
{
    if st.aborted {
        return Outcome::Aborted;
    }
    let mut failed_from: Option<Engine> = None;
    // M2 (T5.7.2 review round 3): set true the moment ANY step gets a real
    // model reply that is then rejected (`bad_output`) or blocked
    // (`tripwire`) — see `Outcome::Nothing`'s own doc for why this, and only
    // this, distinguishes "the model looked and answered badly" from "the
    // model never answered at all".
    let mut deterministic = false;
    for step in steps {
        let started = Instant::now();
        let mut rec = AiCallRecord {
            id: mint_id(),
            run_id: run_id.to_string(),
            task_kind: cap,
            engine: Engine::Reflex,
            fallback_from: None,
            served_tier: None,
            model_id: None,
            prompt_tokens: None,
            completion_tokens: None,
            latency_ms: 0,
            ok: false,
            error_kind: None,
            proposal_id: None,
            at: now(),
        };
        match step {
            Step::ReflexDecisive => {
                let got = reflex_decisive();
                rec.latency_ms = started.elapsed().as_millis() as u64;
                if let Some(value) = got {
                    rec.ok = true;
                    return Outcome::Produced {
                        value,
                        engine: Engine::Reflex,
                        rec,
                    };
                }
                // L5: "no decisive rule" is not a failure; not counted as degradation.
                rec.error_kind = Some("undecided");
                record_call_best_effort(sink, rec).await;
            }
            Step::ReflexFallback => {
                rec.fallback_from = failed_from;
                let got = reflex_fallback();
                rec.latency_ms = started.elapsed().as_millis() as u64;
                if let Some(value) = got {
                    rec.ok = true;
                    return Outcome::Produced {
                        value,
                        engine: Engine::Reflex,
                        rec,
                    };
                }
                rec.error_kind = Some("no_match");
                record_call_best_effort(sink, rec).await;
            }
            Step::Model(engine) => {
                let Some(m) = model else { continue };
                if st.skip.contains(engine) {
                    failed_from = Some(*engine);
                    continue;
                }
                if st.models_off || st.calls_left == 0 || Instant::now() >= st.deadline {
                    st.models_off = true;
                    return Outcome::Deferred;
                }
                st.calls_left -= 1;
                rec.engine = *engine;
                rec.fallback_from = failed_from;
                let res = m.complete(build_request(*engine)).await;
                rec.latency_ms = started.elapsed().as_millis() as u64;
                match res {
                    Ok(reply) => {
                        rec.served_tier = Some(reply.tier);
                        rec.model_id = reply.model_id.clone();
                        rec.prompt_tokens = reply.prompt_tokens;
                        rec.completion_tokens = reply.completion_tokens;
                        if reply.tier == ServedTier::Remote {
                            // L4: re-read the switch right before trusting a remote reply.
                            let fresh = settings_src.settings().await.unwrap_or_default();
                            if tripwire(reply.tier, fresh) {
                                rec.error_kind = Some("privacy_tripwire");
                                record_call_best_effort(sink, rec).await;
                                failed_from = Some(*engine);
                                deterministic = true;
                                continue;
                            }
                        }
                        match parse(&reply) {
                            Ok(value) => {
                                rec.ok = true;
                                return Outcome::Produced {
                                    value,
                                    engine: *engine,
                                    rec,
                                };
                            }
                            Err(why) => {
                                rec.error_kind = Some(why);
                                record_call_best_effort(sink, rec).await;
                                failed_from = Some(*engine);
                                deterministic = true;
                            }
                        }
                    }
                    Err(f) => {
                        // §11.3.4's own-bug flags: `backend_config`/
                        // `forbidden` are configuration mistakes worth a
                        // `warn!`; `response_too_large`/`bad_request` are
                        // THIS end's bug (max_tokens ≤ 1024 by construction,
                        // or a malformed request) and get `error!`.
                        log_model_failure(&f);
                        rec.error_kind = Some(f.kind_str());
                        record_call_best_effort(sink, rec).await;
                        match f.action() {
                            LadderAction::Abort => {
                                st.aborted = true;
                                return Outcome::Aborted;
                            }
                            LadderAction::Defer => {
                                st.models_off = true;
                                return Outcome::Deferred;
                            }
                            LadderAction::Degrade => {
                                if f.opens_circuit() {
                                    st.skip.insert(*engine);
                                }
                                failed_from = Some(*engine);
                            }
                        }
                    }
                }
            }
        }
    }
    Outcome::Nothing { deterministic }
}

/// R6 (§11.3.5): a non-producing attempt's call record is best-effort — the
/// model didn't misbehave, the sink did, and that must not abort the run.
/// Failure is logged at `warn!` (2026-09-24 review: was silently swallowed
/// via `let _ =`), so an operator can still notice "we're losing call
/// history" without it ever affecting `run_item`'s control flow.
async fn record_call_best_effort<S: AiSink>(sink: &S, rec: AiCallRecord) {
    if let Err(e) = sink.record_call(rec).await {
        tracing::warn!(error = %e, "ai: failed to record a non-producing call attempt (not fatal, R6)");
    }
}

/// §11.3.4's own two logging rows: `backend_config`/`forbidden` are
/// configuration mistakes (someone else's setup, or a manifest/settings
/// mismatch) worth a `warn!`; `response_too_large`/`bad_request` can only
/// happen if THIS code sent a bad request (`max_tokens` is ≤ 1024 by
/// construction, so a `response_too_large` here means a real bug) — `error!`.
fn log_model_failure(f: &ModelFailure) {
    match f {
        ModelFailure::Unavailable {
            cause: UnavailableCause::BackendConfig,
            ..
        } => tracing::warn!(?f, "ai model backend misconfigured"),
        ModelFailure::Unavailable {
            cause: UnavailableCause::ResponseTooLarge,
            ..
        } => tracing::error!(?f, "ai model response too large (max_tokens bug?)"),
        ModelFailure::Forbidden => {
            tracing::warn!(
                ?f,
                "ai model call forbidden (models capability not granted?)"
            )
        }
        ModelFailure::BadRequest => tracing::error!(?f, "ai model request malformed (our bug)"),
        _ => {}
    }
}

// Referenced only through the generic bound above at monomorphization time;
// named here so this file's own `use` stays honest about what it needs.
#[allow(dead_code)]
type _ReadErrorUsed = ReadError;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::ports::{
        Complexity, ModelFailure, ModelMessage, Role, SinkError, UnavailableCause,
    };
    use crate::ai::ProposalDraft;
    use serde_json::Map;
    use std::future::Future;
    use std::sync::{Arc, Mutex};

    struct StubModel(Result<ModelReply, ModelFailure>, Arc<Mutex<u32>>);
    impl StubModel {
        fn new(r: Result<ModelReply, ModelFailure>) -> Self {
            Self(r, Arc::default())
        }
        fn calls(&self) -> u32 {
            *self.1.lock().unwrap()
        }
    }
    impl ModelPort for StubModel {
        fn complete(
            &self,
            _req: ModelRequest,
        ) -> impl Future<Output = Result<ModelReply, ModelFailure>> + Send {
            *self.1.lock().unwrap() += 1;
            let r = self.0.clone();
            async move { r }
        }
    }
    struct ByEngine {
        exec: Result<ModelReply, ModelFailure>,
        local: Result<ModelReply, ModelFailure>,
    }
    impl ModelPort for ByEngine {
        fn complete(
            &self,
            req: ModelRequest,
        ) -> impl Future<Output = Result<ModelReply, ModelFailure>> + Send {
            let r = if req.complexity == Complexity::Complex {
                self.exec.clone()
            } else {
                self.local.clone()
            };
            async move { r }
        }
    }
    #[derive(Default, Clone)]
    struct MemSink(Arc<Mutex<Vec<AiCallRecord>>>);
    impl AiSink for MemSink {
        fn submit(
            &self,
            _cap: Capability,
            d: ProposalDraft,
            mut rec: AiCallRecord,
        ) -> impl Future<Output = Result<(), SinkError>> + Send {
            rec.proposal_id = Some(d.id.clone());
            self.0.lock().unwrap().push(rec);
            async { Ok(()) }
        }
        fn record_call(
            &self,
            rec: AiCallRecord,
        ) -> impl Future<Output = Result<(), SinkError>> + Send {
            self.0.lock().unwrap().push(rec);
            async { Ok(()) }
        }
        async fn record_classify_eval(&self, _task_id: &str) -> Result<(), SinkError> {
            Ok(())
        }
        fn precheck(
            &self,
            _cap: Capability,
            d: &[ProposalDraft],
        ) -> impl Future<Output = Vec<bool>> + Send {
            let n = d.len();
            async move { vec![true; n] }
        }
    }
    struct Settings(bool);
    impl SettingsRead for Settings {
        fn settings(&self) -> impl Future<Output = Result<AiSettings, ReadError>> + Send {
            let v = self.0;
            async move {
                Ok(AiSettings {
                    executive_enabled: v,
                })
            }
        }
    }

    fn req(e: Engine) -> ModelRequest {
        ModelRequest {
            messages: vec![ModelMessage {
                role: Role::User,
                content: "x".into(),
            }],
            schema_name: "x",
            schema: Map::new(),
            max_tokens: 64,
            complexity: if e == Engine::Executive {
                Complexity::Complex
            } else {
                Complexity::Simple
            },
        }
    }
    fn reply(tier: ServedTier) -> ModelReply {
        ModelReply {
            text: "ok".into(),
            model_id: Some("m".into()),
            tier,
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
        }
    }
    const NO_PROVIDER: ModelFailure = ModelFailure::Unavailable {
        retryable: true,
        cause: crate::ai::ports::UnavailableCause::NoProvider,
    };

    async fn go<M: ModelPort>(
        steps: &[Step],
        st: &mut RunState,
        switch_now: bool,
        m: Option<&M>,
        sink: &MemSink,
    ) -> Outcome<&'static str> {
        run_item(
            "run1",
            Capability::Classify,
            steps,
            st,
            m,
            sink,
            &Settings(switch_now),
            req,
            |r| {
                if r.text == "ok" {
                    Ok("model")
                } else {
                    Err("bad_output")
                }
            },
            || None,
            || Some("reflex-guess"),
            || "2026-09-24T00:00:00Z".into(),
            || "id".into(),
        )
        .await
    }
    /// Emulates the caller: `Produced` → atomic submit.
    async fn go_and_submit<M: ModelPort>(
        steps: &[Step],
        st: &mut RunState,
        switch_now: bool,
        m: Option<&M>,
        sink: &MemSink,
    ) -> Outcome<&'static str> {
        let out = go(steps, st, switch_now, m, sink).await;
        if let Outcome::Produced { rec, .. } = &out {
            let d = ProposalDraft {
                id: "p1".into(),
                ops: vec![],
                rationale: None,
            };
            sink.submit(Capability::Classify, d, rec.clone())
                .await
                .unwrap();
        }
        out
    }
    fn st() -> RunState {
        RunState::new(Instant::now())
    }

    #[test]
    fn ai_ladder_executive_gate() {
        // J4: Model(Executive) appears ONLY on (RemoteAllowed, on, port present).
        for access in [ModelAccess::LocalOnly, ModelAccess::RemoteAllowed] {
            for enabled in [false, true] {
                for port in [false, true] {
                    let settings = AiSettings {
                        executive_enabled: enabled,
                    };
                    let steps = plan(Capability::Summarize, access, settings, port);
                    let has_exec = steps.contains(&Step::Model(Engine::Executive));
                    let want = access == ModelAccess::RemoteAllowed && enabled && port;
                    assert_eq!(
                        has_exec, want,
                        "access={access:?} enabled={enabled} port={port}"
                    );
                }
            }
        }
        // classify always leads with ReflexDecisive; propose without a port is
        // just the reflex fallback.
        assert_eq!(
            plan(
                Capability::Classify,
                ModelAccess::LocalOnly,
                AiSettings::default(),
                true
            ),
            vec![
                Step::ReflexDecisive,
                Step::Model(Engine::Local),
                Step::ReflexFallback
            ]
        );
        assert_eq!(
            plan(
                Capability::Propose,
                ModelAccess::LocalOnly,
                AiSettings::default(),
                false
            ),
            vec![Step::ReflexFallback]
        );
    }

    #[test]
    fn ai_ladder_failure_table() {
        // J3 (2026-09-24 review: exhaustive over `action()`, `opens_circuit()`
        // AND `kind_str()`, with all four `Unavailable` causes distinguished
        // — §11.3.4's table verbatim). `action()`'s own `match` has no
        // wildcard arm, so a forgotten new `ModelFailure` variant fails to
        // COMPILE, not just fails this test.
        use LadderAction::*;
        use UnavailableCause::*;
        let cases: [(ModelFailure, LadderAction, bool, &str); 14] = [
            (
                ModelFailure::Unavailable {
                    retryable: true,
                    cause: NoProvider,
                },
                Degrade,
                true,
                "unavailable.no_provider",
            ),
            (
                ModelFailure::Unavailable {
                    retryable: false,
                    cause: RequestRejected,
                },
                Degrade,
                false,
                "unavailable.request_rejected",
            ),
            (
                ModelFailure::Unavailable {
                    retryable: false,
                    cause: BackendConfig,
                },
                Degrade,
                true,
                "unavailable.backend_config",
            ),
            (
                ModelFailure::Unavailable {
                    retryable: false,
                    cause: ResponseTooLarge,
                },
                Degrade,
                false,
                "unavailable.response_too_large",
            ),
            (ModelFailure::Timeout, Degrade, false, "timeout"),
            (ModelFailure::Forbidden, Degrade, true, "forbidden"),
            (ModelFailure::BadRequest, Degrade, false, "bad_request"),
            (ModelFailure::Other, Degrade, false, "other"),
            (ModelFailure::Busy, Defer, false, "busy"),
            (ModelFailure::RateLimited, Defer, false, "rate_limited"),
            (ModelFailure::NotReady, Defer, false, "not_ready"),
            (
                ModelFailure::GenerationEnding,
                Abort,
                false,
                "generation_ending",
            ),
            (
                ModelFailure::ConnectionLost,
                Abort,
                false,
                "connection_lost",
            ),
            (ModelFailure::Cancelled, Abort, false, "cancelled"),
        ];
        assert_eq!(cases.len(), 14, "4 Unavailable causes + 10 other variants");
        for (f, want_action, want_circuit, want_kind) in cases {
            assert_eq!(f.action(), want_action, "{f:?} action");
            assert_eq!(f.opens_circuit(), want_circuit, "{f:?} opens_circuit");
            assert_eq!(f.kind_str(), want_kind, "{f:?} kind_str");
        }
    }

    #[tokio::test]
    async fn ai_ladder_local_unavailable_degrades_to_reflex() {
        // J1
        let sink = MemSink::default();
        let m = StubModel::new(Err(NO_PROVIDER));
        let steps = plan(
            Capability::Classify,
            ModelAccess::LocalOnly,
            AiSettings::default(),
            true,
        );
        let out = go_and_submit(&steps, &mut st(), false, Some(&m), &sink).await;
        assert!(matches!(
            out,
            Outcome::Produced {
                engine: Engine::Reflex,
                ..
            }
        ));
        {
            let rows = sink.0.lock().unwrap();
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0].error_kind, Some("undecided"));
            assert_eq!(rows[1].error_kind, Some("unavailable.no_provider"));
            assert_eq!(rows[2].fallback_from, Some(Engine::Local));
            assert!(rows[2].ok);
            assert_eq!(rows[2].proposal_id.as_deref(), Some("p1"));
            assert_eq!(
                source_for(rows[2].engine, rows[2].served_tier),
                ProposalSource::Rule
            );
        }

        // Positive control (mutation target): the SAME failure recovered ->
        // only 2 rows, `source = local_brain`, no `fallback_from`.
        let sink2 = MemSink::default();
        let m2 = StubModel::new(Ok(reply(ServedTier::Local)));
        let out2 = go_and_submit(&steps, &mut st(), false, Some(&m2), &sink2).await;
        let Outcome::Produced { rec, .. } = out2 else {
            panic!()
        };
        assert_eq!(
            source_for(rec.engine, rec.served_tier),
            ProposalSource::LocalBrain
        );
        assert_eq!(rec.fallback_from, None);
        let rows2 = sink2.0.lock().unwrap();
        assert_eq!(rows2.len(), 2); // undecided + produced, no failed model row
    }

    #[tokio::test]
    async fn ai_ladder_abort_connection_lost_and_generation_ending() {
        // J2 (2026-09-24 review: also cover `Cancelled` — the kernel-shutdown
        // abort case, §11.3.4's third "中止" row).
        for f in [
            ModelFailure::ConnectionLost,
            ModelFailure::GenerationEnding,
            ModelFailure::Cancelled,
        ] {
            let sink = MemSink::default();
            let m = StubModel::new(Err(f));
            let mut s = st();
            let steps = [Step::Model(Engine::Local), Step::ReflexFallback];
            assert_eq!(
                go(&steps, &mut s, false, Some(&m), &sink).await,
                Outcome::Aborted
            );
            assert_eq!(
                go(&steps, &mut s, false, Some(&m), &sink).await,
                Outcome::Aborted
            );
            assert_eq!(sink.0.lock().unwrap().len(), 1);
        }

        // Mutation target: the same slot with `Timeout` degrades instead and
        // produces a reflex proposal.
        let sink = MemSink::default();
        let m = StubModel::new(Err(ModelFailure::Timeout));
        let out = go(
            &[Step::Model(Engine::Local), Step::ReflexFallback],
            &mut st(),
            false,
            Some(&m),
            &sink,
        )
        .await;
        assert!(matches!(
            out,
            Outcome::Produced {
                engine: Engine::Reflex,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn ai_ladder_defer_busy_and_rate_limited_and_not_ready() {
        // J2b
        for f in [
            ModelFailure::Busy,
            ModelFailure::RateLimited,
            ModelFailure::NotReady,
        ] {
            let sink = MemSink::default();
            let m = StubModel::new(Err(f));
            let mut s = st();
            let steps = [Step::Model(Engine::Local), Step::ReflexFallback];
            assert_eq!(
                go(&steps, &mut s, false, Some(&m), &sink).await,
                Outcome::Deferred
            );
            assert_eq!(
                go(&steps, &mut s, false, Some(&m), &sink).await,
                Outcome::Deferred
            );
            assert_eq!(m.calls(), 1);
            assert!(sink
                .0
                .lock()
                .unwrap()
                .iter()
                .all(|r| r.engine != Engine::Reflex));
        }

        // Mutation target: `Unavailable/request_rejected` degrades instead
        // (does not defer) and the NEXT item still calls the model.
        let sink = MemSink::default();
        let m = StubModel::new(Err(ModelFailure::Unavailable {
            retryable: false,
            cause: crate::ai::ports::UnavailableCause::RequestRejected,
        }));
        let mut s = st();
        let steps = [Step::Model(Engine::Local), Step::ReflexFallback];
        assert!(matches!(
            go(&steps, &mut s, false, Some(&m), &sink).await,
            Outcome::Produced { .. }
        ));
        go(&steps, &mut s, false, Some(&m), &sink).await;
        assert_eq!(m.calls(), 2);
    }

    #[tokio::test]
    async fn ai_ladder_budget_and_circuit() {
        // J2c(a): budget = 1 -> second item is deferred.
        let sink = MemSink::default();
        let m = StubModel::new(Ok(reply(ServedTier::Local)));
        let mut s = st();
        s.calls_left = 1;
        let steps = [Step::Model(Engine::Local), Step::ReflexFallback];
        assert!(matches!(
            go(&steps, &mut s, false, Some(&m), &sink).await,
            Outcome::Produced { .. }
        ));
        assert_eq!(
            go(&steps, &mut s, false, Some(&m), &sink).await,
            Outcome::Deferred
        );

        // J2c(b): first item's `no_provider` opens the circuit -> second
        // item does not call `local` again (count stays 1), goes straight to
        // R2 with `fallback_from = local`.
        let sink = MemSink::default();
        let m = StubModel::new(Err(NO_PROVIDER));
        let mut s = st();
        let steps = [Step::Model(Engine::Local), Step::ReflexFallback];
        go(&steps, &mut s, false, Some(&m), &sink).await;
        let second = go(&steps, &mut s, false, Some(&m), &sink).await;
        assert_eq!(m.calls(), 1); // local not asked again this run
        let Outcome::Produced { rec, .. } = second else {
            panic!()
        };
        assert_eq!(rec.fallback_from, Some(Engine::Local));

        // Mutation target: `timeout` does NOT open the circuit -> the second
        // item still calls `local`.
        let sink = MemSink::default();
        let m = StubModel::new(Err(ModelFailure::Timeout));
        let mut s = st();
        go(&steps, &mut s, false, Some(&m), &sink).await;
        go(&steps, &mut s, false, Some(&m), &sink).await;
        assert_eq!(m.calls(), 2);
    }

    #[tokio::test]
    async fn ai_ladder_remote_down_local_up() {
        // Executive fails, local succeeds, in the SAME plan (both steps
        // present) -> `fallback_from = executive`, `source = local_brain`,
        // and the failed executive attempt is recorded.
        let sink = MemSink::default();
        let on = AiSettings {
            executive_enabled: true,
        };
        let m = ByEngine {
            exec: Err(NO_PROVIDER),
            local: Ok(reply(ServedTier::Local)),
        };
        let steps = plan(Capability::Classify, ModelAccess::RemoteAllowed, on, true);
        let out = go_and_submit(&steps, &mut st(), true, Some(&m), &sink).await;
        let Outcome::Produced { engine, rec, .. } = out else {
            panic!()
        };
        assert_eq!(engine, Engine::Local);
        assert_eq!(rec.fallback_from, Some(Engine::Executive));
        assert_eq!(
            source_for(rec.engine, rec.served_tier),
            ProposalSource::LocalBrain
        );
        let rows = sink.0.lock().unwrap();
        assert!(rows.iter().any(|r| r.engine == Engine::Executive && !r.ok));
    }

    #[tokio::test]
    async fn ai_ladder_served_tier_decides_source() {
        // J5: requesting `executive`, served `local` -> `source = local_brain`.
        let sink = MemSink::default();
        let m = StubModel::new(Ok(reply(ServedTier::Local)));
        let out = go(
            &[Step::Model(Engine::Executive)],
            &mut st(),
            true,
            Some(&m),
            &sink,
        )
        .await;
        let Outcome::Produced { rec, .. } = out else {
            panic!()
        };
        assert_eq!(
            source_for(rec.engine, rec.served_tier),
            ProposalSource::LocalBrain
        );
        assert_eq!(rec.prompt_tokens, Some(10));

        // Positive control: served `remote` (switch on) -> `executive`.
        let sink2 = MemSink::default();
        let m2 = StubModel::new(Ok(reply(ServedTier::Remote)));
        let out2 = go(
            &[Step::Model(Engine::Executive)],
            &mut st(),
            true,
            Some(&m2),
            &sink2,
        )
        .await;
        let Outcome::Produced { rec: rec2, .. } = out2 else {
            panic!()
        };
        assert_eq!(
            source_for(rec2.engine, rec2.served_tier),
            ProposalSource::Executive
        );
    }

    #[tokio::test]
    async fn ai_ladder_privacy_tripwire() {
        // J6: plan made with the switch ON, user turns it OFF before the
        // reply lands -> the reply is discarded, degrades.
        let sink = MemSink::default();
        let m = StubModel::new(Ok(reply(ServedTier::Remote)));
        let out = go(
            &[Step::Model(Engine::Executive), Step::ReflexFallback],
            &mut st(),
            false,
            Some(&m),
            &sink,
        )
        .await;
        assert!(matches!(
            out,
            Outcome::Produced {
                engine: Engine::Reflex,
                ..
            }
        ));
        assert_eq!(
            sink.0.lock().unwrap()[0].error_kind,
            Some("privacy_tripwire")
        );

        // Positive control: switch stays ON -> the same remote reply is accepted.
        let sink2 = MemSink::default();
        let out2 = go(
            &[Step::Model(Engine::Executive)],
            &mut st(),
            true,
            Some(&m),
            &sink2,
        )
        .await;
        let Outcome::Produced { rec, .. } = out2 else {
            panic!()
        };
        assert_eq!(
            source_for(rec.engine, rec.served_tier),
            ProposalSource::Executive
        );
    }

    #[tokio::test]
    async fn ai_ladder_bad_output_degrades() {
        let sink = MemSink::default();
        let m = StubModel::new(Ok(ModelReply {
            text: "garbage".into(),
            ..reply(ServedTier::Local)
        }));
        let out = go(
            &[Step::Model(Engine::Local), Step::ReflexFallback],
            &mut st(),
            false,
            Some(&m),
            &sink,
        )
        .await;
        assert!(matches!(
            out,
            Outcome::Produced {
                engine: Engine::Reflex,
                ..
            }
        ));
        assert_eq!(sink.0.lock().unwrap()[0].error_kind, Some("bad_output"));
    }

    #[tokio::test]
    async fn ai_ladder_run_item_future_is_send() {
        let sink = MemSink::default();
        let h = tokio::spawn(async move {
            let m = StubModel::new(Ok(reply(ServedTier::Local)));
            let mut s = st();
            go(
                &[Step::Model(Engine::Local)],
                &mut s,
                false,
                Some(&m),
                &sink,
            )
            .await
        });
        assert!(matches!(h.await.unwrap(), Outcome::Produced { .. }));
    }

    // ---- 2026-09-24 review: warn!/error! logging + record_call R6 --------

    /// A minimal `tracing::Subscriber` that just records the level of every
    /// event — enough to prove `log_model_failure`/`record_call_best_effort`
    /// actually call `tracing::warn!`/`error!`, without pulling in a
    /// tracing-subscriber test helper crate this project doesn't otherwise
    /// depend on.
    #[derive(Default, Clone)]
    struct LevelSpy(Arc<Mutex<Vec<tracing::Level>>>);
    impl tracing::Subscriber for LevelSpy {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            self.0.lock().unwrap().push(*event.metadata().level());
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    fn levels_during<F: FnOnce()>(f: F) -> Vec<tracing::Level> {
        let spy = LevelSpy::default();
        tracing::subscriber::with_default(spy.clone(), f);
        let out = spy.0.lock().unwrap().clone();
        out
    }

    /// §11.3.4's own two logging rows: `backend_config`/`forbidden` ->
    /// `warn!`; `response_too_large`/`bad_request` -> `error!`. Mutation
    /// target: change any one of `log_model_failure`'s match arms (e.g. drop
    /// the `BackendConfig` arm to the `_ => {}` catch-all) and the
    /// corresponding level goes missing.
    #[test]
    fn ai_ladder_logs_backend_config_and_forbidden_as_warn_response_too_large_and_bad_request_as_error(
    ) {
        let cases = [
            (
                ModelFailure::Unavailable {
                    retryable: false,
                    cause: UnavailableCause::BackendConfig,
                },
                tracing::Level::WARN,
            ),
            (ModelFailure::Forbidden, tracing::Level::WARN),
            (
                ModelFailure::Unavailable {
                    retryable: false,
                    cause: UnavailableCause::ResponseTooLarge,
                },
                tracing::Level::ERROR,
            ),
            (ModelFailure::BadRequest, tracing::Level::ERROR),
        ];
        for (f, want) in cases {
            let levels = levels_during(|| log_model_failure(&f));
            assert_eq!(levels, vec![want], "{f:?}");
        }
        // Positive control: a failure NOT in §11.3.4's logging rows (e.g.
        // `Timeout`, an ordinary/expected condition) logs nothing.
        assert_eq!(
            levels_during(|| log_model_failure(&ModelFailure::Timeout)),
            Vec::<tracing::Level>::new()
        );
    }

    /// R6: `record_call`'s failure is logged at `warn!` but does NOT stop
    /// the run — the model didn't misbehave, the sink did. Mutation target:
    /// revert `record_call_best_effort` to `let _ = sink.record_call(rec).await;`
    /// and the `warn!` assertion goes from `vec![WARN]` to `vec![]`.
    #[test]
    fn ai_ladder_record_call_failure_warns_but_does_not_abort_the_run() {
        #[derive(Default)]
        struct AlwaysFailsRecordCall;
        impl AiSink for AlwaysFailsRecordCall {
            async fn submit(
                &self,
                _cap: Capability,
                _draft: ProposalDraft,
                _rec: AiCallRecord,
            ) -> Result<(), SinkError> {
                unreachable!("this test never produces")
            }
            async fn record_call(&self, _rec: AiCallRecord) -> Result<(), SinkError> {
                Err(SinkError::Store("boom".into()))
            }
            async fn record_classify_eval(&self, _task_id: &str) -> Result<(), SinkError> {
                Ok(())
            }
            async fn precheck(&self, _cap: Capability, drafts: &[ProposalDraft]) -> Vec<bool> {
                vec![true; drafts.len()]
            }
        }

        let sink = AlwaysFailsRecordCall;
        let spy = LevelSpy::default();
        let out = tracing::subscriber::with_default(spy.clone(), || {
            // `with_default` is sync; drive the async ladder step to
            // completion on the current thread so the thread-local
            // subscriber stays active across every `.await` inside it.
            futures_lite_block_on(go_with_sink(&sink))
        });
        // The run still completed normally (fell through to the reflex
        // fallback) despite `record_call` failing on every non-producing
        // attempt.
        assert!(matches!(
            out,
            Outcome::Produced {
                engine: Engine::Reflex,
                ..
            }
        ));
        let levels = spy.0.lock().unwrap();
        assert!(
            levels.contains(&tracing::Level::WARN),
            "a failed record_call must be logged at warn!, levels seen: {levels:?}"
        );
    }

    async fn go_with_sink<S: AiSink>(sink: &S) -> Outcome<&'static str> {
        let m = StubModel::new(Err(NO_PROVIDER));
        let steps = [
            Step::ReflexDecisive,
            Step::Model(Engine::Local),
            Step::ReflexFallback,
        ];
        run_item(
            "run1",
            Capability::Classify,
            &steps,
            &mut st(),
            Some(&m),
            sink,
            &Settings(false),
            req,
            |r| {
                if r.text == "ok" {
                    Ok("model")
                } else {
                    Err("bad_output")
                }
            },
            || None,
            || Some("reflex-guess"),
            || "2026-09-24T00:00:00Z".into(),
            || "id".into(),
        )
        .await
    }

    /// `tracing::subscriber::with_default` takes a SYNC closure; `go_with_sink`
    /// is async. A tiny current-thread block_on avoids pulling in a runtime
    /// just for this one test — `futures_lite`/`pollster` aren't
    /// dependencies, so this hand-rolls the minimum needed: poll in a loop
    /// with a no-op waker (every future here is either already-ready or
    /// resolves on the first poll after an immediately-ready inner future,
    /// since nothing in `go_with_sink`'s path ever actually awaits I/O).
    fn futures_lite_block_on<F: std::future::Future>(fut: F) -> F::Output {
        use std::task::{Context, Poll};
        futures::pin_mut!(fut);
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        loop {
            if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return v;
            }
        }
    }
}
