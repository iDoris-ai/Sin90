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
    let store = Sin90Store::open_memory().await.unwrap();
    let sink = RecordingSink::default();
    let state = Sin90State::new(
        store,
        Arc::new(sink.clone()),
        ActorKeys {
            human: HUMAN.into(),
            automation: AUTOMATION.into(),
        },
    );
    (router(state), sink)
}

fn human_req(method: &str, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {HUMAN}"))
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
        .header(header::AUTHORIZATION, format!("Bearer {AUTOMATION}"))
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
        .header(header::AUTHORIZATION, format!("Bearer {AUTOMATION}"))
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
        .header(header::AUTHORIZATION, format!("Bearer {AUTOMATION}"))
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
        .header(header::AUTHORIZATION, format!("Bearer {AUTOMATION}"))
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
