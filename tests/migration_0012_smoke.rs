//! T3.3.2 对账器评审第 2 轮 (branch a): a minimal, self-contained smoke test
//! for migration `0012_outbox_other_bucket_attempts.sql` — mirrors
//! `tests/migration_0011_smoke.rs`'s own precedent (itself mirroring
//! `migration_0010_smoke.rs`), per pre-pr-check SZ-4 ("a migration must not
//! mix with other changes in one PR"). Deliberately does NOT depend on
//! `Sin90Store` or `store::test_hooks` (both — plus every Rust caller of
//! this column — live on the later stacked `feat/t3.3.2-reconciler`
//! branch): it runs the SAME `sqlx::migrate!` macro `Sin90Store::open_memory`
//! uses, against a bare in-memory pool it builds itself, and reads the
//! resulting schema back with `pragma_table_info`.

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

#[tokio::test]
async fn migration_0012_adds_outbox_other_bucket_attempts_not_null_default_0() {
    let pool = migrated_pool().await;

    let rows =
        sqlx::query("SELECT name, \"notnull\", dflt_value FROM pragma_table_info('sin90_outbox')")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(!rows.is_empty(), "sin90_outbox table does not exist");

    let mut found = false;
    for r in &rows {
        if r.get::<String, _>("name") == "other_bucket_attempts" {
            found = true;
            assert_eq!(
                r.get::<i64, _>("notnull"),
                1,
                "other_bucket_attempts must be NOT NULL"
            );
            assert_eq!(
                r.get::<Option<String>, _>("dflt_value").as_deref(),
                Some("0"),
                "other_bucket_attempts's column default must be 0"
            );
        }
    }
    assert!(
        found,
        "sin90_outbox.other_bucket_attempts column is missing"
    );
}

/// A row inserted the way pre-0012 code would (no `other_bucket_attempts` in
/// the column list) reads back `0` via the column default — an existing
/// user's db upgrading through this migration must not end up with a `NULL`
/// on rows it already had (which would break the reconciler's exhaustion
/// arithmetic, `other_bucket_attempts + 1`, the moment it touches an old
/// row).
#[tokio::test]
async fn migration_0012_existing_style_insert_defaults_other_bucket_attempts_to_0() {
    let pool = migrated_pool().await;
    sqlx::query(
        "INSERT INTO sin90_outbox (id, kind, dedup_key, desired, status, created_at, done_at)
         VALUES ('o1', 'scheduler.upsert', 'routine:x', '{}', 'pending', '2026-01-01T00:00:00Z', NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let other_bucket_attempts: i64 =
        sqlx::query_scalar("SELECT other_bucket_attempts FROM sin90_outbox WHERE id = 'o1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(other_bucket_attempts, 0);
}

/// Positive control for the assertion helper itself: a table that does NOT
/// exist yields an EMPTY `pragma_table_info` result (not an error) — so the
/// `!rows.is_empty()` check above is actually load-bearing.
#[tokio::test]
async fn pragma_table_info_on_a_nonexistent_table_is_empty_not_an_error() {
    let pool = migrated_pool().await;
    let rows = sqlx::query("SELECT name FROM pragma_table_info('sin90_table_that_does_not_exist')")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(rows.is_empty());
}
