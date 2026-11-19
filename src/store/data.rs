//! Whole-store snapshots and atomic data replacement.
pub use super::templates::templates;
use super::{repo::*, Result, Sin90Store, StoreError};
use crate::core::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{Column, Row, TypeInfo, ValueRef};
use std::{collections::BTreeMap, io::Write};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backup {
    pub format: String,
    pub schema: i64,
    pub exported_at: String,
    pub tables: BTreeMap<String, Vec<BTreeMap<String, Value>>>,
}
fn io(e: std::io::Error) -> StoreError {
    StoreError::Internal(format!("backup I/O: {e}"))
}
fn ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}
async fn tables(tx: &mut Tx<'_>) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type='table' AND substr(name,1,6)='sin90_' ORDER BY name").fetch_all(&mut **tx).await?)
}
async fn schema(tx: &mut Tx<'_>) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE success=1")
            .fetch_one(&mut **tx)
            .await?,
    )
}
async fn snapshot(tx: &mut Tx<'_>) -> Result<Backup> {
    let mut output = BTreeMap::new();
    for table in tables(tx).await? {
        let mut rows = Vec::new();
        for row in sqlx::query(&format!("SELECT * FROM {} ORDER BY rowid", ident(&table)))
            .fetch_all(&mut **tx)
            .await?
        {
            let mut values = BTreeMap::new();
            for column in row.columns() {
                let name = column.name();
                let raw = row.try_get_raw(name)?;
                let value = if raw.is_null() {
                    Value::Null
                } else {
                    match raw.type_info().name() {
                        "INTEGER" => json!(row.try_get::<i64, _>(name)?),
                        "REAL" => json!(row.try_get::<f64, _>(name)?),
                        "BLOB" => json!({"blob_hex": hex::encode(row.try_get::<Vec<u8>,_>(name)?)}),
                        _ => json!(row.try_get::<String, _>(name)?),
                    }
                };
                values.insert(name.to_string(), value);
            }
            rows.push(values);
        }
        output.insert(table, rows);
    }
    Ok(Backup {
        format: "sin90-backup".into(),
        schema: schema(tx).await?,
        exported_at: now_iso8601(),
        tables: output,
    })
}
impl Sin90Store {
    pub async fn export_data(&self) -> Result<Backup> {
        let mut tx = self.pool().begin().await?;
        let backup = snapshot(&mut tx).await?;
        tx.commit().await?;
        Ok(backup)
    }
    pub(super) fn require_backup_directory(&self) -> Result<()> {
        if self.data_dir.is_none() {
            return Err(StoreError::Conflict(
                "data directory required for automatic backups".into(),
            ));
        }
        Ok(())
    }
    fn save_backup(&self, backup: &Backup) -> Result<()> {
        let dir = self
            .data_dir
            .as_ref()
            .ok_or_else(|| {
                StoreError::Conflict("data directory required for automatic backups".into())
            })?
            .join("backups");
        std::fs::create_dir_all(&dir).map_err(io)?;
        let name = format!(
            "auto-{}-{}.json",
            backup.exported_at.replace(':', "-"),
            ulid()
        );
        let temp = dir.join(format!(".{name}.tmp"));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(io)?;
        file.write_all(&serde_json::to_vec(backup)?).map_err(io)?;
        file.sync_all().map_err(io)?;
        std::fs::rename(&temp, dir.join(&name)).map_err(io)?;
        std::fs::File::open(&dir)
            .and_then(|f| f.sync_all())
            .map_err(io)?;
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .map_err(io)?
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(io)?
            .into_iter()
            .filter(|e| {
                e.file_name().to_string_lossy().starts_with("auto-")
                    && e.path().extension().is_some_and(|x| x == "json")
            })
            .collect();
        names.sort_by_key(|e| e.file_name());
        let remove = names.len().saturating_sub(10);
        for entry in names.into_iter().take(remove) {
            std::fs::remove_file(entry.path()).map_err(io)?;
        }
        Ok(())
    }
    pub async fn import_data(&self, backup: &Backup) -> Result<()> {
        self.require_backup_directory()?;
        self.replace_data(backup, true, false).await
    }
    pub(super) async fn replace_data(
        &self,
        backup: &Backup,
        save: bool,
        autoload: bool,
    ) -> Result<()> {
        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;
        if backup.format != "sin90-backup" || backup.schema != schema(&mut tx).await? {
            return Err(StoreError::Invalid(
                "unsupported backup format or schema; export with the matching Sin90 version"
                    .into(),
            ));
        }
        let names = tables(&mut tx).await?;
        if backup.tables.keys().any(|t| !names.contains(t)) {
            return Err(StoreError::Invalid("unknown backup table".into()));
        }
        if autoload && !empty_for_autoload(&mut tx).await? {
            mark_autoload_done(&mut tx).await?;
            tx.commit().await?;
            return Ok(());
        }
        let before = snapshot(&mut tx).await?;
        if save {
            self.save_backup(&before)?;
        }
        // Deferred FK checks allow self-referencing tasks/plans in any row order.
        sqlx::query("PRAGMA defer_foreign_keys=ON")
            .execute(&mut *tx)
            .await?;
        for name in &names {
            if name == "sin90_attention_watermark" {
                continue;
            }
            sqlx::query(&format!("DELETE FROM {}", ident(name)))
                .execute(&mut *tx)
                .await?;
        }
        for row in &before.tables["sin90_settings"] {
            if row["key"]
                .as_str()
                .is_some_and(|key| key.starts_with("ai."))
            {
                sqlx::query("INSERT INTO sin90_settings(key,value,updated_at) VALUES(?,?,?)")
                    .bind(row["key"].as_str())
                    .bind(row["value"].as_str())
                    .bind(row["updated_at"].as_str())
                    .execute(&mut *tx)
                    .await?;
            }
        }
        for (table, rows) in &backup.tables {
            if table == "sin90_outbox" || table == "sin90_attention_watermark" {
                continue;
            }
            let columns: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info(?)")
                .bind(table)
                .fetch_all(&mut *tx)
                .await?;
            for row in rows {
                if table == "sin90_settings" {
                    let key = row
                        .get("key")
                        .and_then(Value::as_str)
                        .ok_or_else(|| StoreError::Invalid("invalid setting key".into()))?;
                    if key != "review"
                        && key != "progress.streak_weeks"
                        && !key.starts_with("template.")
                    {
                        continue;
                    }
                }
                if row.is_empty() || row.keys().any(|c| !columns.contains(c)) {
                    return Err(StoreError::Invalid("invalid backup columns".into()));
                }
                let cols = row.keys().map(|c| ident(c)).collect::<Vec<_>>().join(",");
                let placeholders = vec!["?"; row.len()].join(",");
                let sql = format!(
                    "INSERT INTO {} ({cols}) VALUES ({placeholders})",
                    ident(table)
                );
                let mut query = sqlx::query(&sql);
                for value in row.values() {
                    query =
                        match value {
                            Value::Null => query.bind(None::<String>),
                            Value::String(v) => query.bind(v),
                            Value::Number(v) if v.is_i64() => query.bind(v.as_i64().unwrap()),
                            Value::Number(v) => query.bind(
                                v.as_f64()
                                    .ok_or_else(|| StoreError::Invalid("invalid number".into()))?,
                            ),
                            Value::Object(v) if v.len() == 1 && v.contains_key("blob_hex") => query
                                .bind(
                                    hex::decode(v["blob_hex"].as_str().ok_or_else(|| {
                                        StoreError::Invalid("invalid blob".into())
                                    })?)
                                    .map_err(|_| StoreError::Invalid("invalid blob".into()))?,
                                ),
                            _ => return Err(StoreError::Invalid("invalid backup cell".into())),
                        };
                }
                query
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| StoreError::Invalid(format!("invalid backup row: {e}")))?;
            }
        }
        for r in sqlx::query("SELECT cron,tz FROM sin90_routines")
            .fetch_all(&mut *tx)
            .await?
        {
            validate_cron(&r.get::<String, _>("cron")).map_err(StoreError::Invalid)?;
            validate_tz(&r.get::<String, _>("tz")).map_err(StoreError::Invalid)?;
        }
        sqlx::query(include_str!("migrations/0014_triage_direction.sql"))
            .execute(&mut *tx)
            .await?;
        let review: Option<String> =
            sqlx::query_scalar("SELECT value FROM sin90_settings WHERE key='review'")
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(value) = review {
            let settings: super::reviews::ReviewSettings =
                serde_json::from_str(&value).map_err(|e| StoreError::Invalid(e.to_string()))?;
            if !(1..=7).contains(&settings.weekday) || settings.hour > 23 {
                return Err(StoreError::Invalid("invalid review settings".into()));
            }
        }
        super::reviews::review_settings_in(&mut tx, None).await?;
        mark_autoload_done(&mut tx).await?;
        let plan_rows = sqlx::query("SELECT * FROM sin90_period_plans")
            .fetch_all(&mut *tx)
            .await?;
        let mut plans = BTreeMap::new();
        for row in plan_rows {
            let plan = super::period_plans::row_to_plan(row)
                .map_err(|e| StoreError::Invalid(e.to_string()))?;
            validate_plan(&plan.plan).map_err(StoreError::Invalid)?;
            plans.insert(plan.id.clone(), plan);
        }
        for plan in plans.values() {
            let parent = plan.plan.parent_id.as_ref().and_then(|id| plans.get(id));
            validate_parent(&plan.direction_id, &plan.plan, parent).map_err(StoreError::Invalid)?;
        }
        if !sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&mut *tx)
            .await?
            .is_empty()
        {
            return Err(StoreError::Invalid("backup has broken references".into()));
        }
        // Rebuild projections in this same transaction, before publishing state.
        super::attention::rebuild_in(&mut tx).await?;
        let routines = sqlx::query("SELECT id,cron,tz,status FROM sin90_routines")
            .fetch_all(&mut *tx)
            .await?;
        let mut ids = Vec::new();
        for r in routines {
            let id: String = r.get("id");
            let status: String = r.get("status");
            ids.push(id.clone());
            if status != "retired" {
                let desired = routine_outbox_upsert_desired(
                    &id,
                    &r.get::<String, _>("cron"),
                    &r.get::<String, _>("tz"),
                    status == "active",
                );
                upsert_outbox(
                    &mut tx,
                    "scheduler.upsert",
                    &routine_dedup_key(&id),
                    &desired,
                )
                .await?;
            } else {
                upsert_outbox(
                    &mut tx,
                    "scheduler.delete",
                    &routine_dedup_key(&id),
                    &json!({"key":routine_kernel_key(&id)}),
                )
                .await?;
            }
        }
        if let Some(old) = before.tables.get("sin90_routines") {
            for r in old {
                let id = r["id"]
                    .as_str()
                    .ok_or_else(|| StoreError::Internal("routine id".into()))?;
                if !ids.iter().any(|v| v == id) {
                    upsert_outbox(
                        &mut tx,
                        "scheduler.delete",
                        &routine_dedup_key(id),
                        &json!({"key": routine_kernel_key(id)}),
                    )
                    .await?;
                }
            }
        }
        tx.commit().await?;
        self.notify.notify_one();
        Ok(())
    }
    pub async fn restore_backup(&self, name: &str) -> Result<()> {
        self.require_backup_directory()?;
        if name.len() <= 5
            || !name.ends_with(".json")
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
            || name.contains("..")
        {
            return Err(StoreError::Invalid("invalid backup name".into()));
        }
        let dir = self
            .data_dir
            .as_ref()
            .ok_or_else(|| StoreError::Conflict("data directory required".into()))?
            .join("backups");
        let root = dir.canonicalize().map_err(io)?;
        let path = dir.join(name);
        if !path.canonicalize().map_err(io)?.starts_with(&root) {
            return Err(StoreError::Invalid(
                "backup path escapes backups directory".into(),
            ));
        }
        if std::fs::symlink_metadata(&path)
            .map_err(io)?
            .file_type()
            .is_symlink()
        {
            return Err(StoreError::Invalid("backup symlinks are forbidden".into()));
        }
        let backup: Backup = serde_json::from_slice(&std::fs::read(path).map_err(io)?)
            .map_err(|e| StoreError::Invalid(e.to_string()))?;
        self.import_data(&backup).await
    }
    pub async fn data_status(&self) -> Result<Value> {
        let mut tx = self.pool().begin().await?;
        let rows = sqlx::query("SELECT key,value FROM sin90_settings WHERE key IN ('template.id','template.lang','template.loaded_at')").fetch_all(&mut *tx).await?;
        let mut meta = serde_json::Map::new();
        for row in rows {
            let key: String = row.get("key");
            let value: String = row.get("value");
            meta.insert(
                key.trim_start_matches("template.").into(),
                serde_json::from_str::<Value>(&value)?,
            );
        }
        let template = if meta.len() == 3 {
            if let Some(Value::Object(description)) = super::templates::template_description(
                meta["id"].as_str().unwrap_or(""),
                meta["lang"].as_str().unwrap_or(""),
            ) {
                meta.extend(description);
            }
            Value::Object(meta)
        } else {
            Value::Null
        };
        let mut counts = serde_json::Map::new();
        for (key, table) in [
            ("areas", "areas"),
            ("directions", "directions"),
            ("tasks", "tasks"),
            ("plans", "period_plans"),
            ("routines", "routines"),
        ] {
            let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM sin90_{table}"))
                .fetch_one(&mut *tx)
                .await?;
            counts.insert(key.into(), json!(count));
        }
        tx.commit().await?;
        let mut backups = Vec::new();
        if let Some(dir) = &self.data_dir {
            let dir = dir.join("backups");
            if dir.exists() {
                for entry in std::fs::read_dir(dir).map_err(io)? {
                    let entry = entry.map_err(io)?;
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with("auto-")
                        && name.ends_with(".json")
                        && entry.file_type().map_err(io)?.is_file()
                    {
                        let b: Backup =
                            serde_json::from_slice(&std::fs::read(entry.path()).map_err(io)?)?;
                        backups.push(json!({"name":name,"at":b.exported_at,"bytes":entry.metadata().map_err(io)?.len()}));
                    }
                }
            }
        }
        backups.sort_by(|a, b| b["name"].as_str().cmp(&a["name"].as_str()));
        Ok(json!({"template":template,"backups":backups,"counts":counts}))
    }
}
pub(super) async fn mark_autoload_done(tx: &mut Tx<'_>) -> Result<()> {
    sqlx::query("INSERT INTO sin90_settings(key,value,updated_at) VALUES('template.autoload_done','true',?) ON CONFLICT(key) DO UPDATE SET value='true'")
        .bind(now_iso8601()).execute(&mut **tx).await?;
    Ok(())
}

pub(super) async fn empty_for_autoload(tx: &mut Tx<'_>) -> Result<bool> {
    // Include new user tables automatically; exempt only seeds and the events
    // and scheduler projection created by the system review seed itself.
    for table in tables(tx).await? {
        let predicate = match table.as_str() {
            "sin90_attention_watermark" => continue,
            "sin90_directions" => "id != 'sin90-triage'".to_string(),
            "sin90_routines" => format!("id != '{}'", super::reviews::SYSTEM_REVIEW_ID),
            "sin90_settings" => "key != 'review' AND substr(key,1,3) != 'ai.'".to_string(),
            "sin90_events" => format!(
                "NOT ((entity='routine' AND entity_id='{}') OR (entity='settings.review' AND entity_id='review'))",
                super::reviews::SYSTEM_REVIEW_ID
            ),
            "sin90_outbox" => format!(
                "dedup_key != '{}'", routine_dedup_key(super::reviews::SYSTEM_REVIEW_ID)
            ),
            _ => "1".to_string(),
        };
        let exists: bool = sqlx::query_scalar(&format!(
            "SELECT EXISTS(SELECT 1 FROM {} WHERE {predicate})",
            ident(&table)
        ))
        .fetch_one(&mut **tx)
        .await?;
        if exists {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests;
