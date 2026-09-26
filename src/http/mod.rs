//! Sin90's business routes — the "http" layer of design §5.2.
//!
//! Depends on `core` + `store`. Does NOT know Agent24 exists: it only knows
//! "someone will send requests in" and "someone MAY collect emitted events"
//! (the [`EventSink`] trait below) — `adapter_agent24` is the someone, in the
//! shipped form; a `standalone` test harness is the other.
//!
//! Ported route-for-route from Agent24's `agent24-sin90-os` (design §1.4) —
//! the seven existing routes' handler bodies are unchanged; six are new (M0
//! §6): `/areas`, `/areas/{id}`, `/tasks`, `/tasks/{id}`, `/events`, plus
//! `/packs/install` (design §7.4, brought forward from M6 because the seed
//! data itself was trivial to wire once Area existed).
//!
//! New in this port: [`ActorKey`]-gated direct writes (design §7.1) — every
//! direct-write route requires the human actor key; only `POST /proposals`
//! accepts the automation key. `POST /proposals/{id}/accept` commits state,
//! so it requires the human key too — the automation key may propose but
//! never approve its own proposal (Codex 2026-09-22 review, High: this used
//! to accept either key, letting automation self-approve). This is enforced
//! HERE, in Sin90's own code, not delegated to Agent24 (design §7.1's
//! evaluation: the kernel has no AI-vs-human caller concept and should not
//! grow one for this).

pub mod actor;
mod ai_classify;
mod ai_propose;
pub mod ai_runs;
mod ai_summarize;
pub mod state;

use axum::body::Bytes;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::Json;
use serde::Deserialize;

use crate::core::{
    Alloc, AreaStatus, Energy, FireTrigger, NewReview, NewRoutine, ReviewKind, ReviewPatch,
    RoutinePatch, RoutineStatus, ScheduleBlockStatus, Sin90Proposal, TaskKind, TaskStatus,
    WeekStatus,
};
use crate::store::{AutoReviewCreated, RoutineFireOutcome, StoreError};

pub use actor::{Actor, ActorKeys};
pub use state::{
    EventSink, HttpModelPort, ModelCaller, NullEventSink, SemaphoredModelCaller, Sin90State,
    MODEL_MAX_IN_FLIGHT_PER_MODULE,
};

/// The v1 error envelope every handler below returns on failure — same shape
/// regardless of which layer produced the error, so a client cannot tell
/// "Sin90's own code" from "the transport wrapping it" apart.
#[derive(serde::Serialize)]
struct ErrorBody<'a> {
    error: ErrorDetail<'a>,
}
#[derive(serde::Serialize)]
struct ErrorDetail<'a> {
    code: &'a str,
    message: &'a str,
}

fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(ErrorBody {
            error: ErrorDetail { code, message },
        }),
    )
        .into_response()
}

/// Map a store error to an HTTP response. A FOREIGN KEY violation is a client
/// mistake (referenced a nonexistent entity) → 404, not the 500 a raw sqlx
/// error would otherwise become.
fn map_err(err: StoreError) -> Response {
    if err.is_fk_violation() {
        return error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            "a referenced entity does not exist",
        );
    }
    match err {
        StoreError::NotFound(m) => error_response(StatusCode::NOT_FOUND, "not_found", &m),
        StoreError::Transition(e) => {
            error_response(StatusCode::CONFLICT, "conflict", &e.to_string())
        }
        StoreError::Proposal(e) => error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unprocessable",
            &e.to_string(),
        ),
        StoreError::Conflict(m) => error_response(StatusCode::CONFLICT, "conflict", &m),
        StoreError::Invalid(m) => error_response(StatusCode::BAD_REQUEST, "invalid_request", &m),
        StoreError::WeekNotOpen(m) => error_response(
            StatusCode::CONFLICT,
            "conflict",
            &format!("task {m}'s week is not open"),
        ),
        StoreError::SameWeekCarry(m) => error_response(
            StatusCode::CONFLICT,
            "conflict",
            &format!("cannot carry task {m} into its own week"),
        ),
        StoreError::Internal(_)
        | StoreError::Sqlx(_)
        | StoreError::Migrate(_)
        | StoreError::Serde(_) => {
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", "store error")
        }
    }
}

#[allow(clippy::result_large_err)]
fn parse<T: for<'de> Deserialize<'de>>(
    bytes: &Bytes,
    what: &str,
) -> std::result::Result<T, Response> {
    serde_json::from_slice(bytes).map_err(|e| {
        error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            &format!("invalid {what}: {e}"),
        )
    })
}

/// Build the router. `state.actor_keys` gates direct writes (design §7.1);
/// `state.sink` is where emitted events go (design §5.2 — Sin90 does not know
/// who is on the other end of it).
///
/// `mounted` — T3.2.2, architecture.md #4 — is true only when this router
/// will be served behind Agent24's kernel proxy (`main.rs`'s
/// `run_as_agent24_module`, nested under `/api/v1/sin90`). `POST
/// /_a24/scheduler/fired` is registered ONLY in that case: the route's
/// trustworthiness comes entirely from the kernel proxy stripping any
/// client-forged `X-A24-*` headers before a request reaches here — the
/// `--standalone`/`serve` path (`main.rs`'s `run_standalone`) has no such
/// proxy in front of it, so a client could forge `X-A24-Fire-Id` directly.
/// Not registering the route there at all (404, not "route exists but
/// rejects") is the guard; a caller that got past a real Agent24 proxy is
/// the only one who can ever reach the handler.
pub fn router(state: Sin90State, mounted: bool) -> axum::Router {
    let mut r = axum::Router::new()
        .route("/areas", post(create_area).get(list_areas))
        .route("/areas/{id}", patch(transition_area))
        .route("/directions", post(create_direction).get(list_directions))
        .route("/tasks", post(create_task).get(list_tasks))
        .route("/tasks/{id}", patch(transition_task))
        .route("/schedule-blocks", post(create_block).get(list_blocks))
        .route("/schedule-blocks/{id}", patch(transition_block))
        .route("/weeks", post(create_week).get(list_weeks))
        .route("/weeks/{id}", patch(transition_week))
        .route("/weeks/{id}/attention", get(week_attention))
        .route("/rhythms", post(create_rhythm).get(list_rhythms))
        .route("/rhythms/{id}", get(get_rhythm))
        .route("/proposals", post(submit_proposal).get(list_proposals))
        .route("/proposals/{id}", get(get_proposal))
        .route("/proposals/{id}/accept", post(accept_proposal))
        .route("/proposals/{id}/reject", post(reject_proposal))
        .route("/attention", get(attention))
        .route("/events", get(list_events))
        .route("/packs/install", post(install_pack))
        .route("/capture", post(capture))
        .route("/today", get(today))
        .route("/routines", post(create_routine).get(list_routines))
        .route("/routines/{id}", get(get_routine).patch(update_routine))
        .route("/routines/{id}/transition", post(transition_routine))
        .route("/reviews", post(create_review).get(list_reviews))
        .route("/reviews/{id}", get(get_review).patch(update_review))
        .route("/reviews/{id}/finalize", post(finalize_review))
        .route("/review/weekly/draft", get(weekly_review_draft))
        .route("/settings/ai", get(get_ai_settings).put(put_ai_settings))
        // New (T5.2.1, design §11.4 公共): NOT under `/_a24/*` — registered in
        // both standalone and mounted mode ("standalone 模式同样注册（port
        // 为 `None`，只有 reflex）").
        .route("/ai/classify", post(ai_classify::trigger_classify))
        // New (T5.4.1, design §11.4.3): same posture as `/ai/classify` above
        // — not under `/_a24/*`, registered in both standalone and mounted
        // mode.
        .route("/ai/propose", post(ai_propose::trigger_propose))
        // New (T5.3.1, design §11.4.2): same posture as `/ai/classify`/
        // `/ai/propose` above.
        .route("/ai/summarize", post(ai_summarize::trigger_summarize))
        .route("/ai/runs/{run_id}", get(ai_runs::get_ai_run));
    if mounted {
        r = r.route("/_a24/scheduler/fired", post(scheduler_fired));
    }
    r.with_state(state)
}

// ---- request/query bodies (deny_unknown_fields: reject model typos loudly) --

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewAreaReq {
    title: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AreaTransitionReq {
    to: AreaStatus,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewDirectionReq {
    title: String,
    target_window: String,
    #[serde(default)]
    area_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewTaskReq {
    title: String,
    #[serde(default)]
    direction_id: Option<String>,
    #[serde(default)]
    parent_task_id: Option<String>,
    #[serde(default)]
    kind: Option<TaskKind>,
    #[serde(default)]
    energy: Option<Energy>,
    #[serde(default)]
    est_minutes: Option<u32>,
}

#[derive(Deserialize)]
struct TaskListQuery {
    direction_id: Option<String>,
    area_id: Option<String>,
    status: Option<TaskStatus>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskTransitionReq {
    to: TaskStatus,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewBlockReq {
    #[serde(default)]
    direction_id: Option<String>,
    #[serde(default)]
    task_id: Option<String>,
    planned_minutes: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BlockTransitionReq {
    to: ScheduleBlockStatus,
}

#[derive(Deserialize)]
struct AttentionQuery {
    start: String,
    end: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewWeekReq {
    iso_week: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WeekTransitionReq {
    to: WeekStatus,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureReq {
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewRhythmReq {
    allocations: Vec<Alloc>,
}

#[derive(Deserialize)]
struct EventsQuery {
    entity: Option<String>,
    entity_id: Option<String>,
    since_seq: Option<i64>,
    limit: Option<i64>,
}

#[derive(Deserialize)]
struct RoutineListQuery {
    status: Option<RoutineStatus>,
    area_id: Option<String>,
    direction_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoutineTransitionReq {
    to: RoutineStatus,
}

#[derive(Deserialize)]
struct ReviewListQuery {
    kind: Option<ReviewKind>,
    period: Option<String>,
}

/// `POST /_a24/scheduler/fired` body (T3.2.2). Field set and names match the
/// kernel's `FiredBody` exactly (Agent24 design doc
/// `ME4-S1-scheduler-callback.md` §5.3: `{key, trigger, scheduled_for,
/// fired_at}`) — `deny_unknown_fields` so a kernel-side field this module
/// doesn't know about yet fails loudly rather than being silently dropped.
/// `fired_at` is not persisted (spec.md M3's `sin90_routine_fires` has no
/// such column — only `received_at`, this module's OWN receive timestamp,
/// matters for `/today`'s day-boundary rule) but is still validated as a
/// fixed-width ISO-8601 timestamp below, same as `scheduled_for`, so a
/// malformed body fails fast as a 400 rather than being stored opaquely and
/// discovered wrong later.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FiredReq {
    key: String,
    scheduled_for: String,
    fired_at: String,
    trigger: FireTrigger,
}

// ---- Area handlers (new) ----------------------------------------------------

async fn create_area(State(state): State<Sin90State>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    let req: NewAreaReq = match parse(&body, "area") {
        Ok(b) => b,
        Err(r) => return r,
    };
    match state.store.create_area(&req.title).await {
        Ok(a) => {
            state.emit(
                "area.created",
                serde_json::json!({ "id": a.id, "title": a.title }),
            );
            (StatusCode::CREATED, Json(a)).into_response()
        }
        Err(e) => map_err(e),
    }
}

async fn list_areas(State(state): State<Sin90State>) -> Response {
    match state.store.list_areas().await {
        Ok(v) => Json(serde_json::json!({ "areas": v })).into_response(),
        Err(e) => map_err(e),
    }
}

async fn transition_area(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    AxPath(id): AxPath<String>,
    body: Bytes,
) -> Response {
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    let req: AreaTransitionReq = match parse(&body, "area transition") {
        Ok(b) => b,
        Err(r) => return r,
    };
    match state.store.transition_area(&id, req.to).await {
        Ok(a) => {
            state.emit(
                "area.transitioned",
                serde_json::json!({ "area_id": a.id, "to": a.status }),
            );
            Json(a).into_response()
        }
        Err(e) => map_err(e),
    }
}

// ---- Direction handlers (ported, area_id added) -----------------------------

async fn create_direction(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    let req: NewDirectionReq = match parse(&body, "direction") {
        Ok(b) => b,
        Err(r) => return r,
    };
    match state
        .store
        .create_direction(&req.title, &req.target_window, req.area_id.as_deref())
        .await
    {
        Ok(d) => {
            state.emit(
                "direction.created",
                serde_json::json!({ "id": d.id, "title": d.title, "area_id": d.area_id }),
            );
            (StatusCode::CREATED, Json(d)).into_response()
        }
        Err(e) => map_err(e),
    }
}

async fn list_directions(State(state): State<Sin90State>) -> Response {
    match state.store.list_directions().await {
        Ok(v) => Json(serde_json::json!({ "directions": v })).into_response(),
        Err(e) => map_err(e),
    }
}

// ---- Task handlers (new) -----------------------------------------------------

async fn create_task(State(state): State<Sin90State>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    let req: NewTaskReq = match parse(&body, "task") {
        Ok(b) => b,
        Err(r) => return r,
    };
    match state
        .store
        .create_task(
            &req.title,
            req.direction_id.as_deref(),
            req.parent_task_id.as_deref(),
            req.kind.unwrap_or(TaskKind::Other),
            req.energy.unwrap_or(Energy::Mid),
            req.est_minutes,
        )
        .await
    {
        Ok(t) => {
            state.emit(
                "task.created",
                serde_json::json!({ "id": t.id, "direction_id": t.direction_id, "parent_task_id": t.parent_task_id }),
            );
            (StatusCode::CREATED, Json(t)).into_response()
        }
        Err(e) => map_err(e),
    }
}

async fn list_tasks(State(state): State<Sin90State>, Query(q): Query<TaskListQuery>) -> Response {
    match state
        .store
        .list_tasks(q.direction_id.as_deref(), q.area_id.as_deref(), q.status)
        .await
    {
        Ok(v) => Json(serde_json::json!({ "tasks": v })).into_response(),
        Err(e) => map_err(e),
    }
}

async fn transition_task(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    AxPath(id): AxPath<String>,
    body: Bytes,
) -> Response {
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    let req: TaskTransitionReq = match parse(&body, "task transition") {
        Ok(b) => b,
        Err(r) => return r,
    };
    match state.store.transition_task(&id, req.to).await {
        Ok(t) => {
            state.emit(
                "task.transitioned",
                serde_json::json!({ "task_id": t.id, "to": t.status }),
            );
            Json(t).into_response()
        }
        Err(e) => map_err(e),
    }
}

// ---- ScheduleBlock handlers (ported, unchanged) -----------------------------

async fn create_block(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    let req: NewBlockReq = match parse(&body, "block") {
        Ok(b) => b,
        Err(r) => return r,
    };
    match state
        .store
        .create_block(
            req.direction_id.as_deref(),
            req.task_id.as_deref(),
            req.planned_minutes,
        )
        .await
    {
        Ok(b) => {
            state.emit(
                "block.created",
                serde_json::json!({ "block_id": b.id, "direction_id": b.direction_id }),
            );
            (StatusCode::CREATED, Json(b)).into_response()
        }
        Err(e) => map_err(e),
    }
}

async fn transition_block(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    AxPath(id): AxPath<String>,
    body: Bytes,
) -> Response {
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    let req: BlockTransitionReq = match parse(&body, "transition") {
        Ok(b) => b,
        Err(r) => return r,
    };
    match state.store.transition_block(&id, req.to).await {
        Ok(b) => {
            state.emit(
                "block.transitioned",
                serde_json::json!({ "block_id": b.id, "to": b.status }),
            );
            Json(b).into_response()
        }
        Err(e) => map_err(e),
    }
}

async fn list_blocks(State(state): State<Sin90State>) -> Response {
    match state.store.list_blocks().await {
        Ok(v) => Json(serde_json::json!({ "blocks": v })).into_response(),
        Err(e) => map_err(e),
    }
}

// ---- Week handlers (new, design M2) -----------------------------------------

/// `POST /weeks` — open a new week (`planning`). Direct write, human gate:
/// starting a week is a planning-ritual action, same convention as
/// `create_area`/`create_task` above, not something an automated intake
/// should be minting on its own.
async fn create_week(State(state): State<Sin90State>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    let req: NewWeekReq = match parse(&body, "week") {
        Ok(b) => b,
        Err(r) => return r,
    };
    match state.store.create_week(&req.iso_week).await {
        Ok(w) => {
            state.emit(
                "week.created",
                serde_json::json!({ "id": w.id, "iso_week": w.iso_week }),
            );
            (StatusCode::CREATED, Json(w)).into_response()
        }
        Err(e) => map_err(e),
    }
}

async fn list_weeks(State(state): State<Sin90State>) -> Response {
    match state.store.list_weeks().await {
        Ok(v) => Json(serde_json::json!({ "weeks": v })).into_response(),
        Err(e) => map_err(e),
    }
}

/// `PATCH /weeks/{id}` — `planning -> active -> reviewing -> closed`. Direct
/// write, human gate, same convention as `transition_area`/`transition_task`.
async fn transition_week(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    AxPath(id): AxPath<String>,
    body: Bytes,
) -> Response {
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    let req: WeekTransitionReq = match parse(&body, "week transition") {
        Ok(b) => b,
        Err(r) => return r,
    };
    match state.store.transition_week(&id, req.to).await {
        Ok(w) => {
            state.emit(
                "week.transitioned",
                serde_json::json!({ "week_id": w.id, "to": w.status }),
            );
            Json(w).into_response()
        }
        Err(e) => map_err(e),
    }
}

/// `GET /weeks/{id}/attention` — planned vs. actual for this week (design M2
/// acceptance line). No auth gate: a read commits nothing, same posture as
/// `GET /tasks`/`GET /today`.
async fn week_attention(State(state): State<Sin90State>, AxPath(id): AxPath<String>) -> Response {
    match state.store.week_attention(&id).await {
        Ok(a) => Json(a).into_response(),
        Err(e) => map_err(e),
    }
}

// ---- Rhythm handlers (new, design M3/T3.4.1) --------------------------------

/// `POST /rhythms` (spec.md "Rhythm 路由") — direct write, human gate, same
/// convention as `create_area`/`create_direction`/`create_week`. Adjusting an
/// existing Rhythm does NOT go through here: that stays behind the proposal
/// gate (`POST /proposals` submitting `AdjustRhythm`, then human
/// `POST /proposals/{id}/accept`) — this route only ever creates, and the
/// store layer enforces it (no direct-write adjust method exists).
async fn create_rhythm(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    let req: NewRhythmReq = match parse(&body, "rhythm") {
        Ok(b) => b,
        Err(r) => return r,
    };
    match state.store.create_rhythm(&req.allocations).await {
        Ok(rh) => {
            state.emit(
                "rhythm.created",
                serde_json::json!({ "id": rh.id, "allocations": rh.allocations }),
            );
            (StatusCode::CREATED, Json(rh)).into_response()
        }
        Err(e) => map_err(e),
    }
}

async fn list_rhythms(State(state): State<Sin90State>) -> Response {
    match state.store.list_rhythms().await {
        Ok(v) => Json(serde_json::json!({ "rhythms": v })).into_response(),
        Err(e) => map_err(e),
    }
}

async fn get_rhythm(State(state): State<Sin90State>, AxPath(id): AxPath<String>) -> Response {
    match state.store.get_rhythm(&id).await {
        Ok(r) => Json(r).into_response(),
        Err(e) => map_err(e),
    }
}

// ---- Proposal handlers (submit: either key; accept: human only) ------------

async fn submit_proposal(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Design §7.1: the Proposal gate is where the AUTOMATION key is accepted —
    // this is the one family of routes that key is good for.
    if let Err(r) = state.require_any_actor(&headers) {
        return r;
    }
    let proposal: Sin90Proposal = match parse(&body, "proposal") {
        Ok(b) => b,
        Err(r) => return r,
    };
    match state.store.submit_proposal(&proposal).await {
        Ok(()) => {
            state.emit(
                "proposal.submitted",
                serde_json::json!({ "id": proposal.id }),
            );
            (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({ "id": proposal.id, "status": "pending" })),
            )
                .into_response()
        }
        Err(e) => map_err(e),
    }
}

async fn accept_proposal(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    AxPath(id): AxPath<String>,
) -> Response {
    // Codex 2026-09-22 review (High): `require_any_actor` here let the
    // automation key submit AND accept its own proposal, making the
    // human-approval boundary (design §7.1) cosmetic. Accept commits state —
    // only a human may pull that trigger; automation may still submit.
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    match state.store.apply_proposal(&id).await {
        Ok(outcome) => {
            if outcome.applied_now {
                state.emit(
                    "proposal.applied",
                    serde_json::json!({ "proposal_id": outcome.receipt.proposal_id }),
                );
            }
            Json(outcome.receipt).into_response()
        }
        Err(e) => map_err(e),
    }
}

/// Optional body of `POST /proposals/{id}/reject` — `deny_unknown_fields`
/// (T5.7.1): a stray/mistyped key is a 400, same posture every other
/// request-body struct in this file takes. An entirely absent body (the
/// common case — no reason given) never reaches `serde_json`: an empty
/// `Bytes` is short-circuited to `reason: None` below, since
/// `serde_json::from_slice(b"")` would otherwise fail as a parse error, not
/// "no reason provided".
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RejectProposalReq {
    #[serde(default)]
    reason: Option<String>,
}

/// `reason`'s max length, in Unicode scalar values ("字符", not bytes) —
/// Opus review L2.
const MAX_REJECT_REASON_CHARS: usize = 1000;

/// L2 (Opus review): trims surrounding whitespace, treats a trim-to-empty
/// string the same as "no reason given" (`None`, not an empty string
/// persisted to the log) — a caller sending `{"reason": "   "}` almost
/// certainly means nothing, not an intentional empty note — and rejects
/// (400) a reason over [`MAX_REJECT_REASON_CHARS`] characters, before it
/// ever reaches the store.
#[allow(clippy::result_large_err)]
fn normalize_reject_reason(raw: Option<String>) -> std::result::Result<Option<String>, Response> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().count() > MAX_REJECT_REASON_CHARS {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            &format!("reason must be at most {MAX_REJECT_REASON_CHARS} characters"),
        ));
    }
    Ok(Some(trimmed.to_string()))
}

/// `POST /proposals/{id}/reject` (T5.7.1, design §2 #27, Q6): human-only,
/// same gate `accept_proposal` uses and for the same reason — reject commits
/// a terminal state change (`pending → rejected`), so only a human may pull
/// that trigger; automation may still submit (design §7.1). Every successful
/// reject also writes one append-only `sin90_proposal_rejections` row (the
/// store's job, `Sin90Store::reject_proposal`'s doc) and mirrors
/// `proposal.rejected` to `EventSink`, same "internal row + sink mirror"
/// split every other mutating handler in this file follows — both payloads
/// key the proposal id `proposal_id` (Opus review L1) and the mirror also
/// carries `ops_summary`, not just `capability_source`.
async fn reject_proposal(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    AxPath(id): AxPath<String>,
    body: Bytes,
) -> Response {
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    let reason = if body.is_empty() {
        None
    } else {
        match parse::<RejectProposalReq>(&body, "proposal reject") {
            Ok(b) => b.reason,
            Err(r) => return r,
        }
    };
    let reason = match normalize_reject_reason(reason) {
        Ok(r) => r,
        Err(r) => return r,
    };
    match state.store.reject_proposal(&id, reason.as_deref()).await {
        Ok(outcome) => {
            state.emit(
                "proposal.rejected",
                serde_json::json!({
                    "proposal_id": id,
                    "capability_source": outcome.capability_source,
                    "ops_summary": outcome.ops_summary,
                    "reason": reason,
                }),
            );
            Json(serde_json::json!({ "id": id, "status": "rejected" })).into_response()
        }
        Err(e) => map_err(e),
    }
}

async fn list_proposals(State(state): State<Sin90State>) -> Response {
    match state.store.list_proposals().await {
        Ok(v) => Json(serde_json::json!({ "proposals": v })).into_response(),
        Err(e) => map_err(e),
    }
}

async fn get_proposal(State(state): State<Sin90State>, AxPath(id): AxPath<String>) -> Response {
    match state.store.get_proposal(&id).await {
        Ok(p) => Json(p).into_response(),
        Err(e) => map_err(e),
    }
}

// ---- attention / events (ported / new) --------------------------------------

async fn attention(State(state): State<Sin90State>, q: Query<AttentionQuery>) -> Response {
    let Query(q) = q;
    if !crate::core::is_fixed_iso8601(&q.start) || !crate::core::is_fixed_iso8601(&q.end) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "start and end must be fixed-width ISO-8601 (YYYY-MM-DDThh:mm:ssZ)",
        );
    }
    if q.start >= q.end {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "start must be strictly before end",
        );
    }
    match state.store.attention(&q.start, &q.end).await {
        Ok(rows) => Json(serde_json::json!({ "attention": rows })).into_response(),
        Err(e) => map_err(e),
    }
}

async fn list_events(State(state): State<Sin90State>, Query(q): Query<EventsQuery>) -> Response {
    match state
        .store
        .list_events(
            q.entity.as_deref(),
            q.entity_id.as_deref(),
            q.since_seq,
            q.limit,
        )
        .await
    {
        Ok(rows) => Json(serde_json::json!({ "events": rows })).into_response(),
        Err(e) => map_err(e),
    }
}

async fn install_pack(State(state): State<Sin90State>, headers: HeaderMap) -> Response {
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    match state
        .store
        .install_seed_pack(&crate::store::five_life_systems())
        .await
    {
        Ok(areas) => {
            for a in &areas {
                state.emit(
                    "area.created",
                    serde_json::json!({ "id": a.id, "title": a.title }),
                );
            }
            (
                StatusCode::CREATED,
                Json(serde_json::json!({ "areas": areas })),
            )
                .into_response()
        }
        Err(e) => map_err(e),
    }
}

// ---- Capture / Today (design M1) --------------------------------------------

/// `POST /capture` — a raw, unclassified note (design M1). Lands as an
/// ordinary `direction_id = NULL` `Task` (no new entity, per M1's own
/// acceptance line) via the same `create_task` path `POST /tasks` uses, just
/// with every optional field left at its default.
///
/// Gate: `require_any_actor`, not `require_human`. This is a deliberate
/// departure from every other direct-write route above. Capture is the one
/// direct write this design treats as low-stakes enough for either actor:
/// it commits nothing (no `direction_id`, no plan, no state machine edge —
/// it can only ever be sitting in `backlog`), and a future automated
/// intake (a script watching some inbox) capturing a raw note is exactly
/// the kind of provisional, needs-a-human-later action `Sin90Proposal`
/// exists for in spirit, if not literally through that table. The line
/// design §7.1 actually draws is "AI must not commit state a human hasn't
/// reviewed" — an inbox item a human still has to triage, categorize, and
/// promote out of `backlog` before it does anything, has not had state
/// committed to it in that sense.
async fn capture(State(state): State<Sin90State>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(r) = state.require_any_actor(&headers) {
        return r;
    }
    let req: CaptureReq = match parse(&body, "capture") {
        Ok(b) => b,
        Err(r) => return r,
    };
    match state
        .store
        .create_task(&req.text, None, None, TaskKind::Other, Energy::Mid, None)
        .await
    {
        Ok(t) => {
            state.emit(
                "task.created",
                serde_json::json!({ "id": t.id, "direction_id": t.direction_id, "parent_task_id": t.parent_task_id }),
            );
            (StatusCode::CREATED, Json(t)).into_response()
        }
        Err(e) => map_err(e),
    }
}

/// `GET /today` (design M1) — a pure read composed of four independent
/// queries; see [`crate::store::Sin90Store::today_view`] for the selection
/// rule behind each section. No auth gate: reading the view commits nothing,
/// same posture as `GET /tasks` / `GET /areas` above.
async fn today(State(state): State<Sin90State>) -> Response {
    match state.store.today_view().await {
        Ok(view) => Json(view).into_response(),
        Err(e) => map_err(e),
    }
}

// ---- Routine handlers (new, M3, design §2 #6, §3.2, T3.1.2) ----------------
//
// `create`/`update`/`transition` are direct writes, human-gated, same
// convention as Area/Task/Week above (no `Sin90Op::CreateRoutine` proposal
// variant exists — see `store::repo`'s module doc for why). `GET /routines`
// and `GET /routines/{id}` are reads, no gate, same posture as `GET /tasks`.
// Every successful write also emits via `EventSink` — the store already
// appends its own `sin90_events` row inside the same transaction; this is a
// SEPARATE mirror out to whoever is on the other end of `state.sink`
// (Agent24's kernel, in the adapter form) — the same "two audiences, two
// writes" split every other direct-write handler above already follows.

/// `POST /routines` — body deserializes straight into [`NewRoutine`]
/// (`deny_unknown_fields`, same convention as every other `New*Req`: a
/// stray/mistyped key must fail loudly, not be silently dropped).
async fn create_routine(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    let req: NewRoutine = match parse(&body, "routine") {
        Ok(b) => b,
        Err(r) => return r,
    };
    match state.store.create_routine(&req).await {
        Ok(routine) => {
            state.emit(
                "routine.created",
                serde_json::json!({
                    "id": routine.id, "area_id": routine.area_id,
                    "direction_id": routine.direction_id, "status": routine.status,
                }),
            );
            (StatusCode::CREATED, Json(routine)).into_response()
        }
        Err(e) => map_err(e),
    }
}

/// `GET /routines?status=&area_id=&direction_id=` — a read, no gate, same
/// posture as `GET /tasks`. Unrecognized query keys are silently ignored by
/// axum's `Query` extractor, same as every other list route above.
async fn list_routines(
    State(state): State<Sin90State>,
    Query(q): Query<RoutineListQuery>,
) -> Response {
    match state
        .store
        .list_routines(q.area_id.as_deref(), q.direction_id.as_deref(), q.status)
        .await
    {
        Ok(v) => Json(serde_json::json!({ "routines": v })).into_response(),
        Err(e) => map_err(e),
    }
}

/// `GET /routines/{id}` — a read, no gate. Unknown id -> 404 via `map_err`.
async fn get_routine(State(state): State<Sin90State>, AxPath(id): AxPath<String>) -> Response {
    match state.store.get_routine(&id).await {
        Ok(r) => Json(r).into_response(),
        Err(e) => map_err(e),
    }
}

/// `PATCH /routines/{id}` — body deserializes straight into [`RoutinePatch`]
/// (double-`Option` fields: absent key = leave unchanged, `null` = clear,
/// value = set — see `RoutinePatch`'s doc comment). A `retired` routine
/// rejects any patch with `StoreError::Conflict` -> 409 (`store::repo`'s H2).
///
/// Mirrors `routine.updated` to `EventSink` — but ONLY when
/// [`crate::store::RoutineUpdate::changed`] is non-empty (T3.1.2 review): a
/// no-op patch (absent fields, or ones re-stating the current values) writes
/// no internal `sin90_events` row either (`update_routine`'s L1), so the
/// mirror must stay silent too, one-for-one with the store. The mirrored
/// payload is the SAME shape the store's own `updated` event uses — the
/// full post-update snapshot plus a `"changed"` array — not an ad hoc field
/// list, so both audiences see the identical fact.
async fn update_routine(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    AxPath(id): AxPath<String>,
    body: Bytes,
) -> Response {
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    let patch: RoutinePatch = match parse(&body, "routine patch") {
        Ok(b) => b,
        Err(r) => return r,
    };
    match state.store.update_routine(&id, &patch).await {
        Ok(outcome) => {
            if !outcome.changed.is_empty() {
                let mut payload = match serde_json::to_value(&outcome.routine) {
                    Ok(v) => v,
                    Err(e) => return map_err(e.into()),
                };
                if let serde_json::Value::Object(map) = &mut payload {
                    map.insert("changed".to_string(), serde_json::json!(outcome.changed));
                }
                state.emit("routine.updated", payload);
            }
            Json(outcome.routine).into_response()
        }
        Err(e) => map_err(e),
    }
}

/// `POST /routines/{id}/transition` — `active <-> paused`,
/// `{active,paused} -> retired` (design §3.2); an illegal edge (including any
/// edge out of `retired`) comes back as `StoreError::Transition` -> 409, same
/// convention as `PATCH /tasks/{id}`/`PATCH /weeks/{id}` above.
///
/// The mirrored event kind is the SAME destination-specific name
/// `store::repo::transition_routine` uses internally (`routine.paused` /
/// `routine.resumed` / `routine.retired`, T3.1.2 review) — not a generic
/// `routine.transitioned` — and the payload matches the store's own ad hoc
/// `{"routine_id": id}` shape, so both audiences agree on both the event
/// name and its contents.
async fn transition_routine(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    AxPath(id): AxPath<String>,
    body: Bytes,
) -> Response {
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    let req: RoutineTransitionReq = match parse(&body, "routine transition") {
        Ok(b) => b,
        Err(r) => return r,
    };
    match state.store.transition_routine(&id, req.to).await {
        Ok(routine) => {
            let kind = match routine.status {
                RoutineStatus::Paused => "routine.paused",
                RoutineStatus::Active => "routine.resumed",
                RoutineStatus::Retired => "routine.retired",
            };
            state.emit(kind, serde_json::json!({ "routine_id": routine.id }));
            Json(routine).into_response()
        }
        Err(e) => map_err(e),
    }
}

// ---- Review handlers (new, M4, design §2/§3.2/§4.1, T4.1.1) ----------------
//
// `create`/`update`/`finalize` are direct writes, human-gated, same
// convention as Area/Task/Week/Routine above (no `Sin90Op::CreateReview`
// proposal variant — same reasoning the Routine section doc above gives).
// `GET /reviews` and `GET /reviews/{id}` are reads, no gate. Every
// successful write also mirrors to `EventSink`, one-for-one with the
// store's own internal `sin90_events` row (same "two audiences, two
// writes" split every other direct-write handler above follows) — a no-op
// `PATCH` writes neither (`update_review_body`'s doc).

/// `POST /reviews` — body deserializes straight into [`NewReview`]
/// (`deny_unknown_fields`). `kind`/`period` validation (format, plus a
/// `rhythm` period's existence check) and the `UNIQUE(kind, period)` 409 all
/// happen in `store::repo::create_review`.
async fn create_review(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    let req: NewReview = match parse(&body, "review") {
        Ok(b) => b,
        Err(r) => return r,
    };
    match state.store.create_review(&req).await {
        Ok(review) => {
            state.emit(
                "review.created",
                serde_json::json!({
                    "id": review.id, "kind": review.kind, "period": review.period,
                    "status": review.status, "source": "human",
                }),
            );
            (StatusCode::CREATED, Json(review)).into_response()
        }
        Err(e) => map_err(e),
    }
}

/// `GET /reviews?kind=&period=` — a read, no gate, same posture as
/// `GET /routines`.
async fn list_reviews(
    State(state): State<Sin90State>,
    Query(q): Query<ReviewListQuery>,
) -> Response {
    match state.store.list_reviews(q.kind, q.period.as_deref()).await {
        Ok(v) => Json(serde_json::json!({ "reviews": v })).into_response(),
        Err(e) => map_err(e),
    }
}

/// `GET /reviews/{id}` — a read, no gate. Unknown id -> 404 via `map_err`.
async fn get_review(State(state): State<Sin90State>, AxPath(id): AxPath<String>) -> Response {
    match state.store.get_review(&id).await {
        Ok(r) => Json(r).into_response(),
        Err(e) => map_err(e),
    }
}

/// `PATCH /reviews/{id}` — body deserializes into [`ReviewPatch`] (a single
/// required `body` field — see that type's doc for why this isn't a
/// double-`Option` shape like `RoutinePatch`). A `finalized` Review rejects
/// any patch with `StoreError::Conflict` -> 409
/// (`store::repo::update_review_body`'s doc).
///
/// Mirrors `review.updated` to `EventSink` — but ONLY when
/// [`crate::store::ReviewUpdate::changed`] is `true` (same no-op-means-no-
/// mirror rule `update_routine` follows above): re-stating the current body
/// writes no internal `sin90_events` row either, so the mirror stays silent
/// too.
async fn update_review(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    AxPath(id): AxPath<String>,
    body: Bytes,
) -> Response {
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    let patch: ReviewPatch = match parse(&body, "review patch") {
        Ok(b) => b,
        Err(r) => return r,
    };
    match state.store.update_review_body(&id, &patch.body).await {
        Ok(outcome) => {
            if outcome.changed {
                state.emit(
                    "review.updated",
                    serde_json::json!({ "review_id": outcome.review.id, "body": outcome.review.body }),
                );
            }
            Json(outcome.review).into_response()
        }
        Err(e) => map_err(e),
    }
}

/// `POST /reviews/{id}/finalize` — `draft -> finalized` (design §3.2, the
/// only legal edge); finalizing an already-`finalized` Review comes back as
/// `StoreError::Transition` -> 409, same convention `PATCH
/// /routines/{id}/transition` above uses.
async fn finalize_review(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    AxPath(id): AxPath<String>,
) -> Response {
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    match state.store.finalize_review(&id).await {
        Ok(review) => {
            state.emit(
                "review.finalized",
                serde_json::json!({ "review_id": review.id }),
            );
            Json(review).into_response()
        }
        Err(e) => map_err(e),
    }
}

#[derive(Deserialize)]
struct WeeklyDraftQuery {
    week: String,
}

/// `GET /review/weekly/draft?week=YYYY-Www` (T4.3.1, spec.md M4) — a read,
/// no actor gate, same posture as every other `GET` in this file
/// (`/attention`, `/weeks/{id}/attention`, `/events`). Emits no event: this
/// route computes a number, it doesn't write anything (mirrors `/attention`
/// and `/weeks/{id}/attention`, neither of which emits either). All the
/// real work — including the `week` format check — is
/// `Sin90Store::weekly_draft`'s (`StoreError::Invalid` -> 400 via `map_err`,
/// same as `POST /reviews`' `period` validation).
async fn weekly_review_draft(
    State(state): State<Sin90State>,
    Query(q): Query<WeeklyDraftQuery>,
) -> Response {
    match state.store.weekly_draft(&q.week).await {
        Ok(draft) => Json(draft).into_response(),
        Err(e) => map_err(e),
    }
}

// ---- AI settings (T5.1.1, design §11.3.2, J10b) ----------------------------
//
// The one switch T5.1.1 exposes: `ai.executive_enabled`, stored generically
// in `sin90_settings` (`store::ai_port`). Read is ungated (same convention
// every other `GET` here uses); the write requires the human key — an AI run
// reads this switch (`ai::SettingsRead`) but never has a route that could
// flip it for itself.

#[derive(serde::Serialize)]
struct AiSettingsBody {
    executive_enabled: bool,
}
impl From<crate::ai::AiSettings> for AiSettingsBody {
    fn from(s: crate::ai::AiSettings) -> Self {
        Self {
            executive_enabled: s.executive_enabled,
        }
    }
}

/// `GET /settings/ai`.
async fn get_ai_settings(State(state): State<Sin90State>) -> Response {
    match state.store.get_ai_settings().await {
        Ok(s) => Json(AiSettingsBody::from(s)).into_response(),
        Err(e) => map_err(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AiSettingsPatch {
    executive_enabled: bool,
}

/// `PUT /settings/ai {"executive_enabled": bool}` — human key only;
/// mirrors `setting.changed` to `EventSink`, same one-event-per-real-write
/// convention every other direct write here follows.
async fn put_ai_settings(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    let patch: AiSettingsPatch = match parse(&body, "ai settings patch") {
        Ok(b) => b,
        Err(r) => return r,
    };
    match state
        .store
        .put_ai_executive_enabled(patch.executive_enabled)
        .await
    {
        Ok(s) => {
            state.emit(
                "setting.changed",
                serde_json::json!({"key": "ai.executive_enabled", "value": s.executive_enabled}),
            );
            Json(AiSettingsBody::from(s)).into_response()
        }
        Err(e) => map_err(e),
    }
}

// ---- fired receipt (new, T3.2.2, design §2 #16) -----------------------------
//
// `POST /_a24/scheduler/fired` — the kernel scheduler's at-least-once
// delivery landing point (spec.md M3 "fired"; architecture.md #4). Only
// registered when `router(.., mounted: true)` — see that function's doc for
// why. No actor-key gate: this route's authenticity comes from the mount
// boundary itself (the kernel proxy strips client-forged `X-A24-*` headers),
// not from `x-sin90-actor-key` — the kernel is not "human" or "automation"
// in that sense.
//
// `fired_at` (part of the body, see `FiredReq`) is today ONLY format-checked
// (fixed-width ISO-8601) and then discarded — it is not persisted anywhere
// (no column, not in the mirrored `routine.fired` event). If a later task
// needs it (e.g. review/audit wanting to know when the kernel actually sent
// a delivery, not just the `scheduled_for` slot or this module's own
// `received_at`), that is new work, not something already wired up here.

/// `X-A24-Fire-Id` (T3.2.2, spec.md M3): required on every fired delivery —
/// missing it is a 400, not a silently-accepted request, since without it
/// there is nothing to dedup a kernel retry against.
#[allow(clippy::result_large_err)]
fn require_fire_id(headers: &HeaderMap) -> std::result::Result<&str, Response> {
    headers
        .get("x-a24-fire-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "missing required header X-A24-Fire-Id",
            )
        })
}

/// Every outcome below answers 2xx — spec.md M3 and the kernel's delivery
/// contract (Agent24 design doc §4.1) both require it: the kernel treats any
/// non-2xx as a failed delivery and retries, and none of
/// [`RoutineFireOutcome`]'s variants are an actual delivery failure (see that
/// type's doc). The handler itself does no slow work — it calls
/// `record_routine_fire` once and returns; nothing here calls out to the
/// kernel, the AI ladder, or blocks on anything else. (T4.3.2:
/// `record_routine_fire` itself now does a HANDFUL of extra local SQLite
/// reads — never more than one extra connection checkout plus a few
/// indexed queries — when the fired Routine is `kind: review`, to precompute
/// and maybe insert a weekly draft Review; see that method's own doc for
/// exactly why and its explicit non-goal of doing so INSIDE its write
/// transaction. Still no network call, no AI call, no blocking wait on
/// anything outside this process — "fast" here means "no slow work", not
/// "exactly one query".)
async fn scheduler_fired(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let fire_id = match require_fire_id(&headers) {
        Ok(id) => id.to_string(),
        Err(r) => return r,
    };
    let req: FiredReq = match parse(&body, "fired") {
        Ok(b) => b,
        Err(r) => return r,
    };
    if !crate::core::is_fixed_iso8601(&req.scheduled_for)
        || !crate::core::is_fixed_iso8601(&req.fired_at)
    {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "scheduled_for and fired_at must be fixed-width ISO-8601 (YYYY-MM-DDThh:mm:ssZ)",
        );
    }

    match state
        .store
        .record_routine_fire(&fire_id, &req.key, &req.scheduled_for, req.trigger)
        .await
    {
        Ok(RoutineFireOutcome::Recorded {
            routine_id,
            auto_review,
        }) => {
            // Mirror the SAME fields the store's own internal `routine.fired`
            // event carries (design convention every other direct-write
            // handler above follows) — only on a REAL new record, never on a
            // duplicate, matching `update_routine`'s "no internal event -> no
            // mirrored event" rule.
            state.emit(
                "routine.fired",
                serde_json::json!({
                    "routine_id": routine_id, "fire_id": fire_id,
                    "scheduled_for": req.scheduled_for, "trigger": req.trigger,
                }),
            );
            // T4.3.2: this fire ALSO auto-created a weekly draft Review —
            // mirror `review.created` for it too, same shape `create_review`
            // mirrors on the human path (`"source": "human"` there vs.
            // `"routine"` here is the only difference).
            let review_created = auto_review.as_ref().map(|r| r.review_id.clone());
            if let Some(AutoReviewCreated { review_id, period }) = &auto_review {
                state.emit(
                    "review.created",
                    serde_json::json!({
                        "id": review_id, "kind": "weekly", "period": period,
                        "status": "draft", "source": "routine",
                    }),
                );
            }
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "status": "recorded", "routine_id": routine_id,
                    "review_created": review_created,
                })),
            )
                .into_response()
        }
        Ok(RoutineFireOutcome::Duplicate { routine_id }) => (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "duplicate", "routine_id": routine_id })),
        )
            .into_response(),
        Ok(RoutineFireOutcome::UnknownKey) => {
            // Not an error the kernel should retry over — this module's own
            // bookkeeping drift (T3.3.2's reconciler is where orphans get
            // cleaned up, not here; see design §16/architecture.md #4).
            tracing::warn!(key = %req.key, fire_id = %fire_id, "sin90: fired for unknown key (orphan — reconciler will handle it)");
            (
                StatusCode::OK,
                Json(serde_json::json!({ "status": "unknown_key" })),
            )
                .into_response()
        }
        Ok(RoutineFireOutcome::RoutineRetired { routine_id }) => {
            tracing::warn!(routine_id = %routine_id, fire_id = %fire_id, "sin90: fired for a retired routine (orphan — reconciler will handle it)");
            (
                StatusCode::OK,
                Json(serde_json::json!({ "status": "routine_retired", "routine_id": routine_id })),
            )
                .into_response()
        }
        Err(e) => map_err(e),
    }
}

#[cfg(test)]
mod tests;
