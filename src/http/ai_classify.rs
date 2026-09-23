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

use crate::ai::classify::{self, ClassifyInputError, ItemResult};
use crate::ai::{AiSink, Capability, ModelAccess, NoModelPort, ProposalDraft};
use crate::core::{ProposalStatus, Sin90Op, Task, TaskId};
use crate::store::{Sin90Store, StoreError};

use super::ai_runs::{AiRunItem, BusyGuard};
use super::state::Sin90State;
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
        let mut reg = state.ai_runs.lock().unwrap_or_else(|p| p.into_inner());
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

        let model: Option<&NoModelPort> = None; // T5.1.2: no real adapter wired yet.
        let outcomes = classify::run_classify(
            &rid,
            &targets,
            ModelAccess::LocalOnly,
            model,
            store,
            &reader,
        )
        .await;

        // H2 (design §11.4 公共's L1): mirror `proposal.submitted` for every
        // proposal the run actually produced — same shape `POST /proposals`
        // itself emits, from the layer that holds the `EventSink`, not `ai/`.
        for outcome in &outcomes {
            if let ItemResult::Proposed(id) = &outcome.result {
                bg_state.emit("proposal.submitted", json!({ "id": id }));
            }
        }

        let aborted = outcomes.iter().any(|o| o.result == ItemResult::Aborted);
        let mut items: Vec<AiRunItem> = skipped
            .into_iter()
            .map(|task_id| AiRunItem {
                target: task_id,
                result: "skipped".to_string(),
            })
            .collect();
        items.extend(outcomes.into_iter().map(|o| AiRunItem {
            target: o.task_id,
            result: item_result_str(&o.result).to_string(),
        }));

        let final_state = if aborted { "aborted" } else { "done" };
        guard.finish(final_state, items);
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({"run_id": run_id, "capability": "classify"})),
    )
        .into_response()
}

fn item_result_str(r: &ItemResult) -> &'static str {
    match r {
        ItemResult::Proposed(_) => "proposed",
        ItemResult::Nothing => "nothing",
        ItemResult::Deferred => "deferred",
        ItemResult::Rejected => "rejected",
        // Design's item-result vocabulary is `proposed|nothing|deferred|
        // rejected|skipped` — an item that never ran because the run aborted
        // maps to "skipped" (it was, from the item's point of view, skipped).
        ItemResult::Aborted => "skipped",
    }
}

fn classify_input_error(e: ClassifyInputError) -> Response {
    match e {
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
/// plenty of untouched ones further back).
async fn auto_select_targets(store: &Sin90Store, reader: &crate::store::AiReader) -> Vec<Task> {
    use crate::ai::AiReadModel;
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
        let (kept, _skipped) = match dedup_targets(store, &candidates).await {
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
/// successfully ("仍然有效"). Lives here, not in `ai::classify`, because it
/// needs `Sin90Store::list_pending_proposals` — a plain store method, not
/// part of `AiReadModel`/`AiSink`'s deliberately narrow surface (design
/// §11.5). `pub(crate)` (not private): exercised directly from
/// `http::tests` (J14) without standing up a full background run.
pub(crate) async fn dedup_targets(
    store: &Sin90Store,
    targets: &[Task],
) -> Result<(Vec<Task>, Vec<TaskId>), StoreError> {
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
    if drafts.is_empty() {
        return Ok((targets.to_vec(), Vec::new()));
    }
    let valid = AiSink::precheck(store, Capability::Classify, &drafts).await;
    let skip: std::collections::HashSet<TaskId> = draft_task_ids
        .into_iter()
        .zip(valid)
        .filter_map(|(id, ok)| ok.then_some(id))
        .collect();
    let kept = targets
        .iter()
        .filter(|t| !skip.contains(&t.id))
        .cloned()
        .collect();
    let skipped = targets
        .iter()
        .filter(|t| skip.contains(&t.id))
        .map(|t| t.id.clone())
        .collect();
    Ok((kept, skipped))
}
