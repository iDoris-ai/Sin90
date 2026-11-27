//! Automatic review schedule and calendar review event projections.
use super::{repo::*, Result, Sin90Store, StoreError};
use crate::ai::ports::{SummarizeBucket, SummarizeDraft, SummarizeRoutineRow};
use crate::core::*;
use jiff::{civil::Date, tz::TimeZone, Span, Timestamp};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;
use std::collections::BTreeMap;

// Reserved valid ULID: stable identity makes concurrent initialization idempotent.
/// The system review Routine behind `/settings/review`. Change the schedule
/// through `PUT /settings/review` (human only), not the Routine routes: the
/// next settings write overwrites the cron. `GET /settings/review` is a plain
/// read like the other list routes and needs no human key.
/// The IANA name for the review Routine's `tz`. Under Agent24's module
/// sandbox the zone can be known without its name (no readable
/// `/etc/localtime` link, no `TZ`); then the current offset is written as an
/// `Etc/GMT` zone so 18:00 still fires at local 18:00. Never fails: a missing
/// name must not stop the module from starting.
pub(crate) fn system_tz_name(zone: &TimeZone, now: Timestamp) -> String {
    if let Some(name) = zone.iana_name() {
        return name.to_string();
    }
    let seconds = zone.to_offset(now).seconds();
    if seconds % 3600 != 0 || seconds == 0 {
        return "UTC".to_string();
    }
    // Etc/GMT signs are inverted: UTC+7 is Etc/GMT-7.
    let hours = seconds / 3600;
    format!(
        "Etc/GMT{}{}",
        if hours > 0 { '-' } else { '+' },
        hours.abs()
    )
}

pub const SYSTEM_REVIEW_ID: &str = "00000000000000000000000001";
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewCadence {
    Weekly,
    Monthly,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewSettings {
    pub auto: bool,
    pub cadence: ReviewCadence,
    pub weekday: u8,
    pub hour: u8,
}
impl Default for ReviewSettings {
    fn default() -> Self {
        Self {
            auto: true,
            cadence: ReviewCadence::Weekly,
            weekday: 7,
            hour: 18,
        }
    }
}
impl Sin90Store {
    pub async fn get_review_settings(&self) -> Result<ReviewSettings> {
        self.review_settings(None).await
    }
    pub async fn put_review_settings(&self, settings: ReviewSettings) -> Result<ReviewSettings> {
        if !(1..=7).contains(&settings.weekday) || settings.hour > 23 {
            return Err(StoreError::Invalid(
                "weekday must be 1..7 and hour 0..23".into(),
            ));
        }
        self.review_settings(Some(settings)).await
    }
    async fn review_settings(&self, replacement: Option<ReviewSettings>) -> Result<ReviewSettings> {
        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;
        let settings = review_settings_in(&mut tx, replacement).await?;
        tx.commit().await?;
        self.notify.notify_one();
        Ok(settings)
    }
}
pub(super) async fn review_settings_in(
    tx: &mut Tx<'_>,
    replacement: Option<ReviewSettings>,
) -> Result<ReviewSettings> {
    let old: Option<String> =
        sqlx::query_scalar("SELECT value FROM sin90_settings WHERE key='review'")
            .fetch_optional(&mut **tx)
            .await?;
    let settings = match &replacement {
        Some(s) => s.clone(),
        None => old
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?
            .unwrap_or_default(),
    };
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sin90_routines WHERE id=?)")
        .bind(SYSTEM_REVIEW_ID)
        .fetch_one(&mut **tx)
        .await?;
    let current_tz: Option<String> = sqlx::query_scalar("SELECT tz FROM sin90_routines WHERE id=?")
        .bind(SYSTEM_REVIEW_ID)
        .fetch_optional(&mut **tx)
        .await?;
    let wanted_tz = system_tz_name(&TimeZone::system(), Timestamp::now());
    if replacement.is_some()
        || old.is_none()
        || !exists
        || current_tz.as_deref() != Some(&wanted_tz)
    {
        let now = now_iso8601();
        let weekday =
            ["MON", "TUE", "WED", "THU", "FRI", "SAT", "SUN"][usize::from(settings.weekday - 1)];
        let cron = format!("0 {} * * {}", settings.hour, weekday);
        let tz = system_tz_name(&TimeZone::system(), Timestamp::now());
        let tz = tz.as_str();
        let status = if settings.auto { "active" } else { "paused" };
        sqlx::query("INSERT INTO sin90_routines(id,title,kind,cron,tz,status,created_at,updated_at) VALUES (?,'自动复盘','review',?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET cron=excluded.cron,tz=excluded.tz,status=excluded.status,updated_at=excluded.updated_at")
                .bind(SYSTEM_REVIEW_ID).bind(&cron).bind(tz).bind(status).bind(&now).bind(&now).execute(&mut **tx).await?;
        sqlx::query("INSERT INTO sin90_settings(key,value,updated_at) VALUES ('review',?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at")
                .bind(serde_json::to_string(&settings)?).bind(&now).execute(&mut **tx).await?;
        append_event(tx,"routine",SYSTEM_REVIEW_ID,if exists {"updated"} else {"created"},None,Some(status),&json!({"routine_id":SYSTEM_REVIEW_ID,"kind":"review","cron":cron,"tz":tz,"status":status}),&now).await?;
        append_event(
            tx,
            "settings.review",
            "review",
            "updated",
            None,
            None,
            &json!(settings),
            &now,
        )
        .await?;
        upsert_outbox(
            tx,
            "scheduler.upsert",
            &routine_dedup_key(SYSTEM_REVIEW_ID),
            &routine_outbox_upsert_desired(SYSTEM_REVIEW_ID, &cron, tz, settings.auto),
        )
        .await?;
    }
    Ok(settings)
}
pub(crate) fn local_day(s: &str) -> Option<Date> {
    s.parse::<Timestamp>()
        .ok()
        .map(|t| t.to_zoned(TimeZone::system()).date())
}
fn add(d: Date, n: i64) -> Date {
    d.checked_add(Span::new().days(n))
        .expect("bounded review date")
}
pub(crate) fn review_week(day: Date) -> String {
    let d = if day.weekday().to_monday_zero_offset() == 6 {
        day
    } else {
        add(day, -7)
    };
    let w = d.iso_week_date();
    format!("{:04}-W{:02}", w.year(), w.week())
}
fn latest_month_end(day: Date) -> Date {
    // Sunday closes the reviewed ISO week, including a month ending today.
    // Other weekdays wait until the calendar month has actually ended.
    if day.weekday().to_monday_zero_offset() == 6 && add(day, 1).month() != day.month() {
        day
    } else {
        add(Date::new(day.year(), day.month(), 1).unwrap(), -1)
    }
}
pub(super) async fn should_draft(tx: &mut Tx<'_>, id: &str, day: Date) -> Result<bool> {
    if id != SYSTEM_REVIEW_ID {
        return Ok(true);
    }
    let settings: String =
        sqlx::query_scalar("SELECT value FROM sin90_settings WHERE key='review'")
            .fetch_one(&mut **tx)
            .await?;
    let settings: ReviewSettings = serde_json::from_str(&settings)?;
    if settings.cadence == ReviewCadence::Weekly {
        return Ok(true);
    }
    let sunday = if day.weekday().to_monday_zero_offset() == 6 {
        day
    } else {
        add(day, -i64::from(day.weekday().to_monday_zero_offset()) - 1)
    };
    let end = latest_month_end(day);
    Ok(end >= add(sunday, -6) && end <= sunday)
}

pub(super) async fn draft_closed_periods(tx: &mut Tx<'_>, day: Date, now: &str) -> Result<()> {
    let end = latest_month_end(day);
    let mut periods = vec![(
        ReviewKind::Monthly,
        format!("{:04}-{:02}", end.year(), end.month()),
    )];
    if end.month() % 3 == 0 {
        periods.push((
            ReviewKind::Quarterly,
            format!("{:04}-Q{}", end.year(), end.month() / 3),
        ));
    }
    if end.month() == 12 {
        periods.push((ReviewKind::Yearly, format!("{:04}", end.year())));
    }
    for (kind, period) in periods {
        let wire = to_wire(&kind)?;
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sin90_reviews WHERE kind=? AND period=?)",
        )
        .bind(&wire)
        .bind(&period)
        .fetch_one(&mut **tx)
        .await?;
        if exists {
            continue;
        }
        let d = calendar_draft(tx, kind, &period).await?;
        let id = ulid();
        sqlx::query("INSERT INTO sin90_reviews(id,kind,period,status,body,created_at,updated_at) VALUES (?,?,?,'draft',?,?,?)").bind(&id).bind(&wire).bind(&period).bind(&d.auto_draft_md).bind(now).bind(now).execute(&mut **tx).await?;
        append_event(
            tx,
            "review",
            &id,
            "created",
            None,
            Some("draft"),
            &json!({"review_id":id,"kind":wire,"period":period,"source":"calendar"}),
            now,
        )
        .await?;
    }
    Ok(())
}

fn calendar_bounds(kind: ReviewKind, period: &str) -> Result<(Date, Date)> {
    let pk = match kind {
        ReviewKind::Monthly => PlanKind::Month,
        ReviewKind::Quarterly => PlanKind::Quarter,
        ReviewKind::Yearly => PlanKind::Year,
        _ => return Err(StoreError::Invalid("expected calendar review".into())),
    };
    if !valid_period(pk, period) {
        return Err(StoreError::Invalid("invalid review period".into()));
    }
    let year = period[..4].parse::<i16>().unwrap();
    let (month, len) = match pk {
        PlanKind::Month => (period[5..].parse::<i8>().unwrap(), 1),
        PlanKind::Quarter => (1 + (period.as_bytes()[6] - b'1') as i8 * 3, 3),
        _ => (1, 12),
    };
    let start = Date::new(year, month, 1).unwrap();
    let end = start
        .checked_add(Span::new().months(len))
        .map_err(|e| StoreError::Invalid(e.to_string()))?;
    Ok((start, end))
}

impl Sin90Store {
    pub(crate) async fn review_activity_since(&self, review: &Review, since: &str) -> Result<bool> {
        if review.kind == ReviewKind::Weekly {
            return self.week_activity_since(&review.period, since).await;
        }
        let (start, end) = calendar_bounds(review.kind, &review.period)?;
        let rows = sqlx::query("SELECT entity,kind,at FROM sin90_events WHERE at > ? AND entity IN ('task','block','routine','period_plan','direction')").bind(since).fetch_all(self.pool()).await?;
        for row in rows {
            let entity: String = row.get("entity");
            if review.kind == ReviewKind::Yearly
                && matches!(entity.as_str(), "period_plan" | "direction")
            {
                return Ok(true);
            }
            let day = local_day(&row.get::<String, _>("at"))
                .ok_or_else(|| StoreError::Internal("invalid event timestamp".into()))?;
            if day >= start && day < end {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

pub(crate) async fn calendar_draft(
    tx: &mut Tx<'_>,
    kind: ReviewKind,
    period: &str,
) -> Result<SummarizeDraft> {
    let (start, end) = calendar_bounds(kind, period)?;
    let mut tasks = BTreeMap::<String, String>::new();
    let mut minutes = BTreeMap::<String, i64>::new();
    let mut tasks_done = 0;
    let mut routines = BTreeMap::<String, (i64, i64)>::new();
    let rows = sqlx::query(
        "SELECT entity,entity_id,kind,to_state,payload,at FROM sin90_events ORDER BY seq",
    )
    .fetch_all(&mut **tx)
    .await?;
    for r in rows {
        if r.get::<String, _>("entity") == "routine" && r.get::<String, _>("kind") == "fired" {
            let day = local_day(&r.get::<String, _>("at"))
                .ok_or_else(|| StoreError::Internal("invalid event timestamp".into()))?;
            if day >= start && day < end {
                routines.entry(r.get("entity_id")).or_default().0 += 1;
            }
        }
        let Some(c) = super::progress::completion_from_event(&r, &mut tasks, &TimeZone::system())?
        else {
            continue;
        };
        if c.day < start || c.day >= end {
            continue;
        }
        match c.minutes {
            Some(n) => {
                *minutes.entry(c.direction).or_default() += n;
                if let Some(id) = c.routine {
                    routines.entry(id).or_default().1 += 1;
                }
            }
            None => tasks_done += 1,
        }
    }
    let mut draft = SummarizeDraft {
        week: period.into(),
        by_area: vec![],
        by_direction: minutes
            .into_iter()
            .map(|(id, minutes)| SummarizeBucket {
                label: Some(id),
                minutes,
            })
            .collect(),
        tasks_done,
        routines: routines
            .into_iter()
            .map(|(label, (fired, completed))| SummarizeRoutineRow {
                label,
                fired,
                completed,
            })
            .collect(),
        completions: Vec::new(),
        auto_draft_md: String::new(),
    };
    draft.auto_draft_md =
        crate::ai::summarize::render_facts(period, &crate::ai::summarize::facts(&draft));
    Ok(draft)
}

/// Proposals only: reuse existing year parents and leave user rows out entirely.
pub(crate) async fn yearly_plans(tx: &mut Tx<'_>, period: &str) -> Result<Vec<Sin90Op>> {
    if !valid_period(PlanKind::Year, period) {
        return Err(StoreError::Invalid("invalid year".into()));
    }
    let year: i32 = period.parse().unwrap();
    let rows = sqlx::query("SELECT * FROM sin90_period_plans ORDER BY period")
        .fetch_all(&mut **tx)
        .await?;
    let all: Vec<_> = rows
        .into_iter()
        .map(super::period_plans::row_to_plan)
        .collect::<Result<_>>()?;
    let mut ops = Vec::new();
    let mut directions = std::collections::BTreeSet::new();
    for p in &all {
        directions.insert(p.direction_id.clone());
    }
    for direction_id in directions {
        let mut plans = Vec::new();
        for p in all.iter().filter(|p| {
            p.direction_id == direction_id
                && p.plan.kind == PlanKind::Year
                && (p.plan.period == (year + 1).to_string()
                    || p.plan.period == (year + 2).to_string())
        }) {
            if p.source != PlanSource::User {
                plans.push(p.plan.clone());
            }
            if p.plan.period != (year + 1).to_string() {
                continue;
            }
            for (kind, n, mark) in [(PlanKind::Half, 2, "H"), (PlanKind::Quarter, 4, "Q")] {
                for i in 1..=n {
                    let child_period = format!("{}-{}{}", year + 1, mark, i);
                    if all.iter().any(|c| {
                        c.direction_id == direction_id
                            && c.plan.kind == kind
                            && c.plan.period == child_period
                            && c.source == PlanSource::User
                    }) {
                        continue;
                    }
                    let mut child = p.plan.clone();
                    child.kind = kind;
                    child.period = child_period;
                    child.parent_id = Some(p.id.clone());
                    child.status = PlanStatus::Draft;
                    child.metric_target =
                        p.plan.metric_target.map(|t| t / n + i64::from(i <= t % n));
                    plans.push(child);
                }
            }
        }
        if !plans.is_empty() {
            ops.push(Sin90Op::UpsertPeriodPlans {
                direction_id,
                plans,
            });
        }
    }
    Ok(ops)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{
        ports::{AiReadModel, ModelAccess, NoModelPort},
        summarize::{run_summarize, SummarizeItemResult},
    };
    use crate::store::RoutineFireOutcome;
    fn stamp(day: &str) -> String {
        format!("{day}T18:00:00")
            .parse::<jiff::civil::DateTime>()
            .unwrap()
            .to_zoned(TimeZone::system())
            .unwrap()
            .timestamp()
            .to_string()
    }
    async fn fire(store: &Sin90Store, day: &str) -> RoutineFireOutcome {
        store
            .record_routine_fire(
                day,
                &format!("routine.{SYSTEM_REVIEW_ID}"),
                &stamp(day),
                FireTrigger::Tick,
            )
            .await
            .unwrap()
    }
    #[test]
    fn month_end_is_closed_after_the_day_except_sunday_review() {
        assert_eq!(
            latest_month_end("2026-03-31".parse().unwrap()).to_string(),
            "2026-02-28"
        );
        assert_eq!(
            latest_month_end("2026-04-01".parse().unwrap()).to_string(),
            "2026-03-31"
        );
        assert_eq!(
            latest_month_end("2026-05-31".parse().unwrap()).to_string(),
            "2026-05-31"
        );
        assert_eq!(review_week("2027-01-04".parse().unwrap()), "2026-W53");
    }
    #[tokio::test]
    async fn defaults_idempotent_sunday_monday_and_paused_fire() {
        let s = Sin90Store::open_memory().await.unwrap();
        assert_eq!(
            s.get_review_settings().await.unwrap(),
            ReviewSettings::default()
        );
        s.get_review_settings().await.unwrap();
        assert_eq!(s.list_routines(None, None, None).await.unwrap().len(), 1);
        let r = s.get_routine(SYSTEM_REVIEW_ID).await.unwrap();
        assert_eq!(r.cron, "0 18 * * SUN");
        validate_cron(&r.cron).unwrap();
        let RoutineFireOutcome::Recorded {
            auto_review: Some(r),
            ..
        } = fire(&s, "2026-09-27").await
        else {
            panic!("missing weekly");
        };
        assert_eq!(r.period, "2026-W39");
        s.put_review_settings(ReviewSettings {
            weekday: 1,
            ..ReviewSettings::default()
        })
        .await
        .unwrap();
        assert_eq!(
            s.get_routine(SYSTEM_REVIEW_ID).await.unwrap().cron,
            "0 18 * * MON"
        );
        let RoutineFireOutcome::Recorded {
            auto_review: Some(r),
            ..
        } = fire(&s, "2026-10-05").await
        else {
            panic!("missing weekly");
        };
        assert_eq!(r.period, "2026-W40");
        s.put_review_settings(ReviewSettings {
            auto: false,
            ..ReviewSettings::default()
        })
        .await
        .unwrap();
        assert_eq!(
            s.get_routine(SYSTEM_REVIEW_ID).await.unwrap().status,
            RoutineStatus::Paused
        );
        assert!(matches!(
            fire(&s, "2026-10-11").await,
            RoutineFireOutcome::Recorded {
                auto_review: None,
                ..
            }
        ));
        assert!(s
            .list_reviews(Some(ReviewKind::Weekly), Some("2026-W41"))
            .await
            .unwrap()
            .is_empty());
        s.put_review_settings(ReviewSettings::default())
            .await
            .unwrap();
        assert_eq!(
            s.get_routine(SYSTEM_REVIEW_ID).await.unwrap().status,
            RoutineStatus::Active
        );
    }
    #[tokio::test]
    async fn monthly_cadence_skips_ordinary_weeks_but_records_receipt() {
        let s = Sin90Store::open_memory().await.unwrap();
        s.put_review_settings(ReviewSettings {
            cadence: ReviewCadence::Monthly,
            ..ReviewSettings::default()
        })
        .await
        .unwrap();
        assert!(matches!(
            fire(&s, "2026-09-20").await,
            RoutineFireOutcome::Recorded {
                auto_review: None,
                ..
            }
        ));
        assert!(matches!(
            fire(&s, "2026-09-20").await,
            RoutineFireOutcome::Duplicate { .. }
        ));
        assert!(matches!(
            fire(&s, "2026-10-04").await,
            RoutineFireOutcome::Recorded {
                auto_review: Some(_),
                ..
            }
        ));
    }
    #[tokio::test]
    async fn monthly_cadence_drafts_only_the_first_review_after_month_end() {
        let s = Sin90Store::open_memory().await.unwrap();
        s.put_review_settings(ReviewSettings {
            cadence: ReviewCadence::Monthly,
            ..ReviewSettings::default()
        })
        .await
        .unwrap();
        // Sundays: Jan 31 (Sat) closes in the week ending Feb 1; Feb 28 (Sat)
        // in the week ending Mar 1. Mondays review the previous week the same way.
        for (day, drafted) in [
            ("2026-02-01", true),
            ("2026-02-08", false),
            ("2026-02-15", false),
            ("2026-02-22", false),
            ("2026-03-01", true),
            ("2026-03-08", false),
            ("2026-03-09", false),
            ("2026-04-06", true),
            ("2026-04-13", false),
        ] {
            let got = matches!(
                fire(&s, day).await,
                RoutineFireOutcome::Recorded {
                    auto_review: Some(_),
                    ..
                }
            );
            assert_eq!(got, drafted, "{day}");
        }
    }
    #[tokio::test]
    async fn month_end_tuesday_saturday_sunday_first_review_and_dedup() {
        for (day, month) in [
            ("2026-04-05", "2026-03"),
            ("2026-02-01", "2026-01"),
            ("2026-05-31", "2026-05"),
        ] {
            let s = Sin90Store::open_memory().await.unwrap();
            s.get_review_settings().await.unwrap();
            fire(&s, day).await;
            let reviews = s
                .list_reviews(Some(ReviewKind::Monthly), Some(month))
                .await
                .unwrap();
            assert_eq!(reviews.len(), 1, "{day}");
            s.record_routine_fire(
                "again",
                &format!("routine.{SYSTEM_REVIEW_ID}"),
                &stamp(day),
                FireTrigger::RunNow,
            )
            .await
            .unwrap();
            assert_eq!(
                s.list_reviews(Some(ReviewKind::Monthly), Some(month))
                    .await
                    .unwrap()
                    .len(),
                1
            );
        }
    }
    #[tokio::test]
    async fn calendar_numbers_use_full_month_and_shared_progress_replay() {
        let s = Sin90Store::open_memory().await.unwrap();
        let d = s.create_direction("work", "year", None).await.unwrap();
        for (day, n) in [
            ("2026-03-01", 20),
            ("2026-03-30", 30),
            ("2026-03-31", 40),
            ("2026-04-01", 90),
        ] {
            let b = s.create_block(Some(&d.id), None, n, None).await.unwrap();
            s.transition_block(&b.id, ScheduleBlockStatus::Started)
                .await
                .unwrap();
            s.transition_block(&b.id, ScheduleBlockStatus::Completed)
                .await
                .unwrap();
            crate::store::test_hooks::set_last_event_at(&s, "block", &b.id, &stamp(day))
                .await
                .unwrap();
        }
        // A mutable-row-only edit cannot add a completion to the event projection.
        sqlx::query("UPDATE sin90_schedule_blocks SET planned_minutes=999")
            .execute(s.pool())
            .await
            .unwrap();
        s.get_review_settings().await.unwrap();
        fire(&s, "2026-04-05").await;
        let r = s
            .list_reviews(Some(ReviewKind::Monthly), Some("2026-03"))
            .await
            .unwrap()
            .remove(0);
        let draft = s.ai_reader().review_draft(&r).await.unwrap();
        assert_eq!(draft.by_direction[0].minutes, 90);
        assert!(r.body.contains("1 小时 30 分钟"), "{}", r.body);
        let q = s
            .list_reviews(Some(ReviewKind::Quarterly), Some("2026-Q1"))
            .await
            .unwrap();
        assert_eq!(q.len(), 1);
    }
    fn year(period: &str, n: i64) -> NewPeriodPlan {
        NewPeriodPlan {
            kind: PlanKind::Year,
            period: period.into(),
            parent_id: None,
            target: "work".into(),
            metric: PlanMetric::Count,
            metric_target: Some(n),
            routine_id: None,
            status: PlanStatus::Active,
        }
    }
    #[tokio::test]
    async fn yearly_summarize_proposes_only_until_accepted_and_preserves_user_rows() {
        let s = Sin90Store::open_memory().await.unwrap();
        let d = s.create_direction("work", "3y", None).await.unwrap();
        let next = s
            .create_period_plan(&d.id, &year("2027", 100))
            .await
            .unwrap();
        s.create_period_plan(&d.id, &year("2028", 200))
            .await
            .unwrap();
        let mut user_half = year("2027-H1", 88);
        user_half.kind = PlanKind::Half;
        user_half.parent_id = Some(next.id.clone());
        let user = s.create_period_plan(&d.id, &user_half).await.unwrap();
        s.get_review_settings().await.unwrap();
        fire(&s, "2027-01-03").await;
        let review = s
            .list_reviews(Some(ReviewKind::Yearly), Some("2026"))
            .await
            .unwrap()
            .remove(0);
        let result = run_summarize(
            "annual",
            &review,
            ModelAccess::LocalOnly,
            None::<&NoModelPort>,
            &s,
            &s.ai_reader(),
        )
        .await;
        let SummarizeItemResult::Proposed(id) = result else {
            panic!("{result:?}");
        };
        let proposal = s.get_proposal(&id).await.unwrap();
        assert!(proposal.ops.iter().any(|op| matches!(
            op,
            Sin90Op::DraftReviewBody { body, .. }
                if body != &review.body && body.contains("建议沿用年度目标")
        )));
        let ops = proposal.ops;
        let plans = ops
            .iter()
            .find_map(|op| match op {
                Sin90Op::UpsertPeriodPlans { plans, .. } => Some(plans),
                _ => None,
            })
            .unwrap();
        assert_eq!(plans.len(), 5);
        assert!(plans
            .iter()
            .all(|p| p.period != "2027-H1" && p.kind != PlanKind::Year));
        assert_eq!(
            plans
                .iter()
                .find(|p| p.period == "2027-H2")
                .unwrap()
                .metric_target,
            Some(50)
        );
        assert_eq!(
            plans
                .iter()
                .filter(|p| p.kind == PlanKind::Quarter)
                .map(|p| p.metric_target.unwrap())
                .sum::<i64>(),
            100
        );
        assert_eq!(
            s.list_period_plans(Some(&d.id), None, None)
                .await
                .unwrap()
                .len(),
            3
        );
        // A human edits another proposed row before acceptance: L2a skips it.
        let mut q = year("2027-Q1", 77);
        q.kind = PlanKind::Quarter;
        q.parent_id = Some(next.id);
        s.create_period_plan(&d.id, &q).await.unwrap();
        let applied = s.apply_proposal(&id).await.unwrap();
        assert_eq!(applied.receipt.skipped_period_plans.len(), 1);
        let all = s.list_period_plans(Some(&d.id), None, None).await.unwrap();
        assert_eq!(all.len(), 8);
        assert_eq!(
            all.iter()
                .find(|p| p.id == user.id)
                .unwrap()
                .plan
                .metric_target,
            Some(88)
        );
        assert_eq!(
            all.iter()
                .find(|p| p.plan.period == "2027-Q1")
                .unwrap()
                .plan
                .metric_target,
            Some(77)
        );
    }
    #[tokio::test]
    async fn manual_review_also_drafts_latest_closed_month() {
        let s = Sin90Store::open_memory().await.unwrap();
        let day = Timestamp::now().to_zoned(TimeZone::system()).date();
        let end = latest_month_end(day);
        let period = format!("{:04}-{:02}", end.year(), end.month());
        s.create_review(&NewReview {
            kind: ReviewKind::Weekly,
            period: review_week(day),
        })
        .await
        .unwrap();
        assert_eq!(
            s.list_reviews(Some(ReviewKind::Monthly), Some(&period))
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn annual_fallback_keeps_ai_year_segments_unchanged() {
        let s = Sin90Store::open_memory().await.unwrap();
        let d = s.create_direction("work", "3y", None).await.unwrap();
        let annuals = vec![year("2027", 101), year("2028", 303)];
        s.submit_proposal(&Sin90Proposal {
            id: "seed-years".into(),
            status: ProposalStatus::Pending,
            source: ProposalSource::Rule,
            ops: vec![Sin90Op::UpsertPeriodPlans {
                direction_id: d.id,
                plans: annuals.clone(),
            }],
            rationale: None,
        })
        .await
        .unwrap();
        s.apply_proposal("seed-years").await.unwrap();
        let mut tx = s.pool().begin().await.unwrap();
        let ops = yearly_plans(&mut tx, "2026").await.unwrap();
        let Sin90Op::UpsertPeriodPlans { plans, .. } = &ops[0] else {
            panic!("missing plans");
        };
        assert_eq!(
            plans
                .iter()
                .filter(|p| p.kind == PlanKind::Year)
                .cloned()
                .collect::<Vec<_>>(),
            annuals
        );
        assert_eq!(
            plans
                .iter()
                .filter(|p| p.kind == PlanKind::Half)
                .map(|p| p.metric_target.unwrap())
                .sum::<i64>(),
            101
        );
        assert_eq!(
            plans
                .iter()
                .filter(|p| p.kind == PlanKind::Quarter)
                .map(|p| p.metric_target.unwrap())
                .sum::<i64>(),
            101
        );
    }

    #[tokio::test]
    async fn summarize_accepts_month_quarter_year_with_rule_fallback() {
        let s = Sin90Store::open_memory().await.unwrap();
        for (i, kind, period) in [
            (0, ReviewKind::Monthly, "2026-01"),
            (1, ReviewKind::Quarterly, "2026-Q1"),
            (2, ReviewKind::Yearly, "2026"),
        ] {
            let review = s
                .create_review(&NewReview {
                    kind,
                    period: period.into(),
                })
                .await
                .unwrap();
            let result = run_summarize(
                &format!("calendar-{i}"),
                &review,
                ModelAccess::LocalOnly,
                None::<&NoModelPort>,
                &s,
                &s.ai_reader(),
            )
            .await;
            let SummarizeItemResult::Proposed(id) = result else {
                panic!("{result:?}");
            };
            let proposal = s.get_proposal(&id).await.unwrap();
            assert!(
                matches!(&proposal.ops[0],Sin90Op::DraftReviewBody{body,..} if body.contains(period))
            );
            assert!(s.get_review(&review.id).await.unwrap().body.is_empty());
        }
    }

    #[tokio::test]
    async fn strict_calendar_period_validation() {
        let s = Sin90Store::open_memory().await.unwrap();
        for (kind, valid, bad) in [
            (
                ReviewKind::Monthly,
                "2026-02",
                vec!["2026-2", "2026-13", "0000-01", " 2026-02"],
            ),
            (
                ReviewKind::Quarterly,
                "2026-Q4",
                vec!["2026-q4", "2026-Q0", "2026-Q5"],
            ),
            (ReviewKind::Yearly, "2026", vec!["26", "0000", "2026\n"]),
        ] {
            s.create_review(&NewReview {
                kind,
                period: valid.into(),
            })
            .await
            .unwrap();
            for period in bad {
                assert!(matches!(
                    s.create_review(&NewReview {
                        kind,
                        period: period.into()
                    })
                    .await,
                    Err(StoreError::Invalid(_))
                ));
            }
        }
    }

    #[test]
    fn a_zone_without_a_name_falls_back_to_its_offset() {
        let now = Timestamp::now();
        let plus7 = TimeZone::fixed(jiff::tz::offset(7));
        assert_eq!(system_tz_name(&plus7, now), "Etc/GMT-7");
        let minus5 = TimeZone::fixed(jiff::tz::offset(-5));
        assert_eq!(system_tz_name(&minus5, now), "Etc/GMT+5");
        assert_eq!(system_tz_name(&TimeZone::UTC, now), "UTC");
        let named = TimeZone::get("Asia/Bangkok").unwrap();
        assert_eq!(system_tz_name(&named, now), "Asia/Bangkok");
        // The fallback is itself a zone the routine validator accepts.
        assert!(TimeZone::get("Etc/GMT-7").is_ok());
    }
}
