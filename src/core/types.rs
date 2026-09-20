//! Sin90 domain entities and their status enums (wire shapes).
//!
//! Statuses serialize snake_case to match the `sin90.db` TEXT columns and the
//! JSON wire. This module is pure data: no persistence, no Agent24 dependency
//! (design §5.2 — `core` is the zero-dependency layer).
//!
//! Ported from Agent24's `agent24-sin90` crate (design §0/§1); `Area` is new
//! (design §2 #1, §3.2), and `Task.parent_task_id` is a new column (design §2
//! #3, §3.2) — the rest is carried over unchanged.

use serde::{Deserialize, Serialize};

pub type DirectionId = String;
pub type TaskId = String;
pub type WeekId = String;
pub type RhythmId = String;
pub type ScheduleBlockId = String;
pub type ReviewId = String;
pub type AreaId = String;

// ---------------------------------------------------------------------------
// Status enums (each has a state machine in `transitions.rs`)
// ---------------------------------------------------------------------------

/// New (design §2 #1): a permanent, lifecycle-free container ("Work", "Health",
/// ...). Unlike every other status here, both edges of the two-state machine
/// are legal — archiving and un-archiving a life area is a normal action, not
/// a one-way door.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AreaStatus {
    Active,
    Archived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectionStatus {
    Draft,
    Active,
    Paused,
    Achieved,
    Abandoned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Backlog,
    Planned,
    InProgress,
    Done,
    Dropped,
    CarriedOver,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WeekStatus {
    Planning,
    Active,
    Reviewing,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleBlockStatus {
    Planned,
    Started,
    Completed,
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RhythmStatus {
    Active,
    Adjusted,
    Retired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    Draft,
    Finalized,
}

/// Persistent proposal lifecycle (backs `sin90_proposals.status`); its state
/// machine lives in `transitions.rs` like every other status. `applying` is the
/// CAS-claimed state that makes a re-tried accept idempotent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    Pending,
    Applying,
    Applied,
    Rejected,
}

// ---------------------------------------------------------------------------
// Descriptive value enums (classification outputs, not state machines)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    DeepWork,
    Admin,
    Meeting,
    Learning,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Energy {
    High,
    Mid,
    Low,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewKind {
    Daily,
    Weekly,
    Rhythm,
}

/// One direction's share of a Rhythm's attention budget, in whole percent.
/// `deny_unknown_fields`: this is model-produced input, so a stray/mistyped key
/// must fail loudly, not be silently dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Alloc {
    pub direction_id: DirectionId,
    pub pct: u32,
}

// ---------------------------------------------------------------------------
// Entities
// ---------------------------------------------------------------------------

/// New (design §2 #1, §3.2): a permanent life container above `Direction`
/// ("Work", "Health", "Learning", ...). No lifecycle beyond active/archived —
/// it does not "complete". `Direction.area_id` is optional, so an
/// uncategorized direction remains valid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Area {
    pub id: AreaId,
    pub title: String,
    pub slug: String,
    pub status: AreaStatus,
    pub sort_key: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Direction {
    pub id: DirectionId,
    /// New (design §3.2): optional link up to an `Area`. Nullable so an
    /// uncategorized direction is still legal.
    pub area_id: Option<AreaId>,
    pub title: String,
    pub status: DirectionStatus,
    /// A month or quarter window, e.g. "2026-08" or "2026-Q3".
    pub target_window: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub direction_id: Option<DirectionId>,
    pub week_id: Option<WeekId>,
    /// New (design §2 #3, §3.2): self-reference. A "Project" is simply a task
    /// with children — no separate entity. Constrained to exactly one level
    /// (a task with a parent may not itself have children) — see
    /// `store::repo` for the enforcing query.
    pub parent_task_id: Option<TaskId>,
    pub title: String,
    pub status: TaskStatus,
    pub kind: TaskKind,
    pub energy: Energy,
    pub est_minutes: Option<u32>,
    /// Set on the *new* task produced by a carry-over; links back to the closed one.
    pub carried_from: Option<TaskId>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Week {
    pub id: WeekId,
    pub status: WeekStatus,
    /// ISO week label, e.g. "2026-W33".
    pub iso_week: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rhythm {
    pub id: RhythmId,
    pub status: RhythmStatus,
    pub allocations: Vec<Alloc>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleBlock {
    pub id: ScheduleBlockId,
    pub direction_id: Option<DirectionId>,
    pub task_id: Option<TaskId>,
    pub status: ScheduleBlockStatus,
    pub planned_minutes: u32,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub id: ReviewId,
    pub kind: ReviewKind,
    pub status: ReviewStatus,
    pub week_id: Option<WeekId>,
    pub body: String,
    pub created_at: String,
    pub updated_at: String,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn status_enums_serialize_snake_case() {
        assert_eq!(
            serde_json::to_string(&TaskStatus::InProgress).unwrap(),
            "\"in_progress\""
        );
        assert_eq!(
            serde_json::to_string(&TaskStatus::CarriedOver).unwrap(),
            "\"carried_over\""
        );
        assert_eq!(
            serde_json::to_string(&DirectionStatus::Abandoned).unwrap(),
            "\"abandoned\""
        );
        assert_eq!(
            serde_json::to_string(&ScheduleBlockStatus::Completed).unwrap(),
            "\"completed\""
        );
        assert_eq!(
            serde_json::to_string(&AreaStatus::Archived).unwrap(),
            "\"archived\""
        );
    }

    #[test]
    fn status_enums_roundtrip() {
        let s = TaskStatus::CarriedOver;
        let j = serde_json::to_string(&s).unwrap();
        let back: TaskStatus = serde_json::from_str(&j).unwrap();
        assert_eq!(s, back);
    }
}
