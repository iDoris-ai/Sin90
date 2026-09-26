//! `POST /debug/kernel-roundtrip` (T3.2.3, `test-hooks` only) — proves a
//! REAL mount's typed kernel clients (`adapter_agent24::clients::Clients`)
//! actually round-trip through a real `agent24d`, one capability at a time:
//! memory (`_a24/memory/private/*`), approval (`_a24/approval/*`), and
//! scheduler (`_a24/scheduler/*`).
//!
//! Lives HERE, inside `adapter_agent24`, and NOT in `http` — `lib.rs`'s own
//! dependency arrow (`core <- store <- http <- adapter_agent24`) means
//! `http` must never depend on Agent24 types, and this whole route only
//! exists to poke `KernelClients`/`clients::Clients`, both of which are
//! Agent24 types. It is therefore its OWN standalone `axum::Router`, over
//! its own tiny state — never merged into `crate::http::router`'s state or
//! module tree — that `main.rs::run_as_agent24_module` merges onto the main
//! mounted router at the HTTP level (review round on the T3.2.3 debug route:
//! the first cut wrongly put this handler in `http`, giving `http` a
//! `Sin90State` field of an Agent24 type).
//!
//! Exists ONLY so `tests/agent24_mount_blackbox.rs`'s `kernel_clients_roundtrip`
//! has something to call through the real constrained proxy — the shipped
//! `sin90` binary has no route that lets an ordinary HTTP client drive these
//! typed clients at all (T3.2.1 itself added no business caller; T3.2.2's
//! `fired` is kernel-initiated, not client-initiated). Never compiled into a
//! ship build: `test-hooks` is off by default (`Cargo.toml`), and `main.rs`
//! also refuses to build at all with `test-hooks` on in a release profile
//! (`compile_error!` in `main.rs`) — this route must never reach a real
//! install.
//!
//! # Where `request_id`/`approval_token` come from
//!
//! `approval::gate` needs both, and this handler mints neither: it reads
//! `X-A24-Request-Id`/`X-A24-Approval-Token` off its OWN incoming request.
//! Agent24's constrained proxy injects a FRESH pair on every proxied
//! request, not just approval-specific ones
//! (`agent24-os-proto::proxy`'s module doc: "the SECRET the kernel mints
//! alongside `request_id` and injects on every proxied request") — so by the
//! time this handler runs, the admission `gate` needs already exists for
//! THIS request, the same way it would for any other mounted route.
//!
//! `gate`'s action is fixed to `schedule_callback` — the one entry Agent24's
//! closed set actually accepts this round (Agent24
//! `module_approval_broker.rs`'s own doc: "闭集匹配 adds the first entry,
//! `schedule_callback`"); any other action would come back `Forbidden` by
//! design, which would not prove the round trip. Agent24's own
//! `domain.rs::scheduler_callback_forbidden_without_grant_and_offered_and_working_with_it`
//! (and its neighbors) already cover the negative control this task's
//! reviewer asked for — an ungranted module's `gate`/`advise`/`status` all
//! coming back `forbidden` — on the kernel side; standing up a SECOND Sin90
//! manifest and a second full mount cycle here to duplicate that coverage
//! was judged not worth the cost for this round (see the PR notes / task
//! report).
//!
//! # Not cleaned up — throwaway `$HOME` only
//!
//! A successful call leaves behind ONE real, permanently-`pending`
//! `schedule_callback` approval row (targeting `2099-01-01`, so Agent24's
//! own due-sweep never touches it) and ONE real `_a24/memory/private/*`
//! entry — neither is deleted afterward (unlike the scheduler key, which
//! this handler upserts, lists, and deletes within the same call). This
//! route is therefore only fit to run against a throwaway, single-use
//! `$HOME`/kernel install — exactly what
//! `tests/agent24_mount_blackbox.rs::kernel_clients_roundtrip` does with its
//! own `tmp_home` — never a persistent one.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Json;
use serde_json::{json, Map};

use crate::adapter_agent24::clients::approval::ApprovalToken;
use crate::adapter_agent24::clients::scheduler::ModuleSpec;
use crate::adapter_agent24::clients::{ClientError, Clients};
use crate::adapter_agent24::KernelClients;
use crate::http::actor::{forbidden, Actor, ActorKeys};

/// The `Routine` key this round-trip upserts, lists, deletes, then lists
/// again — a name no real business code chooses (T3.3.2's reconciler does
/// not exist yet), so a stray leftover row is unmistakably this debug
/// route's own.
const SCHEDULER_TEST_KEY: &str = "routine.test";

/// Far enough in the future that Agent24's own background sweep for due
/// `schedule_callback` approvals (`module_approval_broker.rs`) never reaches
/// it during a test run — this route only cares that `gate` round-trips and
/// comes back `Pending`, not that the approval later executes.
const GATE_TARGET_FAR_FUTURE: &str = "2099-01-01T00:00:00Z";

#[derive(Clone)]
struct DebugState {
    clients: Arc<KernelClients>,
    actor_keys: Arc<ActorKeys>,
}

/// Builds the standalone debug router. `main.rs::run_as_agent24_module`
/// merges this onto the main mounted router (`test-hooks` only) — see the
/// module doc for why it is not simply another route on
/// `crate::http::router`.
pub fn router(clients: Arc<KernelClients>, actor_keys: Arc<ActorKeys>) -> axum::Router {
    axum::Router::new()
        .route("/debug/kernel-roundtrip", post(kernel_roundtrip))
        .with_state(DebugState {
            clients,
            actor_keys,
        })
}

/// A minimal copy of `http`'s own v1 error envelope shape — deliberately
/// duplicated, not imported: `http::error_response` is a private helper of
/// `http::mod`'s own handlers, and reaching into it would recreate exactly
/// the cross-layer coupling this module exists to avoid.
fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"error": {"code": code, "message": message}})),
    )
        .into_response()
}

fn client_err_response(step: &str, err: ClientError) -> Response {
    error_response(
        StatusCode::BAD_GATEWAY,
        "kernel_roundtrip_failed",
        &format!("{step}: {err}"),
    )
}

async fn kernel_roundtrip(State(state): State<DebugState>, headers: HeaderMap) -> Response {
    match state.actor_keys.identify(&headers) {
        Some(Actor::Human) => {}
        Some(Actor::Automation) => {
            return forbidden("this debug route requires the human actor key")
        }
        None => return forbidden("missing or unrecognized actor key"),
    }

    // Cheap to build fresh per call — `Clients::build`'s own doc.
    let clients = Clients::build(&state.clients);
    let offer = state.clients.offer().to_vec();

    // ---- memory: remember, then recall it back --------------------------
    let Some(memory) = &clients.memory else {
        return error_response(
            StatusCode::FORBIDDEN,
            "memory_not_granted",
            "Offer.provides did not cover _a24/memory/private/",
        );
    };
    let mut body = Map::new();
    body.insert(
        "probe".to_string(),
        json!("t3.2.3-kernel-clients-roundtrip"),
    );
    let remembered = match memory.remember("t3.2.3.debug", body, None).await {
        Ok(r) => r,
        Err(e) => return client_err_response("memory.remember", e),
    };
    let recall = match memory.recall("t3.2.3.debug", 20, None, None).await {
        Ok(p) => p,
        Err(e) => return client_err_response("memory.recall", e),
    };
    let memory_found_in_recall = recall.items.iter().any(|item| item.id == remembered.id);

    // ---- approval: gate the one action in the kernel's closed set -------
    let Some(approval) = &clients.approval else {
        return error_response(
            StatusCode::FORBIDDEN,
            "approval_not_granted",
            "Offer.provides did not cover _a24/approval/",
        );
    };
    let Some(request_id) = headers
        .get("x-a24-request-id")
        .and_then(|v| v.to_str().ok())
    else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "missing_request_id",
            "this request itself carries no X-A24-Request-Id — it did not arrive through the \
             real kernel proxy",
        );
    };
    let Some(approval_token) = headers
        .get("x-a24-approval-token")
        .and_then(|v| v.to_str().ok())
    else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "missing_approval_token",
            "this request itself carries no X-A24-Approval-Token — it did not arrive through \
             the real kernel proxy",
        );
    };
    let gate_answer = match approval
        .gate(
            "schedule_callback",
            Some(GATE_TARGET_FAR_FUTURE),
            json!({"probe": "t3.2.3-kernel-clients-roundtrip"}),
            request_id,
            &ApprovalToken::new(approval_token),
        )
        .await
    {
        Ok(a) => a,
        Err(e) => return client_err_response("approval.gate", e),
    };

    // ---- scheduler: upsert, list, delete, list again ----------------------
    let Some(scheduler) = &clients.scheduler else {
        return error_response(
            StatusCode::FORBIDDEN,
            "scheduler_not_granted",
            "Offer.provides did not cover _a24/scheduler/",
        );
    };
    let spec = ModuleSpec::Every { secs: 3600 };
    let upsert = match scheduler
        .upsert(SCHEDULER_TEST_KEY, &spec, true, None, None)
        .await
    {
        Ok(u) => u,
        Err(e) => return client_err_response("scheduler.upsert", e),
    };
    let list = match scheduler.list(None).await {
        Ok(l) => l,
        Err(e) => return client_err_response("scheduler.list", e),
    };
    let scheduler_found_in_list = list.schedules.iter().any(|s| s.key == SCHEDULER_TEST_KEY);
    let delete = match scheduler.delete(SCHEDULER_TEST_KEY, None).await {
        Ok(d) => d,
        Err(e) => return client_err_response("scheduler.delete", e),
    };
    // L3 (review): prove the delete actually took, not just that it
    // answered `Deleted` — list once more and confirm the key is gone.
    let list_after_delete = match scheduler.list(None).await {
        Ok(l) => l,
        Err(e) => return client_err_response("scheduler.list_after_delete", e),
    };
    let found_after_delete = list_after_delete
        .schedules
        .iter()
        .any(|s| s.key == SCHEDULER_TEST_KEY);

    Json(json!({
        "offer": offer,
        "memory": {
            "remembered_id": remembered.id,
            "found_in_recall": memory_found_in_recall,
        },
        "approval": {
            "approval_id": gate_answer.approval_id,
            "decision": format!("{:?}", gate_answer.decision),
            "binding": gate_answer.binding,
        },
        "scheduler": {
            "upsert_outcome": format!("{:?}", upsert.outcome),
            "found_in_list": scheduler_found_in_list,
            "delete_outcome": format!("{:?}", delete.outcome),
            "found_after_delete": found_after_delete,
        },
    }))
    .into_response()
}
