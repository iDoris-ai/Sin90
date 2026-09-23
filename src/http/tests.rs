//! In-process HTTP-layer integration tests: real axum router, real (in-memory)
//! SQLite, real serde wire shapes — everything except a real Agent24 daemon
//! and a real subprocess. This is the fastest place to pin M0 §6's judgements
//! A4-A10 (the ones that do not require an actual daemon restart/mount, which
//! `tests/blackbox.rs` covers instead with a `--standalone` subprocess).

#![allow(clippy::unwrap_used)]

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

use crate::http::{router, ActorKeys, EventSink, Sin90State};
use crate::store::Sin90Store;

/// Records every emitted event kind+payload, so a test can assert on the
/// events a mutation produced (M0 §6 A11's in-process half — the real WS
/// delivery is `adapter_agent24`'s job, exercised in `tests/blackbox.rs`).
#[derive(Default, Clone)]
struct RecordingSink(Arc<Mutex<Vec<(String, Value)>>>);
impl EventSink for RecordingSink {
    fn emit(&self, kind: &str, payload: serde_json::Map<String, Value>) {
        self.0
            .lock()
            .unwrap()
            .push((kind.to_string(), Value::Object(payload)));
    }
}

const HUMAN: &str = "test-human-key";
const AUTOMATION: &str = "test-automation-key";

async fn test_app() -> (axum::Router, RecordingSink) {
    let (app, sink, _store) = test_app_with_store().await;
    (app, sink)
}

/// Same as [`test_app`], plus the `Sin90Store` handle itself — for the one
/// test (`today_view`'s carry-over rule) that needs to reach past the HTTP
/// surface via `store::test_hooks` to backdate a row's `created_at`, which no
/// route exposes (nor should one: `created_at` is server-assigned, always).
///
/// `router(.., mounted: false)` — every test in this file except the
/// `fired` module below exercises ordinary business routes that behave
/// identically mounted or not, so this is the "standalone" shape (also the
/// one `fired`'s own 404 test needs as its subject).
async fn test_app_with_store() -> (axum::Router, RecordingSink, Sin90Store) {
    test_app_with_store_mode(false).await
}

/// T3.2.2: same harness as [`test_app_with_store`], but built with
/// `mounted: true` — the only shape `POST /_a24/scheduler/fired` is
/// registered under (`router()`'s doc, architecture.md #4). Used only by the
/// `fired` test module below.
async fn test_app_mounted() -> (axum::Router, RecordingSink, Sin90Store) {
    test_app_with_store_mode(true).await
}

async fn test_app_with_store_mode(mounted: bool) -> (axum::Router, RecordingSink, Sin90Store) {
    let store = Sin90Store::open_memory().await.unwrap();
    let sink = RecordingSink::default();
    let state = Sin90State::new(
        store.clone(),
        Arc::new(sink.clone()),
        ActorKeys {
            human: HUMAN.into(),
            automation: AUTOMATION.into(),
        },
    );
    (router(state, mounted), sink, store)
}

fn automation_req(method: &str, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-sin90-actor-key", AUTOMATION)
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn human_req(method: &str, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-sin90-actor-key", HUMAN)
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get_req(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

/// A `POST /_a24/scheduler/fired` request (T3.2.2). `fire_id: None` omits
/// `X-A24-Fire-Id` entirely — the 400 test's own subject. No actor-key
/// header: this route is not gated by `x-sin90-actor-key` (its trust comes
/// from the mount boundary, `router()`'s doc).
fn fired_req(uri: &str, fire_id: Option<&str>, body: Value) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(id) = fire_id {
        b = b.header("x-a24-fire-id", id);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

// ---- A4: create Area --------------------------------------------------------

#[tokio::test]
async fn a4_create_area() {
    let (app, _sink) = test_app().await;
    let resp = app
        .oneshot(human_req("POST", "/areas", json!({"title": "工作"})))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["title"], "工作");
    assert_eq!(body["status"], "active");
    assert!(!body["id"].as_str().unwrap().is_empty());
}

// ---- A5: create Direction under an Area -------------------------------------

#[tokio::test]
async fn a5_create_direction_under_area() {
    let (app, _sink) = test_app().await;
    let area = body_json(
        app.clone()
            .oneshot(human_req("POST", "/areas", json!({"title": "工作"})))
            .await
            .unwrap(),
    )
    .await;
    let area_id = area["id"].as_str().unwrap();

    let resp = app
        .oneshot(human_req(
            "POST",
            "/directions",
            json!({"title": "Q4 交付 v0.5", "target_window": "2026-Q4", "area_id": area_id}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["area_id"], area_id);
}

// ---- A6/A7/A8: create task, legal transitions, illegal transition rejected --

#[tokio::test]
async fn a6_a7_a8_task_lifecycle_and_illegal_transition_rejected() {
    let (app, sink) = test_app().await;
    let direction = body_json(
        app.clone()
            .oneshot(human_req(
                "POST",
                "/directions",
                json!({"title": "d", "target_window": "2026-Q4"}),
            ))
            .await
            .unwrap(),
    )
    .await;
    let direction_id = direction["id"].as_str().unwrap();

    // A6: create — starts in backlog.
    let task = body_json(
        app.clone()
            .oneshot(human_req(
                "POST",
                "/tasks",
                json!({"title": "写 M0 设计", "direction_id": direction_id}),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(task["status"], "backlog");
    let task_id = task["id"].as_str().unwrap().to_string();

    // backlog -> in_progress is NOT legal (must go through planned) — confirms
    // the matrix is really wired, not just "anything not done succeeds".
    let resp = app
        .clone()
        .oneshot(human_req(
            "PATCH",
            &format!("/tasks/{task_id}"),
            json!({"to": "in_progress"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // backlog -> planned -> in_progress -> done is the legal path.
    for to in ["planned", "in_progress", "done"] {
        let resp = app
            .clone()
            .oneshot(human_req(
                "PATCH",
                &format!("/tasks/{task_id}"),
                json!({"to": to}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "transition to {to}");
    }

    // A8: done -> backlog is illegal (done is terminal) -> 409, no new event.
    let events_before = sink.0.lock().unwrap().len();
    let resp = app
        .oneshot(human_req(
            "PATCH",
            &format!("/tasks/{task_id}"),
            json!({"to": "backlog"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "conflict");
    assert_eq!(
        sink.0.lock().unwrap().len(),
        events_before,
        "a rejected transition must not emit an event"
    );
}

// ---- A9: filtered reads ------------------------------------------------------

#[tokio::test]
async fn a9_list_tasks_filters_by_direction_and_area() {
    let (app, _sink) = test_app().await;
    let area = body_json(
        app.clone()
            .oneshot(human_req("POST", "/areas", json!({"title": "工作"})))
            .await
            .unwrap(),
    )
    .await;
    let area_id = area["id"].as_str().unwrap().to_string();
    let direction = body_json(
        app.clone()
            .oneshot(human_req(
                "POST",
                "/directions",
                json!({"title": "d", "target_window": "2026-Q4", "area_id": area_id}),
            ))
            .await
            .unwrap(),
    )
    .await;
    let direction_id = direction["id"].as_str().unwrap().to_string();
    app.clone()
        .oneshot(human_req(
            "POST",
            "/tasks",
            json!({"title": "t1", "direction_id": direction_id}),
        ))
        .await
        .unwrap();
    // Unrelated task with no direction — must not show up in the filtered list.
    app.clone()
        .oneshot(human_req("POST", "/tasks", json!({"title": "unrelated"})))
        .await
        .unwrap();

    let by_direction = body_json(
        app.clone()
            .oneshot(get_req(&format!("/tasks?direction_id={direction_id}")))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(by_direction["tasks"].as_array().unwrap().len(), 1);

    let by_area = body_json(
        app.clone()
            .oneshot(get_req(&format!("/tasks?area_id={area_id}")))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(by_area["tasks"].as_array().unwrap().len(), 1);

    let areas = body_json(app.oneshot(get_req("/areas")).await.unwrap()).await;
    assert_eq!(areas["areas"].as_array().unwrap().len(), 1);
}

// ---- A10: GET /events returns exactly the expected receipt, seq increasing --

#[tokio::test]
async fn a10_events_for_a_task_are_exactly_created_then_two_transitions() {
    let (app, _sink) = test_app().await;
    let task = body_json(
        app.clone()
            .oneshot(human_req("POST", "/tasks", json!({"title": "t"})))
            .await
            .unwrap(),
    )
    .await;
    let task_id = task["id"].as_str().unwrap().to_string();
    for to in ["planned", "in_progress"] {
        app.clone()
            .oneshot(human_req(
                "PATCH",
                &format!("/tasks/{task_id}"),
                json!({"to": to}),
            ))
            .await
            .unwrap();
    }

    let events = body_json(
        app.oneshot(get_req(&format!("/events?entity=task&entity_id={task_id}")))
            .await
            .unwrap(),
    )
    .await;
    let rows = events["events"].as_array().unwrap();
    // Exactly 3, not >=3: a positive control against "produced an extra event"
    // as well as "produced none" — see design §6's note on why this must be
    // an exact count, not a non-zero check.
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows[0]["kind"], "created");
    assert_eq!(rows[1]["kind"], "transitioned");
    assert_eq!(rows[1]["to_state"], "planned");
    assert_eq!(rows[2]["kind"], "transitioned");
    assert_eq!(rows[2]["to_state"], "in_progress");
    let seqs: Vec<i64> = rows.iter().map(|r| r["seq"].as_i64().unwrap()).collect();
    assert!(seqs[0] < seqs[1] && seqs[1] < seqs[2], "{seqs:?}");
}

// ---- §7.1: the actor-key gate itself ----------------------------------------

#[tokio::test]
async fn missing_actor_key_is_rejected_on_a_direct_write_route() {
    let (app, _sink) = test_app().await;
    let req = Request::builder()
        .method("POST")
        .uri("/areas")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"title": "x"}).to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn automation_key_cannot_write_directly_but_can_submit_a_proposal() {
    let (app, _sink) = test_app().await;
    let direct = Request::builder()
        .method("POST")
        .uri("/areas")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-sin90-actor-key", AUTOMATION)
        .body(Body::from(json!({"title": "x"}).to_string()))
        .unwrap();
    let resp = app.clone().oneshot(direct).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "automation key must not be able to write /areas directly"
    );

    let proposal = Request::builder()
        .method("POST")
        .uri("/proposals")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-sin90-actor-key", AUTOMATION)
        .body(Body::from(
            json!({
                "id": "p1", "status": "pending", "source": "local_brain",
                "ops": [{"op": "create_area", "title": "x"}], "rationale": null
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.oneshot(proposal).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "automation key must be able to submit a Proposal"
    );
}

// ---- Proposal round-trip: submit -> accept -> area actually created --------

#[tokio::test]
async fn proposal_round_trip_creates_area_only_after_accept() {
    let (app, _sink) = test_app().await;
    let submit = Request::builder()
        .method("POST")
        .uri("/proposals")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-sin90-actor-key", AUTOMATION)
        .body(Body::from(
            json!({
                "id": "p1", "status": "pending", "source": "local_brain",
                "ops": [{"op": "create_area", "title": "Learning"}], "rationale": null
            })
            .to_string(),
        ))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(submit).await.unwrap().status(),
        StatusCode::ACCEPTED
    );

    // Not yet applied — no Area exists.
    let areas = body_json(app.clone().oneshot(get_req("/areas")).await.unwrap()).await;
    assert_eq!(areas["areas"].as_array().unwrap().len(), 0);

    let accept = Request::builder()
        .method("POST")
        .uri("/proposals/p1/accept")
        .header("x-sin90-actor-key", HUMAN)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(accept).await.unwrap().status(),
        StatusCode::OK
    );

    let areas = body_json(app.oneshot(get_req("/areas")).await.unwrap()).await;
    assert_eq!(areas["areas"].as_array().unwrap().len(), 1);
    assert_eq!(areas["areas"][0]["title"], "Learning");
}

// ---- Regression: automation must not be able to self-approve (Codex High) --

/// Codex 2026-09-22 review, High: `accept_proposal` used to accept either
/// actor key, so an automated caller could submit a proposal and immediately
/// accept it itself, making the "a human must approve" boundary (design
/// §7.1) cosmetic. Pins the fix: automation may still submit, but only the
/// human key may accept.
#[tokio::test]
async fn automation_can_submit_but_not_accept_its_own_proposal() {
    let (app, _sink) = test_app().await;
    let submit = Request::builder()
        .method("POST")
        .uri("/proposals")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-sin90-actor-key", AUTOMATION)
        .body(Body::from(
            json!({
                "id": "p1", "status": "pending", "source": "local_brain",
                "ops": [{"op": "create_area", "title": "Learning"}], "rationale": null
            })
            .to_string(),
        ))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(submit).await.unwrap().status(),
        StatusCode::ACCEPTED,
        "automation must still be able to submit a Proposal"
    );

    let accept_as_automation = Request::builder()
        .method("POST")
        .uri("/proposals/p1/accept")
        .header("x-sin90-actor-key", AUTOMATION)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone()
            .oneshot(accept_as_automation)
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN,
        "automation must not be able to accept (self-approve) a Proposal"
    );

    // Negative control: unapplied — no Area exists yet.
    let areas = body_json(app.clone().oneshot(get_req("/areas")).await.unwrap()).await;
    assert_eq!(areas["areas"].as_array().unwrap().len(), 0);

    // Positive control: the human key can still accept the same proposal.
    let accept_as_human = Request::builder()
        .method("POST")
        .uri("/proposals/p1/accept")
        .header("x-sin90-actor-key", HUMAN)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(accept_as_human).await.unwrap().status(),
        StatusCode::OK
    );
    let areas = body_json(app.oneshot(get_req("/areas")).await.unwrap()).await;
    assert_eq!(areas["areas"].as_array().unwrap().len(), 1);
}

// ---- SFU-10: submit_proposal validates before persisting -------------------

/// SFU-10: a proposal with no ops is structurally invalid (`ProposalError::
/// Empty`). This used to be caught only at accept time (the proposal landed
/// as `pending` and only failed later); it must now be caught at SUBMIT time,
/// with nothing persisted.
#[tokio::test]
async fn submit_proposal_with_no_ops_is_422_and_nothing_is_persisted() {
    let (app, _sink) = test_app().await;
    let resp = app
        .clone()
        .oneshot(automation_proposal(json!({
            "id": "p-empty", "status": "pending", "source": "local_brain",
            "ops": [], "rationale": null
        })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // Nothing was persisted — the id must not exist at all, not even `pending`.
    let get = app
        .clone()
        .oneshot(get_req("/proposals/p-empty"))
        .await
        .unwrap();
    assert_eq!(get.status(), StatusCode::NOT_FOUND);
}

/// SFU-10: an op referencing an entity that does not exist (`ProposalError::
/// UnknownEntity` — the class `apply_proposal` used to be the ONLY place that
/// caught) is also rejected at submit time.
#[tokio::test]
async fn submit_proposal_referencing_an_unknown_task_is_422_and_nothing_is_persisted() {
    let (app, _sink) = test_app().await;
    let resp = app
        .clone()
        .oneshot(automation_proposal(json!({
            "id": "p-ghost", "status": "pending", "source": "local_brain",
            "ops": [{"op": "transition_task", "task_id": "ghost", "to": "in_progress"}],
            "rationale": null
        })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let get = app
        .clone()
        .oneshot(get_req("/proposals/p-ghost"))
        .await
        .unwrap();
    assert_eq!(get.status(), StatusCode::NOT_FOUND);
}

/// SFU-10: a blank-title `create_area` op (`ProposalError::BlankField`,
/// structural — no DB lookup needed to catch it) is also caught at submit
/// time.
#[tokio::test]
async fn submit_proposal_with_a_blank_title_op_is_422_and_nothing_is_persisted() {
    let (app, _sink) = test_app().await;
    let resp = app
        .clone()
        .oneshot(automation_proposal(json!({
            "id": "p-blank", "status": "pending", "source": "local_brain",
            "ops": [{"op": "create_area", "title": "   "}], "rationale": null
        })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let get = app
        .clone()
        .oneshot(get_req("/proposals/p-blank"))
        .await
        .unwrap();
    assert_eq!(get.status(), StatusCode::NOT_FOUND);
}

/// Positive control for the three tests above: a structurally legal proposal
/// still gets 202 and lands as `pending` — submit-time validation must not be
/// stricter than the existing (accept-time) rule it reuses.
#[tokio::test]
async fn submit_proposal_that_is_structurally_valid_is_still_202() {
    let (app, _sink) = test_app().await;
    let resp = app
        .clone()
        .oneshot(automation_proposal(json!({
            "id": "p-valid", "status": "pending", "source": "local_brain",
            "ops": [{"op": "create_area", "title": "Health"}], "rationale": null
        })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let get = body_json(
        app.clone()
            .oneshot(get_req("/proposals/p-valid"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(get["status"], "pending");
}

/// A rejected (invalid) submission never lands, so re-submitting the exact
/// same id+ops afterward is NOT the "idempotent replay" path — there is
/// nothing stored to replay against, so it is validated again and rejected
/// again (not silently accepted, and not a spurious `Conflict`).
#[tokio::test]
async fn resubmitting_the_same_invalid_proposal_is_422_again_not_a_silent_conflict() {
    let (app, _sink) = test_app().await;
    let body = json!({
        "id": "p-empty2", "status": "pending", "source": "local_brain",
        "ops": [], "rationale": null
    });
    for _ in 0..2 {
        let resp = app
            .clone()
            .oneshot(automation_proposal(body.clone()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
    let get = app
        .clone()
        .oneshot(get_req("/proposals/p-empty2"))
        .await
        .unwrap();
    assert_eq!(get.status(), StatusCode::NOT_FOUND);
}

// ---- Area archive/reactivate round-trip (design §3.2: both edges legal) ----

#[tokio::test]
async fn area_can_be_archived_and_reactivated() {
    let (app, _sink) = test_app().await;
    let area = body_json(
        app.clone()
            .oneshot(human_req("POST", "/areas", json!({"title": "工作"})))
            .await
            .unwrap(),
    )
    .await;
    let id = area["id"].as_str().unwrap().to_string();

    let archived = body_json(
        app.clone()
            .oneshot(human_req(
                "PATCH",
                &format!("/areas/{id}"),
                json!({"to": "archived"}),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(archived["status"], "archived");

    let reactivated = body_json(
        app.oneshot(human_req(
            "PATCH",
            &format!("/areas/{id}"),
            json!({"to": "active"}),
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(reactivated["status"], "active");
}

// ---- nested project (Task.parent_task_id, design §2 #3) --------------------

#[tokio::test]
async fn a_task_with_children_is_a_project_but_nesting_is_limited_to_one_level() {
    let (app, _sink) = test_app().await;
    let root = body_json(
        app.clone()
            .oneshot(human_req("POST", "/tasks", json!({"title": "Project X"})))
            .await
            .unwrap(),
    )
    .await;
    let root_id = root["id"].as_str().unwrap().to_string();

    let child = app
        .clone()
        .oneshot(human_req(
            "POST",
            "/tasks",
            json!({"title": "sub-task", "parent_task_id": root_id}),
        ))
        .await
        .unwrap();
    assert_eq!(child.status(), StatusCode::CREATED);
    let child = body_json(child).await;
    let child_id = child["id"].as_str().unwrap().to_string();

    // A grandchild is rejected: nesting is one level only.
    let grandchild = app
        .oneshot(human_req(
            "POST",
            "/tasks",
            json!({"title": "grandchild", "parent_task_id": child_id}),
        ))
        .await
        .unwrap();
    assert_eq!(grandchild.status(), StatusCode::CONFLICT);
}

// ---- §7.4 seed pack: installable, and its Areas are ordinary afterward -----

#[tokio::test]
async fn installing_the_seed_pack_creates_five_areas_via_the_http_route() {
    let (app, _sink) = test_app().await;
    let resp = app
        .clone()
        .oneshot(human_req("POST", "/packs/install", json!({})))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let areas = body_json(app.oneshot(get_req("/areas")).await.unwrap()).await;
    assert_eq!(areas["areas"].as_array().unwrap().len(), 5);
}

// ---- M1: /capture + /today ---------------------------------------------------

#[tokio::test]
async fn capture_lands_as_an_uncategorized_backlog_task_and_shows_up_in_today() {
    let (app, sink) = test_app().await;
    let resp = app
        .clone()
        .oneshot(human_req(
            "POST",
            "/capture",
            json!({"text": "水电费还没交"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let task = body_json(resp).await;
    assert_eq!(task["title"], "水电费还没交");
    assert!(
        task["direction_id"].is_null(),
        "a capture must not be pre-classified into a Direction"
    );
    assert_eq!(task["status"], "backlog");

    // M1's acceptance line: it shows up in /today's un-classified section.
    let today = body_json(app.oneshot(get_req("/today")).await.unwrap()).await;
    let inbox = today["inbox"].as_array().unwrap();
    assert!(
        inbox.iter().any(|t| t["id"] == task["id"]),
        "captured task must appear in /today's inbox: {inbox:?}"
    );

    // It also emitted the ordinary task.created event — capture is not a
    // separate event kind, it's a Task creation like any other (design M1:
    // "does not introduce a new entity").
    let events = sink.0.lock().unwrap();
    assert!(events.iter().any(|(kind, _)| kind == "task.created"));
}

#[tokio::test]
async fn capture_accepts_the_automation_key_unlike_every_other_direct_write_route() {
    let (app, _sink) = test_app().await;
    let resp = app
        .oneshot(automation_req(
            "POST",
            "/capture",
            json!({"text": "自动采集的一条笔记"}),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "capture is the one direct write this design opens to the automation \
         key too — see the handler doc comment for why"
    );
}

#[tokio::test]
async fn today_must_do_orders_in_progress_before_planned_before_backlog_oldest_first() {
    let (app, _sink) = test_app().await;
    let area = body_json(
        app.clone()
            .oneshot(human_req("POST", "/areas", json!({"title": "工作"})))
            .await
            .unwrap(),
    )
    .await;
    let direction = body_json(
        app.clone()
            .oneshot(human_req(
                "POST",
                "/directions",
                json!({"title": "Q4", "target_window": "2026-Q4", "area_id": area["id"]}),
            ))
            .await
            .unwrap(),
    )
    .await;
    let direction_id = direction["id"].clone();

    let mk_task = |app: axum::Router, title: &str| {
        let direction_id = direction_id.clone();
        let title = title.to_string();
        async move {
            body_json(
                app.oneshot(human_req(
                    "POST",
                    "/tasks",
                    json!({"title": title, "direction_id": direction_id}),
                ))
                .await
                .unwrap(),
            )
            .await
        }
    };

    // Created in this order: backlog task, then a task promoted to
    // in_progress, then a task moved to planned then in_progress isn't
    // needed — a second in_progress task proves the tie-break (oldest of the
    // in_progress tier first), and the backlog task proves the tier itself
    // (it must rank behind BOTH in_progress tasks regardless of creation
    // order — it's created FIRST but must still sort LAST).
    let backlog_task = mk_task(app.clone(), "backlog item").await;
    let first_in_progress = mk_task(app.clone(), "started first").await;
    let second_in_progress = mk_task(app.clone(), "started second").await;

    // backlog -> planned -> in_progress: the matrix has no direct
    // backlog -> in_progress edge.
    for t in [&first_in_progress, &second_in_progress] {
        let id = t["id"].as_str().unwrap();
        for to in ["planned", "in_progress"] {
            let resp = app
                .clone()
                .oneshot(human_req(
                    "PATCH",
                    &format!("/tasks/{id}"),
                    json!({"to": to}),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
    }

    let today = body_json(app.oneshot(get_req("/today")).await.unwrap()).await;
    let must_do: Vec<&str> = today["must_do"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        must_do,
        vec![
            first_in_progress["id"].as_str().unwrap(),
            second_in_progress["id"].as_str().unwrap(),
            backlog_task["id"].as_str().unwrap(),
        ],
        "in_progress tasks (oldest first) must outrank backlog regardless of creation order"
    );
}

#[tokio::test]
async fn today_carry_over_candidates_are_in_progress_tasks_started_before_today() {
    let (app, _sink, store) = test_app_with_store().await;
    let area = body_json(
        app.clone()
            .oneshot(human_req("POST", "/areas", json!({"title": "工作"})))
            .await
            .unwrap(),
    )
    .await;
    let direction = body_json(
        app.clone()
            .oneshot(human_req(
                "POST",
                "/directions",
                json!({"title": "Q4", "target_window": "2026-Q4", "area_id": area["id"]}),
            ))
            .await
            .unwrap(),
    )
    .await;

    let old_task = body_json(
        app.clone()
            .oneshot(human_req(
                "POST",
                "/tasks",
                json!({"title": "stale in-progress work", "direction_id": direction["id"]}),
            ))
            .await
            .unwrap(),
    )
    .await;
    let old_id = old_task["id"].as_str().unwrap();
    for to in ["planned", "in_progress"] {
        let resp = app
            .clone()
            .oneshot(human_req(
                "PATCH",
                &format!("/tasks/{old_id}"),
                json!({"to": to}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
    // Backdate when it STARTED — a real day boundary, not a sleep.
    crate::store::test_hooks::set_task_started_at(&store, old_id, "2020-01-01T00:00:00Z")
        .await
        .unwrap();

    let today_task = body_json(
        app.clone()
            .oneshot(human_req(
                "POST",
                "/tasks",
                json!({"title": "started today", "direction_id": direction["id"]}),
            ))
            .await
            .unwrap(),
    )
    .await;
    let today_id = today_task["id"].as_str().unwrap();
    for to in ["planned", "in_progress"] {
        let resp = app
            .clone()
            .oneshot(human_req(
                "PATCH",
                &format!("/tasks/{today_id}"),
                json!({"to": to}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    let today = body_json(app.oneshot(get_req("/today")).await.unwrap()).await;
    let candidates: Vec<&str> = today["carry_over_candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        candidates,
        vec![old_id],
        "only the task started on an earlier UTC day is a carry-over candidate; \
         one started today (however in-progress) is not yet stale"
    );
}

// ============================================================================
// M2 — Work Pack: Week lifecycle, CreateTasks/CarryOverTask via Proposal,
// week_attention (planned vs. actual, pure event replay for the actual side).
// ============================================================================

fn automation_proposal(body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/proposals")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-sin90-actor-key", AUTOMATION)
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Accept is a human-only action (Codex 2026-09-22 review, High: accepting
/// used to work with either key, letting automation self-approve its own
/// proposal — see `automation_can_submit_but_not_accept_its_own_proposal`).
fn accept_req(id: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/proposals/{id}/accept"))
        .header("x-sin90-actor-key", HUMAN)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn week_full_lifecycle_planning_to_closed() {
    let (app, _sink) = test_app().await;
    let week = body_json(
        app.clone()
            .oneshot(human_req("POST", "/weeks", json!({"iso_week": "2026-W40"})))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(week["status"], "planning");
    let id = week["id"].as_str().unwrap().to_string();

    for to in ["active", "reviewing", "closed"] {
        let resp = app
            .clone()
            .oneshot(human_req(
                "PATCH",
                &format!("/weeks/{id}"),
                json!({"to": to}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "planning->...->{to}");
        let body = body_json(resp).await;
        assert_eq!(body["status"], to);
    }
}

#[tokio::test]
async fn week_illegal_transition_is_rejected_and_no_state_moves() {
    // Regression for "judgement must first be verified": a matrix that only
    // ever sees legal transitions in tests can't tell "correctly permissive"
    // from "checks nothing" apart. Skipping straight from planning to
    // reviewing (over active) must be rejected.
    let (app, _sink) = test_app().await;
    let week = body_json(
        app.clone()
            .oneshot(human_req("POST", "/weeks", json!({"iso_week": "2026-W41"})))
            .await
            .unwrap(),
    )
    .await;
    let id = week["id"].as_str().unwrap().to_string();

    let resp = app
        .clone()
        .oneshot(human_req(
            "PATCH",
            &format!("/weeks/{id}"),
            json!({"to": "reviewing"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // Still `planning` — the rejected PATCH must not have partially applied.
    let weeks = body_json(app.oneshot(get_req("/weeks")).await.unwrap()).await;
    let w = weeks["weeks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["id"] == id)
        .unwrap();
    assert_eq!(w["status"], "planning");
}

/// Shared setup for the two tests below: an Area + Direction (so created
/// tasks are findable via `GET /tasks?direction_id=`, which has no
/// `week_id` filter of its own) and an open Week.
async fn area_direction_and_open_week(app: &axum::Router) -> (String, String) {
    let area = body_json(
        app.clone()
            .oneshot(human_req("POST", "/areas", json!({"title": "工作"})))
            .await
            .unwrap(),
    )
    .await;
    let direction = body_json(
        app.clone()
            .oneshot(human_req(
                "POST",
                "/directions",
                json!({
                    "title": "Q4 交付 v0.5", "target_window": "2026-Q4",
                    "area_id": area["id"].as_str().unwrap()
                }),
            ))
            .await
            .unwrap(),
    )
    .await;
    let week = body_json(
        app.clone()
            .oneshot(human_req("POST", "/weeks", json!({"iso_week": "2026-W42"})))
            .await
            .unwrap(),
    )
    .await;
    (
        direction["id"].as_str().unwrap().to_string(),
        week["id"].as_str().unwrap().to_string(),
    )
}

#[tokio::test]
async fn carry_over_task_via_proposal_leaves_a_traceable_chain_into_the_next_week() {
    let (app, _sink) = test_app().await;
    let (direction_id, week1) = area_direction_and_open_week(&app).await;

    // CreateTasks (design §1.3/§3.3): batch-create into week1, via the
    // Proposal path — this is the ONLY path that sets a task's `week_id`.
    let submit = app
        .clone()
        .oneshot(automation_proposal(json!({
            "id": "p-create", "status": "pending", "source": "local_brain",
            "ops": [{"op": "create_tasks", "week_id": week1,
                     "tasks": [{"title": "写 M2 设计", "direction_id": direction_id}]}],
            "rationale": null
        })))
        .await
        .unwrap();
    assert_eq!(submit.status(), StatusCode::ACCEPTED);
    assert_eq!(
        app.clone()
            .oneshot(accept_req("p-create"))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );

    let tasks = body_json(
        app.clone()
            .oneshot(get_req(&format!("/tasks?direction_id={direction_id}")))
            .await
            .unwrap(),
    )
    .await;
    let tasks = tasks["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 1);
    let task_id = tasks[0]["id"].as_str().unwrap().to_string();
    assert_eq!(tasks[0]["status"], "planned");
    assert_eq!(tasks[0]["week_id"], week1);

    // Advance it partway (direct write — `transition_task` does not gate on
    // week-open status, only `CreateTasks`/`ReorderTasks`/`CarryOverTask` do).
    assert_eq!(
        app.clone()
            .oneshot(human_req(
                "PATCH",
                &format!("/tasks/{task_id}"),
                json!({"to": "in_progress"})
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );

    let week2 = body_json(
        app.clone()
            .oneshot(human_req("POST", "/weeks", json!({"iso_week": "2026-W43"})))
            .await
            .unwrap(),
    )
    .await;
    let week2 = week2["id"].as_str().unwrap().to_string();

    let submit = app
        .clone()
        .oneshot(automation_proposal(json!({
            "id": "p-carry", "status": "pending", "source": "local_brain",
            "ops": [{"op": "carry_over_task", "task_id": task_id, "to_week": week2}],
            "rationale": null
        })))
        .await
        .unwrap();
    assert_eq!(submit.status(), StatusCode::ACCEPTED);
    assert_eq!(
        app.clone()
            .oneshot(accept_req("p-carry"))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );

    let tasks = body_json(
        app.oneshot(get_req(&format!("/tasks?direction_id={direction_id}")))
            .await
            .unwrap(),
    )
    .await;
    let tasks = tasks["tasks"].as_array().unwrap();
    assert_eq!(
        tasks.len(),
        2,
        "carry-over closes the source task AND creates a fresh one — the row count must grow, not stay flat"
    );
    let old = tasks.iter().find(|t| t["id"] == task_id).unwrap();
    assert_eq!(
        old["status"], "carried_over",
        "the source task is closed, not deleted — it's the chain's first link"
    );
    let new = tasks.iter().find(|t| t["id"] != task_id).unwrap();
    assert_eq!(
        new["carried_from"], task_id,
        "the new task must point back to the one it replaced — this is the traceable chain, not a fresh unrelated task"
    );
    assert_eq!(new["week_id"], week2);
    assert_eq!(new["status"], "planned");
}

#[tokio::test]
async fn week_attention_computes_planned_vs_actual_purely_from_event_replay() {
    let (app, _sink) = test_app().await;
    let (direction_id, week_id) = area_direction_and_open_week(&app).await;

    let submit = app
        .clone()
        .oneshot(automation_proposal(json!({
            "id": "p1", "status": "pending", "source": "local_brain",
            "ops": [{"op": "create_tasks", "week_id": week_id,
                     "tasks": [{"title": "deep work", "direction_id": direction_id}]}],
            "rationale": null
        })))
        .await
        .unwrap();
    assert_eq!(submit.status(), StatusCode::ACCEPTED);
    app.clone().oneshot(accept_req("p1")).await.unwrap();

    let tasks = body_json(
        app.clone()
            .oneshot(get_req(&format!("/tasks?direction_id={direction_id}")))
            .await
            .unwrap(),
    )
    .await;
    let task_id = tasks["tasks"][0]["id"].as_str().unwrap().to_string();

    // A block planned for 90 minutes, in this week's task — before it's
    // completed, `actual_min` must be 0: the plan exists, nothing happened yet.
    let block = body_json(
        app.clone()
            .oneshot(human_req(
                "POST",
                "/schedule-blocks",
                json!({"direction_id": direction_id, "task_id": task_id, "planned_minutes": 90}),
            ))
            .await
            .unwrap(),
    )
    .await;
    let block_id = block["id"].as_str().unwrap().to_string();

    let a = body_json(
        app.clone()
            .oneshot(get_req(&format!("/weeks/{week_id}/attention")))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(a["planned_min"], 90);
    assert_eq!(a["actual_min"], 0);
    assert_eq!(a["deviation_min"], -90);

    // A SECOND block, on a task with NO week (an M1-style inbox task) — this
    // is the negative control for "the query is scoped by week, not just
    // summing every completed block system-wide": it must complete without
    // moving week_id's numbers at all.
    let unrelated_task = body_json(
        app.clone()
            .oneshot(human_req(
                "POST",
                "/tasks",
                json!({"title": "unrelated inbox item"}),
            ))
            .await
            .unwrap(),
    )
    .await;
    let unrelated_task_id = unrelated_task["id"].as_str().unwrap().to_string();
    let unrelated_block = body_json(
        app.clone()
            .oneshot(human_req(
                "POST",
                "/schedule-blocks",
                json!({"task_id": unrelated_task_id, "planned_minutes": 500}),
            ))
            .await
            .unwrap(),
    )
    .await;
    let unrelated_block_id = unrelated_block["id"].as_str().unwrap().to_string();
    for to in ["started", "completed"] {
        app.clone()
            .oneshot(human_req(
                "PATCH",
                &format!("/schedule-blocks/{unrelated_block_id}"),
                json!({"to": to}),
            ))
            .await
            .unwrap();
    }
    let a = body_json(
        app.clone()
            .oneshot(get_req(&format!("/weeks/{week_id}/attention")))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        a["planned_min"], 90,
        "a block on a task outside this week must not inflate this week's planned total"
    );
    assert_eq!(
        a["actual_min"], 0,
        "...nor its actual total, even after that unrelated block completes"
    );

    // Now complete THIS week's block — both numbers must move, and only now.
    for to in ["started", "completed"] {
        let resp = app
            .clone()
            .oneshot(human_req(
                "PATCH",
                &format!("/schedule-blocks/{block_id}"),
                json!({"to": to}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    let a = body_json(
        app.oneshot(get_req(&format!("/weeks/{week_id}/attention")))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(a["planned_min"], 90);
    assert_eq!(
        a["actual_min"], 90,
        "actual_min must come from the completed-block event, not from re-reading planned_minutes off the live row"
    );
    assert_eq!(a["deviation_min"], 0);
}

#[tokio::test]
async fn week_attention_for_an_unknown_week_is_404_not_a_silent_zero() {
    // Without this check, `week_attention` would happily run its two
    // COALESCE(...,0) aggregates against a week_id that matches nothing and
    // report 0/0/0 for a week that was never created — indistinguishable
    // from a real, empty week. That's a client-facing lie, not "no data yet".
    let (app, _sink) = test_app().await;
    let resp = app
        .oneshot(get_req("/weeks/does-not-exist/attention"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ============================================================================
// Codex 2026-09-22 review — store invariants (Medium #4, #5, #11)
// ============================================================================

async fn patch_status(app: &axum::Router, uri: &str, to: &str) -> StatusCode {
    app.clone()
        .oneshot(human_req("PATCH", uri, json!({"to": to})))
        .await
        .unwrap()
        .status()
}

/// Medium #4: the direct `PATCH /tasks/{id}` path used to skip the
/// "week must be open" invariant the Proposal path enforces, so a task in a
/// reviewing/closed week could still change.
#[tokio::test]
async fn direct_task_transition_is_refused_once_its_week_is_no_longer_open() {
    let (app, _sink) = test_app().await;
    let (direction_id, week_id) = area_direction_and_open_week(&app).await;
    let submit = app
        .clone()
        .oneshot(automation_proposal(json!({
            "id": "p-two", "status": "pending", "source": "local_brain",
            "ops": [{"op": "create_tasks", "week_id": week_id,
                     "tasks": [{"title": "a", "direction_id": direction_id},
                               {"title": "b", "direction_id": direction_id}]}],
            "rationale": null
        })))
        .await
        .unwrap();
    assert_eq!(submit.status(), StatusCode::ACCEPTED);
    assert_eq!(
        app.clone()
            .oneshot(accept_req("p-two"))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let tasks = body_json(
        app.clone()
            .oneshot(get_req(&format!("/tasks?direction_id={direction_id}")))
            .await
            .unwrap(),
    )
    .await;
    let ids: Vec<String> = tasks["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), 2);

    // Control: while the week is open, a direct transition works.
    assert_eq!(
        patch_status(&app, &format!("/tasks/{}", ids[0]), "in_progress").await,
        StatusCode::OK
    );

    for to in ["active", "reviewing"] {
        assert_eq!(
            patch_status(&app, &format!("/weeks/{week_id}"), to).await,
            StatusCode::OK
        );
    }
    assert_eq!(
        patch_status(&app, &format!("/tasks/{}", ids[1]), "in_progress").await,
        StatusCode::CONFLICT,
        "a task in a reviewing week must not change through the direct path"
    );
    let after = body_json(
        app.oneshot(get_req(&format!("/tasks?direction_id={direction_id}")))
            .await
            .unwrap(),
    )
    .await;
    let t1 = after["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == ids[1].as_str())
        .unwrap();
    assert_eq!(
        t1["status"], "planned",
        "refused transition must not move state"
    );
}

/// Medium #5: the Proposal `CreateArea` op inserted the bare slug and failed
/// on the UNIQUE column when a same-titled area existed; direct creation
/// already suffixed `-2`, `-3`.
#[tokio::test]
async fn proposal_created_area_gets_a_suffixed_slug_on_collision() {
    let (app, _sink) = test_app().await;
    assert_eq!(
        app.clone()
            .oneshot(human_req("POST", "/areas", json!({"title": "Health"})))
            .await
            .unwrap()
            .status(),
        StatusCode::CREATED
    );
    let submit = app
        .clone()
        .oneshot(automation_proposal(json!({
            "id": "p-area", "status": "pending", "source": "local_brain",
            "ops": [{"op": "create_area", "title": "Health"}], "rationale": null
        })))
        .await
        .unwrap();
    assert_eq!(submit.status(), StatusCode::ACCEPTED);
    assert_eq!(
        app.clone()
            .oneshot(accept_req("p-area"))
            .await
            .unwrap()
            .status(),
        StatusCode::OK,
        "a valid proposal must apply even when its slug collides"
    );
    let areas = body_json(app.oneshot(get_req("/areas")).await.unwrap()).await;
    let mut slugs: Vec<&str> = areas["areas"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["slug"].as_str().unwrap())
        .collect();
    slugs.sort_unstable();
    assert_eq!(slugs, ["health", "health-2"]);
}

/// Medium #11: `iso_week` was stored verbatim with no uniqueness.
#[tokio::test]
async fn week_labels_are_validated_canonicalized_and_unique() {
    let (app, _sink) = test_app().await;
    let post = |label: &str| human_req("POST", "/weeks", json!({"iso_week": label}));
    for bad in ["garbage", "2026-W99", "2026-W00", "2021-W53"] {
        assert_eq!(
            app.clone().oneshot(post(bad)).await.unwrap().status(),
            StatusCode::BAD_REQUEST,
            "{bad} must be rejected"
        );
    }
    let resp = app.clone().oneshot(post("2026-w42")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(body_json(resp).await["iso_week"], "2026-W42");
    assert_eq!(
        app.clone()
            .oneshot(post("2026-W42"))
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT,
        "a second row for the same calendar week must be refused"
    );
    let weeks = body_json(app.oneshot(get_req("/weeks")).await.unwrap()).await;
    assert_eq!(weeks["weeks"].as_array().unwrap().len(), 1);
}

// ============================================================================
// Codex 2026-09-22 review — carry-over (Medium #8, #9)
// ============================================================================

/// Medium #8: the rule used `created_at`, so a months-old backlog item
/// started TODAY was immediately offered as carry-over.
#[tokio::test]
async fn an_old_task_started_today_is_not_a_carry_over_candidate() {
    let (app, _sink, store) = test_app_with_store().await;
    let direction = body_json(
        app.clone()
            .oneshot(human_req(
                "POST",
                "/directions",
                json!({"title": "d", "target_window": "2026-Q4"}),
            ))
            .await
            .unwrap(),
    )
    .await;
    let task = body_json(
        app.clone()
            .oneshot(human_req(
                "POST",
                "/tasks",
                json!({"title": "old backlog item", "direction_id": direction["id"]}),
            ))
            .await
            .unwrap(),
    )
    .await;
    let id = task["id"].as_str().unwrap();
    crate::store::test_hooks::set_task_created_at(&store, id, "2020-01-01T00:00:00Z")
        .await
        .unwrap();
    for to in ["planned", "in_progress"] {
        assert_eq!(
            app.clone()
                .oneshot(human_req(
                    "PATCH",
                    &format!("/tasks/{id}"),
                    json!({"to": to})
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
    }
    let today = body_json(app.clone().oneshot(get_req("/today")).await.unwrap()).await;
    assert!(
        today["carry_over_candidates"]
            .as_array()
            .unwrap()
            .is_empty(),
        "started today → not a carry-over candidate, however old: {today}"
    );

    // Control: once its start is on an earlier day, it IS a candidate.
    crate::store::test_hooks::set_task_started_at(&store, id, "2020-01-02T00:00:00Z")
        .await
        .unwrap();
    let today = body_json(app.oneshot(get_req("/today")).await.unwrap()).await;
    assert_eq!(today["carry_over_candidates"][0]["id"], id);
}

/// Medium #9: the carried copy hard-coded kind=other, energy=mid,
/// est_minutes=NULL, parent_task_id=NULL.
#[tokio::test]
async fn carry_over_keeps_kind_energy_estimate_and_project() {
    let (app, _sink) = test_app().await;
    let (direction_id, _week1) = area_direction_and_open_week(&app).await;
    let project = body_json(
        app.clone()
            .oneshot(human_req(
                "POST",
                "/tasks",
                json!({"title": "project", "direction_id": direction_id}),
            ))
            .await
            .unwrap(),
    )
    .await;
    let child = body_json(
        app.clone()
            .oneshot(human_req(
                "POST",
                "/tasks",
                json!({
                    "title": "write the design", "direction_id": direction_id,
                    "parent_task_id": project["id"], "kind": "deep_work",
                    "energy": "high", "est_minutes": 90
                }),
            ))
            .await
            .unwrap(),
    )
    .await;
    let child_id = child["id"].as_str().unwrap().to_string();
    // Only planned/in-progress work can be carried over (transition matrix).
    assert_eq!(
        app.clone()
            .oneshot(human_req(
                "PATCH",
                &format!("/tasks/{child_id}"),
                json!({"to": "planned"})
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let week2 = body_json(
        app.clone()
            .oneshot(human_req("POST", "/weeks", json!({"iso_week": "2026-W43"})))
            .await
            .unwrap(),
    )
    .await;
    let submit = app
        .clone()
        .oneshot(automation_proposal(json!({
            "id": "p-carry-meta", "status": "pending", "source": "local_brain",
            "ops": [{"op": "carry_over_task", "task_id": child_id, "to_week": week2["id"]}],
            "rationale": null
        })))
        .await
        .unwrap();
    assert_eq!(submit.status(), StatusCode::ACCEPTED);
    assert_eq!(
        app.clone()
            .oneshot(accept_req("p-carry-meta"))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let tasks = body_json(
        app.oneshot(get_req(&format!("/tasks?direction_id={direction_id}")))
            .await
            .unwrap(),
    )
    .await;
    let new = tasks["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["carried_from"] == child_id.as_str())
        .expect("the carried copy exists");
    assert_eq!(new["kind"], "deep_work");
    assert_eq!(new["energy"], "high");
    assert_eq!(new["est_minutes"], 90);
    assert_eq!(
        new["parent_task_id"], project["id"],
        "stays in the same project"
    );
    assert_eq!(new["week_id"], week2["id"]);
}

// ============================================================================
// T3.4.1 — Rhythm open: POST/GET /rhythms direct-write + read; adjustment
// stays behind the existing proposal gate (AdjustRhythm via POST /proposals +
// human accept). No new Op, no direct-write adjust route (spec.md "Rhythm
// 路由", tasks.md T3.4.1).
//
// Opus 2026-09-23 review (H2): everything below lives in `mod rhythm` so
// `cargo test --lib http::tests::rhythm -- --list` matches every test here;
// individual names drop the now-redundant `rhythm_` prefix the module
// already provides.
// ============================================================================

mod rhythm {
    use super::*;

    /// Shared setup: two Directions to allocate a Rhythm across.
    async fn two_directions(app: &axum::Router) -> (String, String) {
        let d1 = body_json(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    "/directions",
                    json!({"title": "d1", "target_window": "2026-Q4"}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let d2 = body_json(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    "/directions",
                    json!({"title": "d2", "target_window": "2026-Q4"}),
                ))
                .await
                .unwrap(),
        )
        .await;
        (
            d1["id"].as_str().unwrap().to_string(),
            d2["id"].as_str().unwrap().to_string(),
        )
    }

    /// Full path: create -> submit AdjustRhythm -> human accept -> GET shows
    /// `adjusted` with the new allocations. This is the T3.4.1 acceptance line
    /// itself: adjustment only ever lands through the proposal gate.
    #[tokio::test]
    async fn created_then_adjusted_via_proposal_and_accept() {
        let (app, _sink) = test_app().await;
        let (d1, d2) = two_directions(&app).await;

        let created = body_json(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    "/rhythms",
                    json!({"allocations": [{"direction_id": d1, "pct": 60}]}),
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(created["status"], "active");
        let rhythm_id = created["id"].as_str().unwrap().to_string();

        let submit = app
            .clone()
            .oneshot(automation_proposal(json!({
                "id": "p-adjust", "status": "pending", "source": "local_brain",
                "ops": [{"op": "adjust_rhythm", "rhythm_id": rhythm_id,
                         "new_alloc": [{"direction_id": d1, "pct": 30}, {"direction_id": d2, "pct": 40}]}],
                "rationale": null
            })))
            .await
            .unwrap();
        assert_eq!(submit.status(), StatusCode::ACCEPTED);
        assert_eq!(
            app.clone()
                .oneshot(accept_req("p-adjust"))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );

        let fetched = body_json(
            app.oneshot(get_req(&format!("/rhythms/{rhythm_id}")))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(fetched["status"], "adjusted");
        let allocs = fetched["allocations"].as_array().unwrap();
        assert_eq!(allocs.len(), 2);
        assert!(allocs
            .iter()
            .any(|a| a["direction_id"] == d1 && a["pct"] == 30));
        assert!(allocs
            .iter()
            .any(|a| a["direction_id"] == d2 && a["pct"] == 40));
    }

    /// The direct-write gate itself: automation may not create a Rhythm, only
    /// a human key may (positive control alongside the negative).
    #[tokio::test]
    async fn direct_create_requires_human_key() {
        let (app, _sink) = test_app().await;
        let (d1, _d2) = two_directions(&app).await;
        let body = json!({"allocations": [{"direction_id": d1, "pct": 50}]});

        let resp = app
            .clone()
            .oneshot(automation_req("POST", "/rhythms", body.clone()))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "automation key must not be able to POST /rhythms directly"
        );

        // Positive control: the human key succeeds with the same body.
        let resp = app
            .oneshot(human_req("POST", "/rhythms", body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    /// The automation key must not be able to accept its own AdjustRhythm
    /// proposal either — same boundary as every other proposal kind
    /// (`automation_can_submit_but_not_accept_its_own_proposal`), pinned
    /// again here because T3.4.1 explicitly routes adjustment through this
    /// gate.
    #[tokio::test]
    async fn automation_key_cannot_accept_adjust_proposal() {
        let (app, _sink) = test_app().await;
        let (d1, _d2) = two_directions(&app).await;
        let created = body_json(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    "/rhythms",
                    json!({"allocations": [{"direction_id": d1, "pct": 50}]}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let rhythm_id = created["id"].as_str().unwrap().to_string();

        let submit = app
            .clone()
            .oneshot(automation_proposal(json!({
                "id": "p-self", "status": "pending", "source": "local_brain",
                "ops": [{"op": "adjust_rhythm", "rhythm_id": rhythm_id,
                         "new_alloc": [{"direction_id": d1, "pct": 10}]}],
                "rationale": null
            })))
            .await
            .unwrap();
        assert_eq!(submit.status(), StatusCode::ACCEPTED);

        let accept_as_automation = Request::builder()
            .method("POST")
            .uri("/proposals/p-self/accept")
            .header("x-sin90-actor-key", AUTOMATION)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone()
                .oneshot(accept_as_automation)
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN,
            "automation must not be able to self-approve a Rhythm adjustment"
        );

        // Negative control: still `active`, not `adjusted` — the rejected
        // accept must not have moved state.
        let fetched = body_json(
            app.oneshot(get_req(&format!("/rhythms/{rhythm_id}")))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(fetched["status"], "active");
    }

    // ---- direct-write structural checks (core::check_alloc, via 400) -------

    #[tokio::test]
    async fn create_rejects_pct_sum_over_100() {
        let (app, _sink) = test_app().await;
        let (d1, d2) = two_directions(&app).await;
        let resp = app
            .oneshot(human_req(
                "POST",
                "/rhythms",
                json!({"allocations": [{"direction_id": d1, "pct": 60}, {"direction_id": d2, "pct": 41}]}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], "invalid_request");
    }

    /// Positive control for the sum check above: a sum of exactly 100 is
    /// legal.
    #[tokio::test]
    async fn create_allows_pct_sum_of_exactly_100() {
        let (app, _sink) = test_app().await;
        let (d1, d2) = two_directions(&app).await;
        let resp = app
            .oneshot(human_req(
                "POST",
                "/rhythms",
                json!({"allocations": [{"direction_id": d1, "pct": 60}, {"direction_id": d2, "pct": 40}]}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    /// pct=0 is out of the 1..=100 structural range — 400 (positive control:
    /// pct=1, the range's own lower bound, succeeds). Opus 2026-09-23 review
    /// M3.
    #[tokio::test]
    async fn create_rejects_pct_zero() {
        let (app, _sink) = test_app().await;
        let (d1, _d2) = two_directions(&app).await;
        let resp = app
            .clone()
            .oneshot(human_req(
                "POST",
                "/rhythms",
                json!({"allocations": [{"direction_id": d1.clone(), "pct": 0}]}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], "invalid_request");

        let resp = app
            .oneshot(human_req(
                "POST",
                "/rhythms",
                json!({"allocations": [{"direction_id": d1, "pct": 1}]}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    /// Opus 2026-09-23 review M1: an empty `allocations` list is a
    /// structural error (`core::check_alloc::EmptyAllocations`), not a
    /// legal "no-op" rhythm — 400 on the direct-write path (positive
    /// control: a single legal allocation succeeds).
    #[tokio::test]
    async fn create_rejects_empty_allocations() {
        let (app, _sink) = test_app().await;
        let resp = app
            .clone()
            .oneshot(human_req("POST", "/rhythms", json!({"allocations": []})))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], "invalid_request");

        let (d1, _d2) = two_directions(&app).await;
        let resp = app
            .oneshot(human_req(
                "POST",
                "/rhythms",
                json!({"allocations": [{"direction_id": d1, "pct": 1}]}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn create_rejects_duplicate_direction() {
        let (app, _sink) = test_app().await;
        let (d1, _d2) = two_directions(&app).await;
        let resp = app
            .oneshot(human_req(
                "POST",
                "/rhythms",
                json!({"allocations": [{"direction_id": d1, "pct": 10}, {"direction_id": d1, "pct": 20}]}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], "invalid_request");
    }

    #[tokio::test]
    async fn create_rejects_unknown_field() {
        let (app, _sink) = test_app().await;
        let (d1, _d2) = two_directions(&app).await;
        let resp = app
            .oneshot(human_req(
                "POST",
                "/rhythms",
                json!({"allocations": [{"direction_id": d1, "pct": 10}], "note": "typo field"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// Opus 2026-09-23 review M2: an unknown direction is 404 — the same
    /// "referenced entity does not exist" convention `create_task`/
    /// `create_block` already use — NOT 400 (400 is reserved for the
    /// structural checks above, which never touch the database).
    #[tokio::test]
    async fn create_rejects_unknown_direction() {
        let (app, _sink) = test_app().await;
        let resp = app
            .clone()
            .oneshot(human_req(
                "POST",
                "/rhythms",
                json!({"allocations": [{"direction_id": "does-not-exist", "pct": 50}]}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], "not_found");

        // Positive control: a real direction succeeds.
        let (d1, _d2) = two_directions(&app).await;
        let resp = app
            .oneshot(human_req(
                "POST",
                "/rhythms",
                json!({"allocations": [{"direction_id": d1, "pct": 50}]}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn get_unknown_id_is_404() {
        let (app, _sink) = test_app().await;
        let resp = app
            .oneshot(get_req("/rhythms/does-not-exist"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ---- H1: AdjustRhythm's apply arm must enforce the SAME invariants as
    // create_rhythm — direction existence and pct range — not just the
    // pure `validate()` structural check. ------------------------------------

    /// Opus 2026-09-23 review H1: `AdjustRhythm`'s apply used to skip the
    /// direction-existence check `create_rhythm` already enforced — accept
    /// would silently move a Rhythm to `adjusted` referencing a direction
    /// that does not exist. Now `require_directions_exist` runs inside the
    /// SAME apply transaction, so this is 404, and the rhythm never moves.
    #[tokio::test]
    async fn adjust_rejects_unknown_direction_rhythm_stays_active_positive_control_legal_adjustment(
    ) {
        let (app, _sink) = test_app().await;
        let (d1, d2) = two_directions(&app).await;
        let created = body_json(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    "/rhythms",
                    json!({"allocations": [{"direction_id": d1, "pct": 50}]}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let rhythm_id = created["id"].as_str().unwrap().to_string();

        let submit = app
            .clone()
            .oneshot(automation_proposal(json!({
                "id": "p-ghost", "status": "pending", "source": "local_brain",
                "ops": [{"op": "adjust_rhythm", "rhythm_id": rhythm_id,
                         "new_alloc": [{"direction_id": "does-not-exist", "pct": 50}]}],
                "rationale": null
            })))
            .await
            .unwrap();
        assert_eq!(submit.status(), StatusCode::ACCEPTED);
        let accept = app.clone().oneshot(accept_req("p-ghost")).await.unwrap();
        assert_eq!(
            accept.status(),
            StatusCode::NOT_FOUND,
            "an AdjustRhythm referencing an unknown direction must be rejected, not applied"
        );

        // Negative control: the rejected accept must not have moved state.
        let fetched = body_json(
            app.clone()
                .oneshot(get_req(&format!("/rhythms/{rhythm_id}")))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(fetched["status"], "active");

        // Positive control: a legal adjustment (existing direction) applies.
        let submit = app
            .clone()
            .oneshot(automation_proposal(json!({
                "id": "p-legal", "status": "pending", "source": "local_brain",
                "ops": [{"op": "adjust_rhythm", "rhythm_id": rhythm_id,
                         "new_alloc": [{"direction_id": d2, "pct": 30}]}],
                "rationale": null
            })))
            .await
            .unwrap();
        assert_eq!(submit.status(), StatusCode::ACCEPTED);
        assert_eq!(
            app.clone()
                .oneshot(accept_req("p-legal"))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let fetched = body_json(
            app.oneshot(get_req(&format!("/rhythms/{rhythm_id}")))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(fetched["status"], "adjusted");
    }

    /// Opus 2026-09-23 review H1/M1: `AdjustRhythm`'s pure `validate()`
    /// already rejects pct=0 via the shared `core::check_alloc`. Rebased onto
    /// #29 (SFU-10, "提交即校验"): `submit_proposal` now runs that same
    /// `validate()` at SUBMIT time, before anything is persisted — so pct=0
    /// is now 422 at submit, not merely at accept, and the proposal never
    /// exists at all. This pins the new-and-correct place it is caught, plus
    /// the legal-adjustment positive control.
    #[tokio::test]
    async fn adjust_rejects_pct_zero_positive_control_legal_adjustment() {
        let (app, _sink) = test_app().await;
        let (d1, _d2) = two_directions(&app).await;
        let created = body_json(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    "/rhythms",
                    json!({"allocations": [{"direction_id": d1.clone(), "pct": 50}]}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let rhythm_id = created["id"].as_str().unwrap().to_string();

        let submit = app
            .clone()
            .oneshot(automation_proposal(json!({
                "id": "p-pct0", "status": "pending", "source": "local_brain",
                "ops": [{"op": "adjust_rhythm", "rhythm_id": rhythm_id.clone(),
                         "new_alloc": [{"direction_id": d1.clone(), "pct": 0}]}],
                "rationale": null
            })))
            .await
            .unwrap();
        assert_eq!(
            submit.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "pct=0 must be rejected at submit time (SFU-10), not persisted as pending"
        );
        assert_eq!(
            app.clone()
                .oneshot(get_req("/proposals/p-pct0"))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND,
            "a submit-time-rejected proposal must not exist at all"
        );

        let fetched = body_json(
            app.clone()
                .oneshot(get_req(&format!("/rhythms/{rhythm_id}")))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(fetched["status"], "active");

        // Positive control: pct in range applies.
        let submit = app
            .clone()
            .oneshot(automation_proposal(json!({
                "id": "p-legal", "status": "pending", "source": "local_brain",
                "ops": [{"op": "adjust_rhythm", "rhythm_id": rhythm_id,
                         "new_alloc": [{"direction_id": d1, "pct": 5}]}],
                "rationale": null
            })))
            .await
            .unwrap();
        assert_eq!(submit.status(), StatusCode::ACCEPTED);
        assert_eq!(
            app.clone()
                .oneshot(accept_req("p-legal"))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let fetched = body_json(
            app.oneshot(get_req(&format!("/rhythms/{rhythm_id}")))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(fetched["status"], "adjusted");
    }

    /// Opus 2026-09-23 review M1: an empty `new_alloc` is rejected on the
    /// proposal path too (`validate()` → `check_alloc` →
    /// `ProposalError::EmptyAllocations`). Rebased onto #29 (SFU-10, "提交即
    /// 校验"): `submit_proposal` now runs `validate()` at SUBMIT time, so this
    /// is 422 at submit and the proposal is never persisted — the rhythm
    /// stays `active`; positive control: a single legal allocation applies.
    #[tokio::test]
    async fn adjust_rejects_empty_allocations_positive_control_legal_adjustment() {
        let (app, _sink) = test_app().await;
        let (d1, _d2) = two_directions(&app).await;
        let created = body_json(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    "/rhythms",
                    json!({"allocations": [{"direction_id": d1.clone(), "pct": 50}]}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let rhythm_id = created["id"].as_str().unwrap().to_string();

        let submit = app
            .clone()
            .oneshot(automation_proposal(json!({
                "id": "p-empty", "status": "pending", "source": "local_brain",
                "ops": [{"op": "adjust_rhythm", "rhythm_id": rhythm_id, "new_alloc": []}],
                "rationale": null
            })))
            .await
            .unwrap();
        assert_eq!(
            submit.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "an empty new_alloc must be rejected at submit time (SFU-10), not persisted"
        );
        assert_eq!(
            app.clone()
                .oneshot(get_req("/proposals/p-empty"))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND,
            "a submit-time-rejected proposal must not exist at all"
        );

        let fetched = body_json(
            app.clone()
                .oneshot(get_req(&format!("/rhythms/{rhythm_id}")))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(fetched["status"], "active");

        let submit = app
            .clone()
            .oneshot(automation_proposal(json!({
                "id": "p-legal", "status": "pending", "source": "local_brain",
                "ops": [{"op": "adjust_rhythm", "rhythm_id": rhythm_id,
                         "new_alloc": [{"direction_id": d1, "pct": 10}]}],
                "rationale": null
            })))
            .await
            .unwrap();
        assert_eq!(submit.status(), StatusCode::ACCEPTED);
        assert_eq!(
            app.clone()
                .oneshot(accept_req("p-legal"))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let fetched = body_json(
            app.oneshot(get_req(&format!("/rhythms/{rhythm_id}")))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(fetched["status"], "adjusted");
    }

    // ---- M3: list + the created event ---------------------------------------

    #[tokio::test]
    async fn list_is_empty_then_newest_first() {
        let (app, _sink) = test_app().await;
        let empty = body_json(app.clone().oneshot(get_req("/rhythms")).await.unwrap()).await;
        assert_eq!(empty, json!({"rhythms": []}));

        let (d1, d2) = two_directions(&app).await;
        let first = body_json(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    "/rhythms",
                    json!({"allocations": [{"direction_id": d1, "pct": 10}]}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let second = body_json(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    "/rhythms",
                    json!({"allocations": [{"direction_id": d2, "pct": 20}]}),
                ))
                .await
                .unwrap(),
        )
        .await;

        let listed = body_json(app.oneshot(get_req("/rhythms")).await.unwrap()).await;
        let rows = listed["rhythms"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0]["id"], second["id"],
            "newest first, same convention as every other list_* route"
        );
        assert_eq!(rows[1]["id"], first["id"]);
    }

    /// `rhythm.created` fires exactly once per create, with `to_state` =
    /// `active` — checked both via `/events` (the source of truth) and the
    /// in-process `EventSink` the HTTP layer also emits to. Mutation: delete
    /// the `append_event` call in `create_rhythm` → this goes red (see
    /// commit body for the run).
    #[tokio::test]
    async fn create_emits_exactly_one_created_event_with_active_to_state() {
        let (app, sink) = test_app().await;
        let (d1, _d2) = two_directions(&app).await;
        let created = body_json(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    "/rhythms",
                    json!({"allocations": [{"direction_id": d1, "pct": 10}]}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let rhythm_id = created["id"].as_str().unwrap().to_string();

        let events = body_json(
            app.oneshot(get_req(&format!(
                "/events?entity=rhythm&entity_id={rhythm_id}"
            )))
            .await
            .unwrap(),
        )
        .await;
        let rows = events["events"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0]["kind"], "created");
        assert_eq!(rows[0]["to_state"], "active");

        assert!(
            sink.0
                .lock()
                .unwrap()
                .iter()
                .any(|(kind, payload)| kind == "rhythm.created" && payload["id"] == rhythm_id),
            "EventSink must have received rhythm.created for this id"
        );
    }
}

// ============================================================================
// M3 — Routine routes (T3.1.2). `cargo test --lib http::tests::routine`.
// ============================================================================

mod routine {
    use super::*;

    /// A minimal, legal `POST /routines` body — `0 7 * * MON,WED,FRI` is the
    /// spec.md example (weekday NAMES, comma list; digits are rejected —
    /// `not_a_cron_positive_control` below is the positive control that this
    /// value itself is accepted).
    fn new_routine_body() -> Value {
        json!({"title": "Morning run", "kind": "exercise", "cron": "0 7 * * MON,WED,FRI"})
    }

    // ---- actor-key gate: automation -> 403, human -> 2xx (positive control) --

    #[tokio::test]
    async fn create_routine_requires_human_key() {
        let (app, _sink) = test_app().await;
        let resp = app
            .clone()
            .oneshot(automation_req("POST", "/routines", new_routine_body()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Positive control: the human key succeeds with the same body.
        let resp = app
            .oneshot(human_req("POST", "/routines", new_routine_body()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn update_routine_requires_human_key() {
        let (app, _sink) = test_app().await;
        let created = body_json(
            app.clone()
                .oneshot(human_req("POST", "/routines", new_routine_body()))
                .await
                .unwrap(),
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();

        let resp = app
            .clone()
            .oneshot(automation_req(
                "PATCH",
                &format!("/routines/{id}"),
                json!({"title": "hacked"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Positive control: the human key succeeds with the same body.
        let resp = app
            .oneshot(human_req(
                "PATCH",
                &format!("/routines/{id}"),
                json!({"title": "renamed"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn transition_routine_requires_human_key() {
        let (app, _sink) = test_app().await;
        let created = body_json(
            app.clone()
                .oneshot(human_req("POST", "/routines", new_routine_body()))
                .await
                .unwrap(),
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();

        let resp = app
            .clone()
            .oneshot(automation_req(
                "POST",
                &format!("/routines/{id}/transition"),
                json!({"to": "paused"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Positive control: the human key succeeds with the same body.
        let resp = app
            .oneshot(human_req(
                "POST",
                &format!("/routines/{id}/transition"),
                json!({"to": "paused"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ---- 400: unknown field, illegal cron; positive control: legal cron ------

    #[tokio::test]
    async fn create_routine_rejects_unknown_field() {
        let (app, _sink) = test_app().await;
        let mut body = new_routine_body();
        body["nope"] = json!(1);
        let resp = app
            .oneshot(human_req("POST", "/routines", body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_routine_rejects_posix_digit_weekday_and_day_and_weekday_both_restricted() {
        let (app, _sink) = test_app().await;

        // POSIX-style digit weekday: cron 0.15 uses 1=Sun, not POSIX's
        // 0/7=Sun — a digit would silently mean the wrong day (design §M3 /
        // spec.md's H1).
        let mut digit_weekday = new_routine_body();
        digit_weekday["cron"] = json!("0 7 * * 1-5");
        let resp = app
            .clone()
            .oneshot(human_req("POST", "/routines", digit_weekday))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Day-of-month AND day-of-week both restricted: cron 0.15 ANDs them,
        // POSIX ORs them — ambiguous across the two dialects, rejected.
        let mut both_restricted = new_routine_body();
        both_restricted["cron"] = json!("0 7 1 * MON");
        let resp = app
            .clone()
            .oneshot(human_req("POST", "/routines", both_restricted))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Positive control: the spec.md example (weekday names, day-of-month
        // left at `*`) is accepted.
        let resp = app
            .oneshot(human_req("POST", "/routines", new_routine_body()))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::CREATED,
            "a legal cron string must still be accepted"
        );
    }

    // ---- PATCH: null clears target_count, absent leaves it unchanged ---------

    #[tokio::test]
    async fn patch_null_target_count_clears_it() {
        let (app, _sink) = test_app().await;
        let mut body = new_routine_body();
        body["target_count"] = json!(3);
        let created = body_json(
            app.clone()
                .oneshot(human_req("POST", "/routines", body))
                .await
                .unwrap(),
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();
        assert_eq!(created["target_count"], json!(3));

        let patched = body_json(
            app.oneshot(human_req(
                "PATCH",
                &format!("/routines/{id}"),
                json!({"target_count": null}),
            ))
            .await
            .unwrap(),
        )
        .await;
        assert!(
            patched["target_count"].is_null(),
            "an explicit null must clear target_count: {patched:?}"
        );
    }

    #[tokio::test]
    async fn patch_absent_target_count_leaves_it_unchanged() {
        let (app, _sink) = test_app().await;
        let mut body = new_routine_body();
        body["target_count"] = json!(3);
        let created = body_json(
            app.clone()
                .oneshot(human_req("POST", "/routines", body))
                .await
                .unwrap(),
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();

        // No `target_count` key at all — must leave the existing value alone.
        let patched = body_json(
            app.oneshot(human_req(
                "PATCH",
                &format!("/routines/{id}"),
                json!({"title": "renamed"}),
            ))
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(
            patched["target_count"],
            json!(3),
            "an absent key must not touch target_count: {patched:?}"
        );
        assert_eq!(patched["title"], "renamed");
    }

    // ---- retired is a closed door: PATCH -> 409, transition retired->active -> 409 --

    #[tokio::test]
    async fn patch_on_retired_routine_is_409() {
        let (app, _sink) = test_app().await;
        let created = body_json(
            app.clone()
                .oneshot(human_req("POST", "/routines", new_routine_body()))
                .await
                .unwrap(),
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();
        assert_eq!(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    &format!("/routines/{id}/transition"),
                    json!({"to": "retired"}),
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );

        let resp = app
            .oneshot(human_req(
                "PATCH",
                &format!("/routines/{id}"),
                json!({"title": "too late"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn transition_retired_to_active_is_409() {
        let (app, _sink) = test_app().await;
        let created = body_json(
            app.clone()
                .oneshot(human_req("POST", "/routines", new_routine_body()))
                .await
                .unwrap(),
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();
        assert_eq!(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    &format!("/routines/{id}/transition"),
                    json!({"to": "retired"}),
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );

        let resp = app
            .oneshot(human_req(
                "POST",
                &format!("/routines/{id}/transition"),
                json!({"to": "active"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn transition_rejects_unknown_field() {
        let (app, _sink) = test_app().await;
        let created = body_json(
            app.clone()
                .oneshot(human_req("POST", "/routines", new_routine_body()))
                .await
                .unwrap(),
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();
        let resp = app
            .oneshot(human_req(
                "POST",
                &format!("/routines/{id}/transition"),
                json!({"to": "paused", "extra": 1}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // ---- GET list: status / area_id / direction_id filters, each with a
    // positive control (a matching + a non-matching row) -----------------------

    #[tokio::test]
    async fn list_filters_by_status_area_and_direction_have_positive_controls() {
        let (app, _sink) = test_app().await;
        let area1 = body_json(
            app.clone()
                .oneshot(human_req("POST", "/areas", json!({"title": "Health"})))
                .await
                .unwrap(),
        )
        .await;
        let area2 = body_json(
            app.clone()
                .oneshot(human_req("POST", "/areas", json!({"title": "Work"})))
                .await
                .unwrap(),
        )
        .await;
        let direction1 = body_json(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    "/directions",
                    json!({"title": "d1", "target_window": "2026-Q4", "area_id": area1["id"]}),
                ))
                .await
                .unwrap(),
        )
        .await;

        let mut r1 = new_routine_body();
        r1["area_id"] = area1["id"].clone();
        r1["direction_id"] = direction1["id"].clone();
        let r1 = body_json(
            app.clone()
                .oneshot(human_req("POST", "/routines", r1))
                .await
                .unwrap(),
        )
        .await;
        let r1_id = r1["id"].as_str().unwrap().to_string();

        let mut r2 = new_routine_body();
        r2["title"] = json!("Deep work block");
        r2["kind"] = json!("deep_work");
        r2["area_id"] = area2["id"].clone();
        let r2 = body_json(
            app.clone()
                .oneshot(human_req("POST", "/routines", r2))
                .await
                .unwrap(),
        )
        .await;
        let r2_id = r2["id"].as_str().unwrap().to_string();

        // area_id filter: matches only r1 (positive control: r2 exists in a
        // different area and must not show up).
        let by_area = body_json(
            app.clone()
                .oneshot(get_req(&format!(
                    "/routines?area_id={}",
                    area1["id"].as_str().unwrap()
                )))
                .await
                .unwrap(),
        )
        .await;
        let ids: Vec<&str> = by_area["routines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec![r1_id.as_str()]);

        // direction_id filter: matches only r1.
        let by_direction = body_json(
            app.clone()
                .oneshot(get_req(&format!(
                    "/routines?direction_id={}",
                    direction1["id"].as_str().unwrap()
                )))
                .await
                .unwrap(),
        )
        .await;
        let ids: Vec<&str> = by_direction["routines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec![r1_id.as_str()]);

        // status filter: pause r2, then filter by each status — each finds
        // exactly the matching routine (positive control both ways).
        assert_eq!(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    &format!("/routines/{r2_id}/transition"),
                    json!({"to": "paused"}),
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let active = body_json(
            app.clone()
                .oneshot(get_req("/routines?status=active"))
                .await
                .unwrap(),
        )
        .await;
        let ids: Vec<&str> = active["routines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec![r1_id.as_str()]);

        let paused = body_json(
            app.oneshot(get_req("/routines?status=paused"))
                .await
                .unwrap(),
        )
        .await;
        let ids: Vec<&str> = paused["routines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec![r2_id.as_str()]);
    }

    // ---- GET /routines/{id}: unknown id -> 404 --------------------------------

    #[tokio::test]
    async fn get_unknown_routine_is_404() {
        let (app, _sink) = test_app().await;
        let resp = app
            .oneshot(get_req("/routines/does-not-exist"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ---- events mirrored to the kernel sink -----------------------------------
    //
    // T3.1.2 review: the mirror must be one-for-one with the store's OWN
    // `sin90_events` writes — same event count, same kind names, same
    // payload shape (`store::repo::update_routine`'s `RoutineUpdate::changed`
    // is exactly what gates this; `transition_routine`'s destination-specific
    // kind is what names it).

    #[tokio::test]
    async fn create_emits_a_mirrored_routine_created_event() {
        let (app, sink) = test_app().await;
        app.oneshot(human_req("POST", "/routines", new_routine_body()))
            .await
            .unwrap();
        assert!(sink
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|(kind, _)| kind == "routine.created"));
    }

    /// A no-op PATCH (absent fields, or fields re-stating the current
    /// values) must emit ZERO mirrored events — the store itself wrote no
    /// `sin90_events` row for it either (`update_routine`'s L1), so a mirror
    /// firing anyway would be a lie about what happened. Positive control:
    /// an actual field change still emits exactly one `routine.updated`,
    /// whose payload is the full post-update snapshot plus a `changed` array
    /// naming exactly the field that moved — not an ad hoc field list.
    #[tokio::test]
    async fn noop_patch_emits_no_event_real_change_emits_exactly_one_with_changed() {
        let (app, sink) = test_app().await;
        let created = body_json(
            app.clone()
                .oneshot(human_req("POST", "/routines", new_routine_body()))
                .await
                .unwrap(),
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();
        let events_before = sink.0.lock().unwrap().len();

        // Absent fields.
        assert_eq!(
            app.clone()
                .oneshot(human_req("PATCH", &format!("/routines/{id}"), json!({})))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            sink.0.lock().unwrap().len(),
            events_before,
            "an absent-fields (no-op) patch must not emit a mirrored event"
        );

        // Present but re-stating the current value.
        assert_eq!(
            app.clone()
                .oneshot(human_req(
                    "PATCH",
                    &format!("/routines/{id}"),
                    json!({"title": "Morning run"}),
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            sink.0.lock().unwrap().len(),
            events_before,
            "a same-values (no-op) patch must not emit a mirrored event"
        );

        // Positive control: an actual change.
        assert_eq!(
            app.oneshot(human_req(
                "PATCH",
                &format!("/routines/{id}"),
                json!({"title": "Evening run"}),
            ))
            .await
            .unwrap()
            .status(),
            StatusCode::OK
        );
        let events = sink.0.lock().unwrap();
        let new_events = &events[events_before..];
        assert_eq!(
            new_events.len(),
            1,
            "a real change must emit exactly one mirrored event: {new_events:?}"
        );
        assert_eq!(new_events[0].0, "routine.updated");
        assert_eq!(new_events[0].1["id"], json!(id));
        assert_eq!(new_events[0].1["title"], json!("Evening run"));
        assert_eq!(new_events[0].1["changed"], json!(["title"]));
    }

    /// The mirrored transition event's kind is the SAME destination-specific
    /// name `store::repo::transition_routine` uses internally — `paused` /
    /// `resumed` / `retired` — not a generic `routine.transitioned`. Each
    /// edge is asserted individually (exact new-event count of 1, exact
    /// kind), not just "some routine.* event fired somewhere".
    #[tokio::test]
    async fn transition_mirrored_event_kind_matches_the_destination_status() {
        let (app, sink) = test_app().await;
        let created = body_json(
            app.clone()
                .oneshot(human_req("POST", "/routines", new_routine_body()))
                .await
                .unwrap(),
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();

        // active -> paused: `routine.paused`.
        let events_before = sink.0.lock().unwrap().len();
        assert_eq!(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    &format!("/routines/{id}/transition"),
                    json!({"to": "paused"}),
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        {
            let events = sink.0.lock().unwrap();
            let new_events = &events[events_before..];
            assert_eq!(new_events.len(), 1, "{new_events:?}");
            assert_eq!(new_events[0].0, "routine.paused");
            assert_eq!(new_events[0].1["routine_id"], json!(id));
        }

        // paused -> active: `routine.resumed`.
        let events_before = sink.0.lock().unwrap().len();
        assert_eq!(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    &format!("/routines/{id}/transition"),
                    json!({"to": "active"}),
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        {
            let events = sink.0.lock().unwrap();
            let new_events = &events[events_before..];
            assert_eq!(new_events.len(), 1, "{new_events:?}");
            assert_eq!(new_events[0].0, "routine.resumed");
            assert_eq!(new_events[0].1["routine_id"], json!(id));
        }

        // active -> retired: `routine.retired`.
        let events_before = sink.0.lock().unwrap().len();
        assert_eq!(
            app.oneshot(human_req(
                "POST",
                &format!("/routines/{id}/transition"),
                json!({"to": "retired"}),
            ))
            .await
            .unwrap()
            .status(),
            StatusCode::OK
        );
        let events = sink.0.lock().unwrap();
        let new_events = &events[events_before..];
        assert_eq!(new_events.len(), 1, "{new_events:?}");
        assert_eq!(new_events[0].0, "routine.retired");
        assert_eq!(new_events[0].1["routine_id"], json!(id));
    }
}

// ---- T4.1.1: Review three-kind routes ---------------------------------------

mod review {
    use super::*;

    fn new_review_body(kind: &str, period: &str) -> Value {
        json!({"kind": kind, "period": period})
    }

    // ---- actor-key gate: automation -> 403, human -> 2xx (positive control) --

    #[tokio::test]
    async fn create_review_requires_human_key() {
        let (app, _sink) = test_app().await;
        let resp = app
            .clone()
            .oneshot(automation_req(
                "POST",
                "/reviews",
                new_review_body("daily", "2026-09-24"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Positive control: the human key succeeds with the same body.
        let resp = app
            .oneshot(human_req(
                "POST",
                "/reviews",
                new_review_body("daily", "2026-09-24"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn update_review_requires_human_key() {
        let (app, _sink) = test_app().await;
        let created = body_json(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    "/reviews",
                    new_review_body("daily", "2026-09-24"),
                ))
                .await
                .unwrap(),
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();

        let resp = app
            .clone()
            .oneshot(automation_req(
                "PATCH",
                &format!("/reviews/{id}"),
                json!({"body": "hacked"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Positive control: the human key succeeds with the same body.
        let resp = app
            .oneshot(human_req(
                "PATCH",
                &format!("/reviews/{id}"),
                json!({"body": "notes"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn finalize_review_requires_human_key() {
        let (app, _sink) = test_app().await;
        let created = body_json(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    "/reviews",
                    new_review_body("daily", "2026-09-24"),
                ))
                .await
                .unwrap(),
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();

        let resp = app
            .clone()
            .oneshot(automation_req(
                "POST",
                &format!("/reviews/{id}/finalize"),
                json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Positive control: the human key succeeds.
        let resp = app
            .oneshot(human_req(
                "POST",
                &format!("/reviews/{id}/finalize"),
                json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ---- 400: unknown field, illegal period; positive control: legal period --

    #[tokio::test]
    async fn create_review_rejects_unknown_field() {
        let (app, _sink) = test_app().await;
        let mut body = new_review_body("daily", "2026-09-24");
        body["nope"] = json!(1);
        let resp = app
            .oneshot(human_req("POST", "/reviews", body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_review_rejects_illegal_daily_and_weekly_period() {
        let (app, _sink) = test_app().await;

        let resp = app
            .clone()
            .oneshot(human_req(
                "POST",
                "/reviews",
                new_review_body("daily", "2026-13-40"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let resp = app
            .clone()
            .oneshot(human_req(
                "POST",
                "/reviews",
                new_review_body("weekly", "2026-W99"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Positive control: a legal daily period is accepted.
        let resp = app
            .oneshot(human_req(
                "POST",
                "/reviews",
                new_review_body("daily", "2026-09-24"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn create_review_rhythm_period_requires_existing_rhythm() {
        let (app, _sink, store) = test_app_with_store().await;

        let resp = app
            .clone()
            .oneshot(human_req(
                "POST",
                "/reviews",
                new_review_body("rhythm", "no-such-rhythm"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // Positive control: an EXISTING rhythm id is accepted. T3.4.1 has no
        // production `POST /rhythms` yet, so the rhythm is seeded directly.
        crate::store::test_hooks::insert_rhythm(&store, "rhythm-1")
            .await
            .unwrap();
        let resp = app
            .oneshot(human_req(
                "POST",
                "/reviews",
                new_review_body("rhythm", "rhythm-1"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    // ---- duplicate (kind, period) -> 409; positive control: distinct period --

    #[tokio::test]
    async fn create_review_duplicate_kind_period_is_409_distinct_period_is_201() {
        let (app, _sink) = test_app().await;
        assert_eq!(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    "/reviews",
                    new_review_body("daily", "2026-09-24"),
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::CREATED
        );

        let resp = app
            .clone()
            .oneshot(human_req(
                "POST",
                "/reviews",
                new_review_body("daily", "2026-09-24"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);

        // Positive control: a different period for the same kind succeeds.
        let resp = app
            .oneshot(human_req(
                "POST",
                "/reviews",
                new_review_body("daily", "2026-09-25"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    // ---- finalized is a closed door: PATCH -> 409, finalize twice -> 409 -----

    #[tokio::test]
    async fn patch_on_finalized_review_is_409() {
        let (app, _sink) = test_app().await;
        let created = body_json(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    "/reviews",
                    new_review_body("daily", "2026-09-24"),
                ))
                .await
                .unwrap(),
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();
        assert_eq!(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    &format!("/reviews/{id}/finalize"),
                    json!({}),
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );

        let resp = app
            .oneshot(human_req(
                "PATCH",
                &format!("/reviews/{id}"),
                json!({"body": "too late"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn finalize_review_twice_is_409() {
        let (app, _sink) = test_app().await;
        let created = body_json(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    "/reviews",
                    new_review_body("daily", "2026-09-24"),
                ))
                .await
                .unwrap(),
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();
        assert_eq!(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    &format!("/reviews/{id}/finalize"),
                    json!({}),
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );

        let resp = app
            .oneshot(human_req(
                "POST",
                &format!("/reviews/{id}/finalize"),
                json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    // ---- no-op PATCH mirrors zero events; a real change mirrors one --------

    #[tokio::test]
    async fn noop_patch_mirrors_zero_events_real_change_mirrors_one() {
        let (app, sink) = test_app().await;
        let created = body_json(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    "/reviews",
                    new_review_body("daily", "2026-09-24"),
                ))
                .await
                .unwrap(),
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();
        let events_before = sink.0.lock().unwrap().len();

        // No-op: re-stating the current (empty) body.
        let resp = app
            .clone()
            .oneshot(human_req(
                "PATCH",
                &format!("/reviews/{id}"),
                json!({"body": ""}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            sink.0.lock().unwrap().len(),
            events_before,
            "a no-op PATCH must not mirror an event"
        );

        // Positive control: a real change mirrors exactly one event.
        let resp = app
            .oneshot(human_req(
                "PATCH",
                &format!("/reviews/{id}"),
                json!({"body": "notes"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let events = sink.0.lock().unwrap();
        let new_events = &events[events_before..];
        assert_eq!(new_events.len(), 1, "{new_events:?}");
        assert_eq!(new_events[0].0, "review.updated");
    }

    // ---- list filters ----------------------------------------------------

    #[tokio::test]
    async fn list_reviews_filters_by_kind_and_period() {
        let (app, _sink) = test_app().await;
        for (kind, period) in [
            ("daily", "2026-09-24"),
            ("daily", "2026-09-25"),
            ("weekly", "2026-W39"),
        ] {
            assert_eq!(
                app.clone()
                    .oneshot(human_req("POST", "/reviews", new_review_body(kind, period)))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::CREATED
            );
        }

        let all = body_json(app.clone().oneshot(get_req("/reviews")).await.unwrap()).await;
        assert_eq!(all["reviews"].as_array().unwrap().len(), 3);

        let daily = body_json(
            app.clone()
                .oneshot(get_req("/reviews?kind=daily"))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(daily["reviews"].as_array().unwrap().len(), 2);

        let one = body_json(
            app.oneshot(get_req("/reviews?kind=daily&period=2026-09-24"))
                .await
                .unwrap(),
        )
        .await;
        let rows = one["reviews"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["period"], "2026-09-24");
    }
}

// ---- T4.3.1: GET /review/weekly/draft ---------------------------------------

mod weekly_draft {
    use super::*;
    use crate::store::test_hooks;

    #[tokio::test]
    async fn weekly_draft_route_rejects_invalid_week_with_400() {
        let (app, _sink) = test_app().await;
        let resp = app
            .oneshot(get_req("/review/weekly/draft?week=not-a-week"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// A read, no actor gate — same posture as `/attention`/`/events` (T4.1.1
    /// review routes are the only ones this file requires human keys on).
    /// Positive control lives in `weekly_draft_route_wires_to_store_replay`
    /// below, which also hits this route with no auth header and expects
    /// 200.
    #[tokio::test]
    async fn weekly_draft_route_needs_no_actor_key() {
        let (app, _sink) = test_app().await;
        let resp = app
            .oneshot(get_req("/review/weekly/draft?week=2026-W39"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// End-to-end: real HTTP requests build an Area -> Direction -> completed
    /// ScheduleBlock, the completion event is backdated into a controlled
    /// week (`test_hooks::set_last_event_at`, same technique
    /// `store::weekly_draft`'s own tests use), and the route's JSON reflects
    /// exactly that — pinning the route is correctly wired to
    /// `Sin90Store::weekly_draft`, not re-testing its arithmetic (that's
    /// `store::weekly_draft`'s job).
    #[tokio::test]
    async fn weekly_draft_route_wires_to_store_replay() {
        let (app, _sink, store) = test_app_with_store().await;

        let area = body_json(
            app.clone()
                .oneshot(human_req("POST", "/areas", json!({"title": "Work"})))
                .await
                .unwrap(),
        )
        .await;
        let area_id = area["id"].as_str().unwrap().to_string();

        let direction = body_json(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    "/directions",
                    json!({
                        "title": "Coding", "target_window": "this-quarter",
                        "area_id": area_id,
                    }),
                ))
                .await
                .unwrap(),
        )
        .await;
        let direction_id = direction["id"].as_str().unwrap().to_string();

        let block = body_json(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    "/schedule-blocks",
                    json!({"direction_id": direction_id, "planned_minutes": 90}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let block_id = block["id"].as_str().unwrap().to_string();

        for to in ["started", "completed"] {
            let resp = app
                .clone()
                .oneshot(human_req(
                    "PATCH",
                    &format!("/schedule-blocks/{block_id}"),
                    json!({"to": to}),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
        test_hooks::set_last_event_at(&store, "block", &block_id, "2026-09-24T10:00:00Z")
            .await
            .unwrap();

        let draft = body_json(
            app.oneshot(get_req("/review/weekly/draft?week=2026-W39"))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(draft["week"], "2026-W39");
        assert_eq!(
            draft["by_area"],
            json!([{"area_id": area_id, "minutes": 90}])
        );
        assert_eq!(
            draft["by_direction"],
            json!([{"direction_id": direction_id, "minutes": 90}])
        );
        assert_eq!(draft["tasks_done"], 0);
        assert_eq!(draft["routines"], json!([]));
    }
}

// ---- T3.2.2: POST /_a24/scheduler/fired -------------------------------------

mod fired {
    use super::*;

    fn fired_body(key: &str, trigger: &str) -> Value {
        json!({
            "key": key,
            "scheduled_for": "2026-09-24T07:00:00Z",
            "fired_at": "2026-09-24T07:00:05Z",
            "trigger": trigger,
        })
    }

    async fn create_routine_id(app: &axum::Router) -> String {
        let created = body_json(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    "/routines",
                    json!({
                        "title": "Morning run", "kind": "exercise",
                        "cron": "0 7 * * MON,WED,FRI",
                    }),
                ))
                .await
                .unwrap(),
        )
        .await;
        created["id"].as_str().unwrap().to_string()
    }

    // ---- required header ---------------------------------------------------

    #[tokio::test]
    async fn fired_missing_fire_id_header_is_400() {
        let (app, _sink, _store) = test_app_mounted().await;
        let id = create_routine_id(&app).await;
        let resp = app
            .oneshot(fired_req(
                "/_a24/scheduler/fired",
                None,
                fired_body(&format!("routine.{id}"), "tick"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // ---- unknown body field --------------------------------------------------

    #[tokio::test]
    async fn fired_unknown_body_field_is_400() {
        let (app, _sink, _store) = test_app_mounted().await;
        let id = create_routine_id(&app).await;
        let mut body = fired_body(&format!("routine.{id}"), "tick");
        body["nope"] = json!(1);
        let resp = app
            .oneshot(fired_req(
                "/_a24/scheduler/fired",
                Some("fire-bad-field"),
                body,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // ---- standalone -> 404; positive control: mounted -> 2xx -----------------

    #[tokio::test]
    async fn fired_route_is_404_standalone_positive_control_mounted_2xx() {
        let (standalone_app, _sink, _store) = test_app_with_store().await; // mounted = false
        let resp = standalone_app
            .oneshot(fired_req(
                "/_a24/scheduler/fired",
                Some("fire-standalone"),
                fired_body("routine.does-not-matter", "tick"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // Positive control: the SAME kind of request against a mounted
        // router (route actually registered, and the key resolves) succeeds.
        let (mounted_app, _msink, _mstore) = test_app_mounted().await;
        let mounted_id = create_routine_id(&mounted_app).await;
        let resp = mounted_app
            .oneshot(fired_req(
                "/_a24/scheduler/fired",
                Some("fire-mounted"),
                fired_body(&format!("routine.{mounted_id}"), "tick"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ---- duplicate fire_id: idempotent, one event; distinct fire_id: two -----

    #[tokio::test]
    async fn fired_duplicate_fire_id_one_event_positive_control_distinct_fire_id_two_events() {
        let (app, sink, _store) = test_app_mounted().await;
        let id = create_routine_id(&app).await;
        let key = format!("routine.{id}");

        let resp1 = app
            .clone()
            .oneshot(fired_req(
                "/_a24/scheduler/fired",
                Some("fire-x"),
                fired_body(&key, "tick"),
            ))
            .await
            .unwrap();
        assert_eq!(resp1.status(), StatusCode::OK);
        let resp2 = app
            .clone()
            .oneshot(fired_req(
                "/_a24/scheduler/fired",
                Some("fire-x"),
                fired_body(&key, "tick"),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp2.status(),
            StatusCode::OK,
            "kernel retry must still be 2xx"
        );

        let fired_events_after_dup: usize = sink
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| k == "routine.fired")
            .count();
        assert_eq!(
            fired_events_after_dup, 1,
            "same fire_id sent twice must mirror exactly one routine.fired event"
        );

        // Positive control: a DIFFERENT fire_id for the same routine is a
        // real second due slot, not a retry — it DOES add a second event.
        let resp3 = app
            .oneshot(fired_req(
                "/_a24/scheduler/fired",
                Some("fire-y"),
                fired_body(&key, "tick"),
            ))
            .await
            .unwrap();
        assert_eq!(resp3.status(), StatusCode::OK);
        let fired_events_after_distinct: usize = sink
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| k == "routine.fired")
            .count();
        assert_eq!(
            fired_events_after_distinct, 2,
            "a distinct fire_id must add a new event"
        );
    }

    // ---- unknown key: 200, no row, no event -----------------------------------

    #[tokio::test]
    async fn fired_unknown_key_is_200_and_writes_no_event() {
        let (app, sink, store) = test_app_mounted().await;
        let events_before = sink.0.lock().unwrap().len();
        let resp = app
            .oneshot(fired_req(
                "/_a24/scheduler/fired",
                Some("fire-unknown-key"),
                fired_body("routine.does-not-exist", "tick"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            sink.0.lock().unwrap().len(),
            events_before,
            "unknown key must not mirror any event"
        );
        assert_eq!(
            crate::store::test_hooks::routine_fire_count(&store, "does-not-exist")
                .await
                .unwrap(),
            0
        );
    }

    // ---- retired routine: 200, no event ---------------------------------------

    #[tokio::test]
    async fn fired_retired_routine_is_200_and_writes_no_event() {
        let (app, sink, _store) = test_app_mounted().await;
        let id = create_routine_id(&app).await;
        let resp = app
            .clone()
            .oneshot(human_req(
                "POST",
                &format!("/routines/{id}/transition"),
                json!({"to": "retired"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let events_before = sink.0.lock().unwrap().len();
        let resp = app
            .oneshot(fired_req(
                "/_a24/scheduler/fired",
                Some("fire-after-retire"),
                fired_body(&format!("routine.{id}"), "tick"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            sink.0.lock().unwrap().len(),
            events_before,
            "a retired routine's fire must not mirror any event"
        );
    }

    // ---- /today surfaces today's fired routines --------------------------------

    #[tokio::test]
    async fn fired_today_routine_appears_in_today_view() {
        let (app, _sink, _store) = test_app_mounted().await;
        let id = create_routine_id(&app).await;
        let resp = app
            .clone()
            .oneshot(fired_req(
                "/_a24/scheduler/fired",
                Some("fire-today-http"),
                fired_body(&format!("routine.{id}"), "tick"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let today = body_json(app.oneshot(get_req("/today")).await.unwrap()).await;
        let ids: Vec<&str> = today["fired_routines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&id.as_str()), "{ids:?}");
    }

    // ---- T4.3.2: review Routine fired -> mirrors review.created too -----------

    async fn create_review_routine_id(app: &axum::Router) -> String {
        let created = body_json(
            app.clone()
                .oneshot(human_req(
                    "POST",
                    "/routines",
                    json!({
                        "title": "Weekly review", "kind": "review",
                        "cron": "0 18 * * SUN",
                    }),
                ))
                .await
                .unwrap(),
        )
        .await;
        created["id"].as_str().unwrap().to_string()
    }

    /// The HTTP-layer half of T4.3.2: a `review`-kind Routine's fire must
    /// mirror BOTH `routine.fired` AND `review.created` to `EventSink` — the
    /// store-level `review_routine_*` tests (`store::repo::review_routine_tests`)
    /// already pin the store's own internal event/row; this pins the second
    /// mirror this handler is responsible for adding, with `source: "routine"`.
    #[tokio::test]
    async fn fired_review_routine_mirrors_review_created_with_source_routine() {
        let (app, sink, _store) = test_app_mounted().await;
        let id = create_review_routine_id(&app).await;

        let resp = app
            .oneshot(fired_req(
                "/_a24/scheduler/fired",
                Some("fire-review-1"),
                fired_body(&format!("routine.{id}"), "tick"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert!(
            body["review_created"].is_string(),
            "response must surface the auto-created review's id: {body}"
        );

        let events = sink.0.lock().unwrap();
        assert!(
            events.iter().any(|(k, _)| k == "routine.fired"),
            "must still mirror routine.fired: {events:?}"
        );
        let (_, payload) = events
            .iter()
            .find(|(k, _)| k == "review.created")
            .unwrap_or_else(|| panic!("must mirror review.created: {events:?}"));
        assert_eq!(payload["source"], "routine");
        assert_eq!(payload["kind"], "weekly");
        assert_eq!(payload["status"], "draft");
    }

    /// Positive control: a duplicate `fire_id` retry must mirror
    /// `review.created` exactly ONCE, not once per delivery attempt.
    #[tokio::test]
    async fn fired_review_routine_duplicate_fire_id_mirrors_review_created_once() {
        let (app, sink, _store) = test_app_mounted().await;
        let id = create_review_routine_id(&app).await;
        let key = format!("routine.{id}");

        for _ in 0..2 {
            let resp = app
                .clone()
                .oneshot(fired_req(
                    "/_a24/scheduler/fired",
                    Some("fire-review-dup"),
                    fired_body(&key, "tick"),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }

        let created_count = sink
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| k == "review.created")
            .count();
        assert_eq!(
            created_count, 1,
            "duplicate fire_id must not re-mirror review.created"
        );
    }
}
