//! Direction → current year/half/quarter/month, as a pending proposal only.
use serde::Deserialize;
use serde_json::{json, Value};

use super::ladder::{plan, run_item, Outcome, RunState};
use super::ports::{
    AiReadModel, AiSink, Capability, Complexity, Engine, ModelAccess, ModelMessage, ModelPort,
    ModelRequest, ProposalDraft, Role, SinkError,
};
use crate::core::{
    Direction, NewPeriodPlan, PeriodPlan, PlanKind, PlanMetric, PlanSource, PlanStatus, Sin90Op,
};

/// Calendar intervals are computed at the HTTP boundary using the system timezone.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Period {
    pub kind: PlanKind,
    pub label: String,
    pub days: i64,
}
#[derive(Debug, Clone)]
pub struct Input {
    pub direction: Direction,
    pub plans: Vec<PeriodPlan>,
    pub periods: Vec<Period>,
    pub progress: Value,
}

pub fn fallback(input: &Input) -> Vec<NewPeriodPlan> {
    let mut result = Vec::new();
    let mut parent: Option<(NewPeriodPlan, i64)> = None;
    for period in &input.periods {
        let existing = input
            .plans
            .iter()
            .find(|p| p.plan.kind == period.kind && p.plan.period == period.label);
        let next = if let Some(p) =
            existing.filter(|p| p.source == PlanSource::User || period.kind == PlanKind::Year)
        {
            p.plan.clone()
        } else {
            let (target, metric, metric_target, routine_id) = match &parent {
                None => (
                    format!(
                        "{} · {}年部分",
                        input
                            .direction
                            .goal_3y
                            .as_deref()
                            .unwrap_or(&input.direction.title),
                        period.label
                    ),
                    PlanMetric::None,
                    None,
                    None,
                ),
                Some((p, days)) => {
                    let n = p
                        .metric_target
                        .filter(|_| p.metric != PlanMetric::None)
                        .map(|n| {
                            let n = (i128::from(n) * i128::from(period.days) + i128::from(*days)
                                - 1)
                                / i128::from(*days);
                            i64::try_from(n).expect("a child interval cannot exceed its parent")
                        });
                    (
                        format!("{} · {}部分", p.target, period.label),
                        p.metric,
                        n,
                        p.routine_id.clone(),
                    )
                }
            };
            let p = NewPeriodPlan {
                kind: period.kind,
                period: period.label.clone(),
                parent_id: parent.as_ref().map(|(p, _)| format!("period:{}", p.period)),
                target,
                metric,
                metric_target,
                routine_id,
                status: PlanStatus::Draft,
            };
            result.push(p.clone());
            p
        };
        parent = Some((next, period.days));
    }
    result
}

#[derive(serde::Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Decision {
    plans: Vec<NewPeriodPlan>,
    reason: String,
}

fn request(input: &Input, scaffold: &[NewPeriodPlan], engine: Engine) -> ModelRequest {
    let nullable_string = json!({"type":["string","null"]});
    let schema = json!({"type":"object","additionalProperties":false,"required":["plans","reason"],"properties":{
        "reason":{"type":"string"},"plans":{"type":"array","items":{"type":"object","additionalProperties":false,
        "required":["kind","period","parent_id","target","metric","metric_target","routine_id","status"],
        "properties":{"kind":{"type":"string","enum":["year","half","quarter","month"]},"period":{"type":"string"},"parent_id":nullable_string,"target":{"type":"string"},"metric":{"type":"string","enum":["none","count","minutes"]},"metric_target":{"type":["integer","null"],"minimum":0},"routine_id":nullable_string,"status":{"type":"string","enum":["draft"]}}}}}});
    ModelRequest {
        messages: vec![ModelMessage { role: Role::System, content: "将方向拆分到本月。只返回严格 JSON。保留 scaffold 的行、顺序、kind、period、parent_id、routine_id、status，可调整 target、metric 和 metric_target。不要生成 source=user 的行。reason 必须用中文说明三年目标、期间天数和最近八周进度如何影响建议。".into() }, ModelMessage {role: Role::User, content: json!({"title":input.direction.title,"goal_3y":input.direction.goal_3y,"goal_from_year":input.direction.goal_from_year,"period_plans":input.plans,"calendar_periods":input.periods,"recent_8_weeks_progress":input.progress,"scaffold":scaffold}).to_string()}],
        schema_name: "sin90_decompose", schema: schema.as_object().unwrap().clone(), max_tokens: 4096,
        complexity: if engine == Engine::Executive { Complexity::Complex } else { Complexity::Simple },
    }
}
fn parse(
    text: &str,
    scaffold: &[NewPeriodPlan],
) -> Result<(Vec<NewPeriodPlan>, String), &'static str> {
    let raw: Value = serde_json::from_str(text).map_err(|_| "bad_output")?;
    let decision: Decision = serde_json::from_str(text).map_err(|_| "bad_output")?;
    if serde_json::to_value(&decision).map_err(|_| "bad_output")? != raw {
        return Err("bad_output");
    }
    if decision.reason.trim().is_empty()
        || !decision
            .reason
            .chars()
            .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
        || decision.plans.len() != scaffold.len()
    {
        return Err("bad_output");
    }
    for (actual, expected) in decision.plans.iter().zip(scaffold) {
        let mut allowed = expected.clone();
        allowed.target.clone_from(&actual.target);
        allowed.metric_target = actual.metric_target;
        allowed.metric = actual.metric;
        if actual != &allowed || crate::core::validate_plan(actual).is_err() {
            return Err("bad_output");
        }
    }
    Ok((decision.plans, decision.reason))
}

pub async fn run<M: ModelPort, S: AiSink, R: AiReadModel>(
    run_id: &str,
    input: &Input,
    access: ModelAccess,
    model: Option<&M>,
    sink: &S,
    read: &R,
) -> &'static str {
    let scaffold = fallback(input);
    let settings = read.settings().await.unwrap_or_default();
    let steps = plan(Capability::Decompose, access, settings, model.is_some());
    let mut state = RunState::new(std::time::Instant::now());
    let outcome = run_item(run_id, Capability::Decompose, &steps, &mut state, model, sink, read,
        |engine| request(input, &scaffold, engine), |reply| parse(&reply.text, &scaffold), || None,
        || Some((scaffold.clone(), "规则兜底：保留已有年度计划，从三年目标生成缺少的年度计划；按当前半年、季度、月份逐级拆分，量化目标按期间天数比例分配并向上取整，跳过用户计划。".into())),
        crate::core::now_iso8601, crate::core::ulid).await;
    match outcome {
        Outcome::Produced {
            value: (plans, reason),
            rec,
            ..
        } => {
            if plans.is_empty() {
                if let Err(e) = sink.record_call(rec).await {
                    tracing::warn!(error = %e, run_id, "decompose: failed to record no-op");
                }
                return "nothing";
            }
            let draft = ProposalDraft {
                id: format!("ai-decompose-{}", crate::core::ulid()),
                ops: vec![Sin90Op::UpsertPeriodPlans {
                    direction_id: input.direction.id.clone(),
                    plans,
                }],
                rationale: Some(reason),
            };
            let mut failure = rec.clone();
            match sink.submit(Capability::Decompose, draft, rec).await {
                Ok(()) => "proposed",
                Err(e) => {
                    failure.ok = false;
                    failure.error_kind = Some(match e {
                        SinkError::Invalid(_) => "rejected_by_precheck",
                        SinkError::Store(_) => "submit_store_error",
                    });
                    if let Err(e) = sink.record_call(failure).await {
                        tracing::warn!(error = %e, run_id, "decompose: failed to record submit failure");
                    }
                    "rejected"
                }
            }
        }
        Outcome::Deferred => "deferred",
        Outcome::Aborted => "aborted",
        Outcome::Nothing { .. } => "nothing",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiSettings, ModelFailure, ModelReply, NoModelPort, ServedTier, Step};
    use crate::store::Sin90Store;

    async fn fixture() -> (Sin90Store, Input) {
        let store = Sin90Store::open_memory().await.unwrap();
        let mut direction = store.create_direction("阅读", "2026", None).await.unwrap();
        direction.goal_3y = Some("读完36本书".into());
        direction.goal_from_year = Some(2026);
        let periods = [
            (PlanKind::Year, "2026", 365),
            (PlanKind::Half, "2026-H2", 184),
            (PlanKind::Quarter, "2026-Q4", 92),
            (PlanKind::Month, "2026-11", 30),
        ]
        .into_iter()
        .map(|(kind, label, days)| Period {
            kind,
            label: label.into(),
            days,
        })
        .collect();
        (
            store,
            Input {
                direction,
                periods,
                plans: Vec::new(),
                progress: json!([]),
            },
        )
    }

    #[tokio::test]
    async fn fallback_is_pending_until_accepted_and_resolves_new_parents() {
        let (store, input) = fixture().await;
        assert_eq!(
            run(
                "decompose-empty",
                &input,
                ModelAccess::LocalOnly,
                None::<&NoModelPort>,
                &store,
                &store.ai_reader()
            )
            .await,
            "proposed"
        );
        assert!(store
            .list_period_plans(Some(&input.direction.id), None, None)
            .await
            .unwrap()
            .is_empty());
        let proposals = store.list_pending_proposals().await.unwrap();
        assert_eq!(proposals.len(), 1);
        assert!(proposals[0]
            .rationale
            .as_deref()
            .unwrap()
            .contains("规则兜底"));
        store.apply_proposal(&proposals[0].id).await.unwrap();
        let rows = store
            .list_period_plans(Some(&input.direction.id), None, None)
            .await
            .unwrap();
        assert_eq!(rows.len(), 4);
        for (i, period) in input.periods.iter().enumerate() {
            let p = rows.iter().find(|p| p.plan.period == period.label).unwrap();
            assert_eq!(p.source, PlanSource::Ai);
            if i == 0 {
                assert_eq!(p.plan.target, "读完36本书 · 2026年部分");
            } else {
                let parent = rows
                    .iter()
                    .find(|p| p.plan.period == input.periods[i - 1].label)
                    .unwrap();
                assert_eq!(p.plan.parent_id.as_ref(), Some(&parent.id));
                assert_eq!(
                    p.plan.target,
                    format!("{} · {}部分", parent.plan.target, period.label)
                );
            }
        }
    }

    #[tokio::test]
    async fn user_rows_are_omitted_but_supply_quantitative_parent_targets() {
        let (store, mut input) = fixture().await;
        let annual = NewPeriodPlan {
            kind: PlanKind::Year,
            period: "2026".into(),
            parent_id: None,
            target: "读完12本书".into(),
            metric: PlanMetric::Count,
            metric_target: Some(12),
            routine_id: None,
            status: PlanStatus::Active,
        };
        let user = store
            .create_period_plan(&input.direction.id, &annual)
            .await
            .unwrap();
        input.plans = vec![user.clone()];
        let rows = fallback(&input);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter()
                .map(|p| p.metric_target.unwrap())
                .collect::<Vec<_>>(),
            vec![7, 4, 2]
        );
        assert_eq!(
            run(
                "user-parent",
                &input,
                ModelAccess::LocalOnly,
                None::<&NoModelPort>,
                &store,
                &store.ai_reader()
            )
            .await,
            "proposed"
        );
        let draft = store.list_pending_proposals().await.unwrap().remove(0);
        store.apply_proposal(&draft.id).await.unwrap();
        let all = store
            .list_period_plans(Some(&input.direction.id), None, None)
            .await
            .unwrap();
        assert_eq!(all.iter().find(|p| p.id == user.id).unwrap(), &user);
        input.plans = all;
        let quarter = input
            .plans
            .iter()
            .find(|p| p.plan.kind == PlanKind::Quarter)
            .unwrap()
            .clone();
        let changed = store
            .update_period_plan(
                &quarter.id,
                &crate::core::PeriodPlanPatch {
                    target: Some("我的季度".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        input.plans.retain(|p| p.id != quarter.id);
        input.plans.extend(changed);
        let draft = fallback(&input);
        assert!(!draft.iter().any(|p| p.kind == PlanKind::Quarter));
        assert!(draft.last().unwrap().target.starts_with("我的季度"));
    }

    struct InvalidModel;
    impl ModelPort for InvalidModel {
        async fn complete(&self, req: ModelRequest) -> Result<ModelReply, ModelFailure> {
            assert_eq!(req.schema_name, "sin90_decompose");
            let context: Value = serde_json::from_str(&req.messages[1].content).unwrap();
            assert_eq!(context["goal_from_year"], 2026);
            assert!(context.get("period_plans").is_some());
            assert!(context.get("recent_8_weeks_progress").is_some());
            Ok(ModelReply {
                text: "not JSON".into(),
                model_id: Some("invalid-test".into()),
                tier: ServedTier::Local,
                prompt_tokens: Some(10),
                completion_tokens: Some(2),
            })
        }
    }
    #[tokio::test]
    async fn invalid_model_json_degrades_and_records_both_attempts() {
        let (store, input) = fixture().await;
        assert_eq!(
            run(
                "invalid-json",
                &input,
                ModelAccess::LocalOnly,
                Some(&InvalidModel),
                &store,
                &store.ai_reader()
            )
            .await,
            "proposed"
        );
        let calls = store.list_ai_calls_for_run("invalid-json").await.unwrap();
        assert_eq!(calls.len(), 2);
        let raw: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT engine,error_kind FROM sin90_ai_calls WHERE run_id=? ORDER BY rowid",
        )
        .bind("invalid-json")
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert_eq!(
            raw,
            vec![
                ("local".into(), Some("bad_output".into())),
                ("reflex".into(), None)
            ]
        );
        assert!(store.list_pending_proposals().await.unwrap()[0]
            .rationale
            .as_deref()
            .unwrap()
            .contains("规则兜底"));
    }
    #[tokio::test]
    async fn strict_parser_accepts_only_scaffold_with_chinese_reason() {
        let (_, input) = fixture().await;
        let scaffold = fallback(&input);
        let mut value = json!({"plans":scaffold,"reason":"根据年度目标和最近八周进度拆分。"});
        assert!(parse(&value.to_string(), &scaffold).is_ok());
        value["plans"][0]["kind"] = json!("month");
        assert!(parse(&value.to_string(), &scaffold).is_err());
        value = json!({"plans":scaffold,"reason":"根据目标拆分"});
        value["plans"][0].as_object_mut().unwrap().remove("metric");
        assert!(parse(&value.to_string(), &scaffold).is_err());
        value = json!({"plans":scaffold,"reason":"根据目标拆分"});
        value["plans"][0]["metric_target"] = json!(-1);
        assert!(parse(&value.to_string(), &scaffold).is_err());
    }
    #[tokio::test]
    async fn user_edit_after_proposal_is_preserved_at_acceptance() {
        let (store, input) = fixture().await;
        assert_eq!(
            run(
                "race-user",
                &input,
                ModelAccess::LocalOnly,
                None::<&NoModelPort>,
                &store,
                &store.ai_reader()
            )
            .await,
            "proposed"
        );
        let draft = store.list_pending_proposals().await.unwrap().remove(0);
        let mut annual = fallback(&input).remove(0);
        annual.target = "采纳前写下的年度目标".into();
        let user = store
            .create_period_plan(&input.direction.id, &annual)
            .await
            .unwrap();
        store.apply_proposal(&draft.id).await.unwrap();
        let rows = store
            .list_period_plans(Some(&input.direction.id), None, None)
            .await
            .unwrap();
        assert_eq!(rows.iter().find(|p| p.id == user.id).unwrap(), &user);
        assert_eq!(
            rows.iter()
                .find(|p| p.plan.kind == PlanKind::Half)
                .unwrap()
                .plan
                .parent_id
                .as_ref(),
            Some(&user.id)
        );
    }

    #[test]
    fn decompose_uses_propose_ladder_and_honors_remote_permission() {
        for access in [ModelAccess::LocalOnly, ModelAccess::RemoteAllowed] {
            for executive_enabled in [false, true] {
                for present in [false, true] {
                    let settings = AiSettings { executive_enabled };
                    assert_eq!(
                        plan(Capability::Decompose, access, settings, present),
                        plan(Capability::Propose, access, settings, present)
                    );
                }
            }
        }
        assert_eq!(
            plan(
                Capability::Decompose,
                ModelAccess::RemoteAllowed,
                AiSettings {
                    executive_enabled: true
                },
                true
            ),
            vec![
                Step::Model(Engine::Executive),
                Step::Model(Engine::Local),
                Step::ReflexFallback
            ]
        );
    }
}
