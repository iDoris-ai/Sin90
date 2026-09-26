//! T5.2.2 (design DESIGN-LIFEOS.md §2 #30): a minimal, self-contained smoke
//! test for migration `0014_triage_direction.sql` — mirrors
//! `tests/migration_0013_smoke.rs`'s own precedent (pre-pr-check SZ-4: a
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

/// The reserved "待定" Direction is seeded by the migration itself — present
/// from the very first boot, so classify's fallback (T5.2.2) never needs a
/// lazy-creation code path. Uses `sin90::core::TRIAGE_DIRECTION_ID` (not a
/// hand-copied literal) so this test fails loudly if the Rust constant and
/// the seeded row's id ever drift apart.
#[tokio::test]
async fn migration_0014_seeds_the_triage_direction() {
    let pool = migrated_pool().await;
    let row = sqlx::query(
        "SELECT title, status, area_id, target_window FROM sin90_directions WHERE id = ?",
    )
    .bind(sin90::core::TRIAGE_DIRECTION_ID)
    .fetch_optional(&pool)
    .await
    .unwrap();
    let row = row.expect("migration 0014 must seed the sin90-triage row");
    assert_eq!(row.get::<String, _>("title"), "待定");
    assert_eq!(row.get::<String, _>("status"), "active");
    assert_eq!(
        row.get::<Option<String>, _>("area_id"),
        None,
        "the triage Direction must not belong to any Area (design §2 #30)"
    );
}

/// L2 (coordinator review, 2026-09-26 round 2): exercises the migration's
/// `ON CONFLICT(id) DO NOTHING` DIRECTLY — `sqlx::migrate!` itself never
/// re-runs an already-applied migration file (checksum-tracked), so calling
/// `.run(&pool)` a second time would prove nothing about that clause. This
/// re-issues the EXACT same INSERT statement `0014_triage_direction.sql`
/// ran once already (via `migrated_pool()`) against the SAME pool: it must
/// not error (a bare `INSERT` would hit the `id` PRIMARY KEY constraint) and
/// must not duplicate the row.
#[tokio::test]
async fn migration_0014_insert_is_idempotent_on_rerun() {
    let pool = migrated_pool().await;
    // Re-run the migration file's OWN contents (not a hand-copied string),
    // so deleting its `ON CONFLICT(id) DO NOTHING` turns this test red —
    // PR-Daemon review of #65: a hand-maintained copy passed even with the
    // clause removed from the real file.
    let migration_sql = include_str!("../src/store/migrations/0014_triage_direction.sql");
    assert!(
        migration_sql.contains("INSERT INTO sin90_directions"),
        "include_str! must point at the real 0014 seed migration"
    );
    // Second run of the identical statement the migration already applied once.
    sqlx::raw_sql(migration_sql).execute(&pool).await.unwrap();

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_directions WHERE id = ?")
        .bind(sin90::core::TRIAGE_DIRECTION_ID)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        count, 1,
        "a second identical INSERT must not duplicate the seeded row"
    );
}

/// Positive control for the assertion above: a made-up id that was NEVER
/// seeded reads back no row at all — proves `fetch_optional` + `.expect`
/// above is actually load-bearing, not vacuously true for any id.
#[tokio::test]
async fn migration_0014_does_not_seed_an_unrelated_id() {
    let pool = migrated_pool().await;
    let row = sqlx::query("SELECT 1 FROM sin90_directions WHERE id = 'not-a-real-direction'")
        .fetch_optional(&pool)
        .await
        .unwrap();
    assert!(row.is_none());
}
