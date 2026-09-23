//! T5.1.1 (design §11.5) — `Sin90Store`'s implementation of the `ai` module's
//! entire I/O surface: [`AiReader`] (a read-only pool implementing
//! `ai::AiReadModel`) and `impl ai::AiSink for Sin90Store` (the dry-run
//! submit path, §11.4 公共's H3 mechanism).
//!
//! This file, not `src/ai/**`, is where `Sin90Store`/`sqlx`/raw SQL are
//! allowed to appear — the boundary check (`tests/ai_boundary.rs`, J7) only
//! walks `src/ai/**/*.rs`.
//!
//! **Capability → allowed ops, today (deliberate T5.1.1 narrowing).** The
//! design's `allowed_ops(cap)` (§11.4 公共) is `Classify ⇒
//! {AssignTaskDirection}`, `Summarize ⇒ {DraftReviewBody}`, `Propose ⇒
//! {CarryOverTask, ReorderTasks, CreateTasks}`. `AssignTaskDirection` and
//! `DraftReviewBody` do not exist on this branch yet (T5.2.1/T5.3.1 add
//! them) — so [`allowed_ops`] here answers "nothing" for `Classify` and
//! `Summarize` and the real three-op set for `Propose`. `submit`/`precheck`
//! for the first two capabilities will always be `SinkError::Invalid` until
//! those ops land; that is expected, not a bug this task should paper over.

use sqlx::{Acquire, Row, SqlitePool};

use crate::ai::ports::{
    AiCallRecord, AiReadModel, AiSettings, AiSink, Capability, DirectionCandidate, ProposalDraft,
    ReadError, SettingsRead, SinkError,
};
use crate::ai::source_for;
use crate::core::{
    validate, DirectionId, DirectionStatus, ProposalStatus, Review, Sin90Op, Sin90Proposal, Task,
    WeekId,
};
use crate::store::repo::{
    append_event, apply_op, build_snapshot, from_wire, row_to_review, row_to_task, to_wire,
    REVIEW_COLUMNS,
};
use crate::store::{Result as StoreResult, Sin90Store, StoreError};

/// `sin90_settings` key for the executive on/off switch (§11.3.2).
const EXECUTIVE_ENABLED_KEY: &str = "ai.executive_enabled";

/// The columns every `sin90_tasks` read in this file selects — same list
/// `row_to_task` expects, kept as one constant so the inbox/week-tasks
/// queries below can't drift apart from each other.
const TASK_COLUMNS: &str = "id, direction_id, week_id, parent_task_id, title, status, kind, \
     energy, est_minutes, carried_from, created_at, updated_at";

/// A read-only handle over `sin90.db` (§11.5 v2.1 M1): every connection in
/// its pool was opened with `pragma("query_only", "ON")` — see
/// `Sin90Store::ai_reader`'s doc for how the pool itself is built. `ai/`
/// never sees this type, only the `AiReadModel`/`SettingsRead` traits it
/// implements.
#[derive(Clone)]
pub struct AiReader(SqlitePool);

impl AiReader {
    pub(crate) fn new(pool: SqlitePool) -> Self {
        Self(pool)
    }

    #[cfg(test)]
    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.0
    }
}

fn rerr(e: impl std::fmt::Display) -> ReadError {
    ReadError(e.to_string())
}

impl SettingsRead for AiReader {
    async fn settings(&self) -> Result<AiSettings, ReadError> {
        read_ai_settings(&self.0).await
    }
}

impl AiReadModel for AiReader {
    async fn inbox(&self, limit: u32) -> Result<Vec<Task>, ReadError> {
        let rows = sqlx::query(&format!(
            "SELECT {TASK_COLUMNS} FROM sin90_tasks
             WHERE direction_id IS NULL AND status NOT IN ('done', 'dropped')
             ORDER BY created_at ASC
             LIMIT ?"
        ))
        .bind(limit)
        .fetch_all(&self.0)
        .await
        .map_err(rerr)?;
        rows.into_iter()
            .map(row_to_task)
            .collect::<StoreResult<_>>()
            .map_err(rerr)
    }

    async fn direction_candidates(&self, limit: u32) -> Result<Vec<DirectionCandidate>, ReadError> {
        let rows = sqlx::query(
            "SELECT d.id AS id, d.title AS title, d.status AS status, a.title AS area_title
             FROM sin90_directions d
             LEFT JOIN sin90_areas a ON a.id = d.area_id
             WHERE d.status NOT IN ('achieved', 'abandoned')
             ORDER BY d.updated_at DESC
             LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.0)
        .await
        .map_err(rerr)?;
        rows.into_iter()
            .map(|r| {
                Ok(DirectionCandidate {
                    direction_id: r.get::<String, _>("id"),
                    title: r.get("title"),
                    status: from_wire::<DirectionStatus>(&r.get::<String, _>("status"))
                        .map_err(rerr)?,
                    area_title: r.get("area_title"),
                })
            })
            .collect()
    }

    /// **T5.1.1 placeholder** (see this file's module doc): the frozen
    /// `normalize_title` algorithm (§11.4.1 R1) is a T5.2.1 deliverable
    /// (`src/ai/classify.rs`). Until then this compares each classified
    /// task's title, folded through the SAME coarse
    /// whitespace-collapse-and-lowercase here, against `normalized` taken
    /// as-is — correct as long as the caller normalizes its query the same
    /// way this folds candidates, wrong only in the sense that it is not yet
    /// the frozen algorithm's exact Unicode/CJK handling.
    async fn title_history(&self, normalized: &str) -> Result<Vec<DirectionId>, ReadError> {
        let rows = sqlx::query(
            "SELECT title, direction_id FROM sin90_tasks
             WHERE direction_id IS NOT NULL",
        )
        .fetch_all(&self.0)
        .await
        .map_err(rerr)?;
        let mut out = Vec::new();
        for r in rows {
            let title: String = r.get("title");
            if coarse_normalize(&title) == normalized {
                out.push(r.get::<String, _>("direction_id"));
            }
        }
        Ok(out)
    }

    async fn review(&self, id: &str) -> Result<Option<Review>, ReadError> {
        let row = sqlx::query(&format!(
            "SELECT {REVIEW_COLUMNS} FROM sin90_reviews WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(&self.0)
        .await
        .map_err(rerr)?;
        row.map(row_to_review).transpose().map_err(rerr)
    }

    async fn week_tasks(&self, week_id: &WeekId) -> Result<Vec<Task>, ReadError> {
        let rows = sqlx::query(&format!(
            "SELECT {TASK_COLUMNS} FROM sin90_tasks
             WHERE week_id = ?
             ORDER BY sort_key ASC, created_at ASC"
        ))
        .bind(week_id)
        .fetch_all(&self.0)
        .await
        .map_err(rerr)?;
        rows.into_iter()
            .map(row_to_task)
            .collect::<StoreResult<_>>()
            .map_err(rerr)
    }
}

/// Whitespace-collapse + lowercase — see [`AiReadModel::title_history`]'s doc.
fn coarse_normalize(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

async fn read_ai_settings(pool: &SqlitePool) -> Result<AiSettings, ReadError> {
    let row = sqlx::query("SELECT value FROM sin90_settings WHERE key = ?")
        .bind(EXECUTIVE_ENABLED_KEY)
        .fetch_optional(pool)
        .await
        .map_err(rerr)?;
    let executive_enabled = match row {
        None => false,
        Some(r) => {
            let raw: String = r.get("value");
            serde_json::from_str::<bool>(&raw).map_err(rerr)?
        }
    };
    Ok(AiSettings { executive_enabled })
}

/// §11.4 公共 H3 / §11.5: which `Sin90Op` variants a capability's proposals
/// may contain — checked BEFORE any write-lock is taken (a pure, cheap
/// gate). See this file's module doc for why `Classify`/`Summarize` allow
/// nothing yet.
fn allowed_ops(cap: Capability, ops: &[Sin90Op]) -> Result<(), SinkError> {
    if ops.is_empty() {
        return Err(SinkError::Invalid("proposal has no ops".into()));
    }
    let ok = |op: &Sin90Op| -> bool {
        match cap {
            Capability::Classify | Capability::Summarize => false,
            Capability::Propose => matches!(
                op,
                Sin90Op::CarryOverTask { .. }
                    | Sin90Op::ReorderTasks { .. }
                    | Sin90Op::CreateTasks { .. }
            ),
        }
    };
    if ops.iter().all(ok) {
        Ok(())
    } else {
        Err(SinkError::Invalid(format!(
            "one or more ops are not allowed for capability {}",
            cap.as_str()
        )))
    }
}

/// Steps 2–3 of `submit`/`precheck` (§11.4 公共): `build_snapshot` →
/// `validate` → SAVEPOINT `apply_op` × n → unconditional `ROLLBACK TO`. Never
/// commits anything — the SAVEPOINT is always rolled back regardless of
/// outcome (H3's "试跑"), so the caller's outer transaction is untouched by
/// this function either way.
async fn dry_run(
    tx: &mut crate::store::repo::Tx<'_>,
    id: &str,
    ops: &[Sin90Op],
    rationale: &Option<String>,
) -> Result<(), SinkError> {
    let snapshot = build_snapshot(tx, ops)
        .await
        .map_err(|e| SinkError::Store(e.to_string()))?;
    let proposal = Sin90Proposal {
        id: id.to_string(),
        status: ProposalStatus::Pending,
        // Unused by `validate` (it only inspects `.ops`); a real `source` is
        // derived later, after a successful dry run, from the call record.
        source: crate::core::ProposalSource::Rule,
        ops: ops.to_vec(),
        rationale: rationale.clone(),
    };
    validate(&proposal, &snapshot).map_err(|e| SinkError::Invalid(e.to_string()))?;

    let mut sp = tx
        .begin()
        .await
        .map_err(|e| SinkError::Store(e.to_string()))?; // SAVEPOINT
    let mut event_ids = Vec::new();
    let mut dry: Result<(), SinkError> = Ok(());
    for op in ops {
        if let Err(e) = apply_op(&mut sp, op, &mut event_ids).await {
            dry = Err(SinkError::Invalid(e.to_string()));
            break;
        }
    }
    sp.rollback()
        .await
        .map_err(|e| SinkError::Store(e.to_string()))?; // ROLLBACK TO SAVEPOINT — unconditional
    dry
}

impl AiSink for Sin90Store {
    async fn submit(
        &self,
        cap: Capability,
        draft: ProposalDraft,
        mut rec: AiCallRecord,
    ) -> Result<(), SinkError> {
        allowed_ops(cap, &draft.ops)?;

        let mut tx = self
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| SinkError::Store(e.to_string()))?;

        dry_run(&mut tx, &draft.id, &draft.ops, &draft.rationale).await?;

        let source = source_for(rec.engine, rec.served_tier);
        let source_wire = to_wire(&source).map_err(|e| SinkError::Store(e.to_string()))?;
        let ops_json =
            serde_json::to_string(&draft.ops).map_err(|e| SinkError::Store(e.to_string()))?;
        let now = crate::core::now_iso8601();

        sqlx::query(
            "INSERT INTO sin90_proposals (id, status, source, ops, rationale, created_at)
             VALUES (?, 'pending', ?, ?, ?, ?)",
        )
        .bind(&draft.id)
        .bind(&source_wire)
        .bind(&ops_json)
        .bind(&draft.rationale)
        .bind(&now)
        .execute(&mut *tx)
        .await
        .map_err(|e| SinkError::Store(e.to_string()))?;

        append_event(
            &mut tx,
            "proposal",
            &draft.id,
            "submitted",
            None,
            Some("pending"),
            &serde_json::json!({"id": draft.id, "source": source_wire, "ops_count": draft.ops.len()}),
            &now,
        )
        .await
        .map_err(|e| SinkError::Store(e.to_string()))?;

        rec.proposal_id = Some(draft.id.clone());
        insert_call_row(&mut *tx, &rec)
            .await
            .map_err(|e| SinkError::Store(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| SinkError::Store(e.to_string()))?;
        Ok(())
    }

    async fn record_call(&self, rec: AiCallRecord) -> Result<(), SinkError> {
        let mut conn = self
            .pool()
            .acquire()
            .await
            .map_err(|e| SinkError::Store(e.to_string()))?;
        insert_call_row(&mut *conn, &rec)
            .await
            .map_err(|e| SinkError::Store(e.to_string()))
    }

    async fn precheck(&self, cap: Capability, drafts: &[ProposalDraft]) -> Vec<bool> {
        let Ok(mut tx) = self.pool().begin_with("BEGIN IMMEDIATE").await else {
            return vec![false; drafts.len()];
        };
        let mut out = Vec::with_capacity(drafts.len());
        for d in drafts {
            let ok = allowed_ops(cap, &d.ops).is_ok()
                && dry_run(&mut tx, &d.id, &d.ops, &d.rationale).await.is_ok();
            out.push(ok);
        }
        // Never commits: this is a read (§11.4 公共's "去重" — one write-lock
        // acquisition, zero net writes).
        let _ = tx.rollback().await;
        out
    }
}

/// Shared by `submit` (inside its transaction) and `record_call` (its own
/// single-statement write) — same column list, same bind order.
async fn insert_call_row<'e, E>(executor: E, rec: &AiCallRecord) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query(
        "INSERT INTO sin90_ai_calls
             (id, run_id, task_kind, engine, fallback_from, served_tier, model_id,
              prompt_tokens, completion_tokens, latency_ms, ok, error_kind, proposal_id, at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&rec.id)
    .bind(&rec.run_id)
    .bind(rec.task_kind.as_str())
    .bind(rec.engine.as_str())
    .bind(rec.fallback_from.map(|e| e.as_str()))
    .bind(rec.served_tier.map(|t| t.as_str()))
    .bind(&rec.model_id)
    .bind(rec.prompt_tokens.map(|n| n as i64))
    .bind(rec.completion_tokens.map(|n| n as i64))
    .bind(rec.latency_ms as i64)
    .bind(rec.ok)
    .bind(rec.error_kind)
    .bind(&rec.proposal_id)
    .bind(&rec.at)
    .execute(executor)
    .await?;
    Ok(())
}

/// `PUT /settings/ai` (J10b): upsert `ai.executive_enabled` and mirror
/// `setting.changed`, same one-`BEGIN IMMEDIATE`-with-event convention every
/// other direct write in `repo.rs` uses.
impl Sin90Store {
    pub async fn get_ai_settings(&self) -> StoreResult<AiSettings> {
        read_ai_settings(self.pool())
            .await
            .map_err(|e| StoreError::Internal(e.0))
    }

    pub async fn put_ai_executive_enabled(&self, value: bool) -> StoreResult<AiSettings> {
        let now = crate::core::now_iso8601();
        let value_json = serde_json::to_string(&value)?;
        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "INSERT INTO sin90_settings (key, value, updated_at) VALUES (?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(EXECUTIVE_ENABLED_KEY)
        .bind(&value_json)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        append_event(
            &mut tx,
            "setting",
            EXECUTIVE_ENABLED_KEY,
            "changed",
            None,
            None,
            &serde_json::json!({"key": EXECUTIVE_ENABLED_KEY, "value": value}),
            &now,
        )
        .await?;
        tx.commit().await?;
        Ok(AiSettings {
            executive_enabled: value,
        })
    }
}

#[cfg(test)]
mod tests {
    //! J8 (adapted to T5.1.1's scope), J9, and the read-only-pool tests
    //! (§11.5 v2.1 M1) — see `tests/ai_boundary.rs`'s module doc for why
    //! these live here (needs `Sin90Store::pool()`, `pub(crate)`) rather
    //! than in an integration test.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::BTreeMap;

    use sqlx::sqlite::SqlitePool;

    use super::*;
    use crate::core::NewTask;

    fn rec(id: &str, run_id: &str, ok: bool, error_kind: Option<&'static str>) -> AiCallRecord {
        AiCallRecord {
            id: id.into(),
            run_id: run_id.into(),
            task_kind: Capability::Propose,
            engine: crate::ai::Engine::Reflex,
            fallback_from: None,
            served_tier: None,
            model_id: None,
            prompt_tokens: None,
            completion_tokens: None,
            latency_ms: 1,
            ok,
            error_kind,
            proposal_id: None,
            at: "2026-09-24T00:00:00Z".into(),
        }
    }

    /// Every table in the db, one canonical string per row (SQLite's own
    /// `quote()` — handles NULL/INTEGER/TEXT/BLOB uniformly), ordered by
    /// `rowid` so insertion order is stable. Schema-agnostic on purpose:
    /// this test must not need updating every time a column is added
    /// elsewhere.
    async fn snapshot_all_tables(pool: &SqlitePool) -> BTreeMap<String, Vec<String>> {
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
        )
        .fetch_all(pool)
        .await
        .unwrap();
        let mut out = BTreeMap::new();
        for t in tables {
            let cols: Vec<String> =
                sqlx::query_scalar(&format!("SELECT name FROM pragma_table_info('{t}')"))
                    .fetch_all(pool)
                    .await
                    .unwrap();
            let expr = cols
                .iter()
                .map(|c| format!("quote({c})"))
                .collect::<Vec<_>>()
                .join(" || '|' || ");
            let rows: Vec<String> =
                sqlx::query_scalar(&format!("SELECT {expr} AS r FROM {t} ORDER BY rowid"))
                    .fetch_all(pool)
                    .await
                    .unwrap();
            out.insert(t, rows);
        }
        out
    }

    fn diff_keys(
        before: &BTreeMap<String, Vec<String>>,
        after: &BTreeMap<String, Vec<String>>,
    ) -> Vec<String> {
        before
            .keys()
            .filter(|k| before.get(*k) != after.get(*k))
            .cloned()
            .collect()
    }

    /// Seeds one open week with two planned tasks, via the EXISTING (non-AI)
    /// proposal path — `AiSink`/`AiReadModel` must never be the only way to
    /// get fixture data into a test; that would make "did AI write?"
    /// unanswerable by construction.
    async fn seed_week_with_tasks(store: &Sin90Store) -> (String, String, String) {
        let week = store.create_week("2026-W40").await.unwrap();
        let p = Sin90Proposal {
            id: "seed-create-tasks".into(),
            status: ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![Sin90Op::CreateTasks {
                week_id: week.id.clone(),
                tasks: vec![
                    NewTask {
                        title: "t1".into(),
                        direction_id: None,
                    },
                    NewTask {
                        title: "t2".into(),
                        direction_id: None,
                    },
                ],
            }],
            rationale: None,
        };
        store.submit_proposal(&p).await.unwrap();
        store.apply_proposal(&p.id).await.unwrap();
        let tasks: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM sin90_tasks WHERE week_id = ? ORDER BY sort_key ASC",
        )
        .bind(&week.id)
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert_eq!(tasks.len(), 2);
        (week.id, tasks[0].clone(), tasks[1].clone())
    }

    #[tokio::test]
    async fn ai_boundary_tables_unchanged() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (week_id, t1, t2) = seed_week_with_tasks(&store).await;
        let pool = store.pool().clone();

        // ---- 1. every AiReadModel read + AiSink::precheck: reads alone
        //         must change NOTHING, including the three tables that ARE
        //         allowed to change from a real submit/record_call (they
        //         simply aren't touched by a pure read).
        let before = snapshot_all_tables(&pool).await;
        let reader = store.ai_reader();
        let _ = reader.settings().await.unwrap();
        let _ = reader.inbox(50).await.unwrap();
        let _ = reader.direction_candidates(50).await.unwrap();
        let _ = reader.title_history("t1").await.unwrap();
        let _ = reader.review("does-not-exist").await.unwrap();
        let _ = reader.week_tasks(&week_id).await.unwrap();
        let valid_draft = ProposalDraft {
            id: "precheck-1".into(),
            ops: vec![Sin90Op::ReorderTasks {
                week_id: week_id.clone(),
                order: vec![t2.clone(), t1.clone()],
            }],
            rationale: None,
        };
        let results = AiSink::precheck(
            &store,
            Capability::Propose,
            std::slice::from_ref(&valid_draft),
        )
        .await;
        assert_eq!(results, vec![true]);
        let after = snapshot_all_tables(&pool).await;
        assert_eq!(
            diff_keys(&before, &after),
            Vec::<String>::new(),
            "AiReadModel reads + AiSink::precheck must not change ANY table"
        );

        // ---- 2. AiSink::record_call (a failed, non-producing attempt)
        //         touches ONLY sin90_ai_calls.
        let before = after;
        AiSink::record_call(&store, rec("call-1", "run-1", false, Some("timeout")))
            .await
            .unwrap();
        let after = snapshot_all_tables(&pool).await;
        assert_eq!(
            diff_keys(&before, &after),
            vec!["sin90_ai_calls".to_string()],
            "record_call must touch ONLY sin90_ai_calls"
        );

        // ---- 3. AiSink::submit (H3's dry run: validate + SAVEPOINT
        //         apply_op + unconditional ROLLBACK TO) touches ONLY
        //         sin90_proposals, sin90_ai_calls, and sin90_events (the
        //         proposal.submitted row) — sin90_tasks is UNCHANGED even
        //         though the proposal's ops reorder it, because the apply
        //         itself was rolled back.
        let before = after;
        let produced = rec("call-2", "run-1", true, None);
        AiSink::submit(&store, Capability::Propose, valid_draft.clone(), produced)
            .await
            .unwrap();
        let after = snapshot_all_tables(&pool).await;
        let mut changed = diff_keys(&before, &after);
        changed.sort();
        assert_eq!(
            changed,
            vec![
                "sin90_ai_calls".to_string(),
                "sin90_events".to_string(),
                "sin90_proposals".to_string(),
            ],
            "submit's dry-run apply must leave sin90_tasks (and everything else) untouched"
        );
        assert_eq!(
            before.get("sin90_tasks"),
            after.get("sin90_tasks"),
            "the ReorderTasks op inside the submitted proposal must not have actually applied"
        );

        // ---- Positive control (this test's proof it isn't vacuous): NOW
        //      really accept the proposal — sin90_tasks MUST change,
        //      proving the diff above would have caught a real mutation had
        //      `submit`'s rollback been broken.
        let before = after;
        store.apply_proposal("precheck-1").await.unwrap();
        let after = snapshot_all_tables(&pool).await;
        assert_ne!(
            before.get("sin90_tasks"),
            after.get("sin90_tasks"),
            "accepting the proposal for real must change sin90_tasks — otherwise this \
             harness could never detect a real regression"
        );
    }

    /// `AiSink::submit` rejects an invalid proposal (an op referencing a
    /// task not in the target week) and leaves the store byte-for-byte
    /// unchanged — no proposal row, no `ok=1` call row, no event, `sort_key`
    /// untouched (§11.4 公共 H3; this branch's stand-in for T5.4.1's J21,
    /// using the ops that already exist today).
    #[tokio::test]
    async fn ai_sink_submit_rejects_invalid_and_leaves_store_unchanged() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (week_id, t1, _t2) = seed_week_with_tasks(&store).await;
        let pool = store.pool().clone();

        let before = snapshot_all_tables(&pool).await;
        let bad_draft = ProposalDraft {
            id: "bad-1".into(),
            ops: vec![Sin90Op::ReorderTasks {
                week_id: week_id.clone(),
                order: vec![t1.clone(), "does-not-exist".into()],
            }],
            rationale: None,
        };
        let err = AiSink::submit(
            &store,
            Capability::Propose,
            bad_draft,
            rec("call-x", "run-x", true, None),
        )
        .await;
        assert!(
            err.is_err(),
            "an op referencing a nonexistent task must be rejected"
        );
        let after = snapshot_all_tables(&pool).await;
        assert_eq!(
            before, after,
            "a rejected submit must leave the store byte-for-byte unchanged"
        );

        // Positive control: the same shape, but every task really is in the week.
        let good_draft = ProposalDraft {
            id: "good-1".into(),
            ops: vec![Sin90Op::ReorderTasks {
                week_id,
                order: vec![t1],
            }],
            rationale: None,
        };
        AiSink::submit(
            &store,
            Capability::Propose,
            good_draft,
            rec("call-y", "run-x", true, None),
        )
        .await
        .unwrap();
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_proposals WHERE id = 'good-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    /// J9: every row from one run shares `run_id`; the produced proposal
    /// has EXACTLY one `ok=1` call row pointing at it; every `ok=0` row has
    /// a non-empty `error_kind`. Mutation target: make `submit` insert the
    /// call row BEFORE the proposal row and fail the proposal insert — the
    /// call row would then exist with no matching proposal, which this
    /// test's join-count assertion would catch.
    #[tokio::test]
    async fn ai_calls_link_integrity() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (week_id, t1, t2) = seed_week_with_tasks(&store).await;
        let pool = store.pool().clone();
        let run_id = "run-integrity-1";

        AiSink::record_call(
            &store,
            rec("c1", run_id, false, Some("unavailable.no_provider")),
        )
        .await
        .unwrap();
        AiSink::record_call(&store, rec("c2", run_id, false, Some("undecided")))
            .await
            .unwrap();
        let draft = ProposalDraft {
            id: "p-integrity-1".into(),
            ops: vec![Sin90Op::ReorderTasks {
                week_id,
                order: vec![t2, t1],
            }],
            rationale: None,
        };
        AiSink::submit(
            &store,
            Capability::Propose,
            draft,
            rec("c3", run_id, true, None),
        )
        .await
        .unwrap();

        let run_ids: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT run_id FROM sin90_ai_calls WHERE run_id = ?")
                .bind(run_id)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(run_ids, vec![run_id.to_string()]);

        let ok_rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM sin90_ai_calls WHERE run_id = ? AND ok = 1")
                .bind(run_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(ok_rows, 1);

        let joined: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sin90_ai_calls c JOIN sin90_proposals p
             ON p.id = c.proposal_id WHERE c.run_id = ? AND c.ok = 1",
        )
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            joined, 1,
            "the ok=1 row's proposal_id must resolve to the proposal submit() inserted"
        );

        let bad_error_kind: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sin90_ai_calls WHERE run_id = ? AND ok = 0 AND error_kind IS NULL",
        )
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            bad_error_kind, 0,
            "every ok=0 row must have a non-null error_kind"
        );
    }

    /// The ai read model's write attempts fail with SQLite's `readonly`
    /// error — in BOTH the file/WAL mode a real deployment uses and the
    /// named shared-cache in-memory mode `open_memory` now uses for tests
    /// (§11.5 v2.1 M1). Mutation target: drop `pragma("query_only", "ON")`
    /// from `Sin90Store::open`/`open_memory` and both halves of this test
    /// go from `is_err()` to a successful write.
    #[tokio::test]
    async fn ai_reader_query_only_shared_cache_memory() {
        let store = Sin90Store::open_memory().await.unwrap();
        let reader_pool = store.ai_reader().pool().clone();
        // Reader sees the writer's data (same named shared-cache db).
        store.create_area("Work").await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_areas")
            .fetch_one(&reader_pool)
            .await
            .unwrap();
        assert_eq!(n, 1);
        // Reader cannot write.
        let err = sqlx::query(
            "INSERT INTO sin90_areas (id, title, slug, status, sort_key, created_at, updated_at)
             VALUES ('x','x','x','active',0,'t','t')",
        )
        .execute(&reader_pool)
        .await;
        assert!(
            err.unwrap_err()
                .to_string()
                .to_lowercase()
                .contains("readonly"),
            "a write through the ai reader pool must fail as readonly"
        );
        // Positive control: the writer pool still writes.
        store.create_area("Health").await.unwrap();
        let n2: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_areas")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(n2, 2);
    }

    #[tokio::test]
    async fn ai_reader_query_only_file_wal() {
        let dir = std::env::temp_dir().join(format!(
            "sin90-ai-boundary-{}-{}",
            std::process::id(),
            crate::core::ulid()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("sin90.db");
        let store = Sin90Store::open(&db_path).await.unwrap();
        let reader_pool = store.ai_reader().pool().clone();

        store.create_area("Work").await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_areas")
            .fetch_one(&reader_pool)
            .await
            .unwrap();
        assert_eq!(n, 1);
        let err = sqlx::query(
            "INSERT INTO sin90_areas (id, title, slug, status, sort_key, created_at, updated_at)
             VALUES ('x','x','x','active',0,'t','t')",
        )
        .execute(&reader_pool)
        .await;
        assert!(
            err.unwrap_err()
                .to_string()
                .to_lowercase()
                .contains("readonly"),
            "a write through the ai reader pool must fail as readonly"
        );
        store.create_area("Health").await.unwrap();
        let n2: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_areas")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(n2, 2);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
