use super::repo::{append_event, from_wire, to_wire, Tx};
use crate::core::*;
use crate::store::{Result, Sin90Store, StoreError};
use sqlx::Row;

pub(crate) fn row_to_plan(r: sqlx::sqlite::SqliteRow) -> Result<PeriodPlan> {
    Ok(PeriodPlan {
        id: r.get("id"),
        direction_id: r.get("direction_id"),
        plan: NewPeriodPlan {
            kind: from_wire(&r.get::<String, _>("kind"))?,
            period: r.get("period"),
            parent_id: r.get("parent_id"),
            target: r.get("target"),
            metric: from_wire(&r.get::<String, _>("metric"))?,
            metric_target: r.get("metric_target"),
            routine_id: r.get("routine_id"),
            status: from_wire(&r.get::<String, _>("status"))?,
        },
        source: from_wire(&r.get::<String, _>("source"))?,
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    })
}

pub(crate) async fn get_plan(tx: &mut Tx<'_>, id: &str) -> Result<Option<PeriodPlan>> {
    sqlx::query("SELECT * FROM sin90_period_plans WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .map(row_to_plan)
        .transpose()
}

async fn check_relations(tx: &mut Tx<'_>, direction_id: &str, plan: &NewPeriodPlan) -> Result<()> {
    validate_plan(plan).map_err(StoreError::Invalid)?;
    let parent = match &plan.parent_id {
        Some(id) => get_plan(tx, id).await?,
        None => None,
    };
    validate_parent(direction_id, plan, parent.as_ref()).map_err(StoreError::Invalid)?;
    if let Some(id) = &plan.routine_id {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sin90_routines WHERE id = ? AND status != 'retired')",
        )
        .bind(id)
        .fetch_one(&mut **tx)
        .await?;
        if !exists {
            return Err(StoreError::Invalid(
                "routine must exist and not be retired".into(),
            ));
        }
    }
    Ok(())
}

pub(crate) async fn plan_event(
    tx: &mut Tx<'_>,
    plan: &PeriodPlan,
    action: &str,
    events: &mut Vec<String>,
) -> Result<()> {
    events.push(
        append_event(
            tx,
            "period_plan",
            &plan.id,
            action,
            None,
            Some(&to_wire(&plan.plan.status)?),
            &serde_json::to_value(plan)?,
            &plan.updated_at,
        )
        .await?,
    );
    Ok(())
}

pub(crate) async fn cascade_drop(
    tx: &mut Tx<'_>,
    id: &str,
    now: &str,
    events: &mut Vec<String>,
) -> Result<Vec<PeriodPlan>> {
    let rows = sqlx::query("WITH RECURSIVE descendants(id) AS (SELECT id FROM sin90_period_plans WHERE parent_id = ? UNION ALL SELECT p.id FROM sin90_period_plans p JOIN descendants d ON p.parent_id = d.id) SELECT p.* FROM sin90_period_plans p JOIN descendants d ON p.id = d.id WHERE p.status IN ('draft', 'active') ORDER BY p.id")
        .bind(id).fetch_all(&mut **tx).await?;
    let mut changed = Vec::new();
    for row in rows {
        let mut plan = row_to_plan(row)?;
        sqlx::query(
            "UPDATE sin90_period_plans SET status = 'dropped', updated_at = ? WHERE id = ?",
        )
        .bind(now)
        .bind(&plan.id)
        .execute(&mut **tx)
        .await?;
        plan.plan.status = PlanStatus::Dropped;
        plan.updated_at = now.into();
        plan_event(tx, &plan, "updated", events).await?;
        changed.push(plan);
    }
    Ok(changed)
}

// Called independently of pure validation: the WHERE clause is the final CAS guard.
pub(crate) async fn write_plan(
    tx: &mut Tx<'_>,
    direction_id: &str,
    new: &NewPeriodPlan,
    source: PlanSource,
    existing: Option<&PeriodPlan>,
    events: &mut Vec<String>,
) -> Result<Option<PeriodPlan>> {
    if source == PlanSource::Ai && existing.is_some_and(|p| p.source == PlanSource::User) {
        return Ok(None);
    }
    check_relations(tx, direction_id, new).await?;
    let now = now_iso8601();
    let id = existing.map_or_else(ulid, |p| p.id.clone());
    let affected = if let Some(old) = existing {
        sqlx::query("UPDATE sin90_period_plans SET parent_id = ?, target = ?, metric = ?, metric_target = ?, routine_id = ?, status = ?, source = ?, updated_at = ? WHERE id = ? AND updated_at = ? AND (? = 'user' OR source = 'ai')")
            .bind(&new.parent_id).bind(&new.target).bind(to_wire(&new.metric)?).bind(new.metric_target).bind(&new.routine_id)
            .bind(to_wire(&new.status)?).bind(to_wire(&source)?).bind(&now).bind(&id).bind(&old.updated_at).bind(to_wire(&source)?)
            .execute(&mut **tx).await?.rows_affected()
    } else {
        sqlx::query("INSERT INTO sin90_period_plans (id, direction_id, kind, period, parent_id, target, metric, metric_target, routine_id, source, status, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(direction_id, kind, period) DO NOTHING")
            .bind(&id).bind(direction_id).bind(to_wire(&new.kind)?).bind(&new.period).bind(&new.parent_id).bind(&new.target)
            .bind(to_wire(&new.metric)?).bind(new.metric_target).bind(&new.routine_id).bind(to_wire(&source)?).bind(to_wire(&new.status)?).bind(&now).bind(&now)
            .execute(&mut **tx).await?.rows_affected()
    };
    if affected == 0 {
        return Ok(None);
    }
    let plan = PeriodPlan {
        id,
        direction_id: direction_id.into(),
        plan: new.clone(),
        source,
        created_at: existing.map_or_else(|| now.clone(), |p| p.created_at.clone()),
        updated_at: now,
    };
    plan_event(
        tx,
        &plan,
        if existing.is_some() {
            "updated"
        } else {
            "created"
        },
        events,
    )
    .await?;
    if new.status == PlanStatus::Dropped {
        cascade_drop(tx, &plan.id, &plan.updated_at, events).await?;
    }
    Ok(Some(plan))
}

impl Sin90Store {
    pub async fn list_period_plans(
        &self,
        direction_id: Option<&str>,
        kind: Option<PlanKind>,
        period: Option<&str>,
    ) -> Result<Vec<PeriodPlan>> {
        if let (Some(k), Some(p)) = (kind, period) {
            if !valid_period(k, p) {
                return Err(StoreError::Invalid("period does not match kind".into()));
            }
        }
        if let Some(p) = period {
            if ![
                PlanKind::Year,
                PlanKind::Half,
                PlanKind::Quarter,
                PlanKind::Month,
            ]
            .iter()
            .any(|k| valid_period(*k, p))
            {
                return Err(StoreError::Invalid("invalid period".into()));
            }
        }
        let kind = kind.map(|k| to_wire(&k)).transpose()?;
        sqlx::query("SELECT * FROM sin90_period_plans WHERE (? IS NULL OR direction_id = ?) AND (? IS NULL OR kind = ?) AND (? IS NULL OR period = ?) ORDER BY period, kind, id")
            .bind(direction_id).bind(direction_id).bind(&kind).bind(&kind).bind(period).bind(period)
            .fetch_all(self.pool()).await?.into_iter().map(row_to_plan).collect()
    }

    pub async fn create_period_plan(
        &self,
        direction_id: &str,
        plan: &NewPeriodPlan,
    ) -> Result<PeriodPlan> {
        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;
        let created = write_plan(
            &mut tx,
            direction_id,
            plan,
            PlanSource::User,
            None,
            &mut Vec::new(),
        )
        .await?
        .ok_or_else(|| StoreError::Conflict("period plan already exists".into()))?;
        tx.commit().await?;
        Ok(created)
    }

    pub async fn update_period_plan(
        &self,
        id: &str,
        patch: &PeriodPlanPatch,
    ) -> Result<Vec<PeriodPlan>> {
        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;
        let old = get_plan(&mut tx, id)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("period plan {id}")))?;
        let mut new = old.plan.clone();
        if let Some(v) = &patch.target {
            new.target = v.clone();
        }
        if let Some(v) = patch.metric {
            new.metric = v;
        }
        if let Some(v) = patch.metric_target {
            new.metric_target = v;
        }
        if let Some(v) = &patch.routine_id {
            new.routine_id = v.clone();
        }
        if let Some(v) = patch.status {
            new.status = v;
        }
        // Record exactly the snapshots written, including cascaded children, for EventSink.
        let mut events = Vec::new();
        let updated = write_plan(
            &mut tx,
            &old.direction_id,
            &new,
            PlanSource::User,
            Some(&old),
            &mut events,
        )
        .await?
        .ok_or_else(|| StoreError::Conflict("period plan changed".into()))?;
        let mut changed = vec![updated];
        for event_id in events.iter().skip(1) {
            let payload: String =
                sqlx::query_scalar("SELECT payload FROM sin90_events WHERE id = ?")
                    .bind(event_id)
                    .fetch_one(&mut *tx)
                    .await?;
            changed.push(serde_json::from_str(&payload)?);
        }
        tx.commit().await?;
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::repo::{apply_op, build_snapshot};

    fn year(period: &str, target: &str) -> NewPeriodPlan {
        NewPeriodPlan {
            kind: PlanKind::Year,
            period: period.into(),
            parent_id: None,
            target: target.into(),
            metric: PlanMetric::None,
            metric_target: None,
            routine_id: None,
            status: PlanStatus::Draft,
        }
    }

    #[tokio::test]
    async fn l2a_apply_rechecks_user_source_after_working_validation() {
        let store = Sin90Store::open_memory().await.unwrap();
        let d = store.create_direction("Run", "2026", None).await.unwrap();
        let mut tx = store.pool().begin_with("BEGIN IMMEDIATE").await.unwrap();
        let old = write_plan(
            &mut tx,
            &d.id,
            &year("2026", "AI"),
            PlanSource::Ai,
            None,
            &mut Vec::new(),
        )
        .await
        .unwrap()
        .unwrap();
        let ops = vec![Sin90Op::UpsertPeriodPlans {
            direction_id: d.id.clone(),
            plans: vec![year("2026", "overwrite"), year("2027", "new")],
        }];
        let snapshot = build_snapshot(&mut tx, &ops).await.unwrap();
        validate(
            &Sin90Proposal {
                id: "p".into(),
                status: ProposalStatus::Pending,
                source: ProposalSource::LocalBrain,
                ops: ops.clone(),
                rationale: None,
            },
            &snapshot,
            "direct",
        )
        .unwrap();
        // Simulate a human mutation after validation. apply_op must independently protect it.
        sqlx::query("UPDATE sin90_period_plans SET source = 'user', target = 'human' WHERE id = ?")
            .bind(&old.id)
            .execute(&mut *tx)
            .await
            .unwrap();
        let mut skipped = Vec::new();
        let mut events = Vec::new();
        apply_op(&mut tx, &ops[0], &mut events, "direct", &mut skipped)
            .await
            .unwrap();
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].id, old.id);
        assert_eq!(events.len(), 1);
        assert_eq!(
            get_plan(&mut tx, &old.id)
                .await
                .unwrap()
                .unwrap()
                .plan
                .target,
            "human"
        );
        tx.commit().await.unwrap();
    }

    #[tokio::test]
    async fn l2a_write_cas_checks_both_source_and_updated_at() {
        let store = Sin90Store::open_memory().await.unwrap();
        let d = store.create_direction("Run", "2026", None).await.unwrap();
        let mut tx = store.pool().begin_with("BEGIN IMMEDIATE").await.unwrap();
        let old = write_plan(
            &mut tx,
            &d.id,
            &year("2026", "original"),
            PlanSource::Ai,
            None,
            &mut Vec::new(),
        )
        .await
        .unwrap()
        .unwrap();
        // Same timestamp, changed source: only the SQL source predicate prevents this write.
        sqlx::query("UPDATE sin90_period_plans SET source = 'user', target = 'human' WHERE id = ?")
            .bind(&old.id)
            .execute(&mut *tx)
            .await
            .unwrap();
        let mut events = Vec::new();
        assert!(write_plan(
            &mut tx,
            &d.id,
            &year("2026", "stale"),
            PlanSource::Ai,
            Some(&old),
            &mut events
        )
        .await
        .unwrap()
        .is_none());
        // Same source, changed timestamp: only the updated_at predicate prevents this write.
        sqlx::query(
            "UPDATE sin90_period_plans SET source = 'ai', updated_at = 'changed' WHERE id = ?",
        )
        .bind(&old.id)
        .execute(&mut *tx)
        .await
        .unwrap();
        assert!(write_plan(
            &mut tx,
            &d.id,
            &year("2026", "stale"),
            PlanSource::Ai,
            Some(&old),
            &mut events
        )
        .await
        .unwrap()
        .is_none());
        assert!(events.is_empty());
        assert_eq!(
            get_plan(&mut tx, &old.id)
                .await
                .unwrap()
                .unwrap()
                .plan
                .target,
            "human"
        );
        tx.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn l2a_working_skips_user_rows_before_validating_replacement() {
        let store = Sin90Store::open_memory().await.unwrap();
        let d = store.create_direction("Run", "2026", None).await.unwrap();
        store
            .create_period_plan(&d.id, &year("2026", "human"))
            .await
            .unwrap();
        let mut tx = store.pool().begin_with("BEGIN IMMEDIATE").await.unwrap();
        let proposal = Sin90Proposal {
            id: "p".into(),
            status: ProposalStatus::Pending,
            source: ProposalSource::Rule,
            ops: vec![Sin90Op::UpsertPeriodPlans {
                direction_id: d.id,
                plans: vec![year("2026", ""), year("2027", "new")],
            }],
            rationale: None,
        };
        let snapshot = build_snapshot(&mut tx, &proposal.ops).await.unwrap();
        validate(&proposal, &snapshot, "direct").unwrap();
        let mut bad = proposal.clone();
        if let Sin90Op::UpsertPeriodPlans { plans, .. } = &mut bad.ops[0] {
            plans[1].target.clear();
        }
        assert!(validate(&bad, &snapshot, "direct").is_err());
        tx.rollback().await.unwrap();
    }
}

#[cfg(test)]
mod transaction_tests {
    use super::*;

    #[tokio::test]
    async fn l2a_ai_overwrite_cascades_and_failed_batch_rolls_everything_back() {
        let store = Sin90Store::open_memory().await.unwrap();
        let d = store.create_direction("Run", "2026", None).await.unwrap();
        let mut tx = store.pool().begin_with("BEGIN IMMEDIATE").await.unwrap();
        let new: NewPeriodPlan = serde_json::from_value(serde_json::json!({"kind":"year", "period":"2026", "target":"Original", "status":"active"})).unwrap();
        let parent = write_plan(&mut tx, &d.id, &new, PlanSource::Ai, None, &mut Vec::new())
            .await
            .unwrap()
            .unwrap();
        tx.commit().await.unwrap();
        let child: NewPeriodPlan = serde_json::from_value(serde_json::json!({"kind":"quarter", "period":"2026-Q1", "parent_id":parent.id, "target":"Child"})).unwrap();
        store.create_period_plan(&d.id, &child).await.unwrap();
        let mut drop_parent = new.clone();
        drop_parent.status = PlanStatus::Dropped;
        drop_parent.target = "Overwritten".into();
        let mut bad = new;
        bad.period = "2027".into();
        bad.routine_id = Some("missing".into());
        let mut proposal = Sin90Proposal {
            id: "rollback".into(),
            status: ProposalStatus::Pending,
            source: ProposalSource::Rule,
            ops: vec![Sin90Op::UpsertPeriodPlans {
                direction_id: d.id.clone(),
                plans: vec![drop_parent.clone(), bad],
            }],
            rationale: None,
        };
        store.submit_proposal(&proposal).await.unwrap();
        assert!(store.apply_proposal(&proposal.id).await.is_err());
        let plans = store
            .list_period_plans(Some(&d.id), None, None)
            .await
            .unwrap();
        assert_eq!(plans[0].plan.target, "Original");
        assert_eq!(plans[0].plan.status, PlanStatus::Active);
        assert_eq!(plans[1].plan.status, PlanStatus::Draft);
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sin90_events WHERE entity = 'period_plan' AND kind = 'updated'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(count, 0);
        proposal.id = "success".into();
        proposal.ops = vec![Sin90Op::UpsertPeriodPlans {
            direction_id: d.id.clone(),
            plans: vec![drop_parent],
        }];
        store.submit_proposal(&proposal).await.unwrap();
        let result = store.apply_proposal(&proposal.id).await.unwrap();
        assert_eq!(result.receipt.event_ids.len(), 2);
        let plans = store
            .list_period_plans(Some(&d.id), None, None)
            .await
            .unwrap();
        assert!(plans.iter().all(|p| p.plan.status == PlanStatus::Dropped));
        assert_eq!(plans[0].plan.target, "Overwritten");
        assert_eq!(plans[0].source, PlanSource::Ai);
    }
}
