//! The "AI does not write the DB" gate as a pure, testable validator.
//!
//! Everything an AI (Local/Executive brain) or a Rule produces is a
//! [`Sin90Proposal`] — a batch of atomic [`Sin90Op`]s. It is persisted `pending`
//! and, on accept, validated by [`validate`] and applied in ONE `sin90.db`
//! transaction with CAS idempotency. This module is the validation half: pure,
//! no DB. The store reads current state under a write lock and hands it in via
//! [`ValidationCtx`], so `validate` never touches I/O.
//!
//! Ported from Agent24's `agent24-sin90` (design §2 #10/#12 — this mechanism is
//! already satisfied by the kernel implementation, confirmed and carried over
//! unchanged). New in this port (design §3.3): `CreateArea` / `CreateTask` ops,
//! and `ValidationCtx` widened with `area_exists` / `task_parent` — the ONE
//! widening this port makes to that trait (widening it again later is a
//! breaking change across every implementor, so this widens it all the way M0
//! needs in one pass).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use std::collections::HashSet;

use crate::core::transitions::{
    check_task_transition, direction_is_terminal, task_is_terminal, week_is_open, TransitionError,
};
use crate::core::types::{
    Alloc, AreaId, DirectionId, DirectionStatus, Energy, ProposalStatus, ReviewId, ReviewStatus,
    RhythmId, TaskId, TaskKind, TaskStatus, WeekId, WeekStatus, TRIAGE_DIRECTION_ID,
};

/// A new task to create inside a week (fields the AI proposes; ids/timestamps
/// are minted by the store on apply). `deny_unknown_fields`: model output with a
/// stray/mistyped key must fail loudly, not silently drop it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewTask {
    pub title: String,
    pub direction_id: Option<DirectionId>,
}

/// One atomic change. A proposal is an ordered batch of these.
///
/// `deny_unknown_fields` (SFU-9): on an internally tagged enum (`tag = "op"`)
/// this attribute placed on the enum itself IS honored per-variant by serde —
/// confirmed by a standalone repro (`/tmp/serde_probe`) against this repo's
/// serde version before relying on it here; there is no need to duplicate the
/// attribute on each variant's fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Sin90Op {
    /// New (design §3.3): an AI-proposed Area. Direct creation (used by the
    /// human-facing HTTP route) does not go through this — only the AI path
    /// does, per §7.1's "AI does not write directly" gate.
    CreateArea {
        title: String,
    },
    /// New (design §3.3): an AI-proposed Task, optionally nested under a
    /// parent (making the parent a "Project" per §2 #3).
    CreateTask {
        title: String,
        direction_id: Option<DirectionId>,
        parent_task_id: Option<TaskId>,
        #[serde(default)]
        kind: Option<TaskKind>,
        #[serde(default)]
        energy: Option<Energy>,
        #[serde(default)]
        est_minutes: Option<u32>,
    },
    CreateDirection {
        title: String,
        target_window: String,
    },
    TransitionTask {
        task_id: TaskId,
        to: TaskStatus,
    },
    CreateTasks {
        week_id: WeekId,
        tasks: Vec<NewTask>,
    },
    ReorderTasks {
        week_id: WeekId,
        order: Vec<TaskId>,
    },
    AdjustRhythm {
        rhythm_id: RhythmId,
        new_alloc: Vec<Alloc>,
    },
    /// Atomic: close the source task (→ carried_over) and create a fresh task
    /// in `to_week` linked via `carried_from`.
    CarryOverTask {
        task_id: TaskId,
        to_week: WeekId,
    },
    /// New (design §11.2.1, T5.2.1): assign a still-unclassified ("inbox")
    /// task to a Direction. This is the classify capability's ONLY writable
    /// op (`store::ai_port::allowed_ops(Capability::Classify)`), though the
    /// Op itself is not gated to the AI path at the type level — a
    /// human/automation client may submit it directly through the existing
    /// `POST /proposals` (F-2), same as every other `Sin90Op`.
    ///
    /// **`deny_unknown_fields` (SFU-9, commit `9404756`, an independent PR
    /// not yet merged into this branch — see `docs/DESIGN-LIFEOS.md` §11.2)
    /// is deliberately NOT applied to this variant.** That commit's own doc
    /// comment claims serde honors `#[serde(deny_unknown_fields)]` "per
    /// variant" when placed on the ENUM's shared `#[serde(tag = "op", ...)]`
    /// attribute — true, but that is an enum-level attribute, not a
    /// per-variant one: `#[serde(deny_unknown_fields)]` written directly
    /// above ONE variant, as this task was first asked to try, does not
    /// compile (`error: unknown serde variant attribute
    /// "deny_unknown_fields"` — serde does not recognize it as a
    /// variant-level attribute at all). So there is no way to give only this
    /// new variant the protection without editing the enum's own
    /// `#[serde(...)]` line, which is exactly the line SFU-9's independent
    /// PR touches. Rather than risk that merge conflict, this variant is left
    /// with the SAME (temporary) lack of protection every one of the other
    /// eight variants has today — SFU-9, whenever it lands, adds
    /// `deny_unknown_fields` to the enum once and every variant (including
    /// this one) gains it uniformly, with zero special-casing needed here.
    AssignTaskDirection {
        task_id: TaskId,
        direction_id: DirectionId,
    },
    /// New (design §11.2.2, T5.2.1's "一次加齐" per §11.2.3/§11.8 — this Op
    /// itself is not summarize's capability logic, only its data-write
    /// primitive; `store::ai_port::allowed_ops(Capability::Summarize)` stays
    /// the empty set until T5.3.1 actually builds summarize and opens it):
    /// overwrite a `draft` Review's ENTIRE body. Not gated to the AI path at
    /// the type level, same as every other `Sin90Op` (F-2) — though in
    /// practice a human edits a Review body through `PATCH /reviews/{id}`
    /// (`update_review_body`, no CAS, no size cap, §11.9 R8), not this Op.
    ///
    /// **Why `base_body_sha256`**: an AI-drafted rewrite is generated from a
    /// body snapshot taken at propose time; if a human edits the SAME review
    /// while that proposal is still pending, accepting it must not silently
    /// clobber the human's edit. `base_body_sha256` pins the proposal to the
    /// body it was generated against — D6 rejects the accept if the review's
    /// current body has moved on (`StaleBase`), the same "AI does not get to
    /// overrule a human out from under them" posture §11.2.1's inbox check
    /// (A3) gives tasks.
    DraftReviewBody {
        review_id: ReviewId,
        base_body_sha256: String,
        body: String,
    },
}

/// ⚖️ design §11.2.2's 64 KiB cap — UTF-8 BYTES (`body.len()`, not char
/// count). Only constrains the AI path (D2); the human `PATCH
/// /reviews/{id}` path has no size cap today (§11.9 R8).
pub const MAX_REVIEW_BODY_BYTES: usize = 64 * 1024;

/// Lowercase-hex SHA-256 of a Review body — the "compare and swap" token
/// `DraftReviewBody.base_body_sha256` is checked against (D6) and
/// `Working.review_hash`/`ReviewSnap.body_sha256` store. Computed in Rust
/// (not SQL, per design §11.2.2's apply note), same `sha2`/`hex` pattern
/// `adapter_agent24::manifest_digest` already uses in this crate.
#[must_use]
pub fn body_sha256(body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body.as_bytes());
    hex::encode(hasher.finalize())
}

fn is_lowercase_hex_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// New (design §11.2.3, T5.2.1): a Review's status + body digest, as
/// `ValidationCtx::review_snap` hands it to `validate` — `body_sha256` is
/// ALREADY the digest (never the raw body), so the pure validator never
/// needs to see Review bodies, only compare tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewSnap {
    pub status: ReviewStatus,
    pub body_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalSource {
    LocalBrain,
    Executive,
    Rule,
}

/// `deny_unknown_fields`: a proposal is the most model-produced input in the
/// system — a stray/mistyped key must fail loudly, not be silently dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sin90Proposal {
    /// Client-stable id: re-submitting the same id is idempotent.
    pub id: String,
    pub status: ProposalStatus,
    pub source: ProposalSource,
    pub ops: Vec<Sin90Op>,
    pub rationale: Option<String>,
}

/// Read-only current-state lookup the store provides (under its write lock) so
/// validation stays pure. `None` means "no such entity".
///
/// SCOPE (the boundary, decided deliberately): this trait exposes only
/// per-entity *existence + status*. Validation here therefore covers
/// structural well-formedness and transition legality. It does NOT check
/// **relational** invariants that an FK cannot express — e.g. that a
/// `TransitionTask`'s task belongs to an *open* week. Those are enforced by
/// the store inside the apply transaction, under the same write lock.
pub trait ValidationCtx {
    fn task_status(&self, id: &str) -> Option<TaskStatus>;
    fn week_status(&self, id: &str) -> Option<WeekStatus>;
    fn rhythm_is_retired(&self, id: &str) -> Option<bool>;
    /// New (design §3.3): does this Area id exist? Used by `CreateTask`'s
    /// indirect `direction_id` path is NOT covered here — `direction_id` is
    /// left un-validated at the pure layer deliberately, same as the ported
    /// `CreateDirection`/`CreateTasks` ops always did (an FK violation at
    /// apply time maps to 404, which is the existing, tested behavior this
    /// port must not change).
    fn area_exists(&self, id: &str) -> bool;
    /// New (design §3.3): outer `None` = task does not exist; inner `None` =
    /// task exists but has no parent. Used to enforce the one-level-deep
    /// constraint (design §3.2) at proposal time, not just at apply time.
    fn task_parent(&self, id: &str) -> Option<Option<TaskId>>;
    /// New (design §11.2.3, T5.2.1): does this Direction id exist, and if so
    /// what's its current status? Used by `AssignTaskDirection`'s A4/A5.
    fn direction_status(&self, id: &str) -> Option<DirectionStatus>;
    /// T5.7.2 review round 2 (M6): was task `id`'s CURRENT 待定 parking
    /// produced by an ACCEPTED classify-capability proposal (the T5.2.2
    /// fallback), as opposed to a human/automation client directly
    /// submitting `AssignTaskDirection(t, 待定)` via `POST /proposals`
    /// (`capability_source = "direct"`, same domain `reject_proposal`
    /// already reports)? Only ever consulted when the task's current
    /// Direction IS the reserved 待定 id (A3's reclassify-out-of-triage
    /// branch below) — meaningless otherwise, so implementors need not
    /// answer it correctly for a task not currently parked in 待定.
    /// `false` for every task that has never been AI-fallback-parked (the
    /// common case, and every `MockCtx` test fixture that doesn't opt in).
    /// Used to narrow A3's "待定→真实 Direction" carve-out to ONLY the
    /// cases T5.2.2's own design intended it for — a task a human
    /// deliberately filed into 待定 by hand must NOT be silently
    /// reclassifiable out from under them by a later AI run (Q7: "用户的
    /// 东西不覆盖"), and must not be offered to classify's retry inbox
    /// either (`AiReadModel::inbox`/`inbox_task`, `store/ai_port.rs`, share
    /// the SAME narrowing via their own equivalent check).
    fn task_triage_via_classify(&self, id: &str) -> bool;
    /// New (design §11.2.3, T5.2.1): outer `None` = the task does not exist;
    /// inner `None` = the task exists and is currently "in the inbox" (no
    /// Direction assigned yet). Used by `AssignTaskDirection`'s A1/A3.
    fn task_direction(&self, id: &str) -> Option<Option<DirectionId>>;
    /// New (design §11.2.3, T5.2.1): a Review's status + body digest, or
    /// `None` if it does not exist. Used by `DraftReviewBody`'s D4-D6.
    ///
    /// §11.2.3/§11.8's "一次加齐": this trait is widened ONCE, for BOTH new
    /// Ops (`AssignTaskDirection` AND `DraftReviewBody`) in the same pass,
    /// even though only the former's capability (classify) is in scope this
    /// task — widening `ValidationCtx` is a breaking change for every
    /// implementor, so the design deliberately does not want to pay that cost
    /// twice. `DraftReviewBody` itself (the Op's validate/apply) is added
    /// alongside this method for the same reason; the `summarize`
    /// CAPABILITY that will eventually PRODUCE this Op (T5.3.1) is not —
    /// `store::ai_port::allowed_ops(Capability::Summarize)` stays the empty
    /// set here, same as `Sin90Op::AssignTaskDirection` needed nothing from
    /// `ai::classify` (which doesn't exist yet) to be a valid, applyable Op.
    fn review_snap(&self, id: &str) -> Option<ReviewSnap>;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProposalError {
    #[error("proposal has no ops")]
    Empty,
    #[error("unknown {entity}: {id}")]
    UnknownEntity { entity: &'static str, id: String },
    #[error(transparent)]
    IllegalTransition(#[from] TransitionError),
    #[error("week {week_id} is not open (status {status:?})")]
    WeekNotOpen { week_id: WeekId, status: WeekStatus },
    #[error("rhythm {rhythm_id} is retired; cannot adjust")]
    RhythmRetired { rhythm_id: RhythmId },
    #[error("{op} for week {week_id} has an empty list")]
    EmptyList { op: &'static str, week_id: WeekId },
    #[error("{op} references {id} more than once")]
    DuplicateRef { op: &'static str, id: String },
    #[error("allocation percentages sum to {sum_pct} (must be <= 100)")]
    InvalidAlloc { sum_pct: u32 },
    /// Opus 2026-09-23 review (H1/M1): the structural rhythm-allocation
    /// checks — non-empty, every `pct` an integer in 1..=100, sum <= 100, no
    /// duplicate direction — are now the ONE `check_alloc` both
    /// `AdjustRhythm`'s pure `validate` and `Sin90Store::create_rhythm`'s
    /// direct write call, so the two paths cannot silently drift apart.
    #[error("allocations must not be empty")]
    EmptyAllocations,
    #[error("pct must be an integer 1..=100, got {pct} for direction {direction_id}")]
    PctOutOfRange { direction_id: DirectionId, pct: u32 },
    #[error("{field} must not be blank")]
    BlankField { field: &'static str },
    #[error("task {parent_id} already has a parent; a task may only be nested one level deep")]
    NestedProject { parent_id: TaskId },
    /// New (design §11.2.1, T5.2.1): the task is not (or no longer) in the
    /// inbox — it already has a Direction, possibly assigned earlier in the
    /// SAME batch (A3).
    #[error("task {task_id} is not in the inbox (already assigned to direction {direction_id})")]
    NotInInbox {
        task_id: TaskId,
        direction_id: DirectionId,
    },
    /// New (design §11.2.1, T5.2.1): the task's current status (as of this
    /// point in the batch) is terminal — a closed task cannot be assigned a
    /// Direction (A2).
    #[error("task {task_id} is closed (status {status:?}); cannot assign a direction")]
    TaskClosed { task_id: TaskId, status: TaskStatus },
    /// New (design §11.2.1, T5.2.1): the target Direction is terminal
    /// (achieved/abandoned) — a closed Direction cannot receive new tasks (A5).
    #[error("direction {direction_id} is closed (status {status:?}); cannot assign")]
    DirectionClosed {
        direction_id: DirectionId,
        status: DirectionStatus,
    },
    /// New (design §11.2.2, T5.2.1): the Review is not `draft` (either
    /// already `finalized`, or — same effect — was finalized after this
    /// proposal was generated but before it was accepted) (D5).
    #[error("review {review_id} is not a draft; no further body changes are accepted")]
    ReviewNotDraft { review_id: ReviewId },
    /// New (design §11.2.2, T5.2.1): the entity's current body digest (as of
    /// this point in the batch) does not match the proposal's
    /// `base_body_sha256` — someone else changed it since this proposal was
    /// generated (D6). `entity`/`id` are generic (design §11.2.3's own
    /// signature) even though only `"review"` is reachable today.
    #[error("{entity} {id}'s body has changed since this proposal was generated")]
    StaleBase { entity: &'static str, id: String },
    /// New (design §11.2.2, T5.2.1): the proposed body is byte-identical to
    /// the current one — a no-op write is rejected rather than silently
    /// producing an empty diff (D7).
    #[error("{op} would not change anything")]
    NoChange { op: &'static str },
    /// New (design §11.2.2, T5.2.1): `field` exceeds its byte-size cap (D2 —
    /// today only `DraftReviewBody.body`, capped at
    /// [`MAX_REVIEW_BODY_BYTES`]).
    #[error("{field} exceeds the {max_bytes}-byte limit")]
    TooLarge {
        field: &'static str,
        max_bytes: usize,
    },
    /// New (design §11.2.2, T5.2.1): `base_body_sha256` is not 64 lowercase
    /// hex characters (D3) — malformed input, not a stale-base mismatch.
    #[error("base_body_sha256 must be 64 lowercase hex characters")]
    BadHash,
}

/// A working view that overlays the pending effects of earlier ops in the SAME
/// batch on top of the store snapshot. This makes a proposal validate as the
/// ordered sequence it is, not as N independent reads of the pre-batch state —
/// so `[t→InProgress, t→Done]` passes and `[t→InProgress, t→InProgress]` fails,
/// exactly as apply would behave under the write lock.
struct Working<'c> {
    ctx: &'c dyn ValidationCtx,
    task: std::collections::HashMap<String, TaskStatus>,
    /// New (design §11.2.3, T5.2.1): overlay for `AssignTaskDirection` — lets
    /// `[Assign(t,d1), Assign(t,d2)]` see `t` as already assigned after the
    /// first op, in the SAME batch, without a DB round-trip.
    task_directions: std::collections::HashMap<String, Option<DirectionId>>,
    /// New (design §11.2.3, T5.2.1): overlay for `DraftReviewBody` — the
    /// CURRENT body digest of a review, updated after each op in the batch
    /// that touches it. Two `DraftReviewBody` ops on the SAME review in one
    /// batch chain: the second's `base_body_sha256` must match the FIRST's
    /// new digest, not the pre-batch one.
    review_hash: std::collections::HashMap<String, String>,
}

impl<'c> Working<'c> {
    fn new(ctx: &'c dyn ValidationCtx) -> Self {
        Self {
            ctx,
            task: std::collections::HashMap::new(),
            task_directions: std::collections::HashMap::new(),
            review_hash: std::collections::HashMap::new(),
        }
    }
    fn task_status(&self, id: &str) -> Option<TaskStatus> {
        self.task
            .get(id)
            .copied()
            .or_else(|| self.ctx.task_status(id))
    }
    fn set_task(&mut self, id: &str, s: TaskStatus) {
        self.task.insert(id.to_string(), s);
    }
    fn task_direction(&self, id: &str) -> Option<Option<DirectionId>> {
        self.task_directions
            .get(id)
            .cloned()
            .or_else(|| self.ctx.task_direction(id))
    }
    fn set_task_direction(&mut self, id: &str, direction_id: Option<DirectionId>) {
        self.task_directions.insert(id.to_string(), direction_id);
    }
    /// The review's CURRENT body digest, overlay-first — `None` only if the
    /// review does not exist at all (`ctx.review_snap` returned `None`).
    fn review_body_hash(&self, id: &str) -> Option<String> {
        self.review_hash
            .get(id)
            .cloned()
            .or_else(|| self.ctx.review_snap(id).map(|s| s.body_sha256))
    }
    fn set_review_hash(&mut self, id: &str, hash: String) {
        self.review_hash.insert(id.to_string(), hash);
    }
}

/// Pure validation: structural well-formedness + every state-changing op must be
/// a legal transition from the entity's status *as of this point in the batch*
/// (store snapshot overlaid with earlier ops). Returns on the FIRST offending
/// op; the store rejects the whole proposal — apply is all-or-nothing.
/// Relational invariants are the store's job (see [`ValidationCtx`]).
///
/// `capability_source` (M-a, T5.7.2 review round 2): THIS proposal's own
/// `"classify"`/`"direct"` provenance — the same vocabulary
/// `Sin90Store::reject_proposal` already resolves (a matching `ok=1`
/// `sin90_ai_calls` row ⇒ `"classify"`, none ⇒ `"direct"`, since
/// `AssignTaskDirection` is the only op `Capability::Classify` ever
/// produces). Not a per-entity fact `ValidationCtx` could answer (it is
/// scoped to THIS call's proposal, not to any task/direction), so it is
/// passed alongside `ctx` instead of widening that trait. Only
/// `AssignTaskDirection`'s A3 carve-out reads it: a human/automation client
/// moving their OWN task out of 待定 (`capability_source == "direct"`) is
/// always allowed, regardless of who parked it there (Q7 "用户的东西不覆
/// 盖" cuts both ways — it must not lock the user out of their own task
/// either); an AI (`"classify"`) run may only reclassify OUT of 待定 what
/// `w.ctx.task_triage_via_classify` says classify itself put there (M6,
/// unaffected by this change).
pub fn validate(
    p: &Sin90Proposal,
    ctx: &dyn ValidationCtx,
    capability_source: &str,
) -> Result<(), ProposalError> {
    if p.ops.is_empty() {
        return Err(ProposalError::Empty);
    }
    let mut w = Working::new(ctx);
    for op in &p.ops {
        validate_op(op, &mut w, capability_source)?;
    }
    Ok(())
}

fn validate_op(
    op: &Sin90Op,
    w: &mut Working<'_>,
    capability_source: &str,
) -> Result<(), ProposalError> {
    match op {
        Sin90Op::CreateArea { title } => {
            non_blank("title", title)?;
            Ok(())
        }

        Sin90Op::CreateTask {
            title,
            parent_task_id,
            ..
        } => {
            non_blank("title", title)?;
            if let Some(parent_id) = parent_task_id {
                let parent =
                    w.ctx
                        .task_parent(parent_id)
                        .ok_or_else(|| ProposalError::UnknownEntity {
                            entity: "task",
                            id: parent_id.clone(),
                        })?;
                // One level deep only (design §3.2): a task that already has a
                // parent may not itself become a parent.
                if parent.is_some() {
                    return Err(ProposalError::NestedProject {
                        parent_id: parent_id.clone(),
                    });
                }
            }
            Ok(())
        }

        Sin90Op::CreateDirection {
            title,
            target_window,
        } => {
            non_blank("title", title)?;
            non_blank("target_window", target_window)?;
            Ok(())
        }

        Sin90Op::TransitionTask { task_id, to } => {
            let from = task_status(w, task_id)?;
            check_task_transition(from, *to)?;
            w.set_task(task_id, *to);
            Ok(())
        }

        Sin90Op::CreateTasks { week_id, tasks } => {
            if tasks.is_empty() {
                return Err(ProposalError::EmptyList {
                    op: "create_tasks",
                    week_id: week_id.clone(),
                });
            }
            require_open_week(w, week_id)
        }

        Sin90Op::ReorderTasks { week_id, order } => {
            require_open_week(w, week_id)?;
            if order.is_empty() {
                return Err(ProposalError::EmptyList {
                    op: "reorder_tasks",
                    week_id: week_id.clone(),
                });
            }
            reject_dupes("reorder_tasks", order.iter())
        }

        Sin90Op::AdjustRhythm {
            rhythm_id,
            new_alloc,
        } => {
            let retired =
                w.ctx
                    .rhythm_is_retired(rhythm_id)
                    .ok_or_else(|| ProposalError::UnknownEntity {
                        entity: "rhythm",
                        id: rhythm_id.clone(),
                    })?;
            if retired {
                return Err(ProposalError::RhythmRetired {
                    rhythm_id: rhythm_id.clone(),
                });
            }
            check_alloc(new_alloc)
        }

        Sin90Op::CarryOverTask { task_id, to_week } => {
            let from = task_status(w, task_id)?;
            // Closing side of the carry-over must itself be a legal task transition.
            check_task_transition(from, TaskStatus::CarriedOver)?;
            require_open_week(w, to_week)?;
            w.set_task(task_id, TaskStatus::CarriedOver);
            Ok(())
        }

        Sin90Op::AssignTaskDirection {
            task_id,
            direction_id,
        } => {
            // A1: the task must exist — `task_direction`'s OUTER `None` is
            // "no such task" (design §11.2.1's own wording: checked via
            // `task_direction`, not `task_status`).
            let current_direction =
                w.task_direction(task_id)
                    .ok_or_else(|| ProposalError::UnknownEntity {
                        entity: "task",
                        id: task_id.clone(),
                    })?;
            // A2: the task's current status (overlaid) must not be terminal.
            let status = task_status(w, task_id)?;
            if task_is_terminal(status) {
                return Err(ProposalError::TaskClosed {
                    task_id: task_id.clone(),
                    status,
                });
            }
            // A3: the task must still be in the inbox (no Direction yet) —
            // this is what makes `[Assign(t,d1), Assign(t,d2)]` reject the
            // second op (the overlay set by the first op is visible here).
            // 2026-09-24 review (round 2, M2): `direction_id` here must be
            // the task's CURRENT (already-assigned) Direction, not the
            // TARGET one this op was trying to assign — the error is "you
            // can't assign, it's already assigned to X", and X is
            // `current_direction`, not `direction_id`.
            //
            // T5.7.2 (design §2 #31, T5.2.2 followup ②): a task parked in
            // the reserved 待定 Direction is still reclassifiable —
            // assigning it to any OTHER Direction is allowed (falls through
            // to A4/A5 below), mirroring `AiReadModel::inbox`/`inbox_task`'s
            // own loosened membership. Re-"assigning" it to 待定 AGAIN is
            // NOT allowed (§2 #30's own "不放宽待定→待定"): that case, and
            // every other already-classified task, still hits the same
            // `NotInInbox` this arm always returned.
            if let Some(existing_direction_id) = current_direction {
                // T5.7.2 review round 2 (M6, narrowed by round 3's M-a): the
                // carve-out asks "is the CURRENT proposal itself the user's
                // own doing, or an AI run?" — `capability_source == "direct"`
                // (a human/automation client submitting `AssignTaskDirection`
                // straight via `POST /proposals`) always may move their own
                // task OUT of 待定, regardless of who parked it there in the
                // first place (M-a: a task the human filed into 待定 by hand
                // must remain movable BY THAT SAME HUMAN — Q7 "用户的东西不
                // 覆盖" protects the user's placement from AI, it must not
                // also lock the user out of it). An AI (`"classify"`) run may
                // only reclassify OUT of 待定 what classify's own fallback
                // put there (`task_triage_via_classify`, M6's original
                // scope, unaffected) — a human's direct placement stays put
                // unless the human themselves moves it.
                let reclassifiable_from_triage = existing_direction_id == TRIAGE_DIRECTION_ID
                    && direction_id != TRIAGE_DIRECTION_ID
                    && (capability_source == "direct" || w.ctx.task_triage_via_classify(task_id));
                if !reclassifiable_from_triage {
                    return Err(ProposalError::NotInInbox {
                        task_id: task_id.clone(),
                        direction_id: existing_direction_id,
                    });
                }
            }
            // A4: the target Direction must exist.
            let dstatus = w.ctx.direction_status(direction_id).ok_or_else(|| {
                ProposalError::UnknownEntity {
                    entity: "direction",
                    id: direction_id.clone(),
                }
            })?;
            // A5: the target Direction must not be terminal.
            if direction_is_terminal(dstatus) {
                return Err(ProposalError::DirectionClosed {
                    direction_id: direction_id.clone(),
                    status: dstatus,
                });
            }
            w.set_task_direction(task_id, Some(direction_id.clone()));
            Ok(())
        }

        Sin90Op::DraftReviewBody {
            review_id,
            base_body_sha256,
            body,
        } => {
            non_blank("body", body)?; // D1
            if body.len() > MAX_REVIEW_BODY_BYTES {
                // D2 (UTF-8 bytes)
                return Err(ProposalError::TooLarge {
                    field: "body",
                    max_bytes: MAX_REVIEW_BODY_BYTES,
                });
            }
            if !is_lowercase_hex_sha256(base_body_sha256) {
                return Err(ProposalError::BadHash); // D3
            }
            let snap =
                w.ctx
                    .review_snap(review_id)
                    .ok_or_else(|| ProposalError::UnknownEntity {
                        entity: "review",
                        id: review_id.clone(),
                    })?; // D4
            if snap.status != ReviewStatus::Draft {
                return Err(ProposalError::ReviewNotDraft {
                    review_id: review_id.clone(),
                }); // D5
            }
            // Overlay-aware: a prior `DraftReviewBody` on the SAME review
            // earlier in this batch already moved the digest forward.
            let current_hash = w
                .review_body_hash(review_id)
                .unwrap_or(snap.body_sha256.clone());
            if current_hash != *base_body_sha256 {
                return Err(ProposalError::StaleBase {
                    entity: "review",
                    id: review_id.clone(),
                }); // D6
            }
            let new_hash = body_sha256(body);
            if new_hash == current_hash {
                return Err(ProposalError::NoChange {
                    op: "draft_review_body",
                }); // D7
            }
            w.set_review_hash(review_id, new_hash);
            Ok(())
        }
    }
}

fn non_blank(field: &'static str, value: &str) -> Result<(), ProposalError> {
    if value.trim().is_empty() {
        Err(ProposalError::BlankField { field })
    } else {
        Ok(())
    }
}

fn task_status(w: &Working<'_>, id: &str) -> Result<TaskStatus, ProposalError> {
    w.task_status(id)
        .ok_or_else(|| ProposalError::UnknownEntity {
            entity: "task",
            id: id.to_string(),
        })
}

fn require_open_week(w: &Working<'_>, id: &str) -> Result<(), ProposalError> {
    let status = w
        .ctx
        .week_status(id)
        .ok_or_else(|| ProposalError::UnknownEntity {
            entity: "week",
            id: id.to_string(),
        })?;
    if week_is_open(status) {
        Ok(())
    } else {
        Err(ProposalError::WeekNotOpen {
            week_id: id.to_string(),
            status,
        })
    }
}

fn reject_dupes<'a>(
    op: &'static str,
    ids: impl Iterator<Item = &'a String>,
) -> Result<(), ProposalError> {
    let mut seen = HashSet::new();
    for id in ids {
        if !seen.insert(id.as_str()) {
            return Err(ProposalError::DuplicateRef { op, id: id.clone() });
        }
    }
    Ok(())
}

/// The ONE structural check for a Rhythm's allocation list (Opus 2026-09-23
/// review H1): non-empty, every `pct` an integer in 1..=100, no duplicate
/// direction, sum <= 100. Shared by `AdjustRhythm`'s `validate_op` arm below
/// AND `Sin90Store::create_rhythm`'s direct write — before this fix the two
/// paths each had their own (divergent, incomplete) copy. Existence of the
/// referenced directions is NOT checked here — same scope boundary as the
/// rest of `ValidationCtx` (module doc): that's a relational check against
/// live rows, done under the write lock by `require_directions_exist` in
/// `store::repo`, identically for both paths.
pub fn check_alloc(alloc: &[Alloc]) -> Result<(), ProposalError> {
    if alloc.is_empty() {
        return Err(ProposalError::EmptyAllocations);
    }
    let mut seen = HashSet::new();
    let mut sum: u32 = 0;
    for a in alloc {
        if !(1..=100).contains(&a.pct) {
            return Err(ProposalError::PctOutOfRange {
                direction_id: a.direction_id.clone(),
                pct: a.pct,
            });
        }
        if !seen.insert(a.direction_id.as_str()) {
            return Err(ProposalError::DuplicateRef {
                op: "adjust_rhythm",
                id: a.direction_id.clone(),
            });
        }
        // Saturating so a huge value can't wrap; > 100 is rejected below.
        sum = sum.saturating_add(a.pct);
    }
    if sum > 100 {
        Err(ProposalError::InvalidAlloc { sum_pct: sum })
    } else {
        Ok(())
    }
}

// Referenced so `cargo build` does not warn about an unused import when this
// module is compiled standalone (AreaId is part of the public Sin90Op surface
// via CreateArea's future callers, kept here for signature clarity).
#[allow(dead_code)]
type _AreaIdUsed = AreaId;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashMap;

    use super::*;

    #[derive(Default)]
    struct MockCtx {
        tasks: HashMap<TaskId, TaskStatus>,
        task_parents: HashMap<TaskId, Option<TaskId>>,
        weeks: HashMap<WeekId, WeekStatus>,
        rhythms_retired: HashMap<RhythmId, bool>,
        areas: HashSet<AreaId>,
        directions: HashMap<DirectionId, DirectionStatus>,
        /// Outer presence = task exists; `None` inner = task is in the inbox.
        task_directions: HashMap<TaskId, Option<DirectionId>>,
        reviews: HashMap<ReviewId, ReviewSnap>,
        /// T5.7.2 review round 2 (M6): task ids whose 待定 parking this
        /// fixture wants `task_triage_via_classify` to report `true` for —
        /// every other id (the default) reports `false`.
        triage_via_classify: HashSet<TaskId>,
    }

    impl ValidationCtx for MockCtx {
        fn task_status(&self, id: &str) -> Option<TaskStatus> {
            self.tasks.get(id).copied()
        }
        fn week_status(&self, id: &str) -> Option<WeekStatus> {
            self.weeks.get(id).copied()
        }
        fn rhythm_is_retired(&self, id: &str) -> Option<bool> {
            self.rhythms_retired.get(id).copied()
        }
        fn area_exists(&self, id: &str) -> bool {
            self.areas.contains(id)
        }
        fn task_parent(&self, id: &str) -> Option<Option<TaskId>> {
            self.task_parents.get(id).cloned()
        }
        fn direction_status(&self, id: &str) -> Option<DirectionStatus> {
            self.directions.get(id).copied()
        }
        fn task_direction(&self, id: &str) -> Option<Option<DirectionId>> {
            self.task_directions.get(id).cloned()
        }
        fn review_snap(&self, id: &str) -> Option<ReviewSnap> {
            self.reviews.get(id).cloned()
        }
        fn task_triage_via_classify(&self, id: &str) -> bool {
            self.triage_via_classify.contains(id)
        }
    }

    fn proposal(ops: Vec<Sin90Op>) -> Sin90Proposal {
        Sin90Proposal {
            id: "p1".into(),
            status: ProposalStatus::Pending,
            source: ProposalSource::LocalBrain,
            ops,
            rationale: None,
        }
    }

    #[test]
    fn empty_proposal_rejected() {
        let ctx = MockCtx::default();
        assert_eq!(
            validate(&proposal(vec![]), &ctx, "direct"),
            Err(ProposalError::Empty)
        );
    }

    #[test]
    fn create_area_requires_non_blank_title() {
        let ctx = MockCtx::default();
        let ok = proposal(vec![Sin90Op::CreateArea {
            title: "Work".into(),
        }]);
        assert!(validate(&ok, &ctx, "direct").is_ok());

        let bad = proposal(vec![Sin90Op::CreateArea { title: "  ".into() }]);
        assert_eq!(
            validate(&bad, &ctx, "direct"),
            Err(ProposalError::BlankField { field: "title" })
        );
    }

    #[test]
    fn create_task_with_unparented_parent_ok_nested_parent_rejected() {
        let mut ctx = MockCtx::default();
        ctx.task_parents.insert("root".into(), None);
        ctx.task_parents.insert("child".into(), Some("root".into()));

        let ok = proposal(vec![Sin90Op::CreateTask {
            title: "sub-task".into(),
            direction_id: None,
            parent_task_id: Some("root".into()),
            kind: None,
            energy: None,
            est_minutes: None,
        }]);
        assert!(validate(&ok, &ctx, "direct").is_ok());

        // "child" already has a parent ("root") — nesting under it is rejected.
        let bad = proposal(vec![Sin90Op::CreateTask {
            title: "grandchild".into(),
            direction_id: None,
            parent_task_id: Some("child".into()),
            kind: None,
            energy: None,
            est_minutes: None,
        }]);
        assert_eq!(
            validate(&bad, &ctx, "direct"),
            Err(ProposalError::NestedProject {
                parent_id: "child".into()
            })
        );
    }

    #[test]
    fn create_task_unknown_parent_rejected() {
        let ctx = MockCtx::default();
        let p = proposal(vec![Sin90Op::CreateTask {
            title: "x".into(),
            direction_id: None,
            parent_task_id: Some("ghost".into()),
            kind: None,
            energy: None,
            est_minutes: None,
        }]);
        assert_eq!(
            validate(&p, &ctx, "direct"),
            Err(ProposalError::UnknownEntity {
                entity: "task",
                id: "ghost".into()
            })
        );
    }

    #[test]
    fn create_direction_always_ok() {
        let ctx = MockCtx::default();
        let p = proposal(vec![Sin90Op::CreateDirection {
            title: "iDoris site".into(),
            target_window: "2026-08".into(),
        }]);
        assert!(validate(&p, &ctx, "direct").is_ok());
    }

    #[test]
    fn transition_task_legal_and_illegal() {
        let mut ctx = MockCtx::default();
        ctx.tasks.insert("t1".into(), TaskStatus::Planned);

        let ok = proposal(vec![Sin90Op::TransitionTask {
            task_id: "t1".into(),
            to: TaskStatus::InProgress,
        }]);
        assert!(validate(&ok, &ctx, "direct").is_ok());

        let bad = proposal(vec![Sin90Op::TransitionTask {
            task_id: "t1".into(),
            to: TaskStatus::Done, // planned -> done is illegal (must go through in_progress)
        }]);
        assert!(matches!(
            validate(&bad, &ctx, "direct"),
            Err(ProposalError::IllegalTransition(
                TransitionError::Task { .. }
            ))
        ));
    }

    #[test]
    fn transition_unknown_task_rejected() {
        let ctx = MockCtx::default();
        let p = proposal(vec![Sin90Op::TransitionTask {
            task_id: "ghost".into(),
            to: TaskStatus::InProgress,
        }]);
        assert_eq!(
            validate(&p, &ctx, "direct"),
            Err(ProposalError::UnknownEntity {
                entity: "task",
                id: "ghost".into()
            })
        );
    }

    #[test]
    fn create_tasks_requires_open_week() {
        let mut ctx = MockCtx::default();
        ctx.weeks.insert("w_open".into(), WeekStatus::Planning);
        ctx.weeks.insert("w_closed".into(), WeekStatus::Closed);

        let ok = proposal(vec![Sin90Op::CreateTasks {
            week_id: "w_open".into(),
            tasks: vec![NewTask {
                title: "draft".into(),
                direction_id: None,
            }],
        }]);
        assert!(validate(&ok, &ctx, "direct").is_ok());

        let bad = proposal(vec![Sin90Op::CreateTasks {
            week_id: "w_closed".into(),
            tasks: vec![NewTask {
                title: "x".into(),
                direction_id: None,
            }],
        }]);
        assert!(matches!(
            validate(&bad, &ctx, "direct"),
            Err(ProposalError::WeekNotOpen { .. })
        ));
    }

    #[test]
    fn reorder_empty_order_rejected() {
        let mut ctx = MockCtx::default();
        ctx.weeks.insert("w".into(), WeekStatus::Active);
        let p = proposal(vec![Sin90Op::ReorderTasks {
            week_id: "w".into(),
            order: vec![],
        }]);
        assert!(matches!(
            validate(&p, &ctx, "direct"),
            Err(ProposalError::EmptyList {
                op: "reorder_tasks",
                ..
            })
        ));
    }

    #[test]
    fn reorder_duplicate_ids_rejected() {
        let mut ctx = MockCtx::default();
        ctx.weeks.insert("w".into(), WeekStatus::Active);
        let p = proposal(vec![Sin90Op::ReorderTasks {
            week_id: "w".into(),
            order: vec!["t1".into(), "t2".into(), "t1".into()],
        }]);
        assert!(matches!(
            validate(&p, &ctx, "direct"),
            Err(ProposalError::DuplicateRef {
                op: "reorder_tasks",
                ..
            })
        ));
    }

    #[test]
    fn create_tasks_empty_list_rejected() {
        let mut ctx = MockCtx::default();
        ctx.weeks.insert("w".into(), WeekStatus::Planning);
        let p = proposal(vec![Sin90Op::CreateTasks {
            week_id: "w".into(),
            tasks: vec![],
        }]);
        assert!(matches!(
            validate(&p, &ctx, "direct"),
            Err(ProposalError::EmptyList {
                op: "create_tasks",
                ..
            })
        ));
    }

    #[test]
    fn create_direction_blank_title_rejected() {
        let ctx = MockCtx::default();
        let p = proposal(vec![Sin90Op::CreateDirection {
            title: "   ".into(),
            target_window: "2026-08".into(),
        }]);
        assert_eq!(
            validate(&p, &ctx, "direct"),
            Err(ProposalError::BlankField { field: "title" })
        );
    }

    #[test]
    fn adjust_rhythm_duplicate_direction_rejected() {
        let mut ctx = MockCtx::default();
        ctx.rhythms_retired.insert("r".into(), false);
        let p = proposal(vec![Sin90Op::AdjustRhythm {
            rhythm_id: "r".into(),
            new_alloc: vec![
                Alloc {
                    direction_id: "d1".into(),
                    pct: 50,
                },
                Alloc {
                    direction_id: "d1".into(),
                    pct: 50,
                },
            ],
        }]);
        assert!(matches!(
            validate(&p, &ctx, "direct"),
            Err(ProposalError::DuplicateRef {
                op: "adjust_rhythm",
                ..
            })
        ));
    }

    // ---- intra-batch state threading (the overlay) ----

    #[test]
    fn intra_batch_sequential_transitions_pass() {
        let mut ctx = MockCtx::default();
        ctx.tasks.insert("t1".into(), TaskStatus::Planned);
        let p = proposal(vec![
            Sin90Op::TransitionTask {
                task_id: "t1".into(),
                to: TaskStatus::InProgress,
            },
            Sin90Op::TransitionTask {
                task_id: "t1".into(),
                to: TaskStatus::Done,
            },
        ]);
        assert!(validate(&p, &ctx, "direct").is_ok());
    }

    #[test]
    fn intra_batch_repeated_transition_fails() {
        let mut ctx = MockCtx::default();
        ctx.tasks.insert("t1".into(), TaskStatus::Planned);
        let p = proposal(vec![
            Sin90Op::TransitionTask {
                task_id: "t1".into(),
                to: TaskStatus::InProgress,
            },
            Sin90Op::TransitionTask {
                task_id: "t1".into(),
                to: TaskStatus::InProgress,
            },
        ]);
        assert!(matches!(
            validate(&p, &ctx, "direct"),
            Err(ProposalError::IllegalTransition(
                TransitionError::Task { .. }
            ))
        ));
    }

    #[test]
    fn intra_batch_carry_over_then_transition_fails() {
        let mut ctx = MockCtx::default();
        ctx.tasks.insert("t1".into(), TaskStatus::InProgress);
        ctx.weeks.insert("next".into(), WeekStatus::Planning);
        let p = proposal(vec![
            Sin90Op::CarryOverTask {
                task_id: "t1".into(),
                to_week: "next".into(),
            },
            Sin90Op::TransitionTask {
                task_id: "t1".into(),
                to: TaskStatus::Done,
            },
        ]);
        assert!(matches!(
            validate(&p, &ctx, "direct"),
            Err(ProposalError::IllegalTransition(
                TransitionError::Task { .. }
            ))
        ));
    }

    #[test]
    fn deny_unknown_fields_on_new_task() {
        let err = serde_json::from_str::<NewTask>(r#"{"title":"x","typo":1}"#);
        assert!(err.is_err(), "unknown field must be rejected");
    }

    /// SFU-9: `Sin90Op` is internally tagged (`tag = "op"`); serde's
    /// `deny_unknown_fields` on that enum IS honored per-variant (confirmed
    /// with a standalone repro against this repo's serde version — see the
    /// doc comment on `Sin90Op`). Table-driven so every one of the 8 variants
    /// is independently pinned: an 9th op added later that forgets a field
    /// in its legal shape here fails loudly at `cases.len()`, not silently.
    #[test]
    fn deny_unknown_fields_covers_every_sin90_op_variant() {
        let cases: Vec<(&str, serde_json::Value)> = vec![
            (
                "create_area",
                serde_json::json!({"op": "create_area", "title": "Work"}),
            ),
            (
                "create_task",
                serde_json::json!({
                    "op": "create_task",
                    "title": "x",
                    "direction_id": null,
                    "parent_task_id": null,
                    "kind": null,
                    "energy": null,
                    "est_minutes": null
                }),
            ),
            (
                "create_direction",
                serde_json::json!({
                    "op": "create_direction",
                    "title": "x",
                    "target_window": "2026-Q4"
                }),
            ),
            (
                "transition_task",
                serde_json::json!({"op": "transition_task", "task_id": "t1", "to": "in_progress"}),
            ),
            (
                "create_tasks",
                serde_json::json!({
                    "op": "create_tasks",
                    "week_id": "w1",
                    "tasks": [{"title": "x", "direction_id": null}]
                }),
            ),
            (
                "reorder_tasks",
                serde_json::json!({"op": "reorder_tasks", "week_id": "w1", "order": ["t1", "t2"]}),
            ),
            (
                "adjust_rhythm",
                serde_json::json!({
                    "op": "adjust_rhythm",
                    "rhythm_id": "r1",
                    "new_alloc": [{"direction_id": "d1", "pct": 50}]
                }),
            ),
            (
                "carry_over_task",
                serde_json::json!({"op": "carry_over_task", "task_id": "t1", "to_week": "w2"}),
            ),
        ];

        assert_eq!(cases.len(), 8, "must cover every Sin90Op variant");

        for (op_name, good) in cases {
            // Positive control: the clean shape parses as the expected op.
            let parsed: Sin90Op = serde_json::from_value(good.clone())
                .unwrap_or_else(|e| panic!("{op_name}: legal shape must parse, got {e}"));
            let round = serde_json::to_value(&parsed).unwrap();
            assert_eq!(
                round["op"], op_name,
                "{op_name}: op tag round-trip mismatch"
            );

            // The assertion: adding one unknown field must be rejected.
            let mut bad = good;
            bad.as_object_mut()
                .unwrap()
                .insert("typo_field".into(), serde_json::json!(1));
            let err = serde_json::from_value::<Sin90Op>(bad);
            assert!(
                err.is_err(),
                "{op_name}: an unknown field must be rejected, got {err:?}"
            );
        }
    }

    #[test]
    fn adjust_rhythm_retired_and_alloc_rules() {
        let mut ctx = MockCtx::default();
        ctx.rhythms_retired.insert("r_live".into(), false);
        ctx.rhythms_retired.insert("r_dead".into(), true);

        let good = proposal(vec![Sin90Op::AdjustRhythm {
            rhythm_id: "r_live".into(),
            new_alloc: vec![
                Alloc {
                    direction_id: "d1".into(),
                    pct: 60,
                },
                Alloc {
                    direction_id: "d2".into(),
                    pct: 40,
                },
            ],
        }]);
        assert!(validate(&good, &ctx, "direct").is_ok());

        // Each individual pct is within 1..=100 (so this pins the SUM check,
        // not `PctOutOfRange` below — that's a separate, deliberately
        // distinguished failure mode since Opus 2026-09-23 review H1).
        let over = proposal(vec![Sin90Op::AdjustRhythm {
            rhythm_id: "r_live".into(),
            new_alloc: vec![
                Alloc {
                    direction_id: "d1".into(),
                    pct: 60,
                },
                Alloc {
                    direction_id: "d2".into(),
                    pct: 50,
                },
            ],
        }]);
        assert_eq!(
            validate(&over, &ctx, "direct"),
            Err(ProposalError::InvalidAlloc { sum_pct: 110 })
        );

        // Retired check fires before `check_alloc` even runs (validate_op's
        // AdjustRhythm arm order) — so an empty `new_alloc` on a retired
        // rhythm still reports RhythmRetired, not EmptyAllocations.
        let dead = proposal(vec![Sin90Op::AdjustRhythm {
            rhythm_id: "r_dead".into(),
            new_alloc: vec![],
        }]);
        assert!(matches!(
            validate(&dead, &ctx, "direct"),
            Err(ProposalError::RhythmRetired { .. })
        ));
    }

    /// Opus 2026-09-23 review (H1/M1): `check_alloc` is now the ONE
    /// structural gate for both `AdjustRhythm` and direct `create_rhythm` —
    /// pin its two new checks (empty list, per-item pct range) at the pure
    /// layer, on a LIVE (non-retired) rhythm so `check_alloc` is actually
    /// reached.
    #[test]
    fn adjust_rhythm_empty_alloc_rejected() {
        let mut ctx = MockCtx::default();
        ctx.rhythms_retired.insert("r".into(), false);
        let p = proposal(vec![Sin90Op::AdjustRhythm {
            rhythm_id: "r".into(),
            new_alloc: vec![],
        }]);
        assert_eq!(
            validate(&p, &ctx, "direct"),
            Err(ProposalError::EmptyAllocations)
        );

        // Positive control: a single legal allocation passes.
        let ok = proposal(vec![Sin90Op::AdjustRhythm {
            rhythm_id: "r".into(),
            new_alloc: vec![Alloc {
                direction_id: "d1".into(),
                pct: 1,
            }],
        }]);
        assert!(validate(&ok, &ctx, "direct").is_ok());
    }

    #[test]
    fn adjust_rhythm_pct_out_of_range_rejected() {
        let mut ctx = MockCtx::default();
        ctx.rhythms_retired.insert("r".into(), false);
        for bad_pct in [0u32, 101] {
            let p = proposal(vec![Sin90Op::AdjustRhythm {
                rhythm_id: "r".into(),
                new_alloc: vec![Alloc {
                    direction_id: "d1".into(),
                    pct: bad_pct,
                }],
            }]);
            assert_eq!(
                validate(&p, &ctx, "direct"),
                Err(ProposalError::PctOutOfRange {
                    direction_id: "d1".into(),
                    pct: bad_pct,
                }),
                "pct={bad_pct}"
            );
        }
        // Positive controls: the range's own endpoints, 1 and 100, both pass.
        for good_pct in [1u32, 100] {
            let p = proposal(vec![Sin90Op::AdjustRhythm {
                rhythm_id: "r".into(),
                new_alloc: vec![Alloc {
                    direction_id: "d1".into(),
                    pct: good_pct,
                }],
            }]);
            assert!(validate(&p, &ctx, "direct").is_ok(), "pct={good_pct}");
        }
    }

    #[test]
    fn carry_over_needs_carryable_task_and_open_week() {
        let mut ctx = MockCtx::default();
        ctx.tasks.insert("t_prog".into(), TaskStatus::InProgress);
        ctx.tasks.insert("t_done".into(), TaskStatus::Done);
        ctx.weeks.insert("next".into(), WeekStatus::Planning);

        let ok = proposal(vec![Sin90Op::CarryOverTask {
            task_id: "t_prog".into(),
            to_week: "next".into(),
        }]);
        assert!(validate(&ok, &ctx, "direct").is_ok());

        let bad = proposal(vec![Sin90Op::CarryOverTask {
            task_id: "t_done".into(),
            to_week: "next".into(),
        }]);
        assert!(matches!(
            validate(&bad, &ctx, "direct"),
            Err(ProposalError::IllegalTransition(
                TransitionError::Task { .. }
            ))
        ));
    }

    #[test]
    fn first_offending_op_stops_validation() {
        let mut ctx = MockCtx::default();
        ctx.tasks.insert("t1".into(), TaskStatus::Planned);
        let p = proposal(vec![
            Sin90Op::TransitionTask {
                task_id: "t1".into(),
                to: TaskStatus::InProgress,
            },
            Sin90Op::TransitionTask {
                task_id: "ghost".into(),
                to: TaskStatus::InProgress,
            },
        ]);
        assert!(matches!(
            validate(&p, &ctx, "direct"),
            Err(ProposalError::UnknownEntity { entity: "task", .. })
        ));
    }

    #[test]
    fn op_json_tag_is_snake_case() {
        let op = Sin90Op::TransitionTask {
            task_id: "t1".into(),
            to: TaskStatus::Done,
        };
        let j = serde_json::to_string(&op).unwrap();
        assert!(j.contains("\"op\":\"transition_task\""), "{j}");
        assert!(j.contains("\"to\":\"done\""), "{j}");
    }

    // ---- AssignTaskDirection (design §11.2.1, T5.2.1) ----------------------

    fn inbox_ctx() -> MockCtx {
        let mut ctx = MockCtx::default();
        ctx.tasks.insert("t1".into(), TaskStatus::Backlog);
        ctx.task_directions.insert("t1".into(), None); // in the inbox
        ctx.directions.insert("d1".into(), DirectionStatus::Active);
        ctx
    }

    fn assign(task_id: &str, direction_id: &str) -> Sin90Op {
        Sin90Op::AssignTaskDirection {
            task_id: task_id.into(),
            direction_id: direction_id.into(),
        }
    }

    #[test]
    fn assign_task_direction_happy_path() {
        let ctx = inbox_ctx();
        let p = proposal(vec![assign("t1", "d1")]);
        assert!(validate(&p, &ctx, "direct").is_ok());
    }

    #[test]
    fn assign_task_direction_unknown_task_rejected() {
        // A1: `task_direction` has no entry at all for "ghost".
        let ctx = inbox_ctx();
        let p = proposal(vec![assign("ghost", "d1")]);
        assert_eq!(
            validate(&p, &ctx, "direct"),
            Err(ProposalError::UnknownEntity {
                entity: "task",
                id: "ghost".into()
            })
        );
    }

    #[test]
    fn assign_task_direction_closed_task_rejected() {
        // A2: task exists, is in the inbox, but its status is terminal.
        let mut ctx = inbox_ctx();
        ctx.tasks.insert("t1".into(), TaskStatus::Done);
        let p = proposal(vec![assign("t1", "d1")]);
        assert_eq!(
            validate(&p, &ctx, "direct"),
            Err(ProposalError::TaskClosed {
                task_id: "t1".into(),
                status: TaskStatus::Done
            })
        );
    }

    #[test]
    fn assign_task_direction_already_classified_rejected() {
        // A3: task exists but already has a Direction (not in the inbox).
        let mut ctx = inbox_ctx();
        ctx.task_directions
            .insert("t1".into(), Some("d-existing".into()));
        let p = proposal(vec![assign("t1", "d1")]);
        assert_eq!(
            validate(&p, &ctx, "direct"),
            Err(ProposalError::NotInInbox {
                task_id: "t1".into(),
                // 2026-09-24 review (round 2, M2): the error must name the
                // task's CURRENT (already-assigned) Direction — "d-existing"
                // — not the TARGET one ("d1") this op was trying to assign.
                direction_id: "d-existing".into()
            })
        );
    }

    #[test]
    fn assign_task_direction_unknown_direction_rejected() {
        // A4: the target Direction does not exist.
        let ctx = inbox_ctx();
        let p = proposal(vec![assign("t1", "ghost-direction")]);
        assert_eq!(
            validate(&p, &ctx, "direct"),
            Err(ProposalError::UnknownEntity {
                entity: "direction",
                id: "ghost-direction".into()
            })
        );
    }

    #[test]
    fn assign_task_direction_closed_direction_rejected() {
        // A5: the target Direction is terminal (achieved/abandoned).
        let mut ctx = inbox_ctx();
        ctx.directions
            .insert("d1".into(), DirectionStatus::Abandoned);
        let p = proposal(vec![assign("t1", "d1")]);
        assert_eq!(
            validate(&p, &ctx, "direct"),
            Err(ProposalError::DirectionClosed {
                direction_id: "d1".into(),
                status: DirectionStatus::Abandoned
            })
        );

        // Positive control: `achieved` is ALSO terminal.
        let mut ctx2 = inbox_ctx();
        ctx2.directions
            .insert("d1".into(), DirectionStatus::Achieved);
        assert!(matches!(
            validate(&proposal(vec![assign("t1", "d1")]), &ctx2, "direct"),
            Err(ProposalError::DirectionClosed { .. })
        ));

        // Positive control: every NON-terminal status is accepted.
        for s in [
            DirectionStatus::Draft,
            DirectionStatus::Active,
            DirectionStatus::Paused,
        ] {
            let mut ok_ctx = inbox_ctx();
            ok_ctx.directions.insert("d1".into(), s);
            assert!(
                validate(&proposal(vec![assign("t1", "d1")]), &ok_ctx, "direct").is_ok(),
                "{s:?} should be assignable"
            );
        }
    }

    /// Batch overlay (design §11.2.1's "同批次交互"): a second `Assign` on the
    /// SAME task in the same batch sees the first one's effect and is
    /// rejected — no DB round-trip needed to catch this at validate time.
    #[test]
    fn assign_task_direction_batch_duplicate_rejected() {
        let mut ctx = inbox_ctx();
        ctx.directions.insert("d2".into(), DirectionStatus::Active);
        let p = proposal(vec![assign("t1", "d1"), assign("t1", "d2")]);
        assert_eq!(
            validate(&p, &ctx, "direct"),
            Err(ProposalError::NotInInbox {
                task_id: "t1".into(),
                // 2026-09-24 review (round 2, M2): the SECOND op's A3 check
                // sees the batch overlay set by the FIRST op — the task is
                // now "already assigned to d1" (not "d2", the second op's
                // own target).
                direction_id: "d1".into()
            })
        );
    }

    /// `[TransitionTask(t→dropped), Assign(t,d)]`: the existing task-status
    /// overlay (used by `TransitionTask`/`CarryOverTask`) is what
    /// `AssignTaskDirection`'s A2 reads too — no separate bookkeeping needed.
    #[test]
    fn assign_task_direction_after_batch_drop_rejected() {
        let mut ctx = inbox_ctx();
        ctx.tasks.insert("t1".into(), TaskStatus::Backlog);
        let p = proposal(vec![
            Sin90Op::TransitionTask {
                task_id: "t1".into(),
                to: TaskStatus::Dropped,
            },
            assign("t1", "d1"),
        ]);
        assert_eq!(
            validate(&p, &ctx, "direct"),
            Err(ProposalError::TaskClosed {
                task_id: "t1".into(),
                status: TaskStatus::Dropped
            })
        );
    }

    /// `[Assign(t,d), TransitionTask(t→planned)]` is legal: assigning a
    /// Direction does not touch the task-status overlay, so the later
    /// transition is validated against the task's ORIGINAL status.
    #[test]
    fn assign_task_direction_then_batch_transition_ok() {
        let ctx = inbox_ctx(); // t1 starts Backlog
        let p = proposal(vec![
            assign("t1", "d1"),
            Sin90Op::TransitionTask {
                task_id: "t1".into(),
                to: TaskStatus::Planned,
            },
        ]);
        assert!(validate(&p, &ctx, "direct").is_ok());
    }

    // ---- M-a (T5.7.2 review round 2): 待定→真实 Direction, capability_source
    // 一律放行 direct -----------------------------------------------------

    /// A task currently in 待定 whose parking is NOT classify's own
    /// (`task_triage_via_classify` reports `false` — a human filed it there
    /// directly) can still be moved to a real Direction by the SAME kind of
    /// caller: `capability_source == "direct"` bypasses the
    /// `task_triage_via_classify` check entirely (M-a: the user must not be
    /// locked out of their own placement). Mutation target: drop the
    /// `capability_source == "direct" ||` leg from `reclassifiable_from_
    /// triage` and this goes red (`NotInInbox` instead of `Ok`).
    #[test]
    fn assign_task_direction_direct_source_moves_own_triage_task_out() {
        let mut ctx = inbox_ctx();
        ctx.task_directions
            .insert("t1".into(), Some(TRIAGE_DIRECTION_ID.into()));
        // Deliberately NOT in `ctx.triage_via_classify` — a direct/human
        // placement, exactly the case M6 alone would still refuse.
        let p = proposal(vec![assign("t1", "d1")]);
        assert!(
            validate(&p, &ctx, "direct").is_ok(),
            "a direct-sourced proposal must be able to move the user's OWN 待定 task out"
        );
    }

    /// Negative control for the test above: the identical fixture (task in
    /// 待定, NOT classify-parked) but `capability_source == "classify"` —
    /// an AI run must still be refused, unchanged from M6's original scope.
    /// Mutation target: same leg as above, but this positive-error
    /// assertion is what catches an overly BROAD fix (e.g. dropping the
    /// `task_triage_via_classify` check entirely instead of gating it on
    /// `capability_source`).
    #[test]
    fn assign_task_direction_classify_source_still_blocked_by_human_triage() {
        let mut ctx = inbox_ctx();
        ctx.task_directions
            .insert("t1".into(), Some(TRIAGE_DIRECTION_ID.into()));
        let p = proposal(vec![assign("t1", "d1")]);
        assert_eq!(
            validate(&p, &ctx, "classify"),
            Err(ProposalError::NotInInbox {
                task_id: "t1".into(),
                direction_id: TRIAGE_DIRECTION_ID.into(),
            })
        );
    }

    /// Positive control: `capability_source == "classify"` CAN still move a
    /// task out of 待定 when `task_triage_via_classify` says classify itself
    /// put it there (M6's original carve-out, unaffected by M-a).
    #[test]
    fn assign_task_direction_classify_source_moves_its_own_triage_task_out() {
        let mut ctx = inbox_ctx();
        ctx.task_directions
            .insert("t1".into(), Some(TRIAGE_DIRECTION_ID.into()));
        ctx.triage_via_classify.insert("t1".into());
        let p = proposal(vec![assign("t1", "d1")]);
        assert!(validate(&p, &ctx, "classify").is_ok());
    }

    // NOTE (2026-09-24 review, M6): a stray field on `AssignTaskDirection`
    // (and every other `Sin90Op` variant) currently parses successfully and
    // is silently dropped — `Sin90Op` has no enum-level `deny_unknown_fields`
    // yet, and that attribute cannot be applied to a single variant on its
    // own (serde rejects it as an "unknown serde variant attribute" — see
    // this file's `AssignTaskDirection` doc). This is a known, TEMPORARY gap
    // pending SFU-9 (#29)'s independent PR, not a behavior worth pinning as
    // an expected-and-tested outcome: a test asserting "unknown field is
    // ignored" would nail the defect down as a spec instead of just noting
    // it, and would need to be manually flipped (not just left to fail) the
    // day SFU-9 lands. So: no test here on purpose.

    #[test]
    fn assign_task_direction_json_tag_is_snake_case() {
        let j = serde_json::to_string(&assign("t1", "d1")).unwrap();
        assert!(j.contains("\"op\":\"assign_task_direction\""), "{j}");
        assert!(j.contains("\"task_id\":\"t1\""), "{j}");
        assert!(j.contains("\"direction_id\":\"d1\""), "{j}");
    }

    // ---- DraftReviewBody (design §11.2.2, T5.2.1's "一次加齐") -------------

    fn draft_ctx() -> (MockCtx, String) {
        let mut ctx = MockCtx::default();
        let body = "line one\nline two";
        let hash = body_sha256(body);
        ctx.reviews.insert(
            "r1".into(),
            ReviewSnap {
                status: ReviewStatus::Draft,
                body_sha256: hash.clone(),
            },
        );
        (ctx, hash)
    }

    fn draft(review_id: &str, base: &str, body: &str) -> Sin90Op {
        Sin90Op::DraftReviewBody {
            review_id: review_id.into(),
            base_body_sha256: base.into(),
            body: body.into(),
        }
    }

    #[test]
    fn draft_review_body_happy_path() {
        let (ctx, hash) = draft_ctx();
        let p = proposal(vec![draft("r1", &hash, "a whole new body")]);
        assert!(validate(&p, &ctx, "direct").is_ok());
    }

    #[test]
    fn draft_review_body_d1_blank_body_rejected() {
        let (ctx, hash) = draft_ctx();
        let p = proposal(vec![draft("r1", &hash, "   ")]);
        assert_eq!(
            validate(&p, &ctx, "direct"),
            Err(ProposalError::BlankField { field: "body" })
        );
    }

    #[test]
    fn draft_review_body_d2_over_size_limit_rejected_at_limit_ok() {
        let (ctx, hash) = draft_ctx();
        // Positive control: EXACTLY at the limit is fine.
        let at_limit = "x".repeat(MAX_REVIEW_BODY_BYTES);
        let ok = proposal(vec![draft("r1", &hash, &at_limit)]);
        assert!(validate(&ok, &ctx, "direct").is_ok());

        let over = "x".repeat(MAX_REVIEW_BODY_BYTES + 1);
        let bad = proposal(vec![draft("r1", &hash, &over)]);
        assert_eq!(
            validate(&bad, &ctx, "direct"),
            Err(ProposalError::TooLarge {
                field: "body",
                max_bytes: MAX_REVIEW_BODY_BYTES
            })
        );
    }

    #[test]
    fn draft_review_body_d3_malformed_hash_rejected() {
        let (ctx, hash) = draft_ctx();
        for bad_hash in [
            "not-hex-at-all",
            &hash[..63],          // one char short
            &format!("{hash}0"),  // one char long
            &hash.to_uppercase(), // must be LOWERCASE hex
        ] {
            let p = proposal(vec![draft("r1", bad_hash, "new body")]);
            assert_eq!(
                validate(&p, &ctx, "direct"),
                Err(ProposalError::BadHash),
                "{bad_hash}"
            );
        }
    }

    #[test]
    fn draft_review_body_d4_unknown_review_rejected() {
        let (ctx, hash) = draft_ctx();
        let p = proposal(vec![draft("ghost", &hash, "new body")]);
        assert_eq!(
            validate(&p, &ctx, "direct"),
            Err(ProposalError::UnknownEntity {
                entity: "review",
                id: "ghost".into()
            })
        );
    }

    #[test]
    fn draft_review_body_d5_finalized_review_rejected() {
        let (mut ctx, hash) = draft_ctx();
        ctx.reviews.get_mut("r1").unwrap().status = ReviewStatus::Finalized;
        let p = proposal(vec![draft("r1", &hash, "new body")]);
        assert_eq!(
            validate(&p, &ctx, "direct"),
            Err(ProposalError::ReviewNotDraft {
                review_id: "r1".into()
            })
        );
    }

    #[test]
    fn draft_review_body_d6_stale_base_rejected() {
        let (ctx, _hash) = draft_ctx();
        let p = proposal(vec![draft(
            "r1",
            &body_sha256("a different base"),
            "new body",
        )]);
        assert_eq!(
            validate(&p, &ctx, "direct"),
            Err(ProposalError::StaleBase {
                entity: "review",
                id: "r1".into()
            })
        );
    }

    #[test]
    fn draft_review_body_d7_no_change_rejected() {
        let (ctx, hash) = draft_ctx();
        // Same body the snapshot already has (see `draft_ctx`).
        let p = proposal(vec![draft("r1", &hash, "line one\nline two")]);
        assert_eq!(
            validate(&p, &ctx, "direct"),
            Err(ProposalError::NoChange {
                op: "draft_review_body"
            })
        );
    }

    /// §11.2.2's CAS chain: two `DraftReviewBody` ops on the SAME review in
    /// one batch — the second must use the FIRST's new digest as its base,
    /// not the pre-batch one. Both directions pinned (scratch's
    /// `draft_body_cas_chain_and_rejections`).
    #[test]
    fn draft_review_body_batch_chains_and_rejects_stale_second() {
        let (ctx, hash) = draft_ctx();
        let first_body = "first rewrite";
        let chained = proposal(vec![
            draft("r1", &hash, first_body),
            draft("r1", &body_sha256(first_body), "second rewrite"),
        ]);
        assert!(validate(&chained, &ctx, "direct").is_ok());

        // The second op still uses the ORIGINAL (pre-batch) hash — stale
        // the moment the first op in the SAME batch already moved it.
        let stale_second = proposal(vec![
            draft("r1", &hash, first_body),
            draft("r1", &hash, "second rewrite"),
        ]);
        assert_eq!(
            validate(&stale_second, &ctx, "direct"),
            Err(ProposalError::StaleBase {
                entity: "review",
                id: "r1".into()
            })
        );
    }

    #[test]
    fn draft_review_body_json_tag_is_snake_case() {
        let j = serde_json::to_string(&draft("r1", "abc123", "hello")).unwrap();
        assert!(j.contains("\"op\":\"draft_review_body\""), "{j}");
        assert!(j.contains("\"review_id\":\"r1\""), "{j}");
        assert!(j.contains("\"base_body_sha256\":\"abc123\""), "{j}");
        assert!(j.contains("\"body\":\"hello\""), "{j}");
    }

    #[test]
    fn body_sha256_is_lowercase_hex_and_stable() {
        let h = body_sha256("hello world");
        assert_eq!(h.len(), 64);
        assert!(h
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert_eq!(h, body_sha256("hello world")); // deterministic
        assert_ne!(h, body_sha256("hello World")); // sensitive to content
    }
}
