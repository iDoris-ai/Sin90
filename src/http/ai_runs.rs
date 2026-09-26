//! Shared "AI run" observation infra (design §11.4 公共): an in-process,
//! in-memory record of what a capability's background run did.
//! `POST /ai/classify` (T5.2.1, the only trigger route today) writes it via
//! [`RunRegistry`]; `GET /ai/runs/{run_id}` reads it. NOT persisted —
//! `sin90_ai_calls`/`sin90_proposals` are the durable record (design §11.9
//! R11); this is only an observation window, capped at 64 entries with FIFO
//! eviction (an approximation of the design's own "内存 LRU 64" — eviction is
//! by INSERTION order, not last-access order; acceptable because a run
//! record is read a few times right after it finishes and never touched
//! again, so "least recently inserted" and "least recently used" coincide in
//! practice for this workload).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use axum::extract::{Path as AxPath, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use crate::ai::Capability;

use super::state::Sin90State;

const MAX_TRACKED_RUNS: usize = 64;

#[derive(Debug, Clone, Serialize)]
pub struct AiRunItem {
    pub target: String,
    pub result: String,
    /// 2026-09-26 review (M2): WHY `result == "skipped"` — `"human_text"`
    /// (design §11.4.2's Q7 gate: the body has human-written content) or
    /// `"dedup"` (design §11.4 公共's "去重": a still-valid pending proposal
    /// of this shape already covers the target). `None` for every other
    /// result (including a non-skip outcome, and for capabilities that
    /// compute their own dedup-skip without going through this field yet).
    /// Serialized only when present — `GET /ai/runs/{id}`'s existing
    /// consumers see no new field on an item this doesn't apply to.
    ///
    /// T5.2.3 adds a THIRD value, `"low_confidence"` — the one case where
    /// this field appears on a result OTHER than `"skipped"`: classify's
    /// model step landed below `classify::CLASSIFY_CONFIDENCE_THRESHOLD`, so
    /// `result == "nothing"` (a decisive non-match, `ok = 1`), not a skip —
    /// the ladder DID try, it just wasn't confident enough to propose.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AiRunRecord {
    pub run_id: String,
    pub capability: &'static str,
    pub state: &'static str,
    pub items: Vec<AiRunItem>,
}

/// Per-capability single-flight + the bounded run log. A plain
/// `std::sync::Mutex` (not `tokio::sync::Mutex`): every method below is
/// synchronous map bookkeeping, held only across those few instructions,
/// never across an `.await` — so a std mutex is both correct and cheaper.
#[derive(Default)]
pub struct RunRegistry {
    order: VecDeque<String>,
    runs: HashMap<String, AiRunRecord>,
    busy: HashMap<Capability, String>,
}

impl RunRegistry {
    /// `Some(existing_run_id)` if `cap` already has a run in flight — the
    /// caller must return 409 without spawning anything (design §11.4 公共's
    /// "每个能力单飞").
    pub fn busy_run(&self, cap: Capability) -> Option<String> {
        self.busy.get(&cap).cloned()
    }

    /// Claims the single-flight slot for `cap` and starts tracking `run_id`
    /// as `"running"`. The caller must have already checked [`Self::busy_run`]
    /// returned `None` — this does not re-check (avoids a second lock
    /// acquisition inside the same critical section the trigger handler
    /// already holds).
    pub fn start(&mut self, cap: Capability, run_id: &str) {
        self.busy.insert(cap, run_id.to_string());
        if self.runs.len() >= MAX_TRACKED_RUNS {
            self.evict_one_non_running();
        }
        self.order.push_back(run_id.to_string());
        self.runs.insert(
            run_id.to_string(),
            AiRunRecord {
                run_id: run_id.to_string(),
                capability: cap.as_str(),
                state: "running",
                items: Vec::new(),
            },
        );
    }

    /// 2026-09-24 review (round 2, low): eviction must never drop a run
    /// that is STILL `"running"` — `GET /ai/runs/{id}` would otherwise
    /// report `state: "unknown"` for a run that is, in fact, actively in
    /// flight (and, for `classify`, still holding the single-flight slot).
    /// Scans `order` (oldest first) for the first entry that is NOT running
    /// and evicts that one instead of blindly popping the front; if EVERY
    /// tracked entry happens to be running, this is a no-op for this call —
    /// the log briefly holds more than [`MAX_TRACKED_RUNS`] entries rather
    /// than lose a live one.
    fn evict_one_non_running(&mut self) {
        let idx = self
            .order
            .iter()
            .position(|id| self.runs.get(id).is_none_or(|r| r.state != "running"));
        if let Some(idx) = idx {
            if let Some(id) = self.order.remove(idx) {
                self.runs.remove(&id);
            }
        }
    }

    /// Releases `cap`'s single-flight slot and records the run's final
    /// items. `state` is `"done"` or `"aborted"` (design §11.4 公共's
    /// `state: running|done|aborted|unknown`) — the caller decides which,
    /// since only it knows whether any item ended `Aborted` (or whether the
    /// whole task panicked, via [`BusyGuard`]'s `Drop`).
    pub fn finish(
        &mut self,
        cap: Capability,
        run_id: &str,
        state: &'static str,
        items: Vec<AiRunItem>,
    ) {
        self.busy.remove(&cap);
        if let Some(rec) = self.runs.get_mut(run_id) {
            rec.state = state;
            rec.items = items;
        }
    }

    pub fn get(&self, run_id: &str) -> Option<AiRunRecord> {
        self.runs.get(run_id).cloned()
    }
}

pub type SharedRunRegistry = Arc<Mutex<RunRegistry>>;

/// 2026-09-24 review (M4): every lock site goes through this instead of a
/// bare `.lock().unwrap()`. A background classify run panicking WHILE
/// holding the lock (a bug elsewhere, e.g. inside `RunRegistry` itself)
/// would otherwise poison the mutex and turn every subsequent
/// `POST /ai/classify` / `GET /ai/runs/{id}` into a 500-by-panic FOREVER — a
/// single bad run should not brick the whole AI subsystem for the rest of
/// the process's life. Recovering the guts of a poisoned mutex is safe here
/// because [`RunRegistry`]'s own methods have no partial-mutation window
/// that could leave it in a torn state (`start`/`finish` are each a handful
/// of infallible `HashMap`/`VecDeque` operations).
pub(super) fn lock_registry(registry: &SharedRunRegistry) -> MutexGuard<'_, RunRegistry> {
    registry.lock().unwrap_or_else(|poisoned| {
        tracing::warn!("ai run registry mutex was poisoned by a prior panic; recovering");
        poisoned.into_inner()
    })
}

/// 2026-09-24 review (M4, blocking-adjacent): RAII guard around ONE
/// capability's single-flight slot. If the classify background task panics
/// before calling [`Self::finish`], `Drop` still releases the slot (via
/// `RunRegistry::finish`, marking the run `"aborted"`) — WITHOUT this, a
/// panicking run would leave `busy` permanently pointing at a run_id that
/// will never finish, and `Capability::Classify` would return `409 ai_busy`
/// forever. Constructed only after the slot is actually claimed
/// (`RunRegistry::start`), consumed by [`Self::finish`] on the normal path so
/// `Drop` never double-releases.
pub struct BusyGuard {
    runs: SharedRunRegistry,
    cap: Capability,
    run_id: String,
    done: bool,
}

impl BusyGuard {
    #[must_use]
    pub fn new(runs: SharedRunRegistry, cap: Capability, run_id: String) -> Self {
        Self {
            runs,
            cap,
            run_id,
            done: false,
        }
    }

    /// The normal-completion path: records the final `state`/`items` and
    /// disarms `Drop` (so it does not ALSO release the slot a second time —
    /// harmless either way since `finish` is idempotent, but confusing to
    /// read if it fired twice).
    pub fn finish(mut self, state: &'static str, items: Vec<AiRunItem>) {
        self.done = true;
        lock_registry(&self.runs).finish(self.cap, &self.run_id, state, items);
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        if !self.done {
            tracing::warn!(
                run_id = %self.run_id,
                capability = self.cap.as_str(),
                "ai run task ended without calling BusyGuard::finish (panic?) — releasing the single-flight slot"
            );
            lock_registry(&self.runs).finish(self.cap, &self.run_id, "aborted", Vec::new());
        }
    }
}

/// `GET /ai/runs/{run_id}` (design §11.4 公共). `state: "unknown"` covers
/// both "this run_id never existed" and "evicted from the 64-entry log, or
/// lost on a process restart" (§11.9 R11) — the design's own vocabulary
/// (`running|done|aborted|unknown`) does not distinguish those. Either way,
/// `calls` (2026-09-24 review, M5) is read from the DURABLE `sin90_ai_calls`
/// table, not the in-memory registry — it is populated even when `items`/
/// `state` have been lost.
pub async fn get_ai_run(
    State(state): State<Sin90State>,
    AxPath(run_id): AxPath<String>,
) -> Response {
    let rec = lock_registry(&state.ai_runs).get(&run_id);
    let calls = state
        .store
        .list_ai_calls_for_run(&run_id)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, run_id = %run_id, "ai: failed to read sin90_ai_calls for a run");
            Vec::new()
        });
    match rec {
        Some(rec) => Json(serde_json::json!({
            "run_id": rec.run_id,
            "capability": rec.capability,
            "state": rec.state,
            "items": rec.items,
            "calls": calls,
        }))
        .into_response(),
        None => {
            // 2026-09-24 review (round 2, low): the registry lost track of
            // this run (evicted, or a process restart), but the DURABLE
            // `sin90_ai_calls` rows may still say which capability it was —
            // every row for one run shares `task_kind` (J9), so the first
            // one (if any) tells us.
            let capability = calls.first().map(|c| c.task_kind.clone());
            Json(serde_json::json!({
                "run_id": run_id,
                "capability": capability,
                "state": "unknown",
                "items": [],
                "calls": calls,
            }))
            .into_response()
        }
    }
}

/// L3 (2026-09-26 review): a hard wall-clock backstop on a whole capability
/// run, shared by `ai_classify`/`ai_summarize`/`ai_propose`'s own trigger
/// routes — lives HERE, in `http`, not in `ai::ladder`, because `ai/` may
/// not depend on `tokio` at all (§11.5's dependency arrow;
/// `tests/ai_boundary.rs`'s `EXTERN_OK` whitelist does not include it).
///
/// `ai::ladder::RunState`'s own budget (`RUN_DEADLINE_SECS`) is checked
/// only BETWEEN items, before starting a new step — it cannot stop a
/// SINGLE already-in-flight `_a24/model/complete` call (up to 125s,
/// `adapter_agent24::clients::model::MODEL_CALL_TIMEOUT`) from pushing a
/// run's real wall-clock time past its intended budget if that call started
/// right at the internal deadline. Wrapping the WHOLE run future in
/// `tokio::time::timeout` here closes that gap: if `deadline` elapses, the
/// wrapped future is DROPPED (cancelling whatever call was mid-flight)
/// instead of being allowed to keep running.
///
/// Takes `deadline` as a parameter — production always passes
/// [`RUN_HARD_DEADLINE`] — so tests can inject a short one instead of
/// waiting out a real, multi-minute deadline.
pub(crate) async fn with_hard_deadline<F: std::future::Future>(
    deadline: std::time::Duration,
    fut: F,
) -> Result<F::Output, tokio::time::error::Elapsed> {
    tokio::time::timeout(deadline, fut).await
}

/// M-1 (2026-09-26 review round 2, a bug in THIS commit's own first cut):
/// the hard backstop must be BIGGER than `ai::RUN_DEADLINE_SECS` alone, not
/// equal to it. `RunState::deadline` is checked only BETWEEN steps
/// (`with_hard_deadline`'s own doc) — a run whose LAST permitted model call
/// starts right at that internal deadline needs up to `MODEL_CALL_TIMEOUT`
/// MORE real time to reach its own check, record the call's outcome, and
/// return `Deferred` for whatever items are left. A backstop set to exactly
/// `RUN_DEADLINE_SECS` fires at (or, depending on scheduling, even slightly
/// BEFORE — the two deadlines are computed from two different `Instant::
/// now()` calls a few instructions apart) the same instant as that internal
/// check, cancelling the run mid-wrap-up instead of after it: every
/// proposal ALREADY durably committed via `AiSink::submit` earlier in the
/// SAME run vanishes from `GET /ai/runs/{id}`'s in-memory observation
/// window (the durable `sin90_proposals`/`sin90_ai_calls` rows are
/// unaffected — only this view is lost, since the whole `outcomes: Vec<_>`
/// this function would have returned never makes it back to the caller),
/// and the interrupted call gets no row at all — not even a failed one,
/// since `record_call_best_effort` never runs for a call whose future was
/// dropped mid-`.await`. `MODEL_CALL_TIMEOUT` worth of headroom, plus a 30s
/// margin for scheduling jitter and the wrap-up's own store write, is
/// enough for that grace period to always fit inside this backstop — making
/// it a genuine "this should never actually fire" ceiling, not a second
/// deadline racing the first one.
pub(crate) const RUN_HARD_DEADLINE: std::time::Duration =
    std::time::Duration::from_secs(crate::ai::RUN_DEADLINE_SECS)
        .saturating_add(crate::adapter_agent24::clients::model::MODEL_CALL_TIMEOUT)
        .saturating_add(std::time::Duration::from_secs(30));

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// L3: a future that would take far longer than `deadline` is cancelled
    /// — `with_hard_deadline` returns `Err` well before the slow future's
    /// own 5s sleep would have elapsed, proving the future is genuinely
    /// dropped/abandoned, not merely raced-and-ignored while still running
    /// in the background. Mutation: replace the body with a bare
    /// `fut.await` (no timeout at all) — this test would then hang for 5s
    /// and the `elapsed` assertion would fail (or the test would simply
    /// take ~5s instead of ~tens of ms, depending on the runner's own
    /// timeout — either way, red).
    #[tokio::test]
    async fn with_hard_deadline_cancels_a_future_that_outruns_it() {
        let started = Instant::now();
        let result = with_hard_deadline(Duration::from_millis(20), async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            42
        })
        .await;
        assert!(
            result.is_err(),
            "a future slower than the deadline must be cancelled, not awaited to completion"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "must return promptly once the deadline elapses, not wait out the slow future"
        );
    }

    /// Positive control for the test above: a future that finishes WELL
    /// within the deadline completes normally, with its real output.
    #[tokio::test]
    async fn with_hard_deadline_positive_control_lets_a_fast_future_finish() {
        let result = with_hard_deadline(Duration::from_secs(5), async { 42 }).await;
        assert_eq!(result.unwrap(), 42);
    }

    /// M-1: pins the RELATIONSHIP `RUN_HARD_DEADLINE` must keep — strictly
    /// bigger than `ai::RUN_DEADLINE_SECS` alone, by at least a full
    /// `MODEL_CALL_TIMEOUT`. Mutation: set `RUN_HARD_DEADLINE` back to
    /// exactly `Duration::from_secs(RUN_DEADLINE_SECS)` (the round-1 bug) —
    /// both assertions go red.
    #[test]
    fn run_hard_deadline_has_at_least_a_full_model_call_timeout_of_headroom() {
        let inner = Duration::from_secs(crate::ai::RUN_DEADLINE_SECS);
        assert!(
            RUN_HARD_DEADLINE > inner,
            "the hard backstop must be strictly bigger than the inner run deadline alone"
        );
        assert!(
            RUN_HARD_DEADLINE >= inner + crate::adapter_agent24::clients::model::MODEL_CALL_TIMEOUT,
            "the backstop must have at least a full MODEL_CALL_TIMEOUT of headroom over the \
             inner deadline, so an in-flight call started right at that deadline can still \
             finish gracefully"
        );
    }

    /// M-1 (behavioral half): simulates a run whose internal budget is hit
    /// FIRST — the wrapped future here takes longer than a short "inner"
    /// stand-in deadline but still finishes within the backstop's own
    /// margin (exactly the shape `RUN_HARD_DEADLINE`'s extra headroom
    /// exists for) — and must complete NORMALLY, with its FULL result, not
    /// be cancelled. Mutation: shrink the backstop passed here to less than
    /// the sleep duration (i.e. reproduce the round-1 bug of "backstop ==
    /// inner deadline, no headroom") — the future gets cancelled and this
    /// test's `unwrap()` panics on `Err`, red.
    #[tokio::test]
    async fn with_hard_deadline_lets_a_run_finish_when_the_inner_budget_is_hit_first() {
        // Stand-in for: RunState::deadline reached at t≈20ms, the
        // already-in-flight last call takes another ~30ms to wrap up
        // (comfortably less than a real MODEL_CALL_TIMEOUT) — total 50ms,
        // well inside a backstop with real headroom.
        let backstop = Duration::from_millis(20) + Duration::from_millis(200); // headroom, not equal
        let result = with_hard_deadline(backstop, async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            vec![
                "item-1".to_string(),
                "item-2".to_string(),
                "item-3".to_string(),
            ]
        })
        .await;
        assert_eq!(
            result.unwrap(),
            vec![
                "item-1".to_string(),
                "item-2".to_string(),
                "item-3".to_string()
            ],
            "a run that finishes within the backstop's headroom must return its FULL result, \
             not be cancelled"
        );
    }
}
