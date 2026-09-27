//! `POST /ai/propose` (T5.4.1, design §11.4 公共 + §11.4.3): validates the
//! target week `week_id` synchronously (the only check that can still turn
//! into a `404`/`409`), claims the single-flight slot, and — inside the
//! background task, AFTER the slot is secured (2026-09-24 review H3, same
//! ordering `ai_classify`'s own M3 fix uses) — computes the dedup decision
//! and spawns [`crate::ai::propose::run_propose`]. The actual propose logic
//! lives there; this file is only the HTTP-shaped glue (`require_any_actor`,
//! `202`/`400`/`404`/`409`, the dedup query `ai::propose` cannot do itself
//! since `Sin90Store::list_pending_proposals` is not part of `AiReadModel`/
//! `AiSink`'s deliberately narrow surface, and wiring the run into the
//! shared [`super::ai_runs`] registry) — same division of labor
//! `http::ai_classify` already established for its own capability.

use std::collections::HashSet;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::ai::propose::{self, ProposeDedup, ProposeInputError, ProposeItemResult};
use crate::ai::{AiSink, Capability, ProposalDraft, MODEL_ACCESS};
use crate::core::{Sin90Op, Task, TaskId, Week, WeekId};
use crate::store::{Sin90Store, StoreError};

use super::ai_classify::EmittingSink;
use super::ai_runs::{lock_registry, with_hard_deadline, AiRunItem, BusyGuard};
use super::state::{HttpModelPort, Sin90State};
use super::{error_response, parse};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposeReq {
    week_id: WeekId,
}

/// `POST /ai/propose {"week_id"}` → `202 {"run_id", "capability"}`;
/// `409 {"code": "ai_busy", "run_id"}` if propose already has a run in
/// flight, or `409 {"code": "week_not_open"}` if `week_id` names a week that
/// is not currently `planning`/`active`; `404` if `week_id` does not exist.
/// `require_any_actor` (design §11.4 公共): triggering only writes
/// `sin90_proposals`/`sin90_ai_calls`, the same actor gate `POST /proposals`
/// uses, not `require_human`.
pub async fn trigger_propose(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(r) = state.require_any_actor(&headers) {
        return r;
    }
    let req: ProposeReq = match parse(&body, "ai propose request") {
        Ok(b) => b,
        Err(r) => return r,
    };

    // §11.4.3's "输入" check happens synchronously, BEFORE the single-flight
    // slot is claimed — an unknown/non-open week must never turn into a 202
    // that silently does nothing.
    let reader = state.store.ai_reader();
    let week = match propose::select_week(&reader, &req.week_id).await {
        Ok(w) => w,
        Err(e) => return propose_input_error(e),
    };

    let run_id = format!("run-{}", crate::core::ulid());
    {
        let mut reg = lock_registry(&state.ai_runs);
        if let Some(existing) = reg.busy_run(Capability::Propose) {
            return (
                StatusCode::CONFLICT,
                Json(json!({"code": "ai_busy", "run_id": existing})),
            )
                .into_response();
        }
        reg.start(Capability::Propose, &run_id);
    }
    let guard = BusyGuard::new(state.ai_runs.clone(), Capability::Propose, run_id.clone());

    let bg_state = state.clone();
    let rid = run_id.clone();
    tokio::spawn(async move {
        let store = &bg_state.store;
        let reader = store.ai_reader();

        // H3 (2026-09-24 review, design §11.4 公共's "去重"): computed HERE,
        // after the slot is secured (same M3 timing `ai_classify` already
        // uses) — a target about to 409 anyway should not pay for an extra
        // round trip.
        let dedup = match dedup_propose(store, &week).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, run_id = %rid, "propose: dedup query failed; running WITHOUT dedup for this run");
                ProposeDedup::default()
            }
        };

        // T5.1.2 (closes the TODO this used to carry — mirrors
        // `ai_classify::trigger_classify`'s own wiring): `Some` only in
        // mounted mode with `_a24/model/` granted, `None` in `standalone`.
        let model = bg_state.model.clone().map(HttpModelPort);
        let model = model.as_ref();
        // M2 (2026-09-24 review): `EmittingSink` mirrors `proposal.submitted`
        // IMMEDIATELY inside `submit` — reused verbatim from `ai_classify`'s
        // own round-2 fix (design §11.4 公共's L1: "submit 成功后...补发",
        // not "after the whole run finishes"), not a second implementation.
        let emitting = EmittingSink {
            store,
            sink: bg_state.sink.clone(),
        };
        // L3 (2026-09-26 review) + M-1 (round 2, mirrors `ai_classify::
        // trigger_classify`'s own fix): a hard backstop (`RUN_HARD_DEADLINE`
        // — deliberately bigger than `RUN_DEADLINE_SECS` alone) via
        // `with_hard_deadline` (`ai_runs`) — see its own doc for the full
        // reasoning.
        let outcomes = with_hard_deadline(
            bg_state.run_hard_deadline,
            propose::run_propose(
                &rid,
                &week,
                &dedup,
                *MODEL_ACCESS,
                model,
                &emitting,
                &reader,
            ),
        )
        .await;

        fn to_items(outcomes: Vec<propose::ProposeItem>) -> Vec<AiRunItem> {
            outcomes
                .into_iter()
                .map(|o| AiRunItem {
                    target: o.kind.as_str().to_string(),
                    result: item_result_str(&o.result).to_string(),
                    // M2 (2026-09-26 review): propose's own `Skipped` IS
                    // dedup — the only way this capability's item-level
                    // result can be "skipped" (§11.4 公共's "去重").
                    reason: matches!(o.result, ProposeItemResult::Skipped).then_some("dedup"),
                })
                .collect()
        }

        let (final_state, items) = match outcomes {
            Ok(outcomes) => {
                let aborted = outcomes
                    .iter()
                    .any(|o| o.result == ProposeItemResult::Aborted);
                (if aborted { "aborted" } else { "done" }, to_items(outcomes))
            }
            Err(_elapsed) => {
                tracing::warn!(
                    run_id = %rid,
                    "propose: run exceeded the {:?} hard deadline; cancelled mid-flight",
                    bg_state.run_hard_deadline
                );
                ("aborted", Vec::new())
            }
        };
        guard.finish(final_state, items);
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({"run_id": run_id, "capability": "propose"})),
    )
        .into_response()
}

fn item_result_str(r: &ProposeItemResult) -> &'static str {
    match r {
        ProposeItemResult::Proposed(_) => "proposed",
        ProposeItemResult::Nothing => "nothing",
        ProposeItemResult::Deferred => "deferred",
        ProposeItemResult::Rejected => "rejected",
        // 2026-09-24 review (aligned with `ai_classify::item_result_str`'s
        // own round-2 fix): NOT "skipped" — an aborted run item and a
        // dedup-skipped one are different outcomes ("we tried and the run
        // died" vs. "we didn't even try, something already covers this").
        ProposeItemResult::Aborted => "aborted",
        ProposeItemResult::Skipped => "skipped",
    }
}

fn propose_input_error(e: ProposeInputError) -> Response {
    match e {
        ProposeInputError::UnknownWeek(id) => error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            &format!("week {id} does not exist"),
        ),
        // L4 (2026-09-24 review): same v1 error envelope every other
        // handler in this crate returns (`error_response`), not a
        // hand-rolled body shape — `error_response`'s `code` field carries
        // the same `"week_not_open"` string a client would otherwise have
        // had to special-case a different JSON shape for.
        ProposeInputError::WeekNotOpen(id, status) => error_response(
            StatusCode::CONFLICT,
            "week_not_open",
            &format!("week {id} is not open (status {status:?})"),
        ),
        ProposeInputError::ReadFailed(msg) => {
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", &msg)
        }
    }
}

/// §11.4 公共's "去重" (H3, 2026-09-24 review) for propose: partitions
/// currently-PENDING, AI-PRODUCED proposals into propose's three shapes FOR
/// THIS WEEK, prechecks them all in ONE batch (`AiSink::precheck`, one
/// write-lock acquisition — same posture `ai_classify::dedup_targets`
/// already uses), and turns the result into what [`crate::ai::propose::
/// run_propose`] needs: which prev-week tasks a still-valid pending CARRY
/// proposal already covers (excluded from re-offering, not a blanket skip —
/// §11.4.3's carry is "the deterministic set minus what's already covered",
/// not "one proposal per week"), and whether a still-valid pending REORDER/
/// CREATE proposal means this run should not even attempt a new one.
///
/// M-2 (2026-09-24 review round 3, design §11.4.3): "still valid" ONLY ever
/// considers proposals this run's OWN capability actually produced —
/// [`Sin90Store::list_ai_produced_proposal_ids`]'s `sin90_ai_calls.
/// proposal_id` join, not the human-readable `"ai-propose-<ulid>"` id
/// prefix `submit_one` mints (a human/automation client submitting through
/// `POST /proposals` supplies their OWN id, unrelated to whether they went
/// through the AI ladder at all — see that method's own doc). A same-shape
/// proposal a human or the automation key submitted directly must NEVER
/// block a fresh AI decision.
///
/// L-5 (2026-09-24 review round 3): `week_tasks`/`list_pending_proposals`/
/// `precheck` below are three SEPARATE reads/writes, not one transaction —
/// a task could be added to W, or another proposal could land, between
/// them. This is deliberately tolerated: the worst a stale read here can do
/// is (a) skip producing a proposal for one run that a fully up-to-date view
/// would have produced (the NEXT trigger tries again), or (b) let through a
/// duplicate pending proposal `ai::propose`'s own ladder would otherwise
/// have avoided (harmless clutter, not wrong data — `AiSink::submit`'s own
/// dry run and `accept`'s own validate both re-check the CURRENT state
/// under a real write lock regardless of what dedup decided).
// `pub(super)` (2026-09-24 review round 4, M-b): exercised directly from
// `http::tests` so a test can inspect the computed `ProposeDedup` itself
// (`excluded_create_direction_ids`/`skip_create`) — `model = None`'s
// production path can never OBSERVE the difference between "blanket
// skipped" and "one direction excluded, another still offered" (reflex
// never creates either way), so the only way to test this function's own
// per-direction logic is to call it directly.
pub(super) async fn dedup_propose(
    store: &Sin90Store,
    week: &Week,
) -> Result<ProposeDedup, StoreError> {
    use crate::ai::AiReadModel;

    let reader = store.ai_reader();
    let week_tasks_all: Vec<Task> = reader
        .week_tasks(&week.id)
        .await
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let week_tasks_non_terminal: Vec<Task> = week_tasks_all
        .iter()
        .filter(|t| !crate::core::task_is_terminal(t.status))
        .cloned()
        .collect();
    let current_non_terminal_ids: HashSet<TaskId> = week_tasks_non_terminal
        .iter()
        .map(|t| t.id.clone())
        .collect();

    let ai_produced_ids = store
        .list_ai_produced_proposal_ids(Capability::Propose.as_str())
        .await?;
    let pending: Vec<_> = store
        .list_pending_proposals()
        .await?
        .into_iter()
        .filter(|p| ai_produced_ids.contains(&p.id))
        .collect();

    // Partition into propose's three shapes for THIS week; everything else
    // (classify's `AssignTaskDirection`, a carry/reorder/create for a
    // DIFFERENT week — neither can even reach here after the AI-produced
    // filter above, but the shape check stays for clarity) is simply not
    // propose's dedup concern.
    let mut carry_drafts: Vec<(ProposalDraft, HashSet<TaskId>)> = Vec::new();
    let mut reorder_drafts: Vec<(ProposalDraft, HashSet<TaskId>)> = Vec::new();
    let mut create_drafts: Vec<(ProposalDraft, Vec<crate::core::DirectionId>)> = Vec::new();

    for p in pending {
        if p.ops.is_empty() {
            continue;
        }
        let is_carry_batch_for_week = p
            .ops
            .iter()
            .all(|op| matches!(op, Sin90Op::CarryOverTask { to_week, .. } if to_week == &week.id));
        if is_carry_batch_for_week {
            let task_ids: HashSet<TaskId> = p
                .ops
                .iter()
                .filter_map(|op| match op {
                    Sin90Op::CarryOverTask { task_id, .. } => Some(task_id.clone()),
                    _ => None,
                })
                .collect();
            carry_drafts.push((
                ProposalDraft {
                    id: p.id,
                    ops: p.ops,
                    rationale: p.rationale,
                },
                task_ids,
            ));
            continue;
        }
        if let [Sin90Op::ReorderTasks { week_id, order }] = p.ops.as_slice() {
            if week_id == &week.id {
                let order_set: HashSet<TaskId> = order.iter().cloned().collect();
                reorder_drafts.push((
                    ProposalDraft {
                        id: p.id,
                        ops: p.ops.clone(),
                        rationale: p.rationale,
                    },
                    order_set,
                ));
                continue;
            }
        }
        if let [Sin90Op::CreateTasks { week_id, tasks }] = p.ops.as_slice() {
            if week_id == &week.id {
                let direction_ids: Vec<crate::core::DirectionId> = tasks
                    .iter()
                    .filter_map(|t| t.direction_id.clone())
                    .collect();
                create_drafts.push((
                    ProposalDraft {
                        id: p.id,
                        ops: p.ops.clone(),
                        rationale: p.rationale,
                    },
                    direction_ids,
                ));
            }
        }
    }

    // T5.7.2 (design §2 #31): this used to `return Ok(ProposeDedup::default())`
    // here when no PENDING draft matched — but the REJECTED half below must
    // still run even when there is no pending proposal at all (the common
    // case once something gets rejected rather than left pending), so this
    // early exit is gone; every set below simply stays empty and every loop
    // over an empty `Vec`/precheck over an empty batch is a cheap no-op.

    // One batch precheck across everything found — one write-lock
    // acquisition, not one per draft (§11.4 公共's own "去重" wording: "由
    // AiSink::precheck 批量完成").
    let mut all_drafts: Vec<ProposalDraft> =
        Vec::with_capacity(carry_drafts.len() + reorder_drafts.len() + create_drafts.len());
    all_drafts.extend(carry_drafts.iter().map(|(d, _)| d.clone()));
    all_drafts.extend(reorder_drafts.iter().map(|(d, _)| d.clone()));
    all_drafts.extend(create_drafts.iter().map(|(d, _)| d.clone()));
    // L-6 (2026-09-24 review round 3): if `precheck` cannot even get the
    // write lock, it fails OPEN — `AiSink::precheck`'s own implementation
    // returns `vec![false; drafts.len()]` on a failed `BEGIN IMMEDIATE`
    // (`store::ai_port`), i.e. "treat every draft as no-longer-valid" —
    // same posture `ai_classify::dedup_targets` already has for its own
    // precheck call: a transient lock contention degrades to "produce a
    // possibly-redundant proposal this run", never to "silently skip
    // producing anything at all".
    // T5.7.2: skip the write-lock acquisition entirely when there is
    // nothing PENDING to precheck (the common case, now that the early
    // `return` above is gone) — `precheck` on an empty batch would still
    // take `BEGIN IMMEDIATE` just to iterate zero times.
    let valid = if all_drafts.is_empty() {
        Vec::new()
    } else {
        AiSink::precheck(store, Capability::Propose, &all_drafts).await
    };
    let mut valid = valid.into_iter();

    let mut excluded_carry_task_ids = HashSet::new();
    for (_, task_ids) in &carry_drafts {
        if valid.next().unwrap_or(false) {
            excluded_carry_task_ids.extend(task_ids.iter().cloned());
        }
    }
    // §11.4.3's own design clarification ("T5.4.1 实现时补"): a pending
    // reorder only counts as "still valid" for dedup purposes if its `order`
    // covers the SAME set as W's current non-terminal tasks — multi one,
    // missing one, both disqualify it. `precheck`'s dry run alone would also
    // pass a PARTIAL reorder (it only checks referenced tasks belong to the
    // week), which is not what "this reorder is still the right answer"
    // means once the week's task set has moved on. This is a SET comparison
    // only — a task that changed STATUS (e.g. `planned` → `in_progress`)
    // without leaving the non-terminal set does not by itself invalidate a
    // pending reorder that still names it; that is intentionally accepted
    // (§11.4.3), not a gap.
    // Low (2026-09-26 review): once `skip_reorder` is already decided,
    // further reorder_drafts can't change it back — but `valid` is a SINGLE
    // shared iterator, positionally aligned with `all_drafts` (carry, THEN
    // reorder, THEN create); a bare `break` here would leave any remaining
    // reorder_drafts' precheck results undrained and misalign every create
    // draft's `valid.next()` below. So `valid.next()` is drained for EVERY
    // reorder draft unconditionally first (`reorder_valid`, order preserved),
    // and only the decision loop over the now-decoupled results gets to
    // `break` early.
    let reorder_valid: Vec<bool> = reorder_drafts
        .iter()
        .map(|_| valid.next().unwrap_or(false))
        .collect();
    let mut skip_reorder = false;
    for (ok, (_, order_set)) in reorder_valid.iter().zip(&reorder_drafts) {
        if *ok && order_set == &current_non_terminal_ids {
            skip_reorder = true;
            break;
        }
    }
    // M-b (2026-09-24 review round 4): create's dedup is PER-DIRECTION, like
    // carry's own per-task exclusion — NOT the earlier "every Direction the
    // draft names must still be a gap, or the WHOLE draft doesn't count"
    // (`.all()` over one draft's directions treated the pair {A, B} as one
    // indivisible unit: if a task under B got created outside propose and B
    // stopped being a gap, the OLD code let A — still genuinely uncovered —
    // through unprotected, since the draft as a whole failed the `.all()`).
    // Now: for EACH pending create's direction ids, whichever ONE of them is
    // STILL a current gap gets excluded from `g_candidates` individually
    // (`run_propose` applies the exclusion to a fresh read); a direction
    // that already stopped being a gap for its own reasons was never a
    // candidate to begin with, so excluding it is a no-op either way.
    // `skip_create` (the blanket L-4 hint) is set only once EVERY current
    // gap ends up excluded (§11.4.3: "剔完为空才整类跳过") — an `.all()`
    // over the FULL current-gap set, not over one draft's own directions.
    let mut excluded_create_direction_ids = HashSet::new();
    let mut skip_create = false;
    if !create_drafts.is_empty() {
        // L-b (2026-09-24 review round 4): a failed read here degrades to
        // "treat as no known gaps" (same posture every other read in this
        // function already has) — logged, not silent, so an operator can
        // still notice "dedup's create half is flying blind this run".
        let alloc = reader.rhythm_alloc().await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "propose: dedup rhythm_alloc read failed, treating as empty");
            Vec::new()
        });
        let current_gap_ids: HashSet<crate::core::DirectionId> =
            crate::ai::propose::gap_directions(&reader, &alloc, &week_tasks_all)
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "propose: dedup gap_directions read failed, treating as empty");
                    Vec::new()
                })
                .into_iter()
                .map(|g| g.direction_id)
                .collect();
        for (_, direction_ids) in &create_drafts {
            let ok = valid.next().unwrap_or(false);
            if ok {
                for d in direction_ids {
                    if current_gap_ids.contains(d) {
                        excluded_create_direction_ids.insert(d.clone());
                    }
                }
            }
        }
        skip_create = !current_gap_ids.is_empty()
            && current_gap_ids
                .iter()
                .all(|g| excluded_create_direction_ids.contains(g));
    }

    // T5.7.2 (design §2 #31): fold REJECTED propose proposals into the SAME
    // exclusion sets the PENDING half above just built — same per-item
    // granularity (`(task_id, to_week)` for carry, `(week_id, order)` for
    // reorder, `(week_id, direction_id)` for create), gated by whether a new
    // non-terminal Direction has appeared since that rejection (§2 #31: the
    // ONLY leg propose's own suppression uses — no single "target task" to
    // compare `updated_at` against for reorder/create, and carry shares the
    // same check for consistency; see the design entry for the full
    // reasoning). A rejected suggestion that's STILL suppressed simply
    // reduces the candidate set this run considers, same as an
    // already-covered-by-a-pending-proposal one does — it does not get its
    // own distinct wire `reason` (§2 #31's own documented scope limit).
    let rejected = store
        .list_rejected_ops(Capability::Propose.as_str())
        .await?;
    if !rejected.is_empty() {
        let max_new_direction = reader
            .max_eligible_direction_created_at()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        // T5.7.2 review round 2 (M2): the time basis is the rejected
        // proposal's OWN `proposed_at`, not `rejected_at` — same reasoning
        // `http::ai_classify::dedup_targets`'s own M2 fix gives: a proposal
        // a human sat on for a while before rejecting can have a genuinely
        // new Direction appear DURING that gap, which must already count as
        // "changed" rather than waiting for the (later) rejection moment.
        let situation_unchanged = |proposed_at: &str| {
            max_new_direction
                .as_deref()
                .is_none_or(|c| c <= proposed_at)
        };
        // Re-derive `current_gap_ids` only if some rejected `CreateTasks`
        // actually needs it — avoids the extra `gap_directions` read on a
        // propose run with no create-shaped rejection at all.
        let mut current_gap_ids: Option<HashSet<crate::core::DirectionId>> = None;
        // Same lazy-cache posture for reorder's own `rhythm_alloc` read
        // (below) — avoids the read entirely on a propose run with no
        // reorder-shaped rejection at all, and re-reading it once covers
        // every rejected reorder this loop happens to see.
        let mut current_alloc: Option<Vec<crate::core::Alloc>> = None;
        for r in &rejected {
            if r.ops.is_empty() || !situation_unchanged(&r.proposed_at) {
                continue;
            }
            // Mirrors the PENDING half's own `is_carry_batch_for_week`
            // shape (`submit_one`/`run_propose` submit every `CarryOverTask`
            // this run decided on as ONE proposal, not one op each) — a
            // batch counts only if EVERY op in it is a carry into THIS week.
            let is_carry_batch_for_week = r.ops.iter().all(
                |op| matches!(op, Sin90Op::CarryOverTask { to_week, .. } if to_week == &week.id),
            );
            if is_carry_batch_for_week {
                for op in &r.ops {
                    if let Sin90Op::CarryOverTask { task_id, .. } = op {
                        excluded_carry_task_ids.insert(task_id.clone());
                    }
                }
            }
            if let [Sin90Op::ReorderTasks { week_id, order }] = r.ops.as_slice() {
                if week_id == &week.id {
                    // 2026-09-26 external review (blocking): the OLD
                    // judgement here was "task set unchanged" + `task_
                    // modified_since` (M3) — but `reorder_reflex` (`ai::
                    // propose`) ranks by `status_tier` THEN by the task's
                    // Direction's rhythm-alloc `pct`, and `task_modified_
                    // since` deliberately EXCLUDES `direction_assigned`
                    // events (correct for its OTHER caller — see that
                    // method's own doc — but blind here): reassigning a
                    // task's Direction, or a quota change on `sin90_
                    // rhythms`, changes reflex's ranking WITHOUT tripping
                    // either the set-coverage check or `task_modified_
                    // since`. Fixed by asking reflex itself, right now:
                    // recompute `reorder_reflex` over the CURRENT
                    // non-terminal set and the CURRENT quota, and only keep
                    // suppressing if that recomputed order is IDENTICAL to
                    // the rejected `order`. This subsumes the old set-
                    // coverage check for free — `reorder_reflex` only ever
                    // returns a full permutation of `week_tasks_non_
                    // terminal`, so a task added to/removed from the
                    // current non-terminal set already makes the two
                    // `Vec<TaskId>` different lengths, let alone content —
                    // and a plain status change (M3's own `transitioned`
                    // leg) still lifts it too, since `status_tier` is
                    // reflex's OWN primary sort key. The `task_modified_
                    // since` leg is gone: content edits that leave both
                    // status and Direction (hence `pct`) untouched cannot
                    // change reflex's output either, so there is nothing
                    // left for that leg to catch that this one doesn't
                    // already subsume.
                    let alloc = match &current_alloc {
                        Some(a) => a,
                        None => {
                            let a = reader.rhythm_alloc().await.unwrap_or_else(|e| {
                                tracing::warn!(error = %e, "propose: dedup (rejected reorder) rhythm_alloc read failed, treating as empty");
                                Vec::new()
                            });
                            current_alloc = Some(a);
                            current_alloc.as_ref().unwrap()
                        }
                    };
                    let recomputed_order =
                        crate::ai::propose::reorder_reflex(&week_tasks_non_terminal, alloc);
                    if &recomputed_order == order {
                        skip_reorder = true;
                    }
                }
            }
            if let [Sin90Op::CreateTasks { week_id, tasks }] = r.ops.as_slice() {
                if week_id == &week.id {
                    let gaps = match &current_gap_ids {
                        Some(g) => g,
                        None => {
                            let alloc = reader.rhythm_alloc().await.unwrap_or_else(|e| {
                                tracing::warn!(error = %e, "propose: dedup (rejected) rhythm_alloc read failed, treating as empty");
                                Vec::new()
                            });
                            let g: HashSet<crate::core::DirectionId> =
                                crate::ai::propose::gap_directions(&reader, &alloc, &week_tasks_all)
                                    .await
                                    .unwrap_or_else(|e| {
                                        tracing::warn!(error = %e, "propose: dedup (rejected) gap_directions read failed, treating as empty");
                                        Vec::new()
                                    })
                                    .into_iter()
                                    .map(|g| g.direction_id)
                                    .collect();
                            current_gap_ids = Some(g);
                            current_gap_ids.as_ref().unwrap()
                        }
                    };
                    for t in tasks {
                        if let Some(d) = &t.direction_id {
                            if gaps.contains(d) {
                                excluded_create_direction_ids.insert(d.clone());
                            }
                        }
                    }
                }
            }
        }
        if let Some(gaps) = &current_gap_ids {
            skip_create = !gaps.is_empty()
                && gaps
                    .iter()
                    .all(|g| excluded_create_direction_ids.contains(g));
        }
    }

    Ok(ProposeDedup {
        excluded_carry_task_ids,
        skip_reorder,
        excluded_create_direction_ids,
        skip_create,
    })
}
