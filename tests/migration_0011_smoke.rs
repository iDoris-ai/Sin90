//! T3.3.2 对账器评审 (branch a): a minimal, self-contained smoke test for
//! migration `0011_outbox_version_kernel_suspended.sql` — this branch's ONLY
//! job is the migration and the DESIGN-LIFEOS.md §2 #29 / §4.1 decisions, per
//! pre-pr-check SZ-4 ("a migration must not mix with other changes in one
//! PR"), mirroring `tests/migration_0010_smoke.rs`'s own precedent. It
//! deliberately does NOT depend on `Sin90Store` or `store::test_hooks` (both
//! — plus every Rust caller of these two columns — live on the later stacked
//! `feat/t3.3.2-reconciler` branch): it runs the SAME `sqlx::migrate!` macro
//! `Sin90Store::open_memory` uses, against a bare in-memory pool it builds
//! itself, and reads the resulting schema back with `pragma_table_info`.

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::Row;
use std::collections::HashMap;
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

#[tokio::test]
async fn migration_0011_adds_outbox_version_not_null_default_1() {
    let pool = migrated_pool().await;

    let rows =
        sqlx::query("SELECT name, \"notnull\", dflt_value FROM pragma_table_info('sin90_outbox')")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(!rows.is_empty(), "sin90_outbox table does not exist");

    let mut found = false;
    for r in &rows {
        if r.get::<String, _>("name") == "version" {
            found = true;
            assert_eq!(r.get::<i64, _>("notnull"), 1, "version must be NOT NULL");
            assert_eq!(
                r.get::<Option<String>, _>("dflt_value").as_deref(),
                Some("1"),
                "version's column default must be 1"
            );
        }
    }
    assert!(found, "sin90_outbox.version column is missing");
}

/// A row inserted the way pre-0011 code would (no `version` in the column
/// list) reads back `version = 1` via the column default — an existing
/// user's db upgrading through this migration must not end up with a `NULL`
/// or `0` version on rows it already had.
#[tokio::test]
async fn migration_0011_existing_style_insert_defaults_version_to_1() {
    let pool = migrated_pool().await;
    sqlx::query(
        "INSERT INTO sin90_outbox (id, kind, dedup_key, desired, status, created_at, done_at)
         VALUES ('o1', 'scheduler.upsert', 'routine:x', '{}', 'pending', '2026-01-01T00:00:00Z', NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let version: i64 = sqlx::query_scalar("SELECT version FROM sin90_outbox WHERE id = 'o1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(version, 1);
}

#[tokio::test]
async fn migration_0011_adds_routines_kernel_suspended_at_nullable() {
    let pool = migrated_pool().await;

    let rows = sqlx::query("SELECT name, \"notnull\" FROM pragma_table_info('sin90_routines')")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(!rows.is_empty(), "sin90_routines table does not exist");

    let cols: HashMap<String, i64> = rows
        .iter()
        .map(|r| (r.get::<String, _>("name"), r.get::<i64, _>("notnull")))
        .collect();
    assert!(
        cols.contains_key("kernel_suspended_at"),
        "sin90_routines.kernel_suspended_at column is missing"
    );
    assert_eq!(
        cols["kernel_suspended_at"], 0,
        "kernel_suspended_at must be nullable"
    );
}

/// A pre-existing `sin90_routines` row (inserted the way pre-0011 code
/// would) reads back `kernel_suspended_at = NULL` — "not yet observed",
/// never a false "currently suspended".
#[tokio::test]
async fn migration_0011_existing_style_routine_insert_leaves_kernel_suspended_at_null() {
    let pool = migrated_pool().await;
    sqlx::query(
        "INSERT INTO sin90_routines
            (id, area_id, direction_id, title, kind, cron, tz, target_count, target_minutes,
             status, created_at, updated_at)
         VALUES ('r1', NULL, NULL, 'Exercise', 'exercise', '0 7 * * *', 'UTC', NULL, NULL,
                 'active', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let value: Option<String> =
        sqlx::query_scalar("SELECT kernel_suspended_at FROM sin90_routines WHERE id = 'r1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(value, None);
}

/// Positive control for the assertion helper itself: a table that does NOT
/// exist yields an EMPTY `pragma_table_info` result (not an error) — so the
/// `!rows.is_empty()` checks above are actually load-bearing.
#[tokio::test]
async fn pragma_table_info_on_a_nonexistent_table_is_empty_not_an_error() {
    let pool = migrated_pool().await;
    let rows = sqlx::query("SELECT name FROM pragma_table_info('sin90_table_that_does_not_exist')")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(rows.is_empty());
}
