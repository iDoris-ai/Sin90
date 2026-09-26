//! `POST /debug/reconciler/force-upsert` (T3.5.1, `test-hooks` only) — lets a
//! test drive TWO real, back-to-back `_a24/scheduler/upsert` calls for the
//! SAME real `Routine`, independent of `adapter_agent24::reconciler`'s own
//! dedup (`Sin90Store::outbox_enqueue_upsert_for_routine`'s "already pending"
//! guard, `store::repo`'s own doc) — that guard means an ordinary restart or
//! a second `outbox_enqueue_*` call does NOT, by itself, ever send the kernel
//! two upserts for an unchanged Routine, so there is no way to exercise
//! DESIGN §M3's own positive control ("幂等对账的正对照 = 故意注册两次，内核里
//!仍只有一条") through the production write paths alone. This route bypasses
//! `sin90_outbox` entirely and calls [`SchedulerClient::upsert`] twice,
//! mirroring the exact conversion `adapter_agent24::reconciler::apply_one`
//! itself does (Routine `cron`/`tz` -> `ModuleSpec::Cron`, key =
//! `routine.<id>`) — proving the KERNEL's own `upsert` is idempotent by key,
//! not merely that Sin90's outbox happens not to re-send it.
//!
//! Lives here, next to `kernel_roundtrip`, for the identical reason that
//! module's own doc gives: this needs `SchedulerClient`/`KernelClients`
//! (Agent24 types), and `http` must never depend on Agent24 (`lib.rs`'s
//! layering doc). Its own tiny state (never `Sin90State`), merged onto the
//! mounted router at the HTTP level by `main.rs::run_as_agent24_module`,
//! `test-hooks`-only, never built into a release binary
//! (`lib.rs`'s `compile_error!`).
//!
//! The kernel key it upserts is [`crate::store::repo::routine_kernel_key`]
//! itself — the SAME `pub(crate)` function `adapter_agent24::reconciler::
//! kernel_key` delegates to (that function's own doc, T3.5.1 review M2/L: a
//! naive `format!("routine.{id}")` — what this route used before that
//! review round — sends the kernel an invalid, uppercase-bearing key, since
//! `routine.id` is an uppercase `ulid()` and the kernel's key charset is
//! `[a-z0-9._-]`; this debug route would always fail with `invalid_params`
//! and never actually exercise the idempotency it exists to prove).

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::adapter_agent24::clients::scheduler::{ModuleSpec, SchedulerClient};
use crate::adapter_agent24::KernelClients;
use crate::core::RoutineStatus;
use crate::http::actor::{forbidden, Actor, ActorKeys};
use crate::store::Sin90Store;

#[derive(Clone)]
struct DebugState {
    clients: Arc<KernelClients>,
    store: Sin90Store,
    actor_keys: Arc<ActorKeys>,
}

/// Builds the standalone debug router — see the module doc for why it is its
/// own router rather than a route on `crate::http::router`.
pub fn router(
    clients: Arc<KernelClients>,
    store: Sin90Store,
    actor_keys: Arc<ActorKeys>,
) -> axum::Router {
    axum::Router::new()
        .route("/debug/reconciler/force-upsert", post(force_upsert))
        .with_state(DebugState {
            clients,
            store,
            actor_keys,
        })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ForceUpsertReq {
    routine_id: String,
}

/// A minimal copy of `http`'s own v1 error envelope shape — see this
/// module's own doc / `kernel_roundtrip.rs`'s identical helper for why this
/// is duplicated rather than imported.
fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"error": {"code": code, "message": message}})),
    )
        .into_response()
}

async fn force_upsert(
    State(state): State<DebugState>,
    headers: HeaderMap,
    Json(req): Json<ForceUpsertReq>,
) -> Response {
    match state.actor_keys.identify(&headers) {
        Some(Actor::Human) => {}
        Some(Actor::Automation) => {
            return forbidden("this debug route requires the human actor key")
        }
        None => return forbidden("missing or unrecognized actor key"),
    }

    let Some(scheduler) = SchedulerClient::new(&state.clients) else {
        return error_response(
            StatusCode::FORBIDDEN,
            "scheduler_not_granted",
            "Offer.provides did not cover _a24/scheduler/",
        );
    };

    let routine = match state.store.get_routine(&req.routine_id).await {
        Ok(r) => r,
        Err(e) => {
            return error_response(StatusCode::NOT_FOUND, "routine_not_found", &e.to_string())
        }
    };

    // Same conversion `adapter_agent24::reconciler::apply_one` does for a
    // real `scheduler.upsert` outbox row — see the module doc. Delegates to
    // `store::repo::routine_kernel_key` for the SAME reason
    // `adapter_agent24::reconciler::kernel_key` does (that function's own
    // doc, T3.5.1): the kernel's key charset is `[a-z0-9._-]`, and
    // `routine.id` is an uppercase `ulid()` — a naive `format!` here would
    // send the kernel an invalid key and this whole debug route would
    // always fail, defeating its own purpose.
    let key = crate::store::repo::routine_kernel_key(&routine.id);
    let spec = ModuleSpec::Cron {
        expr: routine.cron.clone(),
        tz: Some(routine.tz.clone()),
    };
    let enabled = routine.status == RoutineStatus::Active;

    let first = match scheduler.upsert(&key, &spec, enabled, None, None).await {
        Ok(u) => format!("{:?}", u.outcome),
        Err(e) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                "kernel_roundtrip_failed",
                &format!("first upsert: {e}"),
            )
        }
    };
    let second = match scheduler.upsert(&key, &spec, enabled, None, None).await {
        Ok(u) => format!("{:?}", u.outcome),
        Err(e) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                "kernel_roundtrip_failed",
                &format!("second upsert: {e}"),
            )
        }
    };

    Json(json!({
        "key": key,
        "first_outcome": first,
        "second_outcome": second,
    }))
    .into_response()
}
