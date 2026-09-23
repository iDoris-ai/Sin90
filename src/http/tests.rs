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
async fn test_app_with_store() -> (axum::Router, RecordingSink, Sin90Store) {
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
    (router(state), sink, store)
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
    // Backdate it to yesterday (UTC) — a real day boundary, not a sleep.
    crate::store::test_hooks::set_task_created_at(&store, old_id, "2020-01-01T00:00:00Z")
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
