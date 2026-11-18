use sqlx::{sqlite::SqlitePoolOptions, Row};

#[tokio::test]
async fn l2a_migration_preserves_directions_and_enforces_plan_keys() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&pool)
        .await
        .unwrap();
    let migrator = sqlx::migrate!("./src/store/migrations");
    for migration in migrator.iter().filter(|m| m.version < 18) {
        sqlx::raw_sql(&migration.sql).execute(&pool).await.unwrap();
    }
    sqlx::query("INSERT INTO sin90_directions (id, title, status, target_window, created_at, updated_at) VALUES ('d', 'Run', 'draft', '2026', 'now', 'now')").execute(&pool).await.unwrap();
    for migration in migrator.iter().filter(|m| m.version == 18) {
        sqlx::raw_sql(&migration.sql).execute(&pool).await.unwrap();
    }
    let row =
        sqlx::query("SELECT title, goal_3y, goal_from_year FROM sin90_directions WHERE id = 'd'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row.get::<String, _>("title"), "Run");
    assert!(row.get::<Option<String>, _>("goal_3y").is_none());
    assert!(row.get::<Option<i32>, _>("goal_from_year").is_none());
    let insert = "INSERT INTO sin90_period_plans (id, direction_id, kind, period, target, metric, source, status, created_at, updated_at) VALUES (?, ?, 'year', '2026', 'Run', 'none', 'user', 'draft', 'now', 'now')";
    sqlx::query(insert)
        .bind("p")
        .bind("d")
        .execute(&pool)
        .await
        .unwrap();
    assert!(sqlx::query(insert)
        .bind("duplicate")
        .bind("d")
        .execute(&pool)
        .await
        .is_err());
    assert!(sqlx::query(insert)
        .bind("orphan")
        .bind("missing")
        .execute(&pool)
        .await
        .is_err());
    assert!(
        sqlx::query("UPDATE sin90_period_plans SET parent_id = 'missing' WHERE id = 'p'")
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE sin90_period_plans SET routine_id = 'missing' WHERE id = 'p'")
            .execute(&pool)
            .await
            .is_err()
    );
}
