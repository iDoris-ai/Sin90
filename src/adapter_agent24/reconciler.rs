//! T3.3.2 — the reconciler: lands `sin90_outbox`'s desired state onto the
//! kernel scheduler idempotently, and drives a full startup pass that makes
//! the kernel's own `_a24/scheduler/list` truth match Sin90's Routines
//! (tasks.md T3.3.2; spec.md M3 "对账器").
//!
//! Two halves, one shared "apply and classify" core:
//!
//! - **Startup full reconcile** ([`reconcile_full`]): calls `list`, compares
//!   the kernel's per-key state against Sin90's own `active`/`paused`
//!   Routines FIELD BY FIELD (spec, enabled, `system_disabled_reason`), and
//!   enqueues an `outbox` correction for every mismatch — a genuinely
//!   missing key, spec/enabled drift, a `system_disabled_reason` needing
//!   recovery, or an orphan key with no matching local Routine. It never
//!   calls `upsert`/`delete` directly: every correction goes through
//!   [`crate::store::Sin90Store::outbox_enqueue_upsert_for_routine`] /
//!   [`crate::store::Sin90Store::outbox_enqueue_delete_for_orphan`], which
//!   RE-READ `sin90_routines` fresh under their own write lock before writing
//!   anything (H1 review — this is what makes a stale `list()` snapshot safe
//!   to act on: a Routine retired, or already re-queued by something else, in
//!   the window between the snapshot and this call is never clobbered). The
//!   corrections this enqueues are picked up (and any error classified) by
//!   the exact same path an ordinary Routine mutation's own outbox row goes
//!   through — one place, not two, ends up calling the kernel.
//!   `user_suspended` rows are the one case left untouched — instead
//!   recomputed WHOLE-COLUMN into `sin90_routines.kernel_suspended_at`
//!   (design §2 #29; M1 review superseded an earlier separate-table design)
//!   so `/today` can surface "已被你在内核侧暂停".
//! - **Pending pump** ([`drain_pending`]): drains every currently-due
//!   `pending` outbox row (both the ones a full reconcile just enqueued, and
//!   any left over from a previous crash) by calling
//!   `_a24/scheduler/upsert`/`delete` and classifying the result (see
//!   [`apply_one`]'s own doc for the full table — success/permanent/
//!   retryable/stop-the-batch). Every write-back is guarded by the outbox
//!   row's `version` (C1 review, [`crate::store::OutboxRow::version`]): a
//!   kernel call that was in flight while the Routine changed again no
//!   longer marks the NEWER desired state `done` just because the OLD one
//!   happened to succeed.
//!
//! [`reconcile_once`] runs both, in that order, for a one-shot caller (this
//! module's own tests). [`spawn_pump_loop`] is the production entry point
//! `main.rs` wires up in MOUNTED mode only — standalone has no
//! `SchedulerClient` to build one with, so this module never runs there at
//! all (spec.md M3 "无内核（standalone）时 outbox 保持 pending 不报错": there is
//! simply no reconciler task around to report an error).
//!
//! **T3.3.2 review round 2, H1**: [`spawn_pump_loop`]'s ongoing loop calls
//! [`pump_tick`] on every wake, and `pump_tick` ALWAYS drains
//! `sin90_outbox` first, unconditionally — draining is never gated on a
//! full reconcile succeeding (the old design awaited a retry-until-success
//! full reconcile before the loop even started, so a `list()` call that kept
//! failing for any reason — `forbidden`, a malformed Routine row, SQLite
//! `busy`, a revoked generation — meant ordinary Routine mutations sat
//! `pending` forever, stacked under up to a 300s backoff). [`PumpState`]
//! tracks whether a full reconcile has EVER succeeded (`full_ok`); while it
//! hasn't, `pump_tick` retries with [`backoff_after`] on its own schedule
//! (H2 review) WITHOUT blocking the drain that already ran earlier in the
//! same tick; once it has, `pump_tick` re-runs [`reconcile_full`] no more
//! often than [`FULL_RECONCILE_INTERVAL`] (currently one hour) as a
//! low-frequency safety net for drift the outbox path alone cannot see
//! (e.g. a human suspending/resuming a schedule directly on the kernel
//! side). Per L3 review, a Routine mutation ALSO wakes the pump immediately
//! via [`crate::store::Sin90Store::outbox_notify`] rather than waiting for
//! the next [`PUMP_TICK`].
//!
//! **T3.3.2 review round 2, M2**: the kernel revoking this generation
//! (`ClientError::Revoked`, architecture.md 不可破边界 #5) is fatal to the
//! WHOLE pump task, not just the row or tick that observed it — both
//! [`apply_one`] (mid-drain) and [`pump_tick`]'s own full-reconcile call
//! surface this as [`PumpControl::Stop`], which `spawn_pump_loop` obeys by
//! returning immediately, with no further retries or backoff.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;

use crate::adapter_agent24::clients::scheduler::{
    ModuleScheduleState, ModuleSpec, SchedulerClient,
};
use crate::adapter_agent24::clients::ClientError;
use crate::core::{iso8601_after_secs, now_iso8601, RoutineStatus};
use crate::store::{OutboxRow, Sin90Store, StoreError};

/// How often [`spawn_pump_loop`]'s ongoing background half drains whatever is
/// currently due, after the one startup [`reconcile_once`] pass — raced
/// against [`Sin90Store::outbox_notify`] (L3 review), so this is a ceiling on
/// latency, not the typical case.
const PUMP_TICK: Duration = Duration::from_secs(5);

/// H2 review: how often [`spawn_pump_loop`] re-runs [`reconcile_full`] once
/// the STARTUP one has succeeded — low frequency on purpose (a full reconcile
/// is an O(local Routines + kernel schedules) `list` call plus, in the worst
/// case, one atomic re-read-and-maybe-write per mismatch; the fast per-tick
/// pump already handles every ordinary Routine mutation via `sin90_outbox`
/// long before this would ever fire). This is a safety net for drift the
/// outbox path itself cannot see — e.g. a human suspending/resuming a
/// schedule directly on the kernel side — not the primary mechanism.
const FULL_RECONCILE_INTERVAL: Duration = Duration::from_secs(3600);

/// The kernel schedule key for a Routine's id — delegates to
/// [`crate::store::repo::routine_kernel_key`] (`pub(crate)`, `lib.rs`'s own
/// layering: `adapter_agent24` already depends on `store`) rather than
/// re-deriving the `routine.<id>` shape independently, so the lower-casing
/// that function's own doc explains (T3.5.1: the kernel's key charset is
/// `[a-z0-9._-]`, `ulid()` is uppercase) can never drift out of step between
/// the two call sites again — the bug T3.5.1's real-mount test found was
/// exactly that kind of drift.
fn kernel_key(routine_id: &str) -> String {
    crate::store::repo::routine_kernel_key(routine_id)
}

#[derive(Debug, thiserror::Error)]
pub enum ReconcileError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Kernel(#[from] ClientError),
}

/// The wire shape of a `scheduler.upsert` outbox row's `desired` column
/// (`store::repo::routine_outbox_upsert_desired`): `{key, spec: {cron, tz},
/// enabled}`. Note this is NOT [`ModuleSpec`]'s own tagged shape — `desired`
/// is Sin90's own internal outbox record, `ModuleSpec` is what actually goes
/// on the wire to the kernel; [`apply_one`] is where one becomes the other.
#[derive(Debug, Deserialize)]
struct UpsertDesired {
    key: String,
    spec: UpsertSpec,
    enabled: bool,
}

#[derive(Debug, Deserialize)]
struct UpsertSpec {
    cron: String,
    tz: String,
}

/// `scheduler.delete` outbox row's `desired` column
/// (`store::repo::routine_outbox_delete_desired`): just the key.
#[derive(Debug, Deserialize)]
struct DeleteDesired {
    key: String,
}

fn parse_desired<T: serde::de::DeserializeOwned>(row: &OutboxRow) -> Result<T, serde_json::Error> {
    serde_json::from_value(row.desired.clone())
}

/// spec.md M3: "退避（1s 起，×2，上限 5min）". `attempts_after_increment` is the
/// row's `attempts` counter value AFTER this failure is recorded, so the
/// FIRST retryable failure (`attempts_after_increment == 1`) waits 1s, the
/// second 2s, the third 4s, ... capped at 300s (5 minutes). Saturates rather
/// than overflowing for a row that has failed an extreme number of times.
fn backoff_after(attempts_after_increment: i64) -> Duration {
    let exponent = attempts_after_increment.saturating_sub(1).max(0) as u32;
    let secs = 1u64.checked_shl(exponent).unwrap_or(u64::MAX);
    Duration::from_secs(secs.min(300))
}

/// [`ClientError::is_permanent`]'s five variants, mapped to the
/// `failure_kind` string `sin90_outbox.failure_kind`'s own migration comment
/// (`0005_outbox_failed.sql`) and spec.md M3 use. Only ever called when
/// `is_permanent()` already returned `true` for `err` — the `_` arm is
/// unreachable with today's `ClientError`, but returns a generic label
/// instead of panicking: a future permanent variant added to `ClientError`
/// without this match being updated must not crash the reconciler over a
/// mere classification label.
fn permanent_failure_kind(err: &ClientError) -> &'static str {
    match err {
        ClientError::Forbidden(_) => "forbidden",
        ClientError::QuotaExceeded(_) => "quota_exceeded",
        ClientError::InvalidParams(_) => "invalid_params",
        ClientError::TokenInvalid(_) => "token_invalid",
        ClientError::PayloadTooLarge(_) => "payload_too_large",
        _ => "other",
    }
}

/// T3.3.2 review M2: after this many consecutive attempts, an UNCLASSIFIED
/// ("other bucket") failure is treated as exhausted rather than retried
/// forever — an unrecognized condition that has failed 20 times running is
/// no longer usefully distinguished from a permanent one, and an outbox row
/// stuck retrying an unknown failure indefinitely is a silent resource leak
/// (ever-growing `sin90_outbox` history, an endlessly re-armed backoff timer)
/// nobody is ever told about.
const OTHER_BUCKET_EXHAUSTION_THRESHOLD: i64 = 20;

/// PR-Daemon REQUEST_CHANGES on #62 (merge-order forward compatibility, see
/// the `other =>` arm of [`apply_one`]'s dispatch): how long a row whose
/// `kind` this reconciler build does not recognize at all waits before being
/// reconsidered. A fixed hour, not [`backoff_after`]'s exponential schedule
/// — that schedule assumes a transient kernel/connection condition that
/// clears in seconds to minutes; an unrecognized `kind` only clears once
/// this binary is upgraded to a build that knows it, which is on the order
/// of a deploy, not a retry. Long enough that a stacked-PR window (store
/// layer merged before the reconciler layer that dispatches its new kind)
/// does not spin the pump uselessly every few seconds; short enough that a
/// row is not stuck for an unreasonable time once the upgrade does land
/// (the very next `outbox_notify` after a Routine/Review mutation would
/// still land any OTHER pending row immediately — this constant only
/// bounds how long the SAME unrecognized row waits between its own checks).
const UNKNOWN_KIND_RETRY_SECS: u64 = 3600;

/// What [`apply_one`] tells its caller to do with the REST of the current
/// batch — M2 review: some kernel-reported conditions (rate limited, busy,
/// not ready, draining, or the connection/generation itself dying) are
/// signals about the KERNEL or CONNECTION as a whole, not about the one row
/// that happened to surface them; hammering the next row through the same
/// channel cannot help and may make things worse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowOutcome {
    /// This row is done (success, permanently failed, or retried with
    /// backoff) — keep processing the rest of the due rows in this pass.
    Continue,
    /// Stop this pass here — do not attempt any more rows this pass. The
    /// per-tick (or notify-woken) pump will try again on its own.
    StopBatch,
    /// T3.3.2 review round 2, M2: the kernel revoked this generation
    /// (architecture.md 不可破边界 #5: "回调连接断了 = 这一代结束"). Unlike
    /// [`Self::StopBatch`], this is not "try again next tick" — the WHOLE
    /// pump task must stop; see [`PumpControl::Stop`].
    Revoked,
}

/// Applies ONE due `pending` row to the kernel and writes back the outcome —
/// the shared core both [`drain_pending`] and (indirectly, via the rows it
/// enqueues) [`reconcile_full`] go through, so there is exactly one place in
/// this crate that calls a `SchedulerClient` method for an outbox row and
/// classifies what came back.
///
/// **T3.3.2 review round 2, M3**: before doing anything else, re-confirms
/// `row` is still `pending` at exactly `row.version`
/// ([`Sin90Store::outbox_row_is_current`]). [`Sin90Store::outbox_due_pending`]
/// hands back a whole BATCH read at one instant; by the time THIS row's turn
/// comes up (a concurrent Routine mutation, or an earlier row in the same
/// batch triggering a full-reconcile correction for the same dedup_key), the
/// row this function was handed can already be stale. Sending a stale
/// `desired` to the kernel is not merely redundant — it is actively wrong,
/// since the newer version sitting `pending` underneath it is what should
/// actually land. A `false` here is a silent [`RowOutcome::Continue`] with
/// no kernel call at all; the newer version is already queued for a later
/// pass to pick up on its own.
///
/// Classification table for a call that DID go out (M2 review):
/// - **Ok** -> `outbox_mark_done`, [`RowOutcome::Continue`].
/// - **`NotFound` on a `scheduler.delete`** -> treated as success (the
///   kernel already has no record of this key — exactly what a delete
///   wants) -> `outbox_mark_done`, [`RowOutcome::Continue`]. (`NotFound` on
///   an `upsert` does NOT get this treatment — it falls through to the
///   generic bucket below, since an upsert has no "already gone" success
///   reading.)
/// - **`Revoked`** (T3.3.2 review round 2, M2) -> the row is left COMPLETELY
///   untouched (no write at all) and [`RowOutcome::Revoked`] — the kernel
///   revoked this whole generation (architecture.md 不可破边界 #5), so the
///   WHOLE pump task stops, not just this batch; see [`PumpControl::Stop`].
/// - **`ConnectionLost`** -> the row is left COMPLETELY untouched and
///   [`RowOutcome::StopBatch`] — this specific call's outcome is genuinely
///   unknown (`ConnectionLost`'s own doc), so recording it as any kind of
///   failure would be a guess, but (unlike `Revoked`) the connection is not
///   definitively known to be permanently gone, so only this pass stops.
/// - **permanent** (`ClientError::is_permanent`: `forbidden`/
///   `quota_exceeded`/`invalid_params`/`token_invalid`/`payload_too_large`)
///   -> `outbox_mark_failed`, [`RowOutcome::Continue`] (a bad row does not
///   justify stalling the rest of the batch — H3 review).
/// - **`rate_limited` / `busy` / `not_ready` / `draining`** -> retry with
///   backoff (`outbox_mark_retry`, `bump_other_bucket = false` — T3.3.2
///   review round 2, L1: these never count toward
///   [`OTHER_BUCKET_EXHAUSTION_THRESHOLD`]) AND [`RowOutcome::StopBatch`] —
///   these are signals about the kernel's OWN current capacity, not this
///   row; sending more calls right behind this one is the opposite of
///   backing off.
/// - **everything else** (`timeout`, `not_sent`, `request_not_in_flight`,
///   `other`) -> retry with backoff (`bump_other_bucket = true`),
///   [`RowOutcome::Continue`] — UNLESS `row.other_bucket_attempts` has now
///   reached [`OTHER_BUCKET_EXHAUSTION_THRESHOLD`], in which case it is
///   marked `failed(kind = "exhausted")` with a `warn!` instead.
///
/// A row whose `kind` this DISPATCH does not recognize at all (never reaches
/// the kernel-call table above) is a SEPARATE case from a bad payload for a
/// kind it DOES recognize — see the `other =>` arm's own doc below for why
/// the two must not be conflated.
async fn apply_one(
    store: &Sin90Store,
    scheduler: &SchedulerClient,
    row: &OutboxRow,
) -> Result<RowOutcome, ReconcileError> {
    if !store.outbox_row_is_current(&row.id, row.version).await? {
        // M3 review: stale — a newer version already exists for this
        // dedup_key; sending THIS row's desired state now would regress it.
        return Ok(RowOutcome::Continue);
    }
    let is_delete = row.kind == "scheduler.delete";
    let result: Result<(), ClientError> = match row.kind.as_str() {
        "scheduler.upsert" => match parse_desired::<UpsertDesired>(row) {
            Ok(desired) => {
                let spec = ModuleSpec::Cron {
                    expr: desired.spec.cron,
                    tz: Some(desired.spec.tz),
                };
                scheduler
                    .upsert(&desired.key, &spec, desired.enabled, None, None)
                    .await
                    .map(|_| ())
            }
            Err(e) => {
                // H3 review: a bad row must not stall the whole pump — mark
                // it failed and move on, instead of propagating an error
                // that would abort the rest of THIS pass's rows (and, before
                // this fix, be picked back up and fail identically forever).
                store
                    .outbox_mark_failed(
                        &row.id,
                        row.version,
                        "bad_desired",
                        &format!("scheduler.upsert desired payload: {e}"),
                    )
                    .await?;
                return Ok(RowOutcome::Continue);
            }
        },
        "scheduler.delete" => match parse_desired::<DeleteDesired>(row) {
            Ok(desired) => scheduler.delete(&desired.key, None).await.map(|_| ()),
            Err(e) => {
                store
                    .outbox_mark_failed(
                        &row.id,
                        row.version,
                        "bad_desired",
                        &format!("scheduler.delete desired payload: {e}"),
                    )
                    .await?;
                return Ok(RowOutcome::Continue);
            }
        },
        other => {
            // PR-Daemon REQUEST_CHANGES on #62: an unrecognized `kind` is
            // NOT the same failure as `bad_desired` above (a kind this
            // build DOES know, whose payload failed to parse — that one
            // correctly stays a permanent `failed`, since no future
            // build of THIS SAME binary will ever parse that payload
            // differently). An unrecognized kind is a forward-compatibility
            // situation across a stacked-PR merge order: the store layer
            // that WRITES a new outbox `kind` (e.g. `memory.remember`) can
            // land and start running in production before the reconciler
            // layer that knows how to DISPATCH that kind has also merged
            // and deployed — `finalize_review` enqueueing such a row is
            // then a fact this OLDER reconciler build must not treat as
            // "broken forever." `failed` is a terminal state
            // `outbox_due_pending` never revisits (that method's own
            // `WHERE status = 'pending'` filter), so marking it `failed`
            // here would permanently discard a row a LATER reconciler
            // build — once deployed — could still land correctly; nothing
            // re-triggers an arbitrary future kind's row the way a Routine
            // mutation re-triggers its own `scheduler.*` rows via
            // `upsert_outbox`'s collapse. Instead: stay `pending`, back off
            // a long FIXED interval ([`UNKNOWN_KIND_RETRY_SECS`] — not
            // [`backoff_after`]'s exponential schedule, which assumes a
            // transient condition clearing in seconds, not "wait for a
            // deploy"), log once so it is visible, and `Continue` — H3's
            // own guarantee ("a bad row must not stall the batch") still
            // holds; this is simply a different KIND of "not fatal to the
            // batch" than `bad_desired`.
            tracing::warn!(
                kind = %other,
                dedup_key = %row.dedup_key,
                retry_in_secs = UNKNOWN_KIND_RETRY_SECS,
                "sin90: outbox row has a kind this reconciler build does not recognize; leaving \
                 it pending (NOT failed) in case an older store layer outran a newer reconciler \
                 across a stacked-PR merge order — will look at it again later"
            );
            store
                .outbox_mark_retry(
                    &row.id,
                    row.version,
                    &format!(
                        "unrecognized outbox kind {other:?} — forward-compat wait, not a failure"
                    ),
                    &iso8601_after_secs(UNKNOWN_KIND_RETRY_SECS),
                    // L1 posture (T3.3.2 review round 2): never counts toward
                    // OTHER_BUCKET_EXHAUSTION_THRESHOLD — this is not an
                    // unclassified KERNEL-side failure, and must never
                    // eventually flip to failed(exhausted) either; an
                    // upgrade that never lands would then leave the row
                    // waiting forever, which is the correct behavior here
                    // (still strictly better than discarding it).
                    false,
                )
                .await?;
            return Ok(RowOutcome::Continue);
        }
    };

    match result {
        Ok(()) => {
            store.outbox_mark_done(&row.id, row.version, None).await?;
            Ok(RowOutcome::Continue)
        }
        // `delete` + `NotFound` = the kernel already agrees this key is
        // gone — success, not a failure to classify further.
        Err(ClientError::NotFound(_)) if is_delete => {
            store.outbox_mark_done(&row.id, row.version, None).await?;
            Ok(RowOutcome::Continue)
        }
        Err(e @ ClientError::Revoked(_)) => {
            tracing::warn!(
                error = %e,
                dedup_key = %row.dedup_key,
                "sin90: kernel revoked this generation; leaving the row untouched and stopping \
                 the pump task entirely"
            );
            Ok(RowOutcome::Revoked)
        }
        Err(e @ ClientError::ConnectionLost) => {
            tracing::warn!(
                error = %e,
                dedup_key = %row.dedup_key,
                "sin90: outbox pump stopping this pass — the connection itself may be ending"
            );
            Ok(RowOutcome::StopBatch)
        }
        Err(e) if e.is_permanent() => {
            store
                .outbox_mark_failed(
                    &row.id,
                    row.version,
                    permanent_failure_kind(&e),
                    &e.to_string(),
                )
                .await?;
            Ok(RowOutcome::Continue)
        }
        Err(
            e @ (ClientError::RateLimited(_)
            | ClientError::Busy(_)
            | ClientError::NotReady(_)
            | ClientError::Draining(_)),
        ) => {
            let next_attempts = row.attempts + 1;
            let next_attempt_at = iso8601_after_secs(backoff_after(next_attempts).as_secs());
            // L1 review: `bump_other_bucket = false` — a rate-limited/busy/
            // not_ready/draining retry must never count toward
            // `OTHER_BUCKET_EXHAUSTION_THRESHOLD`.
            store
                .outbox_mark_retry(
                    &row.id,
                    row.version,
                    &e.to_string(),
                    &next_attempt_at,
                    false,
                )
                .await?;
            Ok(RowOutcome::StopBatch)
        }
        // Everything else (`timeout`, `not_sent`, `request_not_in_flight`,
        // `other`) — ordinary retry-with-backoff, UNLESS this row has failed
        // with an UNCLASSIFIED error this many times IN A ROW (L1 review:
        // `other_bucket_attempts`, NOT the shared `attempts` counter — a
        // string of rate-limited retries must never push this row toward
        // exhaustion) that "unknown, keep trying" no longer looks different
        // from "will never succeed."
        Err(e) => {
            let next_other_bucket_attempts = row.other_bucket_attempts + 1;
            if next_other_bucket_attempts >= OTHER_BUCKET_EXHAUSTION_THRESHOLD {
                tracing::warn!(
                    error = %e,
                    other_bucket_attempts = next_other_bucket_attempts,
                    dedup_key = %row.dedup_key,
                    "sin90: outbox row exceeded the unclassified-failure retry budget; marking \
                     failed(exhausted)"
                );
                store
                    .outbox_mark_failed(&row.id, row.version, "exhausted", &e.to_string())
                    .await?;
            } else {
                let next_attempts = row.attempts + 1;
                let next_attempt_at = iso8601_after_secs(backoff_after(next_attempts).as_secs());
                store
                    .outbox_mark_retry(&row.id, row.version, &e.to_string(), &next_attempt_at, true)
                    .await?;
            }
            Ok(RowOutcome::Continue)
        }
    }
}

/// Whether [`spawn_pump_loop`]'s task should keep running at all —
/// T3.3.2 review round 2, M2: distinct from [`RowOutcome`], which is
/// per-row/per-batch; this is "is the whole pump task still worth having."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpControl {
    /// Keep going — try again on the next wake.
    Continue,
    /// The kernel revoked this generation — the pump task must stop
    /// entirely, not just this pass. See [`RowOutcome::Revoked`].
    Stop,
}

/// Drains every `pending` `sin90_outbox` row due at or before `now` — split
/// out from [`drain_pending`] so tests can pass a synthetic `now` (e.g. far
/// in the future) instead of sleeping through a real backoff window.
async fn drain_pending_before(
    store: &Sin90Store,
    scheduler: &SchedulerClient,
    now: &str,
) -> Result<PumpControl, ReconcileError> {
    let due = store.outbox_due_pending(now).await?;
    for row in due {
        match apply_one(store, scheduler, &row).await? {
            RowOutcome::Continue => {}
            RowOutcome::StopBatch => break,
            RowOutcome::Revoked => return Ok(PumpControl::Stop),
        }
    }
    Ok(PumpControl::Continue)
}

/// Drains every currently-due `pending` `sin90_outbox` row once — both ones a
/// just-completed [`reconcile_full`] enqueued and any left over from a
/// previous crash (spec.md M3: "后台任务取 pending → 调内核 → 成功标 done；失败按
/// 指数退避"). Stops after one pass over whatever was due AT THE MOMENT it
/// read the list — [`spawn_pump_loop`] is what turns this into an ongoing
/// pump.
pub async fn drain_pending(
    store: &Sin90Store,
    scheduler: &SchedulerClient,
) -> Result<PumpControl, ReconcileError> {
    drain_pending_before(store, scheduler, &now_iso8601()).await
}

/// Sin90's own desired kernel state for one non-retired Routine, as seen at
/// the moment [`reconcile_full`] called `list()` — used ONLY to decide
/// whether a correction looks worth attempting. The actual write, if any, is
/// [`Sin90Store::outbox_enqueue_upsert_for_routine`]'s job, which re-reads
/// the Routine fresh under a write lock before committing to anything (H1
/// review) — so a decision made from a stale `LocalDesired` can at worst
/// trigger a no-op re-check, never a wrong write.
struct LocalDesired {
    routine_id: String,
    cron: String,
    tz: String,
    enabled: bool,
}

/// L1 review: collapse cron whitespace and upper-case it before comparing —
/// a kernel that echoes back extra internal whitespace, or a different case
/// for weekday tokens (`mon,wed,fri` vs `MON,WED,FRI`), must not register as
/// "drift" and trigger a needless re-upsert loop. Digits and punctuation are
/// unaffected by `to_uppercase`, so this is safe for every field position
/// (minute/hour/day-of-month/month/day-of-week).
fn normalize_cron(expr: &str) -> String {
    expr.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_uppercase()
}

/// Compares the kernel's reported spec against Sin90's own cron/tz, with
/// [`normalize_cron`] on BOTH sides (L1 review). `tz` is only trimmed, never
/// case-folded — IANA zone names are case-sensitive (`America/New_York`),
/// unlike cron's day tokens.
fn spec_matches(kernel_spec: &ModuleSpec, cron: &str, tz: &str) -> bool {
    match kernel_spec {
        ModuleSpec::Cron { expr, tz: ktz } => {
            normalize_cron(expr) == normalize_cron(cron)
                && ktz.as_deref().map(str::trim) == Some(tz.trim())
        }
        _ => false,
    }
}

/// Every `active`/`paused` Routine, keyed by its kernel key — `retired`
/// Routines are excluded (a closed door, `store::repo`'s H2: nothing should
/// ever re-upsert one), which is exactly what makes a kernel key with NO
/// entry here (whether the Routine was retired or never existed at all) an
/// orphan for [`reconcile_full`]'s second loop.
///
/// A [`BTreeMap`], not a [`std::collections::HashMap`]: [`reconcile_full`]
/// iterates this in key order when deciding which corrections to enqueue —
/// deterministic ordering matches the kernel's own `list` contract (design
/// §6.1: "≤ 256, sorted by key") and keeps the sequence of outbox rows this
/// module produces reproducible run to run, which is what makes this
/// module's own tests able to assert an exact request sequence rather than
/// an unordered set.
async fn local_desired_map(
    store: &Sin90Store,
) -> Result<BTreeMap<String, LocalDesired>, ReconcileError> {
    let routines = store.list_routines(None, None, None).await?;
    let mut map = BTreeMap::new();
    for r in routines {
        if r.status == RoutineStatus::Retired {
            continue;
        }
        map.insert(
            kernel_key(&r.id),
            LocalDesired {
                routine_id: r.id,
                cron: r.cron,
                tz: r.tz,
                enabled: r.status == RoutineStatus::Active,
            },
        );
    }
    Ok(map)
}

/// The startup (and periodic, [`FULL_RECONCILE_INTERVAL`]) full-reconcile
/// pass (spec.md M3; tasks.md T3.3.2): compares the kernel's ENTIRE
/// `_a24/scheduler/list` truth against Sin90's own `active`/`paused`
/// Routines and enqueues an outbox correction for every mismatch. See the
/// module doc, and [`Sin90Store::outbox_enqueue_upsert_for_routine`]/
/// [`Sin90Store::outbox_enqueue_delete_for_orphan`]'s own docs, for why every
/// correction re-reads `sin90_routines` fresh rather than trusting the
/// snapshot this function itself built.
pub async fn reconcile_full(
    store: &Sin90Store,
    scheduler: &SchedulerClient,
) -> Result<(), ReconcileError> {
    let local = local_desired_map(store).await?;
    let kernel_state = scheduler.list(None).await?;
    let kernel: BTreeMap<String, ModuleScheduleState> = kernel_state
        .schedules
        .into_iter()
        .map(|s| (s.key.clone(), s))
        .collect();

    // M1 review: rebuilt whole from scratch every pass (never incrementally
    // set/cleared per key) — see `sync_kernel_suspended_routines`'s own doc.
    let mut suspended_routine_ids: Vec<String> = Vec::new();

    for desired in local.values() {
        let key = kernel_key(&desired.routine_id);
        match kernel.get(&key) {
            // 本地有、内核缺的，补上。
            None => {
                store
                    .outbox_enqueue_upsert_for_routine(&desired.routine_id)
                    .await?
            }
            // user_suspended 的行不去碰——人为暂停是最高优先级的事实，spec/enabled/
            // system_disabled_reason 是否也 drifted 完全不看。
            Some(state) if state.user_suspended => {
                suspended_routine_ids.push(desired.routine_id.clone());
            }
            Some(state) => {
                // system_disabled_reason 非空的，upsert 以恢复 — even when spec
                // and enabled otherwise already match, a non-null reason
                // means the KERNEL disabled this row on its own (a
                // system-side condition, not the human), and re-asserting
                // the same desired state is exactly how spec.md says to
                // clear that.
                let needs_recovery = state.system_disabled_reason.is_some();
                let drifted = !spec_matches(&state.spec, &desired.cron, &desired.tz)
                    || state.enabled != desired.enabled;
                if needs_recovery || drifted {
                    store
                        .outbox_enqueue_upsert_for_routine(&desired.routine_id)
                        .await?;
                }
            }
        }
    }

    // 内核多出的（孤儿 key），删掉 — any kernel key with no matching active/paused
    // local Routine: either a key Sin90 never created, or a Routine that
    // reached `retired` (excluded from `local` above) whose own
    // `scheduler.delete` outbox row, for whatever reason, never landed.
    //
    // M3 review: only a key under Sin90's OWN `routine.<id>` convention is
    // ever a candidate for deletion — a key shaped any other way is not
    // something this module recognizes as its own, and gets logged, not
    // acted on (the kernel scopes `list` per calling module per design, so
    // this should never actually happen; this is a defensive floor, not the
    // expected path).
    for key in kernel.keys() {
        if local.contains_key(key) {
            continue;
        }
        match key.strip_prefix("routine.") {
            Some(routine_id_lower) => {
                // T3.5.1: `key` is the kernel's own (lower-cased,
                // `routine_kernel_key`'s doc) form — `sin90_routines.id` is
                // the original uppercase `ulid()`, so
                // `outbox_enqueue_delete_for_orphan`'s own `SELECT ...
                // WHERE id = ?` needs the recovered uppercase id, not the
                // lower-cased key fragment. `key` itself (passed through
                // unchanged below) is what actually gets sent back to the
                // kernel on `scheduler.delete` and must stay byte-exact.
                // `to_ascii_uppercase`, not `to_uppercase` (T3.5.1 review,
                // L) — same reasoning as `Sin90Store::record_routine_fire`'s
                // own identical transform.
                let routine_id = routine_id_lower.to_ascii_uppercase();
                store
                    .outbox_enqueue_delete_for_orphan(&routine_id, key)
                    .await?;
            }
            None => {
                tracing::warn!(
                    key = %key,
                    "sin90: kernel reported a schedule key outside Sin90's own `routine.<id>` \
                     convention; leaving it alone"
                );
            }
        }
    }

    store
        .sync_kernel_suspended_routines(&suspended_routine_ids)
        .await?;

    Ok(())
}

/// Runs [`reconcile_full`] once, then [`drain_pending`] once — what a caller
/// wanting one complete startup pass end to end reaches for.
pub async fn reconcile_once(
    store: &Sin90Store,
    scheduler: &SchedulerClient,
) -> Result<(), ReconcileError> {
    reconcile_full(store, scheduler).await?;
    drain_pending(store, scheduler).await?;
    Ok(())
}

/// T3.3.2 review round 2, H1: the pump's per-tick bookkeeping — carried by
/// VALUE across [`pump_tick`] calls (not `&mut`) so both [`spawn_pump_loop`]
/// (which owns it for the task's whole life) and a test (which can pass it
/// into a freshly spawned task and get it back) use the exact same shape.
///
/// - `full_ok`: has a full reconcile EVER succeeded since this pump started?
///   While `false`, [`pump_tick`] treats every due attempt as part of the
///   STARTUP retry sequence (`full_attempts`/`next_full_attempt`, backing
///   off like any other retry — H2 review). Once `true`, it switches to the
///   low-frequency [`FULL_RECONCILE_INTERVAL`] cadence off `last_full_success`
///   instead.
/// - `next_full_attempt`: when the NEXT full-reconcile attempt is due, while
///   `!full_ok` — starts at "now" (due immediately on the very first tick).
/// - `last_full_success`: when the last full reconcile succeeded, once
///   `full_ok` — the anchor [`FULL_RECONCILE_INTERVAL`] counts from.
#[derive(Debug, Clone, Copy)]
struct PumpState {
    full_ok: bool,
    full_attempts: i64,
    next_full_attempt: tokio::time::Instant,
    last_full_success: tokio::time::Instant,
}

impl PumpState {
    fn new() -> Self {
        let now = tokio::time::Instant::now();
        Self {
            full_ok: false,
            full_attempts: 0,
            next_full_attempt: now,
            last_full_success: now,
        }
    }
}

/// ONE iteration of [`spawn_pump_loop`]'s work, directly testable (L3
/// review: `full_reconcile_interval` is an explicit parameter, not the
/// hardcoded [`FULL_RECONCILE_INTERVAL`], so a test can use a millisecond-
/// scale interval instead of waiting a real hour).
///
/// **H1 review (the bug)**: the OLD `spawn_pump_loop` awaited a
/// retry-until-success full reconcile BEFORE its loop even started, so a
/// `list()` call that kept failing (`forbidden`, a malformed Routine row,
/// SQLite `busy`, ...) meant `sin90_outbox` NEVER drained — an ordinary
/// Routine mutation with no relationship to whatever was breaking `list()`
/// would sit `pending` forever, on top of a backoff that grows to 300s. The
/// fix: [`drain_pending`] runs FIRST, unconditionally, on every single call
/// to this function, before this function even asks whether a full
/// reconcile is due — draining is never gated on full reconcile succeeding,
/// ever again.
///
/// Returns the updated `state` alongside [`PumpControl`] — [`PumpControl::
/// Stop`] the moment EITHER draining or a full-reconcile attempt observes
/// the kernel revoking this generation (M2 review): the caller must stop
/// calling this function entirely, not just skip the rest of this tick.
async fn pump_tick(
    store: &Sin90Store,
    scheduler: &SchedulerClient,
    mut state: PumpState,
    full_reconcile_interval: Duration,
) -> (PumpControl, PumpState) {
    match drain_pending(store, scheduler).await {
        Ok(PumpControl::Stop) => return (PumpControl::Stop, state),
        Ok(PumpControl::Continue) => {}
        Err(e) => tracing::warn!(error = %e, "sin90: outbox pump pass failed"),
    }

    let now = tokio::time::Instant::now();
    let full_reconcile_due = if state.full_ok {
        now.duration_since(state.last_full_success) >= full_reconcile_interval
    } else {
        now >= state.next_full_attempt
    };
    if full_reconcile_due {
        match reconcile_full(store, scheduler).await {
            Ok(()) => {
                state.full_ok = true;
                state.full_attempts = 0;
                state.last_full_success = tokio::time::Instant::now();
            }
            // M2 review: revoked at startup must not retry with backoff
            // forever either — same fatal treatment as the ongoing case.
            Err(ReconcileError::Kernel(ClientError::Revoked(_))) => {
                tracing::warn!(
                    "sin90: full reconcile revoked; pump task stopping (full_ok={})",
                    state.full_ok
                );
                return (PumpControl::Stop, state);
            }
            Err(e) => {
                state.full_attempts += 1;
                let wait = backoff_after(state.full_attempts);
                tracing::warn!(
                    error = %e,
                    attempt = state.full_attempts,
                    wait_secs = wait.as_secs(),
                    full_ok = state.full_ok,
                    "sin90: full reconcile failed; retrying with backoff (the outbox pump keeps \
                     draining pending rows in the meantime — H1 review)"
                );
                state.next_full_attempt = tokio::time::Instant::now() + wait;
            }
        }
    }
    (PumpControl::Continue, state)
}

/// The production entry point (`main.rs`, MOUNTED mode only — see module
/// docs for why standalone never calls this): calls [`pump_tick`] forever,
/// on every wake (either [`PUMP_TICK`] elapsing, or
/// [`Sin90Store::outbox_notify`] firing — L3 review, so a Routine mutation
/// lands as soon as the pump wakes rather than waiting up to `PUMP_TICK`),
/// until a tick reports [`PumpControl::Stop`] (M2 review: the kernel revoked
/// this generation), at which point the task returns.
///
/// A single failed pass is always logged, never propagated — it must not
/// kill the whole loop (except the Revoked case above, which is meant to end
/// it). The returned `JoinHandle` runs for the process's whole life;
/// `main.rs` is not expected to ever await or abort it (same posture it
/// already takes toward `KernelEventSink`'s own worker tasks).
pub fn spawn_pump_loop(
    store: Sin90Store,
    scheduler: SchedulerClient,
) -> tokio::task::JoinHandle<()> {
    let notify = store.outbox_notify();
    tokio::spawn(async move {
        let mut state = PumpState::new();
        loop {
            let (control, new_state) =
                pump_tick(&store, &scheduler, state, FULL_RECONCILE_INTERVAL).await;
            state = new_state;
            if control == PumpControl::Stop {
                return;
            }
            tokio::select! {
                _ = tokio::time::sleep(PUMP_TICK) => {}
                _ = notify.notified() => {}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter_agent24::clients::test_support::{
        fake_kernel, read_request, respond, respond_error, FakePeer,
    };
    use crate::core::{NewRoutine, RoutineKind, RoutinePatch};
    use crate::store::test_hooks;
    use serde_json::{json, Value};
    use std::collections::{HashMap, VecDeque};

    /// `sin90_outbox.dedup_key` for a Routine — mirrors `store::repo::
    /// routine_dedup_key` (private to that module). Test-only: production
    /// code no longer needs to build this string itself (H1 review moved
    /// dedup_key construction inside `Sin90Store::outbox_enqueue_upsert_for_
    /// routine`/`outbox_enqueue_delete_for_orphan`), but assertions here
    /// still need it to query `test_hooks::outbox_rows_for`.
    fn outbox_dedup_key(routine_id: &str) -> String {
        format!("routine:{routine_id}")
    }

    async fn scheduler_and_peer() -> (SchedulerClient, FakePeer) {
        let (clients, peer) = fake_kernel(vec!["_a24/scheduler/".to_string()]).await;
        (SchedulerClient::new(&clients).unwrap(), peer)
    }

    fn new_routine(cron: &str) -> NewRoutine {
        NewRoutine {
            title: "Exercise".to_string(),
            area_id: None,
            direction_id: None,
            kind: RoutineKind::Exercise,
            cron: cron.to_string(),
            tz: None,
            target_count: None,
            target_minutes: None,
        }
    }

    // ----- M4 review: a stateful fake kernel scheduler ----------------------
    //
    // Drives REAL `_a24/scheduler/{upsert,delete,list}` semantics (dedup by
    // key, `absent` on deleting an unknown key, an upsert clearing
    // `system_disabled_reason` while preserving `user_suspended`) against an
    // in-memory map, so a test can assert against the fake KERNEL's OWN
    // resulting state — "内核侧仍只有 1 条" becomes a real assertion on
    // `FakeKernel::schedules`, not just "the same key was sent twice."

    #[derive(Debug, Clone, PartialEq)]
    struct FakeSchedule {
        cron: String,
        tz: String,
        enabled: bool,
        user_suspended: bool,
        system_disabled_reason: Option<String>,
    }

    impl FakeSchedule {
        fn new(cron: &str, tz: &str, enabled: bool) -> Self {
            Self {
                cron: cron.to_string(),
                tz: tz.to_string(),
                enabled,
                user_suspended: false,
                system_disabled_reason: None,
            }
        }

        fn to_json(&self, key: &str) -> Value {
            json!({
                "key": key,
                "spec": {"type": "cron", "expr": self.cron, "tz": self.tz},
                "enabled": self.enabled,
                "label": key,
                "user_suspended": self.user_suspended,
                "system_disabled_reason": self.system_disabled_reason,
                "next_run_at": null,
                "last_fire": {"tick": null, "run_now": null},
            })
        }
    }

    #[derive(Default)]
    struct FakeKernel {
        schedules: std::collections::BTreeMap<String, FakeSchedule>,
        /// Per-key queue of scripted `(code, kind)` RPC errors — the NEXT
        /// call touching that key fails with the front of its queue instead
        /// of being applied. The key `"*list*"` scripts `list()` itself.
        errors: HashMap<String, VecDeque<(i64, &'static str)>>,
    }

    impl FakeKernel {
        fn with(mut self, key: &str, schedule: FakeSchedule) -> Self {
            self.schedules.insert(key.to_string(), schedule);
            self
        }

        fn fail_next(mut self, key: &str, code: i64, kind: &'static str) -> Self {
            self.errors
                .entry(key.to_string())
                .or_default()
                .push_back((code, kind));
            self
        }

        /// Drives `peer` for exactly `n` requests, applying real
        /// upsert/delete/list semantics, then returns `self` (with whatever
        /// state resulted) so the test can assert on it and/or mutate it
        /// directly before the next `drive` call (simulating external drift
        /// between reconcile passes).
        async fn drive(mut self, peer: &mut FakePeer, n: usize) -> Self {
            for _ in 0..n {
                let req = read_request(peer).await;
                let method = req["method"].as_str().unwrap().to_string();
                match method.as_str() {
                    "_a24/scheduler/list" => {
                        if let Some((code, kind)) = self.next_error("*list*") {
                            respond_error(peer, &req, code, kind, "test-injected").await;
                            continue;
                        }
                        let schedules: Vec<Value> =
                            self.schedules.iter().map(|(k, s)| s.to_json(k)).collect();
                        respond(peer, &req, json!({ "schedules": schedules })).await;
                    }
                    "_a24/scheduler/upsert" => {
                        let key = req["params"]["key"].as_str().unwrap().to_string();
                        // T3.5.1 review (M2): mirror the REAL kernel's own
                        // key validator (`is_valid_kernel_key`'s own doc) —
                        // this is what makes a regression in
                        // `store::repo::routine_kernel_key`'s `.to_ascii_lowercase()`
                        // (the exact bug T3.5.1's real-mount test found)
                        // show up in THIS crate's own fast unit tests too,
                        // not only in the slow `--ignored` real-mount one.
                        if !is_valid_kernel_key(&key) {
                            respond_error(
                                peer,
                                &req,
                                -32602,
                                "invalid_params",
                                "key must match [a-z0-9._-]{1,128}",
                            )
                            .await;
                            continue;
                        }
                        if let Some((code, kind)) = self.next_error(&key) {
                            respond_error(peer, &req, code, kind, "test-injected").await;
                            continue;
                        }
                        let cron = req["params"]["spec"]["expr"].as_str().unwrap().to_string();
                        let tz = req["params"]["spec"]["tz"]
                            .as_str()
                            .unwrap_or("UTC")
                            .to_string();
                        let enabled = req["params"]["enabled"].as_bool().unwrap();
                        let outcome = if self.schedules.contains_key(&key) {
                            "updated"
                        } else {
                            "created"
                        };
                        let user_suspended = self
                            .schedules
                            .get(&key)
                            .map(|s| s.user_suspended)
                            .unwrap_or(false);
                        let mut entry = FakeSchedule::new(&cron, &tz, enabled);
                        entry.user_suspended = user_suspended; // an upsert never touches this.
                        let schedule_json = entry.to_json(&key);
                        self.schedules.insert(key, entry);
                        respond(
                            peer,
                            &req,
                            json!({"outcome": outcome, "schedule": schedule_json}),
                        )
                        .await;
                    }
                    "_a24/scheduler/delete" => {
                        let key = req["params"]["key"].as_str().unwrap().to_string();
                        // Same key-format validation as the upsert arm above.
                        if !is_valid_kernel_key(&key) {
                            respond_error(
                                peer,
                                &req,
                                -32602,
                                "invalid_params",
                                "key must match [a-z0-9._-]{1,128}",
                            )
                            .await;
                            continue;
                        }
                        if let Some((code, kind)) = self.next_error(&key) {
                            respond_error(peer, &req, code, kind, "test-injected").await;
                            continue;
                        }
                        let outcome = if self.schedules.remove(&key).is_some() {
                            "deleted"
                        } else {
                            "absent"
                        };
                        respond(peer, &req, json!({ "outcome": outcome })).await;
                    }
                    other => panic!("unexpected method in FakeKernel::drive: {other}"),
                }
            }
            self
        }

        fn next_error(&mut self, key: &str) -> Option<(i64, &'static str)> {
            self.errors.get_mut(key).and_then(|q| q.pop_front())
        }
    }

    /// Mirrors the REAL kernel's own key validator byte-for-byte (Agent24
    /// design §6.3 / `agent24d::scheduler_callback`'s own validator, quoted
    /// verbatim in `store::repo::routine_kernel_key`'s doc):
    /// `[a-z0-9._-]{1,128}`, no case folding. `FakeKernel` enforcing this too
    /// (T3.5.1 review, M2) is what makes a naive, uppercase-bearing kernel
    /// key something a FAST unit test catches, not only the slow
    /// `--ignored` real-mount one.
    fn is_valid_kernel_key(key: &str) -> bool {
        !key.is_empty()
            && key.len() <= 128
            && key.bytes().all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
            })
    }

    /// T3.5.1 review (M2), negative control: `FakeKernel` now rejects a key
    /// outside `[a-z0-9._-]{1,128}` exactly like the real kernel does — this
    /// is the exact shape of the bug T3.5.1's real-mount test found
    /// (`core::util::ulid()` is uppercase Crockford Base32; a naive
    /// `format!("routine.{id}")` sends the kernel an invalid key). Mutation
    /// check (T3.5.1 task report): removing `.to_ascii_lowercase()` from
    /// `store::repo::routine_kernel_key` turns EVERY existing reconciler
    /// test that drives a real, `ulid()`-derived Routine through
    /// `apply_one`/`FakeKernel` red via this exact same validation — this
    /// test just names the failure mode directly, on a minimal fixture.
    #[tokio::test]
    async fn fake_kernel_rejects_an_uppercase_key() {
        let (scheduler, mut peer) = scheduler_and_peer().await;
        let call = tokio::spawn(async move {
            scheduler
                .upsert(
                    "routine.UPPER01",
                    &ModuleSpec::Cron {
                        expr: "0 7 * * MON,WED,FRI".to_string(),
                        tz: Some("UTC".to_string()),
                    },
                    true,
                    None,
                    None,
                )
                .await
        });
        let fake = FakeKernel::default().drive(&mut peer, 1).await;
        let err = call
            .await
            .unwrap()
            .expect_err("an uppercase key must be rejected, not silently accepted");
        assert!(
            matches!(err, ClientError::InvalidParams(_)),
            "expected InvalidParams for key \"routine.UPPER01\", got {err:?}"
        );
        assert!(
            fake.schedules.is_empty(),
            "a rejected upsert must not create a row: {:?}",
            fake.schedules
        );
    }

    /// T3.5.1 review (L): self-healing, without a process restart, from a
    /// stale `sin90_outbox` row shaped exactly like the bug T3.5.1's
    /// real-mount test found — a `pending` `scheduler.upsert` row whose
    /// `desired.key` is the OLD, uppercase-bearing form a pre-fix version of
    /// `store::repo::routine_kernel_key` would have written. Tick 1 drains
    /// it (rejected — `FakeKernel` now validates the key charset, `failed`),
    /// then that SAME tick's startup full reconcile calls
    /// `outbox_enqueue_upsert_for_routine`, which finds no OTHER `pending`
    /// row blocking it (`store::repo`'s own doc) and re-asserts the CORRECT,
    /// lower-cased desired state — `upsert_outbox`'s own dedup-key lookup
    /// (`status IN ('pending', 'failed')`) means this REUSES the very same
    /// row (same `id`, `version` bumped), not a second one. Tick 2 drains
    /// that corrected row and it succeeds. No restart, no manual
    /// intervention — this is `spawn_pump_loop`'s own ordinary tick cadence
    /// (`pump_tick`'s doc: drain first, then full-reconcile-if-due), driven
    /// here twice directly.
    #[tokio::test]
    async fn reconciler_recovers_a_stale_uppercase_pending_row_within_two_ticks() {
        let store = Sin90Store::open_memory().await.unwrap();
        let routine = store
            .create_routine(&new_routine("0 7 * * MON,WED,FRI"))
            .await
            .unwrap();
        let dedup = outbox_dedup_key(&routine.id);
        let correct_key = kernel_key(&routine.id); // the FIXED, lower-cased form.
        let stale_key = format!("routine.{}", routine.id); // pre-fix, uppercase.

        // Retire the auto-created (already-correct) pending row as
        // irrelevant leftover bookkeeping, then inject the STALE row a
        // pre-fix version of this crate would have left behind, under the
        // SAME dedup_key `store::repo`'s own writers always use.
        let auto_row = test_hooks::outbox_rows_for(&store, &dedup).await.unwrap()[0].clone();
        store
            .outbox_mark_done(&auto_row.id, auto_row.version)
            .await
            .unwrap();
        test_hooks::insert_raw_outbox_row(
            &store,
            "stale-uppercase-row",
            "scheduler.upsert",
            &dedup,
            &json!({
                "key": stale_key,
                "spec": {"cron": routine.cron, "tz": routine.tz},
                "enabled": true,
            })
            .to_string(),
            &crate::core::now_iso8601(),
        )
        .await
        .unwrap();

        let (scheduler, mut peer) = scheduler_and_peer().await;
        let run = tokio::spawn(async move {
            let state = PumpState::new(); // `next_full_attempt` due immediately.
                                          // Tick 1: drain (the stale row — rejected), then the startup
                                          // full reconcile (enqueues the correct correction).
            let (_, state) = pump_tick(&store, &scheduler, state, FULL_RECONCILE_INTERVAL).await;
            // Tick 2: drain (the freshly-corrected row — accepted). Too soon
            // for `FULL_RECONCILE_INTERVAL` to make a second full reconcile
            // due, so this tick is drain-only.
            pump_tick(&store, &scheduler, state, FULL_RECONCILE_INTERVAL).await;
            store
        });

        // Tick 1: the stale row's upsert (rejected), then `list` (empty —
        // nothing has ever actually landed on the kernel).
        let fake = FakeKernel::default().drive(&mut peer, 2).await;
        // Tick 2: the corrected row's upsert (accepted).
        let fake = fake.drive(&mut peer, 1).await;
        let store = run.await.unwrap();

        assert!(
            fake.schedules.contains_key(&correct_key),
            "the corrected, lower-cased key must have landed: {:?}",
            fake.schedules
        );
        assert!(
            !fake.schedules.contains_key(&stale_key),
            "the stale uppercase key must never have landed: {:?}",
            fake.schedules
        );

        // `upsert_outbox`'s own `status IN ('pending', 'failed')` lookup
        // (this function's own doc) reuses "stale-uppercase-row" IN PLACE
        // for the correction — its already-`done` predecessor (the
        // auto-created row this test retired at setup) is untouched and
        // still present, so this looks specifically for the stale row by
        // id rather than asserting a total row count.
        let rows = test_hooks::outbox_rows_for(&store, &dedup).await.unwrap();
        let row = rows
            .iter()
            .find(|r| r.id == "stale-uppercase-row")
            .unwrap_or_else(|| panic!("stale-uppercase-row missing: {rows:?}"));
        assert_eq!(row.status, "done", "{row:?}");
        assert_eq!(row.desired["key"], json!(correct_key), "{row:?}");
        assert!(
            row.version > 1,
            "expected the STALE row to have been corrected in place (version bumped), not \
             replaced by a fresh one: {row:?}"
        );
    }

    // ----- M4: "本地有、内核缺 → 补上" (direct case) --------------------------

    /// A kernel key that was registered once (and landed) but has since
    /// disappeared from the kernel's own truth (e.g. deleted directly on the
    /// kernel side) — full reconcile must notice it is missing and put it
    /// back.
    #[tokio::test]
    async fn reconcile_missing_kernel_key_is_upserted_back() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (scheduler, mut peer) = scheduler_and_peer().await;
        let routine = store
            .create_routine(&new_routine("0 7 * * MON,WED,FRI"))
            .await
            .unwrap();
        let key = kernel_key(&routine.id);
        let dedup_key = outbox_dedup_key(&routine.id);

        let drain = tokio::spawn(async move {
            drain_pending_before(&store, &scheduler, "2099-01-01T00:00:00Z")
                .await
                .unwrap();
            (store, scheduler)
        });
        let fake = FakeKernel::default().drive(&mut peer, 1).await;
        let (store, scheduler) = drain.await.unwrap();
        assert!(fake.schedules.contains_key(&key));

        // Simulate the kernel losing this schedule entirely — full reconcile
        // must see it is missing and re-upsert it.
        let mut fake = fake;
        fake.schedules.remove(&key);

        let run = tokio::spawn(async move {
            reconcile_once(&store, &scheduler).await.unwrap();
            store
        });
        let fake = fake.drive(&mut peer, 2).await; // list + 1 upsert
        let store = run.await.unwrap();

        let schedule = fake.schedules.get(&key).expect("re-upserted");
        assert_eq!(schedule.cron, "0 7 * * MON,WED,FRI");
        assert!(schedule.enabled);

        let rows = test_hooks::outbox_rows_for(&store, &dedup_key)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2, "initial create + the recovery upsert");
        assert!(rows.iter().all(|r| r.status == "done"));
    }

    // ----- M4: "内核侧仍只有 1 条" via real fake-kernel state semantics -------

    #[tokio::test]
    async fn reconcile_pending_same_desired_processed_twice_kernel_state_stays_one_entry() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (scheduler, mut peer) = scheduler_and_peer().await;
        let routine = store
            .create_routine(&new_routine("0 7 * * MON,WED,FRI"))
            .await
            .unwrap();
        let key = kernel_key(&routine.id);
        let dedup_key = outbox_dedup_key(&routine.id);

        let drain = tokio::spawn(async move {
            drain_pending_before(&store, &scheduler, "2099-01-01T00:00:00Z")
                .await
                .unwrap();
            (store, scheduler)
        });
        let fake = FakeKernel::default().drive(&mut peer, 1).await;
        let (store, scheduler) = drain.await.unwrap();
        assert_eq!(fake.schedules.len(), 1);

        // A second, independent decision (standing in for a later
        // full-reconcile pass finding the exact same desired state still
        // correct — e.g. after a restart) re-enqueues the IDENTICAL
        // correction.
        store
            .outbox_enqueue_upsert_for_routine(&routine.id)
            .await
            .unwrap();

        let drain2 = tokio::spawn(async move {
            drain_pending_before(&store, &scheduler, "2099-01-01T00:00:00Z")
                .await
                .unwrap();
            store
        });
        let fake = fake.drive(&mut peer, 1).await;
        let store = drain2.await.unwrap();

        assert_eq!(
            fake.schedules.len(),
            1,
            "the real kernel-side dedup-by-key semantics: still exactly one entry"
        );
        assert_eq!(fake.schedules[&key].cron, "0 7 * * MON,WED,FRI");

        let rows = test_hooks::outbox_rows_for(&store, &dedup_key)
            .await
            .unwrap();
        assert_eq!(rows.iter().filter(|r| r.status == "pending").count(), 0);
    }

    // ----- J2 / M3: orphan deletion, and the M3 prefix guard -----------------

    #[tokio::test]
    async fn reconcile_orphan_kernel_key_is_deleted() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (scheduler, mut peer) = scheduler_and_peer().await;

        let run = tokio::spawn(async move {
            reconcile_once(&store, &scheduler).await.unwrap();
            store
        });
        let fake = FakeKernel::default()
            .with(
                "routine.orphan1",
                FakeSchedule::new("0 7 * * *", "UTC", true),
            )
            .drive(&mut peer, 2) // list + delete
            .await;
        let store = run.await.unwrap();

        assert!(!fake.schedules.contains_key("routine.orphan1"));
        // T3.5.1: the orphan loop recovers `routine_id` from the kernel key
        // via `.to_ascii_uppercase()` (this function's own doc — the kernel key
        // is always lower-cased, a real `sin90_routines.id` is always an
        // uppercase `ulid()`) BEFORE computing the internal dedup key, so a
        // lower-case fixture key like `"routine.orphan1"` here produces the
        // dedup key `"routine:ORPHAN1"`, not `"routine:orphan1"` — this
        // fixture's `routine_id` is synthetic (no real `sin90_routines` row
        // for it either way), but the case transform still applies exactly
        // as it would for a genuinely retired Routine's real ULID.
        let rows = test_hooks::outbox_rows_for(&store, "routine:ORPHAN1")
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "scheduler.delete");
        assert_eq!(rows[0].status, "done");
    }

    /// M3 review: a key NOT shaped `routine.<id>` is left alone entirely —
    /// only logged, never deleted.
    #[tokio::test]
    async fn reconcile_orphan_outside_routine_prefix_is_left_alone() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (scheduler, mut peer) = scheduler_and_peer().await;

        let run = tokio::spawn(async move {
            reconcile_once(&store, &scheduler).await.unwrap();
            store
        });
        let fake = FakeKernel::default()
            .with(
                "routine.orphan1",
                FakeSchedule::new("0 7 * * *", "UTC", true),
            )
            .with(
                "other-module.xyz",
                FakeSchedule::new("0 8 * * *", "UTC", true),
            )
            .drive(&mut peer, 2) // list + ONE delete (only the routine.* key)
            .await;
        let _store = run.await.unwrap();

        assert!(!fake.schedules.contains_key("routine.orphan1"));
        assert!(
            fake.schedules.contains_key("other-module.xyz"),
            "a key outside Sin90's own convention must never be deleted"
        );
    }

    /// T3.5.1 review (M3): `reconcile_orphan_kernel_key_is_deleted` above
    /// uses a synthetic, already-lower-case fake key ("routine.orphan1")
    /// that never corresponds to any real `sin90_routines` row either way —
    /// it does not exercise the specific case T3.5.1's own fix is about. A
    /// REAL, uppercase `ulid()`-shaped Routine that reached `retired` (whose
    /// own `scheduler.delete` outbox row, for whatever reason, never
    /// landed — module doc) must still be found and deleted, via this
    /// orphan loop's `.to_ascii_uppercase()` recovery correctly mapping the
    /// kernel's lower-cased key back onto the real, uppercase local Routine
    /// id.
    #[tokio::test]
    async fn reconcile_orphan_real_uppercase_ulid_retired_routine_is_deleted() {
        let store = Sin90Store::open_memory().await.unwrap();
        let routine = store
            .create_routine(&new_routine("0 7 * * MON,WED,FRI"))
            .await
            .unwrap();
        let key = kernel_key(&routine.id); // the REAL, lower-cased wire form.
        let dedup = outbox_dedup_key(&routine.id);

        store
            .transition_routine(&routine.id, RoutineStatus::Retired)
            .await
            .unwrap();
        // Simulate "the retire's own scheduler.delete outbox row never
        // landed" (module doc) by marking every pending row for this
        // dedup_key done WITHOUT ever draining it to a kernel — the fake
        // kernel seeded below still has the key registered, exactly as if
        // that delivery had genuinely been lost.
        for row in test_hooks::outbox_rows_for(&store, &dedup).await.unwrap() {
            if row.status == "pending" {
                store.outbox_mark_done(&row.id, row.version).await.unwrap();
            }
        }

        let (scheduler, mut peer) = scheduler_and_peer().await;
        let run = tokio::spawn(async move {
            reconcile_once(&store, &scheduler).await.unwrap();
            store
        });
        let fake = FakeKernel::default()
            .with(&key, FakeSchedule::new("0 7 * * MON,WED,FRI", "UTC", true))
            .drive(&mut peer, 2) // list + delete
            .await;
        let store = run.await.unwrap();

        assert!(
            !fake.schedules.contains_key(&key),
            "the real retired Routine's real (lower-cased) kernel key must be deleted: {:?}",
            fake.schedules
        );
        let rows = test_hooks::outbox_rows_for(&store, &dedup).await.unwrap();
        assert!(
            rows.iter()
                .any(|r| r.kind == "scheduler.delete" && r.status == "done"),
            "{rows:?}"
        );
    }

    // ----- J3/J4: drift + system_disabled recovery, via direct fake-state ---

    #[tokio::test]
    async fn reconcile_spec_drift_is_corrected() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (scheduler, mut peer) = scheduler_and_peer().await;
        let routine = store
            .create_routine(&new_routine("0 7 * * MON,WED,FRI"))
            .await
            .unwrap();
        let key = kernel_key(&routine.id);

        let drain = tokio::spawn(async move {
            drain_pending_before(&store, &scheduler, "2099-01-01T00:00:00Z")
                .await
                .unwrap();
            (store, scheduler)
        });
        let mut fake = FakeKernel::default().drive(&mut peer, 1).await;
        let (store, scheduler) = drain.await.unwrap();

        // External drift: the kernel's own copy of the cron diverges from
        // Sin90's Routine row.
        fake.schedules.get_mut(&key).unwrap().cron = "0 8 * * *".to_string();

        let run = tokio::spawn(async move {
            reconcile_once(&store, &scheduler).await.unwrap();
            store
        });
        let fake = fake.drive(&mut peer, 2).await; // list + corrective upsert
        let _store = run.await.unwrap();

        assert_eq!(fake.schedules[&key].cron, "0 7 * * MON,WED,FRI");
    }

    #[tokio::test]
    async fn reconcile_system_disabled_reason_is_recovered() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (scheduler, mut peer) = scheduler_and_peer().await;
        let routine = store
            .create_routine(&new_routine("0 7 * * MON,WED,FRI"))
            .await
            .unwrap();
        let key = kernel_key(&routine.id);

        let drain = tokio::spawn(async move {
            drain_pending_before(&store, &scheduler, "2099-01-01T00:00:00Z")
                .await
                .unwrap();
            (store, scheduler)
        });
        let mut fake = FakeKernel::default().drive(&mut peer, 1).await;
        let (store, scheduler) = drain.await.unwrap();

        fake.schedules.get_mut(&key).unwrap().system_disabled_reason =
            Some("quota exceeded upstream".to_string());

        let run = tokio::spawn(async move {
            reconcile_once(&store, &scheduler).await.unwrap();
            store
        });
        let fake = fake.drive(&mut peer, 2).await; // list + recovery upsert
        let _store = run.await.unwrap();

        assert!(fake.schedules[&key].system_disabled_reason.is_none());
    }

    // ----- L1: normalized spec comparison avoids false-positive drift -------

    #[tokio::test]
    async fn reconcile_l1_cron_whitespace_and_case_are_normalized_before_comparing() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (scheduler, mut peer) = scheduler_and_peer().await;
        let routine = store
            .create_routine(&new_routine("0 7 * * MON,WED,FRI"))
            .await
            .unwrap();
        let key = kernel_key(&routine.id);
        let dedup_key = outbox_dedup_key(&routine.id);

        let drain = tokio::spawn(async move {
            drain_pending_before(&store, &scheduler, "2099-01-01T00:00:00Z")
                .await
                .unwrap();
            (store, scheduler)
        });
        let mut fake = FakeKernel::default().drive(&mut peer, 1).await;
        let (store, scheduler) = drain.await.unwrap();

        // Textually different, but the SAME schedule once normalized (extra
        // internal whitespace + lower-cased weekday tokens).
        fake.schedules.get_mut(&key).unwrap().cron = "0  7  *  *  mon,wed,fri".to_string();

        let run = tokio::spawn(async move {
            reconcile_once(&store, &scheduler).await.unwrap();
            store
        });
        // Only the `list()` call — no upsert, because normalization must see
        // these as equal.
        let _fake = fake.drive(&mut peer, 1).await;
        let store = run.await.unwrap();

        let rows = test_hooks::outbox_rows_for(&store, &dedup_key)
            .await
            .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "no second correction should have been enqueued at all"
        );
    }

    /// Positive control for the test above: an ACTUAL cron difference (not
    /// just whitespace/case) still triggers a correction — normalization
    /// must not have gone so far it stops detecting real drift.
    #[tokio::test]
    async fn reconcile_l1_normalization_positive_control_real_drift_still_detected() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (scheduler, mut peer) = scheduler_and_peer().await;
        let routine = store
            .create_routine(&new_routine("0 7 * * MON,WED,FRI"))
            .await
            .unwrap();
        let key = kernel_key(&routine.id);

        let drain = tokio::spawn(async move {
            drain_pending_before(&store, &scheduler, "2099-01-01T00:00:00Z")
                .await
                .unwrap();
            (store, scheduler)
        });
        let mut fake = FakeKernel::default().drive(&mut peer, 1).await;
        let (store, scheduler) = drain.await.unwrap();

        fake.schedules.get_mut(&key).unwrap().cron = "0 9 * * MON,WED,FRI".to_string();

        let run = tokio::spawn(async move {
            reconcile_once(&store, &scheduler).await.unwrap();
            store
        });
        let fake = fake.drive(&mut peer, 2).await; // list + corrective upsert
        let _store = run.await.unwrap();

        assert_eq!(fake.schedules[&key].cron, "0 7 * * MON,WED,FRI");
    }

    // ----- J5: user_suspended left alone, positive control: plain drift restored, M1 /today -----

    #[tokio::test]
    async fn reconcile_user_suspended_is_left_alone_positive_control_drift_is_restored() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (scheduler, mut peer) = scheduler_and_peer().await;
        let suspended = store
            .create_routine(&new_routine("0 7 * * MON,WED,FRI"))
            .await
            .unwrap();
        let drifted = store
            .create_routine(&new_routine("0 8 * * TUE,THU"))
            .await
            .unwrap();
        let suspended_key = kernel_key(&suspended.id);
        let drifted_key = kernel_key(&drifted.id);

        let drain = tokio::spawn(async move {
            drain_pending_before(&store, &scheduler, "2099-01-01T00:00:00Z")
                .await
                .unwrap();
            (store, scheduler)
        });
        let mut fake = FakeKernel::default().drive(&mut peer, 2).await; // both initial creates
        let (store, scheduler) = drain.await.unwrap();

        // A human suspended `suspended` directly on the kernel side;
        // `drifted` merely drifted to `enabled: false` with NO suspension
        // involved (this task's own "positive control" scenario).
        {
            let s = fake.schedules.get_mut(&suspended_key).unwrap();
            s.enabled = false;
            s.user_suspended = true;
        }
        fake.schedules.get_mut(&drifted_key).unwrap().enabled = false;

        let run = tokio::spawn(async move {
            reconcile_once(&store, &scheduler).await.unwrap();
            (store, scheduler)
        });
        // list + exactly ONE upsert (drifted only — suspended must not get one).
        let fake = fake.drive(&mut peer, 2).await;
        let (store, scheduler) = run.await.unwrap();

        assert!(
            fake.schedules[&suspended_key].user_suspended,
            "left completely alone"
        );
        assert!(
            !fake.schedules[&suspended_key].enabled,
            "must not have been re-enabled"
        );
        assert!(fake.schedules[&drifted_key].enabled, "drift was restored");

        let suspended_dedup = outbox_dedup_key(&suspended.id);
        let suspended_rows = test_hooks::outbox_rows_for(&store, &suspended_dedup)
            .await
            .unwrap();
        assert_eq!(
            suspended_rows.len(),
            1,
            "only the initial create — no correction for the suspended Routine"
        );

        let today = store.today_view().await.unwrap();
        let suspended_ids: Vec<&str> = today
            .kernel_suspended_routines
            .iter()
            .map(|r| r.id.as_str())
            .collect();
        assert_eq!(suspended_ids, vec![suspended.id.as_str()]);

        // A second pass after the human resumes it on the kernel side clears
        // the notice (M1: whole-column rebuild, not a lingering table row).
        let mut fake = fake;
        fake.schedules
            .get_mut(&suspended_key)
            .unwrap()
            .user_suspended = false;
        fake.schedules.get_mut(&suspended_key).unwrap().enabled = true;
        let run2 = tokio::spawn(async move {
            reconcile_full(&store, &scheduler).await.unwrap();
            store
        });
        let _fake = fake.drive(&mut peer, 1).await; // just the list — nothing else needed.
        let store = run2.await.unwrap();
        let today = store.today_view().await.unwrap();
        assert!(today.kernel_suspended_routines.is_empty());
    }

    // ----- J6/J7: rate_limited backoff-then-success, quota_exceeded permanent failure -----

    #[tokio::test]
    async fn reconcile_rate_limited_then_backoff_then_succeeds() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (scheduler, mut peer) = scheduler_and_peer().await;
        let routine = store
            .create_routine(&new_routine("0 7 * * MON,WED,FRI"))
            .await
            .unwrap();
        let dedup_key = outbox_dedup_key(&routine.id);

        let drain1 = tokio::spawn(async move {
            drain_pending_before(&store, &scheduler, "2099-01-01T00:00:00Z")
                .await
                .unwrap();
            (store, scheduler)
        });
        let fake = FakeKernel::default()
            .fail_next(&kernel_key(&routine.id), -32000, "rate_limited")
            .drive(&mut peer, 1)
            .await;
        let (store, scheduler) = drain1.await.unwrap();

        let rows = test_hooks::outbox_rows_for(&store, &dedup_key)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].status, "pending",
            "retryable failure must stay pending"
        );
        assert!(
            rows[0].failure_kind.is_none(),
            "positive control: rate_limited must NOT set failure_kind/flip to failed"
        );
        assert_eq!(rows[0].attempts, 1);
        assert!(rows[0].next_attempt_at.is_some());

        let drain2 = tokio::spawn(async move {
            drain_pending_before(&store, &scheduler, "2099-01-01T00:00:00Z")
                .await
                .unwrap();
            store
        });
        let _fake = fake.drive(&mut peer, 1).await;
        let store = drain2.await.unwrap();

        let rows = test_hooks::outbox_rows_for(&store, &dedup_key)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "done");
    }

    #[tokio::test]
    async fn reconcile_quota_exceeded_marks_failed_and_never_retries() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (scheduler, mut peer) = scheduler_and_peer().await;
        let routine = store
            .create_routine(&new_routine("0 7 * * MON,WED,FRI"))
            .await
            .unwrap();
        let dedup_key = outbox_dedup_key(&routine.id);

        let drain = tokio::spawn(async move {
            drain_pending_before(&store, &scheduler, "2099-01-01T00:00:00Z")
                .await
                .unwrap();
            store
        });
        let _fake = FakeKernel::default()
            .fail_next(&kernel_key(&routine.id), -32000, "quota_exceeded")
            .drive(&mut peer, 1)
            .await;
        let store = drain.await.unwrap();

        let rows = test_hooks::outbox_rows_for(&store, &dedup_key)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "failed");
        assert_eq!(rows[0].failure_kind.as_deref(), Some("quota_exceeded"));
        assert!(rows[0].next_attempt_at.is_none());

        let due = store
            .outbox_due_pending("2099-01-01T00:00:00Z")
            .await
            .unwrap();
        assert!(due.is_empty(), "a failed row is never picked up on its own");
    }

    // ----- M2: full error-classification table --------------------------------

    /// `delete` + `NotFound` = success (the kernel already agrees the key is
    /// gone).
    #[tokio::test]
    async fn reconcile_m2_delete_not_found_is_treated_as_done() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (scheduler, mut peer) = scheduler_and_peer().await;
        let routine = store
            .create_routine(&new_routine("0 7 * * MON,WED,FRI"))
            .await
            .unwrap();
        let key = kernel_key(&routine.id);
        let dedup_key = outbox_dedup_key(&routine.id);
        store
            .transition_routine(&routine.id, RoutineStatus::Retired)
            .await
            .unwrap();

        let drain = tokio::spawn(async move {
            drain_pending_before(&store, &scheduler, "2099-01-01T00:00:00Z")
                .await
                .unwrap();
            store
        });
        let req = read_request(&mut peer).await;
        assert_eq!(req["method"], "_a24/scheduler/delete");
        assert_eq!(req["params"]["key"], key);
        respond_error(&mut peer, &req, -32000, "not_found", "no such schedule").await;
        let store = drain.await.unwrap();

        let rows = test_hooks::outbox_rows_for(&store, &dedup_key)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "done");
    }

    /// T3.3.2 review round 2, M2: `Revoked` leaves the row COMPLETELY
    /// untouched (`attempts`/`version` unchanged), never reaches the second
    /// row, and reports [`PumpControl::Stop`] — the signal
    /// [`spawn_pump_loop`] uses to end the whole task, not just this batch.
    #[tokio::test]
    async fn reconcile_m2_revoked_leaves_row_untouched_and_reports_pump_stop() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (scheduler, mut peer) = scheduler_and_peer().await;
        let first = store
            .create_routine(&new_routine("0 7 * * MON,WED,FRI"))
            .await
            .unwrap();
        let second = store
            .create_routine(&new_routine("0 8 * * TUE,THU"))
            .await
            .unwrap();
        let first_dedup = outbox_dedup_key(&first.id);
        let second_dedup = outbox_dedup_key(&second.id);
        let first_version = test_hooks::outbox_rows_for(&store, &first_dedup)
            .await
            .unwrap()[0]
            .version;

        let drain = tokio::spawn(async move {
            let control = drain_pending_before(&store, &scheduler, "2099-01-01T00:00:00Z")
                .await
                .unwrap();
            (control, store)
        });
        let req = read_request(&mut peer).await;
        assert_eq!(req["method"], "_a24/scheduler/upsert");
        assert_eq!(req["params"]["key"], kernel_key(&first.id));
        respond_error(&mut peer, &req, -32000, "revoked", "generation ended").await;
        let (control, store) = drain.await.unwrap();
        assert_eq!(control, PumpControl::Stop);

        let first_rows = test_hooks::outbox_rows_for(&store, &first_dedup)
            .await
            .unwrap();
        assert_eq!(first_rows.len(), 1);
        assert_eq!(first_rows[0].status, "pending");
        assert_eq!(
            first_rows[0].attempts, 0,
            "must not have incremented at all"
        );
        assert_eq!(
            first_rows[0].version, first_version,
            "must not have been touched"
        );

        let second_rows = test_hooks::outbox_rows_for(&store, &second_dedup)
            .await
            .unwrap();
        assert_eq!(
            second_rows[0].status, "pending",
            "the batch must have stopped BEFORE reaching the second row"
        );
        assert_eq!(second_rows[0].attempts, 0);
    }

    /// Positive control for the test above: `ConnectionLost` (unlike
    /// `Revoked`) only reports [`PumpControl::Continue`] — the connection is
    /// not definitively known to be permanently gone, so only THIS batch
    /// stops (`RowOutcome::StopBatch`), not the whole pump task.
    #[tokio::test]
    async fn reconcile_m2_connection_lost_reports_pump_continue_not_stop() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (scheduler, mut peer) = scheduler_and_peer().await;
        store
            .create_routine(&new_routine("0 7 * * MON,WED,FRI"))
            .await
            .unwrap();

        let drain = tokio::spawn(async move {
            drain_pending_before(&store, &scheduler, "2099-01-01T00:00:00Z")
                .await
                .unwrap()
        });
        let _req = read_request(&mut peer).await;
        drop(peer); // triggers ConnectionLost on the in-flight call.
        let control = drain.await.unwrap();
        assert_eq!(control, PumpControl::Continue);
    }

    /// T3.3.2 review round 2, M2: `Revoked` during the STARTUP full
    /// reconcile (before `full_ok` has ever been `true`) must not enter the
    /// ordinary retry-with-backoff path either — it reports
    /// [`PumpControl::Stop`] on the very first attempt, no backoff involved.
    #[tokio::test]
    async fn reconcile_m2_revoked_during_startup_full_reconcile_does_not_back_off() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (scheduler, mut peer) = scheduler_and_peer().await;

        let task = tokio::spawn(async move {
            let state = PumpState::new();
            pump_tick(&store, &scheduler, state, FULL_RECONCILE_INTERVAL).await
        });
        let req = read_request(&mut peer).await;
        assert_eq!(req["method"], "_a24/scheduler/list");
        respond_error(&mut peer, &req, -32000, "revoked", "generation ended").await;
        let (control, state) = task.await.unwrap();

        assert_eq!(control, PumpControl::Stop);
        assert_eq!(
            state.full_attempts, 0,
            "revoked must not be treated as an ordinary retry — no backoff bookkeeping at all"
        );
        assert!(!state.full_ok);
    }

    /// End-to-end: the REAL `spawn_pump_loop` task exits (its `JoinHandle`
    /// resolves) the moment the kernel reports `revoked`, without the test
    /// ever calling `.abort()`.
    #[tokio::test]
    async fn reconcile_m2_spawn_pump_loop_task_exits_when_revoked() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (scheduler, mut peer) = scheduler_and_peer().await;

        let handle = spawn_pump_loop(store, scheduler);

        let req = read_request(&mut peer).await;
        assert_eq!(req["method"], "_a24/scheduler/list");
        respond_error(&mut peer, &req, -32000, "revoked", "generation ended").await;

        // If the task did not actually return, this hangs — a real failure
        // mode, not a false pass; the mutation-verification step for this
        // test intentionally does that (see task report).
        handle.await.unwrap();
    }

    /// `rate_limited`/`busy`/`not_ready`/`draining` retry (with backoff) the
    /// row they hit, but ALSO stop the rest of the batch.
    #[tokio::test]
    async fn reconcile_m2_rate_limited_retries_this_row_but_stops_the_batch() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (scheduler, mut peer) = scheduler_and_peer().await;
        let first = store
            .create_routine(&new_routine("0 7 * * MON,WED,FRI"))
            .await
            .unwrap();
        let second = store
            .create_routine(&new_routine("0 8 * * TUE,THU"))
            .await
            .unwrap();
        let second_dedup = outbox_dedup_key(&second.id);

        let drain = tokio::spawn(async move {
            drain_pending_before(&store, &scheduler, "2099-01-01T00:00:00Z")
                .await
                .unwrap();
            store
        });
        let req = read_request(&mut peer).await;
        assert_eq!(req["params"]["key"], kernel_key(&first.id));
        respond_error(
            &mut peer,
            &req,
            -32000,
            "rate_limited",
            "token bucket empty",
        )
        .await;
        let store = drain.await.unwrap();

        let second_rows = test_hooks::outbox_rows_for(&store, &second_dedup)
            .await
            .unwrap();
        assert_eq!(
            second_rows[0].status, "pending",
            "the batch must have stopped before the kernel's rate limit could be hit twice"
        );
        assert_eq!(second_rows[0].attempts, 0);
    }

    /// An unclassified ("other" bucket) error retries normally, but flips to
    /// `failed(kind = "exhausted")` once it has failed
    /// `OTHER_BUCKET_EXHAUSTION_THRESHOLD` times in a row.
    #[tokio::test]
    async fn reconcile_m2_other_bucket_error_exhausts_after_20_attempts() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (scheduler, mut peer) = scheduler_and_peer().await;
        let routine = store
            .create_routine(&new_routine("0 7 * * MON,WED,FRI"))
            .await
            .unwrap();
        let key = kernel_key(&routine.id);
        let dedup_key = outbox_dedup_key(&routine.id);

        let mut store = store;
        let mut scheduler = scheduler;
        for attempt in 1..=OTHER_BUCKET_EXHAUSTION_THRESHOLD {
            let drain = tokio::spawn(async move {
                drain_pending_before(&store, &scheduler, "2099-01-01T00:00:00Z")
                    .await
                    .unwrap();
                (store, scheduler)
            });
            let req = read_request(&mut peer).await;
            assert_eq!(req["params"]["key"], key);
            // `cancelled` has no dedicated `ClientError` variant — it falls
            // into the generic `Other` bucket (module docs).
            respond_error(&mut peer, &req, -32000, "cancelled", "test-injected").await;
            let (s, c) = drain.await.unwrap();
            store = s;
            scheduler = c;

            let rows = test_hooks::outbox_rows_for(&store, &dedup_key)
                .await
                .unwrap();
            assert_eq!(rows.len(), 1);
            if attempt < OTHER_BUCKET_EXHAUSTION_THRESHOLD {
                assert_eq!(
                    rows[0].status, "pending",
                    "attempt {attempt} should still retry"
                );
            } else {
                assert_eq!(rows[0].status, "failed", "attempt {attempt} should exhaust");
                assert_eq!(rows[0].failure_kind.as_deref(), Some("exhausted"));
            }
        }
    }

    // ----- H3: a bad row does not stall the pump ------------------------------

    #[tokio::test]
    async fn reconcile_h3_bad_desired_row_is_marked_failed_and_does_not_block_the_batch() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (scheduler, mut peer) = scheduler_and_peer().await;
        let good = store
            .create_routine(&new_routine("0 7 * * MON,WED,FRI"))
            .await
            .unwrap();
        let good_dedup = outbox_dedup_key(&good.id);

        // Corrupt an outbox row's `desired` payload directly (the only way
        // such a row could exist in practice: a bug elsewhere, or a future
        // schema drift this module was not updated for) — placed BEFORE
        // `good`'s row so it is processed FIRST (`created_at ASC`).
        let bad_id = "bad-row-1";
        test_hooks::insert_raw_outbox_row(
            &store,
            bad_id,
            "scheduler.upsert",
            "routine:does-not-exist",
            "{\"not\": \"the expected shape\"}",
            "2000-01-01T00:00:00Z",
        )
        .await
        .unwrap();

        let run = tokio::spawn(async move {
            drain_pending_before(&store, &scheduler, "2099-01-01T00:00:00Z")
                .await
                .unwrap();
            store
        });
        // The bad row never reaches the wire at all (H3: caught before any
        // kernel call) — only `good`'s upsert does.
        let _fake = FakeKernel::default().drive(&mut peer, 1).await;
        let store = run.await.unwrap();

        let bad_rows = test_hooks::outbox_rows_for(&store, "routine:does-not-exist")
            .await
            .unwrap();
        assert_eq!(bad_rows.len(), 1);
        assert_eq!(bad_rows[0].status, "failed");
        assert_eq!(bad_rows[0].failure_kind.as_deref(), Some("bad_desired"));

        let good_rows = test_hooks::outbox_rows_for(&store, &good_dedup)
            .await
            .unwrap();
        assert_eq!(
            good_rows[0].status, "done",
            "the row AFTER the bad one must still have been processed"
        );
    }

    /// T4.4.1a review (PR-Daemon REQUEST_CHANGES on #62): a row whose `kind`
    /// this reconciler build does not recognize AT ALL — as opposed to a
    /// `bad_desired` row above, whose `kind` IS recognized but whose payload
    /// failed to parse — must stay `pending` with `next_attempt_at` pushed
    /// forward, NEVER `failed`. `failed` is a terminal state
    /// `outbox_due_pending` never revisits, which would permanently discard
    /// a row a LATER, upgraded reconciler build could still land correctly
    /// (the forward-compatibility gap a stacked-PR merge order can open: the
    /// store layer that WRITES a new `kind` can merge/deploy before the
    /// reconciler layer that knows how to DISPATCH it). The row AFTER it in
    /// the same batch must still be processed normally — H3's own "does not
    /// block the batch" guarantee, now proven for this second kind of "not
    /// fatal." Mutation target: reverting to `outbox_mark_failed(...,
    /// "bad_desired", ...)` turns this red (`status` reads `failed`, not
    /// `pending`, and `other_bucket_attempts` stays untouched instead of
    /// this test's own assertion on it becoming meaningless).
    #[tokio::test]
    async fn reconcile_h3_unknown_kind_row_stays_pending_with_backoff_and_does_not_block_the_batch()
    {
        let store = Sin90Store::open_memory().await.unwrap();
        let (scheduler, mut peer) = scheduler_and_peer().await;
        let good = store
            .create_routine(&new_routine("0 7 * * MON,WED,FRI"))
            .await
            .unwrap();
        let good_dedup = outbox_dedup_key(&good.id);

        test_hooks::insert_raw_outbox_row(
            &store,
            "bad-row-2",
            "scheduler.frobnicate",
            "routine:does-not-exist-2",
            "{}",
            "2000-01-01T00:00:00Z",
        )
        .await
        .unwrap();
        let before = test_hooks::outbox_rows_for(&store, "routine:does-not-exist-2")
            .await
            .unwrap();
        assert!(
            before[0].next_attempt_at.is_none(),
            "not pushed back yet — sanity check on the raw insert"
        );

        let run = tokio::spawn(async move {
            drain_pending_before(&store, &scheduler, "2099-01-01T00:00:00Z")
                .await
                .unwrap();
            store
        });
        let _fake = FakeKernel::default().drive(&mut peer, 1).await;
        let store = run.await.unwrap();

        let unknown_rows = test_hooks::outbox_rows_for(&store, "routine:does-not-exist-2")
            .await
            .unwrap();
        assert_eq!(
            unknown_rows[0].status, "pending",
            "unrecognized kind must stay pending, never failed"
        );
        assert!(
            unknown_rows[0].failure_kind.is_none(),
            "must not carry a failure_kind at all — it was never marked failed"
        );
        assert_eq!(
            unknown_rows[0].other_bucket_attempts, 0,
            "must never count toward OTHER_BUCKET_EXHAUSTION_THRESHOLD"
        );
        let next_attempt_at = unknown_rows[0]
            .next_attempt_at
            .as_deref()
            .expect("must have been pushed back");
        let thirty_minutes_from_now = crate::core::iso8601_after_secs(1800);
        assert!(
            next_attempt_at > thirty_minutes_from_now.as_str(),
            "pushed back by roughly UNKNOWN_KIND_RETRY_SECS (1h), not left alone: \
             {next_attempt_at} vs a 30-minute floor {thirty_minutes_from_now}"
        );

        // The row AFTER it in the same batch (`created_at ASC`) must still
        // have been processed normally.
        let good_rows = test_hooks::outbox_rows_for(&store, &good_dedup)
            .await
            .unwrap();
        assert_eq!(good_rows[0].status, "done");
    }

    // ----- M3: a row must be re-confirmed current before any kernel call -----

    /// T3.3.2 review round 2, M3: a row read by `outbox_due_pending` (a
    /// batch snapshot) but collapsed onto a newer version BEFORE its own
    /// turn to be processed must never reach the kernel at all — calling
    /// `apply_one` DIRECTLY with the now-stale row (bypassing
    /// `outbox_due_pending`'s own up-to-date read) proves the pre-check
    /// catches this independently of how the row became stale.
    #[tokio::test]
    async fn reconcile_m3_stale_row_is_skipped_before_any_kernel_call() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (scheduler, peer) = scheduler_and_peer().await;
        let routine = store
            .create_routine(&new_routine("0 7 * * MON,WED,FRI"))
            .await
            .unwrap();
        let dedup_key = outbox_dedup_key(&routine.id);

        let stale_row = store
            .outbox_due_pending("2099-01-01T00:00:00Z")
            .await
            .unwrap()
            .into_iter()
            .next()
            .unwrap();

        // The Routine changes AFTER this row was read but BEFORE it is
        // processed — exactly the window `outbox_due_pending`'s own doc
        // warns a caller about.
        store
            .update_routine(
                &routine.id,
                &RoutinePatch {
                    cron: Some("0 9 * * TUE,THU".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // Nobody answers `peer` in this test at all — if `apply_one` skips
        // the stale row locally (correct), it returns immediately, well
        // inside the timeout; if it instead tries to call the kernel
        // (mutation), it blocks forever waiting for a response that never
        // comes, and the timeout below is what turns that hang into a red
        // assertion instead of an actually-hanging test suite.
        let outcome = tokio::time::timeout(
            Duration::from_millis(200),
            apply_one(&store, &scheduler, &stale_row),
        )
        .await
        .expect("a stale row must be skipped locally, never block on a kernel round-trip")
        .unwrap();
        assert_eq!(outcome, RowOutcome::Continue);
        drop(peer); // nothing was ever sent to it — just releasing the fd.

        let rows = test_hooks::outbox_rows_for(&store, &dedup_key)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "pending");
        assert_eq!(
            rows[0].desired["spec"]["cron"], "0 9 * * TUE,THU",
            "the row must still hold the FRESH (v2) desired state, untouched by the stale call"
        );
    }

    // ----- C1: an in-flight kernel call must not mark a NEWER edit done ------

    #[tokio::test]
    async fn reconcile_c1_inflight_kernel_call_does_not_mark_a_newer_routine_edit_done() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (scheduler, mut peer) = scheduler_and_peer().await;
        let routine = store
            .create_routine(&new_routine("0 7 * * MON,WED,FRI"))
            .await
            .unwrap();
        let key = kernel_key(&routine.id);
        let dedup_key = outbox_dedup_key(&routine.id);

        let v1 = test_hooks::outbox_rows_for(&store, &dedup_key)
            .await
            .unwrap()[0]
            .version;

        let store_for_edit = store.clone();
        let drain = tokio::spawn(async move {
            drain_pending_before(&store, &scheduler, "2099-01-01T00:00:00Z")
                .await
                .unwrap();
            (store, scheduler)
        });

        // Read the v1 upsert request WITHOUT responding yet — the call is
        // now "in flight" from the reconciler's point of view.
        let req = read_request(&mut peer).await;
        assert_eq!(req["method"], "_a24/scheduler/upsert");
        assert_eq!(req["params"]["spec"]["expr"], "0 7 * * MON,WED,FRI");

        // The Routine changes WHILE that call is in flight: v1 -> v2.
        store_for_edit
            .update_routine(
                &routine.id,
                &RoutinePatch {
                    cron: Some("0 9 * * TUE,THU".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // NOW let the v1 call succeed.
        respond(
            &mut peer,
            &req,
            json!({
                "outcome": "created",
                "schedule": FakeSchedule::new("0 7 * * MON,WED,FRI", "UTC", true).to_json(&key),
            }),
        )
        .await;
        let (store, scheduler) = drain.await.unwrap();

        let rows = test_hooks::outbox_rows_for(&store, &dedup_key)
            .await
            .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "still the same physical row, collapsed in place"
        );
        assert_eq!(
            rows[0].status, "pending",
            "C1: v1's success must NOT mark the v2-holding row done"
        );
        assert!(rows[0].version > v1);
        assert_eq!(rows[0].desired["spec"]["cron"], "0 9 * * TUE,THU");

        // A subsequent drain sends v2, and it lands correctly.
        let drain2 = tokio::spawn(async move {
            drain_pending_before(&store, &scheduler, "2099-01-01T00:00:00Z")
                .await
                .unwrap();
            store
        });
        let fake = FakeKernel::default()
            .with(&key, FakeSchedule::new("0 7 * * MON,WED,FRI", "UTC", true))
            .drive(&mut peer, 1)
            .await;
        let store = drain2.await.unwrap();

        assert_eq!(fake.schedules[&key].cron, "0 9 * * TUE,THU");
        let rows = test_hooks::outbox_rows_for(&store, &dedup_key)
            .await
            .unwrap();
        assert_eq!(rows[0].status, "done");
    }

    // ----- M1: the version guard also covers outbox_mark_retry/_failed -------

    /// T3.3.2 review round 2, M1: the SAME in-flight race C1 fixed for
    /// `outbox_mark_done` also has to hold for `outbox_mark_retry` — a v1
    /// call that comes back `rate_limited` AFTER the Routine already moved
    /// to v2 must not touch the v2 row's `attempts`/`failure_kind` at all.
    #[tokio::test]
    async fn reconcile_m1_inflight_rate_limited_does_not_touch_a_newer_routine_edit() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (scheduler, mut peer) = scheduler_and_peer().await;
        let routine = store
            .create_routine(&new_routine("0 7 * * MON,WED,FRI"))
            .await
            .unwrap();
        let dedup_key = outbox_dedup_key(&routine.id);
        let v1 = test_hooks::outbox_rows_for(&store, &dedup_key)
            .await
            .unwrap()[0]
            .version;

        let store_for_edit = store.clone();
        let drain = tokio::spawn(async move {
            drain_pending_before(&store, &scheduler, "2099-01-01T00:00:00Z")
                .await
                .unwrap();
            store
        });

        let req = read_request(&mut peer).await;
        assert_eq!(req["method"], "_a24/scheduler/upsert");
        store_for_edit
            .update_routine(
                &routine.id,
                &RoutinePatch {
                    cron: Some("0 9 * * TUE,THU".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        respond_error(
            &mut peer,
            &req,
            -32000,
            "rate_limited",
            "token bucket empty",
        )
        .await;
        let store = drain.await.unwrap();

        let rows = test_hooks::outbox_rows_for(&store, &dedup_key)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "pending");
        assert_eq!(
            rows[0].attempts, 0,
            "v1's rate_limited failure must not touch the v2 row's attempts"
        );
        assert!(rows[0].failure_kind.is_none());
        assert_eq!(rows[0].desired["spec"]["cron"], "0 9 * * TUE,THU");
        assert!(rows[0].version > v1);
    }

    /// Same race, but the v1 call comes back `quota_exceeded` (a PERMANENT
    /// error, `outbox_mark_failed`'s path) — must not flip the v2 row to
    /// `failed` either.
    #[tokio::test]
    async fn reconcile_m1_inflight_quota_exceeded_does_not_touch_a_newer_routine_edit() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (scheduler, mut peer) = scheduler_and_peer().await;
        let routine = store
            .create_routine(&new_routine("0 7 * * MON,WED,FRI"))
            .await
            .unwrap();
        let dedup_key = outbox_dedup_key(&routine.id);
        let v1 = test_hooks::outbox_rows_for(&store, &dedup_key)
            .await
            .unwrap()[0]
            .version;

        let store_for_edit = store.clone();
        let drain = tokio::spawn(async move {
            drain_pending_before(&store, &scheduler, "2099-01-01T00:00:00Z")
                .await
                .unwrap();
            store
        });

        let req = read_request(&mut peer).await;
        assert_eq!(req["method"], "_a24/scheduler/upsert");
        store_for_edit
            .update_routine(
                &routine.id,
                &RoutinePatch {
                    cron: Some("0 9 * * TUE,THU".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        respond_error(&mut peer, &req, -32000, "quota_exceeded", "256 rows").await;
        let store = drain.await.unwrap();

        let rows = test_hooks::outbox_rows_for(&store, &dedup_key)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].status, "pending",
            "v1's quota_exceeded failure must not flip the v2 row to failed"
        );
        assert!(rows[0].failure_kind.is_none());
        assert_eq!(rows[0].attempts, 0);
        assert_eq!(rows[0].desired["spec"]["cron"], "0 9 * * TUE,THU");
        assert!(rows[0].version > v1);
    }

    // ----- H2: startup full reconcile retries until it succeeds --------------

    #[tokio::test]
    async fn reconcile_h2_startup_full_reconcile_retries_until_success() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (scheduler, mut peer) = scheduler_and_peer().await;

        let task = tokio::spawn(async move {
            let state = PumpState::new();
            let (control, state) =
                pump_tick(&store, &scheduler, state, FULL_RECONCILE_INTERVAL).await;
            (control, state, store, scheduler)
        });

        // No routines exist, so drain has nothing to do — the first (and
        // only, this tick) wire call is the startup full reconcile's
        // `list()`, which fails: kernel not ready yet at boot.
        let req = read_request(&mut peer).await;
        assert_eq!(req["method"], "_a24/scheduler/list");
        respond_error(&mut peer, &req, -32000, "not_ready", "kernel not ready").await;
        let (control, state, store, scheduler) = task.await.unwrap();
        assert_eq!(control, PumpControl::Continue);
        assert!(!state.full_ok, "one failure must not flip full_ok");

        // `spawn_pump_loop`'s own tick cadence would sleep here before
        // calling `pump_tick` again; this test instead just waits out the
        // (1s) backoff `pump_tick` itself computed, so the retry's due-check
        // passes on the very next call.
        tokio::time::sleep(Duration::from_millis(1100)).await;

        let task2 = tokio::spawn(async move {
            pump_tick(&store, &scheduler, state, FULL_RECONCILE_INTERVAL).await
        });
        let req = read_request(&mut peer).await;
        assert_eq!(req["method"], "_a24/scheduler/list");
        respond(&mut peer, &req, json!({ "schedules": [] })).await;
        let (control, state) = task2.await.unwrap();
        assert_eq!(control, PumpControl::Continue);
        assert!(
            state.full_ok,
            "the retry succeeded — full_ok must now be true"
        );
    }

    // ----- H1: draining must never be gated on full reconcile succeeding -----

    /// The exact bug H1 (round 2) reported: the OLD `spawn_pump_loop`
    /// awaited a retry-until-success full reconcile before its loop even
    /// started, so a `list()` call that kept failing meant `sin90_outbox`
    /// never drained. This drives `spawn_pump_loop` itself (the real
    /// production entry point, not a test-only helper) and proves the
    /// pending upsert lands EVEN THOUGH `list()` never succeeds even once.
    #[tokio::test]
    async fn reconcile_h1_pending_rows_still_land_while_list_keeps_failing() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (scheduler, mut peer) = scheduler_and_peer().await;
        let routine = store
            .create_routine(&new_routine("0 7 * * MON,WED,FRI"))
            .await
            .unwrap();
        let key = kernel_key(&routine.id);
        let dedup_key = outbox_dedup_key(&routine.id);
        let store_check = store.clone();

        let handle = spawn_pump_loop(store, scheduler);

        // Drain runs BEFORE the full-reconcile due-check in the SAME tick
        // (H1 fix) — the pending upsert must be the FIRST wire call, not
        // `list()`.
        let req = read_request(&mut peer).await;
        assert_eq!(req["method"], "_a24/scheduler/upsert");
        assert_eq!(req["params"]["key"], key);
        respond(
            &mut peer,
            &req,
            json!({
                "outcome": "created",
                "schedule": FakeSchedule::new("0 7 * * MON,WED,FRI", "UTC", true).to_json(&key),
            }),
        )
        .await;

        // The full-reconcile `list()` call in the SAME tick fails — proving
        // this failure never blocked the drain that already happened above.
        let req = read_request(&mut peer).await;
        assert_eq!(req["method"], "_a24/scheduler/list");
        respond_error(&mut peer, &req, -32000, "forbidden", "test-injected").await;

        handle.abort();
        let rows = test_hooks::outbox_rows_for(&store_check, &dedup_key)
            .await
            .unwrap();
        assert_eq!(
            rows[0].status, "done",
            "the pending row landed despite list() never succeeding"
        );
    }

    // ----- L3: the low-frequency periodic full reconcile actually recurs -----

    /// T3.3.2 review round 2, L3: with an injectable (short) interval instead
    /// of the real hour-long [`FULL_RECONCILE_INTERVAL`], proves the periodic
    /// full reconcile genuinely fires again once the interval elapses — not
    /// just once at startup.
    #[tokio::test]
    async fn reconcile_l3_periodic_full_reconcile_fires_again_after_the_interval() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (scheduler, mut peer) = scheduler_and_peer().await;
        let interval = Duration::from_millis(30);

        let task = tokio::spawn(async move {
            let state = PumpState::new();
            let (_, state) = pump_tick(&store, &scheduler, state, interval).await;
            (state, store, scheduler)
        });
        let req = read_request(&mut peer).await;
        assert_eq!(req["method"], "_a24/scheduler/list");
        respond(&mut peer, &req, json!({ "schedules": [] })).await;
        let (state, store, scheduler) = task.await.unwrap();
        assert!(state.full_ok);

        tokio::time::sleep(interval + Duration::from_millis(70)).await;

        let task2 =
            tokio::spawn(async move { pump_tick(&store, &scheduler, state, interval).await });
        let req2 = read_request(&mut peer).await;
        assert_eq!(
            req2["method"], "_a24/scheduler/list",
            "the periodic full reconcile must fire again once the interval elapses"
        );
        respond(&mut peer, &req2, json!({ "schedules": [] })).await;
        let (control, state2) = task2.await.unwrap();
        assert_eq!(control, PumpControl::Continue);
        assert!(state2.full_ok);
    }

    // ----- M4: standalone never starts the pump -------------------------------

    /// `spawn_pump_loop` takes an OWNED `SchedulerClient`, not an
    /// `Option<SchedulerClient>` — it is structurally impossible to call it
    /// without one already in hand, and `SchedulerClient::new` (this
    /// crate's ONLY constructor for one — see its own doc) returns `None`
    /// for an `Offer` that never granted the scheduler prefix, exactly the
    /// case `run_standalone` is in (it never even calls `KernelClients::
    /// handshake`, let alone builds an `Offer` with that prefix). This test
    /// pins the `None` half of that; the second assertion pins that
    /// `run_standalone`'s own source never mentions this module at all, so
    /// the two together are the full "standalone never starts the pump"
    /// claim without needing to spin up a real process.
    #[tokio::test]
    async fn reconcile_m4_standalone_offer_yields_no_scheduler_client() {
        let (clients, _peer) = fake_kernel(vec!["_a24/events/".to_string()]).await;
        assert!(SchedulerClient::new(&clients).is_none());
    }

    #[test]
    fn reconcile_m4_run_standalone_source_never_mentions_the_reconciler() {
        let source = std::fs::read_to_string("src/main.rs")
            .expect("this test runs with the crate root as its working directory");
        let start = source
            .find("async fn run_standalone(")
            .expect("run_standalone not found in main.rs");
        let end = source[start..]
            .find("\nasync fn run_as_agent24_module")
            .map(|i| start + i)
            .expect("run_as_agent24_module not found after run_standalone");
        let body = &source[start..end];
        assert!(
            !body.contains("reconciler"),
            "run_standalone's body must never reference the reconciler module:\n{body}"
        );
    }

    // ----- pure unit tests -----------------------------------------------------

    /// Mutation control for [`backoff_after`]: the sequence must be exactly
    /// 1s, 2s, 4s, 8s, ... capped at 300s — not, say, linear or uncapped.
    #[test]
    fn backoff_after_is_1s_doubling_capped_at_5_minutes() {
        assert_eq!(backoff_after(1), Duration::from_secs(1));
        assert_eq!(backoff_after(2), Duration::from_secs(2));
        assert_eq!(backoff_after(3), Duration::from_secs(4));
        assert_eq!(backoff_after(4), Duration::from_secs(8));
        assert_eq!(backoff_after(20), Duration::from_secs(300));
    }

    #[test]
    fn normalize_cron_collapses_whitespace_and_uppercases() {
        assert_eq!(
            normalize_cron("0  7  *  *  mon,wed,fri"),
            normalize_cron("0 7 * * MON,WED,FRI")
        );
        assert_eq!(normalize_cron("0 7 * * MON,WED,FRI"), "0 7 * * MON,WED,FRI");
    }
}
