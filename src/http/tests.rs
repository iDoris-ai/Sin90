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
        assert_eq!(skipped, vec![task.id.clone()]);

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

    /// T5.7.1 / T5.2.1's own dependency: a REJECTED classify
    /// (`AssignTaskDirection`) proposal must stop blocking `dedup_targets`
    /// for its target task — mirrors the test above, but invalidates the
    /// pending proposal via `reject_proposal` instead of abandoning the
    /// target Direction, pinning that THIS path (not just precheck's own
    /// dry-run failing) unblocks dedup.
    #[tokio::test]
    async fn proposal_reject_of_classify_proposal_unblocks_dedup_for_same_task() {
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
            id: "p-dedup-reject-1".into(),
            ops: vec![crate::core::Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: direction.id.clone(),
            }],
            rationale: None,
        };
        crate::ai::AiSink::submit(&store, Capability::Classify, draft, call_rec("c-reject-1"))
            .await
            .unwrap();

        // Negative control: still pending — blocks dedup.
        let (kept, skipped) =
            crate::http::ai_classify::dedup_targets(&store, std::slice::from_ref(&task))
                .await
                .unwrap();
        assert!(
            kept.is_empty(),
            "the pending proposal must still block this task"
        );
        assert_eq!(skipped, vec![task.id.clone()]);

        // Reject it (store level directly — the actor gate is an HTTP-layer
        // concern, pinned separately by `proposal_reject_by_automation_key_
        // is_403_and_nothing_changes`).
        store
            .reject_proposal("p-dedup-reject-1", None)
            .await
            .unwrap();

        // The judgement: dedup no longer blocks the task.
        let (kept2, skipped2) =
            crate::http::ai_classify::dedup_targets(&store, std::slice::from_ref(&task))
                .await
                .unwrap();
        assert_eq!(
            kept2.len(),
            1,
            "a rejected proposal must no longer block its target from dedup"
        );
        assert!(skipped2.is_empty());

        // The rejection log correctly attributes this to `classify`.
        let rows = crate::store::test_hooks::proposal_rejection_rows(&store, "p-dedup-reject-1")
            .await
            .unwrap();
        assert_eq!(rows[0].capability_source, "classify");
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

    /// T5.7.1 / T5.2.1's dependency, the `propose` half: rejecting a pending
    /// `carry` proposal that was blocking dedup lets the NEXT `/ai/propose`
    /// run for the same week reproduce a proposal for the SAME candidate
    /// task (unlike the accept-based positive control above — accepting
    /// actually carries the source task over, so that test needs a SECOND
    /// seed task for its "round 3"; rejecting leaves the original task
    /// exactly where it was, so the same target reappears).
    #[tokio::test]
    async fn proposal_reject_of_propose_carry_unblocks_next_run_for_same_target() {
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

        // Round 3: the SAME original candidate is uncovered again (it was
        // never carried over — rejecting, unlike accepting, leaves it
        // untouched).
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
            "proposed",
            "a rejected proposal must not permanently block its target"
        );

        let rows =
            crate::store::test_hooks::proposal_rejection_rows(&store, &pending_after_round1[0].id)
                .await
                .unwrap();
        assert_eq!(rows[0].capability_source, "propose");
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
