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

pub mod ai_port;
pub mod attention;
pub mod packs;
pub mod repo;
pub mod weekly_draft;

pub use ai_port::{AiCallSummary, AiReader};
pub use attention::{AttentionRow, WeekAttention};
pub use packs::{five_life_systems, SeedArea};
pub use repo::{
    AppliedProposal, ApplyOutcome, AutoReviewCreated, EventRow, ReviewUpdate, RoutineFireOutcome,
    RoutineUpdate, StoredProposal, TodayView,
};
pub use weekly_draft::{
    render_weekly_draft_markdown, AreaMinutes, DirectionMinutes, RoutineDraftRow, WeeklyDraft,
};

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
/// owns the connection pool plus (T4.2.1) Sin90's own `data_dir` — the
/// directory `finalize_review` exports a Review's Markdown to (design §2
/// #11/§4.2: `<data_dir>/reviews/<kind>/<period>.md`, `body_ref` stored
/// relative to it).
#[derive(Clone)]
pub struct Sin90Store {
    pool: SqlitePool,
    /// T5.1.1 (design §11.5 v2.1 M1): a SEPARATE small pool over the SAME
    /// target as `pool`, every connection opened with
    /// `pragma("query_only", "ON")` — a write attempted through this pool is
    /// a SQLite `readonly` error, not a convention `ai::AiReadModel`'s
    /// implementor has to remember to honor. Built once at open time (not
    /// lazily per call) since building it needs an `await`.
    ai_pool: SqlitePool,
    /// `Some(path.parent())` for [`Sin90Store::open`] (real deployments —
    /// both `Serve --data-dir` and the Agent24-module `A24_DATA_DIR`, per
    /// `main.rs`, always pass a `path` with a parent). `None` for
    /// [`Sin90Store::open_memory`] — the `:memory:`/dev-and-test-only mode
    /// that already drops events too (see `main.rs::run_standalone`'s doc) —
    /// unless overridden by the test-only
    /// [`Sin90Store::open_memory_with_data_dir`].
    data_dir: Option<std::path::PathBuf>,
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
            .connect_with(options.clone())
            .await?;
        sqlx::migrate!("./src/store/migrations").run(&pool).await?;
        let ai_pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(options.pragma("query_only", "ON"))
            .await?;
        Ok(Self {
            pool,
            ai_pool,
            data_dir: path.parent().map(|p| p.to_path_buf()),
        })
    }

    /// In-memory database for tests. T5.1.1 (design §11.5 v2.1 M1): a
    /// per-instance NAMED shared-cache memory database
    /// (`file:sin90-<ulid>?mode=memory&cache=shared`), not the old anonymous
    /// `sqlite::memory:` — a second pool connecting to the same NAME now sees
    /// the same data (needed for `ai_pool` above), which a second connection
    /// to plain `sqlite::memory:` never could (each anonymous `:memory:`
    /// handle is its own separate database). The single-writer-pool
    /// discipline is unchanged (`max_connections(1)` here); a unique name per
    /// call keeps concurrent test stores from colliding with each other. No
    /// `data_dir` — see the field's doc.
    ///
    /// 2026-09-24 review: a NAMED shared-cache memory database is destroyed
    /// the moment its LAST connection closes — SQLite has no other owner of
    /// its storage. `sqlx`'s default pool reaper closes idle connections
    /// (`idle_timeout`) and periodically recycles even busy ones
    /// (`max_lifetime`); with `max_connections(1)` on the write pool, a
    /// reap-then-nothing-holds-it-open window would silently drop every row
    /// this store has ever written. Both pools below pin `min_connections(1)`
    /// with `idle_timeout(None)`/`max_lifetime(None)` so at least one
    /// connection to this store's name is ALWAYS open for as long as the
    /// `Sin90Store` (and therefore these pools) exists.
    pub async fn open_memory() -> Result<Self> {
        let uri = format!(
            "sqlite:file:sin90-{}?mode=memory&cache=shared",
            crate::core::ulid()
        );
        let options = SqliteConnectOptions::from_str(&uri)?
            .busy_timeout(std::time::Duration::from_secs(5))
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options.clone())
            .await?;
        sqlx::migrate!("./src/store/migrations").run(&pool).await?;
        let ai_pool = SqlitePoolOptions::new()
            .max_connections(2)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options.pragma("query_only", "ON"))
            .await?;
        Ok(Self {
            pool,
            ai_pool,
            data_dir: None,
        })
    }

    /// Test-only (T4.2.1): an `:memory:` store (cheap, no real `sin90.db`)
    /// that STILL has a real filesystem `data_dir`, so `finalize_review`'s
    /// Markdown export can be exercised without standing up a full `open()`.
    /// Not reachable from the shipped binary — `main.rs` only ever calls
    /// `open` (real deployments) or `open_memory` (`Serve` with no
    /// `--data-dir`).
    #[cfg(any(test, feature = "test-hooks"))]
    pub async fn open_memory_with_data_dir(dir: std::path::PathBuf) -> Result<Self> {
        let mut store = Self::open_memory().await?;
        store.data_dir = Some(dir);
        Ok(store)
    }

    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// T5.1.1 (design §11.5): `ai::ports::AiReadModel` handed to the ai
    /// module — the ONLY way it can read `sin90.db`, and it cannot write
    /// through it (see the `ai_pool` field doc). Cheap: `SqlitePool` is an
    /// `Arc` internally, so this clones a handle, not a connection.
    #[must_use]
    pub fn ai_reader(&self) -> crate::store::ai_port::AiReader {
        crate::store::ai_port::AiReader::new(self.ai_pool.clone())
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

    // ----- T3.2.2 routine_fires peeks -------------------------------------

    /// Total `sin90_routine_fires` rows for one `routine_id` — used by the
    /// "same fire_id recorded twice collapses to one row" test and its
    /// positive control ("two distinct fire_ids leave two rows").
    pub async fn routine_fire_count(store: &Sin90Store, routine_id: &str) -> Result<i64> {
        Ok(
            sqlx::query("SELECT COUNT(*) AS n FROM sin90_routine_fires WHERE routine_id = ?")
                .bind(routine_id)
                .fetch_one(store.pool())
                .await?
                .get::<i64, _>("n"),
        )
    }

    /// Backdate one `sin90_routine_fires` row's `received_at` via raw SQL —
    /// the only way a test can put a fire receipt "yesterday" for
    /// `today_view`'s fired-routines section without sleeping past a UTC day
    /// boundary (mirrors `set_task_created_at` above).
    pub async fn set_routine_fire_received_at(
        store: &Sin90Store,
        fire_id: &str,
        received_at: &str,
    ) -> Result<()> {
        sqlx::query("UPDATE sin90_routine_fires SET received_at = ? WHERE fire_id = ?")
            .bind(received_at)
            .bind(fire_id)
            .execute(store.pool())
            .await?;
        Ok(())
    }

    // ----- T4.1.1 Review test scaffolding ----------------------------------

    /// Insert a bare `active` `sin90_rhythms` row via raw SQL. T3.4.1 (`POST
    /// /rhythms`) has not landed yet — there is no production way to create
    /// a Rhythm — so a `kind: rhythm` Review's "period must reference an
    /// EXISTING rhythm" test needs some way to seed one. Test-only; not a
    /// stand-in for T3.4.1's eventual real create path.
    pub async fn insert_rhythm(store: &Sin90Store, id: &str) -> Result<()> {
        let now = crate::core::now_iso8601();
        sqlx::query(
            "INSERT INTO sin90_rhythms (id, status, allocations, created_at, updated_at)
             VALUES (?, 'active', '[]', ?, ?)",
        )
        .bind(id)
        .bind(&now)
        .bind(&now)
        .execute(store.pool())
        .await?;
        Ok(())
    }

    // ----- T4.3.1 weekly draft test scaffolding ----------------------------

    /// Backdate the MOST RECENT `sin90_events` row for `(entity, entity_id)`
    /// via raw SQL — the only way a test can put a `block.transitioned`,
    /// `task.transitioned`, or `routine.fired` event on a controlled day
    /// (production code always stamps `now_iso8601()`; mirrors
    /// `set_task_created_at`/`set_routine_fire_received_at` above, but on
    /// the event log itself rather than a materialized row).
    pub async fn set_last_event_at(
        store: &Sin90Store,
        entity: &str,
        entity_id: &str,
        at: &str,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE sin90_events SET at = ?
             WHERE seq = (
                 SELECT MAX(seq) FROM sin90_events WHERE entity = ? AND entity_id = ?
             )",
        )
        .bind(at)
        .bind(entity)
        .bind(entity_id)
        .execute(store.pool())
        .await?;
        Ok(())
    }

    /// Rewrite a `sin90_schedule_blocks.planned_minutes` directly via raw
    /// SQL, bypassing `transition_block`/its event entirely — `weekly_draft`
    /// negative control (T4.3.1): the draft must not move when this mutable
    /// table changes, since it never reads it (mirrors `rename_direction`
    /// above, applied to a different table/column).
    pub async fn set_block_planned_minutes_direct(
        store: &Sin90Store,
        id: &str,
        minutes: i64,
    ) -> Result<()> {
        sqlx::query("UPDATE sin90_schedule_blocks SET planned_minutes = ? WHERE id = ?")
            .bind(minutes)
            .bind(id)
            .execute(store.pool())
            .await?;
        Ok(())
    }

    /// Rewrite a `sin90_tasks.status` directly via raw SQL, bypassing
    /// `transition_task`/its event entirely — the other half of the
    /// `weekly_draft` negative control: `tasks_done` must not move when this
    /// mutable table changes.
    pub async fn set_task_status_direct(store: &Sin90Store, id: &str, status: &str) -> Result<()> {
        sqlx::query("UPDATE sin90_tasks SET status = ? WHERE id = ?")
            .bind(status)
            .bind(id)
            .execute(store.pool())
            .await?;
        Ok(())
    }

    // ----- Whole-database snapshot (2026-09-24 review M4: moved here from
    // `store::ai_port`'s own test module so `ai::propose`'s tests can reuse
    // it too, instead of keeping a second copy) -----------------------------

    /// Every table in the db, one canonical string per row (SQLite's own
    /// `quote()` — handles NULL/INTEGER/TEXT/BLOB uniformly), ordered by
    /// `rowid` so insertion order is stable. Schema-agnostic on purpose: a
    /// caller must not need to update this every time a column is added
    /// elsewhere. `sin90_events` is split into TWO keys —
    /// `entity = 'proposal'` rows and everything else — so a caller can
    /// assert the narrower, actually-designed claim "only the
    /// `proposal.submitted` mirror row changed" instead of just "some event,
    /// somewhere, changed" (`store::ai_port`'s own J8 judgement first needed
    /// this split; `ai::propose`'s J21 table-diff test reuses it verbatim).
    pub async fn snapshot_all_tables(
        pool: &sqlx::SqlitePool,
    ) -> std::collections::BTreeMap<String, Vec<String>> {
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
        )
        .fetch_all(pool)
        .await
        .unwrap();
        let mut out = std::collections::BTreeMap::new();
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

    /// The keys whose snapshotted value differs between `before` and `after`.
    pub fn diff_snapshot_keys(
        before: &std::collections::BTreeMap<String, Vec<String>>,
        after: &std::collections::BTreeMap<String, Vec<String>>,
    ) -> Vec<String> {
        before
            .keys()
            .filter(|k| before.get(*k) != after.get(*k))
            .cloned()
            .collect()
    }
}
