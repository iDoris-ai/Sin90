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
pub mod state;

use axum::body::Bytes;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::Json;
use serde::Deserialize;

use crate::core::{
    Alloc, AreaStatus, Energy, NewRoutine, RoutinePatch, RoutineStatus, ScheduleBlockStatus,
    Sin90Proposal, TaskKind, TaskStatus, WeekStatus,
};
use crate::store::StoreError;

pub use actor::{Actor, ActorKeys};
pub use state::{EventSink, NullEventSink, Sin90State};

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
pub fn router(state: Sin90State) -> axum::Router {
    axum::Router::new()
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
        .route("/attention", get(attention))
        .route("/events", get(list_events))
        .route("/packs/install", post(install_pack))
        .route("/capture", post(capture))
        .route("/today", get(today))
        .route("/routines", post(create_routine).get(list_routines))
        .route("/routines/{id}", get(get_routine).patch(update_routine))
        .route("/routines/{id}/transition", post(transition_routine))
        .with_state(state)
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

#[cfg(test)]
mod tests;
