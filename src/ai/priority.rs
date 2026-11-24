//! Priority advice through the same engine ladder as the other capabilities.
//! A remote reply is discarded when the manifest is local-only or the
//! executive switch is off. The producing call is recorded here; priority
//! rows themselves are written by the HTTP caller, which also leaves any
//! human override untouched.

use serde_json::{json, Map, Value};

use super::{
    plan, run_item, AiSettings, AiSink, Capability, Complexity, Engine, ModelAccess, ModelMessage,
    ModelPort, ModelReply, ModelRequest, Outcome, ReadError, Role, RunState, SettingsRead,
};

const SYSTEM: &str =
    "You provide cautious task priority advice. Never make changes or override human choices.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorityItem {
    pub task_id: String,
    pub quadrant: Option<String>,
    pub importance: Option<String>,
    pub urgency: Option<String>,
    pub rationale: Option<String>,
    pub sufficient_evidence: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PriorityOutcome {
    Accepted(Vec<PriorityItem>),
    /// `bad_output` is the ladder's "the model replied and parse rejected it".
    Failed {
        bad_output: bool,
    },
}

/// Reports the executive switch as off when this binary's manifest is
/// local-only, so [`super::tripwire`] discards a remote-served reply. This
/// does not write `ai.executive_enabled`.
struct ManifestGoverned<'a, R> {
    inner: &'a R,
    access: ModelAccess,
}

impl<R: SettingsRead> SettingsRead for ManifestGoverned<'_, R> {
    async fn settings(&self) -> Result<AiSettings, ReadError> {
        let mut settings = self.inner.settings().await?;
        if self.access != ModelAccess::RemoteAllowed {
            settings.executive_enabled = false;
        }
        Ok(settings)
    }
}

pub async fn run<M, S, R>(
    run_id: &str,
    user_prompt: &str,
    access: ModelAccess,
    model: Option<&M>,
    sink: &S,
    settings: &R,
) -> PriorityOutcome
where
    M: ModelPort,
    S: AiSink,
    R: SettingsRead,
{
    let governed = ManifestGoverned {
        inner: settings,
        access,
    };
    let now = governed.settings().await.unwrap_or_default();
    let steps = plan(Capability::Priority, access, now, model.is_some());
    let mut state = RunState::new(std::time::Instant::now());
    let outcome = run_item(
        run_id,
        Capability::Priority,
        &steps,
        &mut state,
        model,
        sink,
        &governed,
        |engine| request(user_prompt, engine),
        parse_items,
        || None,
        || None,
        crate::core::now_iso8601,
        crate::core::ulid,
    )
    .await;
    match outcome {
        Outcome::Produced { value, rec, .. } => {
            if let Err(e) = sink.record_call(rec).await {
                tracing::warn!(
                    error = %e,
                    run_id,
                    "priority: failed to record the producing call"
                );
            }
            PriorityOutcome::Accepted(value)
        }
        Outcome::Nothing { deterministic } => PriorityOutcome::Failed {
            bad_output: deterministic,
        },
        Outcome::Deferred | Outcome::Aborted => PriorityOutcome::Failed { bad_output: false },
    }
}

fn request(prompt: &str, engine: Engine) -> ModelRequest {
    ModelRequest {
        messages: vec![
            ModelMessage {
                role: Role::System,
                content: SYSTEM.into(),
            },
            ModelMessage {
                role: Role::User,
                content: prompt.to_string(),
            },
        ],
        schema_name: "sin90_task_priority",
        schema: schema(),
        max_tokens: 2000,
        complexity: if engine == Engine::Executive {
            Complexity::Complex
        } else {
            Complexity::Simple
        },
    }
}

fn schema() -> Map<String, Value> {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["items"],
        "properties": {
            "items": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["task_id", "quadrant", "importance", "urgency", "rationale", "sufficient_evidence"],
                    "properties": {
                        "task_id": {"type": "string"},
                        "quadrant": {"type": "string", "enum": ["do_first", "schedule", "delegate", "eliminate"]},
                        "importance": {"type": "string", "enum": ["high", "low"]},
                        "urgency": {"type": "string", "enum": ["high", "low"]},
                        "rationale": {"type": "string"},
                        "sufficient_evidence": {"type": "boolean"}
                    }
                }
            }
        }
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

fn parse_items(reply: &ModelReply) -> Result<Vec<PriorityItem>, &'static str> {
    let value: Value = serde_json::from_str(&reply.text).map_err(|_| "bad_output")?;
    let Some(items) = value.get("items").and_then(Value::as_array) else {
        return Err("bad_output");
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Some(task_id) = item.get("task_id").and_then(Value::as_str) else {
            continue;
        };
        out.push(PriorityItem {
            task_id: task_id.to_string(),
            quadrant: item
                .get("quadrant")
                .and_then(Value::as_str)
                .map(str::to_string),
            importance: item
                .get("importance")
                .and_then(Value::as_str)
                .map(str::to_string),
            urgency: item
                .get("urgency")
                .and_then(Value::as_str)
                .map(str::to_string),
            rationale: item
                .get("rationale")
                .and_then(Value::as_str)
                .map(str::to_string),
            sufficient_evidence: item.get("sufficient_evidence") == Some(&Value::Bool(true)),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::ai::{ModelFailure, NoModelPort, ServedTier};
    use crate::store::Sin90Store;

    struct Switch(bool);
    impl SettingsRead for Switch {
        async fn settings(&self) -> Result<AiSettings, ReadError> {
            Ok(AiSettings {
                executive_enabled: self.0,
            })
        }
    }

    struct Script {
        text: String,
        tier: ServedTier,
        seen: Mutex<String>,
    }
    impl ModelPort for Script {
        async fn complete(&self, req: ModelRequest) -> Result<ModelReply, ModelFailure> {
            if let Some(user) = req.messages.iter().find(|m| m.role == Role::User) {
                *self.seen.lock().expect("seen") = user.content.clone();
            }
            Ok(ModelReply {
                text: self.text.clone(),
                model_id: Some("stub".into()),
                tier: self.tier,
                prompt_tokens: Some(3),
                completion_tokens: Some(2),
            })
        }
    }

    fn reply(task_id: &str) -> String {
        json!({
            "items": [{
                "task_id": task_id,
                "quadrant": "do_first",
                "importance": "high",
                "urgency": "high",
                "rationale": "remote judgment",
                "sufficient_evidence": true
            }]
        })
        .to_string()
    }

    async fn calls(store: &Sin90Store) -> Vec<(String, bool, Option<String>)> {
        sqlx::query_as("SELECT task_kind, ok, error_kind FROM sin90_ai_calls ORDER BY rowid")
            .fetch_all(store.pool())
            .await
            .unwrap()
    }

    async fn evaluate(
        access: ModelAccess,
        enabled: bool,
        tier: ServedTier,
        text: &str,
    ) -> (PriorityOutcome, Vec<(String, bool, Option<String>)>, String) {
        let store = Sin90Store::open_memory().await.unwrap();
        let model = Script {
            text: text.into(),
            tier,
            seen: Mutex::new(String::new()),
        };
        let outcome = run(
            "run-priority",
            "only-the-selected-tasks",
            access,
            Some(&model),
            &store,
            &Switch(enabled),
        )
        .await;
        let seen = model.seen.lock().expect("seen").clone();
        (outcome, calls(&store).await, seen)
    }

    #[tokio::test]
    async fn remote_reply_is_discarded_when_executive_is_disabled() {
        let (outcome, rows, _) = evaluate(
            ModelAccess::RemoteAllowed,
            false,
            ServedTier::Remote,
            &reply("task-1"),
        )
        .await;
        assert_eq!(outcome, PriorityOutcome::Failed { bad_output: false });
        assert!(rows.iter().any(|row| {
            row.0 == "priority" && !row.1 && row.2.as_deref() == Some("privacy_tripwire")
        }));
        assert!(rows.iter().all(|row| row.0 == "priority"));
    }

    #[tokio::test]
    async fn remote_reply_is_discarded_when_manifest_is_local_only() {
        let (outcome, rows, _) = evaluate(
            ModelAccess::LocalOnly,
            true,
            ServedTier::Remote,
            &reply("task-1"),
        )
        .await;
        assert_eq!(outcome, PriorityOutcome::Failed { bad_output: false });
        assert!(rows.iter().any(|row| {
            row.0 == "priority" && !row.1 && row.2.as_deref() == Some("privacy_tripwire")
        }));
    }

    #[tokio::test]
    async fn allowed_reply_is_audited_as_a_successful_priority_call() {
        let (outcome, rows, seen) = evaluate(
            ModelAccess::RemoteAllowed,
            true,
            ServedTier::Remote,
            &reply("task-1"),
        )
        .await;
        let PriorityOutcome::Accepted(items) = outcome else {
            panic!("expected the governed reply to be accepted, got {outcome:?}");
        };
        assert_eq!(items[0].task_id, "task-1");
        assert_eq!(seen, "only-the-selected-tasks");
        assert!(rows
            .iter()
            .any(|row| row.0 == "priority" && row.1 && row.2.is_none()));
    }

    #[tokio::test]
    async fn invalid_json_is_a_failed_priority_call() {
        let (outcome, rows, _) =
            evaluate(ModelAccess::LocalOnly, false, ServedTier::Local, "not-json").await;
        assert_eq!(outcome, PriorityOutcome::Failed { bad_output: true });
        assert!(rows
            .iter()
            .any(|row| row.0 == "priority" && !row.1 && row.2.as_deref() == Some("bad_output")));
    }

    #[tokio::test]
    async fn no_model_port_records_the_reflex_miss_and_does_not_invent_advice() {
        let store = Sin90Store::open_memory().await.unwrap();
        let outcome = run(
            "run-none",
            "prompt",
            ModelAccess::LocalOnly,
            None::<&NoModelPort>,
            &store,
            &Switch(false),
        )
        .await;
        assert_eq!(outcome, PriorityOutcome::Failed { bad_output: false });
        let rows = calls(&store).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].2.as_deref(), Some("no_match"));
    }
}
