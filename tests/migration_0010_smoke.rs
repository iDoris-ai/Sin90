//! T5.7.1 (branch a0): a minimal, self-contained smoke test for migration
//! `0010_proposal_rejections.sql` — this branch's ONLY job is the migration
//! and the DESIGN-LIFEOS.md §2 #27/#28 decisions, per pre-pr-check SZ-4 ("a
//! migration must not mix with other changes in one PR"). It deliberately
//! does NOT depend on `Sin90Store` or `store::test_hooks` (both live on the
//! later stacked `feat/t5.7.1a-rejection-store` branch): it runs the SAME
//! `sqlx::migrate!` macro `Sin90Store::open_memory` uses, against a bare
//! in-memory pool it builds itself, and reads the resulting schema back with
//! `pragma_table_info` — proving the table + its full column set exist and
//! have the right nullability, independent of any Rust code that will later
//! read/write it.

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::Row;
use std::collections::HashMap;
use std::str::FromStr;

#[tokio::test]
async fn migration_0010_creates_sin90_proposal_rejections_with_expected_columns() {
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

    let rows =
        sqlx::query("SELECT name, \"notnull\" FROM pragma_table_info('sin90_proposal_rejections')")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(
        !rows.is_empty(),
        "sin90_proposal_rejections table does not exist after running migrations"
    );

    let cols: HashMap<String, i64> = rows
        .iter()
        .map(|r| (r.get::<String, _>("name"), r.get::<i64, _>("notnull")))
        .collect();

    // `id TEXT PRIMARY KEY` — SQLite does not set the `notnull` pragma flag
    // for a non-INTEGER PRIMARY KEY column unless `NOT NULL` is spelled out
    // separately (a documented SQLite quirk, not a bug in the migration):
    // checked for presence only, not nullability.
    assert!(cols.contains_key("id"), "missing column: id");

    // Every other column tasks.md T5.7.1 asked the log to carry, and
    // whether it's required (能力来源 / ops 摘要 / 时间戳) or optional (AI 的理由 /
    // 可选拒绝原因).
    for col in [
        "proposal_id",
        "capability_source",
        "proposal_source",
        "ops_summary",
        "proposed_at",
        "rejected_at",
    ] {
        assert!(cols.contains_key(col), "missing NOT NULL column: {col}");
        assert_eq!(cols[col], 1, "{col} must be NOT NULL");
    }
    for col in ["rationale", "reason"] {
        assert!(cols.contains_key(col), "missing nullable column: {col}");
        assert_eq!(cols[col], 0, "{col} must be nullable");
    }

    assert_eq!(
        cols.len(),
        9,
        "unexpected column set (expected exactly 9): {cols:?}"
    );
}

/// Positive control for the assertion helper itself: a table that does NOT
/// exist yields an EMPTY `pragma_table_info` result (not an error) — so the
/// `!rows.is_empty()` check above is actually load-bearing, not vacuously
/// true.
#[tokio::test]
async fn pragma_table_info_on_a_nonexistent_table_is_empty_not_an_error() {
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

    let rows = sqlx::query("SELECT name FROM pragma_table_info('sin90_table_that_does_not_exist')")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(rows.is_empty());
}
