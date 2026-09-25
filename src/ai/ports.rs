//! §11.3 / §11.5 — the `ai` module's ONLY dependencies: a read model, a sink
//! that can do exactly three writes (pending proposal + call record, a
//! failed/non-producing call record alone, and a batch dry-run "is this
//! still valid" check), and a model port. Static vocabulary only — the
//! ladder algorithm itself lives in [`super::ladder`].
//!
//! Ported from the frozen design's scratch crate `t501-check/src/ports.rs`
//! (§11.12), narrowed to what `Sin90Op` here actually has today — see each
//! item's doc for where it had to diverge from the scratch signature.

use std::future::Future;

use serde_json::{Map, Value};

use crate::core::{Alloc, DirectionId, DirectionStatus, Review, Sin90Op, Task, Week, WeekId};

// ---------------------------------------------------------------- vocabulary

/// `Hash` (2026-09-24 review): `http::ai_runs::RunRegistry` keys its
/// per-capability single-flight slot by `Capability`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    Classify,
    Summarize,
    Propose,
}
impl Capability {
    /// Value of `sin90_ai_calls.task_kind`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Classify => "classify",
            Self::Summarize => "summarize",
            Self::Propose => "propose",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Engine {
    Reflex,
    Local,
    Executive,
}
impl Engine {
    /// Value of `sin90_ai_calls.engine` / `fallback_from`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reflex => "reflex",
            Self::Local => "local",
            Self::Executive => "executive",
        }
    }
}

/// `result.tier` of `_a24/model/complete` (Agent24 ME4-S2 §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServedTier {
    Local,
    Remote,
}
impl ServedTier {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
        }
    }
}

/// Compiled in from Sin90's own `domain-os.yml` (`model_access`, default
/// `local_only`). The `include_str!`-driven `MODEL_ACCESS` constant and the
/// `remote-allowed-manifest` cargo feature (§11.3.2, J10c) are a T5.5.1
/// handoff item — this branch has no adapter to wire it to yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModelAccess {
    #[default]
    LocalOnly,
    RemoteAllowed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AiSettings {
    /// `sin90_settings['ai.executive_enabled']`; absent row = false.
    pub executive_enabled: bool,
}

// ---------------------------------------------------------------- model port

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Complexity {
    Simple,
    Complex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
}

#[derive(Debug, Clone)]
pub struct ModelMessage {
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub messages: Vec<ModelMessage>,
    pub schema_name: &'static str,
    /// Sent as `response_format: {type: json_schema, json_schema: {name, schema, strict: true}}`.
    pub schema: Map<String, Value>,
    pub max_tokens: u32,
    pub complexity: Complexity,
}

#[derive(Debug, Clone)]
pub struct ModelReply {
    pub text: String,
    pub model_id: Option<String>,
    pub tier: ServedTier,
    /// `result.usage` (L7): recorded on the call row.
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
}

/// `unavailable.data.cause` (ME4-S2 §7, closed set of 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnavailableCause {
    NoProvider,
    RequestRejected,
    BackendConfig,
    ResponseTooLarge,
}

/// What the adapter's `ClientError` (plus the two variants T5.1.1's design
/// adds to it — `Unavailable`, `Cancelled`, a change this branch cannot make
/// because it has no kernel-callback client, see `src/ai/mod.rs`'s doc)
/// collapses to for the ai module. The ai module never sees `ClientError`
/// itself (it may not import `adapter_agent24` — enforced by the boundary
/// check, J7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelFailure {
    Unavailable {
        retryable: bool,
        cause: UnavailableCause,
    },
    /// Busy — this end's or the kernel's in-flight limit.
    Busy,
    /// RateLimited — the kernel's per-module token bucket is empty.
    RateLimited,
    /// NotReady — handshake not finished.
    NotReady,
    Timeout,
    Forbidden,
    /// InvalidParams / PayloadTooLarge — our own request was wrong.
    BadRequest,
    /// NotFound / QuotaExceeded / TokenInvalid / RequestNotInFlight / Other.
    Other,
    /// Draining / Revoked / NotSent — this generation is ending.
    GenerationEnding,
    /// ConnectionLost: outcome unknown, generation is ending.
    ConnectionLost,
    /// kernel `cancelled` (daemon shutting down).
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LadderAction {
    /// Record this attempt as failed, try the next engine down (incl. reflex).
    Degrade,
    /// Capacity is exhausted: stop ALL model steps for the rest of this run;
    /// this item and every remaining item end `deferred` (no R2 fallback).
    Defer,
    /// Record, stop the whole run, produce nothing more.
    Abort,
}

impl ModelFailure {
    /// §11.3.4's failure table, the whole reason this is a `match` over every
    /// variant with no wildcard arm: a new `ModelFailure` variant that forgets
    /// to extend this fails to COMPILE, not just fails a test (J3).
    #[must_use]
    pub fn action(&self) -> LadderAction {
        match self {
            Self::ConnectionLost | Self::Cancelled | Self::GenerationEnding => LadderAction::Abort,
            Self::Busy | Self::RateLimited | Self::NotReady => LadderAction::Defer,
            Self::Unavailable { .. }
            | Self::Timeout
            | Self::Forbidden
            | Self::BadRequest
            | Self::Other => LadderAction::Degrade,
        }
    }
    /// Value of `sin90_ai_calls.error_kind`.
    #[must_use]
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::Unavailable {
                cause: UnavailableCause::NoProvider,
                ..
            } => "unavailable.no_provider",
            Self::Unavailable {
                cause: UnavailableCause::RequestRejected,
                ..
            } => "unavailable.request_rejected",
            Self::Unavailable {
                cause: UnavailableCause::BackendConfig,
                ..
            } => "unavailable.backend_config",
            Self::Unavailable {
                cause: UnavailableCause::ResponseTooLarge,
                ..
            } => "unavailable.response_too_large",
            Self::Busy => "busy",
            Self::RateLimited => "rate_limited",
            Self::NotReady => "not_ready",
            Self::Timeout => "timeout",
            Self::Forbidden => "forbidden",
            Self::BadRequest => "bad_request",
            Self::Other => "other",
            Self::GenerationEnding => "generation_ending",
            Self::ConnectionLost => "connection_lost",
            Self::Cancelled => "cancelled",
        }
    }
    /// Run-local circuit breaker: after this failure the engine is skipped
    /// for the rest of the run (no point asking again).
    #[must_use]
    pub fn opens_circuit(&self) -> bool {
        matches!(
            self,
            Self::Unavailable {
                cause: UnavailableCause::NoProvider | UnavailableCause::BackendConfig,
                ..
            } | Self::Forbidden
        )
    }
}

pub trait ModelPort: Send + Sync {
    fn complete(
        &self,
        req: ModelRequest,
    ) -> impl Future<Output = Result<ModelReply, ModelFailure>> + Send;
}

/// A `ModelPort` this branch never actually calls (T5.1.2 handoff — no real
/// `_a24/model/complete` adapter is wired yet, see `ai::mod`'s doc). HTTP
/// trigger routes (`POST /ai/classify`, T5.2.1) pass `model: None` typed
/// against this so the engine ladder's generic `M` parameter still has a
/// concrete type to monomorphize against, without pretending a model is
/// present. `plan()` never schedules a `Step::Model(..)` when the caller
/// passes `model_port_present: false`, so `complete` is provably unreachable
/// through that path; it still needs a body to satisfy the trait.
#[derive(Debug, Clone, Copy)]
pub struct NoModelPort;
impl ModelPort for NoModelPort {
    async fn complete(&self, _req: ModelRequest) -> Result<ModelReply, ModelFailure> {
        unreachable!(
            "NoModelPort::complete: the caller must pass model_port_present=false to plan() \
             so no Step::Model is ever scheduled against a None model"
        )
    }
}

// ---------------------------------------------------------------- sink + read

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiCallRecord {
    pub id: String,
    pub run_id: String,
    pub task_kind: Capability,
    pub engine: Engine,
    pub fallback_from: Option<Engine>,
    pub served_tier: Option<ServedTier>,
    pub model_id: Option<String>,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub latency_ms: u64,
    pub ok: bool,
    pub error_kind: Option<&'static str>,
    /// Always `None` when handed to the sink: the store fills it inside
    /// `submit`'s transaction.
    pub proposal_id: Option<String>,
    pub at: String,
}

/// What the ai module hands to [`AiSink::submit`]: NO `source`, NO `status`
/// — the store derives `source` from the accompanying call record (M3) and
/// always writes `pending` (§11.3.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposalDraft {
    pub id: String,
    pub ops: Vec<Sin90Op>,
    pub rationale: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SinkError {
    /// Failed `allowed_ops` / `validate` / dry-run `apply_op` against the
    /// current state (§11.4 公共, H3).
    #[error("rejected by pre-check: {0}")]
    Invalid(String),
    #[error("store: {0}")]
    Store(String),
}

/// The ai module's ENTIRE write surface (§11.5's "类型与连接层" judgment,
/// part of J8/J7's structural guarantee). Implemented by `Sin90Store` in
/// `store/ai_port.rs`; the ai module never names `Sin90Store`.
pub trait AiSink: Send + Sync {
    /// ONE `BEGIN IMMEDIATE`: `allowed_ops(cap)` → `build_snapshot` →
    /// `validate` → SAVEPOINT; `apply_op` × n; `ROLLBACK TO SAVEPOINT` (dry
    /// run, H3) → `source = source_for(rec.engine, rec.served_tier)` →
    /// INSERT proposal 'pending' + `proposal.submitted` event → INSERT call
    /// row (`ok=1`, `proposal_id = draft.id`) → COMMIT. Never applies.
    fn submit(
        &self,
        cap: Capability,
        draft: ProposalDraft,
        rec: AiCallRecord,
    ) -> impl Future<Output = Result<(), SinkError>> + Send;
    /// Failed / non-producing attempts only.
    fn record_call(&self, rec: AiCallRecord) -> impl Future<Output = Result<(), SinkError>> + Send;
    /// Batch "is each pending proposal still valid?" (L3): ONE
    /// `BEGIN IMMEDIATE` per call; per draft a SAVEPOINT running steps 1–3 of
    /// `submit` then `ROLLBACK TO`; finally `ROLLBACK` the whole transaction.
    /// Called once per run, not once per item (one write-lock acquisition).
    fn precheck(
        &self,
        cap: Capability,
        drafts: &[ProposalDraft],
    ) -> impl Future<Output = Vec<bool>> + Send;
}

#[derive(Debug, Clone)]
pub struct DirectionCandidate {
    pub direction_id: DirectionId,
    pub title: String,
    pub status: DirectionStatus,
    pub area_title: Option<String>,
}

#[derive(Debug, thiserror::Error)]
#[error("read model: {0}")]
pub struct ReadError(pub String);

// ---------------------------------------------------------------- summarize

/// One (Area or Direction) bucket's realized minutes for `summarize`'s
/// target week (design §11.4.2, T5.3.1), with its title already resolved by
/// the store — `ai/` may not name anything under `crate::store` (§11.5), so
/// [`AiReadModel::weekly_draft`]'s own implementation does the id→title join
/// before handing this back. `label` is `None` ONLY for the genuine "no
/// direction"/"no area" bucket (empty id) — an id that resolves to no title
/// (a row this store has no route to blank out today, but not assumed
/// impossible) falls back to the RAW ID, never silently collapsing into the
/// same bucket as "no direction/area" (2026-09-26 review, Low: `ai::
/// summarize::facts`'s "未分类" phrase is reserved for the true empty-id
/// case only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummarizeBucket {
    pub label: Option<String>,
    pub minutes: i64,
}

/// One Routine's fired/completed counts for `summarize`'s target week, title
/// already resolved (same posture as [`SummarizeBucket`], same id-fallback
/// rule — a Routine always has a non-empty id, so `label` here is never
/// `None`). `completed` is always `0` today — see `store::weekly_draft`'s
/// module doc for why (no completed-block-to-Routine link exists in this
/// schema).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummarizeRoutineRow {
    pub label: String,
    pub fired: i64,
    pub completed: i64,
}

/// `summarize`'s own shape of T4.3.1's weekly draft (design §11.4.2, §11.1
/// 第 13 条) — NOT `store::weekly_draft::WeeklyDraft` itself: same numbers,
/// but `by_area`/`by_direction`/`routines` carry an already-resolved display
/// `label` instead of a raw id (`ai::summarize` has no read access of its
/// own to turn one into the other — its only I/O is [`AiReadModel`]).
///
/// `auto_draft_md` (2026-09-26 review, C1/H1): T4.3.2's own
/// `render_weekly_draft_markdown` output for this SAME draft, computed by
/// the store from the SAME read (`AiReadModel::weekly_draft`'s
/// implementation) that produced every other field here — NOT re-rendered
/// independently by `ai/`, which may not import `render_weekly_draft_
/// markdown` (a `store` item) at all. This is the SECOND program-rendered
/// text `is_program_only` may compare the current review body against
/// (§11.4.2's revised "可改写条件" ②) — a review whose body was seeded by
/// T4.3.2's own auto-create path (`record_routine_fire`) starts out equal
/// to exactly THIS string, not `ai::summarize::render_facts`'s own "本周数字"
/// block (①), so summarize must recognize both, not just its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummarizeDraft {
    pub week: String,
    pub by_area: Vec<SummarizeBucket>,
    pub by_direction: Vec<SummarizeBucket>,
    pub tasks_done: i64,
    pub routines: Vec<SummarizeRoutineRow>,
    pub auto_draft_md: String,
}

pub trait SettingsRead: Send + Sync {
    fn settings(&self) -> impl Future<Output = Result<AiSettings, ReadError>> + Send;
}

/// Backed by a SEPARATE read-only pool
/// (`SqliteConnectOptions::pragma("query_only", "ON")`, §11.5 v2.1 M1): a
/// write from this path is an error, not a convention. Implemented by
/// `AiReader` in `store/ai_port.rs`.
pub trait AiReadModel: SettingsRead {
    fn inbox(&self, limit: u32) -> impl Future<Output = Result<Vec<Task>, ReadError>> + Send;
    /// New (2026-09-24 review, L4): point lookup — is task `id` CURRENTLY in
    /// the inbox? `Ok(None)` covers both "no such task" and "exists but not
    /// in the inbox" (closed, or already classified) — a caller validating
    /// an explicitly-given id doesn't need to tell those apart, only
    /// "usable as a target or not". Lets a caller check specific ids without
    /// paging through the whole inbox with an arbitrarily large `limit`.
    fn inbox_task(&self, id: &str) -> impl Future<Output = Result<Option<Task>, ReadError>> + Send;
    fn direction_candidates(
        &self,
        limit: u32,
    ) -> impl Future<Output = Result<Vec<DirectionCandidate>, ReadError>> + Send;
    /// New (T5.4.1, 2026-09-24 review L1): a precise single-id lookup —
    /// `propose`'s "缺口 Direction" only ever needs to resolve specific ids
    /// already named by the rhythm's allocation, not a whole capped/ordered
    /// listing (`direction_candidates`'s 40-ish-candidate cap and
    /// `updated_at DESC` ordering exist for classify's different "browse
    /// many, pick one" need, and could silently drop a real allocation
    /// target past that cap). `Ok(None)` if the id does not exist at all —
    /// UNLIKE `direction_candidates`, this does NOT itself exclude terminal
    /// (achieved/abandoned) Directions; the caller decides whether a
    /// terminal Direction still counts (propose does not).
    fn direction(
        &self,
        id: &DirectionId,
    ) -> impl Future<Output = Result<Option<DirectionCandidate>, ReadError>> + Send;
    fn title_history(
        &self,
        normalized: &str,
    ) -> impl Future<Output = Result<Vec<DirectionId>, ReadError>> + Send;
    fn review(&self, id: &str) -> impl Future<Output = Result<Option<Review>, ReadError>> + Send;
    fn week_tasks(
        &self,
        week_id: &WeekId,
    ) -> impl Future<Output = Result<Vec<Task>, ReadError>> + Send;
    /// New (T5.4.1, §11.4.3's "输入"): does this Week exist, and if so its
    /// status/`iso_week` — `propose`'s target W is checked against this
    /// (`Ok(None)` → 404; `week_is_open(status)` false → 409).
    fn week(&self, id: &WeekId) -> impl Future<Output = Result<Option<Week>, ReadError>> + Send;
    /// New (T5.4.1, §11.4.3's "P", 2026-09-24 review H1): the SINGLE NEAREST
    /// Week (by `iso_week`, regardless of status) strictly before
    /// `iso_week` (§11.6/`canonical_iso_week`'s fixed-width `YYYY-Www`
    /// format makes lexicographic and chronological order coincide) — `Ok`
    /// with that week ONLY if it is currently OPEN (`week_is_open`).
    /// `Ok(None)` covers BOTH "no week exists before `iso_week` at all" AND
    /// "the nearest one exists but is not open" — this does NOT keep
    /// searching further back for an older still-open week once it finds a
    /// closed nearest one (§11.4.3: "P 不存在或已关就没有顺延建议" — a
    /// closed nearest week means there simply is no P, not "look past it").
    fn previous_open_week(
        &self,
        iso_week: &str,
    ) -> impl Future<Output = Result<Option<Week>, ReadError>> + Send;
    /// New (T5.4.1, §11.4.3): the CURRENT rhythm's allocation — the most
    /// recently created `sin90_rhythms` row that is NOT `retired`'s
    /// `allocations`; an empty `Vec` if no such row exists (§11.4.3: "Rhythm
    /// 当前配额（非 retired 的最新一条）").
    fn rhythm_alloc(&self) -> impl Future<Output = Result<Vec<Alloc>, ReadError>> + Send;
    /// New (T5.3.1, §11.4.2's "数字来源"): T4.3.1's weekly draft numbers for
    /// `iso_week` (`YYYY-Www`), reshaped into [`SummarizeDraft`] (titles
    /// already resolved, `auto_draft_md` filled in — see that type's own
    /// doc, and `store::weekly_draft::weekly_draft_on`'s doc for why the
    /// numbers are computed in one read). `Err(ReadError)` for a malformed
    /// week label, mirroring `Sin90Store::weekly_draft`'s own
    /// `StoreError::Invalid`.
    fn weekly_draft(
        &self,
        iso_week: &str,
    ) -> impl Future<Output = Result<SummarizeDraft, ReadError>> + Send;
    /// New (T5.3.1, §11.4.2's "数字来源"): titles of tasks that transitioned
    /// to `done` inside `iso_week`'s window, most recent first, capped at 50
    /// (⚖️) — reference material the narrative may cite via `{{tN}}`, never
    /// counted toward [`super::summarize::facts`].
    fn done_titles(
        &self,
        iso_week: &str,
    ) -> impl Future<Output = Result<Vec<String>, ReadError>> + Send;
}
