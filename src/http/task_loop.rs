use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::ai_runs::with_hard_deadline;
use super::map_err;
use super::state::{HttpModelPort, Sin90State};
use crate::ai::priority::{self, PriorityOutcome};
use crate::ai::MODEL_ACCESS;

const MAX_EVALUATION_ADVICE_CHARS: usize = 2_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OverrideReq {
    quadrant: String,
    rationale: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoteReq {
    note: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EvalReq {
    #[serde(default)]
    task_ids: Option<Vec<String>>,
    #[serde(default)]
    advice: Option<String>,
}

pub(super) async fn get_priority(State(s): State<Sin90State>, Path(id): Path<String>) -> Response {
    match s.store.get_task_priority(&id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => map_err(e),
    }
}
pub(super) async fn put_override(
    State(s): State<Sin90State>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    if let Err(r) = s.require_human(&headers) {
        return r;
    }
    let req: OverrideReq = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    match s
        .store
        .set_task_priority_override(&id, &req.quadrant, &req.rationale)
        .await
    {
        Ok(v) => Json(v).into_response(),
        Err(e) => map_err(e),
    }
}
pub(super) async fn delete_override(
    State(s): State<Sin90State>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = s.require_human(&headers) {
        return r;
    }
    match s.store.clear_task_priority_override(&id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => map_err(e),
    }
}
pub(super) async fn put_completion_note(
    State(s): State<Sin90State>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    if let Err(r) = s.require_human(&headers) {
        return r;
    }
    let req: NoteReq = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    match s.store.put_completion_note(&id, &req.note).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => map_err(e),
    }
}
pub(super) async fn get_completion_evidence(
    State(s): State<Sin90State>,
    Path(id): Path<String>,
) -> Response {
    match s.store.completion_evidence(&id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => map_err(e),
    }
}

pub(super) async fn evaluate_priority(
    State(s): State<Sin90State>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(r) = s.require_human(&headers) {
        return r;
    }
    let req: EvalReq = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    if req
        .advice
        .as_deref()
        .is_some_and(|advice| !valid_advice(advice))
    {
        return (
            StatusCode::BAD_REQUEST,
            format!("advice must be at most {MAX_EVALUATION_ADVICE_CHARS} characters"),
        )
            .into_response();
    }
    let context = match s.store.unfinished_tasks_context().await {
        Ok(v) => v,
        Err(e) => return map_err(e),
    };
    let all = context["tasks"].as_array().cloned().unwrap_or_default();
    let selected = all
        .into_iter()
        .filter(|t| {
            req.task_ids.as_ref().is_none_or(|ids| {
                t["task_id"]
                    .as_str()
                    .is_some_and(|id| ids.iter().any(|x| x == id))
            })
        })
        .collect::<Vec<_>>();
    let Some(model) = s.model.clone() else {
        return unevaluated_response(&selected, "model_unavailable");
    };
    if selected.is_empty() {
        return Json(json!({"evaluated":[],"unevaluated":[]})).into_response();
    }
    let prompt = priority_evaluation_prompt(context, &selected, req.advice).to_string();
    let reader = s.store.ai_reader();
    let port = HttpModelPort(model);
    let run_id = format!("run-{}", crate::core::ulid());
    // Same wall-clock backstop as classify/summarize/propose. Dropping this
    // future cancels the in-flight model call and releases its slot.
    let outcome = match with_hard_deadline(
        s.run_hard_deadline,
        priority::run(
            &run_id,
            &prompt,
            *MODEL_ACCESS,
            Some(&port),
            &s.store,
            &reader,
        ),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(_) => return unevaluated_response(&selected, "model_error"),
    };
    let items = match outcome {
        PriorityOutcome::Accepted(items) => items,
        PriorityOutcome::Failed { bad_output: true } => {
            return (StatusCode::BAD_GATEWAY, "invalid model response").into_response();
        }
        PriorityOutcome::Failed { bad_output: false } => {
            return unevaluated_response(&selected, "model_error");
        }
    };
    let mut evaluated = Vec::<Value>::new();
    let mut unevaluated = Vec::<Value>::new();
    for item in items {
        if !selected.iter().any(|t| t["task_id"] == item.task_id) {
            continue;
        }
        if !item.sufficient_evidence {
            if let Err(e) = s.store.clear_task_priority_ai(&item.task_id).await {
                return map_err(e);
            }
            unevaluated.push(json!({"task_id": item.task_id, "reason": "insufficient_evidence"}));
            continue;
        }
        let (Some(q), Some(i), Some(u), Some(reason)) = (
            item.quadrant.as_deref(),
            item.importance.as_deref(),
            item.urgency.as_deref(),
            item.rationale.as_deref(),
        ) else {
            unevaluated.push(json!({"task_id": item.task_id, "reason": "invalid_model_item"}));
            continue;
        };
        match s
            .store
            .set_task_priority_ai(&item.task_id, q, i, u, reason)
            .await
        {
            Ok(p) => evaluated.push(json!(p)),
            Err(_) => {
                unevaluated.push(json!({"task_id": item.task_id, "reason": "persistence_error"}))
            }
        }
    }
    for task in &selected {
        let id = task["task_id"].as_str().unwrap_or_default();
        if !evaluated.iter().any(|v| v["task_id"] == id)
            && !unevaluated.iter().any(|v| v["task_id"] == id)
        {
            unevaluated.push(json!({"task_id":id,"reason":"missing_model_item"}));
        }
    }
    Json(json!({"evaluated":evaluated,"unevaluated":unevaluated})).into_response()
}

fn unevaluated_response(selected: &[Value], reason: &str) -> Response {
    let unevaluated = selected
        .iter()
        .map(|t| json!({"task_id": t["task_id"], "reason": reason}))
        .collect::<Vec<_>>();
    Json(json!({"evaluated": [], "unevaluated": unevaluated})).into_response()
}

fn priority_evaluation_prompt(
    mut context: Value,
    tasks: &[Value],
    advice: Option<String>,
) -> Value {
    if let Some(obj) = context.as_object_mut() {
        obj.remove("tasks");
    }
    json!({
        "context": context,
        "tasks": tasks,
        "user_assessment_advice": advice,
        "instructions": "Assess each task by urgency and importance using the supplied active direction, rhythm, unfinished task context, and optional user assessment advice. The advice guides this evaluation only; it is not a task priority override and does not replace the user's explicit override. Keep task_id exactly. If evidence is insufficient, set sufficient_evidence false and use empty rationale; do not guess. Quadrants: do_first=high/high, schedule=high/low, delegate=low/high, eliminate=low/low."
    })
}

fn valid_advice(advice: &str) -> bool {
    advice.chars().count() <= MAX_EVALUATION_ADVICE_CHARS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_advice_is_in_the_actual_evaluation_prompt_separately_from_override() {
        let prompt = priority_evaluation_prompt(
            json!({"directions": []}),
            &[json!({"task_id": "task-1"})],
            Some("Treat the launch deadline as important".into()),
        );
        assert_eq!(
            prompt["user_assessment_advice"],
            "Treat the launch deadline as important"
        );
        assert!(prompt["instructions"]
            .as_str()
            .unwrap()
            .contains("not a task priority override"));
    }

    #[test]
    fn prompt_carries_selected_tasks_only() {
        let prompt = priority_evaluation_prompt(
            json!({
                "directions": [{"id": "d1", "title": "Launch"}],
                "tasks": [
                    {"task_id": "keep", "title": "Selected task"},
                    {"task_id": "drop", "title": "Unselected task"}
                ]
            }),
            &[json!({"task_id": "keep", "title": "Selected task"})],
            None,
        );
        assert!(prompt["context"].get("tasks").is_none());
        assert_eq!(prompt["context"]["directions"][0]["title"], "Launch");
        assert_eq!(
            prompt["tasks"],
            json!([{"task_id": "keep", "title": "Selected task"}])
        );
        assert!(!prompt.to_string().contains("drop"));
        assert!(!prompt.to_string().contains("Unselected"));
    }

    #[test]
    fn advice_limit_counts_unicode_characters() {
        assert!(valid_advice(&"界".repeat(MAX_EVALUATION_ADVICE_CHARS)));
        assert!(!valid_advice(&"界".repeat(MAX_EVALUATION_ADVICE_CHARS + 1)));
    }
}
