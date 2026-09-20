//! Sin90's pure domain layer — entities, state machines, Proposal validation.
//!
//! Zero Agent24 dependency, zero I/O (design §5.2). Ported from Agent24's
//! `agent24-sin90` crate; see `types.rs`/`transitions.rs`/`proposal.rs` module
//! docs for what changed in the port (`Area`, `Task.parent_task_id`,
//! `CreateArea`/`CreateTask` ops).

pub mod proposal;
pub mod transitions;
pub mod types;
pub mod util;

pub use proposal::{
    validate, NewTask, ProposalError, ProposalSource, Sin90Op, Sin90Proposal, ValidationCtx,
};
pub use transitions::{
    area_transition_allowed, check_area_transition, check_direction_transition,
    check_proposal_transition, check_review_transition, check_rhythm_transition,
    check_schedule_block_transition, check_task_transition, check_week_transition,
    direction_is_terminal, direction_transition_allowed, proposal_is_terminal,
    proposal_transition_allowed, review_is_terminal, review_transition_allowed, rhythm_is_terminal,
    rhythm_transition_allowed, schedule_block_is_terminal, schedule_block_transition_allowed,
    task_is_terminal, task_transition_allowed, week_is_open, week_is_terminal,
    week_transition_allowed, TransitionError,
};
pub use types::{
    Alloc, Area, AreaId, AreaStatus, Direction, DirectionId, DirectionStatus, Energy,
    ProposalStatus, Review, ReviewId, ReviewKind, ReviewStatus, Rhythm, RhythmId, RhythmStatus,
    ScheduleBlock, ScheduleBlockId, ScheduleBlockStatus, Task, TaskId, TaskKind, TaskStatus, Week,
    WeekId, WeekStatus,
};
pub use util::{is_fixed_iso8601, now_iso8601, ulid};
