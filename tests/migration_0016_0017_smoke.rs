use sqlx::{sqlite::SqlitePoolOptions, Row};

#[tokio::test]
async fn l1_additive_migrations_preserve_old_rows_and_enforce_routine_fk() {
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
    for migration in migrator.iter().filter(|m| m.version <= 15) {
        sqlx::raw_sql(&migration.sql).execute(&pool).await.unwrap();
    }
    sqlx::query("INSERT INTO sin90_areas (id, title, slug, status, created_at, updated_at) VALUES ('a', 'Health', 'health', 'active', 'now', 'now')").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO sin90_schedule_blocks (id, status, planned_minutes, created_at, updated_at) VALUES ('b', 'planned', 30, 'now', 'now')").execute(&pool).await.unwrap();
    for migration in migrator.iter().filter(|m| m.version > 15) {
        sqlx::raw_sql(&migration.sql).execute(&pool).await.unwrap();
    }
    let row =
        sqlx::query("SELECT title, current_state, ideal_state FROM sin90_areas WHERE id = 'a'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row.get::<String, _>("title"), "Health");
    assert!(row.get::<Option<String>, _>("current_state").is_none());
    assert!(row.get::<Option<String>, _>("ideal_state").is_none());
    let row =
        sqlx::query("SELECT planned_minutes, routine_id FROM sin90_schedule_blocks WHERE id = 'b'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row.get::<i64, _>("planned_minutes"), 30);
    assert!(row.get::<Option<String>, _>("routine_id").is_none());
    assert!(
        sqlx::query("UPDATE sin90_schedule_blocks SET routine_id = 'missing' WHERE id = 'b'")
            .execute(&pool)
            .await
            .is_err()
    );
}
