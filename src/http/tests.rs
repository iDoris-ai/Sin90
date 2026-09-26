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

// ---- T5.7.1: POST /proposals/{id}/reject + rejection log -------------------

/// `POST /proposals/{id}/reject`, optional JSON body `{"reason"?: string}`
/// (`deny_unknown_fields`) — `None`/absent body maps to `reason: None`
/// (`reject_req(.., None)` sends `Body::empty()`, exactly like `accept_req`).
fn reject_req(id: &str, key: &str, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(format!("/proposals/{id}/reject"))
        .header("x-sin90-actor-key", key);
    let body = match body {
        Some(v) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    builder.body(body).unwrap()
}

/// Submits a structurally-valid, directly-authored (not AI-produced)
/// `create_area` proposal with the given id/rationale via the automation
/// key — the shape every reject test below starts from, since `reject`
/// itself doesn't care what the ops are, only that the proposal exists and
/// is `pending`.
async fn submit_plain_proposal(app: &axum::Router, id: &str, rationale: Option<&str>) {
    let resp = app
        .clone()
        .oneshot(automation_proposal(json!({
            "id": id, "status": "pending", "source": "local_brain",
            "ops": [{"op": "create_area", "title": "x"}], "rationale": rationale
        })))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "setup: submit must succeed"
    );
}

/// Positive control + the core judgement: a human rejecting a `pending`
/// proposal moves it to `rejected`, drops it out of the pending list (so it
/// can never be `accept`ed or re-block dedup), writes exactly one complete
/// `sin90_proposal_rejections` row (including `proposal_source` copied
/// verbatim from the submitted proposal, Opus review M1, and `proposed_at`
/// equal to the proposal's own `created_at`, Opus review L3), mirrors
/// exactly one `proposal.rejected` event to `EventSink` carrying
/// `ops_summary` too (Opus review L1), and appends exactly one INTERNAL
/// `sin90_events` row (`entity = proposal`, `kind = rejected`, Opus review
/// L3) — tasks.md T5.7.1's acceptance line, all in one test since they are
/// one atomic outcome of one call.
#[tokio::test]
async fn proposal_reject_by_human_marks_rejected_hides_from_pending_and_logs_and_emits_event() {
    let (app, sink, store) = test_app_with_store().await;
    submit_plain_proposal(&app, "p-reject-1", Some("AI thought this was worth doing")).await;
    // Backdate created_at to a value distinct from "now" — `now_iso8601()`
    // is second-resolution, so without this a submit-then-reject in the
    // same test could coincidentally produce identical timestamps and mask
    // a `proposed_at` bug (see `set_proposal_created_at`'s doc).
    crate::store::test_hooks::set_proposal_created_at(&store, "p-reject-1", "2020-01-01T00:00:00Z")
        .await
        .unwrap();
    let submitted = store.get_proposal("p-reject-1").await.unwrap();
    assert_eq!(submitted.created_at, "2020-01-01T00:00:00Z");

    let resp = app
        .clone()
        .oneshot(reject_req(
            "p-reject-1",
            HUMAN,
            Some(json!({"reason": "duplicate of an existing Area"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["id"], "p-reject-1");
    assert_eq!(body["status"], "rejected");

    // Status is `rejected`...
    let status = crate::store::test_hooks::proposal_status(&store, "p-reject-1")
        .await
        .unwrap();
    assert_eq!(status.as_deref(), Some("rejected"));

    // ...and gone from the pending list (accept-eligibility AND dedup both
    // key off this exact query, `repo.rs::list_pending_proposals`).
    let pending = store.list_pending_proposals().await.unwrap();
    assert!(
        pending.iter().all(|p| p.id != "p-reject-1"),
        "a rejected proposal must not appear in the pending list: {pending:?}"
    );

    // Exactly one complete rejection-log row.
    let rows = crate::store::test_hooks::proposal_rejection_rows(&store, "p-reject-1")
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];
    assert_eq!(row.proposal_id, "p-reject-1");
    assert_eq!(
        row.capability_source, "direct",
        "no sin90_ai_calls row references this proposal — a human/automation \
         client submitted it directly, not via /ai/classify or /ai/propose"
    );
    assert_eq!(
        row.proposal_source, "local_brain",
        "copied verbatim from the submitted proposal's own `source` field"
    );
    assert_eq!(row.ops_summary, "create_area x1");
    assert_eq!(
        row.rationale.as_deref(),
        Some("AI thought this was worth doing")
    );
    assert_eq!(row.reason.as_deref(), Some("duplicate of an existing Area"));
    assert_eq!(
        row.proposed_at, submitted.created_at,
        "proposed_at must mirror the proposal's own created_at"
    );
    assert!(!row.rejected_at.is_empty());

    // Exactly one `proposal.rejected` event mirrored to EventSink.
    let rejected_events: Vec<_> = sink
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|(kind, _)| kind == "proposal.rejected")
        .cloned()
        .collect();
    assert_eq!(rejected_events.len(), 1, "{rejected_events:?}");
    assert_eq!(rejected_events[0].1["proposal_id"], "p-reject-1");
    assert_eq!(rejected_events[0].1["capability_source"], "direct");
    assert_eq!(rejected_events[0].1["ops_summary"], "create_area x1");

    // Exactly one INTERNAL sin90_events row for this proposal being rejected.
    let events = store
        .list_events(Some("proposal"), Some("p-reject-1"), None, None)
        .await
        .unwrap();
    let rejected_internal: Vec<_> = events.iter().filter(|e| e.kind == "rejected").collect();
    assert_eq!(rejected_internal.len(), 1, "{events:?}");
    assert_eq!(rejected_internal[0].payload["proposal_id"], "p-reject-1");
}

/// The automation key must not be able to reject a Proposal — reject
/// commits a terminal state change, same reasoning `accept_proposal`'s own
/// human-only gate documents (Codex 2026-09-22 review, High). Asserts the
/// negative (403) AND that the store is left completely untouched: still
/// `pending`, still in the pending list, no rejection-log row, no event.
#[tokio::test]
async fn proposal_reject_by_automation_key_is_403_and_nothing_changes() {
    let (app, sink, store) = test_app_with_store().await;
    submit_plain_proposal(&app, "p-reject-auto", None).await;

    let resp = app
        .clone()
        .oneshot(reject_req("p-reject-auto", AUTOMATION, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let status = crate::store::test_hooks::proposal_status(&store, "p-reject-auto")
        .await
        .unwrap();
    assert_eq!(status.as_deref(), Some("pending"), "must be untouched");
    let pending = store.list_pending_proposals().await.unwrap();
    assert!(pending.iter().any(|p| p.id == "p-reject-auto"));
    let rows = crate::store::test_hooks::proposal_rejection_rows(&store, "p-reject-auto")
        .await
        .unwrap();
    assert!(rows.is_empty(), "no log row must be written: {rows:?}");
    assert!(
        sink.0
            .lock()
            .unwrap()
            .iter()
            .all(|(k, _)| k != "proposal.rejected"),
        "no event must be emitted"
    );

    // Positive control: the human key CAN reject the same, still-pending proposal.
    let resp2 = app
        .clone()
        .oneshot(reject_req("p-reject-auto", HUMAN, None))
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
}

/// An already-`applied` proposal cannot be rejected — 409, and the
/// rejection log gains no row for it (the CAS never claims a non-`pending`
/// row, `Sin90Store::reject_proposal`'s doc).
#[tokio::test]
async fn proposal_reject_of_an_applied_proposal_is_409() {
    let (app, _sink, store) = test_app_with_store().await;
    submit_plain_proposal(&app, "p-reject-applied", None).await;
    let accept_resp = app
        .clone()
        .oneshot(accept_req("p-reject-applied"))
        .await
        .unwrap();
    assert_eq!(accept_resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(reject_req("p-reject-applied", HUMAN, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    let rows = crate::store::test_hooks::proposal_rejection_rows(&store, "p-reject-applied")
        .await
        .unwrap();
    assert!(rows.is_empty(), "{rows:?}");
}

/// An already-`rejected` proposal cannot be rejected again — 409, and the
/// log keeps exactly the ONE row the first (successful) reject wrote, not
/// two.
#[tokio::test]
async fn proposal_reject_of_an_already_rejected_proposal_is_409() {
    let (app, _sink, store) = test_app_with_store().await;
    submit_plain_proposal(&app, "p-reject-twice", None).await;
    let first = app
        .clone()
        .oneshot(reject_req("p-reject-twice", HUMAN, None))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);

    let second = app
        .clone()
        .oneshot(reject_req("p-reject-twice", HUMAN, None))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::CONFLICT);

    let rows = crate::store::test_hooks::proposal_rejection_rows(&store, "p-reject-twice")
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "a second reject must not add a second row: {rows:?}"
    );
}

/// The `reason` field is genuinely optional: omitting the body entirely
/// records `reason: NULL`, giving one explicitly records it — both are
/// legal, distinguished outcomes, not one masking a bug in the other.
#[tokio::test]
async fn proposal_reject_reason_is_optional_and_recorded_when_given() {
    let (app, _sink, store) = test_app_with_store().await;
    submit_plain_proposal(&app, "p-reason-none", None).await;
    submit_plain_proposal(&app, "p-reason-some", None).await;

    let r1 = app
        .clone()
        .oneshot(reject_req("p-reason-none", HUMAN, None))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let r2 = app
        .clone()
        .oneshot(reject_req(
            "p-reason-some",
            HUMAN,
            Some(json!({"reason": "wrong direction"})),
        ))
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);

    let none_rows = crate::store::test_hooks::proposal_rejection_rows(&store, "p-reason-none")
        .await
        .unwrap();
    assert_eq!(none_rows[0].reason, None);
    let some_rows = crate::store::test_hooks::proposal_rejection_rows(&store, "p-reason-some")
        .await
        .unwrap();
    assert_eq!(some_rows[0].reason.as_deref(), Some("wrong direction"));
}

/// `deny_unknown_fields`: a stray key in the reject body is a 400, and the
/// proposal is left completely untouched (same "reject nothing written"
/// posture the automation-key test above pins).
#[tokio::test]
async fn proposal_reject_with_unknown_field_is_400_and_nothing_changes() {
    let (app, _sink, store) = test_app_with_store().await;
    submit_plain_proposal(&app, "p-reject-badbody", None).await;

    let resp = app
        .clone()
        .oneshot(reject_req(
            "p-reject-badbody",
            HUMAN,
            Some(json!({"reason": "x", "typo_field": 1})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let status = crate::store::test_hooks::proposal_status(&store, "p-reject-badbody")
        .await
        .unwrap();
    assert_eq!(status.as_deref(), Some("pending"));
    let rows = crate::store::test_hooks::proposal_rejection_rows(&store, "p-reject-badbody")
        .await
        .unwrap();
    assert!(rows.is_empty());
}

/// Opus review L3: rejecting an id that does not exist at all is 404 (not
/// 409 — `Sin90Store::reject_proposal`'s `NotFound` branch, same distinction
/// `accept_proposal` already makes between "missing" and "wrong status").
#[tokio::test]
async fn proposal_reject_of_an_unknown_id_is_404() {
    let (app, _sink) = test_app().await;
    let resp = app
        .clone()
        .oneshot(reject_req("p-does-not-exist", HUMAN, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// Opus review L3: a `summarize`-sourced rejection is deliberately NOT
// tested here. `store::ai_port::allowed_ops(Capability::Summarize)` always
// returns `false` for every op (`ai_port.rs:425` — "which `Sin90Op`
// variants a capability's proposals may contain"), so `AiSink::submit(..,
// Capability::Summarize, ..)` always fails with `SinkError::Invalid` before
// it ever writes a `sin90_ai_calls` row with `proposal_id` set: there is no
// real production code path today that can create a `summarize`-attributed
// pending proposal (T5.3.1, which would give `summarize` its own allowed
// ops, has not landed). Faking one by hand-inserting a raw `sin90_ai_calls`
// row would only re-prove that `reject_proposal`'s capability_source SQL
// join is a plain string lookup — already established by the `classify` and
// `propose` variants of this same test
// (`proposal_reject_of_classify_proposal_unblocks_dedup_for_same_task`,
// `proposal_reject_of_propose_carry_unblocks_next_run_for_same_target`, both
// of which assert `capability_source` on a REAL `AiSink::submit`-produced
// row) — it would not exercise anything summarize-specific. Once T5.3.1
// gives `summarize` real ops, add the third variant here.

/// Opus review L2: `reason` is trimmed; a value that trims to empty is
/// treated exactly like "no reason given" (`None`), not persisted as an
/// empty string.
#[tokio::test]
async fn proposal_reject_reason_is_trimmed_and_whitespace_only_becomes_none() {
    let (app, _sink, store) = test_app_with_store().await;
    submit_plain_proposal(&app, "p-reason-trim", None).await;

    let resp = app
        .clone()
        .oneshot(reject_req(
            "p-reason-trim",
            HUMAN,
            Some(json!({"reason": "  duplicate  "})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let rows = crate::store::test_hooks::proposal_rejection_rows(&store, "p-reason-trim")
        .await
        .unwrap();
    assert_eq!(
        rows[0].reason.as_deref(),
        Some("duplicate"),
        "surrounding whitespace must be trimmed off"
    );

    submit_plain_proposal(&app, "p-reason-blank", None).await;
    let resp2 = app
        .clone()
        .oneshot(reject_req(
            "p-reason-blank",
            HUMAN,
            Some(json!({"reason": "   "})),
        ))
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
    let rows2 = crate::store::test_hooks::proposal_rejection_rows(&store, "p-reason-blank")
        .await
        .unwrap();
    assert_eq!(
        rows2[0].reason, None,
        "a whitespace-only reason must be recorded as no reason at all"
    );
}

/// Opus review L2: a `reason` over 1000 characters is a 400 and nothing is
/// written; exactly 1000 characters is the positive-control boundary that
/// must still succeed.
#[tokio::test]
async fn proposal_reject_reason_over_1000_chars_is_400_positive_control_at_1000_is_ok() {
    let (app, _sink, store) = test_app_with_store().await;
    submit_plain_proposal(&app, "p-reason-too-long", None).await;
    let too_long = "x".repeat(1001);
    let resp = app
        .clone()
        .oneshot(reject_req(
            "p-reason-too-long",
            HUMAN,
            Some(json!({"reason": too_long})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let status = crate::store::test_hooks::proposal_status(&store, "p-reason-too-long")
        .await
        .unwrap();
    assert_eq!(status.as_deref(), Some("pending"), "must be untouched");

    // Positive control: exactly at the limit still succeeds.
    submit_plain_proposal(&app, "p-reason-at-limit", None).await;
    let exactly_1000 = "x".repeat(1000);
    let resp2 = app
        .clone()
        .oneshot(reject_req(
            "p-reason-at-limit",
            HUMAN,
            Some(json!({"reason": exactly_1000.clone()})),
        ))
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
    let rows = crate::store::test_hooks::proposal_rejection_rows(&store, "p-reason-at-limit")
        .await
        .unwrap();
    assert_eq!(rows[0].reason.as_deref(), Some(exactly_1000.as_str()));
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

// ---- T5.1.1: GET|PUT /settings/ai (J10b) ------------------------------------

mod ai_settings {
    use super::*;

    /// Count of `sin90_events` rows — the real, persisted table, not just
    /// what the in-process `EventSink` happened to observe (2026-09-24
    /// review, M3: a mock-sink-only assertion can't tell "the mirror fired"
    /// apart from "the store itself wrote the event too", which is the
    /// actual claim `PUT /settings/ai`'s doc makes).
    async fn event_count(store: &Sin90Store, kind: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM sin90_events WHERE kind = ?")
            .bind(kind)
            .fetch_one(store.pool())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn settings_ai_get_defaults_to_false_when_no_row_exists() {
        let (app, _sink) = test_app().await;
        let resp = app.oneshot(get_req("/settings/ai")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["executive_enabled"], Value::Bool(false));
    }

    #[tokio::test]
    async fn settings_ai_put_requires_human_key_and_mirrors_setting_changed() {
        let (app, sink, store) = test_app_with_store().await;

        // Automation key -> 403: no event in the sink, AND none in the
        // actual database (M3 — the direct-write gate must have refused
        // before touching `sin90_events` at all, not just before notifying
        // the sink).
        let resp = app
            .clone()
            .oneshot(automation_req(
                "PUT",
                "/settings/ai",
                json!({"executive_enabled": true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(sink.0.lock().unwrap().is_empty());
        assert_eq!(
            event_count(&store, "changed").await,
            0,
            "a 403'd PUT must not have written any event to sin90_events"
        );

        // Positive control: human key -> 200, exactly one `setting.changed`
        // in BOTH the sink and the real `sin90_events` table.
        let resp = app
            .clone()
            .oneshot(human_req(
                "PUT",
                "/settings/ai",
                json!({"executive_enabled": true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["executive_enabled"], Value::Bool(true));
        {
            let events = sink.0.lock().unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].0, "setting.changed");
            assert_eq!(events[0].1["key"], "ai.executive_enabled");
            assert_eq!(events[0].1["value"], Value::Bool(true));
        }
        assert_eq!(
            event_count(&store, "changed").await,
            1,
            "a successful PUT must write EXACTLY one setting.changed row to sin90_events"
        );

        // GET now reflects the write.
        let resp = app.oneshot(get_req("/settings/ai")).await.unwrap();
        let body = body_json(resp).await;
        assert_eq!(body["executive_enabled"], Value::Bool(true));
    }

    #[tokio::test]
    async fn settings_ai_put_rejects_unknown_field() {
        let (app, _sink) = test_app().await;
        let resp = app
            .oneshot(human_req(
                "PUT",
                "/settings/ai",
                json!({"executive_enabled": true, "bogus": 1}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
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

// ---- POST /ai/classify + GET /ai/runs/{id} (T5.2.1, design §11.4 公共) -----

mod ai_classify {
    use super::*;
    use crate::ai::Capability;
    use crate::core::{Energy, TaskKind};

    fn no_key_req(method: &str, uri: &str, body: Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    #[tokio::test]
    async fn trigger_classify_requires_an_actor_key() {
        let (app, _sink) = test_app().await;
        let resp = app
            .oneshot(no_key_req("POST", "/ai/classify", json!({})))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    /// Positive control for the 400 below: the automation key alone is
    /// enough to trigger (design §11.4 公共's `require_any_actor`, same gate
    /// `POST /proposals` uses).
    #[tokio::test]
    async fn trigger_classify_automation_key_is_accepted() {
        let (app, _sink) = test_app().await;
        let resp = app
            .oneshot(automation_req("POST", "/ai/classify", json!({})))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn trigger_classify_over_limit_task_ids_is_400() {
        let (app, _sink) = test_app().await;
        let ids: Vec<String> = (0..21).map(|i| format!("t{i}")).collect();
        let resp = app
            .oneshot(automation_req(
                "POST",
                "/ai/classify",
                json!({"task_ids": ids}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn trigger_classify_unknown_field_is_400() {
        let (app, _sink) = test_app().await;
        let resp = app
            .oneshot(automation_req(
                "POST",
                "/ai/classify",
                json!({"oops": true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn trigger_classify_task_id_not_in_inbox_is_400() {
        let (app, _sink) = test_app().await;
        let resp = app
            .oneshot(automation_req(
                "POST",
                "/ai/classify",
                json!({"task_ids": ["does-not-exist"]}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// Pre-seeds the registry directly (rather than racing two real requests,
    /// which would be flaky against a background `tokio::spawn`) to pin the
    /// single-flight 409 shape.
    #[tokio::test]
    async fn trigger_classify_busy_returns_409_with_existing_run_id() {
        let store = Sin90Store::open_memory().await.unwrap();
        let state = Sin90State::new(
            store,
            Arc::new(RecordingSink::default()),
            crate::http::ActorKeys {
                human: HUMAN.into(),
                automation: AUTOMATION.into(),
            },
        );
        state
            .ai_runs
            .lock()
            .unwrap()
            .start(Capability::Classify, "run-already-going");
        let app = router(state, false);
        let resp = app
            .oneshot(automation_req("POST", "/ai/classify", json!({})))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = body_json(resp).await;
        assert_eq!(body["code"], "ai_busy");
        assert_eq!(body["run_id"], "run-already-going");
    }

    #[tokio::test]
    async fn get_ai_run_unknown_id_is_200_state_unknown() {
        let (app, _sink) = test_app().await;
        let resp = app
            .oneshot(get_req("/ai/runs/does-not-exist"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["state"], "unknown");
    }

    /// (2026-09-24 review, round 2, low): even when the run's registry entry
    /// is gone (evicted, or lost on a process restart), `GET /ai/runs/{id}`
    /// still reports `capability` — derived from the durable
    /// `sin90_ai_calls` rows' `task_kind`, since every row for one run
    /// shares it (J9). Mutation target: drop the
    /// `calls.first().map(|c| c.task_kind.clone())` derivation (hardcode
    /// `None`) and this goes red.
    #[tokio::test]
    async fn get_ai_run_unknown_registry_entry_still_reports_capability_from_calls() {
        let (app, _sink, store) = test_app_with_store().await;
        let rec = crate::ai::AiCallRecord {
            id: "orphan-call".into(),
            run_id: "run-orphaned".into(),
            task_kind: Capability::Classify,
            engine: crate::ai::Engine::Reflex,
            fallback_from: None,
            served_tier: None,
            model_id: None,
            prompt_tokens: None,
            completion_tokens: None,
            latency_ms: 0,
            ok: false,
            error_kind: Some("undecided"),
            proposal_id: None,
            at: "2026-09-24T00:00:00Z".into(),
        };
        crate::ai::AiSink::record_call(&store, rec).await.unwrap();

        let resp = app.oneshot(get_req("/ai/runs/run-orphaned")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["state"], "unknown");
        assert_eq!(body["capability"], "classify");
    }

    /// End-to-end through the real router: `202` → background run → polled
    /// to completion via `GET /ai/runs/{id}`. Production has no real
    /// `ModelPort` wired yet (T5.1.2), so this only exercises reflex — the
    /// task's title is crafted to overlap the Direction's title (R2) so the
    /// item deterministically ends `proposed`, not `nothing`.
    #[tokio::test]
    async fn trigger_classify_runs_in_background_and_is_pollable_to_done() {
        let (app, sink, store) = test_app_with_store().await;
        store
            .create_direction("Marketing Launch", "2026-Q4", None)
            .await
            .unwrap();
        store
            .create_task(
                "Marketing Launch checklist",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();

        let resp = app
            .clone()
            .oneshot(automation_req("POST", "/ai/classify", json!({})))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let body = body_json(resp).await;
        assert_eq!(body["capability"], "classify");
        let run_id = body["run_id"].as_str().unwrap().to_string();

        // 2026-09-24 review (L6): a bounded WALL-CLOCK deadline, not a fixed
        // iteration count — this run should finish in well under a second
        // (in-memory SQLite, one item, reflex-only), but a wide 5s ceiling
        // means a slow CI box doesn't turn a real pass into a flaky failure.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut state_str = "running".to_string();
        let mut items = Value::Null;
        let mut calls = Value::Null;
        while tokio::time::Instant::now() < deadline {
            let r = body_json(
                app.clone()
                    .oneshot(get_req(&format!("/ai/runs/{run_id}")))
                    .await
                    .unwrap(),
            )
            .await;
            state_str = r["state"].as_str().unwrap().to_string();
            items = r["items"].clone();
            calls = r["calls"].clone();
            if state_str != "running" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(state_str, "done", "run never finished: items={items:?}");
        let items = items.as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["result"], "proposed");
        // M1 (2026-09-26 review round 2): a non-skipped item carries no
        // `reason` field at all (`AiRunItem::reason`'s `skip_serializing_if`).
        assert!(
            items[0].get("reason").is_none(),
            "a non-skipped item must not have a reason field: {:?}",
            items[0]
        );
        // M5: `calls` is read from the durable `sin90_ai_calls` table.
        let calls = calls.as_array().unwrap();
        assert!(
            !calls.is_empty(),
            "the produced proposal's call row must be listed"
        );
        assert!(calls.iter().any(|c| c["ok"] == true));

        // H2 (design §11.4 公共's L1): the run must have mirrored exactly one
        // `proposal.submitted` event through the SAME `EventSink` the
        // human `POST /proposals` path uses — one per produced proposal.
        let submitted: Vec<_> = sink
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|(kind, _)| kind == "proposal.submitted")
            .cloned()
            .collect();
        assert_eq!(
            submitted.len(),
            1,
            "one proposal.submitted mirror per produced proposal: {submitted:?}"
        );
        assert!(submitted[0].1["id"].is_string());
    }

    // ---- Medium #1 (2026-09-24 review, round 2): immediate mirroring -----

    /// Panics on the Nth call to `submit` — composed AROUND an
    /// `EmittingSink` to prove that sink's emission happens per-`submit`
    /// (synchronously, before the next item is even attempted), not
    /// collected and flushed once at the very end of `run_classify`.
    struct PanicOnNth<S> {
        inner: S,
        panic_at: u32,
        count: std::sync::atomic::AtomicU32,
    }
    impl<S: crate::ai::AiSink> crate::ai::AiSink for PanicOnNth<S> {
        async fn submit(
            &self,
            cap: Capability,
            draft: crate::ai::ProposalDraft,
            rec: crate::ai::AiCallRecord,
        ) -> Result<(), crate::ai::SinkError> {
            let n = self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if n == self.panic_at {
                panic!("simulated mid-run crash on submit #{n}");
            }
            self.inner.submit(cap, draft, rec).await
        }
        async fn record_call(
            &self,
            rec: crate::ai::AiCallRecord,
        ) -> Result<(), crate::ai::SinkError> {
            self.inner.record_call(rec).await
        }
        async fn record_classify_eval(
            &self,
            task_id: &str,
            evaluated_at: &str,
        ) -> Result<(), crate::ai::SinkError> {
            self.inner.record_classify_eval(task_id, evaluated_at).await
        }
        async fn precheck(
            &self,
            cap: Capability,
            drafts: &[crate::ai::ProposalDraft],
        ) -> Vec<bool> {
            self.inner.precheck(cap, drafts).await
        }
    }

    /// Medium #1: `EmittingSink::submit` mirrors `proposal.submitted`
    /// IMMEDIATELY on its own successful commit — proven by panicking on the
    /// SECOND of two items and confirming the FIRST item's event already
    /// landed in the sink despite the run never reaching its normal end.
    /// Both items are made R1-decisive (a matching classification history)
    /// so no model is needed. Mutation target: move the `self.sink.emit(...)`
    /// call in `EmittingSink::submit` to run AFTER `run_classify` returns
    /// (i.e. revert to the old post-hoc loop) and this test's event-count
    /// assertion goes red (0 events recorded, since the run panics before
    /// reaching that point).
    #[tokio::test]
    async fn emitting_sink_emits_immediately_even_if_run_panics_on_a_later_item() {
        let store = Sin90Store::open_memory().await.unwrap();
        let direction = store
            .create_direction("Work", "2026-Q4", None)
            .await
            .unwrap();
        store
            .create_task(
                "Write the report",
                Some(&direction.id),
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();
        let task_a = store
            .create_task(
                "Write the Report",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();
        let task_b = store
            .create_task(
                "write THE report",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();

        let recording = RecordingSink::default();
        let store_for_task = store.clone();
        let recording_for_task = recording.clone();
        let handle = tokio::spawn(async move {
            let store = &store_for_task;
            let reader = store.ai_reader();
            let emitting = crate::http::ai_classify::EmittingSink {
                store,
                sink: std::sync::Arc::new(recording_for_task),
            };
            let panicking = PanicOnNth {
                inner: emitting,
                panic_at: 2,
                count: std::sync::atomic::AtomicU32::new(0),
            };
            crate::ai::classify::run_classify(
                "run-panic-mid",
                &[task_a, task_b],
                crate::ai::ModelAccess::LocalOnly,
                None::<&crate::ai::NoModelPort>,
                &panicking,
                &reader,
            )
            .await
        });
        let joined = handle.await;
        assert!(joined.is_err(), "the simulated crash must have panicked");

        let events = recording.0.lock().unwrap();
        let submitted: Vec<_> = events
            .iter()
            .filter(|(kind, _)| kind == "proposal.submitted")
            .collect();
        assert_eq!(
            submitted.len(),
            1,
            "the FIRST item's proposal must already have its event mirrored, \
             even though the run crashed before reaching the second: {events:?}"
        );
    }

    /// L2 (2026-09-24 review, round 3): the negative control
    /// `emitting_sink_emits_immediately_...` above was missing — a `submit`
    /// that FAILS its dry run (the task is already classified, so
    /// `AssignTaskDirection`'s A3 rejects it) must return `Err` AND must NOT
    /// emit `proposal.submitted` at all. Mutation target: change
    /// `EmittingSink::submit`'s `if result.is_ok()` to `if true` and this
    /// goes red (an event gets recorded for a failed submit).
    #[tokio::test]
    async fn emitting_sink_does_not_emit_when_submit_fails_its_dry_run() {
        let store = Sin90Store::open_memory().await.unwrap();
        let direction = store
            .create_direction("Work", "2026-Q4", None)
            .await
            .unwrap();
        // Already classified — ANY `AssignTaskDirection` targeting it fails
        // A3 ("not in inbox") during `submit`'s dry-run `validate`.
        let task = store
            .create_task(
                "Already classified",
                Some(&direction.id),
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();

        let recording = RecordingSink::default();
        let emitting = crate::http::ai_classify::EmittingSink {
            store: &store,
            sink: std::sync::Arc::new(recording.clone()),
        };
        let draft = crate::ai::ProposalDraft {
            id: "p-conflict".into(),
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: direction.id.clone(),
            }],
            rationale: None,
        };
        let result = crate::ai::AiSink::submit(
            &emitting,
            Capability::Classify,
            draft,
            call_rec("call-conflict"),
        )
        .await;
        assert!(
            result.is_err(),
            "submit must fail its dry run: the task is already classified (A3)"
        );

        let events = recording.0.lock().unwrap();
        assert!(
            events.is_empty(),
            "a failed submit must not emit proposal.submitted: {events:?}"
        );
    }

    // ---- J14: dedup skips/reprocesses (2026-09-24 review) ----------------

    fn call_rec(id: &str) -> crate::ai::AiCallRecord {
        crate::ai::AiCallRecord {
            id: id.into(),
            run_id: "run-dedup-http".into(),
            task_kind: Capability::Classify,
            engine: crate::ai::Engine::Reflex,
            fallback_from: None,
            served_tier: None,
            model_id: None,
            prompt_tokens: None,
            completion_tokens: None,
            latency_ms: 0,
            ok: true,
            error_kind: None,
            proposal_id: None,
            at: "2026-09-24T00:00:00Z".into(),
        }
    }

    /// J14: `dedup_targets` (now `pub(crate)`) skips a task with a still-valid
    /// PENDING `AssignTaskDirection` proposal, and stops skipping it once
    /// that proposal is no longer valid (positive control: the target
    /// Direction gets abandoned — same mechanism `ai::classify`'s own
    /// `precheck_reflects_inbox_and_direction_closure` pins at the
    /// `AiSink::precheck` layer; this test pins it at the HTTP layer's own
    /// `dedup_targets` wrapper instead).
    #[tokio::test]
    async fn dedup_skips_valid_pending_then_reprocesses_after_direction_abandoned() {
        let (_app, _sink, store) = test_app_with_store().await;
        let direction = store
            .create_direction("Side quest", "2026-Q4", None)
            .await
            .unwrap();
        let task = store
            .create_task(
                "Ambiguous task",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();

        let draft = crate::ai::ProposalDraft {
            id: "p-dedup-http-1".into(),
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: direction.id.clone(),
            }],
            rationale: None,
        };
        crate::ai::AiSink::submit(&store, Capability::Classify, draft, call_rec("c1"))
            .await
            .unwrap();

        let (kept, skipped) =
            crate::http::ai_classify::dedup_targets(&store, std::slice::from_ref(&task))
                .await
                .unwrap();
        assert!(
            kept.is_empty(),
            "the still-valid pending proposal must skip this task"
        );
        assert_eq!(skipped, vec![(task.id.clone(), "dedup")]);

        // Positive control: abandon the target Direction — the pending
        // proposal's dry-run now fails A5, so it no longer blocks anything.
        sqlx::query("UPDATE sin90_directions SET status = 'abandoned' WHERE id = ?")
            .bind(&direction.id)
            .execute(store.pool())
            .await
            .unwrap();
        let (kept2, skipped2) =
            crate::http::ai_classify::dedup_targets(&store, std::slice::from_ref(&task))
                .await
                .unwrap();
        assert_eq!(
            kept2.len(),
            1,
            "an invalidated pending proposal must no longer block it"
        );
        assert!(skipped2.is_empty());
    }

    // ---- T5.7.2 (design §2 #31): rejected-suggestion suppression ----------

    /// `cargo test suppress_`'s primary judgement: a REJECTED classify
    /// (`AssignTaskDirection`) proposal keeps blocking `dedup_targets` for
    /// its target task on the NEXT round — the opposite of what this same
    /// scenario did before T5.7.2 (a rejection used to unblock dedup
    /// immediately, letting classify re-propose the exact thing a human just
    /// turned down). `reason` is `"suppressed_rejected"`, distinct from a
    /// still-PENDING proposal's `"dedup"`.
    #[tokio::test]
    async fn suppress_rejected_classify_blocks_next_round() {
        let (_app, _sink, store) = test_app_with_store().await;
        let direction = store
            .create_direction("Side quest", "2026-Q4", None)
            .await
            .unwrap();
        let task = store
            .create_task(
                "Ambiguous task",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();

        let draft = crate::ai::ProposalDraft {
            id: "p-suppress-1".into(),
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: direction.id.clone(),
            }],
            rationale: None,
        };
        crate::ai::AiSink::submit(
            &store,
            Capability::Classify,
            draft,
            call_rec("c-suppress-1"),
        )
        .await
        .unwrap();

        // Negative control: still pending — blocks dedup under the OLD
        // reason too.
        let (kept, skipped) =
            crate::http::ai_classify::dedup_targets(&store, std::slice::from_ref(&task))
                .await
                .unwrap();
        assert!(kept.is_empty());
        assert_eq!(skipped, vec![(task.id.clone(), "dedup")]);

        store.reject_proposal("p-suppress-1", None).await.unwrap();

        // The judgement: a rejected proposal now SUPPRESSES, not unblocks.
        let (kept2, skipped2) =
            crate::http::ai_classify::dedup_targets(&store, std::slice::from_ref(&task))
                .await
                .unwrap();
        assert!(
            kept2.is_empty(),
            "a rejected suggestion must stay suppressed when nothing has changed"
        );
        assert_eq!(skipped2, vec![(task.id.clone(), "suppressed_rejected")]);

        let rows = crate::store::test_hooks::proposal_rejection_rows(&store, "p-suppress-1")
            .await
            .unwrap();
        assert_eq!(rows[0].capability_source, "classify");
    }

    /// Positive control ① (tasks.md T5.7.2's own acceptance line): the task
    /// itself gets modified after the rejection (a genuine human
    /// `transition_task` status change, the only "edit an inbox task" route
    /// this codebase has today) — suppression lifts.
    #[tokio::test]
    async fn suppress_rejected_classify_positive_control_task_modified_reproposes() {
        let (_app, _sink, store) = test_app_with_store().await;
        let direction = store
            .create_direction("Side quest", "2026-Q4", None)
            .await
            .unwrap();
        let task = store
            .create_task(
                "Ambiguous task",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();
        let draft = crate::ai::ProposalDraft {
            id: "p-suppress-2".into(),
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: direction.id.clone(),
            }],
            rationale: None,
        };
        crate::ai::AiSink::submit(
            &store,
            Capability::Classify,
            draft,
            call_rec("c-suppress-2"),
        )
        .await
        .unwrap();
        store.reject_proposal("p-suppress-2", None).await.unwrap();

        // Negative control (before the edit): still suppressed.
        let (kept, _) =
            crate::http::ai_classify::dedup_targets(&store, std::slice::from_ref(&task))
                .await
                .unwrap();
        assert!(kept.is_empty(), "unchanged situation must stay suppressed");

        // The edit: a human moves the task's own status — review round 2
        // (M1): the "task modified" judgement is now a real,
        // `sin90_events`-backed check (`Sin90Store::task_modified_since`),
        // not a raw `updated_at` compare, so what must move forward here is
        // the `"transitioned"` event's own `at`, not the task row's
        // `updated_at` column.
        store
            .transition_task(&task.id, crate::core::TaskStatus::Planned)
            .await
            .unwrap();
        // `now_iso8601()` is second-resolution, so a same-second reject+edit
        // in this test would otherwise tie; force the edit's timestamp
        // forward instead of sleeping (mirrors `set_proposal_created_at`'s
        // own doc for the identical reasoning).
        crate::store::test_hooks::set_task_transitioned_at(
            &store,
            &task.id,
            "2099-01-01T00:00:00Z",
        )
        .await
        .unwrap();
        let refetched = crate::ai::AiReadModel::inbox_task(&store.ai_reader(), &task.id)
            .await
            .unwrap()
            .expect("task is still direction_id IS NULL, still in the inbox");

        let (kept2, skipped2) =
            crate::http::ai_classify::dedup_targets(&store, std::slice::from_ref(&refetched))
                .await
                .unwrap();
        assert_eq!(
            kept2.len(),
            1,
            "a modified task must reproposed after its own rejection"
        );
        assert!(skipped2.is_empty());
    }

    /// T5.7.2 review round 2 (M2): the "situation changed" time basis is the
    /// rejected proposal's OWN `proposed_at` (when it was first submitted),
    /// not `rejected_at` (when a human finally got around to deciding on
    /// it) — a real gap between the two (a proposal a human sat on for a
    /// while before rejecting) must not hide a Direction that appeared
    /// DURING that gap. Forces `proposed_at` safely into the past, creates a
    /// "gap" Direction at real "now", and only THEN rejects (so the gap
    /// Direction's `created_at` is provably `<= rejected_at`, never `>` it —
    /// the OLD `rejected_at`-basis code would therefore have called this
    /// "not new" and kept the suppression). Mutation target: swap
    /// `r.proposed_at` back for `r.rejected_at` in `dedup_targets` and this
    /// goes red (`kept` goes back to empty).
    #[tokio::test]
    async fn suppress_rejected_classify_uses_proposed_at_not_rejected_at_as_situation_basis() {
        let (_app, _sink, store) = test_app_with_store().await;
        let direction_a = store
            .create_direction("Rejected target", "2026-Q4", None)
            .await
            .unwrap();
        let task = store
            .create_task(
                "Ambiguous task",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();

        let draft = crate::ai::ProposalDraft {
            id: "p-m2-gap".into(),
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: direction_a.id.clone(),
            }],
            rationale: None,
        };
        crate::ai::AiSink::submit(&store, Capability::Classify, draft, call_rec("c-m2-gap"))
            .await
            .unwrap();
        crate::store::test_hooks::set_proposal_created_at(
            &store,
            "p-m2-gap",
            "2020-01-01T00:00:00Z",
        )
        .await
        .unwrap();

        // A Direction appears WHILE the (still-pending) proposal is sitting
        // there, unrejected.
        let _gap_direction = store
            .create_direction("Created during the gap", "2026-Q4", None)
            .await
            .unwrap();

        // Only NOW does the human reject it — `rejected_at` lands at real
        // "now", provably at or after `gap_direction`'s `created_at`.
        store.reject_proposal("p-m2-gap", None).await.unwrap();

        let (kept, skipped) =
            crate::http::ai_classify::dedup_targets(&store, std::slice::from_ref(&task))
                .await
                .unwrap();
        assert_eq!(
            kept.len(),
            1,
            "a Direction created between proposing and rejecting must already count as \
             \"new\" — the situation-changed basis is proposed_at, not rejected_at: {skipped:?}"
        );
        assert!(skipped.is_empty());
    }

    /// T5.7.2 review round 2 (M5): a single `sin90_proposal_rejections` row
    /// whose joined `sin90_proposals.ops` is corrupted JSON must not fail
    /// `list_rejected_ops`/`dedup_targets` for EVERY OTHER task — it is
    /// logged and skipped (`Sin90Store::list_rejected_ops`'s own doc), not
    /// propagated as an `Err` that would take an unrelated task's own valid
    /// suppression down with it. Mutation target: revert `list_rejected_ops`
    /// to a bare `.map(..).collect::<Result<Vec<_>>>()` and this goes red —
    /// `dedup_targets` returns `Err`, and the `.unwrap()` below panics.
    #[tokio::test]
    async fn dedup_targets_skips_a_row_with_unparseable_ops_json() {
        let (_app, _sink, store) = test_app_with_store().await;
        let direction_a = store
            .create_direction("Rejected target", "2026-Q4", None)
            .await
            .unwrap();
        let task = store
            .create_task(
                "Ambiguous task",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();

        // A genuine, well-formed rejection for `task` — the suppression
        // this test confirms still works.
        let draft = crate::ai::ProposalDraft {
            id: "p-m5-good".into(),
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: direction_a.id.clone(),
            }],
            rationale: None,
        };
        crate::ai::AiSink::submit(&store, Capability::Classify, draft, call_rec("c-m5-good"))
            .await
            .unwrap();
        store.reject_proposal("p-m5-good", None).await.unwrap();

        // A SEPARATE, corrupted row: a proposal whose `ops` column is not
        // valid JSON at all, rejected under the same capability.
        sqlx::query(
            "INSERT INTO sin90_proposals (id, status, source, ops, created_at)
             VALUES ('p-m5-corrupt', 'rejected', 'rule', 'not valid json {{{', ?)",
        )
        .bind(crate::core::now_iso8601())
        .execute(store.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sin90_proposal_rejections
                 (id, proposal_id, capability_source, proposal_source, ops_summary,
                  proposed_at, rejected_at)
             VALUES ('r-m5-corrupt', 'p-m5-corrupt', 'classify', 'rule', 'corrupt', ?, ?)",
        )
        .bind(crate::core::now_iso8601())
        .bind(crate::core::now_iso8601())
        .execute(store.pool())
        .await
        .unwrap();

        let (kept, skipped) =
            crate::http::ai_classify::dedup_targets(&store, std::slice::from_ref(&task))
                .await
                .unwrap();
        assert!(
            kept.is_empty(),
            "the task's OWN valid rejection must still suppress it despite the corrupt row: {kept:?}"
        );
        assert_eq!(skipped, vec![(task.id.clone(), "suppressed_rejected")]);
    }

    /// Positive control ②: no edit to the task at all, but a brand-new
    /// non-terminal Direction appears after the rejection — suppression
    /// lifts (the "situation changed" OR's second leg).
    #[tokio::test]
    async fn suppress_rejected_classify_positive_control_new_direction_reproposes() {
        let (_app, _sink, store) = test_app_with_store().await;
        let direction = store
            .create_direction("Side quest", "2026-Q4", None)
            .await
            .unwrap();
        let task = store
            .create_task(
                "Ambiguous task",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();
        let draft = crate::ai::ProposalDraft {
            id: "p-suppress-3".into(),
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: direction.id.clone(),
            }],
            rationale: None,
        };
        crate::ai::AiSink::submit(
            &store,
            Capability::Classify,
            draft,
            call_rec("c-suppress-3"),
        )
        .await
        .unwrap();
        store.reject_proposal("p-suppress-3", None).await.unwrap();

        let (kept, _) =
            crate::http::ai_classify::dedup_targets(&store, std::slice::from_ref(&task))
                .await
                .unwrap();
        assert!(
            kept.is_empty(),
            "no new Direction yet — must stay suppressed"
        );

        let fresh = store
            .create_direction("Freshly created", "2026-Q4", None)
            .await
            .unwrap();
        // Force this Direction's `created_at` forward past `proposed_at` —
        // same same-second tie concern the positive control above has.
        crate::store::test_hooks::set_direction_created_at(
            &store,
            &fresh.id,
            "2099-01-01T00:00:00Z",
        )
        .await
        .unwrap();

        let (kept2, skipped2) =
            crate::http::ai_classify::dedup_targets(&store, std::slice::from_ref(&task))
                .await
                .unwrap();
        assert_eq!(
            kept2.len(),
            1,
            "a new non-terminal Direction must lift the suppression"
        );
        assert!(skipped2.is_empty());
    }

    /// No-retry negative control, the acceptance line's last bullet: without
    /// EITHER leg of "situation changed", a rejected suggestion never comes
    /// back on its own, no matter how many times dedup runs.
    #[tokio::test]
    async fn suppress_rejected_classify_no_retry_without_situation_change() {
        let (_app, _sink, store) = test_app_with_store().await;
        let direction = store
            .create_direction("Side quest", "2026-Q4", None)
            .await
            .unwrap();
        let task = store
            .create_task(
                "Ambiguous task",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();
        let draft = crate::ai::ProposalDraft {
            id: "p-suppress-4".into(),
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: direction.id.clone(),
            }],
            rationale: None,
        };
        crate::ai::AiSink::submit(
            &store,
            Capability::Classify,
            draft,
            call_rec("c-suppress-4"),
        )
        .await
        .unwrap();
        store.reject_proposal("p-suppress-4", None).await.unwrap();

        for _ in 0..3 {
            let (kept, skipped) =
                crate::http::ai_classify::dedup_targets(&store, std::slice::from_ref(&task))
                    .await
                    .unwrap();
            assert!(kept.is_empty());
            assert_eq!(skipped, vec![(task.id.clone(), "suppressed_rejected")]);
        }
    }

    /// T5.7.2 review round 2 (M1): the OLD version of this test's "angle
    /// (a)" assertion — "an accepted real-Direction assignment leaves the
    /// inbox for good" — was a FALSE POSITIVE: it is trivially true simply
    /// because a real (non-待定, non-NULL) `direction_id` is never inbox-
    /// eligible at all (`AiReadModel::inbox_task`'s own membership test),
    /// regardless of any "was this task modified" judgement whatsoever — it
    /// never actually exercised the trap the doc comment claimed to guard.
    /// Replaced with the REAL trap `docs/DESIGN-LIFEOS.md` §2 #31 calls out:
    /// an ACCEPTED, UNRELATED `ReorderTasks` proposal bumps
    /// `sin90_tasks.updated_at` for EVERY task in the reordered week
    /// (`Sin90Op::ReorderTasks`'s apply, `store/repo.rs`) — including one
    /// still sitting, untouched, in the classify inbox — and that bump must
    /// never be mistaken for "a human edited this task" (a raw `updated_at`
    /// compare used to be fooled by exactly this;
    /// `Sin90Store::task_modified_since`'s own doc has the full story).
    /// Mutation target: revert `task_modified_since` to a plain
    /// `sin90_tasks.updated_at > since` compare and this goes red (the
    /// reorder bump wrongly lifts the suppression).
    #[tokio::test]
    async fn suppress_rejected_classify_accept_bump_does_not_count_as_modification() {
        let (_app, _sink, store) = test_app_with_store().await;
        let direction_a = store
            .create_direction("Rejected target", "2026-Q4", None)
            .await
            .unwrap();
        let week = store.create_week("2026-W22").await.unwrap();

        // The task lives in `week` (so an unrelated `ReorderTasks` can touch
        // it) with `direction_id: None` — still a normal classify-inbox
        // candidate; week membership and Direction assignment are
        // independent axes.
        let seed = crate::core::Sin90Proposal {
            id: "seed-reorder-trap".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![crate::core::Sin90Op::CreateTasks {
                week_id: week.id.clone(),
                tasks: vec![
                    crate::core::NewTask {
                        title: "Ambiguous task".into(),
                        direction_id: None,
                    },
                    crate::core::NewTask {
                        title: "sibling".into(),
                        direction_id: None,
                    },
                ],
            }],
            rationale: None,
        };
        store.submit_proposal(&seed).await.unwrap();
        store.apply_proposal(&seed.id).await.unwrap();
        let task_ids: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM sin90_tasks WHERE week_id = ? ORDER BY sort_key ASC",
        )
        .bind(&week.id)
        .fetch_all(store.pool())
        .await
        .unwrap();
        let task_id = task_ids[0].clone();

        // Round 1: propose direction_a for the task, human rejects it.
        let draft_a = crate::ai::ProposalDraft {
            id: "p-suppress-trap-1".into(),
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task_id.clone(),
                direction_id: direction_a.id.clone(),
            }],
            rationale: None,
        };
        crate::ai::AiSink::submit(
            &store,
            Capability::Classify,
            draft_a,
            call_rec("c-suppress-trap-1"),
        )
        .await
        .unwrap();
        store
            .reject_proposal("p-suppress-trap-1", None)
            .await
            .unwrap();

        // Round 2: an UNRELATED, ACCEPTED `ReorderTasks` for the whole week
        // — bumps `updated_at` for every task in it, including ours, but is
        // not a human editing THIS task's content.
        let reorder = crate::ai::ProposalDraft {
            id: "p-reorder-trap".into(),
            ops: vec![crate::core::Sin90Op::ReorderTasks {
                week_id: week.id.clone(),
                order: vec![task_ids[1].clone(), task_ids[0].clone()],
            }],
            rationale: None,
        };
        let reorder_rec = crate::ai::AiCallRecord {
            id: "c-reorder-trap".into(),
            run_id: "run-reorder-trap".into(),
            task_kind: Capability::Propose,
            engine: crate::ai::Engine::Reflex,
            fallback_from: None,
            served_tier: None,
            model_id: None,
            prompt_tokens: None,
            completion_tokens: None,
            latency_ms: 0,
            ok: true,
            error_kind: None,
            proposal_id: None,
            at: "2026-09-24T00:00:00Z".into(),
        };
        crate::ai::AiSink::submit(&store, Capability::Propose, reorder, reorder_rec)
            .await
            .unwrap();
        store.apply_proposal("p-reorder-trap").await.unwrap();
        // Force `updated_at` far past `since` (`proposed_at`) so a mutant
        // reverting `task_modified_since` to a naive `sin90_tasks.updated_at
        // > since` comparison is caught for certain — `ReorderTasks`'s own
        // bump already lands after `since` in real wall-clock time, but at
        // second resolution the two can tie within the same test run; this
        // makes the mutation-kill deterministic instead of a coin flip.
        crate::store::test_hooks::set_task_updated_at(&store, &task_id, "2099-01-01T00:00:00Z")
            .await
            .unwrap();

        let refetched = store
            .list_tasks(None, None, None)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.id == task_id)
            .expect("task still exists");
        assert!(
            refetched.direction_id.is_none(),
            "still un-Direction-ed, still a normal classify candidate"
        );

        let (kept, skipped) =
            crate::http::ai_classify::dedup_targets(&store, std::slice::from_ref(&refetched))
                .await
                .unwrap();
        assert!(
            kept.is_empty(),
            "an accepted, unrelated ReorderTasks bump must not count as \
             \"task modified\" — suppression must hold"
        );
        assert_eq!(skipped, vec![(task_id.clone(), "suppressed_rejected")]);
    }

    /// T5.7.2 review round 3: `dedup_targets`/`dedup_targets_with_rejected`
    /// used to bail out of their WHOLE call the moment ANY of their
    /// rejection-related reads failed (`list_rejected_ops` at the top, or
    /// `task_modified_since`/`max_eligible_direction_created_at` inside
    /// `dedup_targets_with_rejected`) — throwing away the UNRELATED
    /// pending-proposal dedup half along with it. Dropping
    /// `sin90_proposal_rejections` (AFTER a real rejection row already
    /// exists in it) fails `list_rejected_ops` for certain, without
    /// touching anything `AiSink::precheck`'s own dry run needs (that table
    /// backs nothing else — see its own doc). The judgement: a still-valid
    /// pending proposal for an UNRELATED task must still skip it
    /// (`"dedup"`), even while the rejection-suppression half degrades —
    /// fail-open (simply not suppressed), not a crash.
    #[tokio::test]
    async fn dedup_targets_degrades_rejection_suppression_without_losing_pending_dedup() {
        let (_app, _sink, store) = test_app_with_store().await;
        let direction = store
            .create_direction("Side quest", "2026-Q4", None)
            .await
            .unwrap();

        // Task A: a still-valid PENDING proposal — the "dedup" half.
        let task_dedup = store
            .create_task(
                "Ambiguous task A",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();
        let draft = crate::ai::ProposalDraft {
            id: "p-degrade-dedup".into(),
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task_dedup.id.clone(),
                direction_id: direction.id.clone(),
            }],
            rationale: None,
        };
        crate::ai::AiSink::submit(
            &store,
            Capability::Classify,
            draft,
            call_rec("c-degrade-dedup"),
        )
        .await
        .unwrap();

        // Task B: a REJECTED proposal that would normally suppress it — the
        // "rejection" half this test corrupts the read for.
        let task_rejected = store
            .create_task(
                "Ambiguous task B",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();
        let draft2 = crate::ai::ProposalDraft {
            id: "p-degrade-rejected".into(),
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task_rejected.id.clone(),
                direction_id: direction.id.clone(),
            }],
            rationale: None,
        };
        crate::ai::AiSink::submit(
            &store,
            Capability::Classify,
            draft2,
            call_rec("c-degrade-rejected"),
        )
        .await
        .unwrap();
        store
            .reject_proposal("p-degrade-rejected", None)
            .await
            .unwrap();

        // Corrupt ONLY the rejection-read path.
        sqlx::query("DROP TABLE sin90_proposal_rejections")
            .execute(store.pool())
            .await
            .unwrap();

        let (kept, skipped) = crate::http::ai_classify::dedup_targets(
            &store,
            &[task_dedup.clone(), task_rejected.clone()],
        )
        .await
        .unwrap();

        assert_eq!(
            kept,
            vec![task_rejected.clone()],
            "rejection-suppression must degrade to \"not suppressed\" \
             (fail-open) when its read fails, instead of aborting the whole \
             call"
        );
        assert_eq!(
            skipped,
            vec![(task_dedup.id.clone(), "dedup")],
            "the unrelated pending-proposal dedup must be entirely \
             unaffected by the broken rejection read"
        );
    }

    /// T5.7.2 review round 2 (H1): replaces `suppress_rejected_classify_
    /// does_not_apply_to_a_triage_parked_task` (removed) — that test pinned
    /// the OLD, buggy scoping (`rejectable` filtered on literal
    /// `direction_id.is_none()`) and only passed because of it; the review
    /// found this scoping backwards: it meant a 待定-parked task's OWN
    /// rejected reclassify-to-D recommendation was NEVER honored, so
    /// classify re-proposed the SAME already-rejected D every time the
    /// SEPARATE 待定 retry gate (`AiReadModel::inbox`/`inbox_task`, `store/
    /// ai_port.rs`) re-qualified the task, for as long as nothing else
    /// changed — exactly the "情况没变不再提" violation Q9 exists to
    /// prevent. This pins the REAL semantics instead: a 待定 task's own
    /// rejected reclassification stays suppressed while nothing changed,
    /// and lifts once a genuinely new Direction appears — the SAME
    /// "situation changed" judgement an ordinary inbox task's rejection
    /// already got. Mutation target: reintroduce the `direction_id.is_none()`
    /// filter on `rejectable` and the first `dedup_targets` call below goes
    /// from `kept.is_empty()` to `kept.len() == 1` (the task falls out of
    /// `rejectable` entirely, so it is never suppressed at all).
    #[tokio::test]
    async fn suppress_rejected_classify_applies_to_a_triage_parked_tasks_own_rejection() {
        let (_app, _sink, store) = test_app_with_store().await;
        let task = store
            .create_task(
                "Ambiguous task",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();

        // Fallback-classified into 待定 first (T5.2.2's own path).
        let triage = crate::ai::ProposalDraft {
            id: "p-h1-triage".into(),
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: crate::core::TRIAGE_DIRECTION_ID.to_string(),
            }],
            rationale: None,
        };
        crate::ai::AiSink::submit(
            &store,
            Capability::Classify,
            triage,
            call_rec("c-h1-triage"),
        )
        .await
        .unwrap();
        store.apply_proposal("p-h1-triage").await.unwrap();

        // classify recommends reclassifying it to a real Direction — the
        // human rejects THAT.
        let direction_d = store
            .create_direction("Rejected target", "2026-Q4", None)
            .await
            .unwrap();
        let reclassify = crate::ai::ProposalDraft {
            id: "p-h1-reclassify".into(),
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: direction_d.id.clone(),
            }],
            rationale: None,
        };
        crate::ai::AiSink::submit(
            &store,
            Capability::Classify,
            reclassify,
            call_rec("c-h1-reclassify"),
        )
        .await
        .unwrap();
        store
            .reject_proposal("p-h1-reclassify", None)
            .await
            .unwrap();

        let triage_task = store
            .list_tasks(None, None, None)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.id == task.id)
            .expect("task still exists");
        assert_eq!(
            triage_task.direction_id.as_deref(),
            Some(crate::core::TRIAGE_DIRECTION_ID)
        );

        // `dedup_targets` is handed the task DIRECTLY (bypassing the inbox's
        // own 待定 retry gate, a SEPARATE mechanism, §2 #31) so this pins
        // ONLY the rejection-suppression judgement.
        let (kept, skipped) =
            crate::http::ai_classify::dedup_targets(&store, std::slice::from_ref(&triage_task))
                .await
                .unwrap();
        assert!(
            kept.is_empty(),
            "nothing changed since the rejection — a 待定 task's own rejected \
             reclassification must stay suppressed, same as an ordinary inbox task's"
        );
        assert_eq!(skipped, vec![(task.id.clone(), "suppressed_rejected")]);

        // A genuinely new Direction appears afterward — suppression lifts.
        let direction_d2 = store.create_direction("D2", "2026-Q4", None).await.unwrap();
        crate::store::test_hooks::set_direction_created_at(
            &store,
            &direction_d2.id,
            "2099-01-01T00:00:00Z",
        )
        .await
        .unwrap();

        let (kept2, skipped2) =
            crate::http::ai_classify::dedup_targets(&store, std::slice::from_ref(&triage_task))
                .await
                .unwrap();
        assert_eq!(
            kept2.len(),
            1,
            "a new non-terminal Direction must lift the suppression"
        );
        assert!(skipped2.is_empty());
    }

    // ---- T5.7.2 (design §2 #31, T5.2.2 followup ②): 待定 reclassification -

    /// `cargo test suppress_`'s 待定 judgements: a task fallback-classified
    /// into 待定 stays excluded from the inbox while no eligible Direction
    /// has appeared since (negative control), and becomes reclassifiable the
    /// moment one does (positive control) — `AiReadModel::inbox`/
    /// `inbox_task`'s own SQL gate, independent of `sin90_proposal_
    /// rejections` entirely (no proposal was ever rejected in this test).
    #[tokio::test]
    async fn suppress_triage_reclassifies_after_new_direction_but_not_before() {
        let (_app, _sink, store) = test_app_with_store().await;
        // Exists BEFORE the 待定 fallback — must NOT itself count as "new".
        let _old_direction = store
            .create_direction("Pre-existing Direction", "2026-Q4", None)
            .await
            .unwrap();
        // PR#69 review round 1: pinned safely in the past — otherwise this
        // Direction's real wall-clock `created_at` and the task's own
        // (post-fallback) `triage_entered_at`, stamped moments later, risk
        // landing in the SAME second, and the gate's widened `>=` (a
        // same-second tie must still count as "seen" for a Direction born
        // MID-BATCH, see `classify_retry_gate_not_masked_by_direction_born_
        // mid_batch`) would then wrongly treat this pre-existing Direction
        // as "new" too, defeating this negative control's own premise.
        crate::store::test_hooks::set_direction_created_at(
            &store,
            &_old_direction.id,
            "2020-01-01T00:00:00Z",
        )
        .await
        .unwrap();
        let task = store
            .create_task(
                "Mystery task",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();

        let draft = crate::ai::ProposalDraft {
            id: "p-triage-1".into(),
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: crate::core::TRIAGE_DIRECTION_ID.to_string(),
            }],
            rationale: None,
        };
        crate::ai::AiSink::submit(&store, Capability::Classify, draft, call_rec("c-triage-1"))
            .await
            .unwrap();
        store.apply_proposal("p-triage-1").await.unwrap();

        let reader = store.ai_reader();

        // Negative control: only the OLD (pre-fallback) Direction exists —
        // must not retry.
        let inbox = crate::ai::AiReadModel::inbox(&reader, 100).await.unwrap();
        assert!(
            !inbox.iter().any(|t| t.id == task.id),
            "no new Direction since the 待定 fallback — must not retry"
        );
        assert!(crate::ai::AiReadModel::inbox_task(&reader, &task.id)
            .await
            .unwrap()
            .is_none());

        // Positive control: a NEW non-terminal Direction appears afterward.
        // Forced forward past the task's own (post-fallback) `updated_at` —
        // `now_iso8601()` is second-resolution, so a same-second sequence in
        // this test would otherwise tie (same reasoning the classify-side
        // positive controls above document).
        let fresh = store
            .create_direction("Freshly created", "2026-Q4", None)
            .await
            .unwrap();
        crate::store::test_hooks::set_direction_created_at(
            &store,
            &fresh.id,
            "2099-01-01T00:00:00Z",
        )
        .await
        .unwrap();

        let inbox2 = crate::ai::AiReadModel::inbox(&reader, 100).await.unwrap();
        assert!(
            inbox2.iter().any(|t| t.id == task.id),
            "a Direction created after the 待定 fallback must let it retry"
        );
        assert!(crate::ai::AiReadModel::inbox_task(&reader, &task.id)
            .await
            .unwrap()
            .is_some());
    }

    /// `AssignTaskDirection`'s A3 (`core::proposal::validate`) actually lets
    /// a 待定-parked task be reassigned to a REAL Direction — the inbox-level
    /// gate above only controls whether classify SELECTS the task; this pins
    /// that an `AssignTaskDirection` proposal targeting it can still be
    /// submitted and accepted once selected, and that 待定 → 待定 stays
    /// refused (§2 #30's own "不放宽待定→待定").
    #[tokio::test]
    async fn suppress_triage_reassign_to_real_direction_ok_triage_to_triage_still_not_in_inbox() {
        let (_app, _sink, store) = test_app_with_store().await;
        let task = store
            .create_task(
                "Mystery task",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();
        let seed = crate::ai::ProposalDraft {
            id: "p-triage-2".into(),
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: crate::core::TRIAGE_DIRECTION_ID.to_string(),
            }],
            rationale: None,
        };
        crate::ai::AiSink::submit(&store, Capability::Classify, seed, call_rec("c-triage-2"))
            .await
            .unwrap();
        store.apply_proposal("p-triage-2").await.unwrap();

        let real = store
            .create_direction("Real home", "2026-Q4", None)
            .await
            .unwrap();

        // 待定 → 待定 is still refused (A3's carve-out is one-directional).
        let again_triage = crate::ai::ProposalDraft {
            id: "p-triage-again".into(),
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: crate::core::TRIAGE_DIRECTION_ID.to_string(),
            }],
            rationale: None,
        };
        let err = crate::ai::AiSink::submit(
            &store,
            Capability::Classify,
            again_triage,
            call_rec("c-triage-again"),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, crate::ai::SinkError::Invalid(_)));

        // 待定 → real Direction goes through.
        let reclassify = crate::ai::ProposalDraft {
            id: "p-triage-3".into(),
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: real.id.clone(),
            }],
            rationale: None,
        };
        crate::ai::AiSink::submit(
            &store,
            Capability::Classify,
            reclassify,
            call_rec("c-triage-3"),
        )
        .await
        .unwrap();
        store.apply_proposal("p-triage-3").await.unwrap();

        let refetched = store
            .list_tasks(None, None, None)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.id == task.id)
            .expect("task still exists");
        assert_eq!(refetched.direction_id.as_deref(), Some(real.id.as_str()));
    }

    // ---- T5.7.2 review round 2 (H2): 待定 retry gate's own exit condition -

    /// H2 core: without `sin90_classify_evals`, a triage task classify
    /// re-examines and STILL cannot place would re-qualify as a target on
    /// EVERY future run for as long as the same newest Direction stays
    /// newest (`none`/low-confidence/no-conclusion never move
    /// `sin90_tasks.updated_at`, the old gate's comparand). Pins the fixed
    /// gate's three states directly against `AiSink::record_classify_eval`
    /// (the pipeline-level regression, at `MAX_CLASSIFY_TASK_IDS` scale, is
    /// `suppress_triage_retry_does_not_starve_new_tasks` below): (1) a newer
    /// eligible Direction opens the gate: (2) recording an evaluation closes
    /// it again for that SAME Direction; (3) a Direction newer still
    /// re-opens it.
    #[tokio::test]
    async fn suppress_triage_retry_gate_advances_past_a_fruitless_evaluation() {
        let (_app, _sink, store) = test_app_with_store().await;
        let task = store
            .create_task(
                "Mystery task",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();
        let draft = crate::ai::ProposalDraft {
            id: "p-h2-gate-1".into(),
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: crate::core::TRIAGE_DIRECTION_ID.to_string(),
            }],
            rationale: None,
        };
        crate::ai::AiSink::submit(&store, Capability::Classify, draft, call_rec("c-h2-gate-1"))
            .await
            .unwrap();
        store.apply_proposal("p-h2-gate-1").await.unwrap();
        // Pin "entered 待定" safely in the past — `now_iso8601()`'s
        // second-resolution would otherwise risk a same-second tie against
        // the Directions created right below (mirrors every other
        // `set_*_at`-backdating test in this suite).
        crate::store::test_hooks::set_task_triage_entered_at(
            &store,
            &task.id,
            "2020-01-01T00:00:00Z",
        )
        .await
        .unwrap();

        let new_direction = store
            .create_direction("Freshly created", "2026-Q4", None)
            .await
            .unwrap();
        let reader = store.ai_reader();
        assert!(
            crate::ai::AiReadModel::inbox_task(&reader, &task.id)
                .await
                .unwrap()
                .is_some(),
            "a newer eligible Direction must open the gate"
        );

        // classify looked at the task and STILL found nothing new (H2's own
        // exit condition). PR#69 review round 1: `evaluated_at` is stamped a
        // few seconds AFTER `new_direction`'s real wall-clock `created_at` —
        // not `now_iso8601()` right here, which risks landing in the SAME
        // second and, under the gate's widened `>=` (a same-second tie must
        // still count as "seen"), wrongly re-opening the gate this
        // assertion is pinning shut.
        crate::ai::AiSink::record_classify_eval(
            &store,
            &task.id,
            &crate::core::iso8601_after_secs(5),
        )
        .await
        .unwrap();
        assert!(
            crate::ai::AiReadModel::inbox_task(&reader, &task.id)
                .await
                .unwrap()
                .is_none(),
            "the SAME newest Direction must not re-qualify the task right after it was evaluated"
        );

        // A Direction newer than the evaluation re-opens the gate.
        let newer_direction = store
            .create_direction("Even fresher", "2026-Q4", None)
            .await
            .unwrap();
        crate::store::test_hooks::set_direction_created_at(
            &store,
            &newer_direction.id,
            "2099-01-01T00:00:00Z",
        )
        .await
        .unwrap();
        let _ = new_direction; // kept only to document it existed before the eval
        assert!(
            crate::ai::AiReadModel::inbox_task(&reader, &task.id)
                .await
                .unwrap()
                .is_some(),
            "a Direction newer than the last evaluation must re-open the gate"
        );
    }

    /// H2 at pipeline scale, the review's own starvation scenario: without
    /// the gate's exit condition, more than `MAX_CLASSIFY_TASK_IDS` (20)
    /// 待定 tasks the model keeps deciding "none" about would occupy EVERY
    /// target slot forever, starving a genuinely new inbox task out of ever
    /// being selected. Builds 21 already-triage tasks (all gate-eligible,
    /// oldest-first) plus one fresh ordinary inbox task created last, and
    /// runs the real `auto_select_targets` → `run_classify` pipeline twice:
    /// run 1 selects the 20 OLDEST 待定 tasks (the cap); `ExplicitNoneModel`
    /// decides "none" for each (already in 待定, so `submit_direction_
    /// assignment`'s re-attempt is refused by A3 → `Rejected`) — `classify_
    /// one`'s H2 write-eval still fires on that branch. Run 2's `auto_
    /// select_targets` then finds those 20 no longer gate-eligible and
    /// reaches the fresh task instead. Mutation target: revert `classify_
    /// one`'s H2 write-eval block to a no-op and run 2 goes back to
    /// re-selecting the same stale 20, never reaching the fresh task.
    #[tokio::test]
    async fn suppress_triage_retry_does_not_starve_new_tasks() {
        let (_app, _sink, store) = test_app_with_store().await;

        let cap = crate::ai::classify::MAX_CLASSIFY_TASK_IDS;
        let mut triage_ids = Vec::new();
        for i in 0..=cap {
            // 0..=cap is cap+1 tasks — one more than the selection cap.
            let t = store
                .create_task(
                    &format!("Mystery {i}"),
                    None,
                    None,
                    TaskKind::Other,
                    Energy::Mid,
                    None,
                )
                .await
                .unwrap();
            let draft = crate::ai::ProposalDraft {
                id: format!("p-starve-{i}"),
                ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                    task_id: t.id.clone(),
                    direction_id: crate::core::TRIAGE_DIRECTION_ID.to_string(),
                }],
                rationale: None,
            };
            crate::ai::AiSink::submit(
                &store,
                Capability::Classify,
                draft,
                call_rec(&format!("c-starve-{i}")),
            )
            .await
            .unwrap();
            store
                .apply_proposal(&format!("p-starve-{i}"))
                .await
                .unwrap();
            crate::store::test_hooks::set_task_triage_entered_at(
                &store,
                &t.id,
                "2020-01-01T00:00:00Z",
            )
            .await
            .unwrap();
            triage_ids.push(t.id);
        }
        let _new_direction = store
            .create_direction("Freshly created", "2026-Q4", None)
            .await
            .unwrap();
        // PR#69 review round 1: pinned safely between the 2020 entered_at
        // floor and `run_classify`'s own `run_started_at` (real wall-clock
        // "now", captured a moment after this) — otherwise the gate's
        // widened `>=` (a same-second tie must still count as "seen", so a
        // Direction born mid-batch is not masked) would ALSO count THIS
        // Direction and run 1's `run_started_at` as tied if both land in the
        // same wall-clock second, keeping the gate open after run 1's eval
        // write and defeating this test's own premise.
        crate::store::test_hooks::set_direction_created_at(
            &store,
            &_new_direction.id,
            "2024-01-01T00:00:00Z",
        )
        .await
        .unwrap();
        let fresh_task = store
            .create_task(
                "Brand new task",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();

        let reader = store.ai_reader();

        // Run 1: the cap OLDEST 待定 tasks (created_at ASC) fill every slot;
        // the fresh task is younger than all of them, past the cap.
        let targets1 = crate::http::ai_classify::auto_select_targets(&store, &reader).await;
        assert_eq!(targets1.len(), cap);
        let target1_ids: std::collections::HashSet<_> =
            targets1.iter().map(|t| t.id.clone()).collect();
        assert!(
            !target1_ids.contains(&fresh_task.id),
            "run 1 must not reach the fresh task yet: {target1_ids:?}"
        );

        let _outcomes = crate::ai::classify::run_classify(
            "run-starve-1",
            &targets1,
            crate::ai::ModelAccess::LocalOnly,
            Some(&ExplicitNoneModel),
            &store,
            &reader,
        )
        .await;

        // Run 2: the 20 just-evaluated 待定 tasks are no longer gate-eligible
        // (H2) — the fresh task is now reachable.
        let targets2 = crate::http::ai_classify::auto_select_targets(&store, &reader).await;
        let target2_ids: std::collections::HashSet<_> =
            targets2.iter().map(|t| t.id.clone()).collect();
        assert!(
            target2_ids.contains(&fresh_task.id),
            "H2: the fresh task must no longer be starved once the stale 待定 retries \
             drop out of the gate: {target2_ids:?}"
        );
    }

    // ---- T5.7.2 review round 2 (M6): 待定→真实 Direction only for --------
    // ---- classify's OWN placements, never a human's direct one ------------

    /// M6 positive control: `suppress_triage_reassign_to_real_direction_ok_
    /// triage_to_triage_still_not_in_inbox` above already pins the
    /// classify-placed case (`AiSink::submit` records the `sin90_ai_calls`
    /// row `task_triage_via_classify` relies on) — nothing new needed here.
    ///
    /// M6 negative control: a task a human/automation client files DIRECTLY
    /// into 待定 (`POST /proposals`, no `AiSink::submit` in the path at all —
    /// `capability_source` resolves to `"direct"`, same domain `reject_
    /// proposal` reports) must NOT be reclassifiable out from under them by
    /// a later AI run, and must never be offered to classify's retry inbox
    /// either — both halves share the SAME `task_triage_via_classify` check
    /// (`core::proposal::validate`'s A3, `AiReadModel::inbox`/`inbox_task`'s
    /// SQL gate).
    #[tokio::test]
    async fn suppress_triage_reassign_requires_classify_placement_not_a_manual_one() {
        let (_app, _sink, store) = test_app_with_store().await;
        let task = store
            .create_task(
                "Filed by hand",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();

        // A human/automation client submits AND accepts this DIRECTLY — no
        // `AiSink::submit` anywhere in this path, so no `sin90_ai_calls` row
        // ever links back to it (`capability_source = "direct"`).
        let manual = crate::core::Sin90Proposal {
            id: "p-manual-triage".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: crate::core::TRIAGE_DIRECTION_ID.to_string(),
            }],
            rationale: None,
        };
        store.submit_proposal(&manual).await.unwrap();
        store.apply_proposal(&manual.id).await.unwrap();
        crate::store::test_hooks::set_task_triage_entered_at(
            &store,
            &task.id,
            "2020-01-01T00:00:00Z",
        )
        .await
        .unwrap();

        let real = store
            .create_direction("Real home", "2026-Q4", None)
            .await
            .unwrap();

        // The retry inbox must never offer this task at all, even though a
        // new eligible Direction exists (the SAME condition that opens the
        // gate for a classify-placed task in the sibling test above).
        let reader = store.ai_reader();
        assert!(
            crate::ai::AiReadModel::inbox_task(&reader, &task.id)
                .await
                .unwrap()
                .is_none(),
            "a manually-filed 待定 task must never enter classify's retry inbox"
        );

        // And an explicit reclassify attempt is refused, same as any other
        // already-classified task (待定 or not) — A3's carve-out never
        // applies to it.
        let reclassify = crate::ai::ProposalDraft {
            id: "p-manual-reclassify".into(),
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: real.id.clone(),
            }],
            rationale: None,
        };
        let err = crate::ai::AiSink::submit(
            &store,
            Capability::Classify,
            reclassify,
            call_rec("c-manual-reclassify"),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, crate::ai::SinkError::Invalid(_)));
    }

    // ---- M-a (T5.7.2 review round 2 follow-up): a human/automation client
    // moving their OWN 待定 placement out is always allowed, unlike an AI ----

    /// The exact mirror of `suppress_triage_reassign_requires_classify_
    /// placement_not_a_manual_one` above — SAME fixture (a manually-filed
    /// 待定 task, `task_triage_via_classify` reports `false`) — but the
    /// reclassify attempt is submitted DIRECTLY (`POST /proposals`'s own
    /// path, `submit_proposal`/`apply_proposal`, `capability_source =
    /// "direct"`) instead of through `AiSink::submit`. Before M-a this was
    /// ALSO refused (M6's carve-out only ever checked `task_triage_via_
    /// classify`, blind to who the CURRENT proposal itself came from) —
    /// locking the user out of a task they filed into 待定 with their own
    /// hands (Q7 cuts both ways). Mutation target: drop the
    /// `capability_source == "direct" ||` leg `core::proposal::validate`'s
    /// A3 carve-out gained and this goes red (`Err` instead of `Ok`).
    #[tokio::test]
    async fn direct_source_can_move_its_own_manually_filed_triage_task_out() {
        let (_app, _sink, store) = test_app_with_store().await;
        let task = store
            .create_task(
                "Filed by hand",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();

        let manual = crate::core::Sin90Proposal {
            id: "p-ma-manual-triage".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: crate::core::TRIAGE_DIRECTION_ID.to_string(),
            }],
            rationale: None,
        };
        store.submit_proposal(&manual).await.unwrap();
        store.apply_proposal(&manual.id).await.unwrap();

        let real = store
            .create_direction("Real home", "2026-Q4", None)
            .await
            .unwrap();

        // The SAME human/automation client (a direct `POST /proposals`, not
        // `AiSink::submit`) moves their own task out — must succeed.
        let move_out = crate::core::Sin90Proposal {
            id: "p-ma-manual-move-out".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: real.id.clone(),
            }],
            rationale: None,
        };
        store.submit_proposal(&move_out).await.unwrap();
        store.apply_proposal(&move_out.id).await.unwrap();

        let refetched = store
            .list_tasks(None, None, None)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.id == task.id)
            .expect("task still exists");
        assert_eq!(
            refetched.direction_id.as_deref(),
            Some(real.id.as_str()),
            "a direct-sourced proposal must be able to move the user's OWN 待定 task out"
        );
    }

    /// Negative control for the test above, at the SAME `apply_proposal`
    /// layer that resolves `capability_source` dynamically: an AI
    /// (`AiSink::submit`, `capability_source = "classify"`) run must still
    /// be refused for the identical manually-filed fixture — M-a narrows the
    /// carve-out, it does not remove classify's own restriction. (Already
    /// exercised end-to-end by `suppress_triage_reassign_requires_classify_
    /// placement_not_a_manual_one` above; kept short here as the direct
    /// counterpart's own paired control.)
    #[tokio::test]
    async fn classify_source_still_refused_for_the_same_manually_filed_task() {
        let (_app, _sink, store) = test_app_with_store().await;
        let task = store
            .create_task(
                "Filed by hand",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();
        let manual = crate::core::Sin90Proposal {
            id: "p-ma-manual-triage-2".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: crate::core::TRIAGE_DIRECTION_ID.to_string(),
            }],
            rationale: None,
        };
        store.submit_proposal(&manual).await.unwrap();
        store.apply_proposal(&manual.id).await.unwrap();
        let real = store
            .create_direction("Real home", "2026-Q4", None)
            .await
            .unwrap();
        let reclassify = crate::ai::ProposalDraft {
            id: "p-ma-classify-reclassify".into(),
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: real.id.clone(),
            }],
            rationale: None,
        };
        let err = crate::ai::AiSink::submit(
            &store,
            Capability::Classify,
            reclassify,
            call_rec("c-ma-classify-reclassify"),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, crate::ai::SinkError::Invalid(_)));
    }

    // ---- N-H1 (T5.7.2 review round 2 follow-up): 待定 provenance survives --
    // ---- CarryOverTask minting a new task id -------------------------------

    /// The regression itself: a task classify fallback-parks into 待定, then
    /// gets carried into the next week (`CarryOverTask` mints a brand-new
    /// id, per-design — `direction_id` is copied, `triage_via`/
    /// `triage_entered_at` must be too). Before this fix, BOTH the H2 retry
    /// gate and M6's A3 carve-out were keyed off a JOIN/lookup that used the
    /// task's CURRENT id — under the carried-over id, neither ever matched
    /// anything again: the task could never re-enter the retry inbox no
    /// matter how many new Directions appeared, and an AI reclassify of it
    /// was refused exactly like a human's direct placement would be.
    /// Mutation target: drop the `triage_via`/`triage_entered_at` columns
    /// from `CarryOverTask`'s apply (leave them out of the new row's
    /// INSERT) and every assertion below goes red.
    #[tokio::test]
    async fn carry_over_task_preserves_classify_triage_provenance_for_retry() {
        let (_app, _sink, store) = test_app_with_store().await;
        let prev = store.create_week("2026-W20").await.unwrap();
        store
            .transition_week(&prev.id, crate::core::WeekStatus::Active)
            .await
            .unwrap();
        let seed = crate::core::Sin90Proposal {
            id: "seed-nh1-task".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![crate::core::Sin90Op::CreateTasks {
                week_id: prev.id.clone(),
                tasks: vec![crate::core::NewTask {
                    title: "Mystery task".into(),
                    direction_id: None,
                }],
            }],
            rationale: None,
        };
        store.submit_proposal(&seed).await.unwrap();
        store.apply_proposal(&seed.id).await.unwrap();
        let task = crate::ai::AiReadModel::week_tasks(&store.ai_reader(), &prev.id)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.title == "Mystery task")
            .expect("seeded task must exist");

        // Classify fallback-parks it into 待定.
        let draft = crate::ai::ProposalDraft {
            id: "p-nh1-triage".into(),
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: crate::core::TRIAGE_DIRECTION_ID.to_string(),
            }],
            rationale: None,
        };
        crate::ai::AiSink::submit(
            &store,
            Capability::Classify,
            draft,
            call_rec("c-nh1-triage"),
        )
        .await
        .unwrap();
        store.apply_proposal("p-nh1-triage").await.unwrap();
        crate::store::test_hooks::set_task_triage_entered_at(
            &store,
            &task.id,
            "2020-01-01T00:00:00Z",
        )
        .await
        .unwrap();
        let (via, entered_at) = crate::store::test_hooks::task_triage_state(&store, &task.id)
            .await
            .unwrap();
        assert_eq!(via.as_deref(), Some("classify"));
        assert_eq!(entered_at.as_deref(), Some("2020-01-01T00:00:00Z"));

        // Carry it into the next week — a brand-new task id.
        let target = store.create_week("2026-W21").await.unwrap();
        let carry = crate::core::Sin90Proposal {
            id: "p-nh1-carry".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![crate::core::Sin90Op::CarryOverTask {
                task_id: task.id.clone(),
                to_week: target.id.clone(),
            }],
            rationale: None,
        };
        store.submit_proposal(&carry).await.unwrap();
        store.apply_proposal(&carry.id).await.unwrap();
        let carried = crate::ai::AiReadModel::week_tasks(&store.ai_reader(), &target.id)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.carried_from.as_deref() == Some(task.id.as_str()))
            .expect("carried-over task must exist under a new id");
        assert_ne!(carried.id, task.id, "CarryOverTask must mint a new task id");

        // The new id must carry the SAME provenance forward.
        let (via, entered_at) = crate::store::test_hooks::task_triage_state(&store, &carried.id)
            .await
            .unwrap();
        assert_eq!(
            via.as_deref(),
            Some("classify"),
            "triage_via must survive the carry under the new id"
        );
        assert_eq!(
            entered_at.as_deref(),
            Some("2020-01-01T00:00:00Z"),
            "triage_entered_at must survive the carry under the new id"
        );

        let reader = store.ai_reader();

        // Negative control: no new eligible Direction yet — still not
        // retry-eligible under the new id.
        assert!(
            crate::ai::AiReadModel::inbox_task(&reader, &carried.id)
                .await
                .unwrap()
                .is_none(),
            "no new Direction since the carry — must not retry yet"
        );

        // A new Direction appears — the carried task (under its NEW id) must
        // become retry-eligible again.
        let fresh = store
            .create_direction("Freshly created", "2026-Q4", None)
            .await
            .unwrap();
        crate::store::test_hooks::set_direction_created_at(
            &store,
            &fresh.id,
            "2099-01-01T00:00:00Z",
        )
        .await
        .unwrap();
        assert!(
            crate::ai::AiReadModel::inbox_task(&reader, &carried.id)
                .await
                .unwrap()
                .is_some(),
            "a new Direction must lift the carried task's retry suppression under its new id"
        );

        // And an AI reclassify of the CARRIED task to a real Direction must
        // still be allowed (M6's carve-out, unaffected by the carry).
        let reclassify = crate::ai::ProposalDraft {
            id: "p-nh1-reclassify".into(),
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: carried.id.clone(),
                direction_id: fresh.id.clone(),
            }],
            rationale: None,
        };
        crate::ai::AiSink::submit(
            &store,
            Capability::Classify,
            reclassify,
            call_rec("c-nh1-reclassify"),
        )
        .await
        .unwrap();
        store.apply_proposal("p-nh1-reclassify").await.unwrap();
        let refetched = crate::ai::AiReadModel::week_tasks(&store.ai_reader(), &target.id)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.id == carried.id)
            .expect("carried task still exists");
        assert_eq!(refetched.direction_id.as_deref(), Some(fresh.id.as_str()));
    }

    /// The protection side: a task a human files DIRECTLY into 待定, then
    /// carried into the next week, must remain protected from an AI
    /// reclassify under its NEW id too (M6 unaffected by the carry) — pins
    /// that `CarryOverTask` copies `triage_via = 'direct'` (not merely
    /// leaving both columns `NULL`, which would coincidentally block the AI
    /// move the same way but is not what the source row actually says: see
    /// `store::test_hooks::task_triage_state`'s own assertion below).
    #[tokio::test]
    async fn carry_over_task_preserves_direct_triage_protection_under_new_id() {
        let (_app, _sink, store) = test_app_with_store().await;
        let prev = store.create_week("2026-W22").await.unwrap();
        store
            .transition_week(&prev.id, crate::core::WeekStatus::Active)
            .await
            .unwrap();
        let seed = crate::core::Sin90Proposal {
            id: "seed-nh1-direct-task".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![crate::core::Sin90Op::CreateTasks {
                week_id: prev.id.clone(),
                tasks: vec![crate::core::NewTask {
                    title: "Filed by hand".into(),
                    direction_id: None,
                }],
            }],
            rationale: None,
        };
        store.submit_proposal(&seed).await.unwrap();
        store.apply_proposal(&seed.id).await.unwrap();
        let task = crate::ai::AiReadModel::week_tasks(&store.ai_reader(), &prev.id)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.title == "Filed by hand")
            .expect("seeded task must exist");

        let manual = crate::core::Sin90Proposal {
            id: "p-nh1-direct-triage".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: crate::core::TRIAGE_DIRECTION_ID.to_string(),
            }],
            rationale: None,
        };
        store.submit_proposal(&manual).await.unwrap();
        store.apply_proposal(&manual.id).await.unwrap();

        let target = store.create_week("2026-W23").await.unwrap();
        let carry = crate::core::Sin90Proposal {
            id: "p-nh1-direct-carry".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![crate::core::Sin90Op::CarryOverTask {
                task_id: task.id.clone(),
                to_week: target.id.clone(),
            }],
            rationale: None,
        };
        store.submit_proposal(&carry).await.unwrap();
        store.apply_proposal(&carry.id).await.unwrap();
        let carried = crate::ai::AiReadModel::week_tasks(&store.ai_reader(), &target.id)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.carried_from.as_deref() == Some(task.id.as_str()))
            .expect("carried-over task must exist under a new id");

        let (via, _) = crate::store::test_hooks::task_triage_state(&store, &carried.id)
            .await
            .unwrap();
        assert_eq!(
            via.as_deref(),
            Some("direct"),
            "a direct placement's triage_via must survive the carry as 'direct', not NULL"
        );

        let real = store
            .create_direction("Real home", "2026-Q4", None)
            .await
            .unwrap();
        // L5 (T5.7.2 review round 3): force this Direction unambiguously
        // NEWER than `carried`'s `triage_entered_at` — without this, `real`'s
        // own `created_at` (recorded at ordinary test wall-clock time, the
        // same second `triage_entered_at` itself was stamped at) may not
        // actually be newer, so the `inbox_task` check below could pass for
        // the WRONG reason (H2's "no fresh Direction" staleness gate alone,
        // never reaching the `triage_via = 'classify'` filter this test
        // exists to pin) instead of the reason the test's own name and doc
        // claim. Mutation target: delete `triage_via = 'classify'` from
        // `store/ai_port.rs`'s `triage_retry_gate_sql`/`inbox`/`inbox_task`
        // and, WITH this line in place, the assertion below goes red.
        crate::store::test_hooks::set_direction_created_at(
            &store,
            &real.id,
            "2099-01-01T00:00:00Z",
        )
        .await
        .unwrap();
        let reader = store.ai_reader();
        assert!(
            crate::ai::AiReadModel::inbox_task(&reader, &carried.id)
                .await
                .unwrap()
                .is_none(),
            "a manually-filed 待定 task must never enter classify's retry inbox, even after a carry"
        );
        let reclassify = crate::ai::ProposalDraft {
            id: "p-nh1-direct-reclassify".into(),
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: carried.id.clone(),
                direction_id: real.id.clone(),
            }],
            rationale: None,
        };
        let err = crate::ai::AiSink::submit(
            &store,
            Capability::Classify,
            reclassify,
            call_rec("c-nh1-direct-reclassify"),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, crate::ai::SinkError::Invalid(_)));
    }

    /// L3 (T5.7.2 review round 3): `POST /tasks` (`Sin90Store::create_task`)
    /// is a path INTO 待定 that never goes through `AssignTaskDirection` at
    /// all — a task filed straight in with `direction_id = "sin90-triage"`
    /// must still stamp `triage_via = 'direct'` and a `triage_entered_at`,
    /// or it would sit at `direction_id = 'sin90-triage'` with BOTH columns
    /// `NULL`, contradicting migration 0015's own "`NULL` = never
    /// 待定-parked" invariant for a task manifestly parked there right now.
    /// Mutation target: drop the `triage_via`/`triage_entered_at` binds from
    /// `create_task`'s INSERT and the first assertion below goes red.
    #[tokio::test]
    async fn create_task_directly_into_triage_stamps_direct_provenance() {
        let (_app, _sink, store) = test_app_with_store().await;
        let task = store
            .create_task(
                "Filed straight into 待定",
                Some(crate::core::TRIAGE_DIRECTION_ID),
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();
        let (via, entered_at) = crate::store::test_hooks::task_triage_state(&store, &task.id)
            .await
            .unwrap();
        assert_eq!(
            via.as_deref(),
            Some("direct"),
            "a task filed straight into 待定 must backfill/stamp as direct-sourced, not NULL"
        );
        assert!(
            entered_at.is_some(),
            "a task filed straight into 待定 must get a triage_entered_at"
        );

        // Positive control: an ordinary Direction, and the inbox
        // (`direction_id = None`), get neither column — the common case
        // the migration's own invariant describes.
        let direction = store
            .create_direction("Real home", "2026-Q4", None)
            .await
            .unwrap();
        let real_task = store
            .create_task(
                "Ordinary task",
                Some(&direction.id),
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();
        let (via, entered_at) = crate::store::test_hooks::task_triage_state(&store, &real_task.id)
            .await
            .unwrap();
        assert_eq!(via, None);
        assert_eq!(entered_at, None);

        let inbox_task = store
            .create_task("Inbox task", None, None, TaskKind::Other, Energy::Mid, None)
            .await
            .unwrap();
        let (via, entered_at) = crate::store::test_hooks::task_triage_state(&store, &inbox_task.id)
            .await
            .unwrap();
        assert_eq!(via, None);
        assert_eq!(entered_at, None);
    }

    /// L3, the proposal-apply path: `Sin90Op::CreateTask`/`CreateTasks`
    /// (used by a human/automation `POST /proposals`, and by `propose`'s own
    /// AI path — though `propose` itself never targets 待定) get the SAME
    /// stamp when either targets 待定 directly. Mutation target: drop either
    /// arm's `triage_via`/`triage_entered_at` binds in `apply_op` and the
    /// matching assertion below goes red.
    #[tokio::test]
    async fn create_task_ops_directly_into_triage_stamp_direct_provenance() {
        let (_app, _sink, store) = test_app_with_store().await;

        let single = crate::core::Sin90Proposal {
            id: "p-l3-create-task".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![crate::core::Sin90Op::CreateTask {
                title: "Filed via CreateTask".into(),
                direction_id: Some(crate::core::TRIAGE_DIRECTION_ID.to_string()),
                parent_task_id: None,
                kind: None,
                energy: None,
                est_minutes: None,
            }],
            rationale: None,
        };
        store.submit_proposal(&single).await.unwrap();
        store.apply_proposal(&single.id).await.unwrap();
        let single_task = store
            .list_tasks(Some(crate::core::TRIAGE_DIRECTION_ID), None, None)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.title == "Filed via CreateTask")
            .expect("CreateTask must have created the task in 待定");
        let (via, entered_at) =
            crate::store::test_hooks::task_triage_state(&store, &single_task.id)
                .await
                .unwrap();
        assert_eq!(via.as_deref(), Some("direct"));
        assert!(entered_at.is_some());

        let week = store.create_week("2026-W24").await.unwrap();
        let batch = crate::core::Sin90Proposal {
            id: "p-l3-create-tasks".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![crate::core::Sin90Op::CreateTasks {
                week_id: week.id.clone(),
                tasks: vec![crate::core::NewTask {
                    title: "Filed via CreateTasks".into(),
                    direction_id: Some(crate::core::TRIAGE_DIRECTION_ID.to_string()),
                }],
            }],
            rationale: None,
        };
        store.submit_proposal(&batch).await.unwrap();
        store.apply_proposal(&batch.id).await.unwrap();
        let batch_task = store
            .list_tasks(Some(crate::core::TRIAGE_DIRECTION_ID), None, None)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.title == "Filed via CreateTasks")
            .expect("CreateTasks must have created the task in 待定");
        let (via, entered_at) = crate::store::test_hooks::task_triage_state(&store, &batch_task.id)
            .await
            .unwrap();
        assert_eq!(via.as_deref(), Some("direct"));
        assert!(entered_at.is_some());
    }

    /// L1 (T5.7.2 review round 3): `CarryOverTask`'s apply already copies
    /// `triage_via`/`triage_entered_at` (N-H1) to the new id — this pins the
    /// OTHER H2 floor, `sin90_classify_evals.evaluated_at`, gets the SAME
    /// treatment. Without it, a task classify had JUST re-evaluated (and
    /// found nothing new, advancing the retry gate's floor) loses that
    /// advance the instant it carries over: the OLD id's eval row is
    /// orphaned and the NEW id starts with none, silently re-opening the H2
    /// gate for something already resolved this cycle. Mutation target:
    /// remove the `INSERT INTO sin90_classify_evals ... SELECT` from
    /// `CarryOverTask`'s apply (`store/repo.rs`) and the assertion below
    /// goes red.
    #[tokio::test]
    async fn carry_over_task_copies_classify_eval_row_to_new_id() {
        let (_app, _sink, store) = test_app_with_store().await;
        let prev = store.create_week("2026-W22").await.unwrap();
        store
            .transition_week(&prev.id, crate::core::WeekStatus::Active)
            .await
            .unwrap();
        let seed = crate::core::Sin90Proposal {
            id: "seed-l1-task".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![crate::core::Sin90Op::CreateTasks {
                week_id: prev.id.clone(),
                tasks: vec![crate::core::NewTask {
                    title: "Ambiguous errand".into(),
                    direction_id: None,
                }],
            }],
            rationale: None,
        };
        store.submit_proposal(&seed).await.unwrap();
        store.apply_proposal(&seed.id).await.unwrap();
        let task = crate::ai::AiReadModel::week_tasks(&store.ai_reader(), &prev.id)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.title == "Ambiguous errand")
            .expect("seeded task must exist");

        // Classify fallback-parks it into 待定.
        let draft = crate::ai::ProposalDraft {
            id: "p-l1-triage".into(),
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: crate::core::TRIAGE_DIRECTION_ID.to_string(),
            }],
            rationale: None,
        };
        crate::ai::AiSink::submit(&store, Capability::Classify, draft, call_rec("c-l1-triage"))
            .await
            .unwrap();
        store.apply_proposal("p-l1-triage").await.unwrap();

        // Classify re-evaluates the SAME (now-in-待定) task and finds
        // nothing new again — writes a real `sin90_classify_evals` row
        // under the task's CURRENT (pre-carry) id (H2's own write path,
        // exercised directly rather than through a full ladder run).
        crate::ai::AiSink::record_classify_eval(&store, &task.id, &crate::core::now_iso8601())
            .await
            .unwrap();
        let old_evaluated_at: String =
            sqlx::query_scalar("SELECT evaluated_at FROM sin90_classify_evals WHERE task_id = ?")
                .bind(&task.id)
                .fetch_one(store.pool())
                .await
                .unwrap();

        let target = store.create_week("2026-W23").await.unwrap();
        let carry = crate::core::Sin90Proposal {
            id: "p-l1-carry".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![crate::core::Sin90Op::CarryOverTask {
                task_id: task.id.clone(),
                to_week: target.id.clone(),
            }],
            rationale: None,
        };
        store.submit_proposal(&carry).await.unwrap();
        store.apply_proposal(&carry.id).await.unwrap();
        let carried = crate::ai::AiReadModel::week_tasks(&store.ai_reader(), &target.id)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.carried_from.as_deref() == Some(task.id.as_str()))
            .expect("carried-over task must exist under a new id");

        let new_evaluated_at: Option<String> =
            sqlx::query_scalar("SELECT evaluated_at FROM sin90_classify_evals WHERE task_id = ?")
                .bind(&carried.id)
                .fetch_optional(store.pool())
                .await
                .unwrap();
        assert_eq!(
            new_evaluated_at,
            Some(old_evaluated_at),
            "CarryOverTask must copy the classify eval row to the carried task's new id"
        );
    }

    // ---- Low (T5.7.2 review round 2 follow-up): direction_assigned's own --
    // ---- event payload carries the REAL prior direction_id -----------------

    /// `AssignTaskDirection`'s apply used to hardcode the `direction_assigned`
    /// event's `from_direction_id` to `null` regardless of the task's actual
    /// prior state — wrong the moment M6's carve-out made a 待定 → real
    /// transition reachable (the prior value was `sin90-triage`, not `null`).
    /// Pins BOTH cases: inbox (`None`) → real, and 待定 (`Some(TRIAGE)`) →
    /// real. Mutation target: hardcode `"from_direction_id": null` back in
    /// `apply_op`'s `AssignTaskDirection` arm and this goes red.
    #[tokio::test]
    async fn direction_assigned_event_carries_real_from_direction_id() {
        let (_app, _sink, store) = test_app_with_store().await;
        let task = store
            .create_task("Some task", None, None, TaskKind::Other, Energy::Mid, None)
            .await
            .unwrap();
        let real = store
            .create_direction("Real home", "2026-Q4", None)
            .await
            .unwrap();

        // Case 1: inbox (NULL) → real.
        let assign = crate::core::Sin90Proposal {
            id: "p-low-from-direction-inbox".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: real.id.clone(),
            }],
            rationale: None,
        };
        store.submit_proposal(&assign).await.unwrap();
        store.apply_proposal(&assign.id).await.unwrap();
        let payload: String = sqlx::query_scalar(
            "SELECT payload FROM sin90_events
             WHERE entity = 'task' AND entity_id = ? AND kind = 'direction_assigned'
             ORDER BY seq DESC LIMIT 1",
        )
        .bind(&task.id)
        .fetch_one(store.pool())
        .await
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["from_direction_id"], serde_json::Value::Null);

        // Case 2: 待定 → real (M6's carve-out) — via a human/direct proposal,
        // which M-a now allows to move its own placement out.
        let task2 = store
            .create_task(
                "Another task",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();
        let to_triage = crate::core::Sin90Proposal {
            id: "p-low-from-direction-triage".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task2.id.clone(),
                direction_id: crate::core::TRIAGE_DIRECTION_ID.to_string(),
            }],
            rationale: None,
        };
        store.submit_proposal(&to_triage).await.unwrap();
        store.apply_proposal(&to_triage.id).await.unwrap();
        let real2 = store
            .create_direction("Real home 2", "2026-Q4", None)
            .await
            .unwrap();
        let out_of_triage = crate::core::Sin90Proposal {
            id: "p-low-from-direction-real".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task2.id.clone(),
                direction_id: real2.id.clone(),
            }],
            rationale: None,
        };
        store.submit_proposal(&out_of_triage).await.unwrap();
        store.apply_proposal(&out_of_triage.id).await.unwrap();
        let payload2: String = sqlx::query_scalar(
            "SELECT payload FROM sin90_events
             WHERE entity = 'task' AND entity_id = ? AND kind = 'direction_assigned'
             ORDER BY seq DESC LIMIT 1",
        )
        .bind(&task2.id)
        .fetch_one(store.pool())
        .await
        .unwrap();
        let parsed2: serde_json::Value = serde_json::from_str(&payload2).unwrap();
        assert_eq!(
            parsed2["from_direction_id"],
            serde_json::Value::String(crate::core::TRIAGE_DIRECTION_ID.to_string()),
            "from_direction_id must be the task's REAL prior Direction (待定), not null: {parsed2}"
        );
    }

    // ---- M4: a panicking run releases the single-flight slot --------------

    #[tokio::test]
    async fn busy_guard_releases_slot_on_panic() {
        use crate::http::ai_runs::{BusyGuard, RunRegistry};
        let runs: crate::http::ai_runs::SharedRunRegistry =
            std::sync::Arc::new(std::sync::Mutex::new(RunRegistry::default()));
        {
            let mut reg = runs.lock().unwrap();
            reg.start(Capability::Classify, "run-panicking");
        }
        let guard_runs = runs.clone();
        let handle = tokio::spawn(async move {
            let _guard = BusyGuard::new(guard_runs, Capability::Classify, "run-panicking".into());
            panic!("simulated background task failure");
        });
        let joined = handle.await;
        assert!(joined.is_err(), "the spawned task must have panicked");

        // The slot must be free — a NEW run can claim it immediately.
        let busy = runs.lock().unwrap().busy_run(Capability::Classify);
        assert_eq!(
            busy, None,
            "a panicking run must not leave the slot stuck busy forever"
        );
        let rec = runs.lock().unwrap().get("run-panicking");
        assert_eq!(rec.map(|r| r.state), Some("aborted"));
    }

    // ---- M3 round 2: auto-select paging + explicit-id dedup regressions --

    /// M3(a) (2026-09-24 review, round 2): with the OLDEST 20 inbox tasks
    /// all already covered by a valid pending proposal, auto-select
    /// (`task_ids` omitted) must keep paging past them to find the 21st,
    /// untouched task — not silently return a short/empty target list.
    /// Mutation target: collapse `auto_select_targets` back to a single
    /// `inbox(MAX_CLASSIFY_TASK_IDS)` fetch (the pre-fix shape) and this
    /// goes red (the 21st task is never found).
    #[tokio::test]
    async fn auto_select_pages_past_a_fully_blocked_first_page() {
        let (_app, _sink, store) = test_app_with_store().await;
        let direction = store
            .create_direction("Work", "2026-Q4", None)
            .await
            .unwrap();

        let mut blocked_ids = Vec::new();
        for i in 0..20 {
            let t = store
                .create_task(
                    &format!("blocked {i}"),
                    None,
                    None,
                    TaskKind::Other,
                    Energy::Mid,
                    None,
                )
                .await
                .unwrap();
            let draft = crate::ai::ProposalDraft {
                id: format!("p-block-{i}"),
                ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                    task_id: t.id.clone(),
                    direction_id: direction.id.clone(),
                }],
                rationale: None,
            };
            crate::ai::AiSink::submit(
                &store,
                Capability::Classify,
                draft,
                call_rec(&format!("call-block-{i}")),
            )
            .await
            .unwrap();
            blocked_ids.push(t.id);
        }
        let open_task = store
            .create_task("still open", None, None, TaskKind::Other, Energy::Mid, None)
            .await
            .unwrap();

        let reader = store.ai_reader();
        let targets = crate::http::ai_classify::auto_select_targets(&store, &reader).await;
        let target_ids: Vec<String> = targets.iter().map(|t| t.id.clone()).collect();
        assert!(
            target_ids.contains(&open_task.id),
            "the untouched 21st task must be found: {target_ids:?}"
        );
        for blocked in &blocked_ids {
            assert!(
                !target_ids.contains(blocked),
                "a blocked task must not be selected: {blocked}"
            );
        }
    }

    /// M3(b) (2026-09-24 review, round 2): an EXPLICITLY given `task_ids`
    /// entry that already has a valid pending proposal is deduped just like
    /// the auto-select path — the run's `items` report it `"skipped"`, not
    /// silently dropped or reprocessed. Mutation target: skip the
    /// `dedup_targets` call for the explicit-ids path and this item's result
    /// flips to `"nothing"`/`"proposed"` instead of `"skipped"`.
    #[tokio::test]
    async fn trigger_classify_explicit_task_id_already_pending_shows_skipped() {
        let (app, _sink, store) = test_app_with_store().await;
        let direction = store
            .create_direction("Work", "2026-Q4", None)
            .await
            .unwrap();
        let task = store
            .create_task(
                "Ambiguous task",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();
        let draft = crate::ai::ProposalDraft {
            id: "p-explicit-skip".into(),
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: direction.id.clone(),
            }],
            rationale: None,
        };
        crate::ai::AiSink::submit(
            &store,
            Capability::Classify,
            draft,
            call_rec("call-explicit-skip"),
        )
        .await
        .unwrap();

        let resp = app
            .clone()
            .oneshot(automation_req(
                "POST",
                "/ai/classify",
                json!({"task_ids": [task.id]}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let run_id = body_json(resp).await["run_id"]
            .as_str()
            .unwrap()
            .to_string();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut state_str = "running".to_string();
        let mut items = Value::Null;
        while tokio::time::Instant::now() < deadline {
            let r = body_json(
                app.clone()
                    .oneshot(get_req(&format!("/ai/runs/{run_id}")))
                    .await
                    .unwrap(),
            )
            .await;
            state_str = r["state"].as_str().unwrap().to_string();
            items = r["items"].clone();
            if state_str != "running" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(state_str, "done", "run never finished: items={items:?}");
        let items = items.as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["target"], task.id);
        assert_eq!(items[0]["result"], "skipped");
        // M2/M1 (2026-09-26 review round 2): dedup-skip carries `reason:
        // "dedup"` — mirrors `ai_propose`'s own dedup-skip assertion.
        assert_eq!(items[0]["reason"], "dedup");
    }

    // ---- run registry eviction must not drop a still-running entry -------

    /// (2026-09-24 review, round 2, low): eviction must skip over an entry
    /// whose `state` is still `"running"` — naive FIFO would evict the
    /// OLDEST entry regardless, which here is the still-running one.
    /// Mutation target: revert `evict_one_non_running` to a plain
    /// `order.pop_front()` and this goes red (`run-still-going` disappears).
    #[test]
    fn run_registry_eviction_skips_still_running_entries() {
        use crate::http::ai_runs::RunRegistry;
        let mut reg = RunRegistry::default();
        reg.start(Capability::Classify, "run-still-going"); // oldest, never finished
        for i in 0..63 {
            let id = format!("run-done-{i}");
            reg.start(Capability::Classify, &id);
            reg.finish(Capability::Classify, &id, "done", Vec::new());
        }
        // 64 entries total (== MAX_TRACKED_RUNS), "run-still-going" is the
        // OLDEST by insertion order. One more `start` forces an eviction.
        reg.start(Capability::Classify, "run-final");
        assert!(
            reg.get("run-still-going").is_some(),
            "the oldest entry, still running, must survive eviction"
        );
        assert!(
            reg.get("run-done-0").is_none(),
            "the oldest DONE entry should have been evicted instead"
        );
    }

    // ---- J11 negative control: automation key cannot accept an AI proposal

    /// J11 negative control (2026-09-24 review, round 2): an AI-produced
    /// `AssignTaskDirection` proposal is exactly as automation-proof as any
    /// other proposal (design §7.1's actor-key gate, already generically
    /// pinned by `automation_key_cannot_write_directly_but_can_submit_a_
    /// proposal` for a hand-built `CreateArea` proposal) — accepting one
    /// classify itself produced still requires the human key, and a
    /// rejected accept leaves the task in the inbox. Positive control (human
    /// key succeeds) already lives in `ai::classify::classify_stub_
    /// proposes_and_data_unchanged`.
    #[tokio::test]
    async fn ai_produced_proposal_cannot_be_accepted_by_automation_key() {
        let (app, _sink, store) = test_app_with_store().await;
        store
            .create_direction("Marketing Launch", "2026-Q4", None)
            .await
            .unwrap();
        let task = store
            .create_task(
                "Marketing Launch checklist",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();

        let resp = app
            .clone()
            .oneshot(automation_req("POST", "/ai/classify", json!({})))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let run_id = body_json(resp).await["run_id"]
            .as_str()
            .unwrap()
            .to_string();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut state_str = "running".to_string();
        while tokio::time::Instant::now() < deadline && state_str == "running" {
            let r = body_json(
                app.clone()
                    .oneshot(get_req(&format!("/ai/runs/{run_id}")))
                    .await
                    .unwrap(),
            )
            .await;
            state_str = r["state"].as_str().unwrap().to_string();
            if state_str == "running" {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }
        assert_eq!(state_str, "done");

        let proposals = store.list_proposals().await.unwrap();
        let proposal = proposals
            .iter()
            .find(|p| {
                matches!(
                    p.ops.as_slice(),
                    [crate::core::Sin90Op::AssignTaskDirection { task_id, .. }] if *task_id == task.id
                )
            })
            .expect("classify must have produced exactly one AssignTaskDirection proposal");

        let resp = app
            .clone()
            .oneshot(automation_req(
                "POST",
                &format!("/proposals/{}/accept", proposal.id),
                Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // The task must still be in the inbox — the rejected accept applied nothing.
        let today = body_json(app.oneshot(get_req("/today")).await.unwrap()).await;
        let inbox_ids: Vec<&str> = today["inbox"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap())
            .collect();
        assert!(inbox_ids.contains(&task.id.as_str()));
    }

    // ---- T5.2.3 review M1: items[].reason on the real wire -----------------

    /// A `ModelPort` that always answers with a real candidate key and
    /// `confidence: "low"` — standing in for a real T5.1.2 adapter, which
    /// `trigger_classify` doesn't have yet (it hardcodes `model: None`
    /// below), so there is no way to drive this outcome through an actual
    /// `POST /ai/classify` round trip today.
    struct LowConfidenceModel;
    impl crate::ai::ModelPort for LowConfidenceModel {
        async fn complete(
            &self,
            _req: crate::ai::ModelRequest,
        ) -> Result<crate::ai::ModelReply, crate::ai::ModelFailure> {
            Ok(crate::ai::ModelReply {
                text: r#"{"choice":"d1","confidence":"low","reason":"not sure"}"#.to_string(),
                model_id: Some("test-model".into()),
                tier: crate::ai::ServedTier::Local,
                prompt_tokens: None,
                completion_tokens: None,
            })
        }
    }

    /// Same shape, but the model explicitly says `"none"` (at HIGH
    /// confidence) — the negative control for the test below: this must NOT
    /// carry the `"low_confidence"` reason.
    struct ExplicitNoneModel;
    impl crate::ai::ModelPort for ExplicitNoneModel {
        async fn complete(
            &self,
            _req: crate::ai::ModelRequest,
        ) -> Result<crate::ai::ModelReply, crate::ai::ModelFailure> {
            Ok(crate::ai::ModelReply {
                text: r#"{"choice":"none","confidence":"high","reason":"nothing fits"}"#
                    .to_string(),
                model_id: Some("test-model".into()),
                tier: crate::ai::ServedTier::Local,
                prompt_tokens: None,
                completion_tokens: None,
            })
        }
    }

    /// T5.2.3 review (M1): `AiRunItem.reason` never had a test exercising
    /// the actual wire shape `GET /ai/runs/{run_id}` returns. Since
    /// `trigger_classify` cannot be driven through a real `POST
    /// /ai/classify` with a stub model (T5.1.2 not wired — production
    /// hardcodes `model: None`), this runs `run_classify` directly against
    /// [`LowConfidenceModel`], feeds the result through the SAME production
    /// mapping `trigger_classify` itself calls
    /// (`crate::http::ai_classify::build_classify_run_items`), inserts it
    /// into the run registry the way `BusyGuard::finish` would, and reads it
    /// back through the real `GET /ai/runs/{run_id}` HTTP handler — so the
    /// serde wire shape (including `#[serde(skip_serializing_if)]`) is
    /// exercised for real, not just the Rust struct. Mutation target: change
    /// `build_classify_run_items`'s `reason: o.reason` to `reason: None` —
    /// this test goes red (`items[0]["reason"]` disappears).
    #[tokio::test]
    async fn get_ai_run_reports_low_confidence_reason_on_the_wire() {
        let store = Sin90Store::open_memory().await.unwrap();
        let sink = RecordingSink::default();
        let state = Sin90State::new(
            store.clone(),
            Arc::new(sink),
            ActorKeys {
                human: HUMAN.into(),
                automation: AUTOMATION.into(),
            },
        );
        let app = crate::http::router(state.clone(), false);

        let _direction = store
            .create_direction("Work", "2026-Q4", None)
            .await
            .unwrap();
        let task = store
            .create_task(
                "Something unrelated to any candidate",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();
        let reader = store.ai_reader();

        let outcomes = crate::ai::classify::run_classify(
            "run-wire-reason-low",
            std::slice::from_ref(&task),
            crate::ai::ModelAccess::LocalOnly,
            Some(&LowConfidenceModel),
            &store,
            &reader,
        )
        .await;
        let items = crate::http::ai_classify::build_classify_run_items(Vec::new(), outcomes);
        {
            let mut reg = state.ai_runs.lock().unwrap();
            reg.start(Capability::Classify, "run-wire-reason-low");
            reg.finish(Capability::Classify, "run-wire-reason-low", "done", items);
        }

        let body = body_json(
            app.oneshot(get_req("/ai/runs/run-wire-reason-low"))
                .await
                .unwrap(),
        )
        .await;
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["result"], "nothing");
        assert_eq!(items[0]["reason"], "low_confidence");
    }

    /// T5.2.3 review (M1) positive control, updated for T5.2.2: an explicit
    /// `choice == "none"` (at high confidence) is no longer `Nothing` at all
    /// — it falls back to the reserved 待定 Direction (`result: "proposed"`)
    /// — but must still carry NO `reason` key on the wire (not `null`, the
    /// key absent entirely, `#[serde(skip_serializing_if = "Option::is_
    /// none")]`), the same way a real match never carries one. Same harness
    /// as the test above, swapping in [`ExplicitNoneModel`].
    #[tokio::test]
    async fn get_ai_run_has_no_reason_key_for_explicit_none() {
        let store = Sin90Store::open_memory().await.unwrap();
        let sink = RecordingSink::default();
        let state = Sin90State::new(
            store.clone(),
            Arc::new(sink),
            ActorKeys {
                human: HUMAN.into(),
                automation: AUTOMATION.into(),
            },
        );
        let app = crate::http::router(state.clone(), false);

        let _direction = store
            .create_direction("Work", "2026-Q4", None)
            .await
            .unwrap();
        let task = store
            .create_task(
                "Something unrelated to any candidate",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();
        let reader = store.ai_reader();

        let outcomes = crate::ai::classify::run_classify(
            "run-wire-reason-none",
            std::slice::from_ref(&task),
            crate::ai::ModelAccess::LocalOnly,
            Some(&ExplicitNoneModel),
            &store,
            &reader,
        )
        .await;
        let items = crate::http::ai_classify::build_classify_run_items(Vec::new(), outcomes);
        {
            let mut reg = state.ai_runs.lock().unwrap();
            reg.start(Capability::Classify, "run-wire-reason-none");
            reg.finish(Capability::Classify, "run-wire-reason-none", "done", items);
        }

        let body = body_json(
            app.oneshot(get_req("/ai/runs/run-wire-reason-none"))
                .await
                .unwrap(),
        )
        .await;
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["result"], "proposed");
        assert!(
            !items[0].as_object().unwrap().contains_key("reason"),
            "an explicit choice==\"none\" 待定 fallback must not carry a reason key at all: {items:?}"
        );
    }

    /// A `ModelPort`/`ModelCaller` whose `complete` never resolves — stands
    /// in for a genuinely hung `_a24/model/complete` call (or the kernel
    /// simply vanishing mid-call) so `with_hard_deadline` (`ai_runs`) is the
    /// ONLY thing that can ever end the run.
    struct HangingModelCaller;
    impl crate::http::ModelCaller for HangingModelCaller {
        fn complete<'a>(
            &'a self,
            _req: crate::ai::ModelRequest,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<crate::ai::ModelReply, crate::ai::ModelFailure>,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(std::future::pending())
        }
    }

    /// M-2 (2026-09-26 review round 2): `trigger_classify`'s `Err(_elapsed)`
    /// branch, exercised end-to-end through the REAL router — `ai_runs`'s
    /// own `with_hard_deadline_*` tests only prove the wrapping mechanism in
    /// isolation, never a route that actually reaches it. Injects a
    /// millisecond-scale [`Sin90State::run_hard_deadline`] and a
    /// [`HangingModelCaller`] (so the ladder's `Step::Model(Local)` call
    /// never returns on its own) and asserts all three things the round-2
    /// review asked for: `GET /ai/runs/{id}` reports `aborted`, the
    /// single-flight slot is released, and an immediate second trigger gets
    /// `202`, not `409`.
    ///
    /// Real (unpaused) time, not `tokio::time::pause` — tried first and
    /// abandoned: mixing a paused/auto-advancing clock with a REAL
    /// background `tokio::spawn` task plus real (if in-memory) SQLite work
    /// reproduced the exact same unpredictable-overshoot behavior
    /// `adapter_agent24::clients::model`'s own L2 timeout test had to work
    /// around for a raw socket — not worth re-fighting here when a small
    /// REAL deadline (the same choice `trigger_classify_runs_in_background_
    /// and_is_pollable_to_done` above already makes for this same shape of
    /// test) is both simpler and just as deterministic in practice.
    #[tokio::test]
    async fn trigger_classify_hard_deadline_aborts_releases_slot_and_allows_a_second_trigger() {
        let store = Sin90Store::open_memory().await.unwrap();
        // `classify_one` short-circuits straight to `Nothing` (no model
        // call at all) when there are zero Direction candidates — need at
        // least one so the ladder actually reaches `Step::Model(Local)`.
        store
            .create_direction("Unrelated Direction", "2026-Q4", None)
            .await
            .unwrap();
        store
            .create_task(
                "a brand new task with no classification history",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();
        let mut state = Sin90State::new(
            store,
            Arc::new(RecordingSink::default()),
            crate::http::ActorKeys {
                human: HUMAN.into(),
                automation: AUTOMATION.into(),
            },
        );
        // A model IS wired (so `plan()` schedules `Step::Model(Local)`
        // regardless of `ModelAccess` — `ai::ladder::plan`'s own doc), but
        // it never answers — the run can only ever end via the hard
        // deadline below.
        state.model = Some(Arc::new(HangingModelCaller));
        state.run_hard_deadline = std::time::Duration::from_millis(50);
        let app = router(state.clone(), false);

        let resp = app
            .clone()
            .oneshot(automation_req("POST", "/ai/classify", json!({})))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let run_id = body_json(resp).await["run_id"]
            .as_str()
            .unwrap()
            .to_string();

        // Real, small wall-clock ceiling (same style as `trigger_classify_
        // runs_in_background_and_is_pollable_to_done` above): the 50ms hard
        // deadline should end this in well under a second, but a 5s ceiling
        // means a slow CI box doesn't turn a real pass into a flaky failure.
        let poll_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut state_str = "running".to_string();
        while tokio::time::Instant::now() < poll_deadline {
            let r = body_json(
                app.clone()
                    .oneshot(get_req(&format!("/ai/runs/{run_id}")))
                    .await
                    .unwrap(),
            )
            .await;
            state_str = r["state"].as_str().unwrap().to_string();
            if state_str != "running" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(
            state_str, "aborted",
            "a run cancelled by the hard deadline must report aborted, not done/running"
        );

        // Single-flight slot released.
        assert!(
            state
                .ai_runs
                .lock()
                .unwrap()
                .busy_run(Capability::Classify)
                .is_none(),
            "the single-flight slot must be released once the hard deadline cancels the run"
        );

        // An immediate second trigger must be accepted, not bounced with 409.
        let resp2 = app
            .oneshot(automation_req("POST", "/ai/classify", json!({})))
            .await
            .unwrap();
        assert_eq!(
            resp2.status(),
            StatusCode::ACCEPTED,
            "the slot must be free for a brand new run right after the aborted one"
        );
    }
}

// ---- POST /ai/propose (T5.4.1, design §11.4 公共 + §11.4.3) ----------------

mod ai_propose {
    use super::*;
    use crate::ai::Capability;
    use crate::core::{NewTask, Sin90Op, WeekStatus};

    /// Polls `GET /ai/runs/{run_id}` until `state != "running"` (5s wall-clock
    /// ceiling, same posture `ai_classify`'s own L6 fix uses) and returns the
    /// final `(state, items)`.
    async fn poll_run_to_done(app: &axum::Router, run_id: &str) -> (String, Value) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut state_str = "running".to_string();
        let mut items = Value::Null;
        while tokio::time::Instant::now() < deadline {
            let r = body_json(
                app.clone()
                    .oneshot(get_req(&format!("/ai/runs/{run_id}")))
                    .await
                    .unwrap(),
            )
            .await;
            state_str = r["state"].as_str().unwrap().to_string();
            items = r["items"].clone();
            if state_str != "running" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        (state_str, items)
    }

    fn item_result<'a>(items: &'a Value, target: &str) -> &'a str {
        items
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["target"] == target)
            .unwrap_or_else(|| panic!("no item for target {target}: {items:?}"))["result"]
            .as_str()
            .unwrap()
    }

    /// M1 (2026-09-26 review round 2): like [`item_result`] but hands back
    /// the WHOLE item (so a caller can also inspect `reason`).
    fn item_by_target<'a>(items: &'a Value, target: &str) -> &'a Value {
        items
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["target"] == target)
            .unwrap_or_else(|| panic!("no item for target {target}: {items:?}"))
    }

    #[tokio::test]
    async fn trigger_propose_requires_an_actor_key() {
        let (app, _sink) = test_app().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/ai/propose")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(json!({"week_id": "w1"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn trigger_propose_unknown_field_is_400() {
        let (app, _sink) = test_app().await;
        let resp = app
            .oneshot(automation_req("POST", "/ai/propose", json!({"oops": true})))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn trigger_propose_unknown_week_is_404() {
        let (app, _sink) = test_app().await;
        let resp = app
            .oneshot(automation_req(
                "POST",
                "/ai/propose",
                json!({"week_id": "does-not-exist"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// L4 (2026-09-24 review): the same v1 error envelope every other route
    /// uses (`{"error": {"code", "message"}}`), not a hand-rolled shape.
    #[tokio::test]
    async fn trigger_propose_week_not_open_is_409() {
        let (app, _sink, store) = test_app_with_store().await;
        let week = store.create_week("2026-W30").await.unwrap();
        store
            .transition_week(&week.id, WeekStatus::Active)
            .await
            .unwrap();
        store
            .transition_week(&week.id, WeekStatus::Reviewing)
            .await
            .unwrap();

        let resp = app
            .oneshot(automation_req(
                "POST",
                "/ai/propose",
                json!({"week_id": week.id}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], "week_not_open");

        // Positive control: `planning` is open — same request succeeds.
        let (app2, _sink2, store2) = test_app_with_store().await;
        let week2 = store2.create_week("2026-W31").await.unwrap();
        let resp2 = app2
            .oneshot(automation_req(
                "POST",
                "/ai/propose",
                json!({"week_id": week2.id}),
            ))
            .await
            .unwrap();
        assert_eq!(resp2.status(), StatusCode::ACCEPTED);
    }

    /// Pre-seeds the registry directly (rather than racing two real requests,
    /// which would be flaky against a background `tokio::spawn`) to pin the
    /// single-flight 409 shape — same technique `ai_classify`'s own test uses.
    #[tokio::test]
    async fn trigger_propose_busy_returns_409_with_existing_run_id() {
        let store = Sin90Store::open_memory().await.unwrap();
        let week = store.create_week("2026-W32").await.unwrap();
        let state = Sin90State::new(
            store,
            Arc::new(RecordingSink::default()),
            ActorKeys {
                human: HUMAN.into(),
                automation: AUTOMATION.into(),
            },
        );
        state
            .ai_runs
            .lock()
            .unwrap()
            .start(Capability::Propose, "run-already-going");
        let app = router(state, false);
        let resp = app
            .oneshot(automation_req(
                "POST",
                "/ai/propose",
                json!({"week_id": week.id}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = body_json(resp).await;
        assert_eq!(body["code"], "ai_busy");
        assert_eq!(body["run_id"], "run-already-going");
    }

    /// End-to-end through the real router: `202` → background run → polled
    /// to completion via `GET /ai/runs/{id}`. Production has no real
    /// `ModelPort` wired yet (T5.1.2), so this only exercises reflex — a
    /// previous OPEN week with a `planned` task makes the carry reflex
    /// deterministically produce a proposal, and the run mirrors
    /// `proposal.submitted` through the SAME `EventSink` (M2: `EmittingSink`,
    /// reused from `ai_classify`, not a second implementation) `POST
    /// /proposals` uses.
    #[tokio::test]
    async fn trigger_propose_runs_in_background_and_is_pollable_to_done() {
        let (app, sink, store) = test_app_with_store().await;
        let prev = store.create_week("2026-W40").await.unwrap();
        store
            .transition_week(&prev.id, WeekStatus::Active)
            .await
            .unwrap();
        let carry_proposal = crate::core::Sin90Proposal {
            id: "seed-prev-task".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![Sin90Op::CreateTasks {
                week_id: prev.id.clone(),
                tasks: vec![NewTask {
                    title: "carry me".into(),
                    direction_id: None,
                }],
            }],
            rationale: None,
        };
        store.submit_proposal(&carry_proposal).await.unwrap();
        store.apply_proposal(&carry_proposal.id).await.unwrap();
        let target = store.create_week("2026-W41").await.unwrap();

        let resp = app
            .clone()
            .oneshot(automation_req(
                "POST",
                "/ai/propose",
                json!({"week_id": target.id}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let body = body_json(resp).await;
        assert_eq!(body["capability"], "propose");
        let run_id = body["run_id"].as_str().unwrap().to_string();

        let (state_str, items) = poll_run_to_done(&app, &run_id).await;
        assert_eq!(state_str, "done", "run never finished: items={items:?}");
        assert_eq!(item_result(&items, "propose.carry"), "proposed");
        assert_eq!(item_result(&items, "propose.reorder"), "nothing");
        assert_eq!(item_result(&items, "propose.create"), "nothing");
        // M1 (2026-09-26 review round 2): a non-skipped item carries no
        // `reason` field at all.
        assert!(
            item_by_target(&items, "propose.carry")
                .get("reason")
                .is_none(),
            "a non-skipped item must not have a reason field: {:?}",
            item_by_target(&items, "propose.carry")
        );

        let submitted: Vec<_> = sink
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|(kind, _)| kind == "proposal.submitted")
            .cloned()
            .collect();
        assert_eq!(
            submitted.len(),
            1,
            "one mirror per produced proposal: {submitted:?}"
        );
    }

    /// H3 (2026-09-24 review, design §11.4 公共's "去重"): triggering propose
    /// TWICE for the same week without accepting anything in between must
    /// produce only ONE pending carry proposal, not two — the second run's
    /// candidate is excluded because a still-valid pending carry proposal
    /// already covers it. Accepting that proposal (closing the source task)
    /// and adding a SECOND, distinct carryable task then lets a THIRD run
    /// produce a fresh proposal — dedup does not block permanently once the
    /// thing it was guarding against is no longer pending.
    #[tokio::test]
    async fn trigger_propose_dedup_skips_repeat_run_then_reproduces_after_accept() {
        let (app, _sink, store) = test_app_with_store().await;
        let prev = store.create_week("2026-W50").await.unwrap();
        store
            .transition_week(&prev.id, WeekStatus::Active)
            .await
            .unwrap();
        let seed_first = crate::core::Sin90Proposal {
            id: "seed-first-prev-task".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![Sin90Op::CreateTasks {
                week_id: prev.id.clone(),
                tasks: vec![NewTask {
                    title: "first carryable".into(),
                    direction_id: None,
                }],
            }],
            rationale: None,
        };
        store.submit_proposal(&seed_first).await.unwrap();
        store.apply_proposal(&seed_first.id).await.unwrap();
        let target = store.create_week("2026-W51").await.unwrap();

        // Round 1: produces a carry proposal.
        let run1 = body_json(
            app.clone()
                .oneshot(automation_req(
                    "POST",
                    "/ai/propose",
                    json!({"week_id": target.id}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let (state1, items1) = poll_run_to_done(&app, run1["run_id"].as_str().unwrap()).await;
        assert_eq!(state1, "done");
        assert_eq!(item_result(&items1, "propose.carry"), "proposed");
        let pending_after_round1 = store.list_pending_proposals().await.unwrap();
        assert_eq!(pending_after_round1.len(), 1, "{pending_after_round1:?}");

        // Round 2 (nothing accepted yet): the SAME candidate is already
        // covered by a still-valid pending carry proposal — excluded from
        // `p_candidates`, so this run's carry decision is empty.
        let run2 = body_json(
            app.clone()
                .oneshot(automation_req(
                    "POST",
                    "/ai/propose",
                    json!({"week_id": target.id}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let (state2, items2) = poll_run_to_done(&app, run2["run_id"].as_str().unwrap()).await;
        assert_eq!(state2, "done");
        assert_eq!(
            item_result(&items2, "propose.carry"),
            "nothing",
            "the only candidate is already covered by a still-pending proposal"
        );
        let pending_after_round2 = store.list_pending_proposals().await.unwrap();
        assert_eq!(
            pending_after_round2.len(),
            1,
            "round 2 must not have added a second carry proposal: {pending_after_round2:?}"
        );

        // Accept round 1's proposal, then add a SECOND, distinct carryable
        // task to P.
        let accept_resp = app
            .clone()
            .oneshot(accept_req(&pending_after_round1[0].id))
            .await
            .unwrap();
        assert_eq!(accept_resp.status(), StatusCode::OK);
        let seed_second = crate::core::Sin90Proposal {
            id: "seed-second-prev-task".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![Sin90Op::CreateTasks {
                week_id: prev.id.clone(),
                tasks: vec![NewTask {
                    title: "second carryable".into(),
                    direction_id: None,
                }],
            }],
            rationale: None,
        };
        store.submit_proposal(&seed_second).await.unwrap();
        store.apply_proposal(&seed_second.id).await.unwrap();

        // Round 3: a fresh candidate exists — dedup does not block it.
        let run3 = body_json(
            app.clone()
                .oneshot(automation_req(
                    "POST",
                    "/ai/propose",
                    json!({"week_id": target.id}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let (state3, items3) = poll_run_to_done(&app, run3["run_id"].as_str().unwrap()).await;
        assert_eq!(state3, "done");
        assert_eq!(item_result(&items3, "propose.carry"), "proposed");
    }

    /// T5.7.2 (design §2 #31), the `propose` half of `cargo test suppress_`:
    /// rejecting a pending `carry` proposal now SUPPRESSES it on the next
    /// `/ai/propose` run for the same week (unlike the pre-T5.7.2 behaviour,
    /// where a rejection immediately unblocked the same candidate task) —
    /// round 3 stays `nothing`. Only after a brand-new non-terminal
    /// Direction appears (§2 #31's "situation changed" leg propose's own
    /// suppression uses) does round 4 reproduce a proposal for the SAME
    /// candidate task (rejecting, unlike accepting, never carries the source
    /// task over, so the same target stays available throughout).
    #[tokio::test]
    async fn suppress_rejected_propose_carry_blocks_until_new_direction() {
        let (app, _sink, store) = test_app_with_store().await;
        let prev = store.create_week("2026-W50").await.unwrap();
        store
            .transition_week(&prev.id, WeekStatus::Active)
            .await
            .unwrap();
        let seed = crate::core::Sin90Proposal {
            id: "seed-reject-prev-task".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![Sin90Op::CreateTasks {
                week_id: prev.id.clone(),
                tasks: vec![NewTask {
                    title: "carryable".into(),
                    direction_id: None,
                }],
            }],
            rationale: None,
        };
        store.submit_proposal(&seed).await.unwrap();
        store.apply_proposal(&seed.id).await.unwrap();
        let target = store.create_week("2026-W51").await.unwrap();

        // Round 1: produces a carry proposal.
        let run1 = body_json(
            app.clone()
                .oneshot(automation_req(
                    "POST",
                    "/ai/propose",
                    json!({"week_id": target.id}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let (state1, items1) = poll_run_to_done(&app, run1["run_id"].as_str().unwrap()).await;
        assert_eq!(state1, "done");
        assert_eq!(item_result(&items1, "propose.carry"), "proposed");
        let pending_after_round1 = store.list_pending_proposals().await.unwrap();
        assert_eq!(pending_after_round1.len(), 1, "{pending_after_round1:?}");

        // Round 2 (nothing decided yet): still blocked by the still-valid pending proposal.
        let run2 = body_json(
            app.clone()
                .oneshot(automation_req(
                    "POST",
                    "/ai/propose",
                    json!({"week_id": target.id}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let (state2, items2) = poll_run_to_done(&app, run2["run_id"].as_str().unwrap()).await;
        assert_eq!(state2, "done");
        assert_eq!(item_result(&items2, "propose.carry"), "nothing");

        // Reject round 1's proposal instead of accepting it.
        let reject_resp = app
            .clone()
            .oneshot(super::reject_req(
                &pending_after_round1[0].id,
                super::HUMAN,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(reject_resp.status(), StatusCode::OK);
        let pending_after_reject = store.list_pending_proposals().await.unwrap();
        assert!(pending_after_reject.is_empty(), "{pending_after_reject:?}");

        // Round 3: the judgement — nothing has changed since the rejection
        // (no edit, no new Direction), so it stays suppressed.
        let run3 = body_json(
            app.clone()
                .oneshot(automation_req(
                    "POST",
                    "/ai/propose",
                    json!({"week_id": target.id}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let (state3, items3) = poll_run_to_done(&app, run3["run_id"].as_str().unwrap()).await;
        assert_eq!(state3, "done");
        assert_eq!(
            item_result(&items3, "propose.carry"),
            "nothing",
            "a rejected suggestion must stay suppressed when nothing has changed"
        );

        let rows =
            crate::store::test_hooks::proposal_rejection_rows(&store, &pending_after_round1[0].id)
                .await
                .unwrap();
        assert_eq!(rows[0].capability_source, "propose");

        // Positive control: a brand-new non-terminal Direction appears.
        // Forced forward past the rejection's own timestamp —
        // `now_iso8601()`'s second resolution means a same-second sequence
        // in this test would otherwise tie (same reasoning the classify-side
        // positive controls document).
        let fresh = store
            .create_direction("Freshly created", "2026-Q4", None)
            .await
            .unwrap();
        crate::store::test_hooks::set_direction_created_at(
            &store,
            &fresh.id,
            "2099-01-01T00:00:00Z",
        )
        .await
        .unwrap();

        let run4 = body_json(
            app.clone()
                .oneshot(automation_req(
                    "POST",
                    "/ai/propose",
                    json!({"week_id": target.id}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let (state4, items4) = poll_run_to_done(&app, run4["run_id"].as_str().unwrap()).await;
        assert_eq!(state4, "done");
        assert_eq!(
            item_result(&items4, "propose.carry"),
            "proposed",
            "a new non-terminal Direction must lift the suppression"
        );
    }

    /// M-c pin (propose's own M2): the `propose` counterpart of
    /// `suppress_rejected_classify_uses_proposed_at_not_rejected_at_as_
    /// situation_basis` above — `dedup_propose`'s rejected-half time basis
    /// must be the rejected proposal's OWN `proposed_at`, not `rejected_at`.
    /// Forces `proposed_at` safely into the past, creates a "gap" Direction
    /// at real "now", and only THEN rejects — the gap Direction's
    /// `created_at` is provably `<= rejected_at` (sequential real-time
    /// calls), never `>` it, so the OLD `rejected_at`-basis code would have
    /// called this "not new" and kept the carry suppressed. Calls
    /// `dedup_propose` DIRECTLY (same posture the create-per-direction tests
    /// above take). Mutation target: swap `r.proposed_at` for `r.rejected_at`
    /// in `dedup_propose`'s `situation_unchanged(&r.proposed_at)` call and
    /// this goes red (`excluded_carry_task_ids` stays non-empty).
    #[tokio::test]
    async fn suppress_rejected_propose_carry_uses_proposed_at_not_rejected_at_as_situation_basis() {
        let (_app, _sink, store) = test_app_with_store().await;
        let prev = store.create_week("2026-W49").await.unwrap();
        store
            .transition_week(&prev.id, crate::core::WeekStatus::Active)
            .await
            .unwrap();
        let seed = crate::core::Sin90Proposal {
            id: "seed-m2-carry-task".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![Sin90Op::CreateTasks {
                week_id: prev.id.clone(),
                tasks: vec![NewTask {
                    title: "carryable".into(),
                    direction_id: None,
                }],
            }],
            rationale: None,
        };
        store.submit_proposal(&seed).await.unwrap();
        store.apply_proposal(&seed.id).await.unwrap();
        let task_id: String = sqlx::query_scalar("SELECT id FROM sin90_tasks WHERE week_id = ?")
            .bind(&prev.id)
            .fetch_one(store.pool())
            .await
            .unwrap();
        let target = store.create_week("2026-W34").await.unwrap();

        let carry_draft = crate::ai::ProposalDraft {
            id: "p-m2-carry".into(),
            ops: vec![Sin90Op::CarryOverTask {
                task_id: task_id.clone(),
                to_week: target.id.clone(),
            }],
            rationale: None,
        };
        let rec = crate::ai::AiCallRecord {
            id: "call-m2-carry".into(),
            run_id: "run-m2-carry".into(),
            task_kind: Capability::Propose,
            engine: crate::ai::Engine::Reflex,
            fallback_from: None,
            served_tier: None,
            model_id: None,
            prompt_tokens: None,
            completion_tokens: None,
            latency_ms: 0,
            ok: true,
            error_kind: None,
            proposal_id: None,
            at: "2026-09-24T00:00:00Z".into(),
        };
        crate::ai::AiSink::submit(&store, Capability::Propose, carry_draft, rec)
            .await
            .unwrap();
        crate::store::test_hooks::set_proposal_created_at(
            &store,
            "p-m2-carry",
            "2020-01-01T00:00:00Z",
        )
        .await
        .unwrap();

        // A Direction appears WHILE the (still-pending) proposal is sitting
        // there, unrejected.
        let _gap_direction = store
            .create_direction("Created during the gap", "2026-Q4", None)
            .await
            .unwrap();

        // Only NOW does the human reject it — `rejected_at` lands at real
        // "now", provably at or after `gap_direction`'s `created_at`.
        store.reject_proposal("p-m2-carry", None).await.unwrap();

        let dedup = crate::http::ai_propose::dedup_propose(&store, &target)
            .await
            .unwrap();
        assert!(
            !dedup.excluded_carry_task_ids.contains(&task_id),
            "a Direction created between proposing and rejecting must already count as \
             \"new\" — the situation-changed basis is proposed_at, not rejected_at: {dedup:?}"
        );
    }

    /// T5.7.2 review round 2 (M4): reorder's own rejection-suppression,
    /// end to end through the real `/ai/propose` trigger — a FULL-COVERAGE
    /// rejected reorder suppresses round 2 (nothing changed); a task's
    /// status changing afterward (M3's own new leg) lifts it for round 3,
    /// even with no new Direction anywhere in sight.
    #[tokio::test]
    async fn suppress_rejected_propose_reorder_blocks_until_task_changes() {
        let (app, _sink, store) = test_app_with_store().await;
        let target = store.create_week("2026-W46").await.unwrap();
        let seed = crate::core::Sin90Proposal {
            id: "seed-reorder-tasks".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![Sin90Op::CreateTasks {
                week_id: target.id.clone(),
                tasks: vec![
                    NewTask {
                        title: "task a".into(),
                        direction_id: None,
                    },
                    NewTask {
                        title: "task b".into(),
                        direction_id: None,
                    },
                ],
            }],
            rationale: None,
        };
        store.submit_proposal(&seed).await.unwrap();
        store.apply_proposal(&seed.id).await.unwrap();
        let task_ids: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM sin90_tasks WHERE week_id = ? ORDER BY sort_key ASC",
        )
        .bind(&target.id)
        .fetch_all(store.pool())
        .await
        .unwrap();
        let task_a = task_ids[0].clone();
        let task_b = task_ids[1].clone();
        // Bump task B to `in_progress` so the reflex WANTS [b, a] — same
        // setup `trigger_propose_dedup_requires_exact_reorder_coverage`
        // above already establishes, so there is something for round 1 to
        // actually propose.
        let bump = crate::core::Sin90Proposal {
            id: "bump-task-b".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![Sin90Op::TransitionTask {
                task_id: task_b.clone(),
                to: crate::core::TaskStatus::InProgress,
            }],
            rationale: None,
        };
        store.submit_proposal(&bump).await.unwrap();
        store.apply_proposal(&bump.id).await.unwrap();

        // Round 1: produces the [b, a] reorder.
        let run1 = body_json(
            app.clone()
                .oneshot(automation_req(
                    "POST",
                    "/ai/propose",
                    json!({"week_id": target.id}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let (state1, items1) = poll_run_to_done(&app, run1["run_id"].as_str().unwrap()).await;
        assert_eq!(state1, "done");
        assert_eq!(item_result(&items1, "propose.reorder"), "proposed");
        let pending = store.list_pending_proposals().await.unwrap();
        let reorder_id = pending
            .iter()
            .find(|p| matches!(p.ops.as_slice(), [Sin90Op::ReorderTasks { .. }]))
            .expect("round 1 must have produced a reorder proposal")
            .id
            .clone();
        store.reject_proposal(&reorder_id, None).await.unwrap();

        // Round 2: nothing changed since the rejection — suppressed.
        let run2 = body_json(
            app.clone()
                .oneshot(automation_req(
                    "POST",
                    "/ai/propose",
                    json!({"week_id": target.id}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let (state2, items2) = poll_run_to_done(&app, run2["run_id"].as_str().unwrap()).await;
        assert_eq!(state2, "done");
        assert_eq!(
            item_result(&items2, "propose.reorder"),
            "skipped",
            "an unchanged rejected reorder must stay suppressed"
        );

        // M3: a task in the week changes status — no new Direction anywhere
        // — must still lift the suppression.
        store
            .transition_task(&task_a, crate::core::TaskStatus::InProgress)
            .await
            .unwrap();
        crate::store::test_hooks::set_task_transitioned_at(&store, &task_a, "2099-01-01T00:00:00Z")
            .await
            .unwrap();

        let run3 = body_json(
            app.clone()
                .oneshot(automation_req(
                    "POST",
                    "/ai/propose",
                    json!({"week_id": target.id}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let (state3, items3) = poll_run_to_done(&app, run3["run_id"].as_str().unwrap()).await;
        assert_eq!(state3, "done");
        assert_ne!(
            item_result(&items3, "propose.reorder"),
            "skipped",
            "a task status change in the week must lift the suppression even with \
             no new Direction: {items3:?}"
        );
    }

    /// 2026-09-26 external review (blocking): the OLD reorder-suppression
    /// judgement (task-set coverage + `task_modified_since`) could not see a
    /// `direction_assigned` reassignment change `reorder_reflex`'s OWN
    /// ranking — `reorder_reflex` (`ai::propose`) sorts by `status_tier`
    /// THEN by the task's Direction's rhythm-alloc `pct` (descending), but
    /// `task_modified_since` deliberately excludes `direction_assigned`
    /// events (that exclusion is correct for its OTHER caller; it just made
    /// this one blind). Reproduces the review's own repro: D1 `pct: 50`, D2
    /// `pct: 90`, task `b` starts under D1 — round 1's reflex order is
    /// `[b, a, c]` (a/c have no Direction, `pct` 0, tie broken by
    /// `created_at`); rejecting it and then reassigning `c` to the
    /// ALREADY-EXISTING D2 (never "new" relative to `proposed_at`, so the
    /// situation-changed gate above stays closed and cannot be what lifts
    /// this) must still lift the suppression, because recomputing
    /// `reorder_reflex` right now yields `[c, b, a]` — a different order
    /// than the one that was rejected. Goes through the real `/ai/propose`
    /// trigger (not `dedup_propose` directly) so the reproduced order itself
    /// — not just `skip_reorder`'s boolean — is pinned.
    #[tokio::test]
    async fn suppress_rejected_propose_reorder_lifts_on_direction_requota() {
        let (app, _sink, store) = test_app_with_store().await;
        let d1 = store.create_direction("D1", "2026-Q4", None).await.unwrap();
        let d2 = store.create_direction("D2", "2026-Q4", None).await.unwrap();
        crate::store::test_hooks::insert_rhythm(&store, "rhythm-d1-d2")
            .await
            .unwrap();
        sqlx::query("UPDATE sin90_rhythms SET allocations = ? WHERE id = ?")
            .bind(
                serde_json::to_string(&vec![
                    crate::core::Alloc {
                        direction_id: d1.id.clone(),
                        pct: 50,
                    },
                    crate::core::Alloc {
                        direction_id: d2.id.clone(),
                        pct: 90,
                    },
                ])
                .unwrap(),
            )
            .bind("rhythm-d1-d2")
            .execute(store.pool())
            .await
            .unwrap();
        let target = store.create_week("2026-W49").await.unwrap();
        let seed = crate::core::Sin90Proposal {
            id: "seed-reorder-tasks-requota".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![Sin90Op::CreateTasks {
                week_id: target.id.clone(),
                tasks: vec![
                    NewTask {
                        title: "task a".into(),
                        direction_id: None,
                    },
                    NewTask {
                        title: "task b".into(),
                        direction_id: Some(d1.id.clone()),
                    },
                    NewTask {
                        title: "task c".into(),
                        direction_id: None,
                    },
                ],
            }],
            rationale: None,
        };
        store.submit_proposal(&seed).await.unwrap();
        store.apply_proposal(&seed.id).await.unwrap();
        let task_ids: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM sin90_tasks WHERE week_id = ? ORDER BY sort_key ASC",
        )
        .bind(&target.id)
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert_eq!(task_ids.len(), 3);
        let task_a = task_ids[0].clone();
        let task_b = task_ids[1].clone();
        let task_c = task_ids[2].clone();

        // Round 1: b's D1 quota (50) outranks a/c's un-Directioned 0 —
        // reflex proposes [b, a, c].
        let run1 = body_json(
            app.clone()
                .oneshot(automation_req(
                    "POST",
                    "/ai/propose",
                    json!({"week_id": target.id}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let (state1, items1) = poll_run_to_done(&app, run1["run_id"].as_str().unwrap()).await;
        assert_eq!(state1, "done");
        assert_eq!(item_result(&items1, "propose.reorder"), "proposed");
        let pending1 = store.list_pending_proposals().await.unwrap();
        let reorder1 = pending1
            .iter()
            .find(|p| matches!(p.ops.as_slice(), [Sin90Op::ReorderTasks { .. }]))
            .expect("round 1 must have produced a reorder proposal");
        match reorder1.ops.as_slice() {
            [Sin90Op::ReorderTasks { order, .. }] => {
                assert_eq!(
                    order,
                    &vec![task_b.clone(), task_a.clone(), task_c.clone()],
                    "round 1 reflex order must rank b (D1 pct 50) first"
                );
            }
            _ => unreachable!(),
        }
        store.reject_proposal(&reorder1.id, None).await.unwrap();

        // Round 2: nothing changed since the rejection — negative control,
        // must stay suppressed.
        let run2 = body_json(
            app.clone()
                .oneshot(automation_req(
                    "POST",
                    "/ai/propose",
                    json!({"week_id": target.id}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let (state2, items2) = poll_run_to_done(&app, run2["run_id"].as_str().unwrap()).await;
        assert_eq!(state2, "done");
        assert_eq!(
            item_result(&items2, "propose.reorder"),
            "skipped",
            "an unchanged rejected reorder must stay suppressed"
        );

        // c gets reassigned to D2 — ALREADY-EXISTING (created before the
        // rejection), so it is never "new" relative to `proposed_at`; the
        // situation-changed (new-Direction) gate must stay closed, isolating
        // this to reorder's OWN recomputed-order check.
        let assign = crate::core::Sin90Proposal {
            id: "assign-task-c-to-d2".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![Sin90Op::AssignTaskDirection {
                task_id: task_c.clone(),
                direction_id: d2.id.clone(),
            }],
            rationale: None,
        };
        store.submit_proposal(&assign).await.unwrap();
        store.apply_proposal(&assign.id).await.unwrap();

        // Round 3: c's new D2 quota (90) now outranks everything — the
        // recomputed reflex order [c, b, a] differs from the rejected
        // [b, a, c], so the suppression must lift and reproduce it.
        let run3 = body_json(
            app.clone()
                .oneshot(automation_req(
                    "POST",
                    "/ai/propose",
                    json!({"week_id": target.id}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let (state3, items3) = poll_run_to_done(&app, run3["run_id"].as_str().unwrap()).await;
        assert_eq!(state3, "done");
        assert_eq!(
            item_result(&items3, "propose.reorder"),
            "proposed",
            "reassigning c to a Direction with a higher quota must lift the \
             suppression even though D2 itself is not new: {items3:?}"
        );
        let pending3 = store.list_pending_proposals().await.unwrap();
        let reorder3 = pending3
            .iter()
            .find(|p| {
                matches!(p.ops.as_slice(), [Sin90Op::ReorderTasks { .. }]) && p.id != reorder1.id
            })
            .expect("round 3 must have produced a NEW reorder proposal");
        match reorder3.ops.as_slice() {
            [Sin90Op::ReorderTasks { order, .. }] => {
                assert_eq!(
                    order,
                    &vec![task_c.clone(), task_b.clone(), task_a.clone()],
                    "round 3 reflex order must re-rank c (now D2 pct 90) first"
                );
            }
            _ => unreachable!(),
        }
    }

    /// T5.7.2 review round 2 (M4): create's rejection-suppression is
    /// PER-DIRECTION (M-b's own pending-side granularity, folded into the
    /// REJECTED half too) — a rejected create naming a gap Direction D
    /// excludes exactly D from being re-proposed, leaving an untouched gap
    /// C still open. Calls `dedup_propose` directly (create needs a model
    /// to actually run through the real ladder; this pins the dedup
    /// computation itself, same posture `dedup_propose_excludes_create_
    /// directions_per_item_not_all_or_nothing` above already takes).
    #[tokio::test]
    async fn suppress_rejected_propose_create_blocks_per_direction() {
        let (_app, _sink, store) = test_app_with_store().await;
        let direction_d = store.create_direction("D", "2026-Q4", None).await.unwrap();
        let direction_c = store.create_direction("C", "2026-Q4", None).await.unwrap();
        crate::store::test_hooks::insert_rhythm(&store, "rhythm-dc")
            .await
            .unwrap();
        sqlx::query("UPDATE sin90_rhythms SET allocations = ? WHERE id = ?")
            .bind(
                serde_json::to_string(&vec![
                    crate::core::Alloc {
                        direction_id: direction_d.id.clone(),
                        pct: 100,
                    },
                    crate::core::Alloc {
                        direction_id: direction_c.id.clone(),
                        pct: 100,
                    },
                ])
                .unwrap(),
            )
            .bind("rhythm-dc")
            .execute(store.pool())
            .await
            .unwrap();
        let target = store.create_week("2026-W47").await.unwrap();

        let rejected_create = crate::ai::ProposalDraft {
            id: "p-create-d".into(),
            ops: vec![Sin90Op::CreateTasks {
                week_id: target.id.clone(),
                tasks: vec![NewTask {
                    title: "new under D".into(),
                    direction_id: Some(direction_d.id.clone()),
                }],
            }],
            rationale: None,
        };
        let rec = crate::ai::AiCallRecord {
            id: "call-create-d".into(),
            run_id: "run-create-d".into(),
            task_kind: Capability::Propose,
            engine: crate::ai::Engine::Reflex,
            fallback_from: None,
            served_tier: None,
            model_id: None,
            prompt_tokens: None,
            completion_tokens: None,
            latency_ms: 0,
            ok: true,
            error_kind: None,
            proposal_id: None,
            at: "2026-09-24T00:00:00Z".into(),
        };
        crate::ai::AiSink::submit(&store, Capability::Propose, rejected_create, rec)
            .await
            .unwrap();
        store.reject_proposal("p-create-d", None).await.unwrap();

        let dedup = crate::http::ai_propose::dedup_propose(&store, &target)
            .await
            .unwrap();
        assert!(
            dedup
                .excluded_create_direction_ids
                .contains(&direction_d.id),
            "D's rejected create must stay excluded while nothing changed: {dedup:?}"
        );
        assert!(
            !dedup
                .excluded_create_direction_ids
                .contains(&direction_c.id),
            "C was never mentioned by the rejection — must not be excluded: {dedup:?}"
        );
        assert!(
            !dedup.skip_create,
            "C is still a genuinely uncovered gap — must NOT blanket-skip create: {dedup:?}"
        );
    }

    /// H3's coordinator-clarified nuance (`docs/DESIGN-LIFEOS.md` §11.4.3,
    /// "T5.4.1 实现时补"): a PENDING `ReorderTasks` proposal that only covers
    /// PART of W's current non-terminal tasks must NOT be treated as "still
    /// valid" for dedup purposes, even though `AiSink::precheck`'s dry run
    /// alone would happily pass it (a partial reorder is a perfectly legal
    /// `Sin90Op` on its own — nothing in `validate`/`apply_op` requires full
    /// coverage). Submits such a partial reorder directly (bypassing
    /// `ai::propose` entirely, simulating a stale leftover from before a
    /// task was added/removed), then triggers propose and confirms it is NOT
    /// skipped — a full reorder is still attempted.
    #[tokio::test]
    async fn trigger_propose_dedup_requires_exact_reorder_coverage() {
        let (app, _sink, store) = test_app_with_store().await;
        let target = store.create_week("2026-W22").await.unwrap();
        let seed = crate::core::Sin90Proposal {
            id: "seed-w-tasks".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![Sin90Op::CreateTasks {
                week_id: target.id.clone(),
                tasks: vec![
                    NewTask {
                        title: "task a".into(),
                        direction_id: None,
                    },
                    NewTask {
                        title: "task b".into(),
                        direction_id: None,
                    },
                ],
            }],
            rationale: None,
        };
        store.submit_proposal(&seed).await.unwrap();
        store.apply_proposal(&seed.id).await.unwrap();
        let task_ids: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM sin90_tasks WHERE week_id = ? ORDER BY sort_key ASC",
        )
        .bind(&target.id)
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert_eq!(task_ids.len(), 2);
        let task_a = &task_ids[0];
        let task_b = &task_ids[1];
        // Bump task B to `in_progress` so the reflex WANTS a different order
        // ([b, a], tier 0 before tier 1) than the current sort_key order
        // ([a, b]) — otherwise there would be nothing to reorder regardless
        // of dedup.
        let bump = crate::core::Sin90Proposal {
            id: "bump-task-b".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![Sin90Op::TransitionTask {
                task_id: task_b.clone(),
                to: crate::core::TaskStatus::InProgress,
            }],
            rationale: None,
        };
        store.submit_proposal(&bump).await.unwrap();
        store.apply_proposal(&bump.id).await.unwrap();

        // A PARTIAL pending reorder — covers only task A, not task B —
        // submitted directly, simulating a stale leftover proposal.
        let partial = crate::ai::ProposalDraft {
            id: "partial-reorder".into(),
            ops: vec![Sin90Op::ReorderTasks {
                week_id: target.id.clone(),
                order: vec![task_a.clone()],
            }],
            rationale: None,
        };
        let rec = crate::ai::AiCallRecord {
            id: "call-partial".into(),
            run_id: "run-seed".into(),
            task_kind: Capability::Propose,
            engine: crate::ai::Engine::Reflex,
            fallback_from: None,
            served_tier: None,
            model_id: None,
            prompt_tokens: None,
            completion_tokens: None,
            latency_ms: 0,
            ok: true,
            error_kind: None,
            proposal_id: None,
            at: "2026-09-24T00:00:00Z".into(),
        };
        crate::ai::AiSink::submit(&store, Capability::Propose, partial, rec)
            .await
            .unwrap();

        let run = body_json(
            app.clone()
                .oneshot(automation_req(
                    "POST",
                    "/ai/propose",
                    json!({"week_id": target.id}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let (state, items) = poll_run_to_done(&app, run["run_id"].as_str().unwrap()).await;
        assert_eq!(state, "done");
        assert_eq!(
            item_result(&items, "propose.reorder"),
            "proposed",
            "a PARTIAL pending reorder must not count as still-valid coverage"
        );
    }

    /// M-1(a) (2026-09-24 review round 3): a FULL-coverage pending reorder
    /// (AI-produced, via `AiSink::submit` directly) makes propose's blanket
    /// skip fire — `propose.reorder == "skipped"`, proposal count unchanged.
    /// This is the positive control `trigger_propose_dedup_requires_exact_
    /// reorder_coverage` above never exercised (that test only proves a
    /// PARTIAL pending reorder does NOT block); without a test asserting the
    /// literal `"skipped"` string, `skip_reorder`/`skip_create` could be
    /// hard-coded to `false` (turning `Skipped` into `Nothing` everywhere)
    /// and all 321 pre-existing tests would stay green. Mutation target:
    /// `dedup_propose`'s reorder loop body changed to a no-op (`skip_reorder`
    /// stays `false`) — this test alone must go red.
    #[tokio::test]
    async fn trigger_propose_dedup_full_coverage_reorder_is_skipped() {
        let (app, _sink, store) = test_app_with_store().await;
        let target = store.create_week("2026-W41").await.unwrap();
        let seed = crate::core::Sin90Proposal {
            id: "seed-w-tasks-61".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![Sin90Op::CreateTasks {
                week_id: target.id.clone(),
                tasks: vec![
                    NewTask {
                        title: "task a".into(),
                        direction_id: None,
                    },
                    NewTask {
                        title: "task b".into(),
                        direction_id: None,
                    },
                ],
            }],
            rationale: None,
        };
        store.submit_proposal(&seed).await.unwrap();
        store.apply_proposal(&seed.id).await.unwrap();
        let task_ids: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM sin90_tasks WHERE week_id = ? ORDER BY sort_key ASC",
        )
        .bind(&target.id)
        .fetch_all(store.pool())
        .await
        .unwrap();

        // A FULL-coverage reorder (both of W's tasks, just swapped) —
        // AI-produced via `AiSink::submit` directly, simulating a prior
        // run's output without needing a real model.
        let full_reorder = crate::ai::ProposalDraft {
            id: "full-reorder-61".into(),
            ops: vec![Sin90Op::ReorderTasks {
                week_id: target.id.clone(),
                order: vec![task_ids[1].clone(), task_ids[0].clone()],
            }],
            rationale: None,
        };
        let rec = crate::ai::AiCallRecord {
            id: "call-full-reorder-61".into(),
            run_id: "run-seed-61".into(),
            task_kind: Capability::Propose,
            engine: crate::ai::Engine::Reflex,
            fallback_from: None,
            served_tier: None,
            model_id: None,
            prompt_tokens: None,
            completion_tokens: None,
            latency_ms: 0,
            ok: true,
            error_kind: None,
            proposal_id: None,
            at: "2026-09-24T00:00:00Z".into(),
        };
        crate::ai::AiSink::submit(&store, Capability::Propose, full_reorder, rec)
            .await
            .unwrap();
        let proposals_before = store.list_pending_proposals().await.unwrap().len();

        let run = body_json(
            app.clone()
                .oneshot(automation_req(
                    "POST",
                    "/ai/propose",
                    json!({"week_id": target.id}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let (state, items) = poll_run_to_done(&app, run["run_id"].as_str().unwrap()).await;
        assert_eq!(state, "done");
        assert_eq!(item_result(&items, "propose.reorder"), "skipped");
        // M1 (2026-09-26 review round 2): dedup-skip carries `reason:
        // "dedup"` — mirrors `ai_classify`'s own dedup-skip assertion.
        assert_eq!(item_by_target(&items, "propose.reorder")["reason"], "dedup");
        let proposals_after = store.list_pending_proposals().await.unwrap().len();
        assert_eq!(
            proposals_before, proposals_after,
            "a blanket-skipped reorder must not have submitted anything new"
        );
    }

    /// M-2 (2026-09-24 review round 3, design §11.4.3): dedup only ever
    /// considers AI-PRODUCED proposals — a same-shape reorder a HUMAN
    /// submitted directly through `POST /proposals` (never touching
    /// `sin90_ai_calls`) must NOT block a fresh AI decision, even though its
    /// shape and coverage are identical to the AI-produced case above.
    #[tokio::test]
    async fn trigger_propose_dedup_ignores_human_submitted_same_shape_reorder() {
        let (app, _sink, store) = test_app_with_store().await;
        let target = store.create_week("2026-W42").await.unwrap();
        let seed = crate::core::Sin90Proposal {
            id: "seed-w-tasks-62".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![Sin90Op::CreateTasks {
                week_id: target.id.clone(),
                tasks: vec![
                    NewTask {
                        title: "task a".into(),
                        direction_id: None,
                    },
                    NewTask {
                        title: "task b".into(),
                        direction_id: None,
                    },
                ],
            }],
            rationale: None,
        };
        store.submit_proposal(&seed).await.unwrap();
        store.apply_proposal(&seed.id).await.unwrap();
        let task_ids: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM sin90_tasks WHERE week_id = ? ORDER BY sort_key ASC",
        )
        .bind(&target.id)
        .fetch_all(store.pool())
        .await
        .unwrap();

        // A FULL-coverage reorder submitted the HUMAN way — `store.
        // submit_proposal` directly, exactly what `POST /proposals` itself
        // calls — never touches `sin90_ai_calls` at all.
        let human_reorder = crate::core::Sin90Proposal {
            id: "human-reorder-62".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![Sin90Op::ReorderTasks {
                week_id: target.id.clone(),
                order: vec![task_ids[1].clone(), task_ids[0].clone()],
            }],
            rationale: None,
        };
        store.submit_proposal(&human_reorder).await.unwrap();

        let run = body_json(
            app.clone()
                .oneshot(automation_req(
                    "POST",
                    "/ai/propose",
                    json!({"week_id": target.id}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let (state, items) = poll_run_to_done(&app, run["run_id"].as_str().unwrap()).await;
        assert_eq!(state, "done");
        assert_ne!(
            item_result(&items, "propose.reorder"),
            "skipped",
            "a human-submitted same-shape proposal must never block the AI's own dedup"
        );
    }

    /// M-2's second clause (2026-09-24 review round 3, §11.4.3): a pending
    /// AI-produced CREATE proposal is "still valid" only while its target
    /// Direction remains a current gap — once that Direction is abandoned,
    /// the pending create is no longer treated as covering anything (skip_
    /// create flips back to `false`). Verified via the OBSERVABLE
    /// difference: round 1 (D still a gap) → `"skipped"`; round 2 (D
    /// abandoned) → NOT skipped (`"nothing"`, since T5.1.2's only production
    /// path, `model = None`, never actually produces a NEW create either way
    /// — reflex has no create step — but the skip itself must lift).
    #[tokio::test]
    async fn trigger_propose_dedup_create_no_longer_skipped_after_direction_abandoned() {
        let (app, _sink, store) = test_app_with_store().await;
        let direction = store
            .create_direction("Health", "2026-Q4", None)
            .await
            .unwrap();
        crate::store::test_hooks::insert_rhythm(&store, "rhythm-62")
            .await
            .unwrap();
        sqlx::query("UPDATE sin90_rhythms SET allocations = ? WHERE id = ?")
            .bind(
                serde_json::to_string(&vec![crate::core::Alloc {
                    direction_id: direction.id.clone(),
                    pct: 100,
                }])
                .unwrap(),
            )
            .bind("rhythm-62")
            .execute(store.pool())
            .await
            .unwrap();
        let target = store.create_week("2026-W43").await.unwrap();

        let pending_create = crate::ai::ProposalDraft {
            id: "pending-create-63".into(),
            ops: vec![Sin90Op::CreateTasks {
                week_id: target.id.clone(),
                tasks: vec![NewTask {
                    title: "new task under Health".into(),
                    direction_id: Some(direction.id.clone()),
                }],
            }],
            rationale: None,
        };
        let rec = crate::ai::AiCallRecord {
            id: "call-pending-create-63".into(),
            run_id: "run-seed-63".into(),
            task_kind: Capability::Propose,
            engine: crate::ai::Engine::Reflex,
            fallback_from: None,
            served_tier: None,
            model_id: None,
            prompt_tokens: None,
            completion_tokens: None,
            latency_ms: 0,
            ok: true,
            error_kind: None,
            proposal_id: None,
            at: "2026-09-24T00:00:00Z".into(),
        };
        crate::ai::AiSink::submit(&store, Capability::Propose, pending_create, rec)
            .await
            .unwrap();

        // Round 1: D is still a gap — the pending create is skipped.
        let run1 = body_json(
            app.clone()
                .oneshot(automation_req(
                    "POST",
                    "/ai/propose",
                    json!({"week_id": target.id}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let (state1, items1) = poll_run_to_done(&app, run1["run_id"].as_str().unwrap()).await;
        assert_eq!(state1, "done");
        assert_eq!(item_result(&items1, "propose.create"), "skipped");

        // D is abandoned — no longer a gap Direction at all.
        sqlx::query("UPDATE sin90_directions SET status = 'abandoned' WHERE id = ?")
            .bind(&direction.id)
            .execute(store.pool())
            .await
            .unwrap();

        let run2 = body_json(
            app.clone()
                .oneshot(automation_req(
                    "POST",
                    "/ai/propose",
                    json!({"week_id": target.id}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let (state2, items2) = poll_run_to_done(&app, run2["run_id"].as_str().unwrap()).await;
        assert_eq!(state2, "done");
        assert_ne!(
            item_result(&items2, "propose.create"),
            "skipped",
            "an abandoned Direction's pending create must no longer count as still-valid coverage"
        );
    }

    /// M-b (2026-09-24 review round 4, design §11.4.3): create's dedup is
    /// PER-DIRECTION — a pending create naming TWO directions {A, B} only
    /// excludes the ones STILL a gap, not all-or-nothing for the whole
    /// draft. B stops being a gap (a task lands in W under it, outside
    /// propose entirely); A remains genuinely covered. A fresh gap C (never
    /// mentioned by anything pending) must still be offered — dedup must
    /// NOT blanket-skip create just because SOME of its directions are
    /// stale. Calls `dedup_propose` DIRECTLY (not through a full trigger):
    /// `model = None`'s production path can never observe the difference
    /// between "blanket skipped" and "A excluded, C still offered" (reflex
    /// never creates either way).
    ///
    /// Mutation target: `dedup_propose`'s `skip_create = ... .all(|g| ...)`
    /// changed to `.any(...)` — `excluded_create_direction_ids` contains
    /// `A`, and `.any()` over `{A, C}` finds that ONE match and wrongly
    /// flips `skip_create` to `true` even though `C` is still wide open;
    /// this test's `assert!(!dedup.skip_create, ...)` goes red.
    #[tokio::test]
    async fn dedup_propose_excludes_create_directions_per_item_not_all_or_nothing() {
        let (_app, _sink, store) = test_app_with_store().await;
        let direction_a = store.create_direction("A", "2026-Q4", None).await.unwrap();
        let direction_b = store.create_direction("B", "2026-Q4", None).await.unwrap();
        let direction_c = store.create_direction("C", "2026-Q4", None).await.unwrap();
        crate::store::test_hooks::insert_rhythm(&store, "rhythm-abc")
            .await
            .unwrap();
        sqlx::query("UPDATE sin90_rhythms SET allocations = ? WHERE id = ?")
            .bind(
                serde_json::to_string(&vec![
                    crate::core::Alloc {
                        direction_id: direction_a.id.clone(),
                        pct: 100,
                    },
                    crate::core::Alloc {
                        direction_id: direction_b.id.clone(),
                        pct: 100,
                    },
                    crate::core::Alloc {
                        direction_id: direction_c.id.clone(),
                        pct: 100,
                    },
                ])
                .unwrap(),
            )
            .bind("rhythm-abc")
            .execute(store.pool())
            .await
            .unwrap();
        let target = store.create_week("2026-W45").await.unwrap();

        // B stops being a gap: a task lands in W under it (a normal
        // `CreateTasks`, nothing to do with propose's own dedup).
        let seed_b = crate::core::Sin90Proposal {
            id: "seed-under-b".into(),
            status: crate::core::ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![Sin90Op::CreateTasks {
                week_id: target.id.clone(),
                tasks: vec![NewTask {
                    title: "already under B".into(),
                    direction_id: Some(direction_b.id.clone()),
                }],
            }],
            rationale: None,
        };
        store.submit_proposal(&seed_b).await.unwrap();
        store.apply_proposal(&seed_b.id).await.unwrap();

        // A pending AI-produced create naming BOTH A and B.
        let pending = crate::ai::ProposalDraft {
            id: "pending-create-ab".into(),
            ops: vec![Sin90Op::CreateTasks {
                week_id: target.id.clone(),
                tasks: vec![
                    NewTask {
                        title: "new under A".into(),
                        direction_id: Some(direction_a.id.clone()),
                    },
                    NewTask {
                        title: "new under B".into(),
                        direction_id: Some(direction_b.id.clone()),
                    },
                ],
            }],
            rationale: None,
        };
        let rec = crate::ai::AiCallRecord {
            id: "call-pending-create-ab".into(),
            run_id: "run-seed-ab".into(),
            task_kind: Capability::Propose,
            engine: crate::ai::Engine::Reflex,
            fallback_from: None,
            served_tier: None,
            model_id: None,
            prompt_tokens: None,
            completion_tokens: None,
            latency_ms: 0,
            ok: true,
            error_kind: None,
            proposal_id: None,
            at: "2026-09-24T00:00:00Z".into(),
        };
        crate::ai::AiSink::submit(&store, Capability::Propose, pending, rec)
            .await
            .unwrap();

        let dedup = crate::http::ai_propose::dedup_propose(&store, &target)
            .await
            .unwrap();
        assert!(
            dedup
                .excluded_create_direction_ids
                .contains(&direction_a.id),
            "{dedup:?}"
        );
        assert!(
            !dedup
                .excluded_create_direction_ids
                .contains(&direction_b.id),
            "B was never a gap to begin with — nothing to exclude: {dedup:?}"
        );
        assert!(
            !dedup.skip_create,
            "C is still a genuinely uncovered gap — must NOT blanket-skip create: {dedup:?}"
        );
    }
}

// ---- POST /ai/summarize (T5.3.1, design §11.4 公共 + §11.4.2) --------------

mod ai_summarize {
    use super::*;
    use crate::ai::Capability;
    use crate::core::NewReview;
    use crate::core::ReviewKind;

    /// Polls `GET /ai/runs/{run_id}` until `state != "running"` (5s
    /// wall-clock ceiling, same posture `ai_classify`/`ai_propose`'s own
    /// fix uses) and returns the final `(state, items)`.
    async fn poll_run_to_done(app: &axum::Router, run_id: &str) -> (String, Value) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut state_str = "running".to_string();
        let mut items = Value::Null;
        while tokio::time::Instant::now() < deadline {
            let r = body_json(
                app.clone()
                    .oneshot(get_req(&format!("/ai/runs/{run_id}")))
                    .await
                    .unwrap(),
            )
            .await;
            state_str = r["state"].as_str().unwrap().to_string();
            items = r["items"].clone();
            if state_str != "running" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        (state_str, items)
    }

    #[tokio::test]
    async fn trigger_summarize_requires_an_actor_key() {
        let (app, _sink) = test_app().await;
        let req = Request::builder()
            .method("POST")
            .uri("/ai/summarize")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({"review_id": "r1"}).to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn trigger_summarize_unknown_field_is_400() {
        let (app, _sink) = test_app().await;
        let resp = app
            .oneshot(automation_req(
                "POST",
                "/ai/summarize",
                json!({"review_id": "r1", "oops": true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn trigger_summarize_unknown_review_is_404() {
        let (app, _sink) = test_app().await;
        let resp = app
            .oneshot(automation_req(
                "POST",
                "/ai/summarize",
                json!({"review_id": "does-not-exist"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// §11.4.2's "输入": `daily`/`rhythm` is a client mistake, not "nothing
    /// to summarize" — `400 unsupported_kind`, not `404`/`409`.
    #[tokio::test]
    async fn trigger_summarize_unsupported_kind_is_400() {
        let (app, _sink, store) = test_app_with_store().await;
        let review = store
            .create_review(&NewReview {
                kind: ReviewKind::Daily,
                period: "2026-09-24".into(),
            })
            .await
            .unwrap();
        let resp = app
            .oneshot(automation_req(
                "POST",
                "/ai/summarize",
                json!({"review_id": review.id}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], "unsupported_kind");
    }

    /// A finalized weekly review is a closed door for summarize, same as
    /// `PATCH /reviews/{id}` — `409`, same v1 error envelope every other
    /// route uses.
    #[tokio::test]
    async fn trigger_summarize_not_draft_is_409() {
        let (app, _sink, store) = test_app_with_store().await;
        let review = store
            .create_review(&NewReview {
                kind: ReviewKind::Weekly,
                period: "2026-W39".into(),
            })
            .await
            .unwrap();
        store.finalize_review(&review.id).await.unwrap();

        let resp = app
            .oneshot(automation_req(
                "POST",
                "/ai/summarize",
                json!({"review_id": review.id}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], "conflict");
    }

    /// J19/M3 (2026-09-26 review): a proposal submitted while the review was
    /// still `draft`, but ACCEPTED only after the review was finalized in
    /// the meantime, must be rejected — `422 ReviewNotDraft`, and the
    /// review's body (finalized) is unchanged.
    #[tokio::test]
    async fn accept_stale_summarize_proposal_after_finalize_is_422() {
        let (app, _sink, store) = test_app_with_store().await;
        let review = store
            .create_review(&NewReview {
                kind: ReviewKind::Weekly,
                period: "2026-W39".into(),
            })
            .await
            .unwrap();

        let resp = app
            .clone()
            .oneshot(automation_req(
                "POST",
                "/ai/summarize",
                json!({"review_id": review.id}),
            ))
            .await
            .unwrap();
        let run_id = body_json(resp).await["run_id"]
            .as_str()
            .unwrap()
            .to_string();
        let (state_str, items) = poll_run_to_done(&app, &run_id).await;
        assert_eq!(state_str, "done");
        assert_eq!(items.as_array().unwrap()[0]["result"], "proposed");

        let pending = store.list_pending_proposals().await.unwrap();
        assert_eq!(pending.len(), 1, "{pending:?}");
        let proposal_id = pending[0].id.clone();

        // Finalize the review WHILE the proposal is still pending.
        store.finalize_review(&review.id).await.unwrap();

        let resp = app
            .oneshot(human_req(
                "POST",
                &format!("/proposals/{proposal_id}/accept"),
                json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = body_json(resp).await;
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("ReviewNotDraft")
                || body["error"]["message"]
                    .as_str()
                    .unwrap()
                    .to_lowercase()
                    .contains("draft"),
            "{body:?}"
        );

        let untouched = store.get_review(&review.id).await.unwrap();
        assert_eq!(untouched.status, crate::core::ReviewStatus::Finalized);
    }

    /// Pre-seeds the registry directly (rather than racing two real
    /// requests, which would be flaky against a background `tokio::spawn`)
    /// to pin the single-flight 409 shape — same technique `ai_classify`/
    /// `ai_propose`'s own tests use.
    #[tokio::test]
    async fn trigger_summarize_busy_returns_409_with_existing_run_id() {
        let store = Sin90Store::open_memory().await.unwrap();
        let review = store
            .create_review(&NewReview {
                kind: ReviewKind::Weekly,
                period: "2026-W39".into(),
            })
            .await
            .unwrap();
        let state = Sin90State::new(
            store,
            Arc::new(RecordingSink::default()),
            ActorKeys {
                human: HUMAN.into(),
                automation: AUTOMATION.into(),
            },
        );
        state
            .ai_runs
            .lock()
            .unwrap()
            .start(Capability::Summarize, "run-already-going");
        let app = router(state, false);
        let resp = app
            .oneshot(automation_req(
                "POST",
                "/ai/summarize",
                json!({"review_id": review.id}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = body_json(resp).await;
        assert_eq!(body["code"], "ai_busy");
        assert_eq!(body["run_id"], "run-already-going");
    }

    /// End-to-end through the real router — `202` → background run → polled
    /// to completion via `GET /ai/runs/{id}` — with NO real `ModelPort`
    /// wired yet (T5.1.2), so this exercises the reflex fallback (a
    /// facts-only body, always decisive).
    #[tokio::test]
    async fn trigger_summarize_runs_in_background_and_is_pollable_to_done() {
        let (app, sink, store) = test_app_with_store().await;
        let area = store.create_area("Coding").await.unwrap();
        let direction = store
            .create_direction("Ship the thing", "2026-Q4", Some(&area.id))
            .await
            .unwrap();
        let block = store
            .create_block(Some(&direction.id), None, 90)
            .await
            .unwrap();
        store
            .transition_block(&block.id, crate::core::ScheduleBlockStatus::Started)
            .await
            .unwrap();
        store
            .transition_block(&block.id, crate::core::ScheduleBlockStatus::Completed)
            .await
            .unwrap();
        crate::store::test_hooks::set_last_event_at(
            &store,
            "block",
            &block.id,
            "2026-09-24T10:00:00Z",
        )
        .await
        .unwrap();
        let task = store
            .create_task(
                "Ship it",
                None,
                None,
                crate::core::TaskKind::Other,
                crate::core::Energy::Mid,
                None,
            )
            .await
            .unwrap();
        for to in [
            crate::core::TaskStatus::Planned,
            crate::core::TaskStatus::InProgress,
            crate::core::TaskStatus::Done,
        ] {
            store.transition_task(&task.id, to).await.unwrap();
        }
        crate::store::test_hooks::set_last_event_at(
            &store,
            "task",
            &task.id,
            "2026-09-24T10:00:00Z",
        )
        .await
        .unwrap();

        // Ground truth (T4.3.1's own function, independently of anything
        // `ai::summarize` computes).
        let t431 = store.weekly_draft("2026-W39").await.unwrap();
        assert_eq!(t431.by_direction[0].minutes, 90);
        assert_eq!(t431.tasks_done, 1);

        let review = store
            .create_review(&NewReview {
                kind: ReviewKind::Weekly,
                period: "2026-W39".into(),
            })
            .await
            .unwrap();
        assert_eq!(review.body, "", "a freshly created review's body is empty");

        let resp = app
            .clone()
            .oneshot(automation_req(
                "POST",
                "/ai/summarize",
                json!({"review_id": review.id}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let body = body_json(resp).await;
        assert_eq!(body["capability"], "summarize");
        let run_id = body["run_id"].as_str().unwrap().to_string();

        let (state_str, items) = poll_run_to_done(&app, &run_id).await;
        assert_eq!(state_str, "done", "run never finished: items={items:?}");
        let items = items.as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["target"], review.id);
        assert_eq!(items[0]["result"], "proposed");
        assert!(items[0].get("reason").is_none(), "{items:?}");

        let proposed = store.get_review(&review.id).await.unwrap();
        // The review itself is UNCHANGED (the proposal is only `pending`,
        // §11.5's structural guarantee — a trigger never writes business
        // state directly).
        assert_eq!(proposed.body, "");

        let pending: Vec<_> = store
            .list_pending_proposals()
            .await
            .unwrap()
            .into_iter()
            .filter(|p| {
                matches!(
                    p.ops.as_slice(),
                    [crate::core::Sin90Op::DraftReviewBody { review_id, .. }]
                        if review_id == &review.id
                )
            })
            .collect();
        assert_eq!(pending.len(), 1, "{pending:?}");
        let crate::core::Sin90Op::DraftReviewBody {
            body: proposed_body,
            ..
        } = &pending[0].ops[0]
        else {
            unreachable!()
        };
        // 90 minutes -> "1 小时 30 分钟" — computed the SAME way
        // `t431.by_direction[0].minutes` was independently read above.
        assert!(
            proposed_body.contains("投入：1 小时 30 分钟"),
            "{proposed_body}"
        );
        assert!(proposed_body.contains("完成任务数：1"), "{proposed_body}");
        assert!(proposed_body.contains("本周数字"), "{proposed_body}");

        // H2 (design §11.4 公共's L1): mirrored through the SAME `EventSink`
        // the human `POST /proposals` path uses.
        let submitted: Vec<_> = sink
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|(kind, _)| kind == "proposal.submitted")
            .cloned()
            .collect();
        assert_eq!(submitted.len(), 1, "{submitted:?}");
    }

    /// Q7/J19 (design §11.4.2's "可改写条件"): a human-written line under the
    /// program's own facts block means this run produces NOTHING — the item
    /// result is `skipped` with `reason: "human_text"`.
    #[tokio::test]
    async fn trigger_summarize_human_text_is_skipped() {
        let (app, _sink, store) = test_app_with_store().await;
        let review = store
            .create_review(&NewReview {
                kind: ReviewKind::Weekly,
                period: "2026-W39".into(),
            })
            .await
            .unwrap();
        store
            .update_review_body(&review.id, "我自己写的复盘，AI 别碰。")
            .await
            .unwrap();

        let resp = app
            .clone()
            .oneshot(automation_req(
                "POST",
                "/ai/summarize",
                json!({"review_id": review.id}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let run_id = body_json(resp).await["run_id"]
            .as_str()
            .unwrap()
            .to_string();

        let (state_str, items) = poll_run_to_done(&app, &run_id).await;
        assert_eq!(state_str, "done");
        let items = items.as_array().unwrap();
        assert_eq!(items[0]["result"], "skipped");
        assert_eq!(items[0]["reason"], "human_text");

        let untouched = store.get_review(&review.id).await.unwrap();
        assert_eq!(untouched.body, "我自己写的复盘，AI 别碰。");
        let pending = store.list_pending_proposals().await.unwrap();
        assert!(
            pending.is_empty(),
            "human text must never produce a proposal: {pending:?}"
        );
    }

    /// §11.4 公共's "去重", generic form (§11.4.2 gives summarize no
    /// capability-specific refinement): a second trigger while a still-valid
    /// pending `DraftReviewBody` proposal already targets this review is
    /// skipped entirely, `reason: "dedup"`, without even reading
    /// `weekly_draft` again.
    #[tokio::test]
    async fn trigger_summarize_dedup_skips_repeat_run() {
        let (app, _sink, store) = test_app_with_store().await;
        let review = store
            .create_review(&NewReview {
                kind: ReviewKind::Weekly,
                period: "2026-W39".into(),
            })
            .await
            .unwrap();

        let resp1 = app
            .clone()
            .oneshot(automation_req(
                "POST",
                "/ai/summarize",
                json!({"review_id": review.id}),
            ))
            .await
            .unwrap();
        let run1 = body_json(resp1).await["run_id"]
            .as_str()
            .unwrap()
            .to_string();
        let (state1, items1) = poll_run_to_done(&app, &run1).await;
        assert_eq!(state1, "done");
        assert_eq!(items1.as_array().unwrap()[0]["result"], "proposed");

        let resp2 = app
            .clone()
            .oneshot(automation_req(
                "POST",
                "/ai/summarize",
                json!({"review_id": review.id}),
            ))
            .await
            .unwrap();
        let run2 = body_json(resp2).await["run_id"]
            .as_str()
            .unwrap()
            .to_string();
        let (state2, items2) = poll_run_to_done(&app, &run2).await;
        assert_eq!(state2, "done");
        assert_eq!(items2.as_array().unwrap()[0]["result"], "skipped");
        assert_eq!(items2.as_array().unwrap()[0]["reason"], "dedup");

        let pending = store.list_pending_proposals().await.unwrap();
        assert_eq!(
            pending.len(),
            1,
            "the second run must not have produced a duplicate proposal: {pending:?}"
        );

        // Positive control: accept the first proposal, then dedup itself
        // (`dedup_summarize`) — the SAME query the trigger route uses —
        // reports it no longer blocks a third run (the pending proposal is
        // gone, `precheck` has nothing left to find valid).
        store.apply_proposal(&pending[0].id).await.unwrap();
        let skip_after_accept = crate::http::ai_summarize::dedup_summarize(&store, &review.id)
            .await
            .unwrap();
        assert!(
            skip_after_accept.is_none(),
            "an applied proposal must not dedup-block a future run"
        );
    }

    // ---- T5.7.2 (design §2 #31): rejected-suggestion suppression ----------

    /// `cargo test suppress_`'s summarize half: a REJECTED `DraftReviewBody`
    /// keeps `dedup_summarize` reporting `"suppressed_rejected"` for the SAME
    /// review while the body is untouched, and stops (`dedup_summarize`
    /// returns `None` — dedup no longer blocks this review; whether the NEXT
    /// `/ai/summarize` run actually PRODUCES a fresh rewrite is a separate
    /// question the ladder itself decides, not something this dedup-only
    /// check claims) the moment the body is edited out from under it (the
    /// fingerprint's own "situation changed" leg — §2 #31's summarize-
    /// specific reuse of `DraftReviewBody`'s D6 hash comparison; no "新
    /// Direction" leg here, by design).
    #[tokio::test]
    async fn suppress_rejected_summarize_blocks_until_body_edited() {
        let (_app, _sink, store) = test_app_with_store().await;
        let review = store
            .create_review(&NewReview {
                kind: ReviewKind::Weekly,
                period: "2026-W40".into(),
            })
            .await
            .unwrap();
        let base_hash = crate::core::body_sha256(&review.body);
        let draft = crate::ai::ProposalDraft {
            id: "p-suppress-summarize-1".into(),
            ops: vec![crate::core::Sin90Op::DraftReviewBody {
                review_id: review.id.clone(),
                base_body_sha256: base_hash,
                body: "a first draft".into(),
            }],
            rationale: None,
        };
        let rec = crate::ai::AiCallRecord {
            id: "c-suppress-summarize-1".into(),
            run_id: "run-suppress-summarize".into(),
            task_kind: Capability::Summarize,
            engine: crate::ai::Engine::Reflex,
            fallback_from: None,
            served_tier: None,
            model_id: None,
            prompt_tokens: None,
            completion_tokens: None,
            latency_ms: 0,
            ok: true,
            error_kind: None,
            proposal_id: None,
            at: "2026-09-24T00:00:00Z".into(),
        };
        crate::ai::AiSink::submit(&store, Capability::Summarize, draft, rec)
            .await
            .unwrap();
        store
            .reject_proposal("p-suppress-summarize-1", None)
            .await
            .unwrap();

        // Negative control: the body is exactly what it was when the
        // rejected proposal was generated against it — still suppressed.
        let reason = crate::http::ai_summarize::dedup_summarize(&store, &review.id)
            .await
            .unwrap();
        assert_eq!(reason, Some("suppressed_rejected"));

        // Positive control: a human edits the review's own body.
        store
            .update_review_body(&review.id, "a human rewrote this")
            .await
            .unwrap();
        let reason2 = crate::http::ai_summarize::dedup_summarize(&store, &review.id)
            .await
            .unwrap();
        assert_eq!(reason2, None, "an edited body must lift the suppression");
    }

    /// T5.7.2 review round 2 (M4): the SAME review rejected TWICE, with a
    /// human body edit in between — `dedup_summarize` must compare against
    /// the MOST RECENT rejection's `base_body_sha256`, not the first one
    /// (`list_rejected_ops` is oldest-first; `dedup_summarize`'s own
    /// `.iter().rev().find_map(..)` is what picks the last match instead of
    /// the first). Mutation target: drop the `.rev()` and this goes red —
    /// the STALE first rejection's base hash no longer matches the current
    /// body, so suppression wrongly lifts.
    #[tokio::test]
    async fn suppress_rejected_summarize_uses_the_most_recent_rejections_base_hash() {
        let (_app, _sink, store) = test_app_with_store().await;
        let review = store
            .create_review(&NewReview {
                kind: ReviewKind::Weekly,
                period: "2026-W41".into(),
            })
            .await
            .unwrap();

        // Round 1: rejected against the ORIGINAL body.
        let base_hash_1 = crate::core::body_sha256(&review.body);
        let draft1 = crate::ai::ProposalDraft {
            id: "p-suppress-summarize-2a".into(),
            ops: vec![crate::core::Sin90Op::DraftReviewBody {
                review_id: review.id.clone(),
                base_body_sha256: base_hash_1.clone(),
                body: "a first draft".into(),
            }],
            rationale: None,
        };
        let rec1 = crate::ai::AiCallRecord {
            id: "c-suppress-summarize-2a".into(),
            run_id: "run-suppress-summarize-2".into(),
            task_kind: Capability::Summarize,
            engine: crate::ai::Engine::Reflex,
            fallback_from: None,
            served_tier: None,
            model_id: None,
            prompt_tokens: None,
            completion_tokens: None,
            latency_ms: 0,
            ok: true,
            error_kind: None,
            proposal_id: None,
            at: "2026-09-24T00:00:00Z".into(),
        };
        crate::ai::AiSink::submit(&store, Capability::Summarize, draft1, rec1)
            .await
            .unwrap();
        store
            .reject_proposal("p-suppress-summarize-2a", None)
            .await
            .unwrap();

        // A human edits the body BETWEEN the two rejections.
        store
            .update_review_body(&review.id, "edited between rejections")
            .await
            .unwrap();
        let current_after_edit = store.get_review(&review.id).await.unwrap();
        let base_hash_2 = crate::core::body_sha256(&current_after_edit.body);
        assert_ne!(
            base_hash_1, base_hash_2,
            "the edit must actually change the hash"
        );

        // Round 2: rejected against the NEW (post-edit) body — the body is
        // NOT touched again after this.
        let draft2 = crate::ai::ProposalDraft {
            id: "p-suppress-summarize-2b".into(),
            ops: vec![crate::core::Sin90Op::DraftReviewBody {
                review_id: review.id.clone(),
                base_body_sha256: base_hash_2.clone(),
                body: "a second draft".into(),
            }],
            rationale: None,
        };
        let rec2 = crate::ai::AiCallRecord {
            id: "c-suppress-summarize-2b".into(),
            run_id: "run-suppress-summarize-2".into(),
            task_kind: Capability::Summarize,
            engine: crate::ai::Engine::Reflex,
            fallback_from: None,
            served_tier: None,
            model_id: None,
            prompt_tokens: None,
            completion_tokens: None,
            latency_ms: 0,
            ok: true,
            error_kind: None,
            proposal_id: None,
            at: "2026-09-24T00:00:01Z".into(),
        };
        crate::ai::AiSink::submit(&store, Capability::Summarize, draft2, rec2)
            .await
            .unwrap();
        store
            .reject_proposal("p-suppress-summarize-2b", None)
            .await
            .unwrap();

        // The body is still exactly what round 2's rejection saw
        // (`base_hash_2`) — must be suppressed. A buggy implementation that
        // picked the FIRST (oldest) rejection instead would compare against
        // `base_hash_1`, which no longer matches, and wrongly lift it.
        let reason = crate::http::ai_summarize::dedup_summarize(&store, &review.id)
            .await
            .unwrap();
        assert_eq!(
            reason,
            Some("suppressed_rejected"),
            "must compare against the MOST RECENT rejection's base hash, not the first one"
        );
    }
}
