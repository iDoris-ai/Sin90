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
pub type RoutineId = String;

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

/// New (design §2 #6, §3.2): a repeating execution template ("run every
/// weekday at 7am"), orthogonal to `Rhythm` (Rhythm allocates attention
/// *across* Directions; Routine is "do X on this cron"). `active <-> paused`
/// is a two-way door (pausing a routine for a trip and resuming it later is
/// normal); `retired` is the one-way exit — see `transitions.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutineStatus {
    Active,
    Paused,
    Retired,
}

/// New (design §2 #6, §3.2): what kind of recurring activity a `Routine`
/// represents. Descriptive, not a state machine (parallels `TaskKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutineKind {
    DeepWork,
    Exercise,
    Review,
    Read,
    Other,
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

/// New field (T4.1.1, design §2/§3.2/§4.1 M4 patch): `period` is a Review's
/// actual identity alongside `kind` — `UNIQUE(kind, period)` (migration
/// 0007) — replacing `week_id` as the thing that pins a Review to a moment,
/// since `week_id` can only ever express "this week" and a `daily`/`rhythm`
/// Review needs a calendar date / rhythm id instead. Shape depends on `kind`
/// (`core::util::validate_cron`-style per-kind validation lives in
/// `store::repo::validate_review_period`, since `rhythm`'s shape — "an
/// existing `Rhythm.id`" — is a relational check, not a pure one):
///   - `daily`  → `YYYY-MM-DD` ([`crate::core::canonical_iso_date`])
///   - `weekly` → `YYYY-Www` ([`crate::core::canonical_iso_week`])
///   - `rhythm` → the referenced [`RhythmId`]
///
/// `week_id` is kept (unchanged column, still nullable) for wire/schema
/// back-compat, but new code never sets it — `period` is now the sole
/// identity axis.
///
/// New field (T4.2.1, design §2 #11/§4.2, migration 0008): `body_ref` is the
/// ONE-WAY Markdown export path, RELATIVE TO `data_dir`
/// (`reviews/<kind>/<period>.md`), written atomically by
/// `store::repo::finalize_review` the moment a Review turns `finalized` — a
/// `draft` Review always has `body_ref: None` (nothing exported yet). SQLite
/// stays the source of truth: `body_ref` is a pointer for humans/tools that
/// want the plain-text file, never a second place the API reads from — every
/// read (`GET /reviews`, `GET /reviews/{id}`) answers from `body`, and a
/// hand-edited `.md` file is never reflected back into this struct.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub id: ReviewId,
    pub kind: ReviewKind,
    pub status: ReviewStatus,
    pub week_id: Option<WeekId>,
    pub period: String,
    pub body: String,
    pub body_ref: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// The wire/store shape for creating a [`Review`] (T4.1.1, design §2/§3.2).
/// `deny_unknown_fields`: client input, same convention as [`NewRoutine`].
/// `body` is not accepted here — a fresh Review always starts as an empty
/// draft; use `PATCH /reviews/{id}` ([`ReviewPatch`]) to write its body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewReview {
    pub kind: ReviewKind,
    pub period: String,
}

/// The wire/store shape for `PATCH /reviews/{id}` (T4.1.1): the ONLY thing a
/// Review patch can change is its `body` — `status` moves only through
/// `POST /reviews/{id}/finalize` (design's `draft → finalized`, a one-way
/// door, not a field to overwrite), and `kind`/`period` are a Review's fixed
/// identity, immutable after creation. Unlike [`RoutinePatch`], `body` is a
/// single required field, not a double-`Option`: there is no "absent vs.
/// null vs. set" distinction to make when there is only one field to patch.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewPatch {
    pub body: String,
}

/// New (design §2 #6, §3.2, M3): a repeating execution template. `area_id`
/// and `direction_id` are both optional and independent — a routine may hang
/// off neither, either, or (rarely) both. `cron` is a bare 5-field expression
/// (no seconds field, no embedded timezone — `tz` carries that) in the `cron`
/// crate's OWN dialect, which is NOT POSIX: its day-of-week field is
/// `1=Sun..7=Sat`, not POSIX's `0/7=Sun,1=Mon` (T3.1.1 review, "H1") — see
/// `core::util::validate_cron`'s doc comment for why that means digits are
/// refused there and only `*`/weekday names are accepted. This cron string
/// is what eventually becomes `ScheduleSpec::Cron.expr` once T3.3.1's outbox
/// upserts it into the kernel scheduler — Sin90 itself never computes a next
/// firing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Routine {
    pub id: RoutineId,
    pub area_id: Option<AreaId>,
    pub direction_id: Option<DirectionId>,
    pub title: String,
    pub kind: RoutineKind,
    pub cron: String,
    /// IANA timezone name (e.g. `"America/New_York"`); defaults to `"UTC"`.
    pub tz: String,
    pub target_count: Option<u32>,
    pub target_minutes: Option<u32>,
    pub status: RoutineStatus,
    pub created_at: String,
    pub updated_at: String,
}

/// The wire/store shape for creating a [`Routine`] (T3.1.1 review, "M2").
/// `deny_unknown_fields`: this is client input (eventually the body of
/// `POST /routines`, T3.1.2), so a stray/mistyped key must fail loudly, not
/// silently drop it (same convention as [`Alloc`]/[`NewTask`]). `tz` is a
/// single `Option`, unlike [`RoutinePatch`]'s: on CREATE there is no existing
/// value to preserve, so "absent" can only ever mean "use the default
/// (`UTC`)" — the absent-vs-null distinction only matters when a value could
/// already be set, which is PATCH's problem, not POST's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewRoutine {
    pub title: String,
    #[serde(default)]
    pub area_id: Option<AreaId>,
    #[serde(default)]
    pub direction_id: Option<DirectionId>,
    pub kind: RoutineKind,
    pub cron: String,
    #[serde(default)]
    pub tz: Option<String>,
    #[serde(default)]
    pub target_count: Option<u32>,
    #[serde(default)]
    pub target_minutes: Option<u32>,
}

/// The wire/store shape for a partial `Routine` update (T3.1.1 review, "M2";
/// eventually `PATCH /routines/{id}`, T3.1.2). Status changes are NOT part of
/// this shape — those go through the transition endpoint/`RoutineStatus`
/// argument instead (design §3.2's `active⇄paused→retired` machine, not a
/// field to overwrite).
///
/// Every field is `None` by default (an absent JSON key), meaning "leave
/// this field unchanged" — a `RoutinePatch::default()` therefore updates
/// nothing (`store::repo`'s L1: a no-op patch writes no event and does not
/// touch `updated_at`). `target_count`/`target_minutes` are double
/// `Option`s specifically so a `PATCH` body can tell "key absent" (`None`,
/// don't touch) apart from `"target_count": null` (`Some(None)`, clear the
/// column) apart from `"target_count": 3` (`Some(Some(3))`, set it) — see
/// [`deserialize_double_option`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RoutinePatch {
    pub title: Option<String>,
    pub cron: Option<String>,
    pub tz: Option<String>,
    #[serde(deserialize_with = "deserialize_double_option")]
    pub target_count: Option<Option<u32>>,
    #[serde(deserialize_with = "deserialize_double_option")]
    pub target_minutes: Option<Option<u32>>,
}

/// New (T3.2.2, design §2 #16): which of the kernel's two fire sources
/// produced a `POST /_a24/scheduler/fired` delivery (Agent24 design doc
/// `ME4-S1-scheduler-callback.md` §4.2/§5.3 — the `trigger` domain a
/// `fire_id` is derived from, `tick` for a due cron slot and `run_now` for a
/// manually-triggered one). Typed here (not left as a bare `String` on the
/// wire) so an unrecognized value is a clean 400 at deserialize time, same
/// convention as every other closed-set field in this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FireTrigger {
    Tick,
    RunNow,
}

/// The standard serde "double `Option`" trick: applied to a field already
/// typed `Option<Option<T>>`, it deserializes the INNER `Option<T>`
/// normally (so a JSON `null` becomes `None`, a value becomes `Some(v)`) and
/// wraps the result in `Some` — meaning this function only ever runs when
/// the key was present at all. Paired with `#[serde(default)]` (on the
/// field or, as here, the whole struct), a MISSING key keeps the outer
/// `None` that `default()` set, never reaching this function. No extra
/// crate (e.g. `serde_with`) needed — this is the same few-line pattern
/// that crate's `serde_with::rust::double_option` module wraps.
fn deserialize_double_option<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
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
        assert_eq!(
            serde_json::to_string(&RoutineStatus::Retired).unwrap(),
            "\"retired\""
        );
        assert_eq!(
            serde_json::to_string(&RoutineKind::DeepWork).unwrap(),
            "\"deep_work\""
        );
        assert_eq!(
            serde_json::to_string(&FireTrigger::RunNow).unwrap(),
            "\"run_now\""
        );
    }

    #[test]
    fn status_enums_roundtrip() {
        let s = TaskStatus::CarriedOver;
        let j = serde_json::to_string(&s).unwrap();
        let back: TaskStatus = serde_json::from_str(&j).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn routine_patch_absent_key_leaves_none_and_unknown_key_is_rejected() {
        let empty: RoutinePatch = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, RoutinePatch::default());
        assert_eq!(empty.target_count, None);

        let err = serde_json::from_str::<RoutinePatch>(r#"{"nope": 1}"#).unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    /// M2's whole point: a `PATCH` body must be able to tell "key absent"
    /// (don't touch), `"target_count": null` (clear it), and
    /// `"target_count": 3` (set it) apart — three JSON shapes, three
    /// distinct `Option<Option<u32>>` values.
    #[test]
    fn routine_patch_double_option_distinguishes_absent_null_and_set() {
        let absent: RoutinePatch = serde_json::from_str(r#"{"title":"x"}"#).unwrap();
        assert_eq!(absent.target_count, None);

        let cleared: RoutinePatch = serde_json::from_str(r#"{"target_count": null}"#).unwrap();
        assert_eq!(cleared.target_count, Some(None));

        let set: RoutinePatch = serde_json::from_str(r#"{"target_count": 3}"#).unwrap();
        assert_eq!(set.target_count, Some(Some(3)));
    }

    #[test]
    fn new_routine_rejects_unknown_fields() {
        let err = serde_json::from_str::<NewRoutine>(
            r#"{"title":"x","kind":"other","cron":"* * * * *","oops":true}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    #[test]
    fn new_review_rejects_unknown_fields_and_omits_body() {
        let err = serde_json::from_str::<NewReview>(
            r#"{"kind":"daily","period":"2026-09-24","oops":true}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");

        // A fresh Review never accepts an initial body through this shape.
        let err = serde_json::from_str::<NewReview>(
            r#"{"kind":"daily","period":"2026-09-24","body":"x"}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    #[test]
    fn review_patch_rejects_unknown_fields_and_requires_body() {
        let ok: ReviewPatch = serde_json::from_str(r#"{"body":"new text"}"#).unwrap();
        assert_eq!(ok.body, "new text");

        let err = serde_json::from_str::<ReviewPatch>(r#"{"body":"x","status":"finalized"}"#)
            .unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");

        let err = serde_json::from_str::<ReviewPatch>(r#"{}"#).unwrap_err();
        assert!(err.to_string().contains("missing field"), "{err}");
    }
}
