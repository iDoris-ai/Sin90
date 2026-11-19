use super::{
    data::empty_for_autoload, period_plans::write_plan, reviews::system_tz_name, Result,
    Sin90Store, StoreError,
};
use crate::core::*;
use jiff::{tz::TimeZone, Timestamp};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;

const EN: &str = include_str!("../../templates/graduate.en.json");
const TH: &str = include_str!("../../templates/graduate.th.json");
const ZH: &str = include_str!("../../templates/graduate.zh.json");
fn template(id: &str, lang: &str) -> Result<Template> {
    let source = match (id, lang) {
        ("graduate", "zh") => ZH,
        ("graduate", "en") => EN,
        ("graduate", "th") => TH,
        _ => {
            return Err(StoreError::Invalid(
                "unknown template or unavailable language".into(),
            ))
        }
    };
    Ok(serde_json::from_str(source)?)
}
pub fn templates() -> Value {
    json!([{"id":"graduate","langs":["zh","en","th"],"title":serde_json::from_str::<Value>(ZH).expect("embedded template")["title"]}])
}
#[derive(Deserialize)]
struct Template {
    id: String,
    lang: String,
    areas: Vec<SeedArea>,
    routines: Vec<SeedRoutine>,
    today: Vec<SeedTask>,
    inbox: Vec<String>,
    week_blocks: Vec<SeedBlock>,
}
#[derive(Deserialize)]
struct SeedArea {
    key: String,
    title: String,
    current_state: String,
    ideal_state: String,
    directions: Vec<SeedDirection>,
}
#[derive(Deserialize)]
struct SeedDirection {
    key: String,
    title: String,
    goal_3y: String,
    years: Vec<SeedPlan>,
    quarter: SeedPlan,
    month: SeedPlan,
}
#[derive(Deserialize)]
struct SeedPlan {
    target: String,
    metric: PlanMetric,
    metric_target: Option<i64>,
    routine: Option<String>,
}
#[derive(Deserialize)]
struct SeedRoutine {
    key: String,
    title: String,
    kind: RoutineKind,
    cron: String,
    area: String,
    direction: String,
    target_minutes: Option<u32>,
    target_count: Option<u32>,
}
#[derive(Deserialize)]
struct SeedTask {
    title: String,
    direction: String,
    energy: Energy,
    est_minutes: u32,
    #[serde(default)]
    kind: Option<TaskKind>,
}
#[derive(Deserialize)]
struct SeedBlock {
    direction: String,
    minutes: u32,
}
pub(super) fn locale_lang(locale: &str) -> &'static str {
    if locale.is_empty() || locale.starts_with("zh") {
        "zh"
    } else if locale.starts_with("th") {
        "th"
    } else {
        "en"
    }
}
pub(super) fn template_description(id: &str, lang: &str) -> Option<Value> {
    let source = match (id, lang) {
        ("graduate", "zh") => ZH,
        ("graduate", "en") => EN,
        ("graduate", "th") => TH,
        _ => return None,
    };
    let v: Value = serde_json::from_str(source).ok()?;
    Some(json!({"title":v["title"],"guide":v["guide"]}))
}
impl Sin90Store {
    /// Called once at service startup, before accepting requests or starting the pump.
    pub async fn initialize_data(&self) -> Result<()> {
        self.get_review_settings().await?;
        let mut tx = self.pool().begin().await?;
        let empty = empty_for_autoload(&mut tx).await?;
        tx.commit().await?;
        if empty {
            let locale = std::env::var("LC_ALL")
                .ok()
                .filter(|s| !s.is_empty())
                .or_else(|| std::env::var("LANG").ok())
                .unwrap_or_default();
            let lang = locale_lang(&locale);
            let staged = Self::stage_template(Some("graduate"), lang).await?;
            self.replace_data(&staged.export_data().await?, false, true)
                .await?;
        }
        Ok(())
    }
    pub async fn reset_data(&self, id: Option<&str>, lang: Option<&str>) -> Result<()> {
        self.require_backup_directory()?;
        let staged = Self::stage_template(id, lang.unwrap_or("zh")).await?;
        self.replace_data(&staged.export_data().await?, true, false)
            .await
    }
    async fn stage_template(id: Option<&str>, lang: &str) -> Result<Self> {
        let parsed = id.map(|id| template(id, lang)).transpose()?;
        let store = Self::open_memory().await?;
        store.get_review_settings().await?;
        // Mark initialization before staging any template writes. The target only
        // sees this staging database after one atomic replacement transaction.
        store
            .template_setting("template.autoload_done", &json!(true))
            .await?;
        if let Some(t) = parsed {
            store.load_template(t).await?;
        }
        store.attention_apply_new_events().await?;
        Ok(store)
    }
    async fn template_setting(&self, key: &str, value: &Value) -> Result<()> {
        sqlx::query("INSERT INTO sin90_settings(key,value,updated_at) VALUES(?,?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at").bind(key).bind(serde_json::to_string(value)?).bind(now_iso8601()).execute(self.pool()).await?;
        Ok(())
    }
    async fn load_template(&self, t: Template) -> Result<()> {
        let now = Timestamp::now().to_zoned(TimeZone::system());
        let year = now.year();
        let mut areas = BTreeMap::new();
        let mut directions = BTreeMap::new();
        let mut routines = BTreeMap::new();
        for a in &t.areas {
            let area = self.create_area(&a.title).await?;
            self.update_area(
                &area.id,
                &AreaPatch {
                    current_state: Some(Some(a.current_state.clone())),
                    ideal_state: Some(Some(a.ideal_state.clone())),
                    ..Default::default()
                },
            )
            .await?;
            for d in &a.directions {
                let direction = self
                    .create_direction(&d.title, &year.to_string(), Some(&area.id))
                    .await?;
                self.update_direction(
                    &direction.id,
                    &DirectionPatch {
                        goal_3y: Some(Some(d.goal_3y.clone())),
                        goal_from_year: Some(Some(i32::from(year))),
                        status: Some(DirectionStatus::Active),
                        ..Default::default()
                    },
                )
                .await?;
                directions.insert(d.key.clone(), direction.id);
            }
            areas.insert(a.key.clone(), area.id);
        }
        for r in t.routines {
            let routine = self
                .create_routine(&NewRoutine {
                    title: r.title,
                    area_id: Some(areas[&r.area].clone()),
                    direction_id: Some(directions[&r.direction].clone()),
                    kind: r.kind,
                    cron: r.cron,
                    tz: Some(system_tz_name(&TimeZone::system(), Timestamp::now())),
                    target_count: r.target_count,
                    target_minutes: r.target_minutes,
                })
                .await?;
            routines.insert(r.key, routine.id);
        }
        for a in t.areas {
            for d in a.directions {
                let mut parent = None;
                let mut plans: Vec<_> = d
                    .years
                    .into_iter()
                    .enumerate()
                    .map(|(i, p)| (PlanKind::Year, format!("{}", i32::from(year) + i as i32), p))
                    .collect();
                plans.push((
                    PlanKind::Quarter,
                    format!("{year}-Q{}", (now.month() - 1) / 3 + 1),
                    d.quarter,
                ));
                plans.push((
                    PlanKind::Month,
                    format!("{year}-{:02}", now.month()),
                    d.month,
                ));
                for (kind, period, p) in plans {
                    let plan = NewPeriodPlan {
                        kind,
                        period,
                        parent_id: if kind == PlanKind::Year {
                            None
                        } else {
                            parent.clone()
                        },
                        target: p.target,
                        metric: p.metric,
                        metric_target: p.metric_target,
                        routine_id: p.routine.map(|r| routines[&r].clone()),
                        status: PlanStatus::Active,
                    };
                    let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;
                    let created = write_plan(
                        &mut tx,
                        &directions[&d.key],
                        &plan,
                        PlanSource::Ai,
                        None,
                        &mut Vec::new(),
                    )
                    .await?
                    .expect("new template plan");
                    tx.commit().await?;
                    if (kind == PlanKind::Year && parent.is_none()) || kind == PlanKind::Quarter {
                        parent = Some(created.id);
                    }
                }
            }
        }
        let iso = now.date().iso_week_date();
        let week = self
            .create_week(&format!("{:04}-W{:02}", iso.year(), iso.week()))
            .await?;
        self.transition_week(&week.id, WeekStatus::Active).await?;
        for task in t.today {
            let task = self
                .create_task_in_week(
                    &task.title,
                    Some(&directions[&task.direction]),
                    None,
                    task.kind.unwrap_or(TaskKind::Other),
                    task.energy,
                    Some(task.est_minutes),
                    Some(&week.id),
                )
                .await?;
            self.transition_task(&task.id, TaskStatus::Planned).await?;
        }
        for text in t.inbox {
            self.create_task(&text, None, None, TaskKind::Other, Energy::Mid, None)
                .await?;
        }
        for b in t.week_blocks {
            self.create_block(Some(&directions[&b.direction]), None, b.minutes, None)
                .await?;
        }
        self.template_setting("template.id", &json!(t.id)).await?;
        self.template_setting("template.lang", &json!(t.lang))
            .await?;
        self.template_setting("template.loaded_at", &json!(now_iso8601()))
            .await?;
        Ok(())
    }
}
