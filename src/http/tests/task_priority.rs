//! HTTP contracts for task priority and completion evidence.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use super::{
    body_json, get_req, human_req, router, ActorKeys, RecordingSink, Sin90State, AUTOMATION, HUMAN,
};
use crate::ai::{ModelFailure, ModelReply, ModelRequest, ServedTier};
use crate::http::ModelCaller;
use crate::store::Sin90Store;

const STALE_KEY: &str = "stale-rotated-actor-key";

struct ScriptedModel {
    tier: ServedTier,
    body: String,
    prompts: Arc<Mutex<Vec<String>>>,
}

impl ModelCaller for ScriptedModel {
    fn complete<'a>(
        &'a self,
        req: ModelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ModelReply, ModelFailure>> + Send + 'a>> {
        let tier = self.tier;
        let body = self.body.clone();
        let prompts = Arc::clone(&self.prompts);
        Box::pin(async move {
            let user = req
                .messages
                .iter()
                .find(|message| message.role == crate::ai::Role::User)
                .map(|message| message.content.clone())
                .unwrap_or_default();
            prompts.lock().unwrap().push(user);
            Ok(ModelReply {
                text: body,
                model_id: Some("priority-test".into()),
                tier,
                prompt_tokens: Some(3),
                completion_tokens: Some(5),
            })
        })
    }
}

struct HangingModel {
    entered: Arc<AtomicBool>,
}

impl ModelCaller for HangingModel {
    fn complete<'a>(
        &'a self,
        _req: ModelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ModelReply, ModelFailure>> + Send + 'a>> {
        let entered = Arc::clone(&self.entered);
        Box::pin(async move {
            entered.store(true, Ordering::SeqCst);
            std::future::pending().await
        })
    }
}

fn keyed(method: &str, uri: &str, key: Option<&str>, raw: &str) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(key) = key {
        builder = builder.header("x-sin90-actor-key", key);
    }
    builder.body(Body::from(raw.to_string())).unwrap()
}

fn app_with_model(store: Sin90Store, model: Arc<dyn ModelCaller>) -> axum::Router {
    app_with_model_deadline(store, model, None)
}

fn app_with_model_deadline(
    store: Sin90Store,
    model: Arc<dyn ModelCaller>,
    deadline: Option<Duration>,
) -> axum::Router {
    let mut state = Sin90State::new(
        store,
        Arc::new(RecordingSink::default()),
        ActorKeys {
            human: HUMAN.into(),
            automation: AUTOMATION.into(),
        },
    );
    state.model = Some(model);
    if let Some(deadline) = deadline {
        state.run_hard_deadline = deadline;
    }
    router(state, false)
}

async fn create_task(app: &axum::Router, title: &str) -> String {
    let response = app
        .clone()
        .oneshot(human_req("POST", "/tasks", json!({ "title": title })))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    body_json(response).await["id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn complete_task(app: &axum::Router, id: &str) {
    for to in ["planned", "in_progress", "done"] {
        let response = app
            .clone()
            .oneshot(human_req(
                "PATCH",
                &format!("/tasks/{id}"),
                json!({ "to": to }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "transition to {to}");
    }
}

async fn priority_of(app: &axum::Router, id: &str) -> Value {
    let response = app
        .clone()
        .oneshot(get_req(&format!("/tasks/{id}/priority")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    body_json(response).await
}

fn model_items(items: &[Value]) -> String {
    json!({ "items": items }).to_string()
}

fn advice_item(task_id: &str, quadrant: &str) -> Value {
    json!({
        "task_id": task_id,
        "quadrant": quadrant,
        "importance": "high",
        "urgency": "low",
        "rationale": "specific evidence for this task",
        "sufficient_evidence": true
    })
}

async fn assert_forbidden(app: &axum::Router, request: Request<Body>) {
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(response).await["error"]["code"], "forbidden");
}

async fn memory_app_replying(
    tier: ServedTier,
    body: String,
) -> (axum::Router, Sin90Store, Arc<Mutex<Vec<String>>>) {
    let store = Sin90Store::open_memory().await.unwrap();
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let app = app_with_model(
        store.clone(),
        Arc::new(ScriptedModel {
            tier,
            body,
            prompts: Arc::clone(&prompts),
        }),
    );
    (app, store, prompts)
}

#[tokio::test]
async fn priority_writes_reject_automation_missing_and_stale_keys_without_writing() {
    let (app, _, prompts) = memory_app_replying(ServedTier::Local, model_items(&[])).await;
    let task_id = create_task(&app, "gate this task").await;
    let keys = [Some(AUTOMATION), None, Some(STALE_KEY)];
    let override_body = r#"{"quadrant":"do_first","rationale":"should not stick"}"#;
    let note_body = r#"{"note":"should not stick"}"#;
    let eval_body = format!(r#"{{"task_ids":["{task_id}"],"advice":"ignore"}}"#);

    for key in keys {
        assert_forbidden(
            &app,
            keyed(
                "PUT",
                &format!("/tasks/{task_id}/priority/override"),
                key,
                "not-json",
            ),
        )
        .await;
        assert_forbidden(
            &app,
            keyed(
                "PUT",
                &format!("/tasks/{task_id}/priority/override"),
                key,
                override_body,
            ),
        )
        .await;
        assert_forbidden(
            &app,
            keyed(
                "DELETE",
                &format!("/tasks/{task_id}/priority/override"),
                key,
                "",
            ),
        )
        .await;
        assert_forbidden(
            &app,
            keyed("POST", "/ai/priority/evaluate", key, "not-json"),
        )
        .await;
        assert_forbidden(
            &app,
            keyed("POST", "/ai/priority/evaluate", key, &eval_body),
        )
        .await;
    }

    let untouched = priority_of(&app, &task_id).await;
    assert!(untouched["ai"].is_null());
    assert!(untouched["human_override"].is_null());
    assert!(
        prompts.lock().unwrap().is_empty(),
        "a rejected evaluate must not call the model"
    );

    let saved = app
        .clone()
        .oneshot(human_req(
            "PUT",
            &format!("/tasks/{task_id}/priority/override"),
            json!({"quadrant": "schedule", "rationale": "human choice"}),
        ))
        .await
        .unwrap();
    assert_eq!(saved.status(), StatusCode::OK);
    assert_eq!(
        body_json(saved).await["human_override"]["quadrant"],
        "schedule"
    );

    for key in keys {
        assert_forbidden(
            &app,
            keyed(
                "DELETE",
                &format!("/tasks/{task_id}/priority/override"),
                key,
                "",
            ),
        )
        .await;
    }
    assert_eq!(
        priority_of(&app, &task_id).await["human_override"]["quadrant"],
        "schedule"
    );

    complete_task(&app, &task_id).await;
    for key in keys {
        assert_forbidden(
            &app,
            keyed(
                "PUT",
                &format!("/tasks/{task_id}/completion-note"),
                key,
                "not-json",
            ),
        )
        .await;
        assert_forbidden(
            &app,
            keyed(
                "PUT",
                &format!("/tasks/{task_id}/completion-note"),
                key,
                note_body,
            ),
        )
        .await;
    }
    let evidence = app
        .oneshot(get_req(&format!("/tasks/{task_id}/completion-evidence")))
        .await
        .unwrap();
    assert_eq!(evidence.status(), StatusCode::OK);
    assert!(body_json(evidence).await["note"].is_null());
}

#[tokio::test]
async fn overlong_evaluation_advice_is_rejected_and_the_limit_counts_characters() {
    let (app, _, _) = super::test_app_with_store().await;
    let task_id = create_task(&app, "advice limit").await;
    let too_long = "界".repeat(2_001);
    let response = app
        .clone()
        .oneshot(human_req(
            "POST",
            "/ai/priority/evaluate",
            json!({"task_ids": [task_id], "advice": too_long}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let text = String::from_utf8(
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(text.contains("2000"), "{text}");
    assert!(priority_of(&app, &task_id).await["ai"].is_null());

    let at_limit = "界".repeat(2_000);
    let response = app
        .clone()
        .oneshot(human_req(
            "POST",
            "/ai/priority/evaluate",
            json!({"task_ids": [task_id], "advice": at_limit}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert!(body["evaluated"].as_array().unwrap().is_empty());
    assert_eq!(body["unevaluated"][0]["task_id"], task_id);
    assert_eq!(body["unevaluated"][0]["reason"], "model_unavailable");
    assert!(priority_of(&app, &task_id).await["ai"].is_null());
}

#[tokio::test]
async fn completion_note_is_denied_until_the_task_is_finished() {
    let (app, _, _) = super::test_app_with_store().await;
    let task_id = create_task(&app, "not done yet").await;
    let denied = app
        .clone()
        .oneshot(human_req(
            "PUT",
            &format!("/tasks/{task_id}/completion-note"),
            json!({"note": "too early"}),
        ))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(denied).await["error"]["code"], "not_found");

    complete_task(&app, &task_id).await;
    let evidence = app
        .clone()
        .oneshot(get_req(&format!("/tasks/{task_id}/completion-evidence")))
        .await
        .unwrap();
    assert_eq!(evidence.status(), StatusCode::OK);
    assert!(
        body_json(evidence).await["note"].is_null(),
        "a note refused before completion must not appear afterwards"
    );

    let saved = app
        .oneshot(human_req(
            "PUT",
            &format!("/tasks/{task_id}/completion-note"),
            json!({"note": "after the fact"}),
        ))
        .await
        .unwrap();
    assert_eq!(saved.status(), StatusCode::OK);
    assert_eq!(body_json(saved).await["note"], "after the fact");
}

#[tokio::test]
async fn model_reply_cannot_write_task_ids_the_caller_did_not_request() {
    let store = Sin90Store::open_memory().await.unwrap();
    let setup = app_with_model(
        store.clone(),
        Arc::new(ScriptedModel {
            tier: ServedTier::Local,
            body: model_items(&[]),
            prompts: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let selected = create_task(&setup, "selected task").await;
    let other = create_task(&setup, "other task").await;
    let app = app_with_model(
        store.clone(),
        Arc::new(ScriptedModel {
            tier: ServedTier::Local,
            body: model_items(&[
                advice_item(&selected, "schedule"),
                advice_item(&other, "do_first"),
                advice_item("not-a-task", "eliminate"),
            ]),
            prompts: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let response = app
        .clone()
        .oneshot(human_req(
            "POST",
            "/ai/priority/evaluate",
            json!({"task_ids": [selected]}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let payload = body_json(response).await;
    let evaluated = payload["evaluated"].as_array().unwrap();
    assert_eq!(evaluated.len(), 1);
    assert_eq!(evaluated[0]["task_id"], selected);
    assert_eq!(evaluated[0]["ai"]["quadrant"], "schedule");
    assert!(payload["unevaluated"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["task_id"] != other && row["task_id"] != "not-a-task"));

    assert_eq!(
        priority_of(&app, &selected).await["ai"]["quadrant"],
        "schedule"
    );
    let untouched = priority_of(&app, &other).await;
    assert!(untouched["ai"].is_null());
    assert_eq!(untouched["evaluation_status"], "unevaluated");
    let missing = app
        .oneshot(get_req("/tasks/not-a-task/priority"))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_task_priorities")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(rows, 1, "only the requested task may gain a priority row");
}

#[tokio::test]
async fn evaluation_prompt_includes_only_the_selected_tasks() {
    let store = Sin90Store::open_memory().await.unwrap();
    let setup = app_with_model(
        store.clone(),
        Arc::new(ScriptedModel {
            tier: ServedTier::Local,
            body: model_items(&[]),
            prompts: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let selected = create_task(&setup, "selected-visible-task").await;
    let other = create_task(&setup, "unselected-private-task").await;
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let advice = "ADVICE-only-for-this-evaluation";
    let app = app_with_model(
        store,
        Arc::new(ScriptedModel {
            tier: ServedTier::Local,
            body: model_items(&[advice_item(&selected, "schedule")]),
            prompts: Arc::clone(&prompts),
        }),
    );
    let response = app
        .oneshot(human_req(
            "POST",
            "/ai/priority/evaluate",
            json!({"task_ids": [selected], "advice": advice}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let seen = prompts.lock().unwrap();
    assert_eq!(seen.len(), 1, "evaluate must call the model once");
    let prompt = &seen[0];
    assert!(prompt.contains(&selected), "{prompt}");
    assert!(prompt.contains("selected-visible-task"), "{prompt}");
    assert!(prompt.contains(advice), "{prompt}");
    assert!(
        !prompt.contains(&other),
        "prompt must not carry tasks the caller did not select: {prompt}"
    );
    assert!(
        !prompt.contains("unselected-private-task"),
        "prompt must not carry tasks the caller did not select: {prompt}"
    );
}

async fn remote_eval_app(quadrant: &str) -> (axum::Router, Sin90Store, String) {
    let (setup, store, prompts) = memory_app_replying(ServedTier::Remote, model_items(&[])).await;
    let task_id = create_task(&setup, "remote assessment").await;
    let app = app_with_model(
        store.clone(),
        Arc::new(ScriptedModel {
            tier: ServedTier::Remote,
            body: model_items(&[advice_item(&task_id, quadrant)]),
            prompts,
        }),
    );
    (app, store, task_id)
}

#[tokio::test]
async fn insufficient_evidence_clears_ai_and_preserves_override() {
    let (setup, store, prompts) = memory_app_replying(ServedTier::Local, model_items(&[])).await;
    let task_id = create_task(&setup, "needs evidence").await;
    store
        .set_task_priority_ai(&task_id, "schedule", "high", "low", "old advice")
        .await
        .unwrap();
    store
        .set_task_priority_override(&task_id, "delegate", "my choice")
        .await
        .unwrap();
    let app = app_with_model(
        store,
        Arc::new(ScriptedModel {
            tier: ServedTier::Local,
            body: model_items(&[json!({"task_id":task_id, "sufficient_evidence":false})]),
            prompts,
        }),
    );
    let response = app
        .clone()
        .oneshot(human_req(
            "POST",
            "/ai/priority/evaluate",
            json!({"task_ids":[task_id]}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body_json(response).await["unevaluated"][0]["reason"],
        "insufficient_evidence"
    );
    let priority = priority_of(&app, &task_id).await;
    assert!(priority["ai"].is_null());
    assert_eq!(priority["effective"]["quadrant"], "delegate");
}

#[tokio::test]
async fn remote_reply_is_dropped_and_audited_when_executive_is_disabled() {
    let (app, store, task_id) = remote_eval_app("do_first").await;
    let response = app
        .clone()
        .oneshot(human_req(
            "POST",
            "/ai/priority/evaluate",
            json!({"task_ids": [task_id]}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let payload = body_json(response).await;
    assert!(
        payload["evaluated"].as_array().unwrap().is_empty(),
        "a remote reply must be dropped while executive access is off: {payload}"
    );
    assert_eq!(payload["unevaluated"][0]["task_id"], task_id);
    assert_eq!(payload["unevaluated"][0]["reason"], "model_error");
    let priority = priority_of(&app, &task_id).await;
    assert!(priority["ai"].is_null(), "{priority}");
    assert_eq!(priority["evaluation_status"], "unevaluated");

    let run_id: String = sqlx::query_scalar(
        "SELECT run_id FROM sin90_ai_calls WHERE task_kind='priority' AND served_tier='remote' LIMIT 1",
    )
    .fetch_one(store.pool())
    .await
    .expect("discarded remote reply has an audit row");
    let audited = body_json(
        app.oneshot(get_req(&format!("/ai/runs/{run_id}")))
            .await
            .unwrap(),
    )
    .await;
    let calls = audited["calls"].as_array().expect("calls");
    assert!(
        calls.iter().any(|call| {
            call["task_kind"] == "priority"
                && call["ok"] == false
                && call["served_tier"] == "remote"
                && call["error_kind"] == "privacy_tripwire"
        }),
        "the dropped remote reply must be audited as privacy_tripwire: {calls:?}"
    );
}

/// This binary embeds the local-only manifest, so turning the executive
/// switch on must not make a remote-served reply authoritative.
#[tokio::test]
#[cfg(not(feature = "remote-allowed-manifest"))]
async fn remote_reply_stays_dropped_on_the_local_only_manifest() {
    let (app, store, task_id) = remote_eval_app("delegate").await;
    let enabled = app
        .clone()
        .oneshot(human_req(
            "PUT",
            "/settings/ai",
            json!({"executive_enabled": true}),
        ))
        .await
        .unwrap();
    assert_eq!(enabled.status(), StatusCode::OK);
    let response = app
        .clone()
        .oneshot(human_req(
            "POST",
            "/ai/priority/evaluate",
            json!({"task_ids": [task_id]}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let payload = body_json(response).await;
    assert!(
        payload["evaluated"].as_array().unwrap().is_empty(),
        "{payload}"
    );
    assert!(priority_of(&app, &task_id).await["ai"].is_null());
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sin90_ai_calls WHERE task_kind = 'priority' AND ok = 0 AND error_kind = 'privacy_tripwire'",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert!(
        rows >= 1,
        "the manifest gate must still audit the discarded reply"
    );
}

#[tokio::test]
#[cfg(feature = "remote-allowed-manifest")]
async fn remote_reply_is_stored_when_manifest_and_executive_allow_it() {
    let (app, _store, task_id) = remote_eval_app("delegate").await;
    let enabled = app
        .clone()
        .oneshot(human_req(
            "PUT",
            "/settings/ai",
            json!({"executive_enabled": true}),
        ))
        .await
        .unwrap();
    assert_eq!(enabled.status(), StatusCode::OK);
    let response = app
        .clone()
        .oneshot(human_req(
            "POST",
            "/ai/priority/evaluate",
            json!({"task_ids": [task_id]}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let payload = body_json(response).await;
    assert_eq!(payload["evaluated"][0]["task_id"], task_id);
    assert_eq!(payload["evaluated"][0]["ai"]["quadrant"], "delegate");
    assert_eq!(
        priority_of(&app, &task_id).await["ai"]["quadrant"],
        "delegate"
    );
}

#[tokio::test]
async fn evaluate_obeys_the_injected_hard_deadline_without_writing() {
    let store = Sin90Store::open_memory().await.unwrap();
    let entered = Arc::new(AtomicBool::new(false));
    let setup = app_with_model(
        store.clone(),
        Arc::new(ScriptedModel {
            tier: ServedTier::Local,
            body: model_items(&[]),
            prompts: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let task_id = create_task(&setup, "deadline").await;
    let app = app_with_model_deadline(
        store,
        Arc::new(HangingModel {
            entered: Arc::clone(&entered),
        }),
        Some(Duration::from_millis(50)),
    );
    let started = std::time::Instant::now();
    let outcome = tokio::time::timeout(
        Duration::from_secs(2),
        app.clone().oneshot(human_req(
            "POST",
            "/ai/priority/evaluate",
            json!({"task_ids": [task_id]}),
        )),
    )
    .await;
    let response = outcome
        .expect("evaluate must return at run_hard_deadline instead of hanging")
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the injected deadline is 50ms"
    );
    assert!(
        entered.load(Ordering::SeqCst),
        "the deadline must bound a model call that actually started"
    );
    assert_eq!(response.status(), StatusCode::OK);
    let payload = body_json(response).await;
    assert!(payload["evaluated"].as_array().unwrap().is_empty());
    assert_eq!(payload["unevaluated"][0]["task_id"], task_id);
    assert_eq!(payload["unevaluated"][0]["reason"], "model_error");
    let priority = priority_of(&app, &task_id).await;
    assert!(
        priority["ai"].is_null(),
        "a deadline must not persist a priority: {priority}"
    );
    assert_eq!(priority["evaluation_status"], "unevaluated");
}
