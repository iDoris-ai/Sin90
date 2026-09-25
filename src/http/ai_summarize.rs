//! `POST /ai/summarize` (T5.3.1, design §11.4 公共 + §11.4.2): validates the
//! target review synchronously (the only checks that can still turn into a
//! `404`/`400`/`409`), claims the single-flight slot, and — inside the
//! background task, AFTER the slot is secured (same M3/H3 ordering
//! `ai_classify`/`ai_propose` already establish) — computes the dedup
//! decision and spawns [`crate::ai::summarize::run_summarize`]. The actual
//! summarize logic lives there; this file is only the HTTP-shaped glue
//! (`require_any_actor`, `202`/`400`/`404`/`409`, the dedup query
//! `ai::summarize` cannot do itself since `Sin90Store::list_pending_
//! proposals` is not part of `AiReadModel`/`AiSink`'s deliberately narrow
//! surface, and wiring the run into the shared [`super::ai_runs`] registry)
//! — same division of labor `http::ai_classify`/`http::ai_propose` already
//! established for their own capabilities.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::ai::summarize::{self, SummarizeInputError, SummarizeItemResult};
use crate::ai::{AiSink, Capability, ModelAccess, NoModelPort, ProposalDraft};
use crate::core::Sin90Op;
use crate::store::{Sin90Store, StoreError};

use super::ai_classify::EmittingSink;
use super::ai_runs::{lock_registry, AiRunItem, BusyGuard};
use super::state::Sin90State;
use super::{error_response, parse};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SummarizeReq {
    review_id: String,
}

/// `POST /ai/summarize {"review_id"}` → `202 {"run_id", "capability"}`;
/// `409 {"code": "ai_busy", "run_id"}` if summarize already has a run in
/// flight, or `409 {"code": "conflict"}` if the review is not a draft;
/// `404` if `review_id` does not exist; `400 {"code": "unsupported_kind"}`
/// if the review is not `kind = weekly`. `require_any_actor` (design §11.4
/// 公共): triggering only writes `sin90_proposals`/`sin90_ai_calls`, the
/// same actor gate `POST /proposals` uses, not `require_human`.
pub async fn trigger_summarize(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(r) = state.require_any_actor(&headers) {
        return r;
    }
    let req: SummarizeReq = match parse(&body, "ai summarize request") {
        Ok(b) => b,
        Err(r) => return r,
    };

    // §11.4.2's "输入" check happens synchronously, BEFORE the single-flight
    // slot is claimed — an unknown/wrong-kind/non-draft review must never
    // turn into a 202 that silently does nothing.
    let reader = state.store.ai_reader();
    let review = match summarize::select_review(&reader, &req.review_id).await {
        Ok(r) => r,
        Err(e) => return summarize_input_error(e),
    };

    let run_id = format!("run-{}", crate::core::ulid());
    {
        let mut reg = lock_registry(&state.ai_runs);
        if let Some(existing) = reg.busy_run(Capability::Summarize) {
            return (
                StatusCode::CONFLICT,
                Json(json!({"code": "ai_busy", "run_id": existing})),
            )
                .into_response();
        }
        reg.start(Capability::Summarize, &run_id);
    }
    let guard = BusyGuard::new(state.ai_runs.clone(), Capability::Summarize, run_id.clone());

    let bg_state = state.clone();
    let rid = run_id.clone();
    tokio::spawn(async move {
        let store = &bg_state.store;
        let reader = store.ai_reader();

        // §11.4 公共's "去重" (computed HERE, after the slot is secured —
        // same M3/H3 timing `ai_classify`/`ai_propose` already use): a
        // target about to be skipped anyway should not pay for an extra
        // round trip before the slot claim, and the trigger itself must not
        // race a background run that is about to discover the same thing.
        let skip = match dedup_summarize(store, &review.id).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, run_id = %rid, "summarize: dedup query failed; running WITHOUT dedup for this run");
                false
            }
        };

        let item = if skip {
            AiRunItem {
                target: review.id.clone(),
                result: "skipped".to_string(),
                // M2 (2026-09-26 review): this run never even tried — a
                // still-valid pending proposal already covers this review.
                reason: Some("dedup"),
            }
        } else {
            // TODO(T5.1.2): hardcoded `None` until the real `ModelPort`
            // adapter (`src/adapter_agent24/clients/model.rs`) is wired up —
            // mirrors `ai_classify::trigger_classify`'s own TODO.
            let model: Option<&NoModelPort> = None;
            // M2-style (mirrors `ai_classify`/`ai_propose`'s own H2/M2 fix):
            // `EmittingSink` mirrors `proposal.submitted` IMMEDIATELY inside
            // `submit`, reused verbatim — not a second implementation.
            let emitting = EmittingSink {
                store,
                sink: bg_state.sink.clone(),
            };
            let result = summarize::run_summarize(
                &rid,
                &review,
                // TODO(T5.1.2): hardcoded until the real `ModelPort` adapter
                // and `domain-os.yml`'s `model_access` are wired up — this
                // trigger route cannot request `RemoteAllowed` today
                // regardless of the installed manifest (mirrors
                // `ai_classify`/`ai_propose`'s own TODO).
                ModelAccess::LocalOnly,
                model,
                &emitting,
                &reader,
            )
            .await;
            // M2: `Skipped` (Q7's human-text gate) is the only result this
            // capability can produce that carries a "why" worth naming —
            // every other result maps to a `None` reason.
            let reason = matches!(result, SummarizeItemResult::Skipped).then_some("human_text");
            AiRunItem {
                target: review.id.clone(),
                result: item_result_str(&result).to_string(),
                reason,
            }
        };

        let aborted = item.result == "aborted";
        let final_state = if aborted { "aborted" } else { "done" };
        guard.finish(final_state, vec![item]);
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({"run_id": run_id, "capability": "summarize"})),
    )
        .into_response()
}

fn item_result_str(r: &SummarizeItemResult) -> &'static str {
    match r {
        SummarizeItemResult::Proposed(_) => "proposed",
        SummarizeItemResult::Nothing => "nothing",
        SummarizeItemResult::Deferred => "deferred",
        SummarizeItemResult::Rejected => "rejected",
        SummarizeItemResult::Aborted => "aborted",
        // Q7/J19 (design §11.4.2's "可改写条件"): human text under EITHER
        // program rendering — same wire word `ai_classify`/`ai_propose`'s
        // own dedup-skip already uses (§11.4 公共's `result` domain has one
        // "skipped" value, not a reason-qualified one; `AiRunItem::reason`,
        // set at this function's call site, carries the "why").
        SummarizeItemResult::Skipped => "skipped",
        // 2026-09-26 review (Low): the read itself failed — mapped to the
        // closed vocabulary's `"aborted"` (see `SummarizeItemResult::
        // ReadFailed`'s own doc for why), logged here so an operator can
        // still find the real reason without it changing the wire shape.
        SummarizeItemResult::ReadFailed(msg) => {
            tracing::error!(error = %msg, "summarize: weekly_draft read failed, reporting as aborted");
            "aborted"
        }
    }
}

fn summarize_input_error(e: SummarizeInputError) -> Response {
    match e {
        SummarizeInputError::UnknownReview(id) => error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            &format!("review {id} does not exist"),
        ),
        SummarizeInputError::UnsupportedKind(kind) => error_response(
            StatusCode::BAD_REQUEST,
            "unsupported_kind",
            &format!("summarize only supports weekly reviews, got {kind:?}"),
        ),
        SummarizeInputError::NotDraft(id, status) => error_response(
            StatusCode::CONFLICT,
            "conflict",
            &format!("review {id} is not a draft (status {status:?})"),
        ),
        SummarizeInputError::ReadFailed(msg) => {
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", &msg)
        }
    }
}

/// §11.4 公共's "去重", generic form (design §11.4.2 gives summarize no
/// capability-specific refinement, unlike classify/propose's own
/// sections — M4, 2026-09-26 review: summarize's `DraftReviewBody` is a
/// whole-body CAS replace, so a second proposal against the SAME base is
/// guaranteed to hit `StaleBase` at accept time regardless of source; there
/// is no scenario where "only block AI-produced ones" would let a human's
/// own pending rewrite and an AI one coexist usefully — so unlike
/// `ai_propose::dedup_propose`'s "AI-produced only" narrowing, ANY
/// still-valid pending `DraftReviewBody` proposal for this review blocks a
/// new one, the same posture `ai_classify::dedup_targets` already has for
/// `AssignTaskDirection`). Lives here, not `ai::summarize`, because it needs
/// `Sin90Store::list_pending_proposals` — a plain store method, not part of
/// `AiReadModel`/`AiSink`'s deliberately narrow surface (design §11.5).
/// `pub(crate)`: exercised directly from `http::tests` without standing up a
/// full background run (mirrors `ai_classify::dedup_targets`'s own
/// visibility).
pub(crate) async fn dedup_summarize(
    store: &Sin90Store,
    review_id: &str,
) -> Result<bool, StoreError> {
    let pending = store.list_pending_proposals().await?;
    let mut drafts = Vec::new();
    for p in pending {
        if let [Sin90Op::DraftReviewBody { review_id: rid, .. }] = p.ops.as_slice() {
            if rid == review_id {
                drafts.push(ProposalDraft {
                    id: p.id,
                    ops: p.ops,
                    rationale: p.rationale,
                });
            }
        }
    }
    if drafts.is_empty() {
        return Ok(false);
    }
    let valid = AiSink::precheck(store, Capability::Summarize, &drafts).await;
    Ok(valid.into_iter().any(|ok| ok))
}
