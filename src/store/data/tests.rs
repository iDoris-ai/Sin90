use super::*;
use crate::store::reviews::{system_tz_name, SYSTEM_REVIEW_ID};
use jiff::{tz::TimeZone, Timestamp};

struct TempDir(std::path::PathBuf);
impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("sin90-data-{}", ulid()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    async fn store(&self) -> Sin90Store {
        Sin90Store::open(&self.0.join("sin90.db")).await.unwrap()
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
async fn canonical(store: &Sin90Store) -> BTreeMap<String, Vec<BTreeMap<String, Value>>> {
    let mut tables = store.export_data().await.unwrap().tables;
    for rows in tables.values_mut() {
        rows.sort_by_key(|r| serde_json::to_string(r).unwrap());
    }
    tables
}

#[tokio::test]
async fn fresh_start_loads_once_and_resolves_calendar_and_links() {
    let dir = TempDir::new();
    let store = dir.store().await;
    store.initialize_data().await.unwrap();
    let status = store.data_status().await.unwrap();
    assert_eq!(
        status["counts"],
        json!({"areas":6,"directions":7,"tasks":7,"plans":30,"routines":7})
    );
    assert_eq!(status["template"]["id"], "graduate");
    let now = Timestamp::now().to_zoned(TimeZone::system());
    let iso = now.date().iso_week_date();
    let weeks = store.list_weeks().await.unwrap();
    assert_eq!(weeks.len(), 1);
    assert_eq!(
        weeks[0].iso_week,
        format!("{:04}-W{:02}", iso.year(), iso.week())
    );
    assert_eq!(store.list_blocks().await.unwrap().len(), 4);
    assert_eq!(
        store
            .list_blocks()
            .await
            .unwrap()
            .iter()
            .map(|b| b.planned_minutes)
            .sum::<u32>(),
        315
    );
    let today = store.today_view().await.unwrap();
    assert_eq!(today.must_do.len(), 3);
    assert_eq!(today.inbox.len(), 4);
    for task in today.must_do {
        assert_eq!(task.status, TaskStatus::Planned);
        assert_eq!(task.week_id.as_deref(), Some(weeks[0].id.as_str()));
        assert!(task.est_minutes.is_some());
    }
    for area in store.list_areas().await.unwrap() {
        assert!(area.current_state.is_some());
        assert!(area.ideal_state.is_some());
    }
    let all = store.list_period_plans(None, None, None).await.unwrap();
    let routines = store.list_routines(None, None, None).await.unwrap();
    for r in &routines {
        assert_eq!(r.tz, system_tz_name(&TimeZone::system(), Timestamp::now()));
        assert_eq!(r.cron.split_whitespace().count(), 5);
    }
    for d in store
        .list_directions()
        .await
        .unwrap()
        .into_iter()
        .filter(|d| d.id != TRIAGE_DIRECTION_ID)
    {
        assert_eq!(d.goal_from_year, Some(i32::from(now.year())));
        assert_eq!(d.status, DirectionStatus::Active);
        let plans: Vec<_> = all.iter().filter(|p| p.direction_id == d.id).collect();
        assert_eq!(plans.len(), 5);
        for p in &plans {
            assert_eq!(p.source, PlanSource::Ai);
            assert_eq!(p.plan.status, PlanStatus::Active);
        }
        let years: Vec<_> = plans
            .iter()
            .filter(|p| p.plan.kind == PlanKind::Year)
            .collect();
        assert_eq!(
            years
                .iter()
                .map(|p| p.plan.period.clone())
                .collect::<Vec<_>>(),
            (0..3)
                .map(|n| (i32::from(now.year()) + n).to_string())
                .collect::<Vec<_>>()
        );
        let q = plans
            .iter()
            .find(|p| p.plan.kind == PlanKind::Quarter)
            .unwrap();
        let m = plans
            .iter()
            .find(|p| p.plan.kind == PlanKind::Month)
            .unwrap();
        assert_eq!(
            q.plan.period,
            format!("{}-Q{}", now.year(), (now.month() - 1) / 3 + 1)
        );
        assert_eq!(q.plan.parent_id.as_deref(), Some(years[0].id.as_str()));
        assert_eq!(m.plan.parent_id.as_deref(), Some(q.id.as_str()));
        assert_eq!(m.plan.period, format!("{}-{:02}", now.year(), now.month()));
        if let Some(id) = &m.plan.routine_id {
            assert!(routines
                .iter()
                .any(|r| &r.id == id && r.direction_id.as_deref() == Some(&d.id)));
        }
        assert_eq!(d.target_window, now.year().to_string());
    }
    let before = canonical(&store).await;
    let reopened = dir.store().await;
    reopened.initialize_data().await.unwrap();
    assert_eq!(before, canonical(&reopened).await);
}

#[tokio::test]
async fn existing_user_data_prevents_autoload_in_each_case() {
    for case in 0..3 {
        let dir = TempDir::new();
        let store = dir.store().await;
        match case {
            0 => {
                store.create_area("mine").await.unwrap();
            }
            1 => {
                store
                    .create_direction("mine", "ongoing", None)
                    .await
                    .unwrap();
            }
            _ => {
                store
                    .create_task("mine", None, None, TaskKind::Other, Energy::Mid, None)
                    .await
                    .unwrap();
            }
        }
        let before = canonical(&store).await;
        store.initialize_data().await.unwrap();
        let mut after = canonical(&store).await;
        let settings = after.get_mut("sin90_settings").unwrap();
        assert!(settings
            .iter()
            .any(|r| r["key"] == "template.autoload_done" && r["value"] == "true"));
        settings.retain(|r| r["key"] != "template.autoload_done");
        assert_eq!(before, after);
        assert!(store.data_status().await.unwrap()["template"].is_null());
    }
}

#[tokio::test]
async fn roundtrip_preserves_all_rows_and_progress_with_completion_events() {
    let dir = TempDir::new();
    let store = dir.store().await;
    store
        .reset_data(Some("graduate"), Some("zh"))
        .await
        .unwrap();
    let block = store.list_blocks().await.unwrap().remove(0);
    store
        .transition_block(&block.id, ScheduleBlockStatus::Started)
        .await
        .unwrap();
    store
        .transition_block(&block.id, ScheduleBlockStatus::Completed)
        .await
        .unwrap();
    store.attention_apply_new_events().await.unwrap();
    let backup = store.export_data().await.unwrap();
    assert!(backup.tables.keys().all(|k| k.starts_with("sin90_")));
    assert!(backup.tables.contains_key("sin90_events"));
    let mut before = canonical(&store).await;
    // The amended plan explicitly regenerates outbox rather than importing it.
    before.remove("sin90_outbox");
    let mut progress = BTreeMap::new();
    for d in store.list_directions().await.unwrap() {
        progress.insert(d.id.clone(), store.progress(&d.id, None).await.unwrap());
    }
    store.reset_data(None, None).await.unwrap();
    assert!(store.list_areas().await.unwrap().is_empty());
    store.import_data(&backup).await.unwrap();
    let mut after = canonical(&store).await;
    after.remove("sin90_outbox");
    assert_eq!(before, after);
    for (id, p) in progress {
        assert_eq!(p, store.progress(&id, None).await.unwrap());
    }
    let watermark = store.attention_daily().await.unwrap();
    assert_eq!(watermark.len(), 1);
}

#[tokio::test]
async fn reset_backs_up_first_retains_ten_seeds_and_stays_blank() {
    let dir = TempDir::new();
    let store = dir.store().await;
    store.create_area("saved before reset").await.unwrap();
    let original = canonical(&store).await;
    store.reset_data(None, None).await.unwrap();
    let status = store.data_status().await.unwrap();
    let first = status["backups"][0]["name"].as_str().unwrap();
    let saved: Backup =
        serde_json::from_slice(&std::fs::read(dir.0.join("backups").join(first)).unwrap()).unwrap();
    let mut rows = saved.tables.clone();
    for v in rows.values_mut() {
        v.sort_by_key(|r| serde_json::to_string(r).unwrap());
    }
    assert_eq!(original, rows);
    assert_eq!(
        status["counts"],
        json!({"areas":0,"directions":1,"tasks":0,"plans":0,"routines":1})
    );
    assert_eq!(
        store.list_directions().await.unwrap()[0].id,
        TRIAGE_DIRECTION_ID
    );
    assert_eq!(
        store.list_routines(None, None, None).await.unwrap()[0].id,
        SYSTEM_REVIEW_ID
    );
    store.initialize_data().await.unwrap();
    assert!(store.list_areas().await.unwrap().is_empty());
    for _ in 0..11 {
        store.reset_data(None, None).await.unwrap();
    }
    assert_eq!(
        store.data_status().await.unwrap()["backups"]
            .as_array()
            .unwrap()
            .len(),
        10
    );
    assert_eq!(
        std::fs::read_dir(dir.0.join("backups")).unwrap().count(),
        10
    );
}

#[tokio::test]
async fn restore_uses_named_snapshot_and_rejects_path_traversal() {
    let dir = TempDir::new();
    let store = dir.store().await;
    store.create_area("recover me").await.unwrap();
    store.reset_data(None, None).await.unwrap();
    let status = store.data_status().await.unwrap();
    let name = status["backups"][0]["name"].as_str().unwrap();
    store.restore_backup(name).await.unwrap();
    assert_eq!(store.list_areas().await.unwrap()[0].title, "recover me");
    assert!(matches!(
        store.restore_backup("../sin90.db").await,
        Err(StoreError::Invalid(_))
    ));
    assert_eq!(
        store.data_status().await.unwrap()["backups"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn invalid_imports_are_atomic_and_newer_schema_is_rejected_before_backup() {
    let dir = TempDir::new();
    let store = dir.store().await;
    store
        .reset_data(Some("graduate"), Some("zh"))
        .await
        .unwrap();
    let before = canonical(&store).await;
    let mut backup = store.export_data().await.unwrap();
    backup.schema += 1;
    let n = store.data_status().await.unwrap()["backups"]
        .as_array()
        .unwrap()
        .len();
    assert!(matches!(
        store.import_data(&backup).await,
        Err(StoreError::Invalid(_))
    ));
    assert_eq!(
        store.data_status().await.unwrap()["backups"]
            .as_array()
            .unwrap()
            .len(),
        n
    );
    backup.schema -= 1;
    backup.tables.get_mut("sin90_tasks").unwrap()[0]
        .insert("direction_id".into(), json!("missing-direction"));
    assert!(matches!(
        store.import_data(&backup).await,
        Err(StoreError::Invalid(_))
    ));
    assert_eq!(before, canonical(&store).await);
}

#[tokio::test]
async fn each_embedded_language_loads_with_the_requested_metadata() {
    let dir = TempDir::new();
    let store = dir.store().await;
    for lang in ["zh", "en", "th"] {
        store
            .reset_data(Some("graduate"), Some(lang))
            .await
            .unwrap();
        let status = store.data_status().await.unwrap();
        assert_eq!(status["template"]["lang"], lang);
        assert_eq!(status["counts"]["plans"], 30);
        assert_eq!(status["counts"]["areas"], 6);
        assert_eq!(store.today_view().await.unwrap().must_do.len(), 3);
    }
}

#[tokio::test]
async fn startup_resynchronizes_system_review_timezone_and_outbox_once() {
    let dir = TempDir::new();
    let store = dir.store().await;
    store.create_area("existing data").await.unwrap();
    let wanted = system_tz_name(&TimeZone::system(), Timestamp::now());
    let old = if wanted == "UTC" {
        "Asia/Bangkok"
    } else {
        "UTC"
    };
    store
        .update_routine(
            SYSTEM_REVIEW_ID,
            &RoutinePatch {
                tz: Some(old.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    store.initialize_data().await.unwrap();
    assert_eq!(
        store.get_routine(SYSTEM_REVIEW_ID).await.unwrap().tz,
        wanted
    );
    let outbox: String=sqlx::query_scalar("SELECT desired FROM sin90_outbox WHERE dedup_key=? AND status='pending' AND kind='scheduler.upsert'").bind(routine_dedup_key(SYSTEM_REVIEW_ID)).fetch_one(store.pool()).await.unwrap();
    let desired: Value = serde_json::from_str(&outbox).unwrap();
    assert_eq!(desired["spec"]["tz"], wanted);
    let before = canonical(&store).await;
    store.initialize_data().await.unwrap();
    assert_eq!(before, canonical(&store).await);
}

#[tokio::test]
async fn import_resends_active_upserts_and_removed_routine_deletes() {
    let dir = TempDir::new();
    let store = dir.store().await;
    store
        .reset_data(Some("graduate"), Some("zh"))
        .await
        .unwrap();
    let backup = store.export_data().await.unwrap();
    let extra = store
        .create_routine(&NewRoutine {
            title: "extra".into(),
            area_id: None,
            direction_id: None,
            kind: RoutineKind::Other,
            cron: "0 9 * * *".into(),
            tz: Some("UTC".into()),
            target_count: None,
            target_minutes: None,
        })
        .await
        .unwrap();
    store.import_data(&backup).await.unwrap();
    let desired:String=sqlx::query_scalar("SELECT desired FROM sin90_outbox WHERE dedup_key=? AND kind='scheduler.delete' AND status='pending'").bind(routine_dedup_key(&extra.id)).fetch_one(store.pool()).await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&desired).unwrap()["key"],
        routine_kernel_key(&extra.id)
    );
    assert_eq!(
        store
            .outbox_due_pending(&now_iso8601())
            .await
            .unwrap()
            .iter()
            .filter(|r| r.kind == "scheduler.upsert")
            .count(),
        7
    );
}

#[tokio::test]
async fn backup_failure_prevents_reset_and_import() {
    let dir = TempDir::new();
    let store = dir.store().await;
    store.create_area("must survive").await.unwrap();
    let backup = store.export_data().await.unwrap();
    std::fs::write(dir.0.join("backups"), b"not a directory").unwrap();
    assert!(store.reset_data(None, None).await.is_err());
    assert!(store.import_data(&backup).await.is_err());
    assert_eq!(
        canonical(&store).await["sin90_areas"][0]["title"],
        "must survive"
    );
}

#[tokio::test]
async fn concurrent_startup_only_loads_one_template() {
    let dir = TempDir::new();
    let store = dir.store().await;
    let other = dir.store().await;
    let (a, b) = tokio::join!(store.initialize_data(), other.initialize_data());
    a.unwrap();
    b.unwrap();
    assert_eq!(store.data_status().await.unwrap()["counts"]["areas"], 6);
    assert_eq!(store.data_status().await.unwrap()["counts"]["tasks"], 7);
}

#[tokio::test]
async fn import_filters_settings_rebuilds_outbox_and_sets_autoload_marker() {
    let dir = TempDir::new();
    let store = dir.store().await;
    store
        .reset_data(Some("graduate"), Some("zh"))
        .await
        .unwrap();
    let mut backup = store.export_data().await.unwrap();
    let settings = backup.tables.get_mut("sin90_settings").unwrap();
    settings.retain(|r| r["key"] != "template.autoload_done");
    settings.push(BTreeMap::from([
        ("key".into(), json!("ai.executive_enabled")),
        ("value".into(), json!("true")),
        ("updated_at".into(), json!(now_iso8601())),
    ]));
    // Untrusted serialized outbox content must never be executed.
    backup.tables.get_mut("sin90_outbox").unwrap().clear();
    store.import_data(&backup).await.unwrap();
    let restored = store.export_data().await.unwrap();
    let settings = &restored.tables["sin90_settings"];
    assert!(!settings.iter().any(|r| r["key"] == "ai.executive_enabled"));
    assert!(settings
        .iter()
        .any(|r| r["key"] == "template.autoload_done" && r["value"] == "true"));
    assert_eq!(restored.tables["sin90_outbox"].len(), 7);
    assert_eq!(restored.tables["sin90_attention_watermark"].len(), 1);
}

#[tokio::test]
async fn import_validates_routine_schedule_and_plan_semantics_and_exact_schema() {
    let dir = TempDir::new();
    let store = dir.store().await;
    store
        .reset_data(Some("graduate"), Some("zh"))
        .await
        .unwrap();
    let backup = store.export_data().await.unwrap();
    let before = canonical(&store).await;
    for case in 0..5 {
        let mut bad = backup.clone();
        match case {
            0 => bad.schema -= 1,
            1 => {
                bad.tables.get_mut("sin90_routines").unwrap()[0]
                    .insert("cron".into(), json!("broken"));
            }
            2 => {
                bad.tables.get_mut("sin90_routines").unwrap()[0]
                    .insert("tz".into(), json!("No/Such_Zone"));
            }
            3 => {
                bad.tables.get_mut("sin90_period_plans").unwrap()[0]
                    .insert("target".into(), json!(""));
            }
            _ => {
                let p = bad
                    .tables
                    .get_mut("sin90_period_plans")
                    .unwrap()
                    .iter_mut()
                    .find(|p| p["kind"] == "month")
                    .unwrap();
                p.insert("parent_id".into(), Value::Null);
            }
        }
        assert!(
            matches!(store.import_data(&bad).await, Err(StoreError::Invalid(_))),
            "case {case}"
        );
        assert_eq!(canonical(&store).await, before, "case {case}");
    }
}

#[test]
fn locale_selection_matches_gui_default_and_supported_languages() {
    for (locale, lang) in [
        ("", "zh"),
        ("zh_CN.UTF-8", "zh"),
        ("th_TH.UTF-8", "th"),
        ("en_US.UTF-8", "en"),
        ("C", "en"),
    ] {
        assert_eq!(super::super::templates::locale_lang(locale), lang);
    }
}

#[tokio::test]
async fn import_reschedules_paused_and_removes_retired_routines() {
    let dir = TempDir::new();
    let store = dir.store().await;
    store
        .reset_data(Some("graduate"), Some("zh"))
        .await
        .unwrap();
    let routines: Vec<_> = store
        .list_routines(None, None, None)
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.id != SYSTEM_REVIEW_ID)
        .collect();
    store
        .transition_routine(&routines[0].id, RoutineStatus::Paused)
        .await
        .unwrap();
    store
        .transition_routine(&routines[1].id, RoutineStatus::Retired)
        .await
        .unwrap();
    let backup = store.export_data().await.unwrap();
    store.import_data(&backup).await.unwrap();
    let outbox = store.outbox_due_pending(&now_iso8601()).await.unwrap();
    let paused = outbox
        .iter()
        .find(|r| r.dedup_key == routine_dedup_key(&routines[0].id))
        .unwrap();
    assert_eq!(paused.kind, "scheduler.upsert");
    assert_eq!(paused.desired["enabled"], false);
    let retired = outbox
        .iter()
        .find(|r| r.dedup_key == routine_dedup_key(&routines[1].id))
        .unwrap();
    assert_eq!(retired.kind, "scheduler.delete");
}

#[tokio::test]
async fn startup_preserves_isolated_legacy_rows_without_loading_template() {
    // Raw inserts deliberately avoid companion events: each table must prevent
    // autoload on its own, even when an old database has no event history.
    for insert in [
        "INSERT INTO sin90_schedule_blocks(id,status,planned_minutes,created_at,updated_at) VALUES('mine','planned',30,'old','old')",
        "INSERT INTO sin90_routines(id,title,kind,cron,tz,status,created_at,updated_at) VALUES('mine','mine','other','0 9 * * *','UTC','active','old','old')",
        "INSERT INTO sin90_reviews(id,kind,period,status,body,created_at,updated_at) VALUES('mine','daily','2026-11-18','draft','keep me','old','old')",
        "INSERT INTO sin90_weeks(id,status,iso_week,created_at,updated_at) VALUES('mine','planning','2026-W47','old','old')",
        "INSERT INTO sin90_period_plans(id,direction_id,kind,period,target,metric,source,status,created_at,updated_at) VALUES('mine','sin90-triage','year','2026','keep me','none','user','active','old','old')",
        "INSERT INTO sin90_events(id,entity,entity_id,kind,payload,at) VALUES('mine','task','gone','created','{}','old')",
        "INSERT INTO sin90_settings(key,value,updated_at) VALUES('progress.streak_weeks','3','old')",
    ] {
        let dir = TempDir::new();
        let store = dir.store().await;
        sqlx::query(insert).execute(store.pool()).await.unwrap();
        let before = canonical(&store).await;
        let reopened = dir.store().await;
        reopened.initialize_data().await.unwrap();
        let mut after = canonical(&reopened).await;
        let settings = after.get_mut("sin90_settings").unwrap();
        assert!(settings.iter().any(|r| r["key"] == "template.autoload_done" && r["value"] == "true"), "{insert}");
        settings.retain(|r| r["key"] != "template.autoload_done");
        assert_eq!(before, after, "{insert}");
        assert!(reopened.data_status().await.unwrap()["template"].is_null());
        reopened.initialize_data().await.unwrap();
        assert!(reopened.list_areas().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn import_and_reset_preserve_local_ai_settings_without_importing_them() {
    let dir = TempDir::new();
    let store = dir.store().await;
    for (key, value) in [("ai.executive_enabled", "true"), ("ai.custom", "local")] {
        sqlx::query("INSERT INTO sin90_settings(key,value,updated_at) VALUES(?,?,'original')")
            .bind(key)
            .bind(value)
            .execute(store.pool())
            .await
            .unwrap();
    }
    let mut backup = store.export_data().await.unwrap();
    let settings = backup.tables.get_mut("sin90_settings").unwrap();
    for row in settings
        .iter_mut()
        .filter(|r| r["key"].as_str().unwrap().starts_with("ai."))
    {
        row.insert("value".into(), json!("untrusted"));
    }
    settings.push(BTreeMap::from([
        ("key".into(), json!("ai.foreign")),
        ("value".into(), json!("true")),
        ("updated_at".into(), json!("foreign")),
    ]));
    for operation in 0..3 {
        match operation {
            0 => store.import_data(&backup).await.unwrap(),
            1 => store.reset_data(None, None).await.unwrap(),
            _ => store
                .reset_data(Some("graduate"), Some("zh"))
                .await
                .unwrap(),
        }
        let rows: Vec<(String, String, String)> = sqlx::query_as("SELECT key,value,updated_at FROM sin90_settings WHERE substr(key,1,3)='ai.' ORDER BY key")
            .fetch_all(store.pool()).await.unwrap();
        assert_eq!(
            rows,
            vec![
                ("ai.custom".into(), "local".into(), "original".into()),
                (
                    "ai.executive_enabled".into(),
                    "true".into(),
                    "original".into()
                ),
            ]
        );
    }
}

#[tokio::test]
async fn ai_settings_alone_allow_fresh_autoload_and_survive_it() {
    let dir = TempDir::new();
    let store = dir.store().await;
    sqlx::query("INSERT INTO sin90_settings(key,value,updated_at) VALUES('ai.executive_enabled','true','original')")
        .execute(store.pool()).await.unwrap();
    store.initialize_data().await.unwrap();
    assert_eq!(
        store.data_status().await.unwrap()["template"]["id"],
        "graduate"
    );
    let value: String =
        sqlx::query_scalar("SELECT value FROM sin90_settings WHERE key='ai.executive_enabled'")
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(value, "true");
}
