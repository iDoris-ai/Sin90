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
