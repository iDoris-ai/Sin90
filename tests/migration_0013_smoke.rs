//! T4.4.1 Opus 评审 L5: a minimal, self-contained smoke test for migration
//! `0013_outbox_result_ref.sql` — mirrors `tests/migration_0012_smoke.rs`'s
//! own precedent, per pre-pr-check SZ-4 ("a migration must not mix with
//! other changes in one PR"). Deliberately does NOT depend on `Sin90Store`
//! or `store::test_hooks`: it runs the SAME `sqlx::migrate!` macro
//! `Sin90Store::open_memory` uses, against a bare in-memory pool it builds
//! itself, and reads the resulting schema back with `pragma_table_info`.

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
async fn migration_0013_adds_outbox_result_ref_nullable() {
    let pool = migrated_pool().await;

    let rows =
        sqlx::query("SELECT name, \"notnull\", dflt_value FROM pragma_table_info('sin90_outbox')")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(!rows.is_empty(), "sin90_outbox table does not exist");

    let mut found = false;
    for r in &rows {
        if r.get::<String, _>("name") == "result_ref" {
            found = true;
            assert_eq!(r.get::<i64, _>("notnull"), 0, "result_ref must be NULLable");
            assert_eq!(
                r.get::<Option<String>, _>("dflt_value"),
                None,
                "result_ref must have no default (NULL for every pre-existing row)"
            );
        }
    }
    assert!(found, "sin90_outbox.result_ref column is missing");
}

/// A row inserted the way pre-0013 code would (no `result_ref` in the
/// column list) reads back `NULL` — an existing user's db upgrading through
/// this migration must not end up with some other placeholder value on rows
/// it already had.
#[tokio::test]
async fn migration_0013_existing_style_insert_leaves_result_ref_null() {
    let pool = migrated_pool().await;
    sqlx::query(
        "INSERT INTO sin90_outbox (id, kind, dedup_key, desired, status, created_at, done_at)
         VALUES ('o1', 'scheduler.upsert', 'routine:x', '{}', 'pending', '2026-01-01T00:00:00Z', NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let result_ref: Option<String> =
        sqlx::query_scalar("SELECT result_ref FROM sin90_outbox WHERE id = 'o1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(result_ref, None);
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
