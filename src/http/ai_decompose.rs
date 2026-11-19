use super::ai_classify::EmittingSink;
use super::ai_runs::{lock_registry, with_hard_deadline, AiRunItem, BusyGuard};
use super::state::HttpModelPort;
use super::*;
use crate::ai::decompose::{self, Input, Period};
use crate::ai::{Capability, MODEL_ACCESS};
use crate::core::{PlanKind, TRIAGE_DIRECTION_ID};
use crate::store::Sin90Store;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    direction_id: String,
    down_to: DownTo,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum DownTo {
    Month,
}

pub(super) async fn trigger(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(r) = state.require_any_actor(&headers) {
        return r;
    }
    let req: Request = match parse(&body, "ai decompose request") {
        Ok(r) => r,
        Err(r) => return r,
    };
    let DownTo::Month = req.down_to;
    if req.direction_id == TRIAGE_DIRECTION_ID {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid",
            "cannot decompose triage direction",
        );
    }
    let direction = match state.store.list_directions().await {
        Ok(rows) => match rows.into_iter().find(|d| d.id == req.direction_id) {
            Some(d) => d,
            None => {
                return error_response(StatusCode::NOT_FOUND, "not_found", "direction not found")
            }
        },
        Err(e) => return map_err(e),
    };
    let run_id = format!("run-{}", crate::core::ulid());
    {
        let mut registry = lock_registry(&state.ai_runs);
        if let Some(existing) = registry.busy_run(Capability::Decompose) {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"code":"ai_busy","run_id":existing})),
            )
                .into_response();
        }
        registry.start(Capability::Decompose, &run_id);
    }
    let guard = BusyGuard::new(state.ai_runs.clone(), Capability::Decompose, run_id.clone());
    let rid = run_id.clone();
    tokio::spawn(async move {
        let outcome = with_hard_deadline(state.run_hard_deadline, async {
            let input = input(&state.store, direction).await?;
            let reader = state.store.ai_reader();
            let model = state.model.clone().map(HttpModelPort);
            let sink = EmittingSink {
                store: &state.store,
                sink: state.sink.clone(),
            };
            Ok::<_, crate::store::StoreError>(
                decompose::run(&rid, &input, *MODEL_ACCESS, model.as_ref(), &sink, &reader).await,
            )
        })
        .await;
        let result = match outcome {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "decompose input read failed");
                "aborted"
            }
            Err(_) => "aborted",
        };
        guard.finish(
            if result == "aborted" {
                "aborted"
            } else {
                "done"
            },
            vec![AiRunItem {
                target: req.direction_id,
                result: result.into(),
                reason: None,
            }],
        );
    });
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"run_id":run_id,"capability":"decompose"})),
    )
        .into_response()
}

async fn input(
    store: &Sin90Store,
    direction: crate::core::Direction,
) -> Result<Input, crate::store::StoreError> {
    let today = jiff::Zoned::now().date();
    let year = today.year();
    let month = today.month();
    let half = (month - 1) / 6;
    let quarter = (month - 1) / 3;
    let mut periods = Vec::new();
    for (kind, label, start_month, months) in [
        (PlanKind::Year, format!("{year:04}"), 1, 12),
        (
            PlanKind::Half,
            format!("{year:04}-H{}", half + 1),
            half * 6 + 1,
            6,
        ),
        (
            PlanKind::Quarter,
            format!("{year:04}-Q{}", quarter + 1),
            quarter * 3 + 1,
            3,
        ),
        (PlanKind::Month, format!("{year:04}-{month:02}"), month, 1),
    ] {
        let start = jiff::civil::Date::new(year, start_month, 1).expect("current calendar");
        let end = start
            .checked_add(jiff::Span::new().months(months))
            .expect("current calendar");
        let days = i64::from(start.until(end).expect("calendar interval").get_days());
        periods.push(Period { kind, label, days });
    }
    let plans = store
        .list_period_plans(Some(&direction.id), None, None)
        .await?;
    let mut progress = store
        .progress(&direction.id, Some(&today.to_string()))
        .await?;
    let first = today
        .checked_sub(jiff::Span::new().weeks(7))
        .expect("recent calendar")
        .iso_week_date();
    let first = format!("{:04}-W{:02}", first.year(), first.week());
    for p in &mut progress {
        p.weeks.retain(|w| w.iso_week >= first);
    }
    Ok(Input {
        direction,
        plans,
        periods,
        progress: serde_json::to_value(progress)?,
    })
}
