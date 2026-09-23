//! T5.1.1 (design §11.5) — `Sin90Store`'s implementation of the `ai` module's
//! entire I/O surface: [`AiReader`] (a read-only pool implementing
//! `ai::AiReadModel`) and `impl ai::AiSink for Sin90Store` (the dry-run
//! submit path, §11.4 公共's H3 mechanism).
//!
//! This file, not `src/ai/**`, is where `Sin90Store`/`sqlx`/raw SQL are
//! allowed to appear — the boundary check (`tests/ai_boundary.rs`, J7) only
//! walks `src/ai/**/*.rs`.
//!
//! **Capability → allowed ops.** The design's `allowed_ops(cap)` (§11.4 公共)
//! is `Classify ⇒ {AssignTaskDirection}`, `Summarize ⇒ {DraftReviewBody}`,
//! `Propose ⇒ {CarryOverTask, ReorderTasks, CreateTasks}`. `AssignTaskDirection`
//! landed in T5.2.1 (this file's [`allowed_ops`] now answers the real
//! one-op set for `Classify`). `Sin90Op::DraftReviewBody` ALSO exists as of
//! T5.2.1 (design §11.2.3/§11.8's "一次加齐" — the Op's type/validate/apply
//! is added alongside `AssignTaskDirection` so `ValidationCtx` is widened
//! only once) — but the `summarize` CAPABILITY that will eventually PRODUCE
//! it is T5.3.1's job, explicitly out of this task's scope. So `allowed_ops`
//! deliberately still answers "nothing" for `Summarize`, and `submit`/
//! `precheck` for it will always be `SinkError::Invalid` until T5.3.1 opens
//! it; that is expected, not a bug this task should paper over.

use sqlx::{Acquire, Row, SqlitePool};

use crate::ai::ports::{
    AiCallRecord, AiReadModel, AiSettings, AiSink, Capability, DirectionCandidate, Engine,
    ProposalDraft, ReadError, SettingsRead, SinkError,
};
use crate::ai::source_for;
use crate::core::{
    direction_is_terminal, task_is_terminal, validate, DirectionId, DirectionStatus,
    ProposalStatus, Review, Sin90Op, Sin90Proposal, Task, TaskStatus, WeekId,
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

/// Every `TaskStatus` variant — kept as an explicit list (Rust has no enum
/// reflection) purely so [`terminal_task_status_wires`] can DERIVE its
/// exclusion set from [`task_is_terminal`] instead of maintaining a second,
/// driftable copy of "which statuses are terminal" as a hand-written SQL
/// literal (2026-09-24 review, H1). Mutation target: add a new terminal
/// `TaskStatus` variant to `core::types` without adding it here too — this
/// list stops being exhaustive and `inbox`'s exclusion set silently misses
/// it again.
const ALL_TASK_STATUSES: [TaskStatus; 6] = [
    TaskStatus::Backlog,
    TaskStatus::Planned,
    TaskStatus::InProgress,
    TaskStatus::Done,
    TaskStatus::Dropped,
    TaskStatus::CarriedOver,
];

/// The wire values of every TERMINAL `TaskStatus`, computed by asking
/// [`task_is_terminal`] about each known status — see [`ALL_TASK_STATUSES`]'s
/// doc for why this indirection exists.
fn terminal_task_status_wires() -> Vec<String> {
    ALL_TASK_STATUSES
        .iter()
        .copied()
        .filter(|&s| task_is_terminal(s))
        .map(|s| to_wire(&s).expect("TaskStatus always serializes"))
        .collect()
}

/// Every `DirectionStatus` variant — same reflection-substitute as
/// [`ALL_TASK_STATUSES`], for [`terminal_direction_status_wires`]
/// (2026-09-24 review round 2, #4).
const ALL_DIRECTION_STATUSES: [DirectionStatus; 5] = [
    DirectionStatus::Draft,
    DirectionStatus::Active,
    DirectionStatus::Paused,
    DirectionStatus::Achieved,
    DirectionStatus::Abandoned,
];

/// The wire values of every TERMINAL `DirectionStatus`, derived from
/// [`direction_is_terminal`] — `direction_candidates` and `title_history`
/// both exclude terminal Directions and must agree on what "terminal" means;
/// deriving it here (instead of two independent `NOT IN ('achieved',
/// 'abandoned')` literals) means a new terminal status added to
/// `core::transitions::direction_is_terminal` without updating
/// [`ALL_DIRECTION_STATUSES`] fails loudly (this list stops being
/// exhaustive) rather than silently leaving one of the two queries stale.
fn terminal_direction_status_wires() -> Vec<String> {
    ALL_DIRECTION_STATUSES
        .iter()
        .copied()
        .filter(|&s| direction_is_terminal(s))
        .map(|s| to_wire(&s).expect("DirectionStatus always serializes"))
        .collect()
}

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
    /// 2026-09-24 review (H1, blocking): the exclusion list is DERIVED from
    /// [`task_is_terminal`] (via [`terminal_task_status_wires`]), not a
    /// hand-maintained SQL literal — the ORIGINAL `NOT IN ('done', 'dropped')`
    /// forgot `carried_over`, so an already-closed, still-unclassified
    /// carried-over task (its ORIGINAL row: status flips to `carried_over`
    /// but `direction_id` is untouched by `CarryOverTask`'s apply) would sort
    /// into the inbox by `created_at ASC` FOREVER — permanently occupying a
    /// slot ahead of every real candidate once 20+ such rows accumulate,
    /// since nothing ever reclassifies a closed task. See
    /// `inbox_excludes_carried_over_tasks` for the regression.
    async fn inbox(&self, limit: u32) -> Result<Vec<Task>, ReadError> {
        let terminal = terminal_task_status_wires();
        let placeholders = terminal.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT {TASK_COLUMNS} FROM sin90_tasks
             WHERE direction_id IS NULL AND status NOT IN ({placeholders})
             ORDER BY created_at ASC
             LIMIT ?"
        );
        let mut q = sqlx::query(&sql);
        for t in &terminal {
            q = q.bind(t);
        }
        q = q.bind(limit);
        let rows = q.fetch_all(&self.0).await.map_err(rerr)?;
        rows.into_iter()
            .map(row_to_task)
            .collect::<StoreResult<_>>()
            .map_err(rerr)
    }

    /// New (2026-09-24 review, L4): a point lookup — "is task `id` CURRENTLY
    /// in the inbox?" — for `ai::classify::select_targets` to validate
    /// explicitly-given `task_ids` against, instead of paging through the
    /// entire inbox with an arbitrary large `limit` (T5.2.1's original
    /// `inbox(10_000)` placeholder). Shares the SAME terminal-status
    /// exclusion [`inbox`] uses.
    async fn inbox_task(&self, id: &str) -> Result<Option<Task>, ReadError> {
        let terminal = terminal_task_status_wires();
        let placeholders = terminal.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT {TASK_COLUMNS} FROM sin90_tasks
             WHERE id = ? AND direction_id IS NULL AND status NOT IN ({placeholders})"
        );
        let mut q = sqlx::query(&sql).bind(id);
        for t in &terminal {
            q = q.bind(t);
        }
        let row = q.fetch_optional(&self.0).await.map_err(rerr)?;
        row.map(row_to_task).transpose().map_err(rerr)
    }

    /// 2026-09-24 review (round 2, #4): the exclusion list is DERIVED from
    /// [`direction_is_terminal`] (via [`terminal_direction_status_wires`]),
    /// the same convention [`AiReadModel::inbox`]'s H1 fix established for
    /// task statuses — not a hand-maintained `NOT IN ('achieved',
    /// 'abandoned')` literal duplicated independently in this function and
    /// in `title_history` below.
    async fn direction_candidates(&self, limit: u32) -> Result<Vec<DirectionCandidate>, ReadError> {
        let terminal = terminal_direction_status_wires();
        let placeholders = terminal.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT d.id AS id, d.title AS title, d.status AS status, a.title AS area_title
             FROM sin90_directions d
             LEFT JOIN sin90_areas a ON a.id = d.area_id
             WHERE d.status NOT IN ({placeholders})
             ORDER BY d.updated_at DESC
             LIMIT ?"
        );
        let mut q = sqlx::query(&sql);
        for t in &terminal {
            q = q.bind(t);
        }
        q = q.bind(limit);
        let rows = q.fetch_all(&self.0).await.map_err(rerr)?;
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

    /// R1's history lookup (§11.4.1, T5.2.1): every ALREADY-classified task
    /// whose Direction is still non-terminal, normalized here through the
    /// **T5.2.1a placeholder** `coarse_normalize` (this branch has no
    /// `ai::classify` yet — the frozen `normalize_title` algorithm, §11.4.1
    /// R1, is `ai::classify`'s deliverable, T5.2.1b) and compared against
    /// `normalized` taken as-is — correct as long as the caller normalizes
    /// its query the same way this folds candidates. The non-terminal-
    /// Direction filter (the `JOIN` + `status NOT IN (...)`, exclusion set
    /// shared with `direction_candidates` above via
    /// [`terminal_direction_status_wires`]) is real already: "is this
    /// Direction still open" is a plain relational check that belongs here
    /// regardless of which normalization algorithm is doing the string
    /// comparison. See `r1_history_ignores_abandoned_direction` for the
    /// regression this filter guards against.
    async fn title_history(&self, normalized: &str) -> Result<Vec<DirectionId>, ReadError> {
        let terminal = terminal_direction_status_wires();
        let placeholders = terminal.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT t.title AS title, t.direction_id AS direction_id
             FROM sin90_tasks t
             JOIN sin90_directions d ON d.id = t.direction_id
             WHERE t.direction_id IS NOT NULL
               AND d.status NOT IN ({placeholders})"
        );
        let mut q = sqlx::query(&sql);
        for t in &terminal {
            q = q.bind(t);
        }
        let rows = q.fetch_all(&self.0).await.map_err(rerr)?;
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
            Capability::Classify => matches!(op, Sin90Op::AssignTaskDirection { .. }),
            Capability::Summarize => false,
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

/// 2026-09-24 review: `apply_op`'s dry run can fail two structurally
/// different ways, and conflating them was a bug. A relational/constraint
/// violation (task not in the target week, week not open, a CAS `UPDATE`
/// matching zero rows, an illegal transition, …) means the PROPOSAL is bad —
/// `SinkError::Invalid`, and the caller is meant to record this as
/// `error_kind = "rejected_by_precheck"` (§11.3.5). A transient
/// infrastructure failure (`SQLITE_BUSY` under contention, a raw I/O error,
/// a `serde_json` payload that fails to (de)serialize) says NOTHING about
/// whether the proposal itself is valid — recording that as
/// "rejected_by_precheck" would be actively misleading (the model didn't
/// produce bad output; the store just hiccuped). Those map to
/// `SinkError::Store` instead, same as every other infra failure in this
/// file.
fn classify_apply_err(e: StoreError) -> SinkError {
    match e {
        StoreError::NotFound(_)
        | StoreError::Conflict(_)
        | StoreError::WeekNotOpen(_)
        | StoreError::SameWeekCarry(_)
        | StoreError::Invalid(_)
        | StoreError::Transition(_)
        | StoreError::Proposal(_) => SinkError::Invalid(e.to_string()),
        StoreError::Sqlx(_)
        | StoreError::Migrate(_)
        | StoreError::Serde(_)
        | StoreError::Internal(_) => SinkError::Store(e.to_string()),
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
            dry = Err(classify_apply_err(e));
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
        // 2026-09-24 review: `submit` is where a call record turns into a
        // committed `ok=1` proposal row — it must not blindly trust what the
        // caller put in `rec`. Two defenses:
        //   1. `rec.engine == Reflex` together with a `served_tier` is a
        //      contradiction (reflex never talks to a model, so it can never
        //      have a served tier) — reject rather than silently store
        //      nonsense that `source_for` would then derive a wrong
        //      `source` from.
        //   2. Regardless of what `rec.ok`/`rec.error_kind` said coming in,
        //      the row `submit` inserts is ALWAYS `ok=1, error_kind=NULL` —
        //      by definition, reaching this point (past the dry run below)
        //      means this attempt produced the proposal being submitted.
        if rec.engine == Engine::Reflex && rec.served_tier.is_some() {
            return Err(SinkError::Invalid(
                "a reflex call record cannot carry a served_tier".into(),
            ));
        }

        let mut tx = self
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| SinkError::Store(e.to_string()))?;

        dry_run(&mut tx, &draft.id, &draft.ops, &draft.rationale).await?;
        rec.ok = true;
        rec.error_kind = None;

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
    use crate::core::{Energy, NewTask, TaskKind};

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
    ///
    /// 2026-09-24 review (M1): `sin90_events` is split into TWO keys —
    /// `entity = 'proposal'` rows and everything else — instead of one
    /// blob. §11.5's own exclusion list names exactly three things a
    /// capability run may change: `sin90_proposals`, `sin90_ai_calls`, and
    /// `sin90_events WHERE entity = 'proposal'`. A single `sin90_events` key
    /// could not tell "the expected proposal.submitted row landed" apart
    /// from "something ALSO wrote a task/direction/review event it had no
    /// business writing" — both just say "sin90_events changed". Splitting
    /// the key lets `ai_boundary_tables_unchanged` assert the narrower,
    /// actually-designed claim: the `entity <> 'proposal'` half must NEVER
    /// change from anything in `ai/`'s reach.
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
            if t == "sin90_events" {
                for (key, where_clause) in [
                    ("sin90_events(entity=proposal)", "WHERE entity = 'proposal'"),
                    (
                        "sin90_events(entity<>proposal)",
                        "WHERE entity <> 'proposal'",
                    ),
                ] {
                    let rows: Vec<String> = sqlx::query_scalar(&format!(
                        "SELECT {expr} AS r FROM {t} {where_clause} ORDER BY rowid"
                    ))
                    .fetch_all(pool)
                    .await
                    .unwrap();
                    out.insert(key.to_string(), rows);
                }
                continue;
            }
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
        let _ = reader.inbox_task(&t1).await.unwrap();
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
        //         sin90_proposals, sin90_ai_calls, and the
        //         `entity = 'proposal'` half of sin90_events (the
        //         proposal.submitted row) — sin90_tasks is UNCHANGED even
        //         though the proposal's ops reorder it, because the apply
        //         itself was rolled back, and the `entity <> 'proposal'`
        //         half of sin90_events is untouched too (M1: this is the
        //         narrower claim the design's 3-item exclusion list
        //         actually makes, not just "sin90_events changed somehow").
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
                "sin90_events(entity=proposal)".to_string(),
                "sin90_proposals".to_string(),
            ],
            "submit's dry-run apply must leave sin90_tasks (and everything else, including \
             non-proposal events) untouched"
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

    // ---- 2026-09-24 review: submit doesn't trust the caller's rec --------

    /// `submit` writes `ok=1, error_kind=NULL` REGARDLESS of what the caller
    /// put in `rec` — reaching past the dry run means this attempt produced
    /// the proposal. Mutation target: remove the `rec.ok = true; rec.error_kind
    /// = None;` overwrite in `submit` and this goes red (the row keeps the
    /// caller's `ok=0, error_kind=Some("bogus")`).
    #[tokio::test]
    async fn ai_sink_submit_always_writes_ok_true_and_null_error_kind() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (week_id, t1, t2) = seed_week_with_tasks(&store).await;
        let draft = ProposalDraft {
            id: "p-forced-ok".into(),
            ops: vec![Sin90Op::ReorderTasks {
                week_id,
                order: vec![t2, t1],
            }],
            rationale: None,
        };
        // A caller that (wrongly) claims failure — submit must not trust it.
        let mut bogus = rec("call-forced", "run-forced", false, Some("bogus"));
        bogus.proposal_id = None;
        AiSink::submit(&store, Capability::Propose, draft, bogus)
            .await
            .unwrap();

        let (ok, error_kind): (bool, Option<String>) =
            sqlx::query_as("SELECT ok, error_kind FROM sin90_ai_calls WHERE id = 'call-forced'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert!(
            ok,
            "submit must force ok=1 regardless of what the caller passed"
        );
        assert_eq!(
            error_kind, None,
            "submit must force error_kind=NULL regardless of what the caller passed"
        );
    }

    /// `engine == Reflex` with a non-`None` `served_tier` is a contradiction
    /// (reflex never talks to a model) — `submit` rejects it before even
    /// opening a transaction. Mutation target: remove the guard in `submit`
    /// and this goes from `is_err()` to succeeding (and `source_for` would
    /// then derive a nonsensical `source` from the bogus tier).
    #[tokio::test]
    async fn ai_sink_submit_rejects_reflex_with_served_tier() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (week_id, t1, t2) = seed_week_with_tasks(&store).await;
        let draft = ProposalDraft {
            id: "p-bad-combo".into(),
            ops: vec![Sin90Op::ReorderTasks {
                week_id,
                order: vec![t2, t1],
            }],
            rationale: None,
        };
        let mut bad = rec("call-bad-combo", "run-bad-combo", true, None);
        bad.served_tier = Some(crate::ai::ports::ServedTier::Local);
        let err = AiSink::submit(&store, Capability::Propose, draft, bad).await;
        assert!(
            err.is_err(),
            "reflex + served_tier is a contradiction and must be rejected"
        );
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM sin90_proposals WHERE id = 'p-bad-combo'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(
            n, 0,
            "the rejected submit must not have written a proposal row"
        );
    }

    /// A relational/constraint error from `apply_op` (task not in the
    /// target week) classifies as `SinkError::Invalid` — the caller is
    /// meant to record this as `rejected_by_precheck`; a raw infrastructure
    /// error (SQLite busy, I/O, a broken internal invariant) classifies as
    /// `SinkError::Store` instead, since it says nothing about whether the
    /// PROPOSAL was valid. Mutation target: collapse `classify_apply_err` to
    /// always return `SinkError::Invalid` and the second assertion goes red
    /// (matching on `SinkError::Store` would then fail).
    #[test]
    fn classify_apply_err_distinguishes_constraint_from_infra_errors() {
        assert!(matches!(
            classify_apply_err(StoreError::NotFound("task x".into())),
            SinkError::Invalid(_)
        ));
        assert!(matches!(
            classify_apply_err(StoreError::WeekNotOpen("w".into())),
            SinkError::Invalid(_)
        ));
        assert!(matches!(
            classify_apply_err(StoreError::Conflict("dup".into())),
            SinkError::Invalid(_)
        ));
        assert!(matches!(
            classify_apply_err(StoreError::Internal("broken invariant".into())),
            SinkError::Store(_)
        ));
        assert!(matches!(
            classify_apply_err(StoreError::Sqlx(sqlx::Error::PoolClosed)),
            SinkError::Store(_)
        ));
    }

    /// H1 (2026-09-24 review, blocking): a task that got carried over WHILE
    /// still unclassified — its ORIGINAL row flips to `carried_over` but
    /// `CarryOverTask`'s apply never touches `direction_id` — must NOT show
    /// up in the inbox (it is closed, historical; nothing will ever classify
    /// it again). The NEW task the carry-over produces (still unclassified,
    /// still open) DOES belong in the inbox. Mutation target: revert
    /// `inbox`'s exclusion list to the literal `('done', 'dropped')` this
    /// replaces and the first assertion goes red.
    #[tokio::test]
    async fn inbox_excludes_carried_over_tasks() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (_week, t1, t2) = seed_week_with_tasks(&store).await; // both planned, direction_id NULL
        let next_week = store.create_week("2026-W41").await.unwrap();

        let carry = Sin90Proposal {
            id: "carry-1".into(),
            status: ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![Sin90Op::CarryOverTask {
                task_id: t1.clone(),
                to_week: next_week.id.clone(),
            }],
            rationale: None,
        };
        store.submit_proposal(&carry).await.unwrap();
        store.apply_proposal("carry-1").await.unwrap();

        let child: String = sqlx::query_scalar("SELECT id FROM sin90_tasks WHERE carried_from = ?")
            .bind(&t1)
            .fetch_one(store.pool())
            .await
            .unwrap();

        let reader = store.ai_reader();
        let inbox_ids: Vec<String> = reader
            .inbox(50)
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert!(
            !inbox_ids.contains(&t1),
            "the CLOSED (carried_over) original task must not be in the inbox: {inbox_ids:?}"
        );
        assert!(
            inbox_ids.contains(&t2),
            "an untouched, still-open unclassified task must still be in the inbox"
        );
        assert!(
            inbox_ids.contains(&child),
            "the NEW task the carry-over produced is still open and unclassified"
        );

        // Positive control for `inbox_task` (L4's point lookup): the closed
        // task is unusable as an explicit classify target; the new one isn't.
        assert!(reader.inbox_task(&t1).await.unwrap().is_none());
        assert!(reader.inbox_task(&child).await.unwrap().is_some());
    }

    /// 2026-09-24 review (round 2, #4): `title_history` must exclude a
    /// classified task whose Direction has since been abandoned — feeding
    /// R1 (`ai::classify::r1_reflex`, T5.2.1b) an empty history for that
    /// title, which R1's own contract reads as "undecided" (able to degrade
    /// to the model or fall back to R2), not as "decisively points nowhere".
    /// Mutation target: delete the `terminal_direction_status_wires()`
    /// filtering (revert to no `WHERE d.status NOT IN (...)` clause at all)
    /// and this goes red — the abandoned Direction's id would come back.
    #[tokio::test]
    async fn r1_history_ignores_abandoned_direction() {
        let store = Sin90Store::open_memory().await.unwrap();
        let direction = store
            .create_direction("Side quest", "2026-Q4", None)
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
        let reader = store.ai_reader();

        // Positive control: while the Direction is still open, history sees it.
        let before = reader.title_history("write the report").await.unwrap();
        assert_eq!(before, vec![direction.id.clone()]);

        // Abandon it (no direct-write route/Op exists for this in this
        // branch — see `abandon_direction`'s doc for why raw SQL is the
        // deliberate choice here, same convention `ai::classify`'s own
        // fixtures use for this exact gap).
        sqlx::query("UPDATE sin90_directions SET status = 'abandoned' WHERE id = ?")
            .bind(&direction.id)
            .execute(store.pool())
            .await
            .unwrap();

        let after = reader.title_history("write the report").await.unwrap();
        assert!(
            after.is_empty(),
            "a classified-but-now-abandoned Direction must not show up in R1's history: {after:?}"
        );
    }
}
