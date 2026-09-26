//! `POST /ai/classify` (T5.2.1, design §11.4 公共 + §11.4.1): validates
//! explicit `task_ids` synchronously (the only checks that can still turn
//! into a `400`), claims the single-flight slot, and spawns a background run
//! of [`crate::ai::classify::run_classify`] — the actual classify logic
//! lives there; this file is only the HTTP-shaped glue (`require_any_actor`,
//! `202`/`400`/`409`, the run registry, and the store-level "去重" query
//! `ai::classify` cannot do itself, since `Sin90Store::list_pending_proposals`
//! is not part of `AiReadModel`/`AiSink`'s deliberately narrow surface,
//! design §11.5).

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use std::sync::Arc;

use crate::ai::classify::{self, ClassifyInputError, ClassifyItem, ItemResult};
use crate::ai::{AiCallRecord, AiSink, Capability, ProposalDraft, SinkError, MODEL_ACCESS};
use crate::core::{ProposalStatus, Sin90Op, Task, TaskId};
use crate::store::{Sin90Store, StoreError};

use super::ai_runs::{lock_registry, with_hard_deadline, AiRunItem, BusyGuard};
use super::state::{EventSink, HttpModelPort, Sin90State};
use super::{error_response, parse};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClassifyReq {
    #[serde(default)]
    task_ids: Option<Vec<TaskId>>,
}

/// A page size to fetch the inbox with when auto-selecting (no `task_ids`)
/// and looking for enough NOT-already-deduped targets (2026-09-24 review,
/// M3) — starts at the normal cap and doubles until either enough targets
/// are found or the inbox is exhausted.
const AUTO_SELECT_MAX_PAGE: u32 = 2000;

/// 2026-09-24 review (round 2, Medium #1): wraps a `Sin90Store` so a
/// successful `submit` mirrors `proposal.submitted` IMMEDIATELY — right
/// after that commit, inside `submit` itself — instead of the run collecting
/// every `Proposed` outcome and emitting them all in a loop AFTER
/// `run_classify` returns (design §11.4 公共's L1: "submit 成功后...补发",
/// not "after the whole run finishes"). Without this, a run that panics
/// partway through, or a process that exits mid-run, loses the mirror event
/// for every proposal ALREADY durably committed to `sin90.db` before the
/// crash — nothing downstream (e.g. a WS subscriber relying on the mirrored
/// event) is ever told about it. `record_call`/`precheck` just delegate:
/// only a produced (`Ok`) `submit` has anything to mirror. Lives here, not
/// in `ai/`: it holds an `Arc<dyn EventSink>`, which `ai/` may never see
/// (§11.5's boundary, unaffected — this type never appears under
/// `src/ai/**`) — the `syn` whitelist checker (J7) stays green because
/// nothing in `ai/classify.rs` changed; `run_classify` only knows it got
/// handed "something that implements `AiSink`", same as before.
/// `pub(crate)` (not private): exercised directly from `http::tests`
/// (Medium #1's own regression test), which needs to compose it with a
/// panic-injecting wrapper to prove emission happens per-`submit`, not
/// batched at the end of a run.
pub(crate) struct EmittingSink<'a> {
    pub(crate) store: &'a Sin90Store,
    pub(crate) sink: Arc<dyn EventSink>,
}

impl AiSink for EmittingSink<'_> {
    async fn submit(
        &self,
        cap: Capability,
        draft: ProposalDraft,
        rec: AiCallRecord,
    ) -> Result<(), SinkError> {
        let id = draft.id.clone();
        let result = AiSink::submit(self.store, cap, draft, rec).await;
        if result.is_ok() {
            let mut payload = serde_json::Map::new();
            payload.insert("id".to_string(), serde_json::Value::String(id));
            self.sink.emit("proposal.submitted", payload);
        }
        result
    }

    async fn record_call(&self, rec: AiCallRecord) -> Result<(), SinkError> {
        AiSink::record_call(self.store, rec).await
    }

    async fn precheck(&self, cap: Capability, drafts: &[ProposalDraft]) -> Vec<bool> {
        AiSink::precheck(self.store, cap, drafts).await
    }
}

/// `POST /ai/classify {"task_ids"?: [...]}` → `202 {"run_id", "capability"}`;
/// `409 {"code": "ai_busy", "run_id"}` if classify already has a run in
/// flight; `400` if `task_ids` is over the cap, has a duplicate, or names a
/// task not in the inbox. `require_any_actor` (design §11.4 公共): triggering
/// only writes `sin90_proposals`/`sin90_ai_calls`, the same actor gate
/// `POST /proposals` uses, not `require_human`.
///
/// 2026-09-24 review (M3): dedup (and, for the no-`task_ids` case, the
/// inbox-paging loop) happens AFTER the single-flight slot is claimed, not
/// before — the OLD order did a `list_pending_proposals` + `precheck` round
/// trip even when about to reject with `409` anyway. The explicit-`task_ids`
/// validation below (length/duplicate/inbox-membership) stays BEFORE the
/// slot claim, because those are the only checks that can still produce a
/// `400` — deferring them into the background task would mean answering
/// `202` and only THEN discovering the request was invalid, which the design
/// does not allow ("task_ids > 20 → 400，不产生 run").
pub async fn trigger_classify(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(r) = state.require_any_actor(&headers) {
        return r;
    }
    let req: ClassifyReq = match parse(&body, "ai classify request") {
        Ok(b) => b,
        Err(r) => return r,
    };

    // Only the explicit-ids path can still 400 — resolve (and validate) it
    // synchronously; the no-ids ("auto") path is entirely deferred into the
    // background task (M3).
    let explicit_targets = match &req.task_ids {
        Some(ids) => {
            let reader = state.store.ai_reader();
            match classify::select_targets(&reader, Some(ids)).await {
                Ok(t) => Some(t),
                Err(e) => return classify_input_error(e),
            }
        }
        None => None,
    };

    let run_id = format!("run-{}", crate::core::ulid());
    {
        // 2026-09-24 review (round 2, low): go through `lock_registry`
        // (poison-recovering) instead of a bare `.lock().unwrap_or_else`
        // duplicated at this call site.
        let mut reg = lock_registry(&state.ai_runs);
        if let Some(existing) = reg.busy_run(Capability::Classify) {
            return (
                StatusCode::CONFLICT,
                Json(json!({"code": "ai_busy", "run_id": existing})),
            )
                .into_response();
        }
        reg.start(Capability::Classify, &run_id);
    }
    let guard = BusyGuard::new(state.ai_runs.clone(), Capability::Classify, run_id.clone());

    let bg_state = state.clone();
    let rid = run_id.clone();
    tokio::spawn(async move {
        let store = &bg_state.store;
        let reader = store.ai_reader();

        // M3: selection + dedup now happens HERE, after the slot is secured.
        let targets = match explicit_targets {
            Some(t) => t,
            None => auto_select_targets(store, &reader).await,
        };
        let (targets, skipped) = match dedup_targets(store, &targets).await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(error = %e, run_id = %rid, "classify: dedup query failed; running WITHOUT dedup for this run");
                (targets, Vec::new())
            }
        };

        // T5.1.2 (closes the TODO this used to carry): `Some` only in
        // mounted mode, and only when the kernel granted `_a24/model/` at
        // handshake (`Sin90State::model`'s own doc) — `standalone` mode
        // leaves this `None`, same as before. `HttpModelPort` wraps the
        // `http`-local `dyn ModelCaller` back into something that
        // implements `ai::ModelPort` (`http::state`'s own doc) without this
        // file ever naming `adapter_agent24::clients::model::ModelClient`.
        let model = bg_state.model.clone().map(HttpModelPort);
        let model = model.as_ref();
        // H2 (design §11.4 公共's L1, round 2 fix): `EmittingSink` mirrors
        // `proposal.submitted` IMMEDIATELY inside `submit`, one commit at a
        // time — not batched into a loop AFTER the whole run finishes (the
        // old shape here lost every already-committed proposal's event if
        // the run panicked, or the process exited, before reaching this
        // point).
        let emitting = EmittingSink {
            store,
            sink: bg_state.sink.clone(),
        };
        // L3 (2026-09-26 review) + M-1 (round 2): a HARD backstop on top of
        // `ai::ladder`'s own internal between-items budget (`RunState::
        // deadline` — checked before STARTING a step, not while one is in
        // flight). `RUN_HARD_DEADLINE` is DELIBERATELY bigger than
        // `RUN_DEADLINE_SECS` alone (its own doc, `ai_runs`) — a run that
        // starts its LAST permitted call right at the internal deadline
        // needs up to a full `MODEL_CALL_TIMEOUT` more real time to wrap up
        // gracefully, and this backstop must not fire before that grace
        // period ends. `with_hard_deadline` (`ai_runs`) lives outside `ai/`,
        // which may not depend on `tokio` at all (§11.5's dependency arrow;
        // `tests/ai_boundary.rs`'s `EXTERN_OK` does not list it).
        let outcomes = with_hard_deadline(
            bg_state.run_hard_deadline,
            classify::run_classify(
                &rid,
                &targets,
                // T5.1.2 (closes the TODO this used to carry): the compiled-in
                // constant, not a hardcoded `LocalOnly` — `standalone` mode's
                // `model` is always `None` regardless, so `plan()` never
                // schedules a `Step::Model` there no matter which `ModelAccess`
                // value this reads.
                *MODEL_ACCESS,
                model,
                &emitting,
                &reader,
            ),
        )
        .await;

        let (final_state, items) = match outcomes {
            Ok(outcomes) => {
                let aborted = outcomes.iter().any(|o| o.result == ItemResult::Aborted);
                let items = build_classify_run_items(skipped, outcomes);
                (if aborted { "aborted" } else { "done" }, items)
            }
            Err(_elapsed) => {
                tracing::warn!(
                    run_id = %rid,
                    "classify: run exceeded the {:?} hard deadline; cancelled mid-flight",
                    bg_state.run_hard_deadline
                );
                // No per-item outcomes to report — the run was cancelled
                // mid-flight, not finished — but the dedup-skipped items
                // computed BEFORE the run started are still honest to show.
                ("aborted", build_classify_run_items(skipped, Vec::new()))
            }
        };
        guard.finish(final_state, items);
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({"run_id": run_id, "capability": "classify"})),
    )
        .into_response()
}

/// T5.2.3 review (M1): extracted out of `trigger_classify`'s background task
/// so this wire mapping — including `reason: o.reason`, the line the
/// low-confidence tag actually travels through onto `AiRunItem` — is directly
/// testable without needing a real `ModelPort` wired into the trigger route
/// itself (T5.1.2 hasn't landed yet; production still hardcodes
/// `model: None` above, so there is no way to drive a low-confidence outcome
/// through an actual `POST /ai/classify` round trip today). `http::tests`
/// calls this directly with hand-built `ClassifyItem`s (from a REAL
/// `run_classify` call against a stub `ModelPort`) to pin the wire shape.
pub(crate) fn build_classify_run_items(
    skipped: Vec<(TaskId, &'static str)>,
    outcomes: Vec<ClassifyItem>,
) -> Vec<AiRunItem> {
    let mut items: Vec<AiRunItem> = skipped
        .into_iter()
        .map(|(task_id, reason)| AiRunItem {
            target: task_id,
            result: "skipped".to_string(),
            // M2 (2026-09-26 review): this run never even tried — either a
            // still-valid pending proposal already covers it (`"dedup"`) or
            // (T5.7.2, design §2 #31) a rejected one does, unchanged since
            // (`"suppressed_rejected"`).
            reason: Some(reason),
        })
        .collect();
    items.extend(outcomes.into_iter().map(|o| AiRunItem {
        target: o.task_id,
        result: item_result_str(&o.result).to_string(),
        // T5.2.3: `o.reason` is `Some("low_confidence")` for a model
        // step that landed below the confidence threshold, `None` for
        // every other cause of this item's result.
        reason: o.reason,
    }));
    items
}

fn item_result_str(r: &ItemResult) -> &'static str {
    match r {
        ItemResult::Proposed(_) => "proposed",
        ItemResult::Nothing => "nothing",
        ItemResult::Deferred => "deferred",
        ItemResult::Rejected => "rejected",
        // 2026-09-24 review (round 2, low; design updated round 3, commit
        // 86fc700): NOT "skipped" — conflating it with "skipped" (dedup's
        // own word for "we didn't even try this one") would have erased a
        // real distinction: this item DID get tried and the run died
        // partway through, a different, more alarming outcome than "already
        // had a valid pending proposal". Design §11.4 公共's `result` value
        // domain now explicitly includes `aborted` alongside
        // `proposed|nothing|deferred|rejected|skipped` — this is no longer
        // an out-of-domain value flagged for the coordinator's attention,
        // it is the documented one.
        ItemResult::Aborted => "aborted",
    }
}

fn classify_input_error(e: ClassifyInputError) -> Response {
    match e {
        ClassifyInputError::EmptyTaskIds => error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "task_ids was given but empty; omit it entirely to auto-select from the inbox",
        ),
        ClassifyInputError::TooManyTaskIds => error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            &format!(
                "task_ids must have at most {} entries",
                classify::MAX_CLASSIFY_TASK_IDS
            ),
        ),
        ClassifyInputError::TaskNotInInbox(id) => error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            &format!("task {id} is not in the inbox"),
        ),
        ClassifyInputError::DuplicateTaskId(id) => error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            &format!("task_ids contains {id} more than once"),
        ),
        ClassifyInputError::ReadFailed(msg) => {
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", &msg)
        }
    }
}

/// 2026-09-24 review (M3): when no `task_ids` were given, keep paging
/// further into the inbox — doubling the page size each time — until either
/// [`classify::MAX_CLASSIFY_TASK_IDS`] non-deduped tasks are found or the
/// inbox is exhausted (the OLD behavior fetched exactly the oldest 20 once,
/// so a run could silently process fewer than 20 real targets whenever the
/// front of the inbox happened to be mostly already-pending tasks, even with
/// plenty of untouched ones further back). `pub(crate)` (not private):
/// exercised directly from `http::tests` (M3 round 2) without standing up a
/// full background run.
pub(crate) async fn auto_select_targets(
    store: &Sin90Store,
    reader: &crate::store::AiReader,
) -> Vec<Task> {
    use crate::ai::AiReadModel;
    // M5 (review round 2): fetched ONCE, outside the page-doubling loop
    // below — see `dedup_targets_with_rejected`'s own doc for why re-reading
    // it on every doubled page was pure waste, not a correctness safeguard.
    // A failed read degrades to "treat as no known rejections" (same
    // fail-open posture the loop's own `dedup_targets_with_rejected` error
    // arm already has), logged so it is not silent.
    let rejected = store
        .list_rejected_ops(Capability::Classify.as_str())
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "classify: auto-select list_rejected_ops read failed; treating as empty");
            Vec::new()
        });
    let mut page = classify::MAX_CLASSIFY_TASK_IDS as u32;
    loop {
        let candidates = match reader.inbox(page).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "classify: auto-select inbox read failed");
                return Vec::new();
            }
        };
        let exhausted = (candidates.len() as u32) < page;
        let (kept, _skipped) = match dedup_targets_with_rejected(store, &candidates, &rejected)
            .await
        {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(error = %e, "classify: auto-select dedup failed; using undeduped candidates");
                (candidates, Vec::new())
            }
        };
        if kept.len() >= classify::MAX_CLASSIFY_TASK_IDS
            || exhausted
            || page >= AUTO_SELECT_MAX_PAGE
        {
            let mut kept = kept;
            kept.truncate(classify::MAX_CLASSIFY_TASK_IDS);
            return kept;
        }
        page = (page * 2).min(AUTO_SELECT_MAX_PAGE);
    }
}

/// §11.4 公共's "去重": skip targets that already have a still-PENDING
/// `AssignTaskDirection` proposal that re-runs `AiSink::precheck`'s dry run
/// successfully ("仍然有效") — `reason` `"dedup"` — **or** (T5.7.2, design §2
/// #31) a REJECTED one whose "situation" has not changed since — `reason`
/// `"suppressed_rejected"`. Lives here, not in `ai::classify`, because both
/// halves need `Sin90Store` methods (`list_pending_proposals`/
/// `list_rejected_ops`) that are not part of `AiReadModel`/`AiSink`'s
/// deliberately narrow surface (design §11.5). `pub(crate)` (not private):
/// exercised directly from `http::tests` (J14) without standing up a full
/// background run.
pub(crate) async fn dedup_targets(
    store: &Sin90Store,
    targets: &[Task],
) -> Result<(Vec<Task>, Vec<(TaskId, &'static str)>), StoreError> {
    // T5.7.2 review round 3: a `list_rejected_ops` failure degrades to "no
    // known rejections" (same fail-open posture `auto_select_targets`'s own
    // copy of this read already uses) instead of aborting the whole call —
    // the pending-proposal dedup half computed inside
    // `dedup_targets_with_rejected` does not depend on this read at all and
    // must not go down with it.
    let rejected = store
        .list_rejected_ops(Capability::Classify.as_str())
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(
                error = %e,
                "dedup_targets: list_rejected_ops failed; treating as empty, pending-dedup unaffected"
            );
            Vec::new()
        });
    dedup_targets_with_rejected(store, targets, &rejected).await
}

/// M5 (review round 2): the `list_rejected_ops` read factored out of
/// [`dedup_targets`] itself so [`auto_select_targets`]'s page-doubling loop
/// can fetch it ONCE, up front, and pass the SAME snapshot into every page's
/// dedup call, instead of re-querying it fresh on every doubled page. The
/// set of REJECTED proposals cannot meaningfully change between this run's
/// own page-doubling attempts — it only grows via a human `POST /proposals/
/// {id}/reject`, an action this same in-flight background run cannot itself
/// trigger — so the repeated re-fetch was pure waste, not a correctness
/// safeguard `auto_select_targets` depended on. [`dedup_targets`] itself
/// (the pending, single-call test surface most of `http::tests` exercises)
/// stays a thin wrapper that fetches its own `rejected` snapshot.
async fn dedup_targets_with_rejected(
    store: &Sin90Store,
    targets: &[Task],
    rejected: &[crate::store::RejectedOpsRow],
) -> Result<(Vec<Task>, Vec<(TaskId, &'static str)>), StoreError> {
    let target_ids: std::collections::HashSet<&str> =
        targets.iter().map(|t| t.id.as_str()).collect();
    // 2026-09-24 review (M3): pending-only at the SQL level, not
    // `list_proposals()` + a Rust-side status filter.
    let pending = store.list_pending_proposals().await?;
    let mut drafts = Vec::new();
    let mut draft_task_ids = Vec::new();
    for p in pending {
        if p.status != ProposalStatus::Pending {
            continue; // defensive; `list_pending_proposals` already filters
        }
        if let [Sin90Op::AssignTaskDirection { task_id, .. }] = p.ops.as_slice() {
            if target_ids.contains(task_id.as_str()) {
                draft_task_ids.push(task_id.clone());
                drafts.push(ProposalDraft {
                    id: p.id,
                    ops: p.ops,
                    rationale: p.rationale,
                });
            }
        }
    }
    let mut skip: std::collections::HashMap<TaskId, &'static str> =
        std::collections::HashMap::new();
    if !drafts.is_empty() {
        let valid = AiSink::precheck(store, Capability::Classify, &drafts).await;
        for (id, ok) in draft_task_ids.into_iter().zip(valid) {
            if ok {
                skip.insert(id, "dedup");
            }
        }
    }

    // T5.7.2 review round 2 (H1): rejection-based suppression now applies to
    // EVERY target, not just a target still literally `direction_id IS
    // NULL` — the ORIGINAL scoping missed the exact case a 待定-parked
    // task's OWN reclassify-to-D proposal gets rejected: with the scoping in
    // place, that rejection was NEVER consulted again (`rejectable` never
    // contained the task at all, since its `direction_id` is `sin90-triage`,
    // not `NULL`), so the moment the 待定 retry gate
    // (`AiReadModel::inbox`/`inbox_task`'s own, separate SQL, `store/
    // ai_port.rs`) re-qualified the task, classify recommended the SAME
    // already-rejected D again, forever, for as long as nothing else about
    // the task's situation changed. The two mechanisms stay independent
    // (the gate decides WHETHER a triage task is even considered a target
    // this run; this one decides whether a PARTICULAR recommendation for it
    // stays suppressed) — only the scoping restriction that used to keep
    // them from ever interacting for the same task is gone.
    let rejectable: Vec<&Task> = targets
        .iter()
        .filter(|t| !skip.contains_key(&t.id))
        .collect();
    if !rejectable.is_empty() {
        // Fingerprint reduces to `task_id` alone here (design §2 #31): this
        // runs BEFORE the ladder decides which Direction it would recommend,
        // so there is nothing yet to compare a rejected `direction_id`
        // against — "same task, situation unchanged" already implies "same
        // recommendation" (R1/R2/the model are pure functions of current
        // state). `list_rejected_ops` is oldest-first, so plain overwriting
        // insert keeps the MOST RECENT rejection per task — its OWN
        // `proposed_at` (review round 2, M2: the "situation changed" time
        // basis moved from `rejected_at` to `proposed_at` — a new eligible
        // Direction that appeared between drafting and rejecting already
        // made that proposal stale by the time it was rejected).
        let mut latest_reject: std::collections::HashMap<TaskId, String> =
            std::collections::HashMap::new();
        for r in rejected {
            for op in &r.ops {
                if let Sin90Op::AssignTaskDirection { task_id, .. } = op {
                    if target_ids.contains(task_id.as_str()) {
                        latest_reject.insert(task_id.clone(), r.proposed_at.clone());
                    }
                }
            }
        }
        if !latest_reject.is_empty() {
            // The "新 Direction" leg (§2 #31): one read, shared by every
            // rejected task this call is considering — a Direction created
            // after ANY of their `proposed_at` values un-suppresses them.
            //
            // T5.7.2 review round 3: this read (and `task_modified_since`
            // below) used to propagate a failure with `?`, which aborted
            // this WHOLE function — throwing away the pending-proposal
            // dedup (`skip` entries already inserted above) along with the
            // rejection-suppression half that actually failed. Only the
            // rejection-suppression half is degraded here (fail-open: a
            // rejected task whose "situation changed" check could not be
            // answered is simply left un-suppressed, i.e. shown to the
            // human again rather than silently hidden) — `skip`'s existing
            // `"dedup"` entries, and every OTHER rejectable task's own
            // check, are unaffected.
            let max_new_direction = match store
                .ai_reader()
                .max_eligible_direction_created_at()
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "dedup_targets: max_eligible_direction_created_at failed; degrading rejection-suppression only, pending-dedup unaffected"
                    );
                    None
                }
            };
            for t in rejectable {
                let Some(proposed_at) = latest_reject.get(&t.id) else {
                    continue;
                };
                // The "task 被修改过" leg — review round 2 (M1): a real,
                // `sin90_events`-backed check (`Sin90Store::
                // task_modified_since`'s own doc has the full story),
                // replacing a direct `t.updated_at > proposed_at` compare —
                // that used to be safe ONLY while this branch stayed scoped
                // to `direction_id IS NULL` tasks (H1 above removed that
                // scoping) and was ALSO fooled by an accepted `ReorderTasks`
                // bumping `updated_at` for every task in a week regardless
                // of Direction, human edit or not.
                let task_modified = match store.task_modified_since(&t.id, proposed_at).await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            task_id = %t.id,
                            "dedup_targets: task_modified_since failed; degrading rejection-suppression for this task only"
                        );
                        continue;
                    }
                };
                let new_direction = max_new_direction
                    .as_deref()
                    .is_some_and(|c| c > proposed_at.as_str());
                if !task_modified && !new_direction {
                    skip.insert(t.id.clone(), "suppressed_rejected");
                }
            }
        }
    }

    let kept = targets
        .iter()
        .filter(|t| !skip.contains_key(&t.id))
        .cloned()
        .collect();
    let skipped = targets
        .iter()
        .filter_map(|t| skip.get(&t.id).map(|reason| (t.id.clone(), *reason)))
        .collect();
    Ok((kept, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// L3 (2026-09-24 review, round 3): pins `item_result_str`'s wire
    /// vocabulary directly, including `Aborted => "aborted"` (design §11.4
    /// 公共's `result` domain, updated in commit 86fc700 to include it
    /// alongside `proposed|nothing|deferred|rejected|skipped`).
    #[test]
    fn item_result_str_covers_every_variant() {
        assert_eq!(
            item_result_str(&ItemResult::Proposed("p1".to_string())),
            "proposed"
        );
        assert_eq!(item_result_str(&ItemResult::Nothing), "nothing");
        assert_eq!(item_result_str(&ItemResult::Deferred), "deferred");
        assert_eq!(item_result_str(&ItemResult::Rejected), "rejected");
        assert_eq!(item_result_str(&ItemResult::Aborted), "aborted");
    }
}
