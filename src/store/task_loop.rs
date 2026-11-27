//! Priority advice and retrospective completion notes. Completion remains the
//! existing task transition event; notes only enrich that event's read model.
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::core::now_iso8601;
use crate::store::{Result, Sin90Store, StoreError};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PriorityAssessment {
    pub quadrant: String,
    pub importance: String,
    pub urgency: String,
    pub rationale: String,
    pub evaluated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskPriority {
    pub task_id: String,
    pub evaluation_status: String,
    pub ai: Option<PriorityAssessment>,
    pub human_override: Option<PriorityOverride>,
    pub effective: Option<PriorityOverride>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PriorityOverride {
    pub quadrant: String,
    pub rationale: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompletionEvidence {
    pub task_id: String,
    pub title: String,
    pub completed_at: String,
    pub quadrant: Option<String>,
    pub note: Option<String>,
}

impl Sin90Store {
    pub async fn get_task_priority(&self, task_id: &str) -> Result<TaskPriority> {
        let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM sin90_tasks WHERE id=?")
            .bind(task_id)
            .fetch_optional(self.pool())
            .await?;
        if exists.is_none() {
            return Err(StoreError::NotFound(format!("task {task_id}")));
        }
        let row = sqlx::query("SELECT * FROM sin90_task_priorities WHERE task_id=?")
            .bind(task_id)
            .fetch_optional(self.pool())
            .await?;
        priority_from_row(task_id, row.as_ref())
    }

    pub async fn set_task_priority_ai(
        &self,
        task_id: &str,
        quadrant: &str,
        importance: &str,
        urgency: &str,
        rationale: &str,
    ) -> Result<TaskPriority> {
        validate_quadrant(quadrant)?;
        if !matches!(importance, "high" | "low") || !matches!(urgency, "high" | "low") {
            return Err(StoreError::Invalid(
                "importance and urgency must be high or low".into(),
            ));
        }
        let now = now_iso8601();
        sqlx::query("INSERT INTO sin90_task_priorities(task_id,ai_quadrant,ai_importance,ai_urgency,ai_rationale,ai_evaluated_at,updated_at) VALUES(?,?,?,?,?,?,?) ON CONFLICT(task_id) DO UPDATE SET ai_quadrant=excluded.ai_quadrant,ai_importance=excluded.ai_importance,ai_urgency=excluded.ai_urgency,ai_rationale=excluded.ai_rationale,ai_evaluated_at=excluded.ai_evaluated_at,updated_at=excluded.updated_at")
            .bind(task_id).bind(quadrant).bind(importance).bind(urgency).bind(rationale).bind(&now).bind(&now).execute(self.pool()).await?;
        self.get_task_priority(task_id).await
    }

    pub async fn clear_task_priority_ai(&self, task_id: &str) -> Result<TaskPriority> {
        sqlx::query("UPDATE sin90_task_priorities SET ai_quadrant=NULL,ai_importance=NULL,ai_urgency=NULL,ai_rationale=NULL,ai_evaluated_at=NULL,updated_at=? WHERE task_id=?")
            .bind(now_iso8601()).bind(task_id).execute(self.pool()).await?;
        self.get_task_priority(task_id).await
    }

    pub async fn set_task_priority_override(
        &self,
        task_id: &str,
        quadrant: &str,
        rationale: &str,
    ) -> Result<TaskPriority> {
        validate_quadrant(quadrant)?;
        let now = now_iso8601();
        sqlx::query("INSERT INTO sin90_task_priorities(task_id,override_quadrant,override_rationale,updated_at) VALUES(?,?,?,?) ON CONFLICT(task_id) DO UPDATE SET override_quadrant=excluded.override_quadrant,override_rationale=excluded.override_rationale,updated_at=excluded.updated_at")
            .bind(task_id).bind(quadrant).bind(rationale).bind(&now).execute(self.pool()).await?;
        self.get_task_priority(task_id).await
    }

    pub async fn clear_task_priority_override(&self, task_id: &str) -> Result<TaskPriority> {
        sqlx::query("UPDATE sin90_task_priorities SET override_quadrant=NULL,override_rationale=NULL,updated_at=? WHERE task_id=?")
            .bind(now_iso8601()).bind(task_id).execute(self.pool()).await?;
        self.get_task_priority(task_id).await
    }

    pub async fn unfinished_tasks_context(&self) -> Result<serde_json::Value> {
        let directions = sqlx::query("SELECT id,title,target_window FROM sin90_directions WHERE status='active' ORDER BY id")
            .fetch_all(self.pool()).await?.into_iter().map(|r| serde_json::json!({"id":r.get::<String,_>("id"),"title":r.get::<String,_>("title"),"target_window":r.get::<String,_>("target_window")})).collect::<Vec<_>>();
        let rhythms = sqlx::query("SELECT id,allocations FROM sin90_rhythms WHERE status IN ('active','adjusted') ORDER BY id")
            .fetch_all(self.pool()).await?.into_iter().map(|r| serde_json::json!({"id":r.get::<String,_>("id"),"allocations":serde_json::from_str::<serde_json::Value>(&r.get::<String,_>("allocations")).unwrap_or(serde_json::Value::Null)})).collect::<Vec<_>>();
        let tasks = sqlx::query("SELECT id,title,direction_id,status FROM sin90_tasks WHERE status NOT IN ('done','dropped') ORDER BY created_at,id")
            .fetch_all(self.pool()).await?.into_iter().map(|r| serde_json::json!({"task_id":r.get::<String,_>("id"),"title":r.get::<String,_>("title"),"direction_id":r.get::<Option<String>,_>("direction_id"),"status":r.get::<String,_>("status")})).collect::<Vec<_>>();
        Ok(serde_json::json!({"directions":directions,"rhythms":rhythms,"tasks":tasks}))
    }

    pub async fn put_completion_note(
        &self,
        task_id: &str,
        note: &str,
    ) -> Result<CompletionEvidence> {
        // Notes enrich an existing completion; they cannot create evidence
        // for a task that has not completed.
        self.completion_evidence(task_id).await?;
        let now = now_iso8601();
        sqlx::query("INSERT INTO sin90_completion_notes(task_id,note,updated_at) VALUES(?,?,?) ON CONFLICT(task_id) DO UPDATE SET note=excluded.note,updated_at=excluded.updated_at")
            .bind(task_id).bind(note).bind(now).execute(self.pool()).await?;
        self.completion_evidence(task_id).await
    }

    pub async fn completion_evidence(&self, task_id: &str) -> Result<CompletionEvidence> {
        let row = sqlx::query("SELECT entity_id, json_extract(payload,'$.title') AS title, at, json_extract(payload,'$.quadrant') AS quadrant FROM sin90_events WHERE entity='task' AND kind='transitioned' AND to_state='done' AND entity_id=? ORDER BY seq DESC LIMIT 1")
            .bind(task_id).fetch_optional(self.pool()).await?;
        let Some(row) = row else {
            return Err(StoreError::NotFound(format!(
                "completion for task {task_id}"
            )));
        };
        let note: Option<String> =
            sqlx::query_scalar("SELECT note FROM sin90_completion_notes WHERE task_id=?")
                .bind(task_id)
                .fetch_optional(self.pool())
                .await?;
        Ok(CompletionEvidence {
            task_id: task_id.into(),
            title: row.get::<Option<String>, _>("title").unwrap_or_default(),
            completed_at: row.get("at"),
            quadrant: row.get("quadrant"),
            note,
        })
    }

    pub async fn weekly_completed_tasks(
        &self,
        start: &str,
        end: &str,
    ) -> Result<Vec<CompletionEvidence>> {
        let rows = sqlx::query("SELECT entity_id, json_extract(payload,'$.title') AS title, at, json_extract(payload,'$.quadrant') AS quadrant FROM sin90_events WHERE entity='task' AND kind='transitioned' AND to_state='done' AND at>=? AND at<? ORDER BY at,seq")
            .bind(start).bind(end).fetch_all(self.pool()).await?;
        let mut out = Vec::new();
        for row in rows {
            let id: String = row.get("entity_id");
            let note: Option<String> =
                sqlx::query_scalar("SELECT note FROM sin90_completion_notes WHERE task_id=?")
                    .bind(&id)
                    .fetch_optional(self.pool())
                    .await?;
            out.push(CompletionEvidence {
                task_id: id,
                title: row.get::<Option<String>, _>("title").unwrap_or_default(),
                completed_at: row.get("at"),
                quadrant: row.get("quadrant"),
                note,
            });
        }
        Ok(out)
    }
}

fn validate_quadrant(q: &str) -> Result<()> {
    if matches!(q, "do_first" | "schedule" | "delegate" | "eliminate") {
        Ok(())
    } else {
        Err(StoreError::Invalid("invalid quadrant".into()))
    }
}
fn priority_from_row(task_id: &str, row: Option<&sqlx::sqlite::SqliteRow>) -> Result<TaskPriority> {
    // SQLite NULL decoded as String is "", so nullable columns must be Option.
    // A column or type error stays an error; only NULL/blank means absent.
    let (ai, human_override) = if let Some(row) = row {
        (assessment_from_row(row)?, override_from_row(row)?)
    } else {
        (None, None)
    };
    Ok(TaskPriority {
        task_id: task_id.into(),
        evaluation_status: if ai.is_some() {
            "evaluated"
        } else {
            "unevaluated"
        }
        .into(),
        effective: human_override.clone().or_else(|| {
            ai.as_ref().map(|a| PriorityOverride {
                quadrant: a.quadrant.clone(),
                rationale: a.rationale.clone(),
            })
        }),
        ai,
        human_override,
    })
}

fn nullable_text(row: &sqlx::sqlite::SqliteRow, column: &str) -> Result<Option<String>> {
    Ok(row.try_get(column)?)
}

fn present_text(row: &sqlx::sqlite::SqliteRow, column: &str) -> Result<Option<String>> {
    Ok(nullable_text(row, column)?.filter(|value| !value.is_empty()))
}

fn assessment_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Option<PriorityAssessment>> {
    let (Some(quadrant), Some(importance), Some(urgency), Some(evaluated_at)) = (
        present_text(row, "ai_quadrant")?,
        present_text(row, "ai_importance")?,
        present_text(row, "ai_urgency")?,
        present_text(row, "ai_evaluated_at")?,
    ) else {
        return Ok(None);
    };
    Ok(Some(PriorityAssessment {
        quadrant,
        importance,
        urgency,
        rationale: nullable_text(row, "ai_rationale")?.unwrap_or_default(),
        evaluated_at,
    }))
}

fn override_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Option<PriorityOverride>> {
    let Some(quadrant) = present_text(row, "override_quadrant")? else {
        return Ok(None);
    };
    Ok(Some(PriorityOverride {
        quadrant,
        rationale: nullable_text(row, "override_rationale")?.unwrap_or_default(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Energy, TaskKind, TaskStatus};

    #[tokio::test]
    async fn override_survives_ai_and_completion_note_enriches_single_event() {
        let dir = std::env::temp_dir().join(format!("sin90-loop-{}", crate::core::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Sin90Store::open(&dir.join("sin90.db")).await.unwrap();
        let task = store
            .create_task(
                "Temporary evidence task",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();
        let only = store
            .set_task_priority_override(&task.id, "schedule", "human choice")
            .await
            .unwrap();
        assert!(only.ai.is_none());
        assert_eq!(only.evaluation_status, "unevaluated");
        assert_eq!(only.human_override.as_ref().unwrap().quadrant, "schedule");
        assert_eq!(only.effective.as_ref().unwrap().quadrant, "schedule");
        let p = store
            .set_task_priority_ai(&task.id, "do_first", "high", "high", "model advice")
            .await
            .unwrap();
        assert_eq!(p.human_override.as_ref().unwrap().quadrant, "schedule");
        assert_eq!(p.effective.as_ref().unwrap().quadrant, "schedule");
        let cleared = store.clear_task_priority_override(&task.id).await.unwrap();
        assert!(cleared.human_override.is_none());
        assert_eq!(cleared.evaluation_status, "evaluated");
        assert_eq!(cleared.ai.as_ref().unwrap().quadrant, "do_first");
        assert_eq!(cleared.effective.as_ref().unwrap().quadrant, "do_first");
        store
            .set_task_priority_override(&task.id, "schedule", "human choice")
            .await
            .unwrap();
        store
            .transition_task(&task.id, TaskStatus::Planned)
            .await
            .unwrap();
        store
            .transition_task(&task.id, TaskStatus::InProgress)
            .await
            .unwrap();
        store
            .transition_task(&task.id, TaskStatus::Done)
            .await
            .unwrap();
        let before = store.completion_evidence(&task.id).await.unwrap();
        assert_eq!(before.quadrant.as_deref(), Some("schedule"));
        assert_eq!(before.note, None);
        store
            .put_completion_note(&task.id, "Recorded afterwards")
            .await
            .unwrap();
        let after = store.completion_evidence(&task.id).await.unwrap();
        assert_eq!(after.note.as_deref(), Some("Recorded afterwards"));
        assert_eq!(
            store
                .weekly_completed_tasks("0000-01-01", "9999-12-31")
                .await
                .unwrap()
                .iter()
                .filter(|x| x.task_id == task.id)
                .count(),
            1
        );
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
