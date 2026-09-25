//! Sin90 persistence — sqlx over its OWN `sin90.db` (never a kernel database).
//!
//! Mirrors the ported discipline: every status change runs inside a
//! `BEGIN IMMEDIATE` transaction — current state is read under the write lock,
//! checked against `core::transitions`'s matrix, then updated, and an event is
//! appended in the SAME transaction. Proposal apply is CAS-idempotent
//! (pending → applying → applied) so a re-tried accept never applies twice.
//!
//! Ported from Agent24's `agent24-sin90-store` crate (design §1.2/§4.1). This
//! port DROPS `open_migrating_from` and its legacy-path probing: that method
//! existed to migrate a pre-ME-1b-b `~/.agent24/sin90.db` — a fact about
//! Agent24's own history, not something a fresh standalone package has. `open`
//! and `open_memory` are unchanged.

pub mod attention;
pub mod packs;
pub mod repo;

pub use attention::{AttentionRow, WeekAttention};
pub use packs::{five_life_systems, SeedArea};
pub use repo::{AppliedProposal, ApplyOutcome, EventRow, RoutineUpdate, StoredProposal, TodayView};

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::path::Path;
use std::str::FromStr;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("serialization: {0}")]
    Serde(#[from] serde_json::Error),
    #[error(transparent)]
    Transition(#[from] crate::core::TransitionError),
    #[error(transparent)]
    Proposal(#[from] crate::core::ProposalError),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    /// Relational invariants the pure validator cannot see (ValidationCtx is
    /// per-entity), enforced here under the write lock: mutating a task whose
    /// week is not open (planning|active) ...
    #[error("task {0}'s week is not open; cannot mutate")]
    WeekNotOpen(String),
    /// ... or carrying a task over into the very week it already lives in
    /// (which would strand the closed task and spawn an endlessly re-carryable
    /// duplicate in the same week).
    #[error("cannot carry task {0} into its own week")]
    SameWeekCarry(String),
    /// Client input that is well-formed JSON but not a valid value (e.g. a
    /// malformed ISO week label) — maps to 400.
    #[error("invalid: {0}")]
    Invalid(String),
    /// A broken internal invariant (not the client's fault) — maps to 500.
    #[error("internal: {0}")]
    Internal(String),
}

impl StoreError {
    /// True if this is a FOREIGN KEY violation (SQLite extended code 787) —
    /// i.e. the client referenced an entity that doesn't exist, a 4xx not a 5xx.
    pub fn is_fk_violation(&self) -> bool {
        matches!(
            self,
            StoreError::Sqlx(sqlx::Error::Database(db)) if db.code().as_deref() == Some("787")
        )
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Opaque wire status returned to callers is defined in `core`; this struct
/// only owns the connection pool.
#[derive(Clone)]
pub struct Sin90Store {
    pool: SqlitePool,
}

impl Sin90Store {
    /// Open (creating if needed) `sin90.db` and run migrations.
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StoreError::Conflict(e.to_string()))?;
        }
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5))
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;
        sqlx::migrate!("./src/store/migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    /// In-memory database for tests (single connection — each `:memory:` handle
    /// is its own database).
    pub async fn open_memory() -> Result<Self> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")?
            .busy_timeout(std::time::Duration::from_secs(5))
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        sqlx::migrate!("./src/store/migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

/// Test-only escape hatches (raw SQL peeks + a mutation to prove event replay
/// is unaffected). Feature-gated so they are NOT part of the released API.
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub mod test_hooks {
    use crate::store::{Result, Sin90Store};
    use sqlx::Row;

    /// Rename a direction via raw SQL — historical event payloads must NOT change.
    pub async fn rename_direction(store: &Sin90Store, id: &str, new_title: &str) -> Result<()> {
        sqlx::query("UPDATE sin90_directions SET title = ? WHERE id = ?")
            .bind(new_title)
            .bind(id)
            .execute(store.pool())
            .await?;
        Ok(())
    }

    pub async fn direction_count(store: &Sin90Store) -> Result<i64> {
        Ok(sqlx::query("SELECT COUNT(*) AS n FROM sin90_directions")
            .fetch_one(store.pool())
            .await?
            .get::<i64, _>("n"))
    }

    pub async fn first_task_id(store: &Sin90Store) -> Result<String> {
        Ok(
            sqlx::query("SELECT id FROM sin90_tasks ORDER BY id LIMIT 1")
                .fetch_one(store.pool())
                .await?
                .get::<String, _>("id"),
        )
    }

    pub async fn close_week(store: &Sin90Store, id: &str) -> Result<()> {
        sqlx::query("UPDATE sin90_weeks SET status = 'closed' WHERE id = ?")
            .bind(id)
            .execute(store.pool())
            .await?;
        Ok(())
    }

    pub async fn event_count(store: &Sin90Store, entity: &str, entity_id: &str) -> Result<i64> {
        Ok(
            sqlx::query(
                "SELECT COUNT(*) AS n FROM sin90_events WHERE entity = ? AND entity_id = ?",
            )
            .bind(entity)
            .bind(entity_id)
            .fetch_one(store.pool())
            .await?
            .get::<i64, _>("n"),
        )
    }

    /// Backdate a task's `created_at` via raw SQL — the only way a test can
    /// put a task on "an earlier day" for `today_view`'s carry-over-candidate
    /// rule without sleeping past a UTC day boundary.
    pub async fn set_task_created_at(store: &Sin90Store, id: &str, created_at: &str) -> Result<()> {
        sqlx::query("UPDATE sin90_tasks SET created_at = ? WHERE id = ?")
            .bind(created_at)
            .bind(id)
            .execute(store.pool())
            .await?;
        Ok(())
    }

    /// Backdate when a task last moved into `in_progress` (its transition
    /// event's `at`) — what `today_view`'s carry-over rule keys off.
    pub async fn set_task_started_at(store: &Sin90Store, id: &str, at: &str) -> Result<()> {
        sqlx::query(
            "UPDATE sin90_events SET at = ?
             WHERE entity = 'task' AND entity_id = ? AND to_state = 'in_progress'",
        )
        .bind(at)
        .bind(id)
        .execute(store.pool())
        .await?;
        Ok(())
    }

    pub async fn proposal_status(store: &Sin90Store, id: &str) -> Result<Option<String>> {
        Ok(
            sqlx::query("SELECT status FROM sin90_proposals WHERE id = ?")
                .bind(id)
                .fetch_optional(store.pool())
                .await?
                .map(|r| r.get::<String, _>("status")),
        )
    }

    // ----- T3.3.1 outbox peeks -------------------------------------------

    /// One `sin90_outbox` row, as read back for assertions — `desired` is
    /// pre-parsed to [`serde_json::Value`] so a test can index into it
    /// (`.desired["spec"]["cron"]`) instead of string-matching.
    #[derive(Debug, Clone)]
    pub struct OutboxTestRow {
        pub id: String,
        pub kind: String,
        pub dedup_key: String,
        pub desired: serde_json::Value,
        pub status: String,
        pub attempts: i64,
        pub failure_kind: Option<String>,
        pub last_error: Option<String>,
        pub next_attempt_at: Option<String>,
    }

    /// All `sin90_outbox` rows for a `dedup_key`, oldest first. Production
    /// code (`store::repo::upsert_outbox`) keeps this to at most one
    /// `pending`/`failed` row per `dedup_key`, but `done` rows accumulate
    /// (never resurrected) — a test that only cares about the live row
    /// should filter on `status` itself, same as the reconciler will.
    pub async fn outbox_rows_for(
        store: &Sin90Store,
        dedup_key: &str,
    ) -> Result<Vec<OutboxTestRow>> {
        let rows = sqlx::query(
            "SELECT id, kind, dedup_key, desired, status, attempts, failure_kind, \
                    last_error, next_attempt_at
             FROM sin90_outbox WHERE dedup_key = ? ORDER BY created_at ASC, rowid ASC",
        )
        .bind(dedup_key)
        .fetch_all(store.pool())
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(OutboxTestRow {
                    id: r.get("id"),
                    kind: r.get("kind"),
                    dedup_key: r.get("dedup_key"),
                    desired: serde_json::from_str(&r.get::<String, _>("desired"))?,
                    status: r.get("status"),
                    attempts: r.get("attempts"),
                    failure_kind: r.get("failure_kind"),
                    last_error: r.get("last_error"),
                    next_attempt_at: r.get("next_attempt_at"),
                })
            })
            .collect()
    }

    /// Total row count across the whole `sin90_outbox` table — used by the
    /// same-transaction-rollback test to prove a failed write leaves no
    /// residue at all (not just "no residue for this one `dedup_key`").
    pub async fn outbox_count(store: &Sin90Store) -> Result<i64> {
        Ok(sqlx::query("SELECT COUNT(*) AS n FROM sin90_outbox")
            .fetch_one(store.pool())
            .await?
            .get::<i64, _>("n"))
    }

    /// Force a row straight to `failed` via raw SQL, bypassing production
    /// code entirely — nothing in THIS task produces `failed` rows (that is
    /// the reconciler's job, T3.3.2); this hook exists only so the "a
    /// `failed` row resets to `pending` on the Routine's next change" test
    /// can set up its starting state.
    pub async fn mark_outbox_failed(
        store: &Sin90Store,
        id: &str,
        failure_kind: &str,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE sin90_outbox
             SET status = 'failed', failure_kind = ?, last_error = 'test-injected', attempts = 3
             WHERE id = ?",
        )
        .bind(failure_kind)
        .bind(id)
        .execute(store.pool())
        .await?;
        Ok(())
    }
}
