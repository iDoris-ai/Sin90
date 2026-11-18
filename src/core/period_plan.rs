use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanKind {
    Year,
    Half,
    Quarter,
    Month,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanMetric {
    #[default]
    None,
    Count,
    Minutes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    #[default]
    Draft,
    Active,
    Done,
    Missed,
    Dropped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanSource {
    Ai,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewPeriodPlan {
    pub kind: PlanKind,
    pub period: String,
    pub parent_id: Option<String>,
    pub target: String,
    #[serde(default)]
    pub metric: PlanMetric,
    pub metric_target: Option<i64>,
    pub routine_id: Option<String>,
    #[serde(default)]
    pub status: PlanStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeriodPlan {
    pub id: String,
    pub direction_id: String,
    #[serde(flatten)]
    pub plan: NewPeriodPlan,
    pub source: PlanSource,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PeriodPlanPatch {
    pub target: Option<String>,
    pub metric: Option<PlanMetric>,
    #[serde(deserialize_with = "super::types::deserialize_double_option")]
    pub metric_target: Option<Option<i64>>,
    #[serde(deserialize_with = "super::types::deserialize_double_option")]
    pub routine_id: Option<Option<String>>,
    pub status: Option<PlanStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedPeriodPlan {
    pub id: String,
    pub direction_id: String,
    pub kind: PlanKind,
    pub period: String,
}

pub fn valid_period(kind: PlanKind, period: &str) -> bool {
    let b = period.as_bytes();
    if b.len() < 4 || !b[..4].iter().all(u8::is_ascii_digit) || &b[..4] == b"0000" {
        return false;
    }
    match kind {
        PlanKind::Year => b.len() == 4,
        PlanKind::Half => b.len() == 7 && &b[4..6] == b"-H" && matches!(b[6], b'1'..=b'2'),
        PlanKind::Quarter => b.len() == 7 && &b[4..6] == b"-Q" && matches!(b[6], b'1'..=b'4'),
        PlanKind::Month => {
            b.len() == 7
                && b[4] == b'-'
                && matches!((b[5], b[6]), (b'0', b'1'..=b'9') | (b'1', b'0'..=b'2'))
        }
    }
}

pub fn validate_plan(plan: &NewPeriodPlan) -> Result<(), String> {
    if !valid_period(plan.kind, &plan.period) {
        return Err("period does not match kind".into());
    }
    if plan.target.trim().is_empty() {
        return Err("target must not be blank".into());
    }
    if plan.metric_target.is_some_and(|n| n < 0) {
        return Err("metric_target must be nonnegative".into());
    }
    Ok(())
}

pub fn validate_parent(
    direction_id: &str,
    plan: &NewPeriodPlan,
    parent: Option<&PeriodPlan>,
) -> Result<(), String> {
    if plan.kind == PlanKind::Year {
        return if plan.parent_id.is_none() {
            Ok(())
        } else {
            Err("year cannot have a parent".into())
        };
    }
    let parent = parent.ok_or("parent plan is required")?;
    let valid = matches!(
        (plan.kind, parent.plan.kind),
        (PlanKind::Half, PlanKind::Year)
            | (PlanKind::Quarter, PlanKind::Half | PlanKind::Year)
            | (PlanKind::Month, PlanKind::Quarter)
    );
    if !valid || parent.direction_id != direction_id {
        return Err("parent must be an allowed upper level in the same direction".into());
    }
    // A parent must also contain its child's calendar period.
    let p = &parent.plan.period;
    let c = &plan.period;
    if p[..4] != c[..4] {
        return Err("parent must contain child's period".into());
    }
    if parent.plan.kind == PlanKind::Half && (c.as_bytes()[6] - b'1') / 2 != p.as_bytes()[6] - b'1'
    {
        return Err("quarter is outside parent half".into());
    }
    if parent.plan.kind == PlanKind::Quarter {
        let month: u8 = c[5..].parse().map_err(|_| "invalid month")?;
        if (month - 1) / 3 != p.as_bytes()[6] - b'1' {
            return Err("month is outside parent quarter".into());
        }
    }
    Ok(())
}
