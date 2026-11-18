use super::*;
use crate::core::{NewPeriodPlan, PeriodPlanPatch, PlanKind};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateReq {
    direction_id: String,
    #[serde(flatten)]
    plan: NewPeriodPlan,
}

#[derive(Deserialize, Default)]
pub(super) struct PlanQuery {
    direction_id: Option<String>,
    kind: Option<PlanKind>,
    period: Option<String>,
}

pub(super) async fn list(State(state): State<Sin90State>, Query(q): Query<PlanQuery>) -> Response {
    match state
        .store
        .list_period_plans(q.direction_id.as_deref(), q.kind, q.period.as_deref())
        .await
    {
        Ok(plans) => Json(serde_json::json!({"period_plans": plans})).into_response(),
        Err(e) => map_err(e),
    }
}

pub(super) async fn create(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    let req: CreateReq = match parse(&body, "period plan") {
        Ok(v) => v,
        Err(r) => return r,
    };
    match state
        .store
        .create_period_plan(&req.direction_id, &req.plan)
        .await
    {
        Ok(plan) => {
            state.emit("period_plan.created", serde_json::json!(plan));
            (StatusCode::CREATED, Json(plan)).into_response()
        }
        Err(e) => map_err(e),
    }
}

pub(super) async fn update(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    AxPath(id): AxPath<String>,
    body: Bytes,
) -> Response {
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    let patch: PeriodPlanPatch = match parse(&body, "period plan patch") {
        Ok(v) => v,
        Err(r) => return r,
    };
    match state.store.update_period_plan(&id, &patch).await {
        Ok(plans) => {
            for plan in &plans {
                state.emit("period_plan.updated", serde_json::json!(plan));
            }
            Json(&plans[0]).into_response()
        }
        Err(e) => map_err(e),
    }
}
