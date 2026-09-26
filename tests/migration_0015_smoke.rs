//! T5.7.2 review round 2 (H2/M5): a minimal, self-contained smoke test for
//! migration `0015_classify_evals_and_rejection_index.sql` — mirrors
//! `tests/migration_0014_smoke.rs`'s own precedent (pre-pr-check SZ-4: a
//! migration must not mix with other changes in one PR). Deliberately does
//! NOT depend on `Sin90Store`: it runs the SAME `sqlx::migrate!` macro
//! `Sin90Store::open_memory` uses, against a bare in-memory pool it builds
//! itself.

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::Row;
use std::str::FromStr;

async fn migrated_pool() -> sqlx::SqlitePool {
    let options = SqliteConnectOptions::from_str("sqlite::memory:")
        .unwrap()
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    sqlx::migrate!("./src/store/migrations")
        .run(&pool)
        .await
        .unwrap();
    pool
}

/// `sin90_classify_evals` exists, is empty at boot, and a task_id can be
/// upserted into it (the shape `AiSink::record_classify_eval` relies on: one
/// row per task, `task_id` as the primary key so a second write REPLACES
/// rather than duplicates).
#[tokio::test]
async fn migration_0015_creates_empty_classify_evals_table() {
    let pool = migrated_pool().await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_classify_evals")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "no task has been evaluated yet at a fresh boot");

    // A real task_id must exist for the FK — seed one directly.
    sqlx::query(
        "INSERT INTO sin90_tasks
             (id, direction_id, week_id, parent_task_id, title, status, kind, energy,
              est_minutes, sort_key, carried_from, created_at, updated_at)
         VALUES ('t1', NULL, NULL, NULL, 'Task', 'backlog', 'other', 'mid', NULL, 0, NULL, ?, ?)",
    )
    .bind("2026-09-26T00:00:00Z")
    .bind("2026-09-26T00:00:00Z")
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO sin90_classify_evals (task_id, evaluated_at) VALUES ('t1', '2026-09-26T00:00:00Z')
         ON CONFLICT(task_id) DO UPDATE SET evaluated_at = excluded.evaluated_at",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sin90_classify_evals (task_id, evaluated_at) VALUES ('t1', '2026-09-26T01:00:00Z')
         ON CONFLICT(task_id) DO UPDATE SET evaluated_at = excluded.evaluated_at",
    )
    .execute(&pool)
    .await
    .unwrap();

    let row = sqlx::query("SELECT evaluated_at FROM sin90_classify_evals WHERE task_id = 't1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row.get::<String, _>("evaluated_at"),
        "2026-09-26T01:00:00Z",
        "an upsert on the same task_id must replace, not duplicate, the row"
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_classify_evals")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

/// The new `(capability_source, rejected_at)` index on
/// `sin90_proposal_rejections` exists — `list_rejected_ops`'s WHERE +
/// ORDER BY shape (M5).
#[tokio::test]
async fn migration_0015_adds_rejection_capability_index() {
    let pool = migrated_pool().await;
    let row = sqlx::query(
        "SELECT 1 FROM sqlite_master
         WHERE type = 'index' AND name = 'idx_sin90_proposal_rejections_capability'",
    )
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert!(
        row.is_some(),
        "migration 0015 must create idx_sin90_proposal_rejections_capability"
    );
}

/// N-H1 (T5.7.2 review round 2 follow-up): `sin90_tasks` gains
/// `triage_via`/`triage_entered_at`, both `NULL` for a freshly-created task
/// (never 待定-parked), and both writable — the shape
/// `Sin90Op::AssignTaskDirection`'s apply relies on.
#[tokio::test]
async fn migration_0015_adds_triage_provenance_columns() {
    let pool = migrated_pool().await;
    sqlx::query(
        "INSERT INTO sin90_tasks
             (id, direction_id, week_id, parent_task_id, title, status, kind, energy,
              est_minutes, sort_key, carried_from, created_at, updated_at)
         VALUES ('t1', NULL, NULL, NULL, 'Task', 'backlog', 'other', 'mid', NULL, 0, NULL, ?, ?)",
    )
    .bind("2026-09-26T00:00:00Z")
    .bind("2026-09-26T00:00:00Z")
    .execute(&pool)
    .await
    .unwrap();

    let row = sqlx::query("SELECT triage_via, triage_entered_at FROM sin90_tasks WHERE id = 't1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.get::<Option<String>, _>("triage_via"), None);
    assert_eq!(row.get::<Option<String>, _>("triage_entered_at"), None);

    sqlx::query(
        "UPDATE sin90_tasks SET direction_id = 'sin90-triage', triage_via = 'classify',
             triage_entered_at = ? WHERE id = 't1'",
    )
    .bind("2026-09-26T01:00:00Z")
    .execute(&pool)
    .await
    .unwrap();
    let row = sqlx::query("SELECT triage_via, triage_entered_at FROM sin90_tasks WHERE id = 't1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row.get::<Option<String>, _>("triage_via"),
        Some("classify".to_string())
    );
    assert_eq!(
        row.get::<Option<String>, _>("triage_entered_at"),
        Some("2026-09-26T01:00:00Z".to_string())
    );
}

/// N-H1 backfill: a task ALREADY parked in 待定 (via an accepted classify
/// proposal) before this migration's own columns existed gets `triage_via =
/// 'classify'` and a `triage_entered_at` derived from its `direction_assigned`
/// event, while a task parked in 待定 with NO such trace (a direct/human
/// placement) backfills as `'direct'` and gets no `triage_entered_at` (no
/// `direction_assigned` event seeded for it here) — the one-time UPDATEs
/// migration 0015 itself ships, run here against the REAL file content
/// (`include_str!`, not a hand-copied string — mirrors
/// `migration_0014_insert_is_idempotent_on_rerun`'s own precedent) so
/// editing the shipped backfill SQL without updating this test cannot pass
/// silently.
///
/// Migrations 0001–0014 are applied first (raw, in order, straight off
/// disk) to reach the pre-0015 schema — `sin90_tasks` has no
/// `triage_via`/`triage_entered_at` columns yet — then the fixture rows are
/// seeded, then 0015 itself runs for the FIRST time in this pool (its
/// `CREATE TABLE`/`CREATE INDEX` statements would collide on a SECOND run,
/// which is why this test builds its own pool from scratch instead of
/// reusing `migrated_pool()`, which has already run every file once).
#[tokio::test]
async fn migration_0015_backfills_existing_triage_rows() {
    let options = SqliteConnectOptions::from_str("sqlite::memory:")
        .unwrap()
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();

    let migrations_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/store/migrations");
    let mut files: Vec<_> = std::fs::read_dir(&migrations_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("sql"))
        .collect();
    files.sort();
    for f in &files {
        let name = f.file_name().unwrap().to_string_lossy();
        if name.starts_with("0015") {
            break; // reached this migration; everything before it is applied.
        }
        let sql = std::fs::read_to_string(f).unwrap();
        sqlx::raw_sql(&sql).execute(&pool).await.unwrap();
    }

    // A classify-produced 待定 parking (an accepted `capability_source =
    // "classify"` proposal assigning it to `sin90-triage`) — must backfill
    // `triage_via = 'classify'` and `triage_entered_at` from its
    // `direction_assigned` event.
    sqlx::query(
        "INSERT INTO sin90_tasks
             (id, direction_id, week_id, parent_task_id, title, status, kind, energy,
              est_minutes, sort_key, carried_from, created_at, updated_at)
         VALUES ('t-classify', 'sin90-triage', NULL, NULL, 'Task', 'backlog', 'other', 'mid',
                 NULL, 0, NULL, ?, ?)",
    )
    .bind("2026-01-01T00:00:00Z")
    .bind("2026-01-01T00:00:00Z")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sin90_ai_calls
             (id, run_id, task_kind, engine, ok, proposal_id, at)
         VALUES ('call-1', 'run-1', 'classify', 'local', 1, 'p-1', ?)",
    )
    .bind("2026-01-01T00:00:01Z")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sin90_proposals (id, status, source, ops, created_at)
         VALUES ('p-1', 'applied', 'local_brain', ?, ?)",
    )
    .bind(
        r#"[{"op":"assign_task_direction","task_id":"t-classify","direction_id":"sin90-triage"}]"#,
    )
    .bind("2026-01-01T00:00:01Z")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sin90_events (id, entity, entity_id, kind, payload, at)
         VALUES ('ev-1', 'task', 't-classify', 'direction_assigned', ?, ?)",
    )
    .bind(r#"{"direction_id":"sin90-triage"}"#)
    .bind("2026-01-01T00:00:02Z")
    .execute(&pool)
    .await
    .unwrap();

    // A direct/human 待定 parking — no `sin90_ai_calls`/`sin90_proposals`
    // trace at all — must backfill `triage_via = 'direct'` and no
    // `triage_entered_at` (no `direction_assigned` event exists for it
    // either, same as a human `POST /proposals` submission never produces
    // one through `AiSink`).
    sqlx::query(
        "INSERT INTO sin90_tasks
             (id, direction_id, week_id, parent_task_id, title, status, kind, energy,
              est_minutes, sort_key, carried_from, created_at, updated_at)
         VALUES ('t-direct', 'sin90-triage', NULL, NULL, 'Task', 'backlog', 'other', 'mid',
                 NULL, 0, NULL, ?, ?)",
    )
    .bind("2026-01-01T00:00:00Z")
    .bind("2026-01-01T00:00:00Z")
    .execute(&pool)
    .await
    .unwrap();

    let migration_0015_sql =
        include_str!("../src/store/migrations/0015_classify_evals_and_rejection_index.sql");
    assert!(
        migration_0015_sql.contains("ALTER TABLE sin90_tasks ADD COLUMN triage_via"),
        "include_str! must point at the real 0015 migration"
    );
    sqlx::raw_sql(migration_0015_sql)
        .execute(&pool)
        .await
        .unwrap();

    let row = sqlx::query(
        "SELECT triage_via, triage_entered_at FROM sin90_tasks WHERE id = 't-classify'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        row.get::<Option<String>, _>("triage_via"),
        Some("classify".to_string()),
        "an accepted classify proposal's target must backfill as classify-sourced"
    );
    assert_eq!(
        row.get::<Option<String>, _>("triage_entered_at"),
        Some("2026-01-01T00:00:02Z".to_string()),
        "entered_at must backfill from the direction_assigned event"
    );

    let row =
        sqlx::query("SELECT triage_via, triage_entered_at FROM sin90_tasks WHERE id = 't-direct'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        row.get::<Option<String>, _>("triage_via"),
        Some("direct".to_string()),
        "a 待定 task with no classify trace must backfill as direct-sourced"
    );
    assert_eq!(
        row.get::<Option<String>, _>("triage_entered_at"),
        None,
        "no direction_assigned event exists for a direct placement to backfill from"
    );
}

/// M1 (T5.7.2 review round 3): the bug the naive (non-ancestry) version of
/// this backfill missed — a classify-produced 待定 parking that was THEN
/// carried over TWICE before this migration ever ran, so the row sitting in
/// 待定 today (`t3`) is two `carried_from` hops away from the id the
/// `assign_task_direction` op and `direction_assigned` event were actually
/// recorded against (`t1`). `t2` (the middle hop) has neither trace at all —
/// exactly what a pre-N-H1 `CarryOverTask` apply produced, since the columns
/// this migration adds did not exist yet for it to copy. A join keyed only
/// on `sin90_tasks.id` finds nothing for `t3` and would backfill it as
/// `'direct'` with no `triage_entered_at` — silently losing both the A3
/// carve-out and the H2 retry floor for it. Mutation target: replace the
/// `ancestry` CTE's recursive join back to a plain `sin90_tasks.id` lookup
/// (N-H1's original, pre-M1 shape) and both assertions below go red.
#[tokio::test]
async fn migration_0015_backfills_across_a_two_level_carry_chain() {
    let options = SqliteConnectOptions::from_str("sqlite::memory:")
        .unwrap()
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();

    let migrations_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/store/migrations");
    let mut files: Vec<_> = std::fs::read_dir(&migrations_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("sql"))
        .collect();
    files.sort();
    for f in &files {
        let name = f.file_name().unwrap().to_string_lossy();
        if name.starts_with("0015") {
            break;
        }
        let sql = std::fs::read_to_string(f).unwrap();
        sqlx::raw_sql(&sql).execute(&pool).await.unwrap();
    }

    // t1: the ORIGINAL task, classified into 待定 by an accepted proposal —
    // same fixture shape as `t-classify` above.
    sqlx::query(
        "INSERT INTO sin90_tasks
             (id, direction_id, week_id, parent_task_id, title, status, kind, energy,
              est_minutes, sort_key, carried_from, created_at, updated_at)
         VALUES ('t1', 'sin90-triage', NULL, NULL, 'Task', 'carried_over', 'other', 'mid',
                 NULL, 0, NULL, ?, ?)",
    )
    .bind("2026-01-01T00:00:00Z")
    .bind("2026-01-01T00:00:00Z")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sin90_ai_calls
             (id, run_id, task_kind, engine, ok, proposal_id, at)
         VALUES ('call-1', 'run-1', 'classify', 'local', 1, 'p-1', ?)",
    )
    .bind("2026-01-01T00:00:01Z")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sin90_proposals (id, status, source, ops, created_at)
         VALUES ('p-1', 'applied', 'local_brain', ?, ?)",
    )
    .bind(r#"[{"op":"assign_task_direction","task_id":"t1","direction_id":"sin90-triage"}]"#)
    .bind("2026-01-01T00:00:01Z")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sin90_events (id, entity, entity_id, kind, payload, at)
         VALUES ('ev-1', 'task', 't1', 'direction_assigned', ?, ?)",
    )
    .bind(r#"{"direction_id":"sin90-triage"}"#)
    .bind("2026-01-01T00:00:02Z")
    .execute(&pool)
    .await
    .unwrap();

    // t2: t1 carried over ONE week later, still parked in 待定 — a
    // pre-N-H1 `CarryOverTask` apply: only `carried_from`/`direction_id`
    // copied, no `triage_via`/`triage_entered_at` (the columns did not exist
    // yet), no `sin90_ai_calls`/`sin90_proposals`/`direction_assigned` trace
    // of its own (carry only ever emits `created`/`transitioned` events).
    sqlx::query(
        "INSERT INTO sin90_tasks
             (id, direction_id, week_id, parent_task_id, title, status, kind, energy,
              est_minutes, sort_key, carried_from, created_at, updated_at)
         VALUES ('t2', 'sin90-triage', NULL, NULL, 'Task', 'carried_over', 'other', 'mid',
                 NULL, 0, 't1', ?, ?)",
    )
    .bind("2026-01-08T00:00:00Z")
    .bind("2026-01-08T00:00:00Z")
    .execute(&pool)
    .await
    .unwrap();

    // t3: t2 carried over AGAIN, still parked in 待定 today — two hops away
    // from t1's own trace, and the row this migration actually has to
    // backfill correctly.
    sqlx::query(
        "INSERT INTO sin90_tasks
             (id, direction_id, week_id, parent_task_id, title, status, kind, energy,
              est_minutes, sort_key, carried_from, created_at, updated_at)
         VALUES ('t3', 'sin90-triage', NULL, NULL, 'Task', 'planned', 'other', 'mid',
                 NULL, 0, 't2', ?, ?)",
    )
    .bind("2026-01-15T00:00:00Z")
    .bind("2026-01-15T00:00:00Z")
    .execute(&pool)
    .await
    .unwrap();

    let migration_0015_sql =
        include_str!("../src/store/migrations/0015_classify_evals_and_rejection_index.sql");
    sqlx::raw_sql(migration_0015_sql)
        .execute(&pool)
        .await
        .unwrap();

    let row = sqlx::query("SELECT triage_via, triage_entered_at FROM sin90_tasks WHERE id = 't3'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row.get::<Option<String>, _>("triage_via"),
        Some("classify".to_string()),
        "t3 must trace back through t2 to t1's classify assignment, not backfill as direct"
    );
    assert_eq!(
        row.get::<Option<String>, _>("triage_entered_at"),
        Some("2026-01-01T00:00:02Z".to_string()),
        "t3's entered_at must come from t1's own direction_assigned event, not be left NULL"
    );

    // t2 (the middle hop) is not itself in 待定 by direction_id at query
    // time from `sin90_tasks`'s perspective — it IS still `direction_id =
    // 'sin90-triage'` in this fixture (only t3 moved on from it in a real
    // carry; here both rows are left parked to keep the fixture minimal) —
    // so it backfills too, from the SAME ancestor (t1).
    let row = sqlx::query("SELECT triage_via, triage_entered_at FROM sin90_tasks WHERE id = 't2'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row.get::<Option<String>, _>("triage_via"),
        Some("classify".to_string()),
        "t2 must also trace back to t1's classify assignment"
    );
    assert_eq!(
        row.get::<Option<String>, _>("triage_entered_at"),
        Some("2026-01-01T00:00:02Z".to_string())
    );
}
