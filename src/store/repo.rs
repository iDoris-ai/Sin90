//! Sin90 repository: direct entity writes + the CAS-idempotent Proposal apply.
//!
//! Every method that changes a status runs inside `BEGIN IMMEDIATE`, reads the
//! current status under the write lock, checks the `core::transitions`
//! matrix, updates, and appends a self-contained event — all in one tx.
//!
//! Ported from Agent24's `agent24-sin90-store` (design §1.3/§3.3). New in this
//! port: `Area` CRUD, `Task` direct-write CRUD (`create_task`/`list_tasks`/
//! `transition_task`), `list_events` (M0's `GET /events`), and the
//! `CreateArea`/`CreateTask` arms of `apply_op` + the widened `DbSnapshot`.

use std::collections::HashMap;

use crate::core::{
    body_sha256, canonical_iso_date, canonical_iso_week, check_alloc, check_area_transition,
    check_review_transition, check_rhythm_transition, check_routine_transition,
    check_schedule_block_transition, check_task_transition, check_week_transition, now_iso8601,
    review_is_terminal, routine_is_terminal, ulid, validate, validate_cron, validate_tz,
    week_is_open, Alloc, Area, AreaStatus, Direction, DirectionStatus, Energy, FireTrigger,
    NewReview, NewRoutine, ProposalSource, ProposalStatus, Review, ReviewKind, ReviewSnap,
    ReviewStatus, Rhythm, RhythmStatus, Routine, RoutinePatch, RoutineStatus, ScheduleBlock,
    ScheduleBlockStatus, Sin90Op, Sin90Proposal, Task, TaskKind, TaskStatus, ValidationCtx, Week,
    WeekStatus,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::json;
use sqlx::{Row, Sqlite, Transaction};

use crate::store::{Result, Sin90Store, StoreError, WeekAttention};

/// Receipt of a successful (or idempotently-replayed) proposal apply.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AppliedProposal {
    pub proposal_id: String,
    /// Ids of events appended by this apply, in order.
    pub event_ids: Vec<String>,
}

/// Outcome of [`Sin90Store::apply_proposal`]. `applied_now` distinguishes a real
/// apply (CAS claimed pending → applied) from an idempotent REPLAY of an already
/// applied proposal — so callers only broadcast a `proposal.applied` event on a
/// real apply, never on a retried accept (the receipt is idempotent; the
/// notification must be too).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyOutcome {
    pub receipt: AppliedProposal,
    pub applied_now: bool,
}

/// Outcome of [`Sin90Store::update_routine`] (T3.1.2 review). `changed` names
/// exactly the fields the patch actually moved (same list `update_routine`'s
/// own `updated` event payload carries) — empty when the patch was a no-op
/// (absent, or re-stating the current values), in which case no row was
/// written and no internal `sin90_events` row was appended either. A caller
/// mirroring this out to another audience (`http::update_routine` -> its
/// `EventSink`, T3.1.2) MUST gate its own emit on `changed` being non-empty,
/// so the mirror never emits more (or fewer) events than the store itself did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutineUpdate {
    pub routine: Routine,
    pub changed: Vec<&'static str>,
}

/// Outcome of [`Sin90Store::update_review_body`] (T4.1.1) — same convention
/// as [`RoutineUpdate`], but `changed` is a bare `bool`: a [`ReviewPatch`]
/// only ever has one field (`body`), so there is nothing to name. `false`
/// means the patch was a no-op (`body` re-stated the current value) — no
/// row was written and no `sin90_events` row was appended; a caller
/// mirroring this to a second audience (`http::update_review`) MUST gate its
/// own emit on `changed`, same rule `RoutineUpdate`'s doc states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewUpdate {
    pub review: Review,
    pub changed: bool,
}

/// Outcome of [`Sin90Store::record_routine_fire`] (T3.2.2). Every variant
/// maps to an HTTP 2xx in `http::scheduler_fired` — spec.md M3 "fired" and
/// Agent24's `ME4-S1-scheduler-callback.md` §4.1 both require it: the kernel
/// treats a non-2xx as a failed delivery and retries, and none of these
/// outcomes are a delivery failure — a duplicate is the kernel's OWN retry
/// working as designed, and an unknown/retired routine is Sin90's local
/// bookkeeping drift (an orphan the reconciler, T3.3.2, will clean up later),
/// not something rejecting the request fixes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutineFireOutcome {
    /// First time this `fire_id` was seen: a `sin90_routine_fires` row was
    /// inserted and a `routine.fired` event appended, in the same tx.
    /// `auto_review` is `Some` when this fire ALSO caused a weekly draft
    /// Review to be auto-created (T4.3.2: `Routine{kind:review}` fired and
    /// no `(weekly, <that ISO week>)` Review existed yet) — `None` for
    /// every other Routine kind, a duplicate fire (never reaches this
    /// variant), or a review-kind fire whose week already had a Review.
    Recorded {
        routine_id: String,
        auto_review: Option<AutoReviewCreated>,
    },
    /// This `fire_id` was already recorded — the kernel's at-least-once
    /// retry of the same due slot (design §4.1/§4.2: same slot, same
    /// `fire_id`). No new row, no new event (spec.md M3: "重复投递幂等返回
    /// 2xx") — and, per the same idempotency rule, no auto-review draft
    /// either: T4.3.2 only acts on a REAL new fire.
    Duplicate { routine_id: String },
    /// `key` is not shaped `routine.<id>`, or no such routine exists.
    UnknownKey,
    /// The routine exists but has already reached `retired` — a schedule the
    /// kernel had not yet finished tearing down when it fired one last time.
    RoutineRetired { routine_id: String },
}

/// T4.3.2: the weekly draft Review a `Routine{kind:review}` fire
/// auto-created, so `http::scheduler_fired` can mirror `review.created` to
/// `EventSink` for it too — the same "internal `sin90_events` row + sink
/// mirror" split every other direct-write handler in this codebase follows
/// (`http::create_review`'s doc). Only ever carries a `weekly` Review today
/// (this task's scope — see [`Sin90Store::record_routine_fire`]'s doc for
/// why daily/rhythm are out of scope), so `kind` isn't a field here; the
/// caller hardcodes `"weekly"`/`"draft"` the same way this module's own
/// `insert_weekly_review_draft_if_absent` does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoReviewCreated {
    pub review_id: String,
    pub period: String,
}

/// A persisted proposal as returned by the read endpoints — the stored row plus
/// its apply receipt (present only once `applied`). `ops` is re-inflated to typed
/// form so a reader reconciles exactly what was proposed, not opaque JSON text.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StoredProposal {
    pub id: String,
    pub status: ProposalStatus,
    pub source: ProposalSource,
    pub ops: Vec<Sin90Op>,
    pub rationale: Option<String>,
    pub created_at: String,
    pub decided_at: Option<String>,
    pub result: Option<AppliedProposal>,
}

/// One row of `GET /events` (M0 §6, judgement A10) — the same self-contained
/// shape `sin90_events` stores, so a reader never has to join a mutable table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EventRow {
    pub seq: i64,
    pub id: String,
    pub entity: String,
    pub entity_id: String,
    pub kind: String,
    pub from_state: Option<String>,
    pub to_state: Option<String>,
    pub payload: serde_json::Value,
    pub at: String,
}

/// Row → [`StoredProposal`], shared by the list and the by-id read.
fn row_to_proposal(r: &sqlx::sqlite::SqliteRow) -> Result<StoredProposal> {
    let result: Option<String> = r.get("result");
    Ok(StoredProposal {
        id: r.get("id"),
        status: from_wire(&r.get::<String, _>("status"))?,
        source: from_wire(&r.get::<String, _>("source"))?,
        ops: serde_json::from_str(&r.get::<String, _>("ops"))?,
        rationale: r.get("rationale"),
        created_at: r.get("created_at"),
        decided_at: r.get("decided_at"),
        result: result.map(|s| serde_json::from_str(&s)).transpose()?,
    })
}

// A unit-variant enum serializes to a JSON string; unwrap that to the wire text.
pub(crate) fn to_wire<T: Serialize>(v: &T) -> Result<String> {
    match serde_json::to_value(v)? {
        serde_json::Value::String(s) => Ok(s),
        other => Ok(other.to_string()),
    }
}

pub(crate) fn from_wire<T: DeserializeOwned>(s: &str) -> Result<T> {
    Ok(serde_json::from_value(serde_json::Value::String(
        s.to_string(),
    ))?)
}

/// Shared row→`Task` mapping for the read paths that select the full column
/// list (`list_tasks`, `today_view`'s three task queries) — introduced with
/// `today_view` so a fourth near-identical `SELECT ... FROM sin90_tasks`
/// projection didn't mean a fourth copy of this mapping.
pub(crate) fn row_to_task(r: sqlx::sqlite::SqliteRow) -> Result<Task> {
    Ok(Task {
        id: r.get("id"),
        direction_id: r.get("direction_id"),
        week_id: r.get("week_id"),
        parent_task_id: r.get("parent_task_id"),
        title: r.get("title"),
        status: from_wire(&r.get::<String, _>("status"))?,
        kind: from_wire(&r.get::<String, _>("kind"))?,
        energy: from_wire(&r.get::<String, _>("energy"))?,
        est_minutes: r.get::<Option<i64>, _>("est_minutes").map(|m| m as u32),
        carried_from: r.get("carried_from"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    })
}

/// Shared row→`Rhythm` mapping for `list_rhythms`/`get_rhythm`/`create_rhythm`'s
/// read-back — `allocations` is stored as a JSON `TEXT` column (migration
/// 0001), so this is the one place that (de)serializes it.
fn row_to_rhythm(r: sqlx::sqlite::SqliteRow) -> Result<Rhythm> {
    Ok(Rhythm {
        id: r.get("id"),
        status: from_wire(&r.get::<String, _>("status"))?,
        allocations: serde_json::from_str(&r.get::<String, _>("allocations"))?,
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    })
}

/// Shared row→`Routine` mapping (M3, design §2 #6, §3.2) for `create_routine`/
/// `get_routine`/`list_routines`/`update_routine`/`transition_routine`, all of
/// which `SELECT` the same full column list.
fn row_to_routine(r: sqlx::sqlite::SqliteRow) -> Result<Routine> {
    Ok(Routine {
        id: r.get("id"),
        area_id: r.get("area_id"),
        direction_id: r.get("direction_id"),
        title: r.get("title"),
        kind: from_wire(&r.get::<String, _>("kind"))?,
        cron: r.get("cron"),
        tz: r.get("tz"),
        target_count: r
            .get::<Option<i64>, _>("target_count")
            .map(|n| i64_to_u32(n, "target_count"))
            .transpose()?,
        target_minutes: r
            .get::<Option<i64>, _>("target_minutes")
            .map(|n| i64_to_u32(n, "target_minutes"))
            .transpose()?,
        status: from_wire(&r.get::<String, _>("status"))?,
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    })
}

const ROUTINE_COLUMNS: &str = "id, area_id, direction_id, title, kind, cron, tz, \
     target_count, target_minutes, status, created_at, updated_at";

/// Shared row→`Review` mapping (T4.1.1, design §2/§3.2/§4.1) for
/// `create_review`/`get_review`/`list_reviews`/`update_review_body`/
/// `finalize_review`, all of which `SELECT` the same full column list.
pub(crate) fn row_to_review(r: sqlx::sqlite::SqliteRow) -> Result<Review> {
    Ok(Review {
        id: r.get("id"),
        kind: from_wire(&r.get::<String, _>("kind"))?,
        status: from_wire(&r.get::<String, _>("status"))?,
        week_id: r.get("week_id"),
        period: r.get("period"),
        body: r.get("body"),
        body_ref: r.get("body_ref"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    })
}

pub(crate) const REVIEW_COLUMNS: &str =
    "id, kind, status, week_id, period, body, body_ref, created_at, updated_at";

/// L3 (T3.1.1 review): an `i64` column value read back as `u32` goes through
/// a checked conversion, not `as u32` — `as` silently truncates/wraps on
/// overflow (e.g. a stray negative value would `as`-cast to a huge positive
/// `u32` instead of erroring). A value this ever fails on is a broken
/// invariant in `sin90.db` itself (this column is `CHECK (... > 0)` and only
/// ever written from a `u32` in the first place), not the caller's fault —
/// hence [`StoreError::Internal`], not [`StoreError::Invalid`].
fn i64_to_u32(n: i64, field: &str) -> Result<u32> {
    u32::try_from(n).map_err(|_| StoreError::Internal(format!("{field} out of u32 range: {n}")))
}

/// `target_count`/`target_minutes` must be positive when present (migration
/// 0004's `CHECK` is the backstop; checked here first for a clean
/// [`StoreError::Invalid`] instead of a raw SQLite constraint error — same
/// convention as `create_week`'s `iso_week` check). Shared with
/// `update_routine` (feat/t3.1.1-routine-store, layered on top of this
/// branch) — defined here because `create_routine` needs it first.
fn check_positive_target(value: Option<u32>, field: &str) -> Result<()> {
    if let Some(v) = value {
        if v == 0 {
            return Err(StoreError::Invalid(format!(
                "{field} must be > 0 when present, got 0"
            )));
        }
    }
    Ok(())
}

/// Format-validate a [`NewReview`]/[`Review`]`.period` against its `kind`
/// (T4.1.1, design §2/§3.2/§4.1): returns the CANONICAL form (same
/// normalize-then-validate convention [`canonical_iso_week`] already uses
/// for `Week.iso_week`) or a clean [`StoreError::Invalid`]. This is a PURE
/// check — no I/O — so it does not confirm a `rhythm` period actually
/// references an existing [`crate::core::RhythmId`]; that is a relational
/// check `create_review` does separately, under its write transaction (same
/// split `create_routine` uses for `area_id`/`direction_id` vs.
/// `validate_cron`/`validate_tz`).
fn validate_review_period(kind: ReviewKind, period: &str) -> Result<String> {
    match kind {
        ReviewKind::Daily => canonical_iso_date(period).ok_or_else(|| {
            StoreError::Invalid(format!(
                "daily review period must be a calendar date like 2026-09-24, got {period:?}"
            ))
        }),
        ReviewKind::Weekly => canonical_iso_week(period).ok_or_else(|| {
            StoreError::Invalid(format!(
                "weekly review period must be an ISO-8601 week like 2026-W39, got {period:?}"
            ))
        }),
        ReviewKind::Rhythm => {
            let trimmed = period.trim();
            if trimmed.is_empty() {
                return Err(StoreError::Invalid(
                    "rhythm review period must be a non-empty rhythm id".into(),
                ));
            }
            Ok(trimmed.to_string())
        }
    }
}

/// Crockford Base32 — the exact alphabet [`crate::core::ulid`] emits, and
/// therefore the only charset a `rhythm` Review's `period` (a `RhythmId`,
/// itself always a `ulid()`) can legitimately hold.
const ULID_ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Compute the `body_ref` a finalized Review's Markdown export gets — a path
/// RELATIVE TO `data_dir` (T4.2.1, design §2 #11/§4.2/spec.md M4):
/// `reviews/<kind>/<period>.md`.
///
/// `period` reaches `finalize_review` already validated once, by
/// [`validate_review_period`] at `create_review` time (daily/weekly: a
/// canonical calendar-date/ISO-week string that structurally cannot contain
/// a path separator; rhythm: confirmed to equal a real `sin90_rhythms.id`,
/// which is always a [`crate::core::ulid`]). This function does NOT lean on
/// that history — it independently re-validates `period` as a single safe
/// filename component before it is allowed anywhere near a filesystem path,
/// because that is the only thing standing between a future bug elsewhere
/// (a new caller of `write_markdown_atomic`, a relaxed `validate_review_period`,
/// a hand-crafted DB row) and a path-traversal write. Rejects a `period`
/// that:
///   - is empty, is exactly `.` or `..`, or starts with `.`
///   - contains `/`, `\`, `..` (as a substring — catches `a..b` too), or a
///     NUL byte
///
/// and, for `rhythm` specifically, additionally whitelists the charset to
/// [`ULID_ALPHABET`] (belt-and-suspenders: even a period that passed the
/// generic checks above must still look like an actual ulid).
fn review_markdown_relpath(kind: ReviewKind, period: &str) -> Result<String> {
    let unsafe_component = period.is_empty()
        || period == "."
        || period == ".."
        || period.starts_with('.')
        || period.contains('/')
        || period.contains('\\')
        || period.contains("..")
        || period.contains('\0');
    if unsafe_component {
        return Err(StoreError::Invalid(format!(
            "review period {period:?} is not a safe filename component"
        )));
    }
    if matches!(kind, ReviewKind::Rhythm) && !period.bytes().all(|b| ULID_ALPHABET.contains(&b)) {
        return Err(StoreError::Invalid(format!(
            "rhythm review period {period:?} is not a valid rhythm id"
        )));
    }
    // `kind`'s wire form is a fixed enum value ("daily"/"weekly"/"rhythm"),
    // never user input — nothing to validate there.
    let kind_str = to_wire(&kind)?;
    Ok(format!("reviews/{kind_str}/{period}.md"))
}

/// Write `body` to `<data_dir>/<relpath>` atomically (T4.2.1): a temp file in
/// the SAME directory as the final path (so the final `rename` is a same-
/// filesystem rename, atomic on every OS Sin90 ships on — never true across
/// filesystems/mount points), fsync'd before the rename, and the containing
/// directory is fsync'd after the rename so the rename itself survives a
/// crash. Creates `<data_dir>/reviews/<kind>/` if it does not exist yet.
///
/// Deliberately synchronous `std::fs`, not `tokio::fs`: this runs inside
/// `finalize_review`'s critical section (BEFORE the DB transaction commits —
/// see that function's doc), and Sin90's whole Review surface is low-volume,
/// human-paced writes, not a hot path — spawning a blocking task here would
/// be complexity with no payoff.
fn write_markdown_atomic(
    data_dir: &std::path::Path,
    relpath: &str,
    body: &str,
) -> std::io::Result<()> {
    use std::io::Write;

    let full_path = data_dir.join(relpath);
    let dir = full_path.parent().expect(
        "review_markdown_relpath always returns `reviews/<kind>/<period>.md`, which has a parent",
    );
    std::fs::create_dir_all(dir)?;

    let file_name = full_path
        .file_name()
        .expect("review_markdown_relpath always ends in `<period>.md`");
    let tmp_path = dir.join(format!(
        ".{}.tmp-{}",
        file_name.to_string_lossy(),
        crate::core::ulid()
    ));

    let write_result = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }

    std::fs::rename(&tmp_path, &full_path)?;

    // Best-effort directory fsync so the rename itself is durable, not just
    // the file's contents — a `File::open` on a directory is valid on the
    // Unix targets Sin90 ships on. Not fatal if it fails (e.g. an odd
    // filesystem that refuses to open a directory as a file): the rename
    // already landed, so the export is not lost, only its durability
    // guarantee against a same-instant crash is weaker.
    if let Ok(dir_handle) = std::fs::File::open(dir) {
        let _ = dir_handle.sync_all();
    }

    Ok(())
}

/// `GET /today`'s response shape (design M1). Every field is a plain read —
/// see [`Sin90Store::today_view`] for the selection rules behind each one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct TodayView {
    pub must_do: Vec<Task>,
    pub deep_block: Option<ScheduleBlock>,
    pub inbox: Vec<Task>,
    pub carry_over_candidates: Vec<Task>,
    /// New (T3.2.2, spec.md M3 "`/today` 增「今日到点的 Routine」段"): every
    /// `Routine` with at least one `sin90_routine_fires` row whose
    /// `received_at` falls on today's UTC calendar day (same day-boundary
    /// rule `carry_over_candidates` above uses), deduplicated so a routine
    /// that fired more than once today appears exactly once.
    pub fired_routines: Vec<Routine>,
}

pub(crate) type Tx<'a> = Transaction<'a, Sqlite>;

// One low-level append; the fixed event-row shape is clearer as positional args
// than a throwaway builder struct.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn append_event(
    tx: &mut Tx<'_>,
    entity: &str,
    entity_id: &str,
    kind: &str,
    from_state: Option<&str>,
    to_state: Option<&str>,
    payload: &serde_json::Value,
    at: &str,
) -> Result<String> {
    let id = ulid();
    sqlx::query(
        "INSERT INTO sin90_events (id, entity, entity_id, kind, from_state, to_state, payload, at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(entity)
    .bind(entity_id)
    .bind(kind)
    .bind(from_state)
    .bind(to_state)
    .bind(serde_json::to_string(payload)?)
    .bind(at)
    .execute(&mut **tx)
    .await?;
    Ok(id)
}

/// T4.3.2: ensure a `(weekly, week)` Review exists, on the SAME transaction
/// [`Sin90Store::record_routine_fire`] already holds — genuinely atomic
/// against a concurrent call for the same routine (that other call's own
/// `BEGIN IMMEDIATE` simply waits for this one to commit first, same as
/// every other writer in this module) and against `create_review` (its
/// `UNIQUE(kind, period)` check-then-insert is likewise gated by the SAME
/// write lock). Returns `None` — inserting nothing, appending no event — if
/// a Review already exists for `(weekly, week)`, draft OR finalized: this
/// is a "make sure one exists" operation, not a create-or-error one, so an
/// existing FINALIZED review for the week is left alone rather than
/// clobbered or reported as a conflict (unlike `create_review`'s own 409,
/// which is for a human explicitly asking to create one).
async fn insert_weekly_review_draft_if_absent(
    tx: &mut Tx<'_>,
    week: &str,
    body: &str,
    now: &str,
) -> Result<Option<AutoReviewCreated>> {
    let taken = sqlx::query("SELECT 1 FROM sin90_reviews WHERE kind = 'weekly' AND period = ?")
        .bind(week)
        .fetch_optional(&mut **tx)
        .await?;
    if taken.is_some() {
        return Ok(None);
    }

    let id = ulid();
    sqlx::query(
        "INSERT INTO sin90_reviews (id, kind, status, week_id, period, body, created_at, updated_at)
         VALUES (?, 'weekly', 'draft', NULL, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(week)
    .bind(body)
    .bind(now)
    .bind(now)
    .execute(&mut **tx)
    .await?;

    append_event(
        tx,
        "review",
        &id,
        "created",
        None,
        Some("draft"),
        &json!({
            "review_id": id, "kind": "weekly", "period": week, "status": "draft",
            "source": "routine",
        }),
        now,
    )
    .await?;

    Ok(Some(AutoReviewCreated {
        review_id: id,
        period: week.to_string(),
    }))
}

/// Slugify a title for `sin90_areas.slug` (unique, URL-safe). Lowercase ASCII
/// alnum kept, everything else collapsed to a single `-`; a numeric suffix is
/// appended by the caller only on collision (kept simple: M0 does not need a
/// slug editor).
fn slugify(title: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("area");
    }
    out
}

/// Collision-free slug under the caller's write transaction: the base slug,
/// then `-2`, `-3`, ... Shared by direct `create_area` and the Proposal
/// `CreateArea` op so both paths agree on collisions (the proposal path used
/// to insert the bare slug and fail on the UNIQUE column).
async fn allocate_area_slug(tx: &mut Tx<'_>, title: &str) -> Result<String> {
    let base_slug = slugify(title);
    let mut slug = base_slug.clone();
    let mut n = 2u32;
    loop {
        let exists: i64 = sqlx::query("SELECT COUNT(*) AS n FROM sin90_areas WHERE slug = ?")
            .bind(&slug)
            .fetch_one(&mut **tx)
            .await?
            .get("n");
        if exists == 0 {
            return Ok(slug);
        }
        slug = format!("{base_slug}-{n}");
        n += 1;
    }
}

/// `sin90_outbox.dedup_key` for a Routine's kernel-scheduler side effect
/// (spec.md M3 "outbox": `dedup_key = routine:<id>`, note the colon — the
/// KERNEL key inside `desired.key` uses a dot instead, see
/// [`routine_outbox_upsert_desired`]/[`routine_outbox_delete_desired`]).
fn routine_dedup_key(id: &str) -> String {
    format!("routine:{id}")
}

/// The `desired` payload for a `scheduler.upsert` outbox row mirroring a
/// Routine's active/paused state into the kernel scheduler (spec.md M3):
/// `{key: "routine.<id>", spec: {cron, tz}, enabled}`. `enabled` is `true`
/// for `active`, `false` for `paused` — `retired` never calls this, it calls
/// [`routine_outbox_delete_desired`] instead.
fn routine_outbox_upsert_desired(
    id: &str,
    cron: &str,
    tz: &str,
    enabled: bool,
) -> serde_json::Value {
    json!({
        "key": format!("routine.{id}"),
        "spec": {"cron": cron, "tz": tz},
        "enabled": enabled,
    })
}

/// The `desired` payload for a `scheduler.delete` outbox row (Routine
/// reached `retired`, spec.md M3): just the kernel key, nothing left to
/// describe.
fn routine_outbox_delete_desired(id: &str) -> serde_json::Value {
    json!({"key": format!("routine.{id}")})
}

/// T3.3.1 (design §2 #15, §4.1; spec.md M3 "outbox"; architecture.md #2:
/// "写内核的副作用只经 `sin90_outbox` 幂等对账，不跨库两阶段提交"): idempotently
/// upsert ONE outbox row for `dedup_key`, inside the CALLER's transaction —
/// never opens or commits its own, so a rollback of the caller's write (e.g.
/// a Routine `UPDATE`/event `INSERT` that fails later in the same tx) rolls
/// this back too.
///
/// - A `pending` OR `failed` row for this `dedup_key` is overwritten in
///   place: `kind`/`desired` become the caller's latest values, `status`
///   resets to `pending`, `attempts` resets to `0`, and the three
///   failure-bookkeeping columns are cleared. This is what makes N rapid
///   changes to the same Routine collapse to exactly one row holding the
///   LATEST desired state — never a queue of stale intents — and what makes
///   a `failed` row recover on the Routine's next change rather than being
///   stuck forever (spec.md "错误处理": "Routine 下次变更时重置为 pending").
/// - If the only existing row for this `dedup_key` is `done` (or there is no
///   row at all), a NEW row is inserted. The `done` row is left alone as a
///   historical record — it is never resurrected — so `done_at` keeps
///   meaning "this exact desired state was confirmed landed onto the
///   kernel", not "some earlier, possibly-different state was".
async fn upsert_outbox(
    tx: &mut Tx<'_>,
    kind: &str,
    dedup_key: &str,
    desired: &serde_json::Value,
) -> Result<()> {
    let existing: Option<String> = sqlx::query(
        "SELECT id FROM sin90_outbox WHERE dedup_key = ? AND status IN ('pending', 'failed')
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(dedup_key)
    .fetch_optional(&mut **tx)
    .await?
    .map(|r| r.get::<String, _>("id"));

    let desired_str = serde_json::to_string(desired)?;
    if let Some(id) = existing {
        sqlx::query(
            "UPDATE sin90_outbox
             SET kind = ?, desired = ?, status = 'pending', attempts = 0,
                 failure_kind = NULL, last_error = NULL, next_attempt_at = NULL
             WHERE id = ?",
        )
        .bind(kind)
        .bind(&desired_str)
        .bind(&id)
        .execute(&mut **tx)
        .await?;
    } else {
        let id = ulid();
        let now = now_iso8601();
        sqlx::query(
            "INSERT INTO sin90_outbox (id, kind, dedup_key, desired, status, created_at, done_at)
             VALUES (?, ?, ?, ?, 'pending', ?, NULL)",
        )
        .bind(&id)
        .bind(kind)
        .bind(dedup_key)
        .bind(&desired_str)
        .bind(&now)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

impl Sin90Store {
    // ----- Area (new, design §2 #1) -------------------------------------------

    pub async fn create_area(&self, title: &str) -> Result<Area> {
        let id = ulid();
        let now = now_iso8601();
        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;
        let slug = allocate_area_slug(&mut tx, title).await?;
        sqlx::query(
            "INSERT INTO sin90_areas (id, title, slug, status, sort_key, created_at, updated_at)
             VALUES (?, ?, ?, 'active', 0, ?, ?)",
        )
        .bind(&id)
        .bind(title)
        .bind(&slug)
        .bind(&now)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        append_event(
            &mut tx,
            "area",
            &id,
            "created",
            None,
            Some("active"),
            &json!({"id": id, "title": title, "slug": slug, "status": "active"}),
            &now,
        )
        .await?;
        tx.commit().await?;
        Ok(Area {
            id,
            title: title.to_string(),
            slug,
            status: AreaStatus::Active,
            sort_key: 0,
            created_at: now.clone(),
            updated_at: now,
        })
    }

    pub async fn list_areas(&self) -> Result<Vec<Area>> {
        let rows = sqlx::query(
            "SELECT id, title, slug, status, sort_key, created_at, updated_at
             FROM sin90_areas
             ORDER BY created_at DESC, rowid DESC",
        )
        .fetch_all(self.pool())
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(Area {
                    id: r.get("id"),
                    title: r.get("title"),
                    slug: r.get("slug"),
                    status: from_wire(&r.get::<String, _>("status"))?,
                    sort_key: r.get("sort_key"),
                    created_at: r.get("created_at"),
                    updated_at: r.get("updated_at"),
                })
            })
            .collect()
    }

    /// Transition an Area (`active <-> archived`, design §3.2 — both edges
    /// legal). Direct write: archiving/reactivating a life area is a human UI
    /// action per §7.1's "direct-write is for humans" convention, same as
    /// `transition_block`/`transition_task`.
    pub async fn transition_area(&self, id: &str, to: AreaStatus) -> Result<Area> {
        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query(
            "SELECT title, slug, status, sort_key, created_at FROM sin90_areas WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Err(StoreError::NotFound(format!("area {id}")));
        };
        let from: AreaStatus = from_wire(&row.get::<String, _>("status"))?;
        check_area_transition(from, to)?;
        let now = now_iso8601();
        let to_str = to_wire(&to)?;
        sqlx::query("UPDATE sin90_areas SET status = ?, updated_at = ? WHERE id = ?")
            .bind(&to_str)
            .bind(&now)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        append_event(
            &mut tx,
            "area",
            id,
            "transitioned",
            Some(&to_wire(&from)?),
            Some(&to_str),
            &json!({"area_id": id}),
            &now,
        )
        .await?;
        tx.commit().await?;
        Ok(Area {
            id: id.to_string(),
            title: row.get("title"),
            slug: row.get("slug"),
            status: to,
            sort_key: row.get("sort_key"),
            created_at: row.get("created_at"),
            updated_at: now,
        })
    }

    // ----- direct entity creation (user/planner actions, not AI proposals) ---

    pub async fn create_direction(
        &self,
        title: &str,
        target_window: &str,
        area_id: Option<&str>,
    ) -> Result<Direction> {
        let id = ulid();
        let now = now_iso8601();
        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "INSERT INTO sin90_directions (id, area_id, title, status, target_window, created_at, updated_at)
             VALUES (?, ?, ?, 'draft', ?, ?, ?)",
        )
        .bind(&id)
        .bind(area_id)
        .bind(title)
        .bind(target_window)
        .bind(&now)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        append_event(
            &mut tx,
            "direction",
            &id,
            "created",
            None,
            Some("draft"),
            &json!({"id": id, "area_id": area_id, "title": title, "status": "draft", "target_window": target_window}),
            &now,
        )
        .await?;
        tx.commit().await?;
        Ok(Direction {
            id,
            area_id: area_id.map(str::to_string),
            title: title.to_string(),
            status: DirectionStatus::Draft,
            target_window: target_window.to_string(),
            created_at: now.clone(),
            updated_at: now,
        })
    }

    /// `POST /rhythms` (spec.md "Rhythm 路由", T3.4.1) — direct write, human
    /// gate: creating a Rhythm is a planning-ritual action, same convention as
    /// `create_area`/`create_direction`/`create_week`.
    ///
    /// Structural checks (non-empty, pct range, duplicate direction, sum <=
    /// 100) are `core::check_alloc` — the SAME function `AdjustRhythm`'s pure
    /// `validate` calls (Opus 2026-09-23 review H1: the two paths used to
    /// each carry their own, divergent copy) — run BEFORE the transaction so
    /// a structurally bad request never takes the write lock, and mapped to
    /// `Invalid` (400) here instead of the `Proposal` variant `?` would give
    /// (422, the AdjustRhythm path's code — direct writes use 400 for a
    /// validation failure everywhere else in this file).
    ///
    /// Direction existence is checked INSIDE the transaction by
    /// `require_directions_exist` — again the SAME helper `AdjustRhythm`'s
    /// apply arm calls — and reported as `NotFound` (404), consistent with
    /// `create_task`/`create_block`'s "referenced entity does not exist" 404
    /// convention (Opus 2026-09-23 review M2; this used to be a 400 here,
    /// diverging from that convention).
    ///
    /// Adjusting an existing Rhythm does NOT go through this store method —
    /// only `Sin90Op::AdjustRhythm` via `POST /proposals` + human accept does
    /// (design §7.1's proposal gate; T3.4.1 adds no new Op and no direct
    /// adjust route).
    pub async fn create_rhythm(&self, allocations: &[Alloc]) -> Result<Rhythm> {
        check_alloc(allocations).map_err(|e| StoreError::Invalid(e.to_string()))?;

        let id = ulid();
        let now = now_iso8601();
        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;
        require_directions_exist(&mut tx, allocations).await?;
        sqlx::query(
            "INSERT INTO sin90_rhythms (id, status, allocations, created_at, updated_at)
             VALUES (?, 'active', ?, ?, ?)",
        )
        .bind(&id)
        .bind(serde_json::to_string(allocations)?)
        .bind(&now)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        append_event(
            &mut tx,
            "rhythm",
            &id,
            "created",
            None,
            Some("active"),
            &json!({"id": id, "status": "active", "allocations": allocations}),
            &now,
        )
        .await?;
        tx.commit().await?;
        Ok(Rhythm {
            id,
            status: RhythmStatus::Active,
            allocations: allocations.to_vec(),
            created_at: now.clone(),
            updated_at: now,
        })
    }

    pub async fn create_week(&self, iso_week: &str) -> Result<Week> {
        let iso_week = crate::core::canonical_iso_week(iso_week).ok_or_else(|| {
            StoreError::Invalid(format!(
                "iso_week must be an ISO-8601 week like 2026-W42, got {iso_week:?}"
            ))
        })?;
        let iso_week = iso_week.as_str();
        let id = ulid();
        let now = now_iso8601();
        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;
        // One Week per calendar week. Checked here for a clean 409; the unique
        // index (migration 0003) is the backstop.
        let taken = sqlx::query("SELECT 1 FROM sin90_weeks WHERE iso_week = ?")
            .bind(iso_week)
            .fetch_optional(&mut *tx)
            .await?;
        if taken.is_some() {
            return Err(StoreError::Conflict(format!(
                "week {iso_week} already exists"
            )));
        }
        sqlx::query(
            "INSERT INTO sin90_weeks (id, status, iso_week, created_at, updated_at)
             VALUES (?, 'planning', ?, ?, ?)",
        )
        .bind(&id)
        .bind(iso_week)
        .bind(&now)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        append_event(
            &mut tx,
            "week",
            &id,
            "created",
            None,
            Some("planning"),
            &json!({"id": id, "iso_week": iso_week, "status": "planning"}),
            &now,
        )
        .await?;
        tx.commit().await?;
        Ok(Week {
            id,
            status: WeekStatus::Planning,
            iso_week: iso_week.to_string(),
            created_at: now.clone(),
            updated_at: now,
        })
    }

    pub async fn list_weeks(&self) -> Result<Vec<Week>> {
        let rows = sqlx::query(
            "SELECT id, status, iso_week, created_at, updated_at
             FROM sin90_weeks
             ORDER BY created_at DESC, rowid DESC",
        )
        .fetch_all(self.pool())
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(Week {
                    id: r.get("id"),
                    status: from_wire(&r.get::<String, _>("status"))?,
                    iso_week: r.get("iso_week"),
                    created_at: r.get("created_at"),
                    updated_at: r.get("updated_at"),
                })
            })
            .collect()
    }

    /// Transition a Week through `planning -> active -> reviewing -> closed`
    /// (design §1.1 — the state machine and `week_is_open`/terminal checks
    /// were already ported and tested in M0; M2 is the first thing to call
    /// this transition, planning/reviewing a week is a human weekly-ritual
    /// action, same convention as `transition_area`/`transition_task`).
    pub async fn transition_week(&self, id: &str, to: WeekStatus) -> Result<Week> {
        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query("SELECT status, iso_week, created_at FROM sin90_weeks WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(row) = row else {
            return Err(StoreError::NotFound(format!("week {id}")));
        };
        let from: WeekStatus = from_wire(&row.get::<String, _>("status"))?;
        check_week_transition(from, to)?;
        let now = now_iso8601();
        let to_str = to_wire(&to)?;
        sqlx::query("UPDATE sin90_weeks SET status = ?, updated_at = ? WHERE id = ?")
            .bind(&to_str)
            .bind(&now)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        append_event(
            &mut tx,
            "week",
            id,
            "transitioned",
            Some(&to_wire(&from)?),
            Some(&to_str),
            &json!({"week_id": id}),
            &now,
        )
        .await?;
        tx.commit().await?;
        Ok(Week {
            id: id.to_string(),
            status: to,
            iso_week: row.get("iso_week"),
            created_at: row.get("created_at"),
            updated_at: now,
        })
    }

    /// Planned-vs-actual for one Week (design M2 acceptance line). Two
    /// independently-sourced numbers, deliberately not computed by the same
    /// query:
    ///
    /// - `planned_min`: a LIVE query — "what is currently planned for this
    ///   week" is, by definition, a statement about current linkage
    ///   (`sin90_schedule_blocks.task_id -> sin90_tasks.week_id`), not a
    ///   historical fact to replay. Reordering or adding blocks to the week
    ///   changes what "planned" means going forward; that's the intended
    ///   behavior, not drift.
    /// - `actual_min`: pure event replay, same discipline as
    ///   [`Sin90Store::attention`] above — reads only `sin90_events.payload`,
    ///   never joins the mutable `sin90_schedule_blocks`/`sin90_tasks` tables.
    ///   That is WHY [`Sin90Store::transition_block`] now snapshots `week_id`
    ///   into the completion event payload (a block's `task_id`, and that
    ///   task's `week_id`, do not change after creation — see
    ///   `direction_title` above for the established precedent of snapshotting
    ///   a value specifically so a later edit or deletion can't rewrite what
    ///   already happened).
    pub async fn week_attention(&self, week_id: &str) -> Result<WeekAttention> {
        // One read transaction = one database snapshot: without it a block
        // created/completed between the queries could pair a stale plan with
        // a fresh actual.
        let mut tx = self.pool().begin().await?;
        let exists = sqlx::query("SELECT 1 FROM sin90_weeks WHERE id = ?")
            .bind(week_id)
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_none() {
            return Err(StoreError::NotFound(format!("week {week_id}")));
        }
        let planned_min: i64 = sqlx::query(
            "SELECT COALESCE(SUM(b.planned_minutes), 0) AS m
             FROM sin90_schedule_blocks b
             JOIN sin90_tasks t ON t.id = b.task_id
             WHERE t.week_id = ?",
        )
        .bind(week_id)
        .fetch_one(&mut *tx)
        .await?
        .get("m");
        let actual_min: i64 = sqlx::query(
            "SELECT COALESCE(SUM(json_extract(payload,'$.minutes')), 0) AS m
             FROM sin90_events
             WHERE entity = 'block' AND kind = 'transitioned' AND to_state = 'completed'
               AND json_extract(payload,'$.week_id') = ?",
        )
        .bind(week_id)
        .fetch_one(&mut *tx)
        .await?
        .get("m");
        tx.commit().await?;
        Ok(WeekAttention {
            week_id: week_id.to_string(),
            planned_min,
            actual_min,
            deviation_min: actual_min - planned_min,
        })
    }

    pub async fn create_block(
        &self,
        direction_id: Option<&str>,
        task_id: Option<&str>,
        planned_minutes: u32,
    ) -> Result<ScheduleBlock> {
        let id = ulid();
        let now = now_iso8601();
        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "INSERT INTO sin90_schedule_blocks
                 (id, direction_id, task_id, status, planned_minutes, created_at, updated_at)
             VALUES (?, ?, ?, 'planned', ?, ?, ?)",
        )
        .bind(&id)
        .bind(direction_id)
        .bind(task_id)
        .bind(planned_minutes as i64)
        .bind(&now)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        append_event(
            &mut tx,
            "block",
            &id,
            "created",
            None,
            Some("planned"),
            &json!({"id": id, "direction_id": direction_id, "planned_minutes": planned_minutes}),
            &now,
        )
        .await?;
        tx.commit().await?;
        Ok(ScheduleBlock {
            id,
            direction_id: direction_id.map(str::to_string),
            task_id: task_id.map(str::to_string),
            status: ScheduleBlockStatus::Planned,
            planned_minutes,
            created_at: now.clone(),
            updated_at: now,
        })
    }

    /// Transition a block; a `completed` transition appends a SELF-CONTAINED
    /// event carrying the direction snapshot + minutes + occurred_at, so the
    /// attention replay never has to join the mutable blocks/directions tables.
    pub async fn transition_block(
        &self,
        id: &str,
        to: ScheduleBlockStatus,
    ) -> Result<ScheduleBlock> {
        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query(
            "SELECT status, planned_minutes, direction_id, task_id, created_at
             FROM sin90_schedule_blocks WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Err(StoreError::NotFound(format!("schedule_block {id}")));
        };
        let from: ScheduleBlockStatus = from_wire(&row.get::<String, _>("status"))?;
        check_schedule_block_transition(from, to)?;

        let minutes: i64 = row.get("planned_minutes");
        let direction_id: Option<String> = row.get("direction_id");
        let task_id: Option<String> = row.get("task_id");
        let created_at: String = row.get("created_at");
        let now = now_iso8601();
        let to_wire_str = to_wire(&to)?;

        sqlx::query("UPDATE sin90_schedule_blocks SET status = ?, updated_at = ? WHERE id = ?")
            .bind(&to_wire_str)
            .bind(&now)
            .bind(id)
            .execute(&mut *tx)
            .await?;

        let direction_title: Option<String> = match &direction_id {
            Some(did) => sqlx::query("SELECT title FROM sin90_directions WHERE id = ?")
                .bind(did)
                .fetch_optional(&mut *tx)
                .await?
                .map(|r| r.get::<String, _>("title")),
            None => None,
        };
        // (design M2) Snapshot the task's `week_id` at transition time, same
        // reason `direction_title` is snapshotted above: `Sin90Store::
        // week_attention`'s `actual_min` must replay purely from
        // `sin90_events.payload`, never joining the live (mutable)
        // `sin90_tasks` table — a task's `week_id` does not change after
        // creation (see `Task::parent_task_id`'s doc comment for the same
        // "set once" property), so this snapshot cannot drift from the join
        // it stands in for.
        let week_id: Option<String> = match &task_id {
            Some(tid) => sqlx::query("SELECT week_id FROM sin90_tasks WHERE id = ?")
                .bind(tid)
                .fetch_optional(&mut *tx)
                .await?
                .and_then(|r| r.get::<Option<String>, _>("week_id")),
            None => None,
        };
        append_event(
            &mut tx,
            "block",
            id,
            "transitioned",
            Some(&to_wire(&from)?),
            Some(&to_wire_str),
            &json!({
                "block_id": id,
                "direction_id": direction_id,
                "week_id": week_id,
                "direction_title": direction_title,
                "minutes": minutes,
                "occurred_at": now,
            }),
            &now,
        )
        .await?;
        tx.commit().await?;
        Ok(ScheduleBlock {
            id: id.to_string(),
            direction_id,
            task_id,
            status: to,
            planned_minutes: minutes as u32,
            created_at,
            updated_at: now,
        })
    }

    // ----- Task direct-write CRUD (new, M0 §6 A6/A7/A9) ------------------------

    /// Direct-write task creation (human UI path). `parent_task_id` makes this
    /// a "Project" child (design §2 #3) — nesting is constrained to exactly one
    /// level: a `parent_task_id` that itself already has a parent is rejected.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_task(
        &self,
        title: &str,
        direction_id: Option<&str>,
        parent_task_id: Option<&str>,
        kind: TaskKind,
        energy: Energy,
        est_minutes: Option<u32>,
    ) -> Result<Task> {
        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;
        if let Some(parent_id) = parent_task_id {
            let parent_row = sqlx::query("SELECT parent_task_id FROM sin90_tasks WHERE id = ?")
                .bind(parent_id)
                .fetch_optional(&mut *tx)
                .await?;
            match parent_row {
                None => return Err(StoreError::NotFound(format!("task {parent_id}"))),
                Some(r) => {
                    let grandparent: Option<String> = r.get("parent_task_id");
                    if grandparent.is_some() {
                        return Err(StoreError::Conflict(format!(
                            "task {parent_id} already has a parent; nesting is limited to one level"
                        )));
                    }
                }
            }
        }
        let id = ulid();
        let now = now_iso8601();
        sqlx::query(
            "INSERT INTO sin90_tasks
                 (id, direction_id, week_id, parent_task_id, title, status, kind, energy,
                  est_minutes, sort_key, carried_from, created_at, updated_at)
             VALUES (?, ?, NULL, ?, ?, 'backlog', ?, ?, ?, 0, NULL, ?, ?)",
        )
        .bind(&id)
        .bind(direction_id)
        .bind(parent_task_id)
        .bind(title)
        .bind(to_wire(&kind)?)
        .bind(to_wire(&energy)?)
        .bind(est_minutes.map(|m| m as i64))
        .bind(&now)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        append_event(
            &mut tx,
            "task",
            &id,
            "created",
            None,
            Some("backlog"),
            &json!({"id": id, "direction_id": direction_id, "parent_task_id": parent_task_id, "title": title}),
            &now,
        )
        .await?;
        tx.commit().await?;
        Ok(Task {
            id,
            direction_id: direction_id.map(str::to_string),
            week_id: None,
            parent_task_id: parent_task_id.map(str::to_string),
            title: title.to_string(),
            status: TaskStatus::Backlog,
            kind,
            energy,
            est_minutes,
            carried_from: None,
            created_at: now.clone(),
            updated_at: now,
        })
    }

    /// Direct-write task transition (human UI path), through the same
    /// `check_task_transition` matrix the Proposal path uses — an illegal
    /// transition (e.g. `done -> backlog`) is rejected here identically
    /// (M0 §6 judgement A8), and produces NO event on rejection (the whole
    /// transaction rolls back before `append_event` runs).
    pub async fn transition_task(&self, id: &str, to: TaskStatus) -> Result<Task> {
        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query(
            "SELECT direction_id, week_id, parent_task_id, title, status, kind, energy,
                    est_minutes, carried_from, created_at
             FROM sin90_tasks WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Err(StoreError::NotFound(format!("task {id}")));
        };
        // Same invariant the Proposal path enforces: a task in a reviewing or
        // closed week is history and must not change through either path.
        require_task_week_open(&mut tx, id).await?;
        let from: TaskStatus = from_wire(&row.get::<String, _>("status"))?;
        check_task_transition(from, to)?;

        let now = now_iso8601();
        let to_str = to_wire(&to)?;
        sqlx::query("UPDATE sin90_tasks SET status = ?, updated_at = ? WHERE id = ?")
            .bind(&to_str)
            .bind(&now)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        append_event(
            &mut tx,
            "task",
            id,
            "transitioned",
            Some(&to_wire(&from)?),
            Some(&to_str),
            &json!({"task_id": id}),
            &now,
        )
        .await?;
        tx.commit().await?;
        Ok(Task {
            id: id.to_string(),
            direction_id: row.get("direction_id"),
            week_id: row.get("week_id"),
            parent_task_id: row.get("parent_task_id"),
            title: row.get("title"),
            status: to,
            kind: from_wire(&row.get::<String, _>("kind"))?,
            energy: from_wire(&row.get::<String, _>("energy"))?,
            est_minutes: row.get::<Option<i64>, _>("est_minutes").map(|m| m as u32),
            carried_from: row.get("carried_from"),
            created_at: row.get("created_at"),
            updated_at: now,
        })
    }

    /// `GET /tasks[?direction_id=&area_id=&status=]` (M0 §6 A9). `area_id`
    /// filters via a join through `sin90_directions` — a task has no `area_id`
    /// column of its own (design §3.1: Area sits above Direction, not beside
    /// Task).
    pub async fn list_tasks(
        &self,
        direction_id: Option<&str>,
        area_id: Option<&str>,
        status: Option<TaskStatus>,
    ) -> Result<Vec<Task>> {
        // Built as one parameterized query with optional predicates rather than
        // string-concatenating filter values — every value stays bound.
        let mut sql = String::from(
            "SELECT t.id, t.direction_id, t.week_id, t.parent_task_id, t.title, t.status,
                    t.kind, t.energy, t.est_minutes, t.carried_from, t.created_at, t.updated_at
             FROM sin90_tasks t",
        );
        if area_id.is_some() {
            sql.push_str(" JOIN sin90_directions d ON d.id = t.direction_id");
        }
        sql.push_str(" WHERE 1=1");
        if direction_id.is_some() {
            sql.push_str(" AND t.direction_id = ?");
        }
        if area_id.is_some() {
            sql.push_str(" AND d.area_id = ?");
        }
        if status.is_some() {
            sql.push_str(" AND t.status = ?");
        }
        sql.push_str(" ORDER BY t.created_at DESC, t.rowid DESC");

        let mut q = sqlx::query(&sql);
        if let Some(d) = direction_id {
            q = q.bind(d);
        }
        if let Some(a) = area_id {
            q = q.bind(a);
        }
        if let Some(s) = status {
            q = q.bind(to_wire(&s)?);
        }
        let rows = q.fetch_all(self.pool()).await?;
        rows.into_iter().map(row_to_task).collect()
    }

    /// `GET /today` (design M1). Four sections, every one a plain read — this
    /// method itself never writes, so `/today` "does not land a third table"
    /// (M1's acceptance line) is true of the whole call, not just the response
    /// shape: no row, temp table, or cache is created to answer it.
    ///
    /// The selection rules below are M1's own judgment calls, not anything the
    /// design doc pins down further than "3 things" / "90-minute block" /
    /// "carry-over candidates" — Task has no priority or due-date column yet
    /// (that's out of M1's scope: adding one would be a data-model change, and
    /// M1 is a read-only view over what M0 already stores), so every rule here
    /// is built only from `status` and `created_at`:
    ///
    /// - **must-do (≤3)**: tasks that already have a `direction_id` (an inbox
    ///   item isn't "must-do" until a human puts it under a Direction),
    ///   ordered `in_progress` first (finish what's started before starting
    ///   more), then `planned`, then `backlog`; oldest-first within each tier
    ///   so a task doesn't rot at the bottom of an ever-growing backlog.
    /// - **deep block**: the single oldest still-`planned` `ScheduleBlock`
    ///   (FIFO). `ScheduleBlock` has no "which day" column in M0's schema, so
    ///   this is deliberately NOT "today's block" in the calendar sense — it's
    ///   "the next one queued" — until M3's `Routine`/scheduling work gives
    ///   blocks a day to belong to.
    /// - **inbox**: `direction_id IS NULL` tasks not yet `done`/`dropped`
    ///   (M1's "not-yet-classified" bucket, oldest first).
    /// - **carry-over candidates**: `in_progress` tasks (WITH a direction —
    ///   distinct from inbox) whose most recent move INTO `in_progress` (from
    ///   the event log; `created_at` only if no such event exists) happened
    ///   before the start of the user's local day — started on an earlier
    ///   day and still open, so a human
    ///   should decide whether to keep pushing, drop, or (M2) actually
    ///   `CarryOverTask` it into next week.
    pub async fn today_view(&self) -> Result<TodayView> {
        let today_start = crate::core::local_day_start_utc();
        // All four sections from one read transaction (one snapshot), so a
        // concurrent transition can't show a task in two sections at once.
        let mut tx = self.pool().begin().await?;

        let must_do = sqlx::query(
            "SELECT id, direction_id, week_id, parent_task_id, title, status, kind, energy,
                    est_minutes, carried_from, created_at, updated_at
             FROM sin90_tasks
             WHERE direction_id IS NOT NULL
               AND status IN ('in_progress', 'planned', 'backlog')
             ORDER BY
               CASE status
                 WHEN 'in_progress' THEN 0
                 WHEN 'planned' THEN 1
                 ELSE 2
               END,
               created_at ASC
             LIMIT 3",
        )
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(row_to_task)
        .collect::<Result<Vec<_>>>()?;

        let deep_block = sqlx::query(
            "SELECT id, direction_id, task_id, status, planned_minutes, created_at, updated_at
             FROM sin90_schedule_blocks
             WHERE status = 'planned'
             ORDER BY created_at ASC
             LIMIT 1",
        )
        .fetch_optional(&mut *tx)
        .await?
        .map(|r| {
            Ok::<_, StoreError>(ScheduleBlock {
                id: r.get("id"),
                direction_id: r.get("direction_id"),
                task_id: r.get("task_id"),
                status: from_wire(&r.get::<String, _>("status"))?,
                planned_minutes: r.get::<i64, _>("planned_minutes") as u32,
                created_at: r.get("created_at"),
                updated_at: r.get("updated_at"),
            })
        })
        .transpose()?;

        let inbox = sqlx::query(
            "SELECT id, direction_id, week_id, parent_task_id, title, status, kind, energy,
                    est_minutes, carried_from, created_at, updated_at
             FROM sin90_tasks
             WHERE direction_id IS NULL
               AND status NOT IN ('done', 'dropped')
             ORDER BY created_at ASC",
        )
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(row_to_task)
        .collect::<Result<Vec<_>>>()?;

        let carry_over_candidates = sqlx::query(
            "SELECT id, direction_id, week_id, parent_task_id, title, status, kind, energy,
                    est_minutes, carried_from, created_at, updated_at
             FROM sin90_tasks t
             WHERE direction_id IS NOT NULL
               AND status = 'in_progress'
               AND COALESCE(
                     (SELECT MAX(e.at) FROM sin90_events e
                      WHERE e.entity = 'task' AND e.entity_id = t.id
                        AND e.to_state = 'in_progress'),
                     t.created_at) < ?
             ORDER BY created_at ASC",
        )
        .bind(&today_start)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(row_to_task)
        .collect::<Result<Vec<_>>>()?;

        // T3.2.2: routines with a `sin90_routine_fires` row received today
        // (>= same UTC-day boundary `carry_over_candidates` uses above).
        // `DISTINCT` over the full routine column set collapses a routine
        // that fired more than once today to one row — every matching join
        // row carries identical routine columns, so `DISTINCT` here is
        // exactly "group by routine.id" without a separate GROUP BY clause.
        let fired_routines = sqlx::query(&format!(
            "SELECT DISTINCT {cols} FROM sin90_routine_fires f
             JOIN sin90_routines r ON r.id = f.routine_id
             WHERE f.received_at >= ?
             ORDER BY r.title ASC",
            cols = ROUTINE_COLUMNS
                .split(", ")
                .map(|c| format!("r.{c}"))
                .collect::<Vec<_>>()
                .join(", ")
        ))
        .bind(&today_start)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(row_to_routine)
        .collect::<Result<Vec<_>>>()?;

        tx.commit().await?;
        Ok(TodayView {
            must_do,
            deep_block,
            inbox,
            carry_over_candidates,
            fired_routines,
        })
    }

    // ----- Routine create/get/list (new, M3, design §2 #6, §3.2, T3.1.1) -----
    //
    // `create` is a direct write (human UI path, same convention as
    // Area/Task/Week above — M0 §6's "direct-write is for humans, Proposal
    // is for AI" split; no `Sin90Op::CreateRoutine` variant exists, on
    // purpose, until a real AI use case needs one). It runs inside
    // `BEGIN IMMEDIATE` and appends exactly one self-contained event in the
    // same transaction; `get`/`list` never open a write transaction and
    // never call `append_event`, so `cargo test routine_` can assert "get/
    // list are zero-event" as the positive control for "every mutation is
    // exactly one event". `update`/`transition` (status changes and field
    // edits) live in feat/t3.1.1-routine-store, layered on top of this
    // branch — this branch is the read-plus-create half of the split.

    /// Create a Routine from a [`NewRoutine`] (T3.1.1 review, "M2" — was a
    /// long positional-argument list before). `tz` defaults to `"UTC"` when
    /// absent; an EXPLICIT empty string is rejected rather than silently
    /// treated as "use the default" (L2 — an empty string is more likely a
    /// caller bug than an intentional default). `title` is trimmed and
    /// rejected if blank (L4). `area_id`/`direction_id` existence is checked
    /// explicitly (not left to the FK violation) so a bad reference comes
    /// back as a clean [`StoreError::NotFound`] rather than a raw SQLite
    /// error.
    pub async fn create_routine(&self, new: &NewRoutine) -> Result<Routine> {
        let title = new.title.trim();
        if title.is_empty() {
            return Err(StoreError::Invalid("title must not be blank".into()));
        }
        let tz: &str = match &new.tz {
            None => "UTC",
            Some(t) if t.is_empty() => {
                return Err(StoreError::Invalid(
                    "tz must not be an empty string (omit the field to default to UTC)".into(),
                ))
            }
            Some(t) => t.as_str(),
        };
        validate_cron(&new.cron).map_err(StoreError::Invalid)?;
        validate_tz(tz).map_err(StoreError::Invalid)?;
        check_positive_target(new.target_count, "target_count")?;
        check_positive_target(new.target_minutes, "target_minutes")?;

        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;
        if let Some(aid) = &new.area_id {
            require_area_exists(&mut tx, aid).await?;
        }
        if let Some(did) = &new.direction_id {
            require_direction_exists(&mut tx, did).await?;
        }

        let id = ulid();
        let now = now_iso8601();
        let kind_str = to_wire(&new.kind)?;
        sqlx::query(
            "INSERT INTO sin90_routines
                 (id, area_id, direction_id, title, kind, cron, tz,
                  target_count, target_minutes, status, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'active', ?, ?)",
        )
        .bind(&id)
        .bind(&new.area_id)
        .bind(&new.direction_id)
        .bind(title)
        .bind(&kind_str)
        .bind(&new.cron)
        .bind(tz)
        .bind(new.target_count.map(i64::from))
        .bind(new.target_minutes.map(i64::from))
        .bind(&now)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        // Ad hoc (not a full-entity snapshot — see `update_routine`'s doc,
        // feat/t3.1.1-routine-store, for why `updated` gets one and this
        // doesn't) payload; the id key is `routine_id`, matching every other
        // routine event (T3.1.1 review, "M5" — `created` used to be the one
        // holdout using a bare `id`).
        append_event(
            &mut tx,
            "routine",
            &id,
            "created",
            None,
            Some("active"),
            &json!({
                "routine_id": id, "area_id": new.area_id, "direction_id": new.direction_id,
                "title": title, "kind": kind_str, "cron": new.cron, "tz": tz,
                "target_count": new.target_count, "target_minutes": new.target_minutes,
                "status": "active",
            }),
            &now,
        )
        .await?;
        // T3.3.1: a freshly created Routine is `active`, so the kernel
        // scheduler needs to know about it — same transaction as the row
        // insert and the `created` event above, so a rollback of either of
        // those rolls this back too.
        upsert_outbox(
            &mut tx,
            "scheduler.upsert",
            &routine_dedup_key(&id),
            &routine_outbox_upsert_desired(&id, &new.cron, tz, true),
        )
        .await?;
        tx.commit().await?;
        Ok(Routine {
            id,
            area_id: new.area_id.clone(),
            direction_id: new.direction_id.clone(),
            title: title.to_string(),
            kind: new.kind,
            cron: new.cron.clone(),
            tz: tz.to_string(),
            target_count: new.target_count,
            target_minutes: new.target_minutes,
            status: RoutineStatus::Active,
            created_at: now.clone(),
            updated_at: now,
        })
    }

    /// A read: no transaction, no event. Missing id is [`StoreError::NotFound`].
    pub async fn get_routine(&self, id: &str) -> Result<Routine> {
        let row = sqlx::query(&format!(
            "SELECT {ROUTINE_COLUMNS} FROM sin90_routines WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await?
        .ok_or_else(|| StoreError::NotFound(format!("routine {id}")))?;
        row_to_routine(row)
    }

    /// A read: no transaction, no event.
    pub async fn list_routines(
        &self,
        area_id: Option<&str>,
        direction_id: Option<&str>,
        status: Option<RoutineStatus>,
    ) -> Result<Vec<Routine>> {
        let mut sql = format!("SELECT {ROUTINE_COLUMNS} FROM sin90_routines WHERE 1=1");
        if area_id.is_some() {
            sql.push_str(" AND area_id = ?");
        }
        if direction_id.is_some() {
            sql.push_str(" AND direction_id = ?");
        }
        if status.is_some() {
            sql.push_str(" AND status = ?");
        }
        sql.push_str(" ORDER BY created_at DESC, rowid DESC");

        let mut q = sqlx::query(&sql);
        if let Some(a) = area_id {
            q = q.bind(a);
        }
        if let Some(d) = direction_id {
            q = q.bind(d);
        }
        if let Some(s) = status {
            q = q.bind(to_wire(&s)?);
        }
        let rows = q.fetch_all(self.pool()).await?;
        rows.into_iter().map(row_to_routine).collect()
    }

    // ----- Routine update/transition (feat/t3.1.1-routine-store, layered on
    // feat/t3.1.1b-routine-store-read's create/get/list) ----------------------

    /// Apply a [`RoutinePatch`] (T3.1.1 review, "M2" — was five positional
    /// arguments before). Status changes go through
    /// [`Self::transition_routine`] instead; a patch never touches `status`.
    ///
    /// - **H2**: a `retired` routine is a closed door — rejected with
    ///   [`StoreError::Conflict`] (maps to HTTP 409 once T3.1.2 wires a
    ///   route) before any comparison or write, even one that would
    ///   otherwise be a no-op.
    /// - **L1**: after resolving every field to its post-patch value, it is
    ///   compared field-by-field against the CURRENT row (not against
    ///   whether the patch supplied a value) — a patch that re-states the
    ///   same values, or supplies none at all, writes nothing: no `UPDATE`,
    ///   no event, `updated_at` untouched.
    /// - **L2/L4**: a non-`None` `tz` must not be empty; a non-`None` `title`
    ///   is trimmed and must not be blank — same rules `create_routine`
    ///   enforces.
    /// - **M5**: the `updated` event's payload is the FULL post-update
    ///   snapshot (`serde_json::to_value(&updated)`, not an ad hoc field
    ///   list) plus a `"changed"` array naming which fields actually moved —
    ///   so a reader never has to diff two snapshots to know what happened.
    ///
    /// Returns [`RoutineUpdate`] (T3.1.2 review), not a bare `Routine`: a
    /// caller mirroring this write out to a second audience (T3.1.2's HTTP
    /// layer -> its `EventSink`) needs to know whether anything ACTUALLY
    /// changed, not just "the call returned `Ok`" — a no-op patch returns
    /// `Ok` too (see L1 above), but must not be mistaken for a real write.
    pub async fn update_routine(&self, id: &str, patch: &RoutinePatch) -> Result<RoutineUpdate> {
        let title = match &patch.title {
            Some(t) => {
                let trimmed = t.trim();
                if trimmed.is_empty() {
                    return Err(StoreError::Invalid("title must not be blank".into()));
                }
                Some(trimmed.to_string())
            }
            None => None,
        };
        if let Some(c) = &patch.cron {
            validate_cron(c).map_err(StoreError::Invalid)?;
        }
        if let Some(t) = &patch.tz {
            if t.is_empty() {
                return Err(StoreError::Invalid("tz must not be an empty string".into()));
            }
            validate_tz(t).map_err(StoreError::Invalid)?;
        }
        if let Some(Some(tc)) = patch.target_count {
            check_positive_target(Some(tc), "target_count")?;
        }
        if let Some(Some(tm)) = patch.target_minutes {
            check_positive_target(Some(tm), "target_minutes")?;
        }

        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query(&format!(
            "SELECT {ROUTINE_COLUMNS} FROM sin90_routines WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Err(StoreError::NotFound(format!("routine {id}")));
        };
        let current = row_to_routine(row)?;

        // H2 (T3.1.1 review): retired is terminal for edits too, not just
        // status transitions — checked before the no-op diff below so even a
        // patch that would otherwise change nothing is still refused (the
        // guarantee is "no writes reach a retired routine", not "no
        // meaningful writes do").
        if routine_is_terminal(current.status) {
            return Err(StoreError::Conflict(format!(
                "routine {id} is retired; no further changes are accepted"
            )));
        }

        let new_title = title.as_deref().unwrap_or(&current.title);
        let new_cron = patch.cron.as_deref().unwrap_or(&current.cron);
        let new_tz = patch.tz.as_deref().unwrap_or(&current.tz);
        let new_target_count = patch.target_count.unwrap_or(current.target_count);
        let new_target_minutes = patch.target_minutes.unwrap_or(current.target_minutes);

        // L1: diff against the CURRENT row, not against "did the patch
        // supply this field" — a patch that re-states the current values is
        // just as much a no-op as an empty one.
        let mut changed: Vec<&'static str> = Vec::new();
        if new_title != current.title {
            changed.push("title");
        }
        if new_cron != current.cron {
            changed.push("cron");
        }
        if new_tz != current.tz {
            changed.push("tz");
        }
        if new_target_count != current.target_count {
            changed.push("target_count");
        }
        if new_target_minutes != current.target_minutes {
            changed.push("target_minutes");
        }
        if changed.is_empty() {
            // Dropping `tx` here rolls back the `BEGIN IMMEDIATE` we opened
            // to read `current` — no row, no event, `updated_at` untouched.
            return Ok(RoutineUpdate {
                routine: current,
                changed,
            });
        }

        let now = now_iso8601();
        sqlx::query(
            "UPDATE sin90_routines
             SET title = ?, cron = ?, tz = ?, target_count = ?, target_minutes = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(new_title)
        .bind(new_cron)
        .bind(new_tz)
        .bind(new_target_count.map(i64::from))
        .bind(new_target_minutes.map(i64::from))
        .bind(&now)
        .bind(id)
        .execute(&mut *tx)
        .await?;

        let updated = Routine {
            title: new_title.to_string(),
            cron: new_cron.to_string(),
            tz: new_tz.to_string(),
            target_count: new_target_count,
            target_minutes: new_target_minutes,
            updated_at: now.clone(),
            ..current
        };
        // M5: full snapshot + which fields moved, rather than an ad hoc
        // field list — a reader gets both "what it looks like now" and
        // "what specifically changed" from one event.
        let mut payload = serde_json::to_value(&updated)?;
        if let serde_json::Value::Object(map) = &mut payload {
            map.insert("changed".to_string(), json!(changed));
        }
        append_event(
            &mut tx, "routine", id, "updated", None, None, &payload, &now,
        )
        .await?;
        // T3.3.1: only `cron`/`tz` feed the kernel scheduler's `spec`
        // (design §2 #15) — a `title`/`target_count`/`target_minutes`-only
        // patch changed `changed` above but must NOT touch outbox (that's
        // the whole point of the "title alone doesn't write outbox, cron
        // does" test pair). `enabled` follows the routine's CURRENT status
        // (patches never touch status) — a paused routine whose cron just
        // moved must still land as `enabled: false`, not `true`.
        if changed.contains(&"cron") || changed.contains(&"tz") {
            let enabled = current.status == RoutineStatus::Active;
            upsert_outbox(
                &mut tx,
                "scheduler.upsert",
                &routine_dedup_key(id),
                &routine_outbox_upsert_desired(id, new_cron, new_tz, enabled),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(RoutineUpdate {
            routine: updated,
            changed,
        })
    }

    /// Transition a Routine's status (`active <-> paused`, `{active,paused} ->
    /// retired`, design §3.2 — `retired` is terminal, see
    /// `core::transitions::routine_transition_allowed`). The event `kind` is
    /// the destination-specific name spec.md pins down (`paused`/`resumed`/
    /// `retired`), NOT a generic `"transitioned"` like most other entities —
    /// there is only one legal edge INTO each of those three states, so the
    /// destination alone is unambiguous.
    pub async fn transition_routine(&self, id: &str, to: RoutineStatus) -> Result<Routine> {
        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query(&format!(
            "SELECT {ROUTINE_COLUMNS} FROM sin90_routines WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Err(StoreError::NotFound(format!("routine {id}")));
        };
        let current = row_to_routine(row)?;
        check_routine_transition(current.status, to)?;

        let kind = match to {
            RoutineStatus::Paused => "paused",
            RoutineStatus::Active => "resumed",
            RoutineStatus::Retired => "retired",
        };
        let now = now_iso8601();
        let to_str = to_wire(&to)?;
        sqlx::query("UPDATE sin90_routines SET status = ?, updated_at = ? WHERE id = ?")
            .bind(&to_str)
            .bind(&now)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        append_event(
            &mut tx,
            "routine",
            id,
            kind,
            Some(&to_wire(&current.status)?),
            Some(&to_str),
            &json!({"routine_id": id}),
            &now,
        )
        .await?;
        // T3.3.1: mirror the new status into the kernel-scheduler outbox
        // (design §2 #15, spec.md M3) — `active`/`paused` are both
        // `scheduler.upsert` (only `enabled` flips), `retired` is
        // `scheduler.delete` (a retired Routine is a closed door — nothing
        // left to upsert, `update_routine` already rejects further edits).
        match to {
            RoutineStatus::Active | RoutineStatus::Paused => {
                upsert_outbox(
                    &mut tx,
                    "scheduler.upsert",
                    &routine_dedup_key(id),
                    &routine_outbox_upsert_desired(
                        id,
                        &current.cron,
                        &current.tz,
                        to == RoutineStatus::Active,
                    ),
                )
                .await?;
            }
            RoutineStatus::Retired => {
                upsert_outbox(
                    &mut tx,
                    "scheduler.delete",
                    &routine_dedup_key(id),
                    &routine_outbox_delete_desired(id),
                )
                .await?;
            }
        }
        tx.commit().await?;
        Ok(Routine {
            status: to,
            updated_at: now,
            ..current
        })
    }

    // ----- fired receipt (T3.2.2, design §2 #16, spec.md M3 "fired") ---------

    /// Record one `POST /_a24/scheduler/fired` delivery, idempotently by
    /// `fire_id` (Agent24's delivery contract is at-least-once with the same
    /// `fire_id` shared by every retry of one due slot —
    /// `ME4-S1-scheduler-callback.md` §4.1/§4.2). `key` is the kernel's
    /// schedule key, expected in the `routine.<id>` shape `store::repo`'s own
    /// outbox code mints (see `routine_outbox_upsert_desired`); any other
    /// shape, or an id that doesn't resolve to a `sin90_routines` row, comes
    /// back as [`RoutineFireOutcome::UnknownKey`] — not an error, since it is
    /// this module's bookkeeping that's out of sync, not the kernel's
    /// mistake (the caller must still answer 2xx so the kernel does not
    /// retry a "failure" that isn't one).
    ///
    /// Everything — the routine lookup, the dedup insert, and the event
    /// append — runs inside ONE `BEGIN IMMEDIATE` transaction, so a
    /// concurrent `transition_routine(.., Retired)` can't interleave between
    /// "routine looked active" and "fire recorded": either this call sees
    /// the retirement (and returns [`RoutineFireOutcome::RoutineRetired`]
    /// without writing anything) or it doesn't (and the retirement, which
    /// also takes `BEGIN IMMEDIATE`, waits for this one to finish first).
    ///
    /// No production caller exists on THIS branch (`feat/t3.2.2a-routine-fires-store`)
    /// — only this module's own `fired_tests` (`#[cfg(test)]`) exercise it.
    /// The caller is `http::scheduler_fired`, added by
    /// `feat/t3.2.2-fired-route` stacked on top. Being `pub` on a `pub`
    /// struct already keeps `dead_code` from firing here either way (same
    /// as every other `pub` store method), so the `allow` below changes
    /// nothing functionally — it documents, same convention as
    /// `core::proposal`'s `_AreaIdUsed`, that a caller-less `pub fn` at this
    /// point in the stack is expected, not a mistake.
    ///
    /// **T4.3.2** (spec.md M4 "review Routine 到点自动建草稿"): when the
    /// fired Routine's `kind` is `review`, this ALSO ensures a `weekly`
    /// draft Review exists for the ISO week `scheduled_for` falls in
    /// ([`crate::core::iso_week_of`]) — but only on a genuinely NEW fire
    /// (never on [`RoutineFireOutcome::Duplicate`] or
    /// [`RoutineFireOutcome::RoutineRetired`]), and only if no `(weekly,
    /// that week)` Review exists yet (draft OR finalized — see
    /// `insert_weekly_review_draft_if_absent`'s doc). Only `weekly` is
    /// implemented: Sin90's data model has no field today that would tell
    /// this function a review-kind Routine's fire should instead produce a
    /// `daily` or `rhythm` Review (`Routine.kind` only says "this is a
    /// review routine", not which `Review.kind`/period it feeds) — adding
    /// that distinction is out of this task's scope (see task report), so a
    /// `review`-kind Routine ALWAYS maps to that week's `weekly` draft, no
    /// matter its actual cron cadence.
    ///
    /// **Same-transaction note**: the fire dedup + `routine.fired` event
    /// above, and the review-existence-check + insert below, both run
    /// inside the ONE `BEGIN IMMEDIATE` transaction this method already
    /// opens — genuinely atomic, not a two-step race. What does NOT run in
    /// that transaction is computing the draft's CONTENT (the `by_area`/
    /// `by_direction`/`tasks_done`/`routines` numbers,
    /// [`Sin90Store::weekly_draft`]): that call goes through
    /// `self.pool()` for its own reads, and `Sin90Store::open_memory`
    /// (every test in this crate) pools exactly ONE connection — reusing it
    /// from inside a transaction that already checked that same connection
    /// out would deadlock waiting for a connection that will never free up
    /// before this transaction commits. So the draft's Markdown body is
    /// precomputed BEFORE `tx` opens below, on a separate pool checkout,
    /// and only the cheap "does this period already have a Review" gate +
    /// the `INSERT` run inside the write transaction. Consequence: the
    /// precomputed body can miss an event written in the (normally
    /// sub-millisecond) gap between that precompute and this transaction's
    /// commit — including, notably, THIS very `routine.fired` event, which
    /// is always appended strictly after the precompute runs. Accepted per
    /// this task's brief ("草稿内容可能差最新一两个事件，可接受但要写明"):
    /// the alternative (rewriting `weekly_draft`/`attention` to accept an
    /// arbitrary `sqlx` executor instead of always going through
    /// `self.pool()`) is real surgery on two other files whose sourcing
    /// discipline is already carefully documented and tested (see
    /// `weekly_draft.rs`'s module doc) — out of proportion to what this
    /// task needs, and not required by its own acceptance criteria (which
    /// only asks that the rendered numbers match `WeeklyDraft`'s own
    /// structure, not that the draft be perfectly live).
    #[allow(dead_code)]
    pub async fn record_routine_fire(
        &self,
        fire_id: &str,
        key: &str,
        scheduled_for: &str,
        trigger: FireTrigger,
    ) -> Result<RoutineFireOutcome> {
        let Some(routine_id) = key.strip_prefix("routine.").filter(|rest| !rest.is_empty()) else {
            return Ok(RoutineFireOutcome::UnknownKey);
        };

        // Precompute (see this method's doc for why it must happen BEFORE
        // `tx` opens): only bother if this routine is `kind: review` and
        // `scheduled_for` parses as a fixed-width timestamp. A malformed
        // `scheduled_for` here does not fail the whole fire receipt — it
        // just means no auto-draft this time (the HTTP layer already
        // rejects a malformed `scheduled_for` with 400 before ever calling
        // this method; this fallback only matters for a direct test/store
        // caller that skips that check).
        let review_week: Option<String> = {
            let kind: Option<String> = sqlx::query("SELECT kind FROM sin90_routines WHERE id = ?")
                .bind(routine_id)
                .fetch_optional(self.pool())
                .await?
                .map(|r| r.get::<String, _>("kind"));
            match kind.as_deref() {
                Some("review") => crate::core::iso_week_of(scheduled_for),
                _ => None,
            }
        };
        // Fetch the actual draft content now, still before `tx` opens, on
        // its own separate `self.pool()` checkout (see this method's doc).
        let precomputed_weekly: Option<(String, String)> = match review_week {
            Some(week) => {
                let draft = self.weekly_draft(&week).await?;
                Some((week, crate::store::render_weekly_draft_markdown(&draft)))
            }
            None => None,
        };

        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;

        let row = sqlx::query("SELECT status, kind FROM sin90_routines WHERE id = ?")
            .bind(routine_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(row) = row else {
            return Ok(RoutineFireOutcome::UnknownKey);
        };
        let status: String = row.get("status");
        let kind: String = row.get("kind");

        if status == "retired" {
            return Ok(RoutineFireOutcome::RoutineRetired {
                routine_id: routine_id.to_string(),
            });
        }

        let now = now_iso8601();
        let trigger_str = to_wire(&trigger)?;
        // `ON CONFLICT(fire_id) DO NOTHING RETURNING fire_id`: a fresh
        // `fire_id` inserts and returns its own id (`Some`); a `fire_id`
        // already in the table hits the PRIMARY KEY conflict, inserts
        // nothing, and RETURNING yields zero rows (`None`) — SQLite only
        // returns rows for the INSERT's actual effect, same as
        // `apply_proposal`'s CAS `UPDATE ... RETURNING` above.
        let inserted: Option<String> = sqlx::query(
            "INSERT INTO sin90_routine_fires
                 (fire_id, routine_id, scheduled_for, trigger, received_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(fire_id) DO NOTHING
             RETURNING fire_id",
        )
        .bind(fire_id)
        .bind(routine_id)
        .bind(scheduled_for)
        .bind(&trigger_str)
        .bind(&now)
        .fetch_optional(&mut *tx)
        .await?
        .map(|r| r.get::<String, _>("fire_id"));

        if inserted.is_none() {
            tx.commit().await?;
            return Ok(RoutineFireOutcome::Duplicate {
                routine_id: routine_id.to_string(),
            });
        }

        append_event(
            &mut tx,
            "routine",
            routine_id,
            "fired",
            None,
            None,
            &json!({
                "routine_id": routine_id, "fire_id": fire_id,
                "scheduled_for": scheduled_for, "trigger": trigger_str,
            }),
            &now,
        )
        .await?;

        let mut auto_review = None;
        if kind == "review" {
            if let Some((week, body)) = &precomputed_weekly {
                auto_review =
                    insert_weekly_review_draft_if_absent(&mut tx, week, body, &now).await?;
            }
        }

        tx.commit().await?;
        Ok(RoutineFireOutcome::Recorded {
            routine_id: routine_id.to_string(),
            auto_review,
        })
    }

    // ----- reads (list + detail) ---------------------------------------------

    pub async fn list_directions(&self) -> Result<Vec<Direction>> {
        let rows = sqlx::query(
            "SELECT id, area_id, title, status, target_window, created_at, updated_at
             FROM sin90_directions
             ORDER BY created_at DESC, rowid DESC",
        )
        .fetch_all(self.pool())
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(Direction {
                    id: r.get("id"),
                    area_id: r.get("area_id"),
                    title: r.get("title"),
                    status: from_wire(&r.get::<String, _>("status"))?,
                    target_window: r.get("target_window"),
                    created_at: r.get("created_at"),
                    updated_at: r.get("updated_at"),
                })
            })
            .collect()
    }

    pub async fn list_rhythms(&self) -> Result<Vec<Rhythm>> {
        let rows = sqlx::query(
            "SELECT id, status, allocations, created_at, updated_at
             FROM sin90_rhythms
             ORDER BY created_at DESC, rowid DESC",
        )
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(row_to_rhythm).collect()
    }

    /// One Rhythm by id. A missing id is `NotFound` (→ 404), same shape as
    /// `get_proposal`.
    pub async fn get_rhythm(&self, id: &str) -> Result<Rhythm> {
        let row = sqlx::query(
            "SELECT id, status, allocations, created_at, updated_at
             FROM sin90_rhythms WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?
        .ok_or_else(|| StoreError::NotFound(format!("rhythm {id}")))?;
        row_to_rhythm(row)
    }

    pub async fn list_blocks(&self) -> Result<Vec<ScheduleBlock>> {
        let rows = sqlx::query(
            "SELECT id, direction_id, task_id, status, planned_minutes, created_at, updated_at
             FROM sin90_schedule_blocks
             ORDER BY created_at DESC, rowid DESC",
        )
        .fetch_all(self.pool())
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(ScheduleBlock {
                    id: r.get("id"),
                    direction_id: r.get("direction_id"),
                    task_id: r.get("task_id"),
                    status: from_wire(&r.get::<String, _>("status"))?,
                    planned_minutes: r.get::<i64, _>("planned_minutes") as u32,
                    created_at: r.get("created_at"),
                    updated_at: r.get("updated_at"),
                })
            })
            .collect()
    }

    pub async fn list_proposals(&self) -> Result<Vec<StoredProposal>> {
        let rows = sqlx::query(
            "SELECT id, status, source, ops, rationale, result, created_at, decided_at
             FROM sin90_proposals
             ORDER BY created_at DESC, rowid DESC",
        )
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_proposal).collect()
    }

    /// New (2026-09-24 review, M3): `PENDING`-only, for `http::ai_classify`'s
    /// dedup step — that call site only ever cares about pending proposals,
    /// so filtering in SQL (rather than `list_proposals()` + a Rust-side
    /// `.filter`) avoids loading every applied/rejected proposal ever made
    /// just to throw most of them away.
    pub async fn list_pending_proposals(&self) -> Result<Vec<StoredProposal>> {
        let rows = sqlx::query(
            "SELECT id, status, source, ops, rationale, result, created_at, decided_at
             FROM sin90_proposals
             WHERE status = 'pending'
             ORDER BY created_at DESC, rowid DESC",
        )
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_proposal).collect()
    }

    /// One proposal by id (with its ops and — once applied — the receipt). A
    /// missing id is `NotFound` (→ 404), the same shape the accept path uses.
    pub async fn get_proposal(&self, id: &str) -> Result<StoredProposal> {
        let row = sqlx::query(
            "SELECT id, status, source, ops, rationale, result, created_at, decided_at
             FROM sin90_proposals WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?
        .ok_or_else(|| StoreError::NotFound(format!("proposal {id}")))?;
        row_to_proposal(&row)
    }

    /// `GET /events[?entity=&entity_id=&since_seq=&limit=]` (M0 §6 A10/A11) —
    /// a direct, filtered read of the append-only log. `limit` defaults to 100
    /// and is capped at 500 so an unbounded query can't be used to page the
    /// whole table in one request.
    pub async fn list_events(
        &self,
        entity: Option<&str>,
        entity_id: Option<&str>,
        since_seq: Option<i64>,
        limit: Option<i64>,
    ) -> Result<Vec<EventRow>> {
        let limit = limit.unwrap_or(100).clamp(1, 500);
        let mut sql = String::from(
            "SELECT seq, id, entity, entity_id, kind, from_state, to_state, payload, at
             FROM sin90_events WHERE 1=1",
        );
        if entity.is_some() {
            sql.push_str(" AND entity = ?");
        }
        if entity_id.is_some() {
            sql.push_str(" AND entity_id = ?");
        }
        if since_seq.is_some() {
            sql.push_str(" AND seq > ?");
        }
        sql.push_str(" ORDER BY seq ASC LIMIT ?");

        let mut q = sqlx::query(&sql);
        if let Some(e) = entity {
            q = q.bind(e);
        }
        if let Some(eid) = entity_id {
            q = q.bind(eid);
        }
        if let Some(s) = since_seq {
            q = q.bind(s);
        }
        q = q.bind(limit);
        let rows = q.fetch_all(self.pool()).await?;
        rows.into_iter()
            .map(|r| {
                let payload_str: String = r.get("payload");
                Ok(EventRow {
                    seq: r.get("seq"),
                    id: r.get("id"),
                    entity: r.get("entity"),
                    entity_id: r.get("entity_id"),
                    kind: r.get("kind"),
                    from_state: r.get("from_state"),
                    to_state: r.get("to_state"),
                    payload: serde_json::from_str(&payload_str)?,
                    at: r.get("at"),
                })
            })
            .collect()
    }

    // ----- proposal gate ------------------------------------------------------

    /// Persist a proposal as `pending`. Idempotent on `id` ONLY when the ops are
    /// identical: re-submitting the same id with a DIFFERENT batch is a
    /// `Conflict`, not a silent no-op that would later apply the stale ops the
    /// caller thinks they replaced.
    ///
    /// SFU-10: a genuinely NEW submission is validated against a snapshot of
    /// current state (same `build_snapshot` + `validate` pair `apply_proposal`
    /// uses) BEFORE anything is written — a structurally invalid batch never
    /// lands as a `pending` row; the caller gets 422 (`StoreError::Proposal`)
    /// and the store is untouched. `accept`/`apply_proposal` still re-runs
    /// `validate` against the state AT APPLY TIME: this submit-time check is a
    /// strict, additional gate, not a replacement — state referenced here
    /// (e.g. a week's open/closed status) can legitimately change between
    /// submit and accept, and only the apply-time check is allowed to decide
    /// the outcome that actually commits.
    ///
    /// An idempotent REPLAY (same id, same ops already stored) is intentionally
    /// NOT re-validated here: it was already validated (successfully) the
    /// first time it was submitted, this call writes nothing new, and
    /// re-validating a batch that was already accepted as `pending` against
    /// possibly-drifted state would make a pure idempotency check start
    /// failing for reasons unrelated to the replay itself.
    pub async fn submit_proposal(&self, p: &Sin90Proposal) -> Result<()> {
        let now = now_iso8601();
        let ops_json = serde_json::to_string(&p.ops)?;
        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;

        let existing: Option<String> = sqlx::query("SELECT ops FROM sin90_proposals WHERE id = ?")
            .bind(&p.id)
            .fetch_optional(&mut *tx)
            .await?
            .map(|r| r.get::<String, _>("ops"));

        match existing {
            Some(existing_ops) if existing_ops == ops_json => {
                // Idempotent replay: identical batch already persisted (and
                // already validated, on its first submission). No-op.
                return Ok(());
            }
            Some(_) => {
                return Err(StoreError::Conflict(format!(
                    "proposal {} already exists with different ops",
                    p.id
                )));
            }
            None => {
                let snapshot = build_snapshot(&mut tx, &p.ops).await?;
                validate(p, &snapshot)?; // Err -> tx drops -> rollback -> nothing written
            }
        }

        sqlx::query(
            "INSERT INTO sin90_proposals (id, status, source, ops, rationale, created_at)
             VALUES (?, 'pending', ?, ?, ?, ?)",
        )
        .bind(&p.id)
        .bind(to_wire(&p.source)?)
        .bind(&ops_json)
        .bind(&p.rationale)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        append_event(
            &mut tx,
            "proposal",
            &p.id,
            "submitted",
            None,
            Some("pending"),
            &json!({"id": p.id, "source": to_wire(&p.source)?, "ops_count": p.ops.len()}),
            &now,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Accept + apply a pending proposal in ONE transaction, CAS-idempotent:
    /// `pending → applying` is a compare-and-set; a re-tried accept whose row is
    /// already `applied` returns the stored receipt without re-applying. A
    /// validation or apply error rolls the whole tx back (proposal → pending).
    pub async fn apply_proposal(&self, proposal_id: &str) -> Result<ApplyOutcome> {
        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;

        let claimed = sqlx::query(
            "UPDATE sin90_proposals SET status = 'applying'
             WHERE id = ? AND status = 'pending'
             RETURNING ops, source",
        )
        .bind(proposal_id)
        .fetch_optional(&mut *tx)
        .await?;

        let Some(claimed) = claimed else {
            let existing = sqlx::query("SELECT status, result FROM sin90_proposals WHERE id = ?")
                .bind(proposal_id)
                .fetch_optional(&mut *tx)
                .await?;
            return match existing {
                None => Err(StoreError::NotFound(format!("proposal {proposal_id}"))),
                Some(r) => match r.get::<String, _>("status").as_str() {
                    "applied" => {
                        let result: String =
                            r.get::<Option<String>, _>("result").ok_or_else(|| {
                                StoreError::Internal("applied proposal has no result".into())
                            })?;
                        Ok(ApplyOutcome {
                            receipt: serde_json::from_str(&result)?,
                            applied_now: false,
                        })
                    }
                    "applying" => Err(StoreError::Conflict(format!(
                        "proposal {proposal_id} is already being applied"
                    ))),
                    other => Err(StoreError::Conflict(format!(
                        "proposal {proposal_id} is {other}, not pending"
                    ))),
                },
            };
        };

        let ops: Vec<Sin90Op> = serde_json::from_str(&claimed.get::<String, _>("ops"))?;
        let source = from_wire(&claimed.get::<String, _>("source"))?;

        let snapshot = build_snapshot(&mut tx, &ops).await?;
        let proposal = Sin90Proposal {
            id: proposal_id.to_string(),
            status: crate::core::types::ProposalStatus::Applying,
            source,
            ops: ops.clone(),
            rationale: None,
        };
        validate(&proposal, &snapshot)?; // Err → tx drops → rollback → back to pending

        let mut event_ids = Vec::new();
        for op in &ops {
            apply_op(&mut tx, op, &mut event_ids).await?;
        }

        let receipt = AppliedProposal {
            proposal_id: proposal_id.to_string(),
            event_ids,
        };
        sqlx::query(
            "UPDATE sin90_proposals SET status = 'applied', result = ?, decided_at = ? WHERE id = ?",
        )
        .bind(serde_json::to_string(&receipt)?)
        .bind(now_iso8601())
        .bind(proposal_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(ApplyOutcome {
            receipt,
            applied_now: true,
        })
    }

    // ----- Review (M4, T4.1.1, design §2/§3.2/§4.1) -------------------------
    //
    // Direct writes, human-gated at the HTTP layer (same convention as
    // Area/Task/Week/Routine above) — there is no `Sin90Op::CreateReview` /
    // `FinalizeReview` proposal variant, same reasoning `http::mod`'s Routine
    // section doc gives for Routine. `create`/`update_body`/`finalize` each
    // append exactly one `sin90_events` row per real write, same tx as the
    // row change; a no-op `update_review_body` writes neither (mirrors
    // `update_routine`'s L1 rule).

    /// Create a Review (T4.1.1). `kind`+`period` format is checked first
    /// ([`validate_review_period`], a pure check); a `rhythm` period is then
    /// confirmed to reference an EXISTING [`crate::core::RhythmId`] under the
    /// write transaction ([`read_rhythm_status`], reused from the Rhythm
    /// section below purely for its existence check — the status it returns
    /// is discarded) — an unknown rhythm id comes back as a clean
    /// [`StoreError::NotFound`] (404), not a raw FK violation. `UNIQUE(kind,
    /// period)` (migration 0007) is the backstop; checked explicitly first
    /// (same convention `create_week` uses for `iso_week`) for a clean
    /// [`StoreError::Conflict`] (409) rather than a raw SQLite constraint
    /// error. A fresh Review always starts `draft` with an empty `body` —
    /// [`NewReview`] does not accept one (see that type's doc) — and
    /// `week_id` is always `NULL`: new code never sets it (design §3.2's
    /// note that `period` replaces it as the identity axis).
    pub async fn create_review(&self, new: &NewReview) -> Result<Review> {
        let period = validate_review_period(new.kind, &new.period)?;

        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;
        if matches!(new.kind, ReviewKind::Rhythm) {
            // Existence-only: the status itself is not this check's concern.
            read_rhythm_status(&mut tx, &period).await?;
        }
        let kind_str = to_wire(&new.kind)?;
        let taken = sqlx::query("SELECT 1 FROM sin90_reviews WHERE kind = ? AND period = ?")
            .bind(&kind_str)
            .bind(&period)
            .fetch_optional(&mut *tx)
            .await?;
        if taken.is_some() {
            return Err(StoreError::Conflict(format!(
                "a {kind_str} review for period {period} already exists"
            )));
        }

        let id = ulid();
        let now = now_iso8601();
        sqlx::query(
            "INSERT INTO sin90_reviews (id, kind, status, week_id, period, body, created_at, updated_at)
             VALUES (?, ?, 'draft', NULL, ?, '', ?, ?)",
        )
        .bind(&id)
        .bind(&kind_str)
        .bind(&period)
        .bind(&now)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        append_event(
            &mut tx,
            "review",
            &id,
            "created",
            None,
            Some("draft"),
            &json!({
                "review_id": id, "kind": kind_str, "period": period, "status": "draft",
                "source": "human",
            }),
            &now,
        )
        .await?;
        tx.commit().await?;
        Ok(Review {
            id,
            kind: new.kind,
            status: ReviewStatus::Draft,
            week_id: None,
            period,
            body: String::new(),
            body_ref: None,
            created_at: now.clone(),
            updated_at: now,
        })
    }

    /// A read: no transaction, no event. Missing id is [`StoreError::NotFound`].
    pub async fn get_review(&self, id: &str) -> Result<Review> {
        let row = sqlx::query(&format!(
            "SELECT {REVIEW_COLUMNS} FROM sin90_reviews WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await?
        .ok_or_else(|| StoreError::NotFound(format!("review {id}")))?;
        row_to_review(row)
    }

    /// A read: no transaction, no event. `period` is matched exactly (already
    /// canonical on write, same convention `list_routines`' `status` filter
    /// uses) — a caller filtering by a non-canonical period (e.g. lowercase
    /// `w`) simply gets zero rows, same as any other exact-match filter.
    pub async fn list_reviews(
        &self,
        kind: Option<ReviewKind>,
        period: Option<&str>,
    ) -> Result<Vec<Review>> {
        let mut sql = format!("SELECT {REVIEW_COLUMNS} FROM sin90_reviews WHERE 1=1");
        if kind.is_some() {
            sql.push_str(" AND kind = ?");
        }
        if period.is_some() {
            sql.push_str(" AND period = ?");
        }
        sql.push_str(" ORDER BY created_at DESC, rowid DESC");

        let mut q = sqlx::query(&sql);
        if let Some(k) = kind {
            q = q.bind(to_wire(&k)?);
        }
        if let Some(p) = period {
            q = q.bind(p);
        }
        let rows = q.fetch_all(self.pool()).await?;
        rows.into_iter().map(row_to_review).collect()
    }

    /// Apply a [`ReviewPatch`] (T4.1.1) — the only mutable field is `body`.
    /// - A `finalized` Review is a closed door, same as `update_routine`'s H2
    ///   for `retired`: rejected with [`StoreError::Conflict`] (409) BEFORE
    ///   the no-op comparison below, so even a patch that would otherwise
    ///   change nothing is still refused once finalized.
    /// - The new `body` is compared against the CURRENT row, not against
    ///   "was a value supplied" (there is only one field, so this is simpler
    ///   than `update_routine`'s L1, but the same rule): re-stating the
    ///   current body writes no `UPDATE` and appends no event.
    pub async fn update_review_body(&self, id: &str, body: &str) -> Result<ReviewUpdate> {
        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query(&format!(
            "SELECT {REVIEW_COLUMNS} FROM sin90_reviews WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Err(StoreError::NotFound(format!("review {id}")));
        };
        let current = row_to_review(row)?;

        if review_is_terminal(current.status) {
            return Err(StoreError::Conflict(format!(
                "review {id} is finalized; no further changes are accepted"
            )));
        }

        if body == current.body {
            // Dropping `tx` here rolls back the `BEGIN IMMEDIATE` we opened
            // to read `current` — no row, no event, `updated_at` untouched.
            return Ok(ReviewUpdate {
                review: current,
                changed: false,
            });
        }

        let now = now_iso8601();
        sqlx::query("UPDATE sin90_reviews SET body = ?, updated_at = ? WHERE id = ?")
            .bind(body)
            .bind(&now)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        let updated = Review {
            body: body.to_string(),
            updated_at: now.clone(),
            ..current
        };
        append_event(
            &mut tx,
            "review",
            id,
            "updated",
            None,
            None,
            &json!({"review_id": id, "body": updated.body}),
            &now,
        )
        .await?;
        tx.commit().await?;
        Ok(ReviewUpdate {
            review: updated,
            changed: true,
        })
    }

    /// Transition a Review `draft -> finalized` (design §3.2 — the only
    /// legal edge, [`check_review_transition`]; `finalized` is terminal, so
    /// finalizing twice is a [`crate::core::TransitionError`] -> 409, same
    /// convention every other transition endpoint above uses).
    ///
    /// T4.2.1: finalizing also exports `body` to
    /// `<data_dir>/reviews/<kind>/<period>.md` (design §2 #11/§4.2) and
    /// records the RELATIVE path in `body_ref`. Ordering and failure
    /// semantics (deliberate, not incidental):
    ///
    ///   1. Read + validate the transition under `BEGIN IMMEDIATE` (as
    ///      before) — nothing written yet.
    ///   2. Write the Markdown file atomically ([`write_markdown_atomic`]),
    ///      STILL before any `UPDATE`/event/commit. If this fails, we return
    ///      the error immediately: no `UPDATE` has run, so dropping `tx`
    ///      rolls back to a no-op and the review is untouched — still
    ///      `draft`, `body_ref` still whatever it was, no `finalized` event.
    ///   3. Only once the file is safely on disk do we `UPDATE` the row
    ///      (`status`, `body_ref`) and append the `finalized` event, then
    ///      commit.
    ///
    /// The one acceptable inconsistency this leaves: if step 3's `commit()`
    /// itself fails (disk/IO error at COMMIT, not before), the `.md` file
    /// from step 2 is now orphaned — written, but the row never actually
    /// flipped to `finalized` (SQLite rolls the transaction back when a
    /// commit fails, so `status` reverts to `draft` and `body_ref` reverts to
    /// whatever it was). This is fine: the Markdown file is a ONE-WAY export,
    /// never read back, so an orphan is inert — and because the review is
    /// still `draft`, a client can simply retry `finalize`, which overwrites
    /// that same orphaned file (same `relpath`) with fresh content and, this
    /// time, hopefully commits. We do NOT attempt to delete the orphan on a
    /// failed commit — best-effort cleanup after a commit failure is itself
    /// I/O that can fail, and leaving a stale file is strictly safer than a
    /// half-deleted one.
    ///
    /// If `self.data_dir` is `None` (only reachable via `open_memory()` with
    /// no test override — the shipped binary always has a `data_dir`, see
    /// `main.rs`), there is nowhere to export to: the Markdown step is
    /// skipped entirely (not a failure) and `body_ref` stays `None`. SQLite
    /// is the source of truth either way, so this degrades the same way a
    /// kernel not granting `events` does elsewhere in this crate — it never
    /// blocks the actual state transition.
    pub async fn finalize_review(&self, id: &str) -> Result<Review> {
        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query(&format!(
            "SELECT {REVIEW_COLUMNS} FROM sin90_reviews WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Err(StoreError::NotFound(format!("review {id}")));
        };
        let current = row_to_review(row)?;
        check_review_transition(current.status, ReviewStatus::Finalized)?;

        // Step 2: write the file FIRST — see this fn's doc for why. Nothing
        // has been written to `sin90_reviews` yet, so an error here leaves
        // the review exactly as it was (dropping `tx` rolls back the read).
        let body_ref = match &self.data_dir {
            Some(dir) => {
                let relpath = review_markdown_relpath(current.kind, &current.period)?;
                write_markdown_atomic(dir, &relpath, &current.body).map_err(|e| {
                    StoreError::Internal(format!(
                        "failed writing review markdown export {relpath:?}: {e}"
                    ))
                })?;
                Some(relpath)
            }
            None => None,
        };

        // Step 3: only now does anything land in the DB.
        let now = now_iso8601();
        sqlx::query(
            "UPDATE sin90_reviews SET status = 'finalized', body_ref = ?, updated_at = ? WHERE id = ?",
        )
        .bind(&body_ref)
        .bind(&now)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        append_event(
            &mut tx,
            "review",
            id,
            "finalized",
            Some("draft"),
            Some("finalized"),
            &json!({"review_id": id}),
            &now,
        )
        .await?;
        tx.commit().await?;
        Ok(Review {
            status: ReviewStatus::Finalized,
            body_ref,
            updated_at: now,
            ..current
        })
    }
}

/// A read-only snapshot of the entities a proposal references, so
/// `core::validate` stays pure. Widened (design §3.3) with `areas` and
/// `task_parents` for the new `CreateArea`/`CreateTask` ops.
#[derive(Default)]
pub(crate) struct DbSnapshot {
    tasks: HashMap<String, TaskStatus>,
    task_parents: HashMap<String, Option<String>>,
    weeks: HashMap<String, WeekStatus>,
    rhythms_retired: HashMap<String, bool>,
    areas: std::collections::HashSet<String>,
    /// New (design §11.2.3, T5.2.1): outer presence = the Direction exists.
    direction_statuses: HashMap<String, DirectionStatus>,
    /// New (design §11.2.3, T5.2.1): outer presence = the task exists; `None`
    /// inner = the task is currently in the inbox.
    task_directions: HashMap<String, Option<String>>,
    /// New (design §11.2.3, T5.2.1): outer presence = the Review exists.
    reviews: HashMap<String, ReviewSnap>,
}

impl ValidationCtx for DbSnapshot {
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
    fn task_parent(&self, id: &str) -> Option<Option<String>> {
        self.task_parents.get(id).cloned()
    }
    fn direction_status(&self, id: &str) -> Option<DirectionStatus> {
        self.direction_statuses.get(id).copied()
    }
    fn task_direction(&self, id: &str) -> Option<Option<String>> {
        self.task_directions.get(id).cloned()
    }
    fn review_snap(&self, id: &str) -> Option<ReviewSnap> {
        self.reviews.get(id).cloned()
    }
}

pub(crate) async fn build_snapshot(tx: &mut Tx<'_>, ops: &[Sin90Op]) -> Result<DbSnapshot> {
    let mut snap = DbSnapshot::default();
    for op in ops {
        match op {
            Sin90Op::CreateArea { .. } | Sin90Op::CreateDirection { .. } => {}
            Sin90Op::CreateTask { parent_task_id, .. } => {
                if let Some(pid) = parent_task_id {
                    load_task_parent(tx, pid, &mut snap).await?;
                }
            }
            Sin90Op::TransitionTask { task_id, .. } => load_task(tx, task_id, &mut snap).await?,
            Sin90Op::CreateTasks { week_id, .. } | Sin90Op::ReorderTasks { week_id, .. } => {
                load_week(tx, week_id, &mut snap).await?
            }
            Sin90Op::AdjustRhythm { rhythm_id, .. } => {
                load_rhythm(tx, rhythm_id, &mut snap).await?
            }
            Sin90Op::CarryOverTask { task_id, to_week } => {
                load_task(tx, task_id, &mut snap).await?;
                load_week(tx, to_week, &mut snap).await?;
            }
            Sin90Op::AssignTaskDirection {
                task_id,
                direction_id,
            } => {
                load_task(tx, task_id, &mut snap).await?; // A2's task_status
                load_task_direction(tx, task_id, &mut snap).await?; // A1/A3
                load_direction_status(tx, direction_id, &mut snap).await?; // A4/A5
            }
            Sin90Op::DraftReviewBody { review_id, .. } => {
                load_review_snap(tx, review_id, &mut snap).await?; // D4-D6
            }
        }
    }
    Ok(snap)
}

async fn load_task(tx: &mut Tx<'_>, id: &str, snap: &mut DbSnapshot) -> Result<()> {
    if let Some(row) = sqlx::query("SELECT status FROM sin90_tasks WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
    {
        snap.tasks
            .insert(id.to_string(), from_wire(&row.get::<String, _>("status"))?);
    }
    Ok(())
}

async fn load_task_parent(tx: &mut Tx<'_>, id: &str, snap: &mut DbSnapshot) -> Result<()> {
    if let Some(row) = sqlx::query("SELECT parent_task_id FROM sin90_tasks WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
    {
        let parent: Option<String> = row.get("parent_task_id");
        snap.task_parents.insert(id.to_string(), parent);
    }
    Ok(())
}

async fn load_week(tx: &mut Tx<'_>, id: &str, snap: &mut DbSnapshot) -> Result<()> {
    if let Some(row) = sqlx::query("SELECT status FROM sin90_weeks WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
    {
        snap.weeks
            .insert(id.to_string(), from_wire(&row.get::<String, _>("status"))?);
    }
    Ok(())
}

/// New (design §11.2.3, T5.2.1): outer presence in `snap.task_directions` =
/// the task exists; the `Option<String>` inside is its current
/// `direction_id` (`None` = still in the inbox).
async fn load_task_direction(tx: &mut Tx<'_>, id: &str, snap: &mut DbSnapshot) -> Result<()> {
    if let Some(row) = sqlx::query("SELECT direction_id FROM sin90_tasks WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
    {
        let direction_id: Option<String> = row.get("direction_id");
        snap.task_directions.insert(id.to_string(), direction_id);
    }
    Ok(())
}

/// New (design §11.2.3, T5.2.1): existence + current status of a Direction.
async fn load_direction_status(tx: &mut Tx<'_>, id: &str, snap: &mut DbSnapshot) -> Result<()> {
    if let Some(row) = sqlx::query("SELECT status FROM sin90_directions WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
    {
        snap.direction_statuses
            .insert(id.to_string(), from_wire(&row.get::<String, _>("status"))?);
    }
    Ok(())
}

/// New (design §11.2.2/§11.2.3, T5.2.1): `DraftReviewBody`'s snapshot —
/// status plus the body digest, computed in RUST from the stored body
/// (`crate::core::body_sha256`), never in SQL (design §11.2.2's apply note).
async fn load_review_snap(tx: &mut Tx<'_>, id: &str, snap: &mut DbSnapshot) -> Result<()> {
    if let Some(row) = sqlx::query("SELECT status, body FROM sin90_reviews WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
    {
        let status: ReviewStatus = from_wire(&row.get::<String, _>("status"))?;
        let body: String = row.get("body");
        snap.reviews.insert(
            id.to_string(),
            ReviewSnap {
                status,
                body_sha256: body_sha256(&body),
            },
        );
    }
    Ok(())
}

async fn load_rhythm(tx: &mut Tx<'_>, id: &str, snap: &mut DbSnapshot) -> Result<()> {
    if let Some(row) = sqlx::query("SELECT status FROM sin90_rhythms WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
    {
        let status: RhythmStatus = from_wire(&row.get::<String, _>("status"))?;
        snap.rhythms_retired
            .insert(id.to_string(), status == RhythmStatus::Retired);
    }
    Ok(())
}

/// Shared by `Sin90Store::create_rhythm` (direct write) and `AdjustRhythm`'s
/// apply arm below (Opus 2026-09-23 review H1): every `direction_id` in
/// `allocations` must reference a live `sin90_directions` row, checked under
/// the SAME `BEGIN IMMEDIATE` write lock as the insert/update that follows —
/// not a pure/`ValidationCtx` check (module doc's scope note: existence
/// against live rows is a relational check, the store's job). A missing
/// direction is `NotFound` (404), the same "referenced entity does not
/// exist" convention `create_task`/`create_block` already use.
async fn require_directions_exist(tx: &mut Tx<'_>, allocations: &[Alloc]) -> Result<()> {
    for a in allocations {
        let exists: i64 = sqlx::query("SELECT COUNT(*) AS n FROM sin90_directions WHERE id = ?")
            .bind(&a.direction_id)
            .fetch_one(&mut **tx)
            .await?
            .get("n");
        if exists == 0 {
            return Err(StoreError::NotFound(format!(
                "direction {}",
                a.direction_id
            )));
        }
    }
    Ok(())
}

pub(crate) async fn apply_op(
    tx: &mut Tx<'_>,
    op: &Sin90Op,
    event_ids: &mut Vec<String>,
) -> Result<()> {
    let now = now_iso8601();
    match op {
        Sin90Op::CreateArea { title } => {
            let id = ulid();
            let slug = allocate_area_slug(tx, title).await?;
            sqlx::query(
                "INSERT INTO sin90_areas (id, title, slug, status, sort_key, created_at, updated_at)
                 VALUES (?, ?, ?, 'active', 0, ?, ?)",
            )
            .bind(&id)
            .bind(title)
            .bind(&slug)
            .bind(&now)
            .bind(&now)
            .execute(&mut **tx)
            .await?;
            let ev = append_event(
                tx,
                "area",
                &id,
                "created",
                None,
                Some("active"),
                &json!({"id": id, "title": title, "slug": slug}),
                &now,
            )
            .await?;
            event_ids.push(ev);
        }

        Sin90Op::CreateTask {
            title,
            direction_id,
            parent_task_id,
            kind,
            energy,
            est_minutes,
        } => {
            let id = ulid();
            sqlx::query(
                "INSERT INTO sin90_tasks
                     (id, direction_id, week_id, parent_task_id, title, status, kind, energy,
                      est_minutes, sort_key, carried_from, created_at, updated_at)
                 VALUES (?, ?, NULL, ?, ?, 'backlog', ?, ?, ?, 0, NULL, ?, ?)",
            )
            .bind(&id)
            .bind(direction_id)
            .bind(parent_task_id)
            .bind(title)
            .bind(to_wire(&kind.unwrap_or(TaskKind::Other))?)
            .bind(to_wire(&energy.unwrap_or(Energy::Mid))?)
            .bind(est_minutes.map(|m| m as i64))
            .bind(&now)
            .bind(&now)
            .execute(&mut **tx)
            .await?;
            let ev = append_event(
                tx,
                "task",
                &id,
                "created",
                None,
                Some("backlog"),
                &json!({"id": id, "direction_id": direction_id, "parent_task_id": parent_task_id, "title": title}),
                &now,
            )
            .await?;
            event_ids.push(ev);
        }

        Sin90Op::CreateDirection {
            title,
            target_window,
        } => {
            let id = ulid();
            sqlx::query(
                "INSERT INTO sin90_directions (id, area_id, title, status, target_window, created_at, updated_at)
                 VALUES (?, NULL, ?, 'draft', ?, ?, ?)",
            )
            .bind(&id)
            .bind(title)
            .bind(target_window)
            .bind(&now)
            .bind(&now)
            .execute(&mut **tx)
            .await?;
            let ev = append_event(
                tx,
                "direction",
                &id,
                "created",
                None,
                Some("draft"),
                &json!({"id": id, "title": title, "status": "draft", "target_window": target_window}),
                &now,
            )
            .await?;
            event_ids.push(ev);
        }

        Sin90Op::TransitionTask { task_id, to } => {
            require_task_week_open(tx, task_id).await?; // relational invariant
            let from = read_task_status(tx, task_id).await?;
            check_task_transition(from, *to)?;
            let to_str = to_wire(to)?;
            sqlx::query("UPDATE sin90_tasks SET status = ?, updated_at = ? WHERE id = ?")
                .bind(&to_str)
                .bind(&now)
                .bind(task_id)
                .execute(&mut **tx)
                .await?;
            let ev = append_event(
                tx,
                "task",
                task_id,
                "transitioned",
                Some(&to_wire(&from)?),
                Some(&to_str),
                &json!({"task_id": task_id}),
                &now,
            )
            .await?;
            event_ids.push(ev);
        }

        Sin90Op::CreateTasks { week_id, tasks } => {
            for (i, t) in tasks.iter().enumerate() {
                let id = ulid();
                sqlx::query(
                    "INSERT INTO sin90_tasks
                         (id, direction_id, week_id, parent_task_id, title, status, kind, energy,
                          est_minutes, sort_key, carried_from, created_at, updated_at)
                     VALUES (?, ?, ?, NULL, ?, 'planned', 'other', 'mid', NULL, ?, NULL, ?, ?)",
                )
                .bind(&id)
                .bind(&t.direction_id)
                .bind(week_id)
                .bind(&t.title)
                .bind(i as i64)
                .bind(&now)
                .bind(&now)
                .execute(&mut **tx)
                .await?;
                let ev = append_event(
                    tx,
                    "task",
                    &id,
                    "created",
                    None,
                    Some("planned"),
                    // §2 #25 / §11.2.1's replay rule #2 (T5.4.1): `direction_id`
                    // added so a task created directly INTO a Direction by
                    // `propose.create` (never assigned via `AssignTaskDirection`)
                    // still has a self-contained ownership fact to replay from.
                    &json!({"id": id, "week_id": week_id, "title": t.title, "direction_id": t.direction_id}),
                    &now,
                )
                .await?;
                event_ids.push(ev);
            }
        }

        Sin90Op::ReorderTasks { week_id, order } => {
            for (i, tid) in order.iter().enumerate() {
                let affected = sqlx::query(
                    "UPDATE sin90_tasks SET sort_key = ?, updated_at = ? WHERE id = ? AND week_id = ?",
                )
                .bind(i as i64)
                .bind(&now)
                .bind(tid)
                .bind(week_id)
                .execute(&mut **tx)
                .await?
                .rows_affected();
                if affected != 1 {
                    return Err(StoreError::NotFound(format!(
                        "task {tid} not in week {week_id} (reorder)"
                    )));
                }
            }
            let ev = append_event(
                tx,
                "week",
                week_id,
                "reordered",
                None,
                None,
                &json!({"week_id": week_id, "order": order}),
                &now,
            )
            .await?;
            event_ids.push(ev);
        }

        Sin90Op::AdjustRhythm {
            rhythm_id,
            new_alloc,
        } => {
            // Opus 2026-09-23 review H1: `new_alloc`'s directions must exist,
            // same as `create_rhythm`'s — checked under this SAME write lock,
            // via the SAME helper, so the two paths cannot drift apart again.
            require_directions_exist(tx, new_alloc).await?;
            let from = read_rhythm_status(tx, rhythm_id).await?;
            check_rhythm_transition(from, RhythmStatus::Adjusted)?;
            sqlx::query(
                "UPDATE sin90_rhythms SET status = 'adjusted', allocations = ?, updated_at = ? WHERE id = ?",
            )
            .bind(serde_json::to_string(new_alloc)?)
            .bind(&now)
            .bind(rhythm_id)
            .execute(&mut **tx)
            .await?;
            let ev = append_event(
                tx,
                "rhythm",
                rhythm_id,
                "adjusted",
                Some(&to_wire(&from)?),
                Some("adjusted"),
                &json!({"rhythm_id": rhythm_id, "allocations": new_alloc}),
                &now,
            )
            .await?;
            event_ids.push(ev);
        }

        Sin90Op::CarryOverTask { task_id, to_week } => {
            require_task_week_open(tx, task_id).await?;
            let src = sqlx::query(
                "SELECT title, direction_id, week_id, parent_task_id, kind, energy, est_minutes
                 FROM sin90_tasks WHERE id = ?",
            )
            .bind(task_id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("task {task_id}")))?;
            let title: String = src.get("title");
            // Carrying a task into next week changes WHEN, not WHAT: kind,
            // energy and estimate come along, and it stays in the same project
            // (parent_task_id unchanged — a project spans weeks).
            let parent_task_id: Option<String> = src.get("parent_task_id");
            let kind: String = src.get("kind");
            let energy: String = src.get("energy");
            let est_minutes: Option<i64> = src.get("est_minutes");
            let direction_id: Option<String> = src.get("direction_id");
            let src_week: Option<String> = src.get("week_id");
            if src_week.as_deref() == Some(to_week.as_str()) {
                return Err(StoreError::SameWeekCarry(task_id.to_string()));
            }
            let from = read_task_status(tx, task_id).await?;
            check_task_transition(from, TaskStatus::CarriedOver)?;
            sqlx::query(
                "UPDATE sin90_tasks SET status = 'carried_over', updated_at = ? WHERE id = ?",
            )
            .bind(&now)
            .bind(task_id)
            .execute(&mut **tx)
            .await?;
            let close_ev = append_event(
                tx,
                "task",
                task_id,
                "transitioned",
                Some(&to_wire(&from)?),
                Some("carried_over"),
                &json!({"task_id": task_id}),
                &now,
            )
            .await?;
            event_ids.push(close_ev);
            let new_id = ulid();
            sqlx::query(
                "INSERT INTO sin90_tasks
                     (id, direction_id, week_id, parent_task_id, title, status, kind, energy,
                      est_minutes, sort_key, carried_from, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, 'planned', ?, ?, ?, 0, ?, ?, ?)",
            )
            .bind(&new_id)
            .bind(&direction_id)
            .bind(to_week)
            .bind(&parent_task_id)
            .bind(&title)
            .bind(&kind)
            .bind(&energy)
            .bind(est_minutes)
            .bind(task_id)
            .bind(&now)
            .bind(&now)
            .execute(&mut **tx)
            .await?;
            let create_ev = append_event(
                tx,
                "task",
                &new_id,
                "created",
                None,
                Some("planned"),
                // §2 #25 (T5.4.1): `direction_id` = the SOURCE task's
                // ownership (read into `direction_id` above, before this
                // INSERT) — the carried-over task inherits it, same as the
                // row itself does.
                &json!({"id": new_id, "week_id": to_week, "carried_from": task_id, "direction_id": direction_id}),
                &now,
            )
            .await?;
            event_ids.push(create_ev);
        }

        Sin90Op::AssignTaskDirection {
            task_id,
            direction_id,
        } => {
            require_task_week_open(tx, task_id).await?; // relational invariant
                                                        // Read at apply time (not the validate-time snapshot) for the
                                                        // event payload's self-contained `area_id` (design §11.2.1);
                                                        // `None` if the Direction row is somehow gone by apply time (an
                                                        // FK/race edge case the design accepts rather than errors on).
            let area_id: Option<String> =
                sqlx::query("SELECT area_id FROM sin90_directions WHERE id = ?")
                    .bind(direction_id)
                    .fetch_optional(&mut **tx)
                    .await?
                    .and_then(|r| r.get::<Option<String>, _>("area_id"));
            // Second CAS (design §11.2.1 step 3): `validate`'s A3 already
            // checked "still in the inbox" against the snapshot taken at the
            // start of this transaction; this UPDATE re-checks it against the
            // CURRENT row, under the same write lock, closing any gap between
            // snapshot and apply.
            //
            // 2026-09-24 review (round 2, low): this branch is DEFENSE IN
            // DEPTH, not something a unit test can currently force to fire
            // independently of A3 — every caller of `apply_op` (both
            // `AiSink::submit`'s dry run and `Sin90Store::apply_proposal`)
            // calls `build_snapshot` + `validate` immediately before, in the
            // SAME transaction, so by construction the snapshot `validate`
            // saw is never stale by the time this UPDATE runs; there is no
            // window today where A3 passes but this `affected != 1`. It
            // exists so a FUTURE caller that reuses `apply_op` without that
            // same validate-then-apply discipline (e.g. a batch/retry path
            // that re-applies an already-decided op) fails safely instead of
            // silently overwriting a task's Direction a second time.
            let affected = sqlx::query(
                "UPDATE sin90_tasks SET direction_id = ?, updated_at = ?
                 WHERE id = ? AND direction_id IS NULL",
            )
            .bind(direction_id)
            .bind(&now)
            .bind(task_id)
            .execute(&mut **tx)
            .await?
            .rows_affected();
            if affected != 1 {
                return Err(StoreError::Conflict(format!(
                    "task {task_id} is no longer in the inbox (assign_task_direction CAS)"
                )));
            }
            let ev = append_event(
                tx,
                "task",
                task_id,
                "direction_assigned",
                None, // status did not change
                None,
                &json!({
                    "task_id": task_id,
                    "from_direction_id": null,
                    "direction_id": direction_id,
                    "area_id": area_id,
                }),
                &now,
            )
            .await?;
            event_ids.push(ev);
        }

        Sin90Op::DraftReviewBody {
            review_id, body, ..
        } => {
            // Second CAS (design §11.2.2 apply step): `validate`'s D5/D6
            // already checked "still draft" / "base matches" against the
            // snapshot taken at the start of this transaction; this UPDATE
            // re-checks `status = 'draft'` against the CURRENT row, under the
            // same write lock (closes the same snapshot/apply gap
            // `AssignTaskDirection`'s CAS closes for tasks). `base_body_sha256`
            // itself is not re-checked here — that would need a second SELECT
            // to recompute the digest; the `status = 'draft'` CAS is the one
            // race this design accepts fixing at this layer (§11.2.2: the
            // dry-run `submit` path validates a beat before this, and the
            // human-accept path's `apply_proposal` re-runs `build_snapshot` +
            // `validate` immediately before calling `apply_op`, so D6 is
            // freshly re-checked either way, not stale by the time it gets
            // here). 2026-09-24 review (round 2, low): like
            // `AssignTaskDirection`'s CAS above, this `status = 'draft'`
            // check is DEFENSE IN DEPTH — no unit test today can force
            // `affected != 1` independently of D5, since every caller
            // validates in the same transaction immediately before applying.
            // It guards a future caller that reuses `apply_op` without that
            // discipline.
            let affected = sqlx::query(
                "UPDATE sin90_reviews SET body = ?, updated_at = ? WHERE id = ? AND status = 'draft'",
            )
            .bind(body)
            .bind(&now)
            .bind(review_id)
            .execute(&mut **tx)
            .await?
            .rows_affected();
            if affected != 1 {
                return Err(StoreError::Conflict(format!(
                    "review {review_id} is not accepting further changes (draft_review_body CAS)"
                )));
            }
            // Same event shape the human path (`update_review_body`) writes —
            // a replay consumer never needs to know a body change came from a
            // proposal (design §11.2.2's "与人类路径逐字段相同").
            let ev = append_event(
                tx,
                "review",
                review_id,
                "updated",
                None,
                None,
                &json!({"review_id": review_id, "body": body}),
                &now,
            )
            .await?;
            event_ids.push(ev);
        }
    }
    Ok(())
}

async fn read_task_status(tx: &mut Tx<'_>, id: &str) -> Result<TaskStatus> {
    let row = sqlx::query("SELECT status FROM sin90_tasks WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| StoreError::NotFound(format!("task {id}")))?;
    from_wire(&row.get::<String, _>("status"))
}

/// Relational invariant the pure validator can't express (ValidationCtx is
/// per-entity): a task may only be mutated while its week is OPEN
/// (planning | active). A task with no week passes. Enforced under the write
/// lock.
async fn require_task_week_open(tx: &mut Tx<'_>, task_id: &str) -> Result<()> {
    let row = sqlx::query(
        "SELECT w.status AS wstatus
         FROM sin90_tasks t JOIN sin90_weeks w ON w.id = t.week_id
         WHERE t.id = ?",
    )
    .bind(task_id)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(row) = row {
        let wstatus: WeekStatus = from_wire(&row.get::<String, _>("wstatus"))?;
        if !week_is_open(wstatus) {
            return Err(StoreError::WeekNotOpen(task_id.to_string()));
        }
    }
    Ok(())
}

/// Existence check for `Routine.area_id` (M3, design §3.2) — checked
/// explicitly under the write lock so a dangling reference comes back as a
/// clean [`StoreError::NotFound`] instead of the FK violation's raw SQLite
/// error (same convention as `create_task`'s `parent_task_id` check).
async fn require_area_exists(tx: &mut Tx<'_>, id: &str) -> Result<()> {
    let found = sqlx::query("SELECT 1 FROM sin90_areas WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?;
    if found.is_none() {
        return Err(StoreError::NotFound(format!("area {id}")));
    }
    Ok(())
}

/// Existence check for `Routine.direction_id` (M3, design §3.2) — see
/// `require_area_exists`.
async fn require_direction_exists(tx: &mut Tx<'_>, id: &str) -> Result<()> {
    let found = sqlx::query("SELECT 1 FROM sin90_directions WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?;
    if found.is_none() {
        return Err(StoreError::NotFound(format!("direction {id}")));
    }
    Ok(())
}

async fn read_rhythm_status(tx: &mut Tx<'_>, id: &str) -> Result<RhythmStatus> {
    let row = sqlx::query("SELECT status FROM sin90_rhythms WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| StoreError::NotFound(format!("rhythm {id}")))?;
    from_wire(&row.get::<String, _>("status"))
}

/// Routine `create`/`get`/`list` tests (T3.1.1 review split — H2/M3/M5/L1
/// and the transition-dependent half of M4 live in feat/t3.1.1-routine-store,
/// layered on top of this branch, which is why they aren't here).
#[cfg(test)]
mod routine_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::core::RoutineKind;
    use crate::store::test_hooks;

    async fn new_store() -> Sin90Store {
        Sin90Store::open_memory().await.unwrap()
    }

    /// A `NewRoutine` with every optional field at its default (`None`) —
    /// tests override only the fields they care about via struct-update
    /// syntax (`NewRoutine { area_id: Some(..), ..nr(..) }`).
    ///
    /// Public within the crate (`pub(crate)`, not private) so
    /// feat/t3.1.1-routine-store's tests, layered on top of this branch, can
    /// reuse it instead of redefining an identical helper.
    pub(crate) fn nr(title: &str, kind: RoutineKind, cron: &str) -> NewRoutine {
        NewRoutine {
            title: title.to_string(),
            area_id: None,
            direction_id: None,
            kind,
            cron: cron.to_string(),
            tz: None,
            target_count: None,
            target_minutes: None,
        }
    }

    /// Also `pub(crate)`: every routine test in feat/t3.1.1-routine-store
    /// (update/transition) starts from a freshly created routine too.
    pub(crate) async fn create_ok(store: &Sin90Store) -> Routine {
        store
            .create_routine(&NewRoutine {
                target_count: Some(3),
                target_minutes: Some(30),
                ..nr("Morning run", RoutineKind::Exercise, "0 7 * * MON,WED,FRI")
            })
            .await
            .unwrap()
    }

    // ----- creation validation ------------------------------------------

    #[tokio::test]
    async fn routine_create_rejects_invalid_cron() {
        let store = new_store().await;
        let err = store
            .create_routine(&nr("Bad cron", RoutineKind::Other, "not a cron"))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invalid(_)), "{err:?}");
    }

    /// H1 (T3.1.1 review): the store layer rejects a digit-based weekday
    /// field too, not just `core::util::validate_cron` in isolation — this
    /// is the actual path `POST /routines` (T3.1.2) will run through.
    #[tokio::test]
    async fn routine_create_rejects_posix_style_digit_weekday() {
        let store = new_store().await;
        let err = store
            .create_routine(&nr(
                "POSIX weekday digits",
                RoutineKind::Other,
                "0 7 * * 1-5",
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invalid(_)), "{err:?}");
    }

    #[tokio::test]
    async fn routine_create_rejects_invalid_tz() {
        let store = new_store().await;
        let err = store
            .create_routine(&NewRoutine {
                tz: Some("Not/AZone".to_string()),
                ..nr("Bad tz", RoutineKind::Other, "0 7 * * MON-FRI")
            })
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invalid(_)), "{err:?}");
    }

    /// L2 (T3.1.1 review): an explicit empty `tz` string is a caller bug,
    /// not "please default to UTC" — those are different inputs
    /// (`routine_create_defaults_tz_to_utc_and_links_area_direction` below
    /// is the positive control: omitting `tz` entirely DOES default).
    #[tokio::test]
    async fn routine_create_rejects_empty_tz_string() {
        let store = new_store().await;
        let err = store
            .create_routine(&NewRoutine {
                tz: Some(String::new()),
                ..nr("Empty tz", RoutineKind::Other, "0 7 * * MON-FRI")
            })
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invalid(_)), "{err:?}");
    }

    /// L4 (T3.1.1 review): a blank/whitespace-only title is rejected, same
    /// as every other entity's title in this store.
    #[tokio::test]
    async fn routine_create_rejects_blank_title() {
        let store = new_store().await;
        for bad_title in ["", "   ", "\t\n"] {
            let err = store
                .create_routine(&nr(bad_title, RoutineKind::Other, "0 7 * * MON-FRI"))
                .await
                .unwrap_err();
            assert!(
                matches!(err, StoreError::Invalid(_)),
                "{bad_title:?}: {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn routine_create_rejects_zero_target_count() {
        let store = new_store().await;
        let err = store
            .create_routine(&NewRoutine {
                target_count: Some(0),
                ..nr("Zero target", RoutineKind::Other, "0 7 * * MON-FRI")
            })
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invalid(_)), "{err:?}");
    }

    #[tokio::test]
    async fn routine_create_rejects_zero_target_minutes() {
        let store = new_store().await;
        let err = store
            .create_routine(&NewRoutine {
                target_minutes: Some(0),
                ..nr("Zero minutes", RoutineKind::Other, "0 7 * * MON-FRI")
            })
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invalid(_)), "{err:?}");
    }

    #[tokio::test]
    async fn routine_create_rejects_missing_area_and_direction() {
        let store = new_store().await;
        let err = store
            .create_routine(&NewRoutine {
                area_id: Some("no-such-area".to_string()),
                ..nr("Ghost area", RoutineKind::Other, "0 7 * * MON-FRI")
            })
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)), "{err:?}");

        let err = store
            .create_routine(&NewRoutine {
                direction_id: Some("no-such-direction".to_string()),
                ..nr("Ghost direction", RoutineKind::Other, "0 7 * * MON-FRI")
            })
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)), "{err:?}");
    }

    #[tokio::test]
    async fn routine_create_defaults_tz_to_utc_and_links_area_direction() {
        let store = new_store().await;
        let area = store.create_area("Health").await.unwrap();
        let direction = store
            .create_direction("Q4 fitness", "2026-Q4", Some(&area.id))
            .await
            .unwrap();
        let routine = store
            .create_routine(&NewRoutine {
                area_id: Some(area.id.clone()),
                direction_id: Some(direction.id.clone()),
                target_count: Some(3),
                target_minutes: Some(30),
                // tz omitted entirely -> defaults to UTC (contrast with
                // `routine_create_rejects_empty_tz_string`'s explicit `""`).
                ..nr("Morning run", RoutineKind::Exercise, "0 7 * * MON,WED,FRI")
            })
            .await
            .unwrap();
        assert_eq!(routine.tz, "UTC");
        assert_eq!(routine.area_id.as_deref(), Some(area.id.as_str()));
        assert_eq!(routine.direction_id.as_deref(), Some(direction.id.as_str()));
        assert_eq!(routine.status, RoutineStatus::Active);
        assert_eq!(routine.kind, RoutineKind::Exercise);
    }

    // ----- events: exactly one per mutation, zero on reads ----------------

    #[tokio::test]
    async fn routine_create_emits_exactly_one_created_event() {
        let store = new_store().await;
        let routine = create_ok(&store).await;
        let n = test_hooks::event_count(&store, "routine", &routine.id)
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    /// Positive control for "get/list are zero-event read paths": the SAME
    /// routine, read both ways, must not add to the event count that
    /// `routine_create_emits_exactly_one_created_event` established.
    #[tokio::test]
    async fn routine_get_and_list_emit_no_events() {
        let store = new_store().await;
        let routine = create_ok(&store).await;
        let before = test_hooks::event_count(&store, "routine", &routine.id)
            .await
            .unwrap();

        let _ = store.get_routine(&routine.id).await.unwrap();
        let _ = store.list_routines(None, None, None).await.unwrap();
        let _ = store
            .list_routines(None, None, Some(RoutineStatus::Active))
            .await
            .unwrap();

        let after = test_hooks::event_count(&store, "routine", &routine.id)
            .await
            .unwrap();
        assert_eq!(before, after, "get/list must not append events");
        assert_eq!(after, 1); // only the original `created`
    }

    // ----- list filters: positive controls (M4, area/direction half) --------
    //
    // The `status` filter's positive control needs `transition_routine`
    // (to produce a non-`active` row) — that half lives in
    // feat/t3.1.1-routine-store's `routine_list_filter_by_status_has_positive_control`.

    /// M4 (T3.1.1 review): two routines in different areas/directions — the
    /// `area_id`/`direction_id` filters must each return exactly the
    /// matching routine, not "everything" or "nothing" (which would look
    /// identical to a correct result if the filter silently did nothing).
    #[tokio::test]
    async fn routine_list_filters_by_area_and_direction_have_positive_controls() {
        let store = new_store().await;
        let area1 = store.create_area("Health").await.unwrap();
        let area2 = store.create_area("Work").await.unwrap();
        let dir1 = store
            .create_direction("Q4 fitness", "2026-Q4", Some(&area1.id))
            .await
            .unwrap();

        let r1 = store
            .create_routine(&NewRoutine {
                area_id: Some(area1.id.clone()),
                direction_id: Some(dir1.id.clone()),
                ..nr("Run", RoutineKind::Exercise, "0 7 * * MON,WED,FRI")
            })
            .await
            .unwrap();
        let r2 = store
            .create_routine(&NewRoutine {
                area_id: Some(area2.id.clone()),
                ..nr("Deep work", RoutineKind::DeepWork, "0 9 * * MON-FRI")
            })
            .await
            .unwrap();

        let ids = |rs: &[Routine]| rs.iter().map(|r| r.id.clone()).collect::<Vec<_>>();

        assert_eq!(
            ids(&store
                .list_routines(Some(&area1.id), None, None)
                .await
                .unwrap()),
            vec![r1.id.clone()]
        );
        assert_eq!(
            ids(&store
                .list_routines(Some(&area2.id), None, None)
                .await
                .unwrap()),
            vec![r2.id.clone()]
        );
        assert_eq!(
            ids(&store
                .list_routines(None, Some(&dir1.id), None)
                .await
                .unwrap()),
            vec![r1.id.clone()]
        );
        // No filter -> both, positive control that the filters above aren't
        // just "return everything regardless".
        assert_eq!(
            store.list_routines(None, None, None).await.unwrap().len(),
            2
        );
        // Both are still `active` at this point (this branch has no
        // `transition_routine`), so a `status` filter is a sanity check
        // only, not yet a positive control — see the module doc above.
        assert_eq!(
            store
                .list_routines(None, None, Some(RoutineStatus::Active))
                .await
                .unwrap()
                .len(),
            2
        );
    }

    // ----- list filter: positive control (M4, status half) ------------------

    /// M4 (T3.1.1 review), status half: needs `transition_routine` to
    /// produce a non-`active` row, so it lives here rather than alongside
    /// `routine_list_filters_by_area_and_direction_have_positive_controls`
    /// (feat/t3.1.1b-routine-store-read, which has no transition method).
    /// Same shape of positive control: two routines, one paused, each
    /// `status` filter returns exactly the matching one.
    #[tokio::test]
    async fn routine_list_filter_by_status_has_positive_control() {
        let store = new_store().await;
        let r1 = create_ok(&store).await;
        let r2 = create_ok(&store).await;
        store
            .transition_routine(&r2.id, RoutineStatus::Paused)
            .await
            .unwrap();

        let ids = |rs: &[Routine]| rs.iter().map(|r| r.id.clone()).collect::<Vec<_>>();
        assert_eq!(
            ids(&store
                .list_routines(None, None, Some(RoutineStatus::Active))
                .await
                .unwrap()),
            vec![r1.id.clone()]
        );
        assert_eq!(
            ids(&store
                .list_routines(None, None, Some(RoutineStatus::Paused))
                .await
                .unwrap()),
            vec![r2.id.clone()]
        );
        // No filter -> both, positive control that the filter above isn't
        // just "return everything regardless".
        assert_eq!(
            store.list_routines(None, None, None).await.unwrap().len(),
            2
        );
    }

    // ----- events: create -> pause -> resume -> retire sequence -------------

    /// M3 (T3.1.1 review): the exact `(kind, from_state, to_state)` sequence
    /// a full create → pause → resume → retire lifecycle produces — not just
    /// the count. Mutation (verified during development, not committed):
    /// remap `transition_routine`'s `paused`/`resumed`/`retired` kinds to a
    /// single generic `"transitioned"` string (the pattern every OTHER
    /// entity in this file uses) and this assertion goes red, proving it
    /// actually pins the destination-specific names down.
    #[tokio::test]
    async fn routine_event_sequence_kinds_and_states_are_exact() {
        let store = new_store().await;
        let routine = create_ok(&store).await;
        store
            .transition_routine(&routine.id, RoutineStatus::Paused)
            .await
            .unwrap();
        store
            .transition_routine(&routine.id, RoutineStatus::Active)
            .await
            .unwrap();
        store
            .transition_routine(&routine.id, RoutineStatus::Retired)
            .await
            .unwrap();

        let events = store
            .list_events(Some("routine"), Some(&routine.id), None, None)
            .await
            .unwrap();
        let seq: Vec<(String, Option<String>, Option<String>)> = events
            .iter()
            .map(|e| (e.kind.clone(), e.from_state.clone(), e.to_state.clone()))
            .collect();
        assert_eq!(
            seq,
            vec![
                ("created".to_string(), None, Some("active".to_string())),
                (
                    "paused".to_string(),
                    Some("active".to_string()),
                    Some("paused".to_string())
                ),
                (
                    "resumed".to_string(),
                    Some("paused".to_string()),
                    Some("active".to_string())
                ),
                (
                    "retired".to_string(),
                    Some("active".to_string()),
                    Some("retired".to_string())
                ),
            ]
        );
    }

    /// M3/M5 (T3.1.1 review): payload field assertions for `created` (ad hoc
    /// fields, `routine_id` key) and `updated` (full snapshot + `changed`).
    #[tokio::test]
    async fn routine_created_and_updated_event_payloads_have_expected_fields() {
        let store = new_store().await;
        let routine = create_ok(&store).await;
        let updated = store
            .update_routine(
                &routine.id,
                &RoutinePatch {
                    title: Some("Evening run".to_string()),
                    cron: Some("0 19 * * MON,WED,FRI".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let events = store
            .list_events(Some("routine"), Some(&routine.id), None, None)
            .await
            .unwrap();
        assert_eq!(events.len(), 2);

        let created_payload = &events[0].payload;
        assert_eq!(created_payload["routine_id"], json!(routine.id));
        assert_eq!(created_payload["title"], json!("Morning run"));
        assert_eq!(created_payload["kind"], json!("exercise"));
        assert_eq!(created_payload["cron"], json!("0 7 * * MON,WED,FRI"));
        assert_eq!(created_payload["status"], json!("active"));

        let updated_payload = &events[1].payload;
        // Full snapshot: the entity's OWN `id` field, not a `routine_id`
        // reference key (M5 — only the ad hoc payloads use `routine_id`).
        assert_eq!(updated_payload["id"], json!(routine.id));
        assert_eq!(updated_payload["title"], json!("Evening run"));
        assert_eq!(updated_payload["cron"], json!("0 19 * * MON,WED,FRI"));
        assert_eq!(updated_payload["tz"], json!(updated.routine.tz));
        assert_eq!(
            updated_payload["target_count"],
            json!(updated.routine.target_count)
        );
        let changed: Vec<String> =
            serde_json::from_value(updated_payload["changed"].clone()).unwrap();
        assert_eq!(changed, vec!["title".to_string(), "cron".to_string()]);
    }

    #[tokio::test]
    async fn routine_update_changes_fields_and_emits_exactly_one_event() {
        let store = new_store().await;
        let routine = create_ok(&store).await;

        let updated = store
            .update_routine(
                &routine.id,
                &RoutinePatch {
                    title: Some("Evening run".to_string()),
                    cron: Some("0 19 * * MON,WED,FRI".to_string()),
                    tz: Some("America/New_York".to_string()),
                    target_count: Some(Some(4)),
                    target_minutes: Some(None), // explicitly clear target_minutes
                },
            )
            .await
            .unwrap();

        assert_eq!(updated.routine.title, "Evening run");
        assert_eq!(updated.routine.cron, "0 19 * * MON,WED,FRI");
        assert_eq!(updated.routine.tz, "America/New_York");
        assert_eq!(updated.routine.target_count, Some(4));
        assert_eq!(updated.routine.target_minutes, None);
        assert_eq!(updated.routine.status, RoutineStatus::Active); // update never touches status
        let mut changed = updated.changed.clone();
        changed.sort_unstable();
        assert_eq!(
            changed,
            vec!["cron", "target_count", "target_minutes", "title", "tz"]
        );

        let n = test_hooks::event_count(&store, "routine", &routine.id)
            .await
            .unwrap();
        assert_eq!(n, 2); // created + updated
    }

    /// L1 (T3.1.1 review): a fully-empty patch is a no-op, AND — the actual
    /// point of L1 — so is a patch that re-states the CURRENT values. Both
    /// must write nothing (no event, `updated_at` untouched), in contrast to
    /// `routine_update_changes_fields_and_emits_exactly_one_event` which
    /// changes real values and DOES add one.
    #[tokio::test]
    async fn routine_update_noop_writes_no_event_and_leaves_updated_at_alone() {
        let store = new_store().await;
        let routine = create_ok(&store).await;

        let empty_patch = store
            .update_routine(&routine.id, &RoutinePatch::default())
            .await
            .unwrap();
        assert_eq!(empty_patch.routine, routine);
        assert!(empty_patch.changed.is_empty());

        let same_values_patch = store
            .update_routine(
                &routine.id,
                &RoutinePatch {
                    title: Some(routine.title.clone()),
                    cron: Some(routine.cron.clone()),
                    tz: Some(routine.tz.clone()),
                    target_count: Some(routine.target_count),
                    target_minutes: Some(routine.target_minutes),
                },
            )
            .await
            .unwrap();
        assert_eq!(same_values_patch.routine, routine);
        assert_eq!(same_values_patch.routine.updated_at, routine.updated_at);
        assert!(same_values_patch.changed.is_empty());

        let n = test_hooks::event_count(&store, "routine", &routine.id)
            .await
            .unwrap();
        assert_eq!(n, 1); // only `created` — neither no-op update added anything
    }

    #[tokio::test]
    async fn routine_update_rejects_invalid_cron_and_tz() {
        let store = new_store().await;
        let routine = create_ok(&store).await;

        let err = store
            .update_routine(
                &routine.id,
                &RoutinePatch {
                    cron: Some("garbage".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invalid(_)), "{err:?}");

        // H1 via the update path too: a digit weekday must be rejected here,
        // not just on create.
        let err = store
            .update_routine(
                &routine.id,
                &RoutinePatch {
                    cron: Some("0 7 * * 1-5".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invalid(_)), "{err:?}");

        let err = store
            .update_routine(
                &routine.id,
                &RoutinePatch {
                    tz: Some("Not/AZone".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invalid(_)), "{err:?}");

        // L2 via the update path: an explicit empty tz is also rejected.
        let err = store
            .update_routine(
                &routine.id,
                &RoutinePatch {
                    tz: Some(String::new()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invalid(_)), "{err:?}");

        let err = store
            .update_routine(
                &routine.id,
                &RoutinePatch {
                    target_count: Some(Some(0)),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invalid(_)), "{err:?}");

        // L4 via the update path: a blank title is also rejected.
        let err = store
            .update_routine(
                &routine.id,
                &RoutinePatch {
                    title: Some("   ".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invalid(_)), "{err:?}");

        // Rejected update must not have touched the row or appended an event.
        let still = store.get_routine(&routine.id).await.unwrap();
        assert_eq!(still, routine);
        let n = test_hooks::event_count(&store, "routine", &routine.id)
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    /// H2 (T3.1.1 review): a retired routine is a closed door for `update`,
    /// not just for `transition`. Paired positive control: the SAME patch
    /// succeeds while the routine is merely `paused`.
    #[tokio::test]
    async fn routine_update_on_retired_rejected_paused_accepted_positive_control() {
        let store = new_store().await;

        // Positive control first: paused accepts the update.
        let paused_routine = create_ok(&store).await;
        store
            .transition_routine(&paused_routine.id, RoutineStatus::Paused)
            .await
            .unwrap();
        let patch = RoutinePatch {
            title: Some("Renamed".to_string()),
            ..Default::default()
        };
        let updated = store
            .update_routine(&paused_routine.id, &patch)
            .await
            .unwrap();
        assert_eq!(updated.routine.title, "Renamed");
        assert_eq!(updated.changed, vec!["title"]);

        // Now the actual assertion: retired rejects the exact same patch.
        let retired_routine = create_ok(&store).await;
        store
            .transition_routine(&retired_routine.id, RoutineStatus::Retired)
            .await
            .unwrap();
        let err = store
            .update_routine(&retired_routine.id, &patch)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Conflict(_)), "{err:?}");

        // Row unchanged, and no `updated` event was appended (still exactly
        // created + retired).
        let still = store.get_routine(&retired_routine.id).await.unwrap();
        assert_eq!(still.title, "Morning run");
        let n = test_hooks::event_count(&store, "routine", &retired_routine.id)
            .await
            .unwrap();
        assert_eq!(n, 2);
    }

    // ----- transitions ------------------------------------------------------

    #[tokio::test]
    async fn routine_transition_active_paused_active_emits_paused_then_resumed() {
        let store = new_store().await;
        let routine = create_ok(&store).await;

        let paused = store
            .transition_routine(&routine.id, RoutineStatus::Paused)
            .await
            .unwrap();
        assert_eq!(paused.status, RoutineStatus::Paused);

        let resumed = store
            .transition_routine(&routine.id, RoutineStatus::Active)
            .await
            .unwrap();
        assert_eq!(resumed.status, RoutineStatus::Active);

        let n = test_hooks::event_count(&store, "routine", &routine.id)
            .await
            .unwrap();
        assert_eq!(n, 3); // created, paused, resumed
    }

    #[tokio::test]
    async fn routine_transition_to_retired_is_terminal() {
        let store = new_store().await;
        let routine = create_ok(&store).await;

        let retired = store
            .transition_routine(&routine.id, RoutineStatus::Retired)
            .await
            .unwrap();
        assert_eq!(retired.status, RoutineStatus::Retired);

        // Every possible destination from `retired` must be rejected —
        // including re-activating (the task's explicit "retired -> active
        // must be rejected" example) and re-pausing.
        for to in [
            RoutineStatus::Active,
            RoutineStatus::Paused,
            RoutineStatus::Retired,
        ] {
            let err = store.transition_routine(&routine.id, to).await.unwrap_err();
            assert!(matches!(err, StoreError::Transition(_)), "{to:?}: {err:?}");
        }

        // Rejected transitions must not have appended events: still exactly
        // created + retired.
        let n = test_hooks::event_count(&store, "routine", &routine.id)
            .await
            .unwrap();
        assert_eq!(n, 2);
    }

    #[tokio::test]
    async fn routine_transition_paused_to_retired_is_legal() {
        let store = new_store().await;
        let routine = create_ok(&store).await;
        store
            .transition_routine(&routine.id, RoutineStatus::Paused)
            .await
            .unwrap();
        let retired = store
            .transition_routine(&routine.id, RoutineStatus::Retired)
            .await
            .unwrap();
        assert_eq!(retired.status, RoutineStatus::Retired);
    }

    #[tokio::test]
    async fn routine_transition_unknown_id_is_not_found() {
        let store = new_store().await;
        let err = store
            .transition_routine("no-such-routine", RoutineStatus::Paused)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)), "{err:?}");
    }
}

// ----- T4.1.1: Review three-kind period store layer (design §2/§3.2/§4.1) -

#[cfg(test)]
mod review_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::store::test_hooks;

    async fn new_store() -> Sin90Store {
        Sin90Store::open_memory().await.unwrap()
    }

    fn nr(kind: ReviewKind, period: &str) -> NewReview {
        NewReview {
            kind,
            period: period.to_string(),
        }
    }

    async fn create_ok(store: &Sin90Store, kind: ReviewKind, period: &str) -> Review {
        store.create_review(&nr(kind, period)).await.unwrap()
    }

    // ----- creation: period format validation -------------------------------

    #[tokio::test]
    async fn review_create_daily_accepts_valid_date_rejects_invalid() {
        let store = new_store().await;
        let review = create_ok(&store, ReviewKind::Daily, "2026-09-24").await;
        assert_eq!(review.period, "2026-09-24");
        assert_eq!(review.status, ReviewStatus::Draft);
        assert_eq!(review.body, "");

        for bad in ["2026-13-01", "2026-02-30", "not-a-date", "2026-9-24"] {
            let err = store
                .create_review(&nr(ReviewKind::Daily, bad))
                .await
                .unwrap_err();
            assert!(matches!(err, StoreError::Invalid(_)), "{bad}: {err:?}");
        }
    }

    #[tokio::test]
    async fn review_create_weekly_accepts_valid_week_rejects_invalid() {
        let store = new_store().await;
        let review = create_ok(&store, ReviewKind::Weekly, "2026-w39").await;
        // Canonicalized, same convention `canonical_iso_week` guarantees for
        // `create_week`'s `iso_week` — lowercase `w` in, uppercase out.
        assert_eq!(review.period, "2026-W39");

        for bad in ["2026-W99", "2026-W00", "garbage", "2026-39"] {
            let err = store
                .create_review(&nr(ReviewKind::Weekly, bad))
                .await
                .unwrap_err();
            assert!(matches!(err, StoreError::Invalid(_)), "{bad}: {err:?}");
        }
    }

    /// A `rhythm` period must reference an EXISTING `sin90_rhythms.id` — a
    /// relational check, not a pure format one (`validate_review_period`'s
    /// doc). Positive control: seeding one via `test_hooks::insert_rhythm`
    /// (T3.4.1 has no production create path yet) makes the same call
    /// succeed.
    #[tokio::test]
    async fn review_create_rhythm_requires_existing_rhythm() {
        let store = new_store().await;
        let err = store
            .create_review(&nr(ReviewKind::Rhythm, "no-such-rhythm"))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)), "{err:?}");

        test_hooks::insert_rhythm(&store, "rhythm-1").await.unwrap();
        let review = create_ok(&store, ReviewKind::Rhythm, "rhythm-1").await;
        assert_eq!(review.period, "rhythm-1");
    }

    // ----- creation: UNIQUE(kind, period) ------------------------------------

    #[tokio::test]
    async fn review_create_duplicate_kind_period_conflicts_distinct_period_succeeds() {
        let store = new_store().await;
        create_ok(&store, ReviewKind::Daily, "2026-09-24").await;

        let err = store
            .create_review(&nr(ReviewKind::Daily, "2026-09-24"))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Conflict(_)), "{err:?}");

        // Positive control: a different period for the same kind succeeds.
        let other = store
            .create_review(&nr(ReviewKind::Daily, "2026-09-25"))
            .await
            .unwrap();
        assert_eq!(other.period, "2026-09-25");

        // Positive control: the SAME period under a different kind also
        // succeeds — the constraint is on the (kind, period) pair, not
        // `period` alone.
        let weekly = store
            .create_review(&nr(ReviewKind::Weekly, "2026-W40"))
            .await
            .unwrap();
        assert_eq!(weekly.period, "2026-W40");
    }

    // ----- update body: no-op writes zero events, real change writes one ----

    #[tokio::test]
    async fn review_update_body_noop_writes_zero_events_real_change_writes_one() {
        let store = new_store().await;
        let review = create_ok(&store, ReviewKind::Daily, "2026-09-24").await;

        // No-op: re-stating the current (empty) body.
        let outcome = store.update_review_body(&review.id, "").await.unwrap();
        assert!(!outcome.changed);
        assert_eq!(
            test_hooks::event_count(&store, "review", &review.id)
                .await
                .unwrap(),
            1, // only `created` so far
            "a no-op PATCH must not append an event"
        );

        // Positive control: an actual change appends exactly one more event.
        let outcome = store
            .update_review_body(&review.id, "today went well")
            .await
            .unwrap();
        assert!(outcome.changed);
        assert_eq!(outcome.review.body, "today went well");
        assert_eq!(
            test_hooks::event_count(&store, "review", &review.id)
                .await
                .unwrap(),
            2
        );
    }

    #[tokio::test]
    async fn review_update_body_on_finalized_is_conflict() {
        let store = new_store().await;
        let review = create_ok(&store, ReviewKind::Daily, "2026-09-24").await;
        store.finalize_review(&review.id).await.unwrap();

        let err = store
            .update_review_body(&review.id, "too late")
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Conflict(_)), "{err:?}");
    }

    // ----- finalize: draft -> finalized once, twice is a conflict -----------

    #[tokio::test]
    async fn review_finalize_draft_to_finalized_then_twice_is_conflict() {
        let store = new_store().await;
        let review = create_ok(&store, ReviewKind::Daily, "2026-09-24").await;

        let finalized = store.finalize_review(&review.id).await.unwrap();
        assert_eq!(finalized.status, ReviewStatus::Finalized);

        // Mutation target: removing `finalize_review`'s
        // `check_review_transition` call (or `review_transition_allowed`'s
        // `(Draft, Finalized)`-only arm) would let this second call succeed
        // instead of 409ing — this test goes red under either mutation.
        let err = store.finalize_review(&review.id).await.unwrap_err();
        assert!(matches!(err, StoreError::Transition(_)), "{err:?}");
    }

    // ----- exact event sequence: create -> update -> finalize ---------------

    #[tokio::test]
    async fn review_event_sequence_is_exact() {
        let store = new_store().await;
        let review = create_ok(&store, ReviewKind::Daily, "2026-09-24").await;
        store.update_review_body(&review.id, "notes").await.unwrap();
        store.finalize_review(&review.id).await.unwrap();

        let events = store
            .list_events(Some("review"), Some(&review.id), None, None)
            .await
            .unwrap();
        let seq: Vec<(String, Option<String>, Option<String>)> = events
            .iter()
            .map(|e| (e.kind.clone(), e.from_state.clone(), e.to_state.clone()))
            .collect();
        assert_eq!(
            seq,
            vec![
                ("created".to_string(), None, Some("draft".to_string())),
                ("updated".to_string(), None, None),
                (
                    "finalized".to_string(),
                    Some("draft".to_string()),
                    Some("finalized".to_string())
                ),
            ]
        );
    }

    // ----- reads --------------------------------------------------------

    #[tokio::test]
    async fn review_get_unknown_id_is_not_found() {
        let store = new_store().await;
        let err = store.get_review("no-such-review").await.unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)), "{err:?}");
    }

    #[tokio::test]
    async fn review_list_filters_by_kind_and_period() {
        let store = new_store().await;
        create_ok(&store, ReviewKind::Daily, "2026-09-24").await;
        create_ok(&store, ReviewKind::Daily, "2026-09-25").await;
        create_ok(&store, ReviewKind::Weekly, "2026-W39").await;

        let daily = store
            .list_reviews(Some(ReviewKind::Daily), None)
            .await
            .unwrap();
        assert_eq!(daily.len(), 2);
        assert!(daily.iter().all(|r| r.kind == ReviewKind::Daily));

        let one = store
            .list_reviews(Some(ReviewKind::Daily), Some("2026-09-24"))
            .await
            .unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].period, "2026-09-24");

        let all = store.list_reviews(None, None).await.unwrap();
        assert_eq!(all.len(), 3);
    }
}

// ----- T4.2.1: body_ref — one-way Markdown export on finalize (design §2 #11/
// §4.2, migration 0008) ------------------------------------------------------

#[cfg(test)]
mod body_ref_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::store::test_hooks;

    /// A fresh, unique directory under the OS temp dir, removed on drop — no
    /// `tempfile` dependency needed for this small a need.
    struct TempDataDir(std::path::PathBuf);

    impl TempDataDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("sin90-body-ref-test-{}", ulid()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDataDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn new_store_with_data_dir(dir: &TempDataDir) -> Sin90Store {
        Sin90Store::open_memory_with_data_dir(dir.path().to_path_buf())
            .await
            .unwrap()
    }

    fn nr(kind: ReviewKind, period: &str) -> NewReview {
        NewReview {
            kind,
            period: period.to_string(),
        }
    }

    // ----- happy path: file content == body, body_ref == expected relpath ---

    #[tokio::test]
    async fn body_ref_finalize_writes_file_matching_body_and_expected_relpath() {
        let tmp = TempDataDir::new();
        let store = new_store_with_data_dir(&tmp).await;
        let review = store
            .create_review(&nr(ReviewKind::Daily, "2026-09-24"))
            .await
            .unwrap();
        store
            .update_review_body(&review.id, "today went well")
            .await
            .unwrap();

        // The target directory does not exist yet — proves `finalize_review`
        // creates it (see the next assertions), not that it merely happened
        // to already be there.
        assert!(!tmp.path().join("reviews").exists());

        let finalized = store.finalize_review(&review.id).await.unwrap();
        assert_eq!(finalized.status, ReviewStatus::Finalized);
        assert_eq!(
            finalized.body_ref.as_deref(),
            Some("reviews/daily/2026-09-24.md")
        );

        let full_path = tmp.path().join("reviews/daily/2026-09-24.md");
        assert!(full_path.is_file());
        assert_eq!(
            std::fs::read_to_string(&full_path).unwrap(),
            "today went well"
        );

        // Re-fetching the row agrees with what `finalize_review` returned.
        let fetched = store.get_review(&review.id).await.unwrap();
        assert_eq!(
            fetched.body_ref.as_deref(),
            Some("reviews/daily/2026-09-24.md")
        );
    }

    /// `kind` participates in the relpath, and a `rhythm` period (a real
    /// `ulid()`-shaped `RhythmId`) passes the charset whitelist.
    #[tokio::test]
    async fn body_ref_rhythm_kind_relpath_uses_rhythm_directory() {
        let tmp = TempDataDir::new();
        let store = new_store_with_data_dir(&tmp).await;
        let rhythm_id = ulid();
        test_hooks::insert_rhythm(&store, &rhythm_id).await.unwrap();
        let review = store
            .create_review(&nr(ReviewKind::Rhythm, &rhythm_id))
            .await
            .unwrap();
        store
            .update_review_body(&review.id, "rhythm notes")
            .await
            .unwrap();

        let finalized = store.finalize_review(&review.id).await.unwrap();
        assert_eq!(
            finalized.body_ref.as_deref(),
            Some(format!("reviews/rhythm/{rhythm_id}.md").as_str())
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(format!("reviews/rhythm/{rhythm_id}.md")))
                .unwrap(),
            "rhythm notes"
        );
    }

    // ----- read API is one-way: a hand-edited .md never leaks back ----------

    #[tokio::test]
    async fn body_ref_hand_edited_md_does_not_affect_read_api() {
        let tmp = TempDataDir::new();
        let store = new_store_with_data_dir(&tmp).await;
        let review = store
            .create_review(&nr(ReviewKind::Daily, "2026-09-24"))
            .await
            .unwrap();
        store
            .update_review_body(&review.id, "original body")
            .await
            .unwrap();
        let finalized = store.finalize_review(&review.id).await.unwrap();
        let relpath = finalized.body_ref.clone().unwrap();

        // Hand-edit the exported file — simulating a human/tool touching the
        // one-way export directly.
        std::fs::write(tmp.path().join(&relpath), "TAMPERED").unwrap();

        // Positive control baked into the same assertion: the file itself
        // really did change...
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(&relpath)).unwrap(),
            "TAMPERED"
        );
        // ...but every read path still answers from SQLite.
        let via_get = store.get_review(&review.id).await.unwrap();
        assert_eq!(via_get.body, "original body");
        let via_list = store
            .list_reviews(Some(ReviewKind::Daily), Some("2026-09-24"))
            .await
            .unwrap();
        assert_eq!(via_list[0].body, "original body");
    }

    // ----- defense-in-depth: illegal `period` rejected, no file written -----

    /// Direct unit coverage of [`review_markdown_relpath`] (bypassing
    /// `create_review`'s own validation entirely — the whole point is to
    /// prove THIS function does not trust its caller). Mutation target:
    /// deleting the path-component checks in `review_markdown_relpath`
    /// turns this red.
    #[test]
    fn body_ref_relpath_rejects_unsafe_period_components() {
        for bad in ["../x", "a/b", ".hidden", "a\\b", "a..b", "", ".", ".."] {
            let err = review_markdown_relpath(ReviewKind::Daily, bad).unwrap_err();
            assert!(matches!(err, StoreError::Invalid(_)), "{bad:?}: {err:?}");
        }
        // NUL is legal Rust `&str` content but never a legal filename byte.
        let with_nul = "2026-09\0-24";
        let err = review_markdown_relpath(ReviewKind::Daily, with_nul).unwrap_err();
        assert!(matches!(err, StoreError::Invalid(_)));

        // Positive control: a canonical period is accepted.
        assert_eq!(
            review_markdown_relpath(ReviewKind::Daily, "2026-09-24").unwrap(),
            "reviews/daily/2026-09-24.md"
        );
    }

    /// A `rhythm` period that dodges the generic separator/`..`/leading-`.`
    /// checks (no slash, no dot) but is not shaped like a `ulid()` (contains
    /// a hyphen, which `ULID_ALPHABET` excludes) is still rejected — the
    /// charset whitelist is a SEPARATE defense from the generic component
    /// check, not implied by it.
    #[test]
    fn body_ref_relpath_rejects_non_ulid_rhythm_period() {
        let err = review_markdown_relpath(ReviewKind::Rhythm, "rhythm-1").unwrap_err();
        assert!(matches!(err, StoreError::Invalid(_)), "{err:?}");
        // Same string IS accepted for daily/weekly (no charset restriction
        // there — the generic component check is the only gate) — proves
        // the whitelist is rhythm-specific, not a blanket ASCII rule.
        assert_eq!(
            review_markdown_relpath(ReviewKind::Daily, "rhythm-1").unwrap(),
            "reviews/daily/rhythm-1.md"
        );
    }

    /// End-to-end: a `sin90_reviews` row with an illegal `period` — crafted
    /// via raw SQL, the only way to get one past `create_review`'s own
    /// validation (format-checked for daily/weekly; existence-checked
    /// against a real `sin90_rhythms.id` for rhythm — see
    /// `validate_review_period`'s and `create_review`'s docs) — makes
    /// `finalize_review` fail, leaves the review `draft`, appends no
    /// `finalized` event, and writes NO file anywhere under `data_dir`
    /// (proving the rejection happens before any filesystem write is
    /// attempted, not just that the wrong file didn't happen to appear).
    #[tokio::test]
    async fn body_ref_finalize_rejects_illegal_period_writes_no_file() {
        let tmp = TempDataDir::new();
        let store = new_store_with_data_dir(&tmp).await;

        let now = crate::core::now_iso8601();
        sqlx::query(
            "INSERT INTO sin90_reviews (id, kind, status, week_id, period, body, created_at, updated_at)
             VALUES ('legacy-bad-period', 'daily', 'draft', NULL, '../x', 'body text', ?, ?)",
        )
        .bind(&now)
        .bind(&now)
        .execute(store.pool())
        .await
        .unwrap();

        let err = store
            .finalize_review("legacy-bad-period")
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invalid(_)), "{err:?}");

        let still_draft = store.get_review("legacy-bad-period").await.unwrap();
        assert_eq!(still_draft.status, ReviewStatus::Draft);
        assert_eq!(still_draft.body_ref, None);
        assert_eq!(
            test_hooks::event_count(&store, "review", "legacy-bad-period")
                .await
                .unwrap(),
            0,
            "no finalized event (and no created event either — this row was inserted via raw SQL)"
        );

        // Nothing was written anywhere under `data_dir` — not even a
        // `reviews/` directory, let alone a file outside it via `../x`.
        assert!(
            std::fs::read_dir(tmp.path()).unwrap().next().is_none(),
            "data_dir must be untouched"
        );
        assert!(
            !tmp.path().parent().unwrap().join("x").exists(),
            "must not have escaped data_dir via `../x`"
        );
    }

    // ----- directory auto-created when missing -------------------------------

    #[tokio::test]
    async fn body_ref_finalize_creates_reviews_directory_when_missing() {
        let tmp = TempDataDir::new();
        let store = new_store_with_data_dir(&tmp).await;
        assert!(!tmp.path().join("reviews").exists());

        let review = store
            .create_review(&nr(ReviewKind::Weekly, "2026-W39"))
            .await
            .unwrap();
        store.finalize_review(&review.id).await.unwrap();

        assert!(tmp.path().join("reviews/weekly").is_dir());
        assert!(tmp.path().join("reviews/weekly/2026-W39.md").is_file());
    }

    // ----- file-write failure leaves the review draft, no finalized event ---

    /// `<data_dir>/reviews` is occupied by a plain FILE (not a directory) —
    /// `create_dir_all("<data_dir>/reviews/<kind>")` must fail, and that
    /// failure must leave `finalize_review` a no-op on the DB side: still
    /// `draft`, no `finalized` event. Mutation target: swapping
    /// `finalize_review`'s ordering to "commit first, write the file after"
    /// turns this red (the row would show `finalized` despite the write
    /// failure).
    #[tokio::test]
    async fn body_ref_file_write_failure_leaves_review_draft_no_finalized_event() {
        let tmp = TempDataDir::new();
        // Occupy the spot `reviews/` needs to become a directory.
        std::fs::write(tmp.path().join("reviews"), b"not a directory").unwrap();

        let store = new_store_with_data_dir(&tmp).await;
        let review = store
            .create_review(&nr(ReviewKind::Daily, "2026-09-24"))
            .await
            .unwrap();
        let events_before = test_hooks::event_count(&store, "review", &review.id)
            .await
            .unwrap();
        assert_eq!(events_before, 1, "only `created` so far");

        let err = store.finalize_review(&review.id).await.unwrap_err();
        assert!(matches!(err, StoreError::Internal(_)), "{err:?}");

        let still_draft = store.get_review(&review.id).await.unwrap();
        assert_eq!(still_draft.status, ReviewStatus::Draft);
        assert_eq!(still_draft.body_ref, None);
        assert_eq!(
            test_hooks::event_count(&store, "review", &review.id)
                .await
                .unwrap(),
            events_before,
            "no `finalized` event on a failed write"
        );
    }

    // ----- data_dir absent (in-memory/dev mode): degrade, don't fail --------

    /// Positive control for the failure test above: with NO `data_dir` at
    /// all (plain `open_memory()`, no override), `finalize_review` still
    /// succeeds — there is simply nowhere to export to, so the export step
    /// is skipped and `body_ref` stays `None`. This is the same
    /// `Sin90Store` every other Review test in `review_tests` already uses.
    #[tokio::test]
    async fn body_ref_no_data_dir_finalize_still_succeeds_with_null_body_ref() {
        let store = Sin90Store::open_memory().await.unwrap();
        let review = store
            .create_review(&nr(ReviewKind::Daily, "2026-09-24"))
            .await
            .unwrap();
        let finalized = store.finalize_review(&review.id).await.unwrap();
        assert_eq!(finalized.status, ReviewStatus::Finalized);
        assert_eq!(finalized.body_ref, None);
    }

    // ----- migration 0008: rows from before this column existed stay NULL ---

    /// Inserts a row the way any pre-0008 `INSERT INTO sin90_reviews` would
    /// have (no `body_ref` in the column list — that column did not exist
    /// yet) — proves `ALTER TABLE ... ADD COLUMN body_ref TEXT NULL`
    /// (migration 0008) leaves such a row's `body_ref` as `NULL`, not some
    /// other default.
    #[tokio::test]
    async fn body_ref_pre_0008_row_reads_back_as_null() {
        let store = Sin90Store::open_memory().await.unwrap();
        let now = crate::core::now_iso8601();
        sqlx::query(
            "INSERT INTO sin90_reviews (id, kind, status, week_id, period, body, created_at, updated_at)
             VALUES ('legacy-1', 'daily', 'draft', NULL, '2026-01-01', '', ?, ?)",
        )
        .bind(&now)
        .bind(&now)
        .execute(store.pool())
        .await
        .unwrap();

        let row = store.get_review("legacy-1").await.unwrap();
        assert_eq!(row.body_ref, None);
    }
}

// ----- T3.3.1: Routine changes write `sin90_outbox` in the same tx --------

#[cfg(test)]
mod outbox_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::routine_tests::{create_ok, nr};
    use super::*;
    use crate::core::RoutineKind;
    use crate::store::test_hooks;

    async fn new_store() -> Sin90Store {
        Sin90Store::open_memory().await.unwrap()
    }

    // ----- collapsing repeated changes to one pending row ------------------

    /// Create → change cron → pause, all on the same Routine: exactly one
    /// `sin90_outbox` row survives for its `dedup_key`, holding the LATEST
    /// desired state (new cron, `enabled: false`) — not a queue of three
    /// stale intents.
    #[tokio::test]
    async fn outbox_repeated_routine_changes_collapse_to_one_pending_row() {
        let store = new_store().await;
        let routine = store
            .create_routine(&nr(
                "Morning run",
                RoutineKind::Exercise,
                "0 7 * * MON,WED,FRI",
            ))
            .await
            .unwrap();
        let dedup_key = routine_dedup_key(&routine.id);

        store
            .update_routine(
                &routine.id,
                &RoutinePatch {
                    cron: Some("0 8 * * MON,WED,FRI".to_string()),
                    ..RoutinePatch::default()
                },
            )
            .await
            .unwrap();
        store
            .transition_routine(&routine.id, RoutineStatus::Paused)
            .await
            .unwrap();

        let rows = test_hooks::outbox_rows_for(&store, &dedup_key)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        let row = &rows[0];
        assert_eq!(row.status, "pending");
        assert_eq!(row.kind, "scheduler.upsert");
        assert_eq!(row.desired["key"], json!(format!("routine.{}", routine.id)));
        assert_eq!(row.desired["spec"]["cron"], json!("0 8 * * MON,WED,FRI"));
        assert_eq!(row.desired["enabled"], json!(false));
    }

    /// Retiring a Routine that already has a pending `scheduler.upsert` row
    /// (from create) overwrites it in place with a `scheduler.delete` — still
    /// exactly one row for the `dedup_key`.
    #[tokio::test]
    async fn outbox_retire_replaces_pending_row_with_scheduler_delete() {
        let store = new_store().await;
        let routine = create_ok(&store).await;
        let dedup_key = routine_dedup_key(&routine.id);

        store
            .transition_routine(&routine.id, RoutineStatus::Retired)
            .await
            .unwrap();

        let rows = test_hooks::outbox_rows_for(&store, &dedup_key)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        let row = &rows[0];
        assert_eq!(row.status, "pending");
        assert_eq!(row.kind, "scheduler.delete");
        assert_eq!(
            row.desired,
            json!({"key": format!("routine.{}", routine.id)})
        );
    }

    // ----- title doesn't write outbox; cron does (positive control) --------

    #[tokio::test]
    async fn outbox_title_only_update_does_not_touch_outbox() {
        let store = new_store().await;
        let routine = create_ok(&store).await;
        let dedup_key = routine_dedup_key(&routine.id);
        let before = test_hooks::outbox_rows_for(&store, &dedup_key)
            .await
            .unwrap();
        assert_eq!(before.len(), 1);

        let update = store
            .update_routine(
                &routine.id,
                &RoutinePatch {
                    title: Some("Evening run".to_string()),
                    ..RoutinePatch::default()
                },
            )
            .await
            .unwrap();
        assert!(update.changed.contains(&"title"));

        let after = test_hooks::outbox_rows_for(&store, &dedup_key)
            .await
            .unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(before[0].id, after[0].id);
        assert_eq!(before[0].desired, after[0].desired);
        assert_eq!(before[0].status, after[0].status);
        assert_eq!(before[0].attempts, after[0].attempts);
    }

    /// Positive control for the test above: a `cron` change on the SAME kind
    /// of routine DOES move the outbox row's `desired`.
    #[tokio::test]
    async fn outbox_cron_update_changes_outbox_positive_control() {
        let store = new_store().await;
        let routine = create_ok(&store).await;
        let dedup_key = routine_dedup_key(&routine.id);
        let before = test_hooks::outbox_rows_for(&store, &dedup_key)
            .await
            .unwrap();

        let update = store
            .update_routine(
                &routine.id,
                &RoutinePatch {
                    cron: Some("0 9 * * MON,WED,FRI".to_string()),
                    ..RoutinePatch::default()
                },
            )
            .await
            .unwrap();
        assert!(update.changed.contains(&"cron"));

        let after = test_hooks::outbox_rows_for(&store, &dedup_key)
            .await
            .unwrap();
        assert_eq!(after.len(), 1);
        assert_ne!(before[0].desired, after[0].desired);
        assert_eq!(
            after[0].desired["spec"]["cron"],
            json!("0 9 * * MON,WED,FRI")
        );
    }

    // ----- failed rows reset on the next change -----------------------------

    #[tokio::test]
    async fn outbox_failed_row_resets_to_pending_on_next_routine_change() {
        let store = new_store().await;
        let routine = create_ok(&store).await;
        let dedup_key = routine_dedup_key(&routine.id);
        let rows = test_hooks::outbox_rows_for(&store, &dedup_key)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        test_hooks::mark_outbox_failed(&store, &rows[0].id, "quota_exceeded")
            .await
            .unwrap();
        let failed = test_hooks::outbox_rows_for(&store, &dedup_key)
            .await
            .unwrap();
        assert_eq!(failed[0].status, "failed");
        assert_eq!(failed[0].failure_kind.as_deref(), Some("quota_exceeded"));
        assert_eq!(failed[0].attempts, 3);

        store
            .update_routine(
                &routine.id,
                &RoutinePatch {
                    cron: Some("0 6 * * MON,WED,FRI".to_string()),
                    ..RoutinePatch::default()
                },
            )
            .await
            .unwrap();

        let after = test_hooks::outbox_rows_for(&store, &dedup_key)
            .await
            .unwrap();
        assert_eq!(after.len(), 1, "{after:?}");
        assert_eq!(
            after[0].id, failed[0].id,
            "same row overwritten, not a new one"
        );
        assert_eq!(after[0].status, "pending");
        assert_eq!(after[0].attempts, 0);
        assert_eq!(after[0].failure_kind, None);
        assert_eq!(after[0].last_error, None);
        assert_eq!(after[0].next_attempt_at, None);
        assert_eq!(
            after[0].desired["spec"]["cron"],
            json!("0 6 * * MON,WED,FRI")
        );
    }

    // ----- same-transaction rollback leaves no residue ----------------------

    /// Test-only (NOT called from any production path): writes one outbox
    /// row via the real `upsert_outbox`, then — in the SAME transaction —
    /// runs a statement guaranteed to fail (a `sin90_routines.status` CHECK
    /// violation). Proves `upsert_outbox`'s "same transaction as the
    /// caller" contract actually rolls back when the CALLER's other write
    /// fails, not just when `upsert_outbox` itself errors.
    async fn write_outbox_then_fail(store: &Sin90Store, dedup_key: &str) -> Result<()> {
        let mut tx = store.pool().begin_with("BEGIN IMMEDIATE").await?;
        upsert_outbox(
            &mut tx,
            "scheduler.upsert",
            dedup_key,
            &json!({
                "key": "routine.doesnotmatter",
                "spec": {"cron": "* * * * *", "tz": "UTC"},
                "enabled": true,
            }),
        )
        .await?;
        // Guaranteed CHECK(status IN ('active','paused','retired')) violation
        // (migration 0004) -> this `?` propagates an Err and `tx` is dropped
        // without `commit()`, rolling back everything written above too.
        sqlx::query(
            "INSERT INTO sin90_routines
                 (id, title, kind, cron, tz, status, created_at, updated_at)
             VALUES ('rollback-test', 'x', 'other', '* * * * *', 'UTC',
                      'not-a-real-status', 't', 't')",
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    #[tokio::test]
    async fn outbox_transaction_rollback_leaves_no_residue() {
        let store = new_store().await;
        let dedup_key = "routine:rollback-test";
        let before = test_hooks::outbox_count(&store).await.unwrap();

        let err = write_outbox_then_fail(&store, dedup_key).await.unwrap_err();
        assert!(matches!(err, StoreError::Sqlx(_)), "{err:?}");

        let after = test_hooks::outbox_count(&store).await.unwrap();
        assert_eq!(
            after, before,
            "a failed transaction must leave no outbox row behind"
        );
        let rows = test_hooks::outbox_rows_for(&store, dedup_key)
            .await
            .unwrap();
        assert!(rows.is_empty(), "{rows:?}");
    }

    // ----- migrating an existing db leaves existing outbox rows alone ------

    /// A `sin90.db` that only has migrations 0001-0004 applied (T3.2.2
    /// hasn't started, so there is no `routine_fires` migration in this
    /// worktree yet — 0004 IS "the old state" here; this migration took the
    /// `0005` slot instead of spec.md's originally-pre-allocated `0006`,
    /// see this file's own header comment and DESIGN §4.1) gets a
    /// `failed`-capable outbox after migration 0005 runs, but every
    /// existing row's original columns (including a `done` row's
    /// `done_at`) are untouched, and the four new columns land at their
    /// "not yet retried" defaults.
    #[tokio::test]
    async fn outbox_migration_0005_preserves_existing_rows() {
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
        use std::str::FromStr;

        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();

        for file in [
            "0001_sin90.sql",
            "0002_lifeos.sql",
            "0003_week_iso_unique.sql",
            "0004_routines.sql",
        ] {
            let sql = std::fs::read_to_string(format!("./src/store/migrations/{file}")).unwrap();
            sqlx::raw_sql(&sql).execute(&pool).await.unwrap();
        }

        sqlx::query(
            "INSERT INTO sin90_outbox (id, kind, dedup_key, desired, status, created_at, done_at)
             VALUES ('old-1', 'scheduler.upsert', 'routine:pre-existing',
                      '{\"key\":\"routine.pre-existing\"}', 'pending',
                      '2026-01-01T00:00:00Z', NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sin90_outbox (id, kind, dedup_key, desired, status, created_at, done_at)
             VALUES ('old-2', 'scheduler.delete', 'routine:gone',
                      '{\"key\":\"routine.gone\"}', 'done',
                      '2026-01-01T00:00:00Z', '2026-01-02T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let sql = std::fs::read_to_string("./src/store/migrations/0005_outbox_failed.sql").unwrap();
        sqlx::raw_sql(&sql).execute(&pool).await.unwrap();

        let row = sqlx::query(
            "SELECT kind, dedup_key, desired, status, created_at, done_at,
                    attempts, failure_kind, last_error, next_attempt_at
             FROM sin90_outbox WHERE id = 'old-1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.get::<String, _>("kind"), "scheduler.upsert");
        assert_eq!(row.get::<String, _>("dedup_key"), "routine:pre-existing");
        assert_eq!(row.get::<String, _>("status"), "pending");
        assert_eq!(row.get::<String, _>("created_at"), "2026-01-01T00:00:00Z");
        assert_eq!(row.get::<Option<String>, _>("done_at"), None);
        assert_eq!(row.get::<i64, _>("attempts"), 0);
        assert_eq!(row.get::<Option<String>, _>("failure_kind"), None);
        assert_eq!(row.get::<Option<String>, _>("last_error"), None);
        assert_eq!(row.get::<Option<String>, _>("next_attempt_at"), None);

        let row2 = sqlx::query("SELECT status, done_at FROM sin90_outbox WHERE id = 'old-2'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row2.get::<String, _>("status"), "done");
        assert_eq!(row2.get::<String, _>("done_at"), "2026-01-02T00:00:00Z");
    }

    // ----- migration directory has no numeric gaps --------------------------

    /// Review follow-up (T3.3.1): `sqlx::migrate!` applies files in
    /// version-number order, so a GAP in the numbering (e.g.
    /// `0001,0002,0003,0004,0006` with no `0005`) is a live footgun — a
    /// LATER change that fills the gap ships a migration numbered lower
    /// than one that already shipped, so it applies AFTER its "true"
    /// chronological place on any db that already upgraded past the gap
    /// (exactly the mistake this task's own migration almost made — see
    /// `0005_outbox_failed.sql`'s header comment). This asserts
    /// `src/store/migrations/` has no such gap: version numbers, sorted,
    /// are exactly `1..=N` for some `N` — same prefix convention
    /// `sqlx::migrate!` itself parses (`NNNN_description.sql`).
    #[tokio::test]
    async fn outbox_migrations_are_contiguous_no_gaps() {
        let mut versions: Vec<u32> = std::fs::read_dir("./src/store/migrations")
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".sql"))
            .map(|name| {
                let prefix = name.split('_').next().unwrap_or(&name).to_string();
                prefix.parse::<u32>().unwrap_or_else(|e| {
                    panic!("migration file {name:?} has no numeric prefix: {e}")
                })
            })
            .collect();
        versions.sort_unstable();

        assert!(!versions.is_empty(), "no migration files found");
        let expected: Vec<u32> = (1..=versions.len() as u32).collect();
        assert_eq!(
            versions, expected,
            "migration version numbers must be contiguous starting at 1, no gaps \
             (found {versions:?}) — a gap means a future migration filling it in \
             would apply out of chronological order on an already-upgraded db"
        );
    }
}

// ----- T4.1.1: migration 0007 backfill on an existing (0006-era) db -------

#[cfg(test)]
mod review_migration_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use sqlx::Row;
    use std::str::FromStr;

    /// A `sin90.db` with only migrations 0001-0006 applied — pre-`period`,
    /// the state every real user's db is in before this task's migration.
    async fn old_pool() -> sqlx::SqlitePool {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        for file in [
            "0001_sin90.sql",
            "0002_lifeos.sql",
            "0003_week_iso_unique.sql",
            "0004_routines.sql",
            "0005_outbox_failed.sql",
            "0006_routine_fires.sql",
        ] {
            let sql = std::fs::read_to_string(format!("./src/store/migrations/{file}")).unwrap();
            sqlx::raw_sql(&sql).execute(&pool).await.unwrap();
        }
        pool
    }

    /// Backfill rule (0007's own header comment, and T4.1.1's task doc): a
    /// pre-existing row whose `week_id` resolves to a real `sin90_weeks` row
    /// gets that week's `iso_week` as its `period`; a row that does NOT
    /// resolve that way gets `'legacy-' || id`. This db predates ANY code
    /// path that ever wrote `sin90_reviews` (0007's comment), so both rows
    /// here are inserted by hand, standing in for the "if such a row existed"
    /// case the migration must still handle correctly.
    #[tokio::test]
    async fn migration_0007_backfills_week_id_rows_and_legacy_placeholders() {
        let pool = old_pool().await;

        sqlx::query(
            "INSERT INTO sin90_weeks (id, status, iso_week, created_at, updated_at)
             VALUES ('week-1', 'planning', '2026-W39', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sin90_reviews (id, kind, status, week_id, body, created_at, updated_at)
             VALUES ('rev-with-week', 'weekly', 'draft', 'week-1', '', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sin90_reviews (id, kind, status, week_id, body, created_at, updated_at)
             VALUES ('rev-orphan', 'daily', 'draft', NULL, '', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        // A second orphan of the SAME kind — proves the `legacy-<id>`
        // fallback is unique per row (guards the `UNIQUE(kind, period)`
        // index below from ever rejecting the migration itself).
        sqlx::query(
            "INSERT INTO sin90_reviews (id, kind, status, week_id, body, created_at, updated_at)
             VALUES ('rev-orphan-2', 'daily', 'draft', NULL, '', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let sql = std::fs::read_to_string("./src/store/migrations/0007_review_period.sql").unwrap();
        sqlx::raw_sql(&sql).execute(&pool).await.unwrap();

        let with_week: String =
            sqlx::query("SELECT period FROM sin90_reviews WHERE id = 'rev-with-week'")
                .fetch_one(&pool)
                .await
                .unwrap()
                .get("period");
        assert_eq!(with_week, "2026-W39", "backfilled from the row's week_id");

        let orphan: String =
            sqlx::query("SELECT period FROM sin90_reviews WHERE id = 'rev-orphan'")
                .fetch_one(&pool)
                .await
                .unwrap()
                .get("period");
        assert_eq!(orphan, "legacy-rev-orphan");

        let orphan2: String =
            sqlx::query("SELECT period FROM sin90_reviews WHERE id = 'rev-orphan-2'")
                .fetch_one(&pool)
                .await
                .unwrap()
                .get("period");
        assert_eq!(orphan2, "legacy-rev-orphan-2");
        assert_ne!(
            orphan, orphan2,
            "each legacy row must get a distinct period"
        );

        // The UNIQUE(kind, period) index itself must now exist and be live.
        let dup = sqlx::query(
            "INSERT INTO sin90_reviews (id, kind, status, week_id, period, body, created_at, updated_at)
             VALUES ('rev-dup', 'weekly', 'draft', NULL, '2026-W39', '', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await;
        assert!(
            dup.is_err(),
            "UNIQUE(kind, period) must reject a second weekly/2026-W39 row"
        );
    }
}

// ----- T3.2.2: fired receipt dedup + event (design §2 #16, spec.md M3) ----

#[cfg(test)]
mod fired_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::routine_tests::{create_ok, nr};
    use super::*;
    use crate::core::{FireTrigger, RoutineKind};
    use crate::store::test_hooks;

    async fn new_store() -> Sin90Store {
        Sin90Store::open_memory().await.unwrap()
    }

    // ----- same fire_id twice -> one row, one event ------------------------

    /// The kernel's at-least-once retry sends the SAME `fire_id` twice for
    /// one due slot (design doc §4.1/§4.2) — recording it must collapse to
    /// exactly one `sin90_routine_fires` row and exactly one `routine.fired`
    /// event, and BOTH calls must report success (idempotent 2xx, not the
    /// second one erroring).
    #[tokio::test]
    async fn fired_duplicate_fire_id_collapses_to_one_row_one_event() {
        let store = new_store().await;
        let routine = create_ok(&store).await;

        let first = store
            .record_routine_fire(
                "fire-dup-1",
                &format!("routine.{}", routine.id),
                "2026-09-24T07:00:00Z",
                FireTrigger::Tick,
            )
            .await
            .unwrap();
        assert!(
            matches!(first, RoutineFireOutcome::Recorded { .. }),
            "{first:?}"
        );

        let second = store
            .record_routine_fire(
                "fire-dup-1",
                &format!("routine.{}", routine.id),
                "2026-09-24T07:00:00Z",
                FireTrigger::Tick,
            )
            .await
            .unwrap();
        assert_eq!(
            second,
            RoutineFireOutcome::Duplicate {
                routine_id: routine.id.clone()
            }
        );

        assert_eq!(
            test_hooks::routine_fire_count(&store, &routine.id)
                .await
                .unwrap(),
            1
        );
        // `routine.created` already wrote one event at create_ok() time — so
        // exactly one MORE (`fired`) event exists for this routine, not two.
        assert_eq!(
            test_hooks::event_count(&store, "routine", &routine.id)
                .await
                .unwrap(),
            2,
            "expected exactly `created` + one `fired` (not two `fired`s)"
        );
    }

    /// Positive control for the test above: two DISTINCT `fire_id`s (a real
    /// second due slot, not a retry) leave two rows and two `fired` events.
    #[tokio::test]
    async fn fired_distinct_fire_ids_leave_two_rows_positive_control() {
        let store = new_store().await;
        let routine = create_ok(&store).await;

        store
            .record_routine_fire(
                "fire-a",
                &format!("routine.{}", routine.id),
                "2026-09-24T07:00:00Z",
                FireTrigger::Tick,
            )
            .await
            .unwrap();
        store
            .record_routine_fire(
                "fire-b",
                &format!("routine.{}", routine.id),
                "2026-09-26T07:00:00Z",
                FireTrigger::Tick,
            )
            .await
            .unwrap();

        assert_eq!(
            test_hooks::routine_fire_count(&store, &routine.id)
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            test_hooks::event_count(&store, "routine", &routine.id)
                .await
                .unwrap(),
            3,
            "created + two distinct fired events"
        );
    }

    // ----- unknown key / retired routine: 200-shaped outcome, no row, no event

    #[tokio::test]
    async fn fired_unknown_key_records_nothing() {
        let store = new_store().await;
        // No routine exists at all yet — this key can never resolve.
        let outcome = store
            .record_routine_fire(
                "fire-unknown",
                "routine.does-not-exist",
                "2026-09-24T07:00:00Z",
                FireTrigger::Tick,
            )
            .await
            .unwrap();
        assert_eq!(outcome, RoutineFireOutcome::UnknownKey);
        assert_eq!(
            test_hooks::routine_fire_count(&store, "does-not-exist")
                .await
                .unwrap(),
            0
        );
    }

    /// A key that isn't even shaped `routine.<id>` is ALSO `UnknownKey`, not
    /// a crash/500 — the kernel only ever sends keys this module minted
    /// itself, but a stale/foreign key must not panic the handler.
    #[tokio::test]
    async fn fired_malformed_key_is_unknown_not_an_error() {
        let store = new_store().await;
        let outcome = store
            .record_routine_fire(
                "fire-malformed",
                "not-a-routine-key",
                "2026-09-24T07:00:00Z",
                FireTrigger::Tick,
            )
            .await
            .unwrap();
        assert_eq!(outcome, RoutineFireOutcome::UnknownKey);
    }

    #[tokio::test]
    async fn fired_retired_routine_records_nothing() {
        let store = new_store().await;
        let routine = create_ok(&store).await;
        store
            .transition_routine(&routine.id, RoutineStatus::Retired)
            .await
            .unwrap();

        let outcome = store
            .record_routine_fire(
                "fire-after-retire",
                &format!("routine.{}", routine.id),
                "2026-09-24T07:00:00Z",
                FireTrigger::Tick,
            )
            .await
            .unwrap();
        assert_eq!(
            outcome,
            RoutineFireOutcome::RoutineRetired {
                routine_id: routine.id.clone()
            }
        );
        assert_eq!(
            test_hooks::routine_fire_count(&store, &routine.id)
                .await
                .unwrap(),
            0
        );
        // create + retire = 2 events; no `fired` got appended on top.
        assert_eq!(
            test_hooks::event_count(&store, "routine", &routine.id)
                .await
                .unwrap(),
            2
        );
    }

    // ----- /today "fired today" section -------------------------------------

    #[tokio::test]
    async fn fired_today_view_lists_routines_fired_today_not_yesterday() {
        let store = new_store().await;
        let today_routine = store
            .create_routine(&nr(
                "Today run",
                RoutineKind::Exercise,
                "0 7 * * MON,WED,FRI",
            ))
            .await
            .unwrap();
        let yesterday_routine = store
            .create_routine(&nr(
                "Yesterday run",
                RoutineKind::Exercise,
                "0 7 * * MON,WED,FRI",
            ))
            .await
            .unwrap();

        // `received_at` is always server-assigned to the real "now" inside
        // `record_routine_fire` (the client-supplied `scheduled_for` below is
        // irrelevant to the day-boundary check) — so this call alone lands
        // `today_routine` in today's bucket with no backdating needed.
        store
            .record_routine_fire(
                "fire-today",
                &format!("routine.{}", today_routine.id),
                "2026-09-24T07:00:00Z",
                FireTrigger::Tick,
            )
            .await
            .unwrap();
        store
            .record_routine_fire(
                "fire-yesterday",
                &format!("routine.{}", yesterday_routine.id),
                "2026-09-23T07:00:00Z",
                FireTrigger::Tick,
            )
            .await
            .unwrap();
        // Backdate the "yesterday" row's `received_at` itself to a real day
        // boundary in the past — a fixed far-past date, not a sleep, and not
        // tied to whatever the real wall-clock date happens to be when this
        // test runs (`today_view` filters on `received_at`, not
        // `scheduled_for`, so without this the test asserts nothing about
        // the boundary it actually checks).
        test_hooks::set_routine_fire_received_at(&store, "fire-yesterday", "2020-01-01T00:00:00Z")
            .await
            .unwrap();

        let view = store.today_view().await.unwrap();
        let ids: Vec<&str> = view.fired_routines.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&today_routine.id.as_str()), "{ids:?}");
        assert!(!ids.contains(&yesterday_routine.id.as_str()), "{ids:?}");
    }

    /// A routine that fired twice today appears exactly once.
    #[tokio::test]
    async fn fired_today_view_dedups_a_routine_fired_twice_today() {
        let store = new_store().await;
        let routine = create_ok(&store).await;
        store
            .record_routine_fire(
                "fire-1",
                &format!("routine.{}", routine.id),
                "2026-09-24T07:00:00Z",
                FireTrigger::Tick,
            )
            .await
            .unwrap();
        store
            .record_routine_fire(
                "fire-2",
                &format!("routine.{}", routine.id),
                "2026-09-24T08:00:00Z",
                FireTrigger::RunNow,
            )
            .await
            .unwrap();

        let view = store.today_view().await.unwrap();
        let count = view
            .fired_routines
            .iter()
            .filter(|r| r.id == routine.id)
            .count();
        assert_eq!(count, 1, "{:?}", view.fired_routines);
    }
}

// ----- T4.3.2: review Routine fired -> auto weekly draft (spec.md M4) -----

#[cfg(test)]
mod review_routine_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::routine_tests::nr;
    use super::*;
    use crate::core::RoutineKind;

    async fn new_store() -> Sin90Store {
        Sin90Store::open_memory().await.unwrap()
    }

    async fn create_review_routine(store: &Sin90Store) -> Routine {
        store
            .create_routine(&nr("Weekly review", RoutineKind::Review, "0 18 * * SUN"))
            .await
            .unwrap()
    }

    /// Reads a Review's `body`/`status` back by id — small local helper so
    /// the tests below don't repeat the same raw query.
    async fn review_row(store: &Sin90Store, id: &str) -> (String, String) {
        let row = sqlx::query("SELECT body, status FROM sin90_reviews WHERE id = ?")
            .bind(id)
            .fetch_one(store.pool())
            .await
            .unwrap();
        (row.get::<String, _>("body"), row.get::<String, _>("status"))
    }

    async fn weekly_review_count(store: &Sin90Store, period: &str) -> i64 {
        sqlx::query("SELECT COUNT(*) AS n FROM sin90_reviews WHERE kind = 'weekly' AND period = ?")
            .bind(period)
            .fetch_one(store.pool())
            .await
            .unwrap()
            .get("n")
    }

    // ----- two fires, same ISO week -> exactly one draft ----------------------

    /// The task's core acceptance: a review-kind Routine firing TWICE in the
    /// SAME ISO week (two distinct `fire_id`s, so this is not the dedup path)
    /// must still leave exactly one `(weekly, week)` Review.
    #[tokio::test]
    async fn review_routine_two_fires_same_week_create_only_one_draft() {
        let store = new_store().await;
        let routine = create_review_routine(&store).await;

        let first = store
            .record_routine_fire(
                "fire-1",
                &format!("routine.{}", routine.id),
                "2026-09-21T18:00:00Z", // 2026-W39, Monday
                FireTrigger::Tick,
            )
            .await
            .unwrap();
        let second = store
            .record_routine_fire(
                "fire-2",
                &format!("routine.{}", routine.id),
                "2026-09-24T18:00:00Z", // still 2026-W39, Thursday
                FireTrigger::Tick,
            )
            .await
            .unwrap();

        let RoutineFireOutcome::Recorded { auto_review, .. } = first else {
            panic!("expected Recorded, got {first:?}");
        };
        assert!(
            auto_review.is_some(),
            "first fire in a fresh week must auto-create the draft"
        );
        let RoutineFireOutcome::Recorded { auto_review, .. } = second else {
            panic!("expected Recorded, got {second:?}");
        };
        assert!(
            auto_review.is_none(),
            "second fire in the SAME week must not auto-create another draft"
        );

        assert_eq!(weekly_review_count(&store, "2026-W39").await, 1);
    }

    /// Positive control for the test above: two fires landing in DIFFERENT
    /// ISO weeks must leave two separate drafts, one per week — proves the
    /// dedup above is keyed on the week, not just "this routine already has
    /// a draft somewhere".
    #[tokio::test]
    async fn review_routine_fires_in_different_weeks_create_two_drafts_positive_control() {
        let store = new_store().await;
        let routine = create_review_routine(&store).await;

        let first = store
            .record_routine_fire(
                "fire-1",
                &format!("routine.{}", routine.id),
                "2026-09-21T18:00:00Z", // 2026-W39
                FireTrigger::Tick,
            )
            .await
            .unwrap();
        let second = store
            .record_routine_fire(
                "fire-2",
                &format!("routine.{}", routine.id),
                "2026-09-28T18:00:00Z", // 2026-W40 — the FOLLOWING week
                FireTrigger::Tick,
            )
            .await
            .unwrap();

        assert!(matches!(
            first,
            RoutineFireOutcome::Recorded {
                auto_review: Some(_),
                ..
            }
        ));
        assert!(matches!(
            second,
            RoutineFireOutcome::Recorded {
                auto_review: Some(_),
                ..
            }
        ));
        assert_eq!(weekly_review_count(&store, "2026-W39").await, 1);
        assert_eq!(weekly_review_count(&store, "2026-W40").await, 1);
    }

    // ----- same fire_id retried -> no duplicate draft --------------------------

    #[tokio::test]
    async fn review_routine_duplicate_fire_id_does_not_duplicate_draft() {
        let store = new_store().await;
        let routine = create_review_routine(&store).await;

        let first = store
            .record_routine_fire(
                "fire-retry",
                &format!("routine.{}", routine.id),
                "2026-09-21T18:00:00Z",
                FireTrigger::Tick,
            )
            .await
            .unwrap();
        assert!(matches!(
            first,
            RoutineFireOutcome::Recorded {
                auto_review: Some(_),
                ..
            }
        ));

        // The kernel's at-least-once retry: SAME fire_id.
        let retried = store
            .record_routine_fire(
                "fire-retry",
                &format!("routine.{}", routine.id),
                "2026-09-21T18:00:00Z",
                FireTrigger::Tick,
            )
            .await
            .unwrap();
        assert_eq!(
            retried,
            RoutineFireOutcome::Duplicate {
                routine_id: routine.id.clone()
            }
        );

        assert_eq!(weekly_review_count(&store, "2026-W39").await, 1);
    }

    // ----- an already-finalized review for the week -> no new draft -----------

    /// A Review for the week already exists and is FINALIZED (not just a
    /// draft) — the fire must not create a second `(weekly, week)` row, and
    /// must not touch the finalized one either.
    #[tokio::test]
    async fn review_routine_existing_finalized_review_blocks_auto_draft() {
        let store = new_store().await;
        let routine = create_review_routine(&store).await;

        let human = store
            .create_review(&NewReview {
                kind: ReviewKind::Weekly,
                period: "2026-W39".to_string(),
            })
            .await
            .unwrap();
        store.finalize_review(&human.id).await.unwrap();

        let outcome = store
            .record_routine_fire(
                "fire-1",
                &format!("routine.{}", routine.id),
                "2026-09-21T18:00:00Z",
                FireTrigger::Tick,
            )
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            RoutineFireOutcome::Recorded {
                auto_review: None,
                ..
            }
        ));

        assert_eq!(weekly_review_count(&store, "2026-W39").await, 1);
        let (_, status) = review_row(&store, &human.id).await;
        assert_eq!(status, "finalized", "the existing review must be untouched");
    }

    // ----- non-review Routine kind -> never auto-creates a review -------------

    /// Negative control: an `exercise`-kind Routine firing must NEVER
    /// auto-create a Review, no matter how many times it fires.
    #[tokio::test]
    async fn review_routine_non_review_kind_never_creates_a_review() {
        let store = new_store().await;
        let routine = store
            .create_routine(&nr(
                "Morning run",
                RoutineKind::Exercise,
                "0 7 * * MON,WED,FRI",
            ))
            .await
            .unwrap();

        let outcome = store
            .record_routine_fire(
                "fire-1",
                &format!("routine.{}", routine.id),
                "2026-09-21T18:00:00Z",
                FireTrigger::Tick,
            )
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            RoutineFireOutcome::Recorded {
                auto_review: None,
                ..
            }
        ));
        assert_eq!(weekly_review_count(&store, "2026-W39").await, 0);
    }

    // ----- draft body matches weekly_draft's own structure ---------------------

    /// The auto-created draft's `body` must contain the SAME numbers
    /// `weekly_draft` itself computes for that week — built from a small
    /// fixture (one completed block, one done task) BEFORE the routine
    /// fires, same technique `weekly_draft_tests` uses.
    #[tokio::test]
    async fn review_routine_draft_body_matches_weekly_draft_structure() {
        use crate::core::{Energy, ScheduleBlockStatus, TaskKind, TaskStatus};
        use crate::store::test_hooks;

        let store = new_store().await;
        let area_id = store.create_area("Work").await.unwrap().id;
        let direction = store
            .create_direction("Coding", "this-quarter", Some(&area_id))
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
        test_hooks::set_last_event_at(&store, "block", &block.id, in_week)
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
        test_hooks::set_last_event_at(&store, "task", &task.id, in_week)
            .await
            .unwrap();

        // Independently computed expectation — the SAME call the draft
        // itself would run, at the SAME point in time (before the routine
        // fires below adds its own `routine.fired` event on top).
        let expected = store.weekly_draft("2026-W39").await.unwrap();
        let expected_md = crate::store::render_weekly_draft_markdown(&expected);
        assert!(expected_md.contains(&format!(
            "{direction_id}: 90 min",
            direction_id = direction.id
        )));
        assert!(expected_md.contains("Tasks done: 1"));

        let routine = create_review_routine(&store).await;
        let outcome = store
            .record_routine_fire(
                "fire-1",
                &format!("routine.{}", routine.id),
                "2026-09-24T18:00:00Z", // 2026-W39, after the fixture events above
                FireTrigger::Tick,
            )
            .await
            .unwrap();
        let RoutineFireOutcome::Recorded { auto_review, .. } = outcome else {
            panic!("expected Recorded, got {outcome:?}");
        };
        let auto_review = auto_review.expect("must auto-create a draft");

        let (body, status) = review_row(&store, &auto_review.review_id).await;
        assert_eq!(status, "draft");
        assert!(
            body.contains(&format!(
                "{direction_id}: 90 min",
                direction_id = direction.id
            )),
            "draft body must contain the area/direction hour numbers weekly_draft computed: {body}"
        );
        assert!(
            body.contains("Tasks done: 1"),
            "draft body must contain the exact tasks_done count: {body}"
        );
    }

    // ----- event payload: source field ------------------------------------

    #[tokio::test]
    async fn review_routine_created_event_payload_has_source_routine() {
        let store = new_store().await;
        let routine = create_review_routine(&store).await;

        let outcome = store
            .record_routine_fire(
                "fire-1",
                &format!("routine.{}", routine.id),
                "2026-09-21T18:00:00Z",
                FireTrigger::Tick,
            )
            .await
            .unwrap();
        let RoutineFireOutcome::Recorded { auto_review, .. } = outcome else {
            panic!("expected Recorded, got {outcome:?}");
        };
        let auto_review = auto_review.expect("must auto-create a draft");

        let row = sqlx::query(
            "SELECT payload FROM sin90_events
             WHERE entity = 'review' AND entity_id = ? AND kind = 'created'",
        )
        .bind(&auto_review.review_id)
        .fetch_one(store.pool())
        .await
        .unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(&row.get::<String, _>("payload")).unwrap();
        assert_eq!(payload["source"], "routine");
        assert_eq!(payload["kind"], "weekly");
        assert_eq!(payload["period"], "2026-W39");
    }

    /// Positive control for the test above: a HUMAN-created review's
    /// `review.created` event payload carries `source: "human"`, not
    /// `"routine"` — proves the field actually distinguishes the two paths,
    /// not just a hardcoded constant on one side.
    #[tokio::test]
    async fn review_routine_human_created_event_payload_has_source_human_positive_control() {
        let store = new_store().await;
        let review = store
            .create_review(&NewReview {
                kind: ReviewKind::Weekly,
                period: "2026-W01".to_string(),
            })
            .await
            .unwrap();

        let row = sqlx::query(
            "SELECT payload FROM sin90_events
             WHERE entity = 'review' AND entity_id = ? AND kind = 'created'",
        )
        .bind(&review.id)
        .fetch_one(store.pool())
        .await
        .unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(&row.get::<String, _>("payload")).unwrap();
        assert_eq!(payload["source"], "human");
    }
}
