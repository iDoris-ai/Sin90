//! T5.1.1 (design §11.5) — `Sin90Store`'s implementation of the `ai` module's
//! entire I/O surface: [`AiReader`] (a read-only pool implementing
//! `ai::AiReadModel`) and `impl ai::AiSink for Sin90Store` (the dry-run
//! submit path, §11.4 公共's H3 mechanism).
//!
//! This file, not `src/ai/**`, is where `Sin90Store`/`sqlx`/raw SQL are
//! allowed to appear — the boundary check (`tests/ai_boundary.rs`, J7) only
//! walks `src/ai/**/*.rs`.
//!
//! **Capability → allowed ops.** The design's `allowed_ops(cap)` (§11.4 公共)
//! is `Classify ⇒ {AssignTaskDirection}`, `Summarize ⇒ {DraftReviewBody}`,
//! `Propose ⇒ {CarryOverTask, ReorderTasks, CreateTasks}`. `AssignTaskDirection`
//! landed in T5.2.1; `Sin90Op::DraftReviewBody`'s type/validate/apply ALSO
//! landed then (design §11.2.3/§11.8's "一次加齐" — added alongside
//! `AssignTaskDirection` so `ValidationCtx` is widened only once), but
//! `allowed_ops(Summarize)` stayed an empty set until the `summarize`
//! CAPABILITY that actually PRODUCES it existed. T5.3.1 (`ai::summarize`)
//! is that capability — `allowed_ops` now answers the real one-op set for
//! `Summarize` too.

use sqlx::{Acquire, Row, SqlitePool};

use crate::ai::ports::{
    AiCallRecord, AiReadModel, AiSettings, AiSink, Capability, DirectionCandidate, Engine,
    ProposalDraft, ReadError, SettingsRead, SinkError, SummarizeBucket, SummarizeDraft,
    SummarizeRoutineRow,
};
use crate::ai::source_for;
use crate::core::{
    direction_is_terminal, task_is_terminal, validate, week_is_open, Alloc, DirectionId,
    DirectionStatus, ProposalStatus, Review, Sin90Op, Sin90Proposal, Task, TaskStatus, Week,
    WeekId, WeekStatus,
};
use crate::store::repo::{
    append_event, apply_op, build_snapshot, from_wire, row_to_review, row_to_task, to_wire,
    REVIEW_COLUMNS,
};
use crate::store::{Result as StoreResult, Sin90Store, StoreError};

/// `sin90_settings` key for the executive on/off switch (§11.3.2).
const EXECUTIVE_ENABLED_KEY: &str = "ai.executive_enabled";

/// The columns every `sin90_tasks` read in this file selects — same list
/// `row_to_task` expects, kept as one constant so the inbox/week-tasks
/// queries below can't drift apart from each other.
const TASK_COLUMNS: &str = "id, direction_id, week_id, parent_task_id, title, status, kind, \
     energy, est_minutes, carried_from, created_at, updated_at";

/// Every `TaskStatus` variant — kept as an explicit list (Rust has no enum
/// reflection) purely so [`terminal_task_status_wires`] can DERIVE its
/// exclusion set from [`task_is_terminal`] instead of maintaining a second,
/// driftable copy of "which statuses are terminal" as a hand-written SQL
/// literal (2026-09-24 review, H1). Mutation target: add a new terminal
/// `TaskStatus` variant to `core::types` without adding it here too — this
/// list stops being exhaustive and `inbox`'s exclusion set silently misses
/// it again. 2026-09-24 review (round 3, L5): that "silently misses it"
/// framing undersold the actual guarantee — nothing here forces this ARRAY
/// LITERAL to grow when the enum does. The test `all_task_statuses_array_is_
/// exhaustive` (below) is what turns "silently stale" into "fails to
/// compile": it wraps every one of these variants in a wildcard-free
/// `match`, so a new `TaskStatus` variant breaks that match (E0004) until
/// it's added both there AND here.
const ALL_TASK_STATUSES: [TaskStatus; 6] = [
    TaskStatus::Backlog,
    TaskStatus::Planned,
    TaskStatus::InProgress,
    TaskStatus::Done,
    TaskStatus::Dropped,
    TaskStatus::CarriedOver,
];

/// The wire values of every TERMINAL `TaskStatus`, computed by asking
/// [`task_is_terminal`] about each known status — see [`ALL_TASK_STATUSES`]'s
/// doc for why this indirection exists.
fn terminal_task_status_wires() -> Vec<String> {
    ALL_TASK_STATUSES
        .iter()
        .copied()
        .filter(|&s| task_is_terminal(s))
        .map(|s| to_wire(&s).expect("TaskStatus always serializes"))
        .collect()
}

/// Every `DirectionStatus` variant — same reflection-substitute as
/// [`ALL_TASK_STATUSES`], for [`terminal_direction_status_wires`]
/// (2026-09-24 review round 2, #4).
const ALL_DIRECTION_STATUSES: [DirectionStatus; 5] = [
    DirectionStatus::Draft,
    DirectionStatus::Active,
    DirectionStatus::Paused,
    DirectionStatus::Achieved,
    DirectionStatus::Abandoned,
];

/// The wire values of every TERMINAL `DirectionStatus`, derived from
/// [`direction_is_terminal`] — `direction_candidates` and `title_history`
/// both exclude terminal Directions and must agree on what "terminal" means;
/// deriving it here (instead of two independent `NOT IN ('achieved',
/// 'abandoned')` literals) means a new terminal status added to
/// `core::transitions::direction_is_terminal` without updating
/// [`ALL_DIRECTION_STATUSES`] fails loudly (this list stops being
/// exhaustive) rather than silently leaving one of the two queries stale.
/// 2026-09-24 review (round 3, L5): same caveat as `ALL_TASK_STATUSES`'s —
/// the compile-time enforcement is `all_direction_statuses_array_is_
/// exhaustive` (below), not the array literal by itself.
fn terminal_direction_status_wires() -> Vec<String> {
    ALL_DIRECTION_STATUSES
        .iter()
        .copied()
        .filter(|&s| direction_is_terminal(s))
        .map(|s| to_wire(&s).expect("DirectionStatus always serializes"))
        .collect()
}

/// A read-only handle over `sin90.db` (§11.5 v2.1 M1): every connection in
/// its pool was opened with `pragma("query_only", "ON")` — see
/// `Sin90Store::ai_reader`'s doc for how the pool itself is built. `ai/`
/// never sees this type, only the `AiReadModel`/`SettingsRead` traits it
/// implements.
#[derive(Clone)]
pub struct AiReader(SqlitePool);

impl AiReader {
    pub(crate) fn new(pool: SqlitePool) -> Self {
        Self(pool)
    }

    #[cfg(test)]
    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.0
    }

    /// T5.7.2 (design §2 #31): the "新 Direction" leg of classify's
    /// rejection-suppression `situation changed` check — the newest
    /// `created_at` among Directions that are BOTH non-terminal and not the
    /// reserved 待定 id, i.e. the exact same eligibility set
    /// `direction_candidates`/`title_history` already filter on. `None` when
    /// no such Direction exists at all (a brand-new user, or every one is
    /// terminal/待定) — the caller then has nothing to compare a `proposed_at`
    /// against, i.e. this leg never fires. Not part of `AiReadModel` (`ai/`
    /// never needs it — the suppression check itself lives at the HTTP layer,
    /// same posture `ai_classify::dedup_targets`'s own `Sin90Store::
    /// list_pending_proposals` dependency already has, §11.5).
    pub async fn max_eligible_direction_created_at(&self) -> Result<Option<String>, ReadError> {
        let terminal = terminal_direction_status_wires();
        let placeholders = terminal.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT MAX(created_at) AS m FROM sin90_directions
             WHERE status NOT IN ({placeholders}) AND id != ?"
        );
        let mut q = sqlx::query(&sql);
        for t in &terminal {
            q = q.bind(t);
        }
        q = q.bind(crate::core::TRIAGE_DIRECTION_ID);
        let row = q.fetch_one(&self.0).await.map_err(rerr)?;
        Ok(row.get::<Option<String>, _>("m"))
    }
}

fn rerr(e: impl std::fmt::Display) -> ReadError {
    ReadError(e.to_string())
}

impl SettingsRead for AiReader {
    async fn settings(&self) -> Result<AiSettings, ReadError> {
        read_ai_settings(&self.0).await
    }
}

/// T5.7.2 review round 2 (H2/M6), N-H1/Low follow-up: the 待定 retry gate's
/// SQL, shared verbatim by [`AiReadModel::inbox`]/[`AiReadModel::inbox_task`]
/// below — assumes the enclosing query aliases `sin90_tasks` as `t`.
/// Replaces the ORIGINAL gate (`updated_at < newest eligible Direction's
/// created_at`), which had two bugs the review round found:
///
/// - **H2 (no exit condition)**: `updated_at` never moves for a triage task
///   classify re-examines and STILL cannot place (`none`/low-confidence/
///   no-conclusion — none of those write anything back), so once some
///   Direction is newer than this task's `updated_at`, the task re-qualifies
///   as a target on EVERY run forever, for as long as that Direction stays
///   the newest one — permanently occupying one of `MAX_CLASSIFY_TASK_IDS`
///   slots. Fixed by comparing against `evaluated_at`/`entered_at` instead:
///   `evaluated_at` (`sin90_classify_evals`, written by `AiSink::
///   record_classify_eval` every time classify looks at a triage task and
///   does not move it out) advances past a fruitless look, so the task
///   drops out of contention again until a Direction newer STILL shows up;
///   `entered_at` (`sin90_tasks.triage_entered_at`, N-H1) is the floor before
///   any evaluation has ever been recorded.
/// - **M6 (待定→真实 Direction only for classify's own placements)**: `t.
///   triage_via = 'classify'` requires this task's 待定 parking to trace
///   back to an ACCEPTED `capability_source = "classify"` proposal — the
///   SAME check `core::proposal::validate`'s A3 carve-out now requires
///   (`ValidationCtx::task_triage_via_classify`, `store/repo.rs`'s
///   `load_task_triage_via_classify`). A task a human filed into 待定
///   directly (`POST /proposals`, `capability_source = "direct"`) must
///   never be offered to classify's retry inbox at all — both mechanisms
///   share this one clause so they cannot drift apart.
///
/// N-H1 (T5.7.2 review round 2 follow-up): `triage_via`/`triage_entered_at`
/// are plain `sin90_tasks` columns now (migration 0015), stamped by
/// `AssignTaskDirection`'s apply and copied forward by `CarryOverTask`'s —
/// this replaces both the `EXISTS (... sin90_ai_calls JOIN sin90_proposals
/// JOIN json_each(p.ops) ...)` M6 used and the `sin90_events` lookup H2 used
/// for "entered 待定 at", NEITHER of which survived a task's id changing
/// under `CarryOverTask` (both were keyed on `t.id`/`entity_id = t.id`, the
/// CURRENT id — a carried-over 待定 task has a brand-new one, so both
/// derivations silently found nothing for it; this migration's own
/// motivating bug). Also fixes the JOIN's own per-read cost (M-d).
///
/// Low (T5.7.2 review round 2 follow-up): the comparand is `MAX(evaluated_at,
/// entered_at)`, not `COALESCE(evaluated_at, entered_at)` — SQLite's
/// multi-argument `max()` NULL-poisons if either side is NULL, hence the
/// `COALESCE(..., '')` around each leg first; both are still ISO-8601
/// strings, so `''` sorts before any real timestamp exactly like the
/// existing `COALESCE(..., '')` on the Direction side already relies on.
fn triage_retry_gate_sql(d_placeholders: &str) -> String {
    format!(
        "t.direction_id = ?
         AND t.triage_via = 'classify'
         AND COALESCE(
               (SELECT MAX(created_at) FROM sin90_directions
                WHERE status NOT IN ({d_placeholders}) AND id != ?),
               ''
             ) > MAX(
               COALESCE((SELECT evaluated_at FROM sin90_classify_evals WHERE task_id = t.id), ''),
               COALESCE(t.triage_entered_at, '')
             )"
    )
}

impl AiReadModel for AiReader {
    /// 2026-09-24 review (H1, blocking): the exclusion list is DERIVED from
    /// [`task_is_terminal`] (via [`terminal_task_status_wires`]), not a
    /// hand-maintained SQL literal — the ORIGINAL `NOT IN ('done', 'dropped')`
    /// forgot `carried_over`, so an already-closed, still-unclassified
    /// carried-over task (its ORIGINAL row: status flips to `carried_over`
    /// but `direction_id` is untouched by `CarryOverTask`'s apply) would sort
    /// into the inbox by `created_at ASC` FOREVER — permanently occupying a
    /// slot ahead of every real candidate once 20+ such rows accumulate,
    /// since nothing ever reclassifies a closed task. See
    /// `inbox_excludes_carried_over_tasks` for the regression.
    ///
    /// T5.7.2 (design §2 #31, T5.2.2 followup ②): ALSO includes a task
    /// parked in the reserved 待定 Direction (`TRIAGE_DIRECTION_ID`), but
    /// ONLY while it satisfies [`triage_retry_gate_sql`]'s retry gate (L4,
    /// T5.7.2 review round 3: this doc used to describe that gate's
    /// ORIGINAL, since-replaced shape — plain `updated_at` strictly before
    /// the newest eligible Direction's `created_at` — which is stale now
    /// that H2/M6/N-H1 rewrote it; see `triage_retry_gate_sql`'s own doc for
    /// the CURRENT shape: `triage_via = 'classify'` (M6) plus `MAX(evaluated_
    /// at, triage_entered_at)` (H2/N-H1) as the floor, not `updated_at`).
    /// Written directly into this query's `WHERE` (a scalar subquery sharing
    /// the SAME "non-terminal + exclude triage itself" set `direction_
    /// candidates` below already filters on) rather than selecting every
    /// triage task unconditionally and filtering in Rust afterward — the
    /// gate IS the membership test, not a second pass over it.
    async fn inbox(&self, limit: u32) -> Result<Vec<Task>, ReadError> {
        let terminal = terminal_task_status_wires();
        let d_terminal = terminal_direction_status_wires();
        let t_placeholders = terminal.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let d_placeholders = d_terminal
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(", ");
        let gate = triage_retry_gate_sql(&d_placeholders);
        let sql = format!(
            "SELECT {TASK_COLUMNS} FROM sin90_tasks t
             WHERE (
                 t.direction_id IS NULL
                 OR ({gate})
             )
             AND t.status NOT IN ({t_placeholders})
             ORDER BY t.created_at ASC
             LIMIT ?"
        );
        let mut q = sqlx::query(&sql).bind(crate::core::TRIAGE_DIRECTION_ID); // gate: t.direction_id = ?
        for t in &d_terminal {
            q = q.bind(t);
        }
        q = q.bind(crate::core::TRIAGE_DIRECTION_ID); // gate: id != ? (exclude triage itself)
        for t in &terminal {
            q = q.bind(t);
        }
        q = q.bind(limit);
        let rows = q.fetch_all(&self.0).await.map_err(rerr)?;
        rows.into_iter()
            .map(row_to_task)
            .collect::<StoreResult<_>>()
            .map_err(rerr)
    }

    /// New (2026-09-24 review, L4): a point lookup — "is task `id` CURRENTLY
    /// in the inbox?" — for `ai::classify::select_targets` to validate
    /// explicitly-given `task_ids` against, instead of paging through the
    /// entire inbox with an arbitrary large `limit` (T5.2.1's original
    /// `inbox(10_000)` placeholder). Shares the SAME terminal-status
    /// exclusion [`inbox`] uses — including [`inbox`]'s own T5.7.2 待定
    /// retry gate (§2 #31), so an explicit `task_ids` request and the
    /// auto-selected path agree on membership.
    async fn inbox_task(&self, id: &str) -> Result<Option<Task>, ReadError> {
        let terminal = terminal_task_status_wires();
        let d_terminal = terminal_direction_status_wires();
        let t_placeholders = terminal.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let d_placeholders = d_terminal
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(", ");
        let gate = triage_retry_gate_sql(&d_placeholders);
        let sql = format!(
            "SELECT {TASK_COLUMNS} FROM sin90_tasks t
             WHERE t.id = ?
             AND (
                 t.direction_id IS NULL
                 OR ({gate})
             )
             AND t.status NOT IN ({t_placeholders})"
        );
        let mut q = sqlx::query(&sql)
            .bind(id)
            .bind(crate::core::TRIAGE_DIRECTION_ID); // gate: t.direction_id = ?
        for t in &d_terminal {
            q = q.bind(t);
        }
        q = q.bind(crate::core::TRIAGE_DIRECTION_ID); // gate: id != ? (exclude triage itself)
        for t in &terminal {
            q = q.bind(t);
        }
        let row = q.fetch_optional(&self.0).await.map_err(rerr)?;
        row.map(row_to_task).transpose().map_err(rerr)
    }

    /// 2026-09-24 review (round 2, #4): the exclusion list is DERIVED from
    /// [`direction_is_terminal`] (via [`terminal_direction_status_wires`]),
    /// the same convention [`AiReadModel::inbox`]'s H1 fix established for
    /// task statuses — not a hand-maintained `NOT IN ('achieved',
    /// 'abandoned')` literal duplicated independently in this function and
    /// in `title_history` below.
    ///
    /// T5.2.2 (design §2 #30): ALSO excludes [`TRIAGE_DIRECTION_ID`] — the
    /// reserved "待定" Direction is only ever reached through classify's own
    /// fallback (`ai::classify`'s dedicated arm, not R1/R2/the model), never
    /// offered up as a real candidate for R1's history match, R2's title
    /// overlap, or the model's choice enum. Without this exclusion it would
    /// be a completely normal (non-terminal) row and would silently start
    /// showing up as a candidate the moment it exists.
    async fn direction_candidates(&self, limit: u32) -> Result<Vec<DirectionCandidate>, ReadError> {
        let terminal = terminal_direction_status_wires();
        let placeholders = terminal.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT d.id AS id, d.title AS title, d.status AS status, a.title AS area_title
             FROM sin90_directions d
             LEFT JOIN sin90_areas a ON a.id = d.area_id
             WHERE d.status NOT IN ({placeholders}) AND d.id != ?
             ORDER BY d.updated_at DESC
             LIMIT ?"
        );
        let mut q = sqlx::query(&sql);
        for t in &terminal {
            q = q.bind(t);
        }
        q = q.bind(crate::core::TRIAGE_DIRECTION_ID);
        q = q.bind(limit);
        let rows = q.fetch_all(&self.0).await.map_err(rerr)?;
        rows.into_iter()
            .map(|r| {
                Ok(DirectionCandidate {
                    direction_id: r.get::<String, _>("id"),
                    title: r.get("title"),
                    status: from_wire::<DirectionStatus>(&r.get::<String, _>("status"))
                        .map_err(rerr)?,
                    area_title: r.get("area_title"),
                })
            })
            .collect()
    }

    /// New (T5.4.1, 2026-09-24 review L1): a precise, single-id lookup — see
    /// the trait method's doc for why this exists ALONGSIDE
    /// `direction_candidates` rather than reusing it with `limit`
    /// artificially raised. Deliberately does NOT filter by status (unlike
    /// `direction_candidates`'s terminal exclusion) — the caller decides.
    async fn direction(&self, id: &DirectionId) -> Result<Option<DirectionCandidate>, ReadError> {
        let row = sqlx::query(
            "SELECT d.id AS id, d.title AS title, d.status AS status, a.title AS area_title
             FROM sin90_directions d
             LEFT JOIN sin90_areas a ON a.id = d.area_id
             WHERE d.id = ?",
        )
        .bind(id)
        .fetch_optional(&self.0)
        .await
        .map_err(rerr)?;
        row.map(|r| {
            Ok(DirectionCandidate {
                direction_id: r.get::<String, _>("id"),
                title: r.get("title"),
                status: from_wire::<DirectionStatus>(&r.get::<String, _>("status"))
                    .map_err(rerr)?,
                area_title: r.get("area_title"),
            })
        })
        .transpose()
    }

    /// R1's history lookup (§11.4.1, T5.2.1b): every ALREADY-classified task
    /// whose Direction is still non-terminal, normalized through the real
    /// `normalize_title` (`crate::ai::classify`, not a local approximation —
    /// T5.2.1a's placeholder here was `coarse_normalize`, now removed) and
    /// compared against `normalized` (the caller normalizes the SAME way, so
    /// the comparison is exact). The non-terminal-Direction filter (the
    /// `JOIN` + `status NOT IN (...)`, exclusion set shared with
    /// `direction_candidates` above via [`terminal_direction_status_wires`])
    /// landed in T5.2.1a already, independent of which normalization
    /// algorithm does the string comparison. See
    /// `r1_history_ignores_abandoned_direction` (T5.2.1a) for the regression
    /// this filter guards against.
    ///
    /// H2 (coordinator review, 2026-09-26 round 2): ALSO excludes
    /// [`TRIAGE_DIRECTION_ID`] — a task fallen back into 待定 (T5.2.2) is
    /// "already classified" in the literal `direction_id IS NOT NULL` sense,
    /// but R1 must never treat that as evidence for "route the next
    /// same-titled task to 待定 too". Without this exclusion, once one task
    /// gets the 待定 fallback, EVERY future same-titled task would be routed
    /// straight to 待定 by R1 (decisive, no model call) even after the user
    /// creates the perfect real Direction for it — R1 would never even ask.
    async fn title_history(&self, normalized: &str) -> Result<Vec<DirectionId>, ReadError> {
        let terminal = terminal_direction_status_wires();
        let placeholders = terminal.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT t.title AS title, t.direction_id AS direction_id
             FROM sin90_tasks t
             JOIN sin90_directions d ON d.id = t.direction_id
             WHERE t.direction_id IS NOT NULL
               AND d.status NOT IN ({placeholders})
               AND d.id != ?"
        );
        let mut q = sqlx::query(&sql);
        for t in &terminal {
            q = q.bind(t);
        }
        q = q.bind(crate::core::TRIAGE_DIRECTION_ID);
        let rows = q.fetch_all(&self.0).await.map_err(rerr)?;
        let mut out = Vec::new();
        for r in rows {
            let title: String = r.get("title");
            if crate::ai::classify::normalize_title(&title) == normalized {
                out.push(r.get::<String, _>("direction_id"));
            }
        }
        Ok(out)
    }

    async fn review(&self, id: &str) -> Result<Option<Review>, ReadError> {
        let row = sqlx::query(&format!(
            "SELECT {REVIEW_COLUMNS} FROM sin90_reviews WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(&self.0)
        .await
        .map_err(rerr)?;
        row.map(row_to_review).transpose().map_err(rerr)
    }

    async fn week_tasks(&self, week_id: &WeekId) -> Result<Vec<Task>, ReadError> {
        let rows = sqlx::query(&format!(
            "SELECT {TASK_COLUMNS} FROM sin90_tasks
             WHERE week_id = ?
             ORDER BY sort_key ASC, created_at ASC"
        ))
        .bind(week_id)
        .fetch_all(&self.0)
        .await
        .map_err(rerr)?;
        rows.into_iter()
            .map(row_to_task)
            .collect::<StoreResult<_>>()
            .map_err(rerr)
    }

    /// T5.4.1 (§11.4.3's "输入"): existence + status/`iso_week` of a Week.
    async fn week(&self, id: &WeekId) -> Result<Option<Week>, ReadError> {
        let row = sqlx::query(
            "SELECT id, status, iso_week, created_at, updated_at FROM sin90_weeks WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.0)
        .await
        .map_err(rerr)?;
        row.map(row_to_week).transpose().map_err(rerr)
    }

    /// T5.4.1's "P" (design §11.4.3, 2026-09-24 review H1): the SINGLE
    /// NEAREST week (by `iso_week`, regardless of status) strictly before
    /// `iso_week` — NOT "the most recent OPEN week, skipping over closed
    /// ones to find an older open one". If that nearest week is not open
    /// (`week_is_open`), there is NO `P` at all (§11.4.3: "P 不存在或已关就
    /// 没有顺延建议") — this function does not look further back for a
    /// still-open week beyond it. `canonical_iso_week`'s fixed-width
    /// `YYYY-Www` output (migration 0001/§11.6) makes a plain
    /// `ORDER BY iso_week DESC` correct for "nearest" — lexicographic and
    /// chronological order coincide.
    async fn previous_open_week(&self, iso_week: &str) -> Result<Option<Week>, ReadError> {
        let row = sqlx::query(
            "SELECT id, status, iso_week, created_at, updated_at FROM sin90_weeks
             WHERE iso_week < ?
             ORDER BY iso_week DESC
             LIMIT 1",
        )
        .bind(iso_week)
        .fetch_optional(&self.0)
        .await
        .map_err(rerr)?;
        let nearest = row.map(row_to_week).transpose().map_err(rerr)?;
        Ok(nearest.filter(|w| week_is_open(w.status)))
    }

    /// T5.4.1: the most recently CREATED non-retired `sin90_rhythms` row's
    /// `allocations` — empty if none exists. There is no production "list
    /// rhythms ordered by recency" query yet (T3.4.1 hasn't landed a create
    /// route either, see `store::test_hooks::insert_rhythm`'s doc), so this
    /// is a fresh, minimal query rather than a reuse of an existing one.
    async fn rhythm_alloc(&self) -> Result<Vec<Alloc>, ReadError> {
        let row = sqlx::query(
            "SELECT allocations FROM sin90_rhythms
             WHERE status <> 'retired'
             ORDER BY created_at DESC, rowid DESC
             LIMIT 1",
        )
        .fetch_optional(&self.0)
        .await
        .map_err(rerr)?;
        match row {
            None => Ok(Vec::new()),
            Some(r) => {
                let raw: String = r.get("allocations");
                serde_json::from_str(&raw).map_err(rerr)
            }
        }
    }

    /// T5.3.1 (§11.4.2's "数字来源", 2026-09-26 review C1/H1/M2): the numbers
    /// come from `store::weekly_draft::weekly_draft_on` — the SAME function
    /// `Sin90Store::weekly_draft` calls (one source of truth). Titles and
    /// `auto_draft_md` (T4.3.2's own `render_weekly_draft_markdown`, see
    /// [`SummarizeDraft::auto_draft_md`]'s doc) are then derived from that
    /// SAME `WeeklyDraft` value. Every read here — `weekly_draft_on`'s own
    /// queries AND every `resolve_label` lookup that follows — runs inside
    /// ONE explicit transaction (`conn.begin()`), not just "the same
    /// connection": a bare connection still starts a fresh implicit
    /// read-transaction PER STATEMENT in SQLite, so a concurrent writer
    /// could in principle rename a Direction between the numbers query and
    /// that Direction's own label lookup a few statements later, handing
    /// back a `SummarizeDraft` that never existed as a single consistent
    /// snapshot. `BEGIN` (SQLite's default deferred mode) pins one snapshot
    /// for the whole call; this reader's pool is `query_only` regardless
    /// (§11.5 v2.1 M1), so there is nothing to commit — the transaction is
    /// always rolled back at the end, same "this is a read" posture
    /// `AiSink::precheck`'s own final `tx.rollback()` already has.
    async fn weekly_draft(&self, iso_week: &str) -> Result<SummarizeDraft, ReadError> {
        let mut conn = self.0.acquire().await.map_err(rerr)?;
        let mut tx = conn.begin().await.map_err(rerr)?;
        let draft = crate::store::weekly_draft::weekly_draft_on(&mut tx, iso_week)
            .await
            .map_err(rerr)?;
        let auto_draft_md = crate::store::render_weekly_draft_markdown(&draft);

        let mut by_direction = Vec::with_capacity(draft.by_direction.len());
        for d in &draft.by_direction {
            let label = resolve_label(&mut tx, "sin90_directions", &d.direction_id).await?;
            by_direction.push(SummarizeBucket {
                label,
                minutes: d.minutes,
            });
        }
        let mut by_area = Vec::with_capacity(draft.by_area.len());
        for a in &draft.by_area {
            let label = resolve_label(&mut tx, "sin90_areas", &a.area_id).await?;
            by_area.push(SummarizeBucket {
                label,
                minutes: a.minutes,
            });
        }
        let mut routines = Vec::with_capacity(draft.routines.len());
        for r in &draft.routines {
            // A Routine's own id is never empty, so `resolve_label`'s
            // "empty id -> None" branch never fires here — `unwrap_or_else`
            // only ever falls back to the id itself when the title lookup
            // comes back `None` (2026-09-26 review, Low).
            let label = resolve_label(&mut tx, "sin90_routines", &r.routine_id)
                .await?
                .unwrap_or_else(|| r.routine_id.clone());
            routines.push(SummarizeRoutineRow {
                label,
                fired: r.fired,
                completed: r.completed,
            });
        }

        // Never commits: this is a read (same posture `AiSink::precheck`
        // already has) — best-effort, a dropped `tx` rolls back on its own
        // regardless.
        let _ = tx.rollback().await;

        Ok(SummarizeDraft {
            week: draft.week,
            by_area,
            by_direction,
            tasks_done: draft.tasks_done,
            routines,
            auto_draft_md,
        })
    }

    /// T5.3.1 (§11.4.2's "数字来源"): titles of tasks that transitioned to
    /// `done` inside `iso_week`'s window, most recent first, capped at
    /// [`DONE_TITLES_LIMIT`] (⚖️) — reference material only, never counted
    /// toward `ai::summarize::facts`.
    async fn done_titles(&self, iso_week: &str) -> Result<Vec<String>, ReadError> {
        let (start, end) = crate::core::iso_week_bounds(iso_week).ok_or_else(|| {
            ReadError(format!(
                "week must be an ISO-8601 week like 2026-W39, got {iso_week:?}"
            ))
        })?;
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT t.title
             FROM sin90_events e
             JOIN sin90_tasks t ON t.id = e.entity_id
             WHERE e.entity = 'task' AND e.kind = 'transitioned' AND e.to_state = 'done'
               AND e.at >= ? AND e.at < ?
             ORDER BY e.at DESC
             LIMIT ?",
        )
        .bind(&start)
        .bind(&end)
        .bind(DONE_TITLES_LIMIT)
        .fetch_all(&self.0)
        .await
        .map_err(rerr)?;
        Ok(rows)
    }
}

/// 2026-09-26 review (Low): shared by `AiReader::weekly_draft`'s area/
/// direction/routine title resolution — one fallback rule, not three
/// hand-copied ones. An EMPTY `id` (the "no direction"/"no area" bucket)
/// resolves to `None`; any NON-empty id whose title lookup comes back
/// `None` (a row this schema has no route to blank out today, but not
/// assumed impossible) falls back to the RAW ID — it must never silently
/// collapse into the SAME bucket as "no direction/area" just because a
/// title happened to be missing.
async fn resolve_label(
    conn: &mut sqlx::SqliteConnection,
    table: &str,
    id: &str,
) -> Result<Option<String>, ReadError> {
    if id.is_empty() {
        return Ok(None);
    }
    let title: Option<String> =
        sqlx::query_scalar(&format!("SELECT title FROM {table} WHERE id = ?"))
            .bind(id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(rerr)?;
    Ok(Some(title.unwrap_or_else(|| id.to_string())))
}

/// ⚖️ §11.4.2: at most 50 done-task titles handed to the model as reference
/// material.
const DONE_TITLES_LIMIT: i64 = 50;

fn row_to_week(r: sqlx::sqlite::SqliteRow) -> StoreResult<Week> {
    Ok(Week {
        id: r.get("id"),
        status: from_wire::<WeekStatus>(&r.get::<String, _>("status"))?,
        iso_week: r.get("iso_week"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    })
}

async fn read_ai_settings(pool: &SqlitePool) -> Result<AiSettings, ReadError> {
    let row = sqlx::query("SELECT value FROM sin90_settings WHERE key = ?")
        .bind(EXECUTIVE_ENABLED_KEY)
        .fetch_optional(pool)
        .await
        .map_err(rerr)?;
    let executive_enabled = match row {
        None => false,
        Some(r) => {
            let raw: String = r.get("value");
            serde_json::from_str::<bool>(&raw).map_err(rerr)?
        }
    };
    Ok(AiSettings { executive_enabled })
}

/// §11.4 公共 H3 / §11.5: which `Sin90Op` variants a capability's proposals
/// may contain — checked BEFORE any write-lock is taken (a pure, cheap
/// gate). See this file's module doc for why `Classify`/`Summarize` allow
/// nothing yet.
fn allowed_ops(cap: Capability, ops: &[Sin90Op]) -> Result<(), SinkError> {
    if ops.is_empty() {
        return Err(SinkError::Invalid("proposal has no ops".into()));
    }
    let ok = |op: &Sin90Op| -> bool {
        match cap {
            Capability::Classify => matches!(op, Sin90Op::AssignTaskDirection { .. }),
            Capability::Summarize => matches!(op, Sin90Op::DraftReviewBody { .. }),
            Capability::Propose => matches!(
                op,
                Sin90Op::CarryOverTask { .. }
                    | Sin90Op::ReorderTasks { .. }
                    | Sin90Op::CreateTasks { .. }
            ),
        }
    };
    if ops.iter().all(ok) {
        Ok(())
    } else {
        Err(SinkError::Invalid(format!(
            "one or more ops are not allowed for capability {}",
            cap.as_str()
        )))
    }
}

/// 2026-09-24 review: `apply_op`'s dry run can fail two structurally
/// different ways, and conflating them was a bug. A relational/constraint
/// violation (task not in the target week, week not open, a CAS `UPDATE`
/// matching zero rows, an illegal transition, …) means the PROPOSAL is bad —
/// `SinkError::Invalid`, and the caller is meant to record this as
/// `error_kind = "rejected_by_precheck"` (§11.3.5). A transient
/// infrastructure failure (`SQLITE_BUSY` under contention, a raw I/O error,
/// a `serde_json` payload that fails to (de)serialize) says NOTHING about
/// whether the proposal itself is valid — recording that as
/// "rejected_by_precheck" would be actively misleading (the model didn't
/// produce bad output; the store just hiccuped). Those map to
/// `SinkError::Store` instead, same as every other infra failure in this
/// file.
fn classify_apply_err(e: StoreError) -> SinkError {
    match e {
        StoreError::NotFound(_)
        | StoreError::Conflict(_)
        | StoreError::WeekNotOpen(_)
        | StoreError::SameWeekCarry(_)
        | StoreError::Invalid(_)
        | StoreError::Transition(_)
        | StoreError::Proposal(_) => SinkError::Invalid(e.to_string()),
        StoreError::Sqlx(_)
        | StoreError::Migrate(_)
        | StoreError::Serde(_)
        | StoreError::Internal(_) => SinkError::Store(e.to_string()),
    }
}

/// Steps 2–3 of `submit`/`precheck` (§11.4 公共): `build_snapshot` →
/// `validate` → SAVEPOINT `apply_op` × n → unconditional `ROLLBACK TO`. Never
/// commits anything — the SAVEPOINT is always rolled back regardless of
/// outcome (H3's "试跑"), so the caller's outer transaction is untouched by
/// this function either way.
async fn dry_run(
    tx: &mut crate::store::repo::Tx<'_>,
    id: &str,
    ops: &[Sin90Op],
    rationale: &Option<String>,
    cap: Capability,
) -> Result<(), SinkError> {
    // M-a (T5.7.2 review round 2): this dry run's own `capability_source` is
    // simply `cap.as_str()` — `dry_run` only ever runs INSIDE `AiSink::
    // submit`/`precheck`, i.e. this proposal IS an AI capability's own
    // output, never a human/automation `POST /proposals` submission (that
    // path is `Sin90Store::submit_proposal`, which never calls this
    // function). `allowed_ops` (checked by every caller before this) already
    // guarantees only `Capability::Classify` can ever produce an
    // `AssignTaskDirection` op, so this is also exactly the `"classify"`
    // string `reject_proposal`'s own resolution would derive once the row
    // this call is about to insert exists.
    let capability_source = cap.as_str();
    let snapshot = build_snapshot(tx, ops)
        .await
        .map_err(|e| SinkError::Store(e.to_string()))?;
    let proposal = Sin90Proposal {
        id: id.to_string(),
        status: ProposalStatus::Pending,
        // Unused by `validate` (it only inspects `.ops`); a real `source` is
        // derived later, after a successful dry run, from the call record.
        source: crate::core::ProposalSource::Rule,
        ops: ops.to_vec(),
        rationale: rationale.clone(),
    };
    validate(&proposal, &snapshot, capability_source)
        .map_err(|e| SinkError::Invalid(e.to_string()))?;

    let mut sp = tx
        .begin()
        .await
        .map_err(|e| SinkError::Store(e.to_string()))?; // SAVEPOINT
    let mut event_ids = Vec::new();
    let mut dry: Result<(), SinkError> = Ok(());
    for op in ops {
        if let Err(e) = apply_op(&mut sp, op, &mut event_ids, capability_source).await {
            dry = Err(classify_apply_err(e));
            break;
        }
    }
    sp.rollback()
        .await
        .map_err(|e| SinkError::Store(e.to_string()))?; // ROLLBACK TO SAVEPOINT — unconditional
    dry
}

impl AiSink for Sin90Store {
    async fn submit(
        &self,
        cap: Capability,
        draft: ProposalDraft,
        mut rec: AiCallRecord,
    ) -> Result<(), SinkError> {
        allowed_ops(cap, &draft.ops)?;
        // 2026-09-24 review: `submit` is where a call record turns into a
        // committed `ok=1` proposal row — it must not blindly trust what the
        // caller put in `rec`. Two defenses:
        //   1. `rec.engine == Reflex` together with a `served_tier` is a
        //      contradiction (reflex never talks to a model, so it can never
        //      have a served tier) — reject rather than silently store
        //      nonsense that `source_for` would then derive a wrong
        //      `source` from.
        //   2. Regardless of what `rec.ok`/`rec.error_kind` said coming in,
        //      the row `submit` inserts is ALWAYS `ok=1, error_kind=NULL` —
        //      by definition, reaching this point (past the dry run below)
        //      means this attempt produced the proposal being submitted.
        if rec.engine == Engine::Reflex && rec.served_tier.is_some() {
            return Err(SinkError::Invalid(
                "a reflex call record cannot carry a served_tier".into(),
            ));
        }
        // L2 (T5.7.2 review round 3): `rec.task_kind` is `sin90_ai_calls.
        // task_kind`'s own value — it must be the SAME capability `submit`
        // is being called for, not merely a record that happens to carry
        // valid ops for `cap` (`allowed_ops` above only checks the OPS, not
        // where the call record itself claims to have come from). Every
        // real caller already constructs `rec` via `run_item(cap, ...)`
        // (`ladder.rs`), so `rec.task_kind == cap` holds by construction
        // today — this is defense in depth against a future caller
        // assembling `rec` by hand and mismatching it, which would silently
        // mislabel `sin90_ai_calls.task_kind` for the row this call inserts.
        if rec.task_kind != cap {
            return Err(SinkError::Invalid(format!(
                "call record task_kind {:?} does not match capability {cap:?} being submitted",
                rec.task_kind
            )));
        }

        let mut tx = self
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| SinkError::Store(e.to_string()))?;

        dry_run(&mut tx, &draft.id, &draft.ops, &draft.rationale, cap).await?;
        rec.ok = true;
        rec.error_kind = None;

        let source = source_for(rec.engine, rec.served_tier);
        let source_wire = to_wire(&source).map_err(|e| SinkError::Store(e.to_string()))?;
        let ops_json =
            serde_json::to_string(&draft.ops).map_err(|e| SinkError::Store(e.to_string()))?;
        let now = crate::core::now_iso8601();

        sqlx::query(
            "INSERT INTO sin90_proposals (id, status, source, ops, rationale, created_at)
             VALUES (?, 'pending', ?, ?, ?, ?)",
        )
        .bind(&draft.id)
        .bind(&source_wire)
        .bind(&ops_json)
        .bind(&draft.rationale)
        .bind(&now)
        .execute(&mut *tx)
        .await
        .map_err(|e| SinkError::Store(e.to_string()))?;

        append_event(
            &mut tx,
            "proposal",
            &draft.id,
            "submitted",
            None,
            Some("pending"),
            &serde_json::json!({"id": draft.id, "source": source_wire, "ops_count": draft.ops.len()}),
            &now,
        )
        .await
        .map_err(|e| SinkError::Store(e.to_string()))?;

        rec.proposal_id = Some(draft.id.clone());
        insert_call_row(&mut *tx, &rec)
            .await
            .map_err(|e| SinkError::Store(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| SinkError::Store(e.to_string()))?;
        Ok(())
    }

    async fn record_call(&self, rec: AiCallRecord) -> Result<(), SinkError> {
        let mut conn = self
            .pool()
            .acquire()
            .await
            .map_err(|e| SinkError::Store(e.to_string()))?;
        insert_call_row(&mut *conn, &rec)
            .await
            .map_err(|e| SinkError::Store(e.to_string()))
    }

    async fn record_classify_eval(&self, task_id: &str) -> Result<(), SinkError> {
        let now = crate::core::now_iso8601();
        sqlx::query(
            "INSERT INTO sin90_classify_evals (task_id, evaluated_at) VALUES (?, ?)
             ON CONFLICT(task_id) DO UPDATE SET evaluated_at = excluded.evaluated_at",
        )
        .bind(task_id)
        .bind(&now)
        .execute(self.pool())
        .await
        .map_err(|e| SinkError::Store(e.to_string()))?;
        Ok(())
    }

    async fn precheck(&self, cap: Capability, drafts: &[ProposalDraft]) -> Vec<bool> {
        let Ok(mut tx) = self.pool().begin_with("BEGIN IMMEDIATE").await else {
            return vec![false; drafts.len()];
        };
        let mut out = Vec::with_capacity(drafts.len());
        for d in drafts {
            let ok = allowed_ops(cap, &d.ops).is_ok()
                && dry_run(&mut tx, &d.id, &d.ops, &d.rationale, cap)
                    .await
                    .is_ok();
            out.push(ok);
        }
        // Never commits: this is a read (§11.4 公共's "去重" — one write-lock
        // acquisition, zero net writes).
        let _ = tx.rollback().await;
        out
    }
}

/// Shared by `submit` (inside its transaction) and `record_call` (its own
/// single-statement write) — same column list, same bind order.
async fn insert_call_row<'e, E>(executor: E, rec: &AiCallRecord) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query(
        "INSERT INTO sin90_ai_calls
             (id, run_id, task_kind, engine, fallback_from, served_tier, model_id,
              prompt_tokens, completion_tokens, latency_ms, ok, error_kind, proposal_id, at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&rec.id)
    .bind(&rec.run_id)
    .bind(rec.task_kind.as_str())
    .bind(rec.engine.as_str())
    .bind(rec.fallback_from.map(|e| e.as_str()))
    .bind(rec.served_tier.map(|t| t.as_str()))
    .bind(&rec.model_id)
    .bind(rec.prompt_tokens.map(|n| n as i64))
    .bind(rec.completion_tokens.map(|n| n as i64))
    .bind(rec.latency_ms as i64)
    .bind(rec.ok)
    .bind(rec.error_kind)
    .bind(&rec.proposal_id)
    .bind(&rec.at)
    .execute(executor)
    .await?;
    Ok(())
}

/// `PUT /settings/ai` (J10b): upsert `ai.executive_enabled` and mirror
/// `setting.changed`, same one-`BEGIN IMMEDIATE`-with-event convention every
/// other direct write in `repo.rs` uses.
impl Sin90Store {
    pub async fn get_ai_settings(&self) -> StoreResult<AiSettings> {
        read_ai_settings(self.pool())
            .await
            .map_err(|e| StoreError::Internal(e.0))
    }

    pub async fn put_ai_executive_enabled(&self, value: bool) -> StoreResult<AiSettings> {
        let now = crate::core::now_iso8601();
        let value_json = serde_json::to_string(&value)?;
        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "INSERT INTO sin90_settings (key, value, updated_at) VALUES (?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(EXECUTIVE_ENABLED_KEY)
        .bind(&value_json)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        append_event(
            &mut tx,
            "setting",
            EXECUTIVE_ENABLED_KEY,
            "changed",
            None,
            None,
            &serde_json::json!({"key": EXECUTIVE_ENABLED_KEY, "value": value}),
            &now,
        )
        .await?;
        tx.commit().await?;
        Ok(AiSettings {
            executive_enabled: value,
        })
    }

    /// New (2026-09-24 review, M5): every `sin90_ai_calls` row for one run —
    /// `GET /ai/runs/{run_id}`'s `calls` field. Reads the DURABLE record
    /// (unlike the in-memory `RunRegistry`), so this still answers something
    /// useful even for a run evicted from the 64-entry log or one from
    /// before a process restart (§11.9 R11) — only `items`/`state` are lost
    /// in those cases, not the call history.
    pub async fn list_ai_calls_for_run(&self, run_id: &str) -> StoreResult<Vec<AiCallSummary>> {
        let rows = sqlx::query(
            "SELECT id, task_kind, engine, fallback_from, served_tier, model_id, ok, \
             error_kind, latency_ms, proposal_id, at
             FROM sin90_ai_calls WHERE run_id = ? ORDER BY at ASC, rowid ASC",
        )
        .bind(run_id)
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| AiCallSummary {
                id: r.get("id"),
                task_kind: r.get("task_kind"),
                engine: r.get("engine"),
                fallback_from: r.get("fallback_from"),
                served_tier: r.get("served_tier"),
                model_id: r.get("model_id"),
                ok: r.get("ok"),
                error_kind: r.get("error_kind"),
                latency_ms: r.get("latency_ms"),
                proposal_id: r.get("proposal_id"),
                at: r.get("at"),
            })
            .collect())
    }

    /// M-2 (2026-09-24 review round 3, design §11.4.3): the set of
    /// `sin90_proposals.id` that are AI-PRODUCED for one capability — the
    /// join `sin90_ai_calls.proposal_id` (with `task_kind = <capability>`,
    /// `ok = 1`) is the ONLY authoritative signal (§11.4 公共's own "提议
    /// 形状": "是不是 AI 产出以 sin90_ai_calls.proposal_id 关联为准，J24"),
    /// NOT the `"ai-<capability>-<ulid>"` id PREFIX `ai::propose::submit_one`
    /// mints for readability — a human or automation client submitting
    /// through `POST /proposals` supplies their OWN id and could pick
    /// anything, including a string that happens to start with
    /// `"ai-propose-"`; propose's dedup (§11.4 公共's "去重") must never let
    /// such a proposal block a fresh AI decision.
    /// L-a (2026-09-24 review round 4): joined against `sin90_proposals`
    /// and filtered to `status = 'pending'` directly in SQL — dedup only
    /// ever cares about STILL-PENDING proposals (an accepted/rejected one
    /// cannot block anything), so there is no reason to hand the caller ids
    /// for proposals it would immediately have to filter back out again
    /// after a SEPARATE `list_pending_proposals()` call.
    pub async fn list_ai_produced_proposal_ids(
        &self,
        task_kind: &str,
    ) -> StoreResult<std::collections::HashSet<String>> {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT p.id FROM sin90_proposals p \
             JOIN sin90_ai_calls c ON c.proposal_id = p.id \
             WHERE p.status = 'pending' AND c.task_kind = ? AND c.ok = 1",
        )
        .bind(task_kind)
        .fetch_all(self.pool())
        .await?;
        Ok(ids.into_iter().collect())
    }
}

/// One `sin90_ai_calls` row, as `GET /ai/runs/{run_id}` serializes it
/// (2026-09-24 review, M5). Deliberately its OWN shape, not
/// `ai::AiCallRecord` — that type's `task_kind`/`engine`/`served_tier` are
/// typed enums meant for the `ai` module's internal use, not a wire format,
/// and `ai::ports` types must not leak `sqlx` row-mapping concerns.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AiCallSummary {
    pub id: String,
    pub task_kind: String,
    pub engine: String,
    pub fallback_from: Option<String>,
    pub served_tier: Option<String>,
    pub model_id: Option<String>,
    pub ok: bool,
    pub error_kind: Option<String>,
    pub latency_ms: i64,
    pub proposal_id: Option<String>,
    pub at: String,
}

#[cfg(test)]
mod tests {
    //! J8 (adapted to T5.1.1's scope), J9, and the read-only-pool tests
    //! (§11.5 v2.1 M1) — see `tests/ai_boundary.rs`'s module doc for why
    //! these live here (needs `Sin90Store::pool()`, `pub(crate)`) rather
    //! than in an integration test.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::BTreeMap;

    use sqlx::sqlite::SqlitePool;

    use super::*;
    use crate::core::{Energy, NewTask, TaskKind};

    fn rec(id: &str, run_id: &str, ok: bool, error_kind: Option<&'static str>) -> AiCallRecord {
        AiCallRecord {
            id: id.into(),
            run_id: run_id.into(),
            task_kind: Capability::Propose,
            engine: crate::ai::Engine::Reflex,
            fallback_from: None,
            served_tier: None,
            model_id: None,
            prompt_tokens: None,
            completion_tokens: None,
            latency_ms: 1,
            ok,
            error_kind,
            proposal_id: None,
            at: "2026-09-24T00:00:00Z".into(),
        }
    }

    // 2026-09-24 review (M4): moved to `store::test_hooks::{snapshot_all_
    // tables, diff_snapshot_keys}` so `ai::propose`'s own J21 table-diff test
    // can reuse the SAME implementation instead of a second copy — thin
    // local aliases below keep every call site in this file unchanged.
    async fn snapshot_all_tables(pool: &SqlitePool) -> BTreeMap<String, Vec<String>> {
        crate::store::test_hooks::snapshot_all_tables(pool).await
    }
    fn diff_keys(
        before: &BTreeMap<String, Vec<String>>,
        after: &BTreeMap<String, Vec<String>>,
    ) -> Vec<String> {
        crate::store::test_hooks::diff_snapshot_keys(before, after)
    }

    /// Seeds one open week with two planned tasks, via the EXISTING (non-AI)
    /// proposal path — `AiSink`/`AiReadModel` must never be the only way to
    /// get fixture data into a test; that would make "did AI write?"
    /// unanswerable by construction.
    async fn seed_week_with_tasks(store: &Sin90Store) -> (String, String, String) {
        let week = store.create_week("2026-W40").await.unwrap();
        let p = Sin90Proposal {
            id: "seed-create-tasks".into(),
            status: ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![Sin90Op::CreateTasks {
                week_id: week.id.clone(),
                tasks: vec![
                    NewTask {
                        title: "t1".into(),
                        direction_id: None,
                    },
                    NewTask {
                        title: "t2".into(),
                        direction_id: None,
                    },
                ],
            }],
            rationale: None,
        };
        store.submit_proposal(&p).await.unwrap();
        store.apply_proposal(&p.id).await.unwrap();
        let tasks: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM sin90_tasks WHERE week_id = ? ORDER BY sort_key ASC",
        )
        .bind(&week.id)
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert_eq!(tasks.len(), 2);
        (week.id, tasks[0].clone(), tasks[1].clone())
    }

    #[tokio::test]
    async fn ai_boundary_tables_unchanged() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (week_id, t1, t2) = seed_week_with_tasks(&store).await;
        let pool = store.pool().clone();

        // ---- 1. every AiReadModel read + AiSink::precheck: reads alone
        //         must change NOTHING, including the three tables that ARE
        //         allowed to change from a real submit/record_call (they
        //         simply aren't touched by a pure read).
        let before = snapshot_all_tables(&pool).await;
        let reader = store.ai_reader();
        let _ = reader.settings().await.unwrap();
        let _ = reader.inbox(50).await.unwrap();
        let _ = reader.inbox_task(&t1).await.unwrap();
        let _ = reader.direction_candidates(50).await.unwrap();
        let _ = reader.title_history("t1").await.unwrap();
        let _ = reader.review("does-not-exist").await.unwrap();
        let _ = reader.week_tasks(&week_id).await.unwrap();
        let valid_draft = ProposalDraft {
            id: "precheck-1".into(),
            ops: vec![Sin90Op::ReorderTasks {
                week_id: week_id.clone(),
                order: vec![t2.clone(), t1.clone()],
            }],
            rationale: None,
        };
        let results = AiSink::precheck(
            &store,
            Capability::Propose,
            std::slice::from_ref(&valid_draft),
        )
        .await;
        assert_eq!(results, vec![true]);
        let after = snapshot_all_tables(&pool).await;
        assert_eq!(
            diff_keys(&before, &after),
            Vec::<String>::new(),
            "AiReadModel reads + AiSink::precheck must not change ANY table"
        );

        // ---- 2. AiSink::record_call (a failed, non-producing attempt)
        //         touches ONLY sin90_ai_calls.
        let before = after;
        AiSink::record_call(&store, rec("call-1", "run-1", false, Some("timeout")))
            .await
            .unwrap();
        let after = snapshot_all_tables(&pool).await;
        assert_eq!(
            diff_keys(&before, &after),
            vec!["sin90_ai_calls".to_string()],
            "record_call must touch ONLY sin90_ai_calls"
        );

        // ---- 3. AiSink::submit (H3's dry run: validate + SAVEPOINT
        //         apply_op + unconditional ROLLBACK TO) touches ONLY
        //         sin90_proposals, sin90_ai_calls, and the
        //         `entity = 'proposal'` half of sin90_events (the
        //         proposal.submitted row) — sin90_tasks is UNCHANGED even
        //         though the proposal's ops reorder it, because the apply
        //         itself was rolled back, and the `entity <> 'proposal'`
        //         half of sin90_events is untouched too (M1: this is the
        //         narrower claim the design's 3-item exclusion list
        //         actually makes, not just "sin90_events changed somehow").
        let before = after;
        let produced = rec("call-2", "run-1", true, None);
        AiSink::submit(&store, Capability::Propose, valid_draft.clone(), produced)
            .await
            .unwrap();
        let after = snapshot_all_tables(&pool).await;
        let mut changed = diff_keys(&before, &after);
        changed.sort();
        assert_eq!(
            changed,
            vec![
                "sin90_ai_calls".to_string(),
                "sin90_events(entity=proposal)".to_string(),
                "sin90_proposals".to_string(),
            ],
            "submit's dry-run apply must leave sin90_tasks (and everything else, including \
             non-proposal events) untouched"
        );
        assert_eq!(
            before.get("sin90_tasks"),
            after.get("sin90_tasks"),
            "the ReorderTasks op inside the submitted proposal must not have actually applied"
        );

        // ---- Positive control (this test's proof it isn't vacuous): NOW
        //      really accept the proposal — sin90_tasks MUST change,
        //      proving the diff above would have caught a real mutation had
        //      `submit`'s rollback been broken.
        let before = after;
        store.apply_proposal("precheck-1").await.unwrap();
        let after = snapshot_all_tables(&pool).await;
        assert_ne!(
            before.get("sin90_tasks"),
            after.get("sin90_tasks"),
            "accepting the proposal for real must change sin90_tasks — otherwise this \
             harness could never detect a real regression"
        );
    }

    /// `AiSink::submit` rejects an invalid proposal (an op referencing a
    /// task not in the target week) and leaves the store byte-for-byte
    /// unchanged — no proposal row, no `ok=1` call row, no event, `sort_key`
    /// untouched (§11.4 公共 H3; this branch's stand-in for T5.4.1's J21,
    /// using the ops that already exist today).
    #[tokio::test]
    async fn ai_sink_submit_rejects_invalid_and_leaves_store_unchanged() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (week_id, t1, _t2) = seed_week_with_tasks(&store).await;
        let pool = store.pool().clone();

        let before = snapshot_all_tables(&pool).await;
        let bad_draft = ProposalDraft {
            id: "bad-1".into(),
            ops: vec![Sin90Op::ReorderTasks {
                week_id: week_id.clone(),
                order: vec![t1.clone(), "does-not-exist".into()],
            }],
            rationale: None,
        };
        let err = AiSink::submit(
            &store,
            Capability::Propose,
            bad_draft,
            rec("call-x", "run-x", true, None),
        )
        .await;
        assert!(
            err.is_err(),
            "an op referencing a nonexistent task must be rejected"
        );
        let after = snapshot_all_tables(&pool).await;
        assert_eq!(
            before, after,
            "a rejected submit must leave the store byte-for-byte unchanged"
        );

        // Positive control: the same shape, but every task really is in the week.
        let good_draft = ProposalDraft {
            id: "good-1".into(),
            ops: vec![Sin90Op::ReorderTasks {
                week_id,
                order: vec![t1],
            }],
            rationale: None,
        };
        AiSink::submit(
            &store,
            Capability::Propose,
            good_draft,
            rec("call-y", "run-x", true, None),
        )
        .await
        .unwrap();
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_proposals WHERE id = 'good-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    /// J9: every row from one run shares `run_id`; the produced proposal
    /// has EXACTLY one `ok=1` call row pointing at it; every `ok=0` row has
    /// a non-empty `error_kind`. Mutation target: make `submit` insert the
    /// call row BEFORE the proposal row and fail the proposal insert — the
    /// call row would then exist with no matching proposal, which this
    /// test's join-count assertion would catch.
    #[tokio::test]
    async fn ai_calls_link_integrity() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (week_id, t1, t2) = seed_week_with_tasks(&store).await;
        let pool = store.pool().clone();
        let run_id = "run-integrity-1";

        AiSink::record_call(
            &store,
            rec("c1", run_id, false, Some("unavailable.no_provider")),
        )
        .await
        .unwrap();
        AiSink::record_call(&store, rec("c2", run_id, false, Some("undecided")))
            .await
            .unwrap();
        let draft = ProposalDraft {
            id: "p-integrity-1".into(),
            ops: vec![Sin90Op::ReorderTasks {
                week_id,
                order: vec![t2, t1],
            }],
            rationale: None,
        };
        AiSink::submit(
            &store,
            Capability::Propose,
            draft,
            rec("c3", run_id, true, None),
        )
        .await
        .unwrap();

        let run_ids: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT run_id FROM sin90_ai_calls WHERE run_id = ?")
                .bind(run_id)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(run_ids, vec![run_id.to_string()]);

        let ok_rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM sin90_ai_calls WHERE run_id = ? AND ok = 1")
                .bind(run_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(ok_rows, 1);

        let joined: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sin90_ai_calls c JOIN sin90_proposals p
             ON p.id = c.proposal_id WHERE c.run_id = ? AND c.ok = 1",
        )
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            joined, 1,
            "the ok=1 row's proposal_id must resolve to the proposal submit() inserted"
        );

        let bad_error_kind: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sin90_ai_calls WHERE run_id = ? AND ok = 0 AND error_kind IS NULL",
        )
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            bad_error_kind, 0,
            "every ok=0 row must have a non-null error_kind"
        );
    }

    /// The ai read model's write attempts fail with SQLite's `readonly`
    /// error — in BOTH the file/WAL mode a real deployment uses and the
    /// named shared-cache in-memory mode `open_memory` now uses for tests
    /// (§11.5 v2.1 M1). Mutation target: drop `pragma("query_only", "ON")`
    /// from `Sin90Store::open`/`open_memory` and both halves of this test
    /// go from `is_err()` to a successful write.
    #[tokio::test]
    async fn ai_reader_query_only_shared_cache_memory() {
        let store = Sin90Store::open_memory().await.unwrap();
        let reader_pool = store.ai_reader().pool().clone();
        // Reader sees the writer's data (same named shared-cache db).
        store.create_area("Work").await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_areas")
            .fetch_one(&reader_pool)
            .await
            .unwrap();
        assert_eq!(n, 1);
        // Reader cannot write.
        let err = sqlx::query(
            "INSERT INTO sin90_areas (id, title, slug, status, sort_key, created_at, updated_at)
             VALUES ('x','x','x','active',0,'t','t')",
        )
        .execute(&reader_pool)
        .await;
        assert!(
            err.unwrap_err()
                .to_string()
                .to_lowercase()
                .contains("readonly"),
            "a write through the ai reader pool must fail as readonly"
        );
        // Positive control: the writer pool still writes.
        store.create_area("Health").await.unwrap();
        let n2: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_areas")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(n2, 2);
    }

    #[tokio::test]
    async fn ai_reader_query_only_file_wal() {
        let dir = std::env::temp_dir().join(format!(
            "sin90-ai-boundary-{}-{}",
            std::process::id(),
            crate::core::ulid()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("sin90.db");
        let store = Sin90Store::open(&db_path).await.unwrap();
        let reader_pool = store.ai_reader().pool().clone();

        store.create_area("Work").await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_areas")
            .fetch_one(&reader_pool)
            .await
            .unwrap();
        assert_eq!(n, 1);
        let err = sqlx::query(
            "INSERT INTO sin90_areas (id, title, slug, status, sort_key, created_at, updated_at)
             VALUES ('x','x','x','active',0,'t','t')",
        )
        .execute(&reader_pool)
        .await;
        assert!(
            err.unwrap_err()
                .to_string()
                .to_lowercase()
                .contains("readonly"),
            "a write through the ai reader pool must fail as readonly"
        );
        store.create_area("Health").await.unwrap();
        let n2: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_areas")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(n2, 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 2026-09-24 review: submit doesn't trust the caller's rec --------

    /// `submit` writes `ok=1, error_kind=NULL` REGARDLESS of what the caller
    /// put in `rec` — reaching past the dry run means this attempt produced
    /// the proposal. Mutation target: remove the `rec.ok = true; rec.error_kind
    /// = None;` overwrite in `submit` and this goes red (the row keeps the
    /// caller's `ok=0, error_kind=Some("bogus")`).
    #[tokio::test]
    async fn ai_sink_submit_always_writes_ok_true_and_null_error_kind() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (week_id, t1, t2) = seed_week_with_tasks(&store).await;
        let draft = ProposalDraft {
            id: "p-forced-ok".into(),
            ops: vec![Sin90Op::ReorderTasks {
                week_id,
                order: vec![t2, t1],
            }],
            rationale: None,
        };
        // A caller that (wrongly) claims failure — submit must not trust it.
        let mut bogus = rec("call-forced", "run-forced", false, Some("bogus"));
        bogus.proposal_id = None;
        AiSink::submit(&store, Capability::Propose, draft, bogus)
            .await
            .unwrap();

        let (ok, error_kind): (bool, Option<String>) =
            sqlx::query_as("SELECT ok, error_kind FROM sin90_ai_calls WHERE id = 'call-forced'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert!(
            ok,
            "submit must force ok=1 regardless of what the caller passed"
        );
        assert_eq!(
            error_kind, None,
            "submit must force error_kind=NULL regardless of what the caller passed"
        );
    }

    /// `engine == Reflex` with a non-`None` `served_tier` is a contradiction
    /// (reflex never talks to a model) — `submit` rejects it before even
    /// opening a transaction. Mutation target: remove the guard in `submit`
    /// and this goes from `is_err()` to succeeding (and `source_for` would
    /// then derive a nonsensical `source` from the bogus tier).
    #[tokio::test]
    async fn ai_sink_submit_rejects_reflex_with_served_tier() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (week_id, t1, t2) = seed_week_with_tasks(&store).await;
        let draft = ProposalDraft {
            id: "p-bad-combo".into(),
            ops: vec![Sin90Op::ReorderTasks {
                week_id,
                order: vec![t2, t1],
            }],
            rationale: None,
        };
        let mut bad = rec("call-bad-combo", "run-bad-combo", true, None);
        bad.served_tier = Some(crate::ai::ports::ServedTier::Local);
        let err = AiSink::submit(&store, Capability::Propose, draft, bad).await;
        assert!(
            err.is_err(),
            "reflex + served_tier is a contradiction and must be rejected"
        );
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM sin90_proposals WHERE id = 'p-bad-combo'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(
            n, 0,
            "the rejected submit must not have written a proposal row"
        );
    }

    /// L2 (T5.7.2 review round 3): `rec.task_kind` disagreeing with the
    /// `cap` `submit` is called for is rejected before even opening a
    /// transaction, same posture as the reflex/served_tier contradiction
    /// just above. Mutation target: remove the `rec.task_kind != cap` guard
    /// in `submit` and this goes from `is_err()` to succeeding (silently
    /// mislabeling `sin90_ai_calls.task_kind` for the inserted row).
    #[tokio::test]
    async fn ai_sink_submit_rejects_task_kind_mismatch() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (_week_id, t1, _t2) = seed_week_with_tasks(&store).await;
        let direction = store
            .create_direction("Kind mismatch target", "2026-Q4", None)
            .await
            .unwrap();
        // `Sin90Op::AssignTaskDirection` — allowed for `Capability::Classify`
        // by `allowed_ops`, so this isolates the `task_kind` check itself
        // rather than tripping the OPS-vs-capability check first.
        let draft = ProposalDraft {
            id: "p-kind-mismatch".into(),
            ops: vec![Sin90Op::AssignTaskDirection {
                task_id: t1,
                direction_id: direction.id,
            }],
            rationale: None,
        };
        // `rec()`'s default `task_kind` is `Capability::Propose`; submitting
        // it under `Capability::Classify` is the mismatch.
        let mismatched = rec("call-kind-mismatch", "run-kind-mismatch", true, None);
        let err = AiSink::submit(&store, Capability::Classify, draft, mismatched).await;
        assert!(
            err.is_err(),
            "a call record's task_kind must match the capability submit is called for"
        );
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM sin90_proposals WHERE id = 'p-kind-mismatch'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(
            n, 0,
            "the rejected submit must not have written a proposal row"
        );
    }

    /// A relational/constraint error from `apply_op` (task not in the
    /// target week) classifies as `SinkError::Invalid` — the caller is
    /// meant to record this as `rejected_by_precheck`; a raw infrastructure
    /// error (SQLite busy, I/O, a broken internal invariant) classifies as
    /// `SinkError::Store` instead, since it says nothing about whether the
    /// PROPOSAL was valid. Mutation target: collapse `classify_apply_err` to
    /// always return `SinkError::Invalid` and the second assertion goes red
    /// (matching on `SinkError::Store` would then fail).
    #[test]
    fn classify_apply_err_distinguishes_constraint_from_infra_errors() {
        assert!(matches!(
            classify_apply_err(StoreError::NotFound("task x".into())),
            SinkError::Invalid(_)
        ));
        assert!(matches!(
            classify_apply_err(StoreError::WeekNotOpen("w".into())),
            SinkError::Invalid(_)
        ));
        assert!(matches!(
            classify_apply_err(StoreError::Conflict("dup".into())),
            SinkError::Invalid(_)
        ));
        assert!(matches!(
            classify_apply_err(StoreError::Internal("broken invariant".into())),
            SinkError::Store(_)
        ));
        assert!(matches!(
            classify_apply_err(StoreError::Sqlx(sqlx::Error::PoolClosed)),
            SinkError::Store(_)
        ));
    }

    /// H1 (2026-09-24 review, blocking): a task that got carried over WHILE
    /// still unclassified — its ORIGINAL row flips to `carried_over` but
    /// `CarryOverTask`'s apply never touches `direction_id` — must NOT show
    /// up in the inbox (it is closed, historical; nothing will ever classify
    /// it again). The NEW task the carry-over produces (still unclassified,
    /// still open) DOES belong in the inbox. Mutation target: revert
    /// `inbox`'s exclusion list to the literal `('done', 'dropped')` this
    /// replaces and the first assertion goes red.
    #[tokio::test]
    async fn inbox_excludes_carried_over_tasks() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (_week, t1, t2) = seed_week_with_tasks(&store).await; // both planned, direction_id NULL
        let next_week = store.create_week("2026-W41").await.unwrap();

        let carry = Sin90Proposal {
            id: "carry-1".into(),
            status: ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![Sin90Op::CarryOverTask {
                task_id: t1.clone(),
                to_week: next_week.id.clone(),
            }],
            rationale: None,
        };
        store.submit_proposal(&carry).await.unwrap();
        store.apply_proposal("carry-1").await.unwrap();

        let child: String = sqlx::query_scalar("SELECT id FROM sin90_tasks WHERE carried_from = ?")
            .bind(&t1)
            .fetch_one(store.pool())
            .await
            .unwrap();

        let reader = store.ai_reader();
        let inbox_ids: Vec<String> = reader
            .inbox(50)
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert!(
            !inbox_ids.contains(&t1),
            "the CLOSED (carried_over) original task must not be in the inbox: {inbox_ids:?}"
        );
        assert!(
            inbox_ids.contains(&t2),
            "an untouched, still-open unclassified task must still be in the inbox"
        );
        assert!(
            inbox_ids.contains(&child),
            "the NEW task the carry-over produced is still open and unclassified"
        );

        // Positive control for `inbox_task` (L4's point lookup): the closed
        // task is unusable as an explicit classify target; the new one isn't.
        assert!(reader.inbox_task(&t1).await.unwrap().is_none());
        assert!(reader.inbox_task(&child).await.unwrap().is_some());
    }

    /// §2 #25 (T5.4.1, layer split 2026-09-24 — kept here at layer A, NOT
    /// moved with `ai::propose` to layer B, since `repo::apply_op` is where
    /// the change actually lives and this exercises it directly through the
    /// existing non-AI `submit_proposal`/`apply_proposal` path, with no
    /// dependency on `ai::propose` at all): both `CreateTasks` (a task
    /// created directly INTO a Direction, never touched by
    /// `AssignTaskDirection`) and `CarryOverTask` (the new task inherits the
    /// SOURCE task's Direction) must stamp the owning Direction into the
    /// `task.created` event's payload, not just the `sin90_tasks` row —
    /// §11.2.1's replay rule #2 needs a self-contained ownership fact per
    /// event. Mutation target: drop `"direction_id": t.direction_id` /
    /// `"direction_id": direction_id` from either `json!` call in
    /// `apply_op` (`store/repo.rs`) and the corresponding assertion below
    /// goes red — the key disappears from the payload entirely (an
    /// `Option<DirectionId>`'s `Some` serializes as a plain string; removing
    /// the field is the only way the mutation could hide, not turning it
    /// null).
    #[tokio::test]
    async fn task_created_event_payload_carries_direction_id_for_create_and_carry_over() {
        let store = Sin90Store::open_memory().await.unwrap();
        let dir = store
            .create_direction("Ship it", "2026-Q4", None)
            .await
            .unwrap();
        let week = store.create_week("2026-W40").await.unwrap();

        // CreateTasks: a task created directly INTO a Direction.
        let create = Sin90Proposal {
            id: "create-with-dir".into(),
            status: ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![Sin90Op::CreateTasks {
                week_id: week.id.clone(),
                tasks: vec![NewTask {
                    title: "owned task".into(),
                    direction_id: Some(dir.id.clone()),
                }],
            }],
            rationale: None,
        };
        store.submit_proposal(&create).await.unwrap();
        store.apply_proposal(&create.id).await.unwrap();
        let created_id: String =
            sqlx::query_scalar("SELECT id FROM sin90_tasks WHERE title = 'owned task'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        let payload: String = sqlx::query_scalar(
            "SELECT payload FROM sin90_events WHERE entity = 'task' AND entity_id = ? AND kind = 'created'",
        )
        .bind(&created_id)
        .fetch_one(store.pool())
        .await
        .unwrap();
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(
            payload.get("direction_id").and_then(|v| v.as_str()),
            Some(dir.id.as_str()),
            "CreateTasks's task.created payload must carry the task's Direction: {payload:?}"
        );

        // CarryOverTask: the new task inherits the SOURCE task's Direction.
        let next_week = store.create_week("2026-W41").await.unwrap();
        let carry = Sin90Proposal {
            id: "carry-with-dir".into(),
            status: ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![Sin90Op::CarryOverTask {
                task_id: created_id.clone(),
                to_week: next_week.id.clone(),
            }],
            rationale: None,
        };
        store.submit_proposal(&carry).await.unwrap();
        store.apply_proposal(&carry.id).await.unwrap();
        let carried_id: String =
            sqlx::query_scalar("SELECT id FROM sin90_tasks WHERE carried_from = ?")
                .bind(&created_id)
                .fetch_one(store.pool())
                .await
                .unwrap();
        let payload: String = sqlx::query_scalar(
            "SELECT payload FROM sin90_events WHERE entity = 'task' AND entity_id = ? AND kind = 'created'",
        )
        .bind(&carried_id)
        .fetch_one(store.pool())
        .await
        .unwrap();
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(
            payload.get("direction_id").and_then(|v| v.as_str()),
            Some(dir.id.as_str()),
            "CarryOverTask's task.created payload must inherit the source task's Direction: {payload:?}"
        );
    }

    /// 2026-09-24 review (round 2, #4): `title_history` must exclude a
    /// classified task whose Direction has since been abandoned — feeding
    /// R1 (`ai::classify::r1_reflex`, T5.2.1b) an empty history for that
    /// title, which R1's own contract reads as "undecided" (able to degrade
    /// to the model or fall back to R2), not as "decisively points nowhere".
    /// Mutation target: delete the `terminal_direction_status_wires()`
    /// filtering (revert to no `WHERE d.status NOT IN (...)` clause at all)
    /// and this goes red — the abandoned Direction's id would come back.
    #[tokio::test]
    async fn r1_history_ignores_abandoned_direction() {
        let store = Sin90Store::open_memory().await.unwrap();
        let direction = store
            .create_direction("Side quest", "2026-Q4", None)
            .await
            .unwrap();
        store
            .create_task(
                "Write the report",
                Some(&direction.id),
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();
        let reader = store.ai_reader();

        // Positive control: while the Direction is still open, history sees it.
        let before = reader.title_history("write the report").await.unwrap();
        assert_eq!(before, vec![direction.id.clone()]);

        // Abandon it (no direct-write route/Op exists for this in this
        // branch — see `abandon_direction`'s doc for why raw SQL is the
        // deliberate choice here, same convention `ai::classify`'s own
        // fixtures use for this exact gap).
        sqlx::query("UPDATE sin90_directions SET status = 'abandoned' WHERE id = ?")
            .bind(&direction.id)
            .execute(store.pool())
            .await
            .unwrap();

        let after = reader.title_history("write the report").await.unwrap();
        assert!(
            after.is_empty(),
            "a classified-but-now-abandoned Direction must not show up in R1's history: {after:?}"
        );
    }

    /// L4 (2026-09-24 review, round 3): `direction_candidates` (§11.4.1's
    /// "候选集") must exclude an abandoned Direction too — this was only
    /// exercised indirectly (through `title_history`'s own regression); this
    /// pins `direction_candidates` itself. Mutation target: swap
    /// `terminal_direction_status_wires()`'s result for a set that names no
    /// real wire value (e.g. `["__none__".to_string()]`) and this goes red
    /// (the abandoned Direction comes back as a candidate).
    #[tokio::test]
    async fn direction_candidates_excludes_abandoned_direction() {
        let store = Sin90Store::open_memory().await.unwrap();
        let open = store
            .create_direction("Work", "2026-Q4", None)
            .await
            .unwrap();
        let closed = store
            .create_direction("Side quest", "2026-Q4", None)
            .await
            .unwrap();
        sqlx::query("UPDATE sin90_directions SET status = 'abandoned' WHERE id = ?")
            .bind(&closed.id)
            .execute(store.pool())
            .await
            .unwrap();

        let reader = store.ai_reader();
        let candidates = reader.direction_candidates(50).await.unwrap();
        let ids: Vec<String> = candidates.iter().map(|c| c.direction_id.clone()).collect();
        assert!(
            ids.contains(&open.id),
            "the still-open Direction must be a candidate: {ids:?}"
        );
        assert!(
            !ids.contains(&closed.id),
            "the abandoned Direction must NOT be a candidate: {ids:?}"
        );
    }

    /// T5.2.2 (design §2 #30): the reserved "待定" Direction — seeded by
    /// migration `0014_triage_direction.sql`, so it is present in this
    /// `open_memory()` fixture from the start, is `active` (non-terminal)
    /// and would otherwise pass every existing filter here — must still
    /// never come back as a candidate. Mutation target: delete the
    /// `AND d.id != ?` clause (or the `.bind(crate::core::TRIAGE_DIRECTION_ID)`
    /// call that feeds it) from `direction_candidates` and this goes red.
    #[tokio::test]
    async fn direction_candidates_excludes_the_triage_direction() {
        let store = Sin90Store::open_memory().await.unwrap();
        let real = store
            .create_direction("Work", "2026-Q4", None)
            .await
            .unwrap();

        let reader = store.ai_reader();
        let candidates = reader.direction_candidates(50).await.unwrap();
        let ids: Vec<String> = candidates.iter().map(|c| c.direction_id.clone()).collect();
        assert!(
            ids.contains(&real.id),
            "a normal, non-terminal Direction must still be a candidate: {ids:?}"
        );
        assert!(
            !ids.contains(&crate::core::TRIAGE_DIRECTION_ID.to_string()),
            "the reserved 待定 Direction must NEVER be offered as a candidate: {ids:?}"
        );
    }

    /// L5 (2026-09-24 review, round 3): `ALL_TASK_STATUSES`/`ALL_DIRECTION_
    /// STATUSES`'s doc comments claim a new terminal variant "fails loudly"
    /// (fails to compile) if forgotten — but a plain fixed-size array
    /// literal is NOT re-checked by the compiler when the source enum grows
    /// a variant, so that claim was aspirational until an EXHAUSTIVE match
    /// (no wildcard arm) existed somewhere to back it. These two functions
    /// are that backing: a new `TaskStatus`/`DirectionStatus` variant added
    /// to `core::types` without a matching arm here fails to COMPILE
    /// (E0004), not silently leaves `ALL_TASK_STATUSES`/
    /// `ALL_DIRECTION_STATUSES` (and therefore the derived exclusion sets)
    /// stale. Exercised by iterating the `ALL_*` arrays so the test itself
    /// also fails loudly (not just "compiles, never runs") if a variant is
    /// ever REMOVED from an `ALL_*` array without being removed from the
    /// enum.
    #[test]
    fn all_task_statuses_array_is_exhaustive() {
        fn assert_exhaustive(s: TaskStatus) {
            match s {
                TaskStatus::Backlog
                | TaskStatus::Planned
                | TaskStatus::InProgress
                | TaskStatus::Done
                | TaskStatus::Dropped
                | TaskStatus::CarriedOver => {}
            }
        }
        for s in ALL_TASK_STATUSES {
            assert_exhaustive(s);
        }
    }

    #[test]
    fn all_direction_statuses_array_is_exhaustive() {
        fn assert_exhaustive(s: DirectionStatus) {
            match s {
                DirectionStatus::Draft
                | DirectionStatus::Active
                | DirectionStatus::Paused
                | DirectionStatus::Achieved
                | DirectionStatus::Abandoned => {}
            }
        }
        for s in ALL_DIRECTION_STATUSES {
            assert_exhaustive(s);
        }
    }

    /// A fixed-reply `ModelPort`, local to this test — J8's own boundary
    /// snapshot test needs a model but this file (not `ai::classify`'s own
    /// tests) is where it belongs, since it exercises the REAL
    /// `Sin90Store`-as-`AiSink` path end to end.
    struct FixedReply(&'static str);
    impl crate::ai::ModelPort for FixedReply {
        async fn complete(
            &self,
            _req: crate::ai::ModelRequest,
        ) -> Result<crate::ai::ModelReply, crate::ai::ModelFailure> {
            Ok(crate::ai::ModelReply {
                text: self.0.to_string(),
                model_id: Some("test-model".into()),
                tier: crate::ai::ServedTier::Local,
                prompt_tokens: None,
                completion_tokens: None,
            })
        }
    }

    /// M7 (2026-09-24 review): J8's table-snapshot judgement, exercised
    /// through the FULL classify capability (`ai::classify::run_classify`),
    /// not just a hand-built `ProposalDraft` — the produced
    /// `AssignTaskDirection` proposal's `submit` dry-run must still leave
    /// `sin90_tasks` untouched. Mutation target: same as
    /// `ai_boundary_tables_unchanged`'s — make `dry_run` actually commit
    /// instead of rolling back its `SAVEPOINT`, and `sin90_tasks` changes.
    #[tokio::test]
    async fn ai_boundary_tables_unchanged_via_full_classify_run() {
        let store = Sin90Store::open_memory().await.unwrap();
        let _direction = store
            .create_direction("Work", "2026-Q4", None)
            .await
            .unwrap();
        let task = store
            .create_task(
                "Write the doc",
                None,
                None,
                crate::core::TaskKind::Other,
                crate::core::Energy::Mid,
                None,
            )
            .await
            .unwrap();
        let reader = store.ai_reader();
        let pool = store.pool().clone();

        let before = snapshot_all_tables(&pool).await;
        let model = FixedReply(r#"{"choice":"d1","confidence":"high","reason":"fits"}"#);
        let items = crate::ai::classify::run_classify(
            "run-j8-full",
            std::slice::from_ref(&task),
            crate::ai::ModelAccess::LocalOnly,
            Some(&model),
            &store,
            &reader,
        )
        .await;
        assert!(matches!(
            items[0].result,
            crate::ai::classify::ItemResult::Proposed(_)
        ));

        let after = snapshot_all_tables(&pool).await;
        let mut changed = diff_keys(&before, &after);
        changed.sort();
        assert_eq!(
            changed,
            vec![
                "sin90_ai_calls".to_string(),
                "sin90_events(entity=proposal)".to_string(),
                "sin90_proposals".to_string(),
            ],
            "a full classify run's submit must leave sin90_tasks untouched"
        );
        assert_eq!(
            before.get("sin90_tasks"),
            after.get("sin90_tasks"),
            "the produced AssignTaskDirection's dry-run apply must not have actually applied"
        );
    }

    // ---- T5.3.1 review (Layer A, H1): AiReader::weekly_draft agrees with
    // Sin90Store::weekly_draft under the same fixture ------------------------

    /// Same fixture style `weekly_draft_tests` uses (one area/direction with
    /// a completed block, one done task) — `AiReader::weekly_draft`'s
    /// NUMBERS (`by_area`/`by_direction` minutes, `tasks_done`) must agree
    /// EXACTLY with `Sin90Store::weekly_draft`'s own numbers for the same
    /// week, since both now go through the SAME `weekly_draft_on`
    /// (2026-09-26 review H1: "数字只有一份来源"). Mutation target: hand-edit
    /// `AiReader::weekly_draft` to call some OTHER query for `tasks_done`
    /// (e.g. a raw `COUNT(*)` over `sin90_tasks` instead of going through
    /// `weekly_draft_on`'s event replay) — this test goes red the moment
    /// the two diverge.
    #[tokio::test]
    async fn ai_reader_weekly_draft_matches_sin90_store_weekly_draft_same_fixture() {
        use crate::ai::AiReadModel;
        use crate::core::{Energy, ScheduleBlockStatus, TaskKind, TaskStatus};

        let store = Sin90Store::open_memory().await.unwrap();
        let area = store.create_area("Work").await.unwrap();
        let direction = store
            .create_direction("Coding", "this-quarter", Some(&area.id))
            .await
            .unwrap();
        let in_week = "2026-09-22T09:00:00Z"; // 2026-W39
        let block = store
            .create_block(Some(&direction.id), None, 90)
            .await
            .unwrap();
        store
            .transition_block(&block.id, ScheduleBlockStatus::Started)
            .await
            .unwrap();
        store
            .transition_block(&block.id, ScheduleBlockStatus::Completed)
            .await
            .unwrap();
        crate::store::test_hooks::set_last_event_at(&store, "block", &block.id, in_week)
            .await
            .unwrap();
        let task = store
            .create_task("t", None, None, TaskKind::Other, Energy::Mid, None)
            .await
            .unwrap();
        for to in [
            TaskStatus::Planned,
            TaskStatus::InProgress,
            TaskStatus::Done,
        ] {
            store.transition_task(&task.id, to).await.unwrap();
        }
        crate::store::test_hooks::set_last_event_at(&store, "task", &task.id, in_week)
            .await
            .unwrap();

        let from_store = store.weekly_draft("2026-W39").await.unwrap();
        let reader = store.ai_reader();
        let from_reader = AiReadModel::weekly_draft(&reader, "2026-W39")
            .await
            .unwrap();

        assert_eq!(from_reader.week, from_store.week);
        assert_eq!(from_reader.tasks_done, from_store.tasks_done);
        assert_eq!(
            from_reader
                .by_direction
                .iter()
                .map(|b| b.minutes)
                .sum::<i64>(),
            from_store
                .by_direction
                .iter()
                .map(|b| b.minutes)
                .sum::<i64>(),
        );
        assert_eq!(
            from_reader.by_area.iter().map(|b| b.minutes).sum::<i64>(),
            from_store.by_area.iter().map(|b| b.minutes).sum::<i64>(),
        );
        assert_eq!(
            from_reader.by_direction.len(),
            from_store.by_direction.len()
        );
        assert_eq!(from_reader.by_area.len(), from_store.by_area.len());
        // Positive control: the reader's OWN label resolution actually ran
        // (not vacuously equal because both sides ended up empty).
        assert_eq!(from_reader.by_direction[0].label.as_deref(), Some("Coding"));
        assert_eq!(from_reader.by_direction[0].minutes, 90);
    }

    /// 2026-09-26 review (Low): a non-empty direction/area id whose title
    /// lookup comes back `None` falls back to the RAW ID, not the "no
    /// direction/area" `UNASSIGNED_LABEL` `ai::summarize::facts` reserves
    /// for a genuinely EMPTY id. Exercised here (not at the `ai::summarize`
    /// unit level) because it needs a real row deleted out from under a
    /// still-referenced id — `resolve_label`'s own contract, not `facts`'s.
    #[tokio::test]
    async fn ai_reader_weekly_draft_label_falls_back_to_id_when_title_lookup_misses() {
        use crate::ai::AiReadModel;
        use crate::core::ScheduleBlockStatus;

        let store = Sin90Store::open_memory().await.unwrap();
        let direction = store
            .create_direction("Temp", "this-quarter", None)
            .await
            .unwrap();
        let in_week = "2026-09-22T09:00:00Z"; // 2026-W39
        let block = store
            .create_block(Some(&direction.id), None, 30)
            .await
            .unwrap();
        store
            .transition_block(&block.id, ScheduleBlockStatus::Started)
            .await
            .unwrap();
        store
            .transition_block(&block.id, ScheduleBlockStatus::Completed)
            .await
            .unwrap();
        crate::store::test_hooks::set_last_event_at(&store, "block", &block.id, in_week)
            .await
            .unwrap();
        // Delete the direction row out from under its own (still event
        // -referenced) id — no production route can do this, but the
        // fallback must not assume it is impossible. The block row is
        // deleted first (its own FK references the direction); the
        // NUMBERS this test checks come entirely from `sin90_events`
        // (`weekly_draft_on`'s own event-replay discipline), so the live
        // block row disappearing changes nothing about them.
        sqlx::query("DELETE FROM sin90_schedule_blocks WHERE id = ?")
            .bind(&block.id)
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("DELETE FROM sin90_directions WHERE id = ?")
            .bind(&direction.id)
            .execute(store.pool())
            .await
            .unwrap();

        let reader = store.ai_reader();
        let draft = AiReadModel::weekly_draft(&reader, "2026-W39")
            .await
            .unwrap();
        assert_eq!(
            draft.by_direction[0].label.as_deref(),
            Some(direction.id.as_str()),
            "must fall back to the raw id, not None/UNASSIGNED_LABEL: {:?}",
            draft.by_direction[0]
        );
        // Positive control: `ai_reader_weekly_draft_matches_sin90_store_
        // weekly_draft_same_fixture` (above) is this same fallback's
        // negative space — an UNDELETED direction resolves its real title
        // ("Coding"), not its id, proving this assertion isn't just always
        // true regardless of what `resolve_label` does.
    }
}
