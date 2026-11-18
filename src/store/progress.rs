//! Calendar progress reconstructed from event snapshots and completion events.
use std::collections::BTreeMap;

use jiff::{civil::Date, tz::TimeZone, Timestamp};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Row;

use super::{repo::append_event, Result, Sin90Store, StoreError};
use crate::core::{now_iso8601, PeriodPlan, PlanKind, PlanMetric, PlanStatus};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProgressWeek {
    pub iso_week: String,
    pub target: i64,
    pub actual: i64,
    pub met: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanProgress {
    pub plan_id: String,
    pub actual: Option<i64>,
    pub expected: Option<f64>,
    pub status: String,
    pub weeks: Vec<ProgressWeek>,
    pub miss_streak: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressSettings {
    pub streak_weeks: usize,
}

fn add(day: Date, n: i64) -> Date {
    day.checked_add(jiff::Span::new().days(n))
        .expect("bounded calendar arithmetic")
}
fn days(start: Date, end: Date) -> i64 {
    start.until(end).expect("date interval").get_days().into()
}
fn bounds(p: &PeriodPlan) -> (Date, Date) {
    let year: i16 = p.plan.period[..4].parse().expect("validated period");
    let (month, len) = match p.plan.kind {
        PlanKind::Year => (1, 12),
        PlanKind::Half => (1 + (p.plan.period.as_bytes()[6] - b'1') * 6, 6),
        PlanKind::Quarter => (1 + (p.plan.period.as_bytes()[6] - b'1') * 3, 3),
        PlanKind::Month => (p.plan.period[5..].parse::<u8>().unwrap(), 1),
    };
    let start = Date::new(year, month as i8, 1).unwrap();
    (
        start,
        start
            .checked_add(jiff::Span::new().months(len))
            .map(|end| add(end, -1))
            .unwrap_or(Date::MAX),
    )
}
fn monday(day: Date) -> Date {
    add(day, -i64::from(day.weekday().to_monday_zero_offset()))
}
fn iso_week(day: Date) -> String {
    let iso = day.iso_week_date();
    format!("{:04}-W{:02}", iso.year(), iso.week())
}

pub(super) struct Completion {
    pub day: Date,
    pub direction: String,
    pub routine: Option<String>,
    pub minutes: Option<i64>,
}
fn actual(p: &PeriodPlan, events: &[Completion], start: Date, end: Date) -> i64 {
    events
        .iter()
        .filter(|e| e.day >= start && e.day <= end)
        .filter_map(|e| match p.plan.metric {
            PlanMetric::Minutes if e.direction == p.direction_id => e.minutes,
            PlanMetric::Count => match &p.plan.routine_id {
                Some(id) if e.routine.as_ref() == Some(id) && e.minutes.is_some() => Some(1),
                None if e.direction == p.direction_id && e.minutes.is_none() => Some(1),
                _ => None,
            },
            _ => None,
        })
        .sum()
}

/// Shared calendar-day completion replay for progress and calendar reviews.
pub(super) fn completion_from_event(
    row: &sqlx::sqlite::SqliteRow,
    tasks: &mut BTreeMap<String, String>,
    tz: &TimeZone,
) -> Result<Option<Completion>> {
    let entity: String = row.get("entity");
    let id: String = row.get("entity_id");
    let kind: String = row.get("kind");
    let payload: Value = serde_json::from_str(&row.get::<String, _>("payload"))?;
    if let ("task", Some(d)) = (entity.as_str(), payload.get("direction_id")) {
        tasks.insert(id.clone(), d.as_str().unwrap_or("").into());
    }
    let state: Option<String> = row.get("to_state");
    if kind != "transitioned"
        || !((entity == "block" && state.as_deref() == Some("completed"))
            || (entity == "task" && state.as_deref() == Some("done")))
    {
        return Ok(None);
    }
    let timestamp: Timestamp = row
        .get::<String, _>("at")
        .parse()
        .map_err(|e| StoreError::Internal(format!("invalid event timestamp: {e}")))?;
    Ok(Some(Completion {
        day: timestamp.to_zoned(tz.clone()).date(),
        direction: if entity == "task" {
            tasks.get(&id).cloned().unwrap_or_default()
        } else {
            payload["direction_id"].as_str().unwrap_or("").into()
        },
        routine: payload["routine_id"].as_str().map(str::to_owned),
        minutes: if entity == "block" {
            Some(payload["minutes"].as_i64().unwrap_or(0))
        } else {
            None
        },
    }))
}

impl Sin90Store {
    pub async fn get_progress_settings(&self) -> Result<ProgressSettings> {
        let value: Option<String> = sqlx::query_scalar(
            "SELECT value FROM sin90_settings WHERE key = 'progress.streak_weeks'",
        )
        .fetch_optional(self.pool())
        .await?;
        let streak_weeks = value
            .map(|v| serde_json::from_str(&v))
            .transpose()?
            .unwrap_or(3);
        if streak_weeks == 0 {
            return Err(StoreError::Invalid("streak_weeks must be positive".into()));
        }
        Ok(ProgressSettings { streak_weeks })
    }
    pub async fn put_progress_settings(
        &self,
        settings: ProgressSettings,
    ) -> Result<ProgressSettings> {
        if settings.streak_weeks == 0 {
            return Err(StoreError::Invalid("streak_weeks must be positive".into()));
        }
        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;
        let now = now_iso8601();
        sqlx::query("INSERT INTO sin90_settings (key, value, updated_at) VALUES ('progress.streak_weeks', ?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at")
            .bind(settings.streak_weeks.to_string()).bind(&now).execute(&mut *tx).await?;
        append_event(
            &mut tx,
            "setting",
            "progress.streak_weeks",
            "changed",
            None,
            None,
            &serde_json::json!({"key":"progress.streak_weeks", "value":settings.streak_weeks}),
            &now,
        )
        .await?;
        tx.commit().await?;
        Ok(settings)
    }
    pub async fn progress(
        &self,
        direction_id: &str,
        at: Option<&str>,
    ) -> Result<Vec<PlanProgress>> {
        self.progress_in(direction_id, at, TimeZone::system()).await
    }

    async fn progress_in(
        &self,
        direction_id: &str,
        at: Option<&str>,
        tz: TimeZone,
    ) -> Result<Vec<PlanProgress>> {
        let at: Date = match at {
            Some(s) if s.len() == 10 => s
                .parse()
                .map_err(|_| StoreError::Invalid("at must be YYYY-MM-DD".into()))?,
            Some(_) => return Err(StoreError::Invalid("at must be YYYY-MM-DD".into())),
            None => Timestamp::now().to_zoned(tz.clone()).date(),
        };
        let threshold = self.get_progress_settings().await?.streak_weeks;
        let mut plans = BTreeMap::<String, PeriodPlan>::new();
        let mut routines = BTreeMap::<String, Value>::new();
        let mut tasks = BTreeMap::<String, String>::new();
        let mut completions = Vec::new();
        // One SELECT provides a consistent event-log snapshot, without mutable entity joins.
        let rows = sqlx::query(
            "SELECT entity, entity_id, kind, to_state, payload, at FROM sin90_events ORDER BY seq",
        )
        .fetch_all(self.pool())
        .await?;
        for row in rows {
            let entity: String = row.get("entity");
            let id: String = row.get("entity_id");
            let kind: String = row.get("kind");
            let payload: Value = serde_json::from_str(&row.get::<String, _>("payload"))?;
            if entity == "period_plan" && matches!(kind.as_str(), "created" | "updated") {
                plans.insert(id.clone(), serde_json::from_value(payload.clone())?);
            }
            if entity == "routine" && matches!(kind.as_str(), "created" | "updated") {
                routines.insert(id.clone(), payload.clone());
            }
            if let Some(completion) = completion_from_event(&row, &mut tasks, &tz)? {
                if completion.day <= at {
                    completions.push(completion);
                }
            }
        }
        let plans: Vec<_> = plans
            .into_values()
            .filter(|p| p.direction_id == direction_id)
            .collect();
        let mut result: Vec<PlanProgress> = Vec::new();
        for p in &plans {
            let (start, end) = bounds(p);
            let quantified = p.plan.metric != PlanMetric::None;
            let mut weeks = Vec::new();
            let mut miss_streak = 0;
            let mut week = monday(start);
            while week <= end && week <= at {
                let week_end = week
                    .checked_add(jiff::Span::new().days(6))
                    .unwrap_or(Date::MAX);
                let lo = week.max(start);
                let hi = week_end.min(end);
                // Closed calendar fragments only, including a month ending midweek.
                if hi <= at && quantified {
                    let target = if let Some(id) = &p.plan.routine_id {
                        routines.get(id).and_then(|r| {
                            r[if p.plan.metric == PlanMetric::Minutes {
                                "target_minutes"
                            } else {
                                "target_count"
                            }]
                            .as_i64()
                        })
                    } else {
                        let mut target: i128 = 0;
                        let mut day = lo;
                        let mut complete = false;
                        while day <= hi {
                            let month = plans.iter().find(|m| {
                                m.plan.kind == PlanKind::Month
                                    && m.plan.metric == p.plan.metric
                                    && m.plan.routine_id.is_none()
                                    && {
                                        let (s, e) = bounds(m);
                                        day >= s && day <= e
                                    }
                            });
                            let Some(m) = month else {
                                break;
                            };
                            let (ms, me) = bounds(m);
                            let segment_end = hi.min(me);
                            let Some(t) = m.plan.metric_target else {
                                break;
                            };
                            let n = i128::from(t) * i128::from(days(day, segment_end) + 1);
                            let d = i128::from(days(ms, me) + 1);
                            target += (n + d - 1) / d;
                            if segment_end == hi {
                                complete = true;
                                break;
                            }
                            day = add(segment_end, 1);
                        }
                        if complete {
                            Some(target.min(i128::from(i64::MAX)) as i64)
                        } else {
                            None
                        }
                    };
                    if let Some(target) = target {
                        let actual = actual(p, &completions, lo, hi);
                        miss_streak = if actual >= target { 0 } else { miss_streak + 1 };
                        weeks.push(ProgressWeek {
                            iso_week: iso_week(week),
                            target,
                            actual,
                            met: actual >= target,
                        });
                    } else {
                        // Unknown targets cannot bridge two runs of missed weeks.
                        miss_streak = 0;
                    }
                }
                let Ok(next) = week.checked_add(jiff::Span::new().days(7)) else {
                    break;
                };
                week = next;
            }
            // A dropped plan is the user's decision to stop: it keeps its
            // numbers but raises no risk and passes none upward.
            let tracked = quantified && p.plan.status != PlanStatus::Dropped;
            let status = if !tracked || miss_streak == 0 {
                "on_track"
            } else if miss_streak >= threshold {
                "at_risk"
            } else if p.plan.kind == PlanKind::Month {
                "behind"
            } else {
                "on_track"
            };
            result.push(PlanProgress {
                plan_id: p.id.clone(),
                actual: quantified.then(|| actual(p, &completions, start, end)),
                expected: if quantified {
                    p.plan.metric_target.map(|t| {
                        let elapsed = if at < start {
                            0
                        } else {
                            days(start, at.min(end)) + 1
                        };
                        t as f64 * elapsed as f64 / (days(start, end) + 1) as f64
                    })
                } else {
                    None
                },
                status: status.into(),
                weeks,
                miss_streak,
            });
        }
        // A routine-backed child can have a different measurement from its parent.
        // Propagate the established risk through the explicit plan hierarchy.
        let risky: Vec<_> = plans
            .iter()
            .filter(|p| {
                result
                    .iter()
                    .any(|r| r.plan_id == p.id && r.status == "at_risk")
            })
            .collect();
        for child in risky {
            let mut parent_id = child.plan.parent_id.as_deref();
            while let Some(id) = parent_id {
                let Some(parent) = plans.iter().find(|p| p.id == id) else {
                    break;
                };
                if let Some(row) = result.iter_mut().find(|r| {
                    r.plan_id == id
                        && parent.plan.metric != PlanMetric::None
                        && parent.plan.status != PlanStatus::Dropped
                }) {
                    row.status = "at_risk".into();
                }
                parent_id = parent.plan.parent_id.as_deref();
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::*;

    async fn plans(metric: PlanMetric, month: &str) -> (Sin90Store, String, Vec<PeriodPlan>) {
        let store = Sin90Store::open_memory().await.unwrap();
        let d = store
            .create_direction("Progress", "2026", None)
            .await
            .unwrap();
        let mut result: Vec<PeriodPlan> = Vec::new();
        for (kind, period, target) in [
            (PlanKind::Year, "2026", 365),
            (
                PlanKind::Quarter,
                if month == "2026-03" {
                    "2026-Q1"
                } else {
                    "2026-Q2"
                },
                90,
            ),
            (PlanKind::Month, month, 31),
        ] {
            let p = store
                .create_period_plan(
                    &d.id,
                    &NewPeriodPlan {
                        kind,
                        period: period.into(),
                        parent_id: result.last().map(|p| p.id.clone()),
                        target: "Goal".into(),
                        metric,
                        metric_target: Some(target),
                        routine_id: None,
                        status: PlanStatus::Active,
                    },
                )
                .await
                .unwrap();
            result.push(p);
        }
        (store, d.id, result)
    }
    fn row<'a>(rows: &'a [PlanProgress], p: &PeriodPlan) -> &'a PlanProgress {
        rows.iter().find(|r| r.plan_id == p.id).unwrap()
    }
    async fn completion(
        store: &Sin90Store,
        direction: &str,
        day: &str,
        task: bool,
        routine: Option<&str>,
    ) {
        let mut tx = store.pool().begin().await.unwrap();
        let at = format!("{day}T12:00:00+07:00");
        let entity = if task { "task" } else { "block" };
        let id = ulid();
        if task {
            append_event(
                &mut tx,
                entity,
                &id,
                "created",
                None,
                None,
                &serde_json::json!({"direction_id":direction}),
                &at,
            )
            .await
            .unwrap();
        }
        append_event(
            &mut tx,
            entity,
            &id,
            "transitioned",
            None,
            Some(if task { "done" } else { "completed" }),
            &serde_json::json!({"direction_id":direction,"routine_id":routine,"minutes":20}),
            &at,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    #[tokio::test]
    async fn a_dropped_month_raises_no_risk_and_the_quarter_stays_on_track() {
        let (store, d, p) = plans(PlanMetric::Count, "2026-03").await;
        store
            .update_period_plan(
                &p[2].id,
                &crate::core::PeriodPlanPatch {
                    status: Some(PlanStatus::Dropped),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let rows = store.progress(&d, Some("2026-03-15")).await.unwrap();
        assert_eq!(row(&rows, &p[2]).miss_streak, 3);
        assert_eq!(row(&rows, &p[2]).status, "on_track");
        assert_eq!(row(&rows, &p[1]).status, "on_track");
        assert_eq!(row(&rows, &p[0]).status, "on_track");
    }

    #[tokio::test]
    async fn completions_after_at_are_not_counted() {
        let (store, d, p) = plans(PlanMetric::Count, "2026-03").await;
        completion(&store, &d, "2026-03-20", true, None).await;
        let rows = store.progress(&d, Some("2026-03-10")).await.unwrap();
        assert_eq!(row(&rows, &p[2]).actual, Some(0));
        let rows = store.progress(&d, Some("2026-03-25")).await.unwrap();
        assert_eq!(row(&rows, &p[2]).actual, Some(1));
    }

    #[tokio::test]
    async fn one_missed_week_stays_local_three_weeks_propagate_and_setting_changes_threshold() {
        let (store, d, p) = plans(PlanMetric::Count, "2026-03").await;
        // March 1 is a closed one-day fragment of ISO W09.
        let one = store.progress(&d, Some("2026-03-01")).await.unwrap();
        assert_eq!(row(&one, &p[2]).status, "behind");
        assert_eq!(row(&one, &p[1]).status, "on_track");
        let three = store.progress(&d, Some("2026-03-15")).await.unwrap();
        assert_eq!(row(&three, &p[2]).miss_streak, 3);
        assert_eq!(row(&three, &p[2]).status, "at_risk");
        assert_eq!(row(&three, &p[1]).status, "at_risk");
        store
            .put_progress_settings(ProgressSettings { streak_weeks: 4 })
            .await
            .unwrap();
        assert_eq!(store.get_progress_settings().await.unwrap().streak_weeks, 4);
        assert_eq!(
            row(
                &store.progress(&d, Some("2026-03-15")).await.unwrap(),
                &p[2]
            )
            .status,
            "behind"
        );
        assert!(store
            .put_progress_settings(ProgressSettings { streak_weeks: 0 })
            .await
            .is_err());
    }

    #[tokio::test]
    async fn none_has_no_numbers_or_propagation() {
        let (store, d, p) = plans(PlanMetric::None, "2026-03").await;
        let rows = store.progress(&d, Some("2026-03-29")).await.unwrap();
        for plan in &p {
            let r = row(&rows, plan);
            assert_eq!(r.status, "on_track");
            assert_eq!((r.actual, r.expected, r.miss_streak), (None, None, 0));
            assert!(r.weeks.is_empty());
        }
    }

    #[tokio::test]
    async fn tuesday_month_end_splits_task_counts_and_minutes_by_local_completion_day() {
        for metric in [PlanMetric::Count, PlanMetric::Minutes] {
            let (store, d, p) = plans(metric, "2026-03").await;
            let q = store
                .create_period_plan(
                    &d,
                    &NewPeriodPlan {
                        kind: PlanKind::Quarter,
                        period: "2026-Q2".into(),
                        parent_id: Some(p[0].id.clone()),
                        target: "Q2".into(),
                        metric,
                        metric_target: Some(90),
                        routine_id: None,
                        status: PlanStatus::Active,
                    },
                )
                .await
                .unwrap();
            let april = store
                .create_period_plan(
                    &d,
                    &NewPeriodPlan {
                        kind: PlanKind::Month,
                        period: "2026-04".into(),
                        parent_id: Some(q.id),
                        target: "April".into(),
                        metric,
                        metric_target: Some(30),
                        routine_id: None,
                        status: PlanStatus::Active,
                    },
                )
                .await
                .unwrap();
            for day in ["2026-03-30", "2026-03-31", "2026-04-01"] {
                completion(&store, &d, day, metric == PlanMetric::Count, None).await;
            }
            let rows = store.progress(&d, Some("2026-04-05")).await.unwrap();
            let unit = if metric == PlanMetric::Count { 1 } else { 20 };
            assert_eq!(row(&rows, &p[2]).actual, Some(2 * unit));
            assert_eq!(row(&rows, &april).actual, Some(unit));
            let march_week = row(&rows, &p[2]).weeks.last().unwrap();
            let april_week = row(&rows, &april).weeks.last().unwrap();
            assert_eq!(march_week.iso_week, "2026-W14");
            assert_eq!(april_week.iso_week, "2026-W14");
            assert_eq!((march_week.target, march_week.actual), (2, 2 * unit));
            assert_eq!((april_week.target, april_week.actual), (5, unit));
        }
    }

    #[tokio::test]
    async fn mutable_rows_do_not_change_replayed_progress_and_expected_is_clamped() {
        let (store, d, p) = plans(PlanMetric::Minutes, "2026-03").await;
        let block = store.create_block(Some(&d), None, 20, None).await.unwrap();
        store
            .transition_block(&block.id, ScheduleBlockStatus::Started)
            .await
            .unwrap();
        store
            .transition_block(&block.id, ScheduleBlockStatus::Completed)
            .await
            .unwrap();
        super::super::test_hooks::set_last_event_at(
            &store,
            "block",
            &block.id,
            "2026-03-02T12:00:00Z",
        )
        .await
        .unwrap();
        let before = store.progress(&d, Some("2026-03-15")).await.unwrap();
        assert_eq!(row(&before, &p[2]).actual, Some(20));
        assert_eq!(row(&before, &p[2]).expected, Some(15.0));
        sqlx::query("UPDATE sin90_schedule_blocks SET planned_minutes = 999, status = 'planned'")
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("UPDATE sin90_period_plans SET metric_target = 9999")
            .execute(store.pool())
            .await
            .unwrap();
        assert_eq!(
            store.progress(&d, Some("2026-03-15")).await.unwrap(),
            before
        );
        assert_eq!(
            row(
                &store.progress(&d, Some("2026-02-01")).await.unwrap(),
                &p[2]
            )
            .expected,
            Some(0.0)
        );
        assert_eq!(
            row(
                &store.progress(&d, Some("2026-04-01")).await.unwrap(),
                &p[2]
            )
            .expected,
            Some(31.0)
        );
    }

    #[tokio::test]
    async fn routine_count_uses_only_matching_completed_blocks_and_event_target() {
        let (store, d, p) = plans(PlanMetric::Count, "2026-03").await;
        let mut plan = p[2].clone();
        plan.plan.routine_id = Some("routine".into());
        let mut tx = store.pool().begin().await.unwrap();
        append_event(
            &mut tx,
            "period_plan",
            &plan.id,
            "updated",
            None,
            None,
            &serde_json::to_value(&plan).unwrap(),
            &now_iso8601(),
        )
        .await
        .unwrap();
        append_event(
            &mut tx,
            "routine",
            "routine",
            "created",
            None,
            None,
            &serde_json::json!({"target_count":2,"target_minutes":50}),
            &now_iso8601(),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        completion(&store, &d, "2026-03-02", false, Some("routine")).await;
        completion(&store, &d, "2026-03-03", false, Some("routine")).await;
        completion(&store, &d, "2026-03-04", false, Some("other")).await;
        completion(&store, &d, "2026-03-05", true, None).await;
        let rows = store.progress(&d, Some("2026-03-08")).await.unwrap();
        let r = row(&rows, &plan);
        assert_eq!(r.actual, Some(2));
        assert_eq!(
            r.weeks.last().unwrap(),
            &ProgressWeek {
                iso_week: "2026-W10".into(),
                target: 2,
                actual: 2,
                met: true
            }
        );
        assert_eq!(r.miss_streak, 0);
        // An unfinished current week must not add a miss.
        assert_eq!(
            row(
                &store.progress(&d, Some("2026-03-09")).await.unwrap(),
                &plan
            )
            .miss_streak,
            0
        );
    }

    #[tokio::test]
    async fn timestamp_is_assigned_using_local_calendar_including_offset_boundary() {
        let (store, d, plans) = plans(PlanMetric::Minutes, "2026-03").await;
        let mut tx = store.pool().begin().await.unwrap();
        append_event(
            &mut tx,
            "block",
            "boundary",
            "transitioned",
            None,
            Some("completed"),
            &serde_json::json!({"direction_id":d,"minutes":20}),
            "2026-03-31T18:00:00Z",
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let utc = store
            .progress_in(&d, Some("2026-03-31"), TimeZone::UTC)
            .await
            .unwrap();
        let bangkok = store
            .progress_in(&d, Some("2026-03-31"), TimeZone::fixed(jiff::tz::offset(7)))
            .await
            .unwrap();
        assert_eq!(row(&utc, &plans[2]).actual, Some(20));
        assert_eq!(row(&bangkok, &plans[2]).actual, Some(0));
    }

    #[tokio::test]
    async fn task_count_ignores_mutable_task_status_and_direction() {
        let (store, d, plans) = plans(PlanMetric::Count, "2026-03").await;
        let task = store
            .create_task("Done", Some(&d), None, TaskKind::Other, Energy::Mid, None)
            .await
            .unwrap();
        for state in [
            TaskStatus::Planned,
            TaskStatus::InProgress,
            TaskStatus::Done,
        ] {
            store.transition_task(&task.id, state).await.unwrap();
        }
        super::super::test_hooks::set_last_event_at(
            &store,
            "task",
            &task.id,
            "2026-03-02T12:00:00Z",
        )
        .await
        .unwrap();
        let before = store.progress(&d, Some("2026-03-08")).await.unwrap();
        assert_eq!(row(&before, &plans[2]).actual, Some(1));
        sqlx::query("UPDATE sin90_tasks SET status = 'backlog', direction_id = NULL")
            .execute(store.pool())
            .await
            .unwrap();
        assert_eq!(
            store.progress(&d, Some("2026-03-08")).await.unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn routine_minutes_target_is_replayed_and_risk_reaches_parent_with_different_metric() {
        let (store, d, plans) = plans(PlanMetric::Count, "2026-03").await;
        let routine = store
            .create_routine(&NewRoutine {
                title: "Run".into(),
                area_id: None,
                direction_id: Some(d.clone()),
                kind: RoutineKind::Exercise,
                cron: "0 8 * * mon".into(),
                tz: Some("Asia/Bangkok".into()),
                target_count: Some(2),
                target_minutes: Some(50),
            })
            .await
            .unwrap();
        store
            .update_period_plan(
                &plans[2].id,
                &PeriodPlanPatch {
                    metric: Some(PlanMetric::Minutes),
                    routine_id: Some(Some(routine.id.clone())),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        completion(&store, &d, "2026-03-03", false, Some(&routine.id)).await;
        // Minutes count every completed block in the direction, including unlinked blocks.
        completion(&store, &d, "2026-03-04", false, None).await;
        let before = store.progress(&d, Some("2026-03-15")).await.unwrap();
        let m = row(&before, &plans[2]);
        assert_eq!(m.actual, Some(40));
        assert!(m.weeks.iter().all(|w| w.target == 50));
        assert_eq!(m.status, "at_risk");
        assert_eq!(row(&before, &plans[1]).status, "at_risk");
        sqlx::query("UPDATE sin90_routines SET target_minutes = 1, target_count = 999")
            .execute(store.pool())
            .await
            .unwrap();
        assert_eq!(
            store.progress(&d, Some("2026-03-15")).await.unwrap(),
            before
        );
    }
}
