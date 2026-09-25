//! `propose` (T5.4.1, design §11.4.3): scheduling suggestions for a target
//! Week `W` — at most three INDEPENDENT proposals (`propose.carry`,
//! `propose.reorder`, `propose.create`), each a batch of one of the three
//! existing Ops `AiSink`'s `allowed_ops(Propose)` already permits
//! (`CarryOverTask`/`ReorderTasks`/`CreateTasks`, `store/ai_port.rs`).
//!
//! One combined model call (schema `{carry, order, new_tasks, reason}`)
//! covers all three at once — the ladder itself (`plan`/`run_item`) is
//! `super::ladder`; this file supplies the propose-specific pieces
//! `run_item` is generic over (the request/schema/parse, the two
//! deterministic reflexes, and the driver that splits ONE produced
//! decision into up to three submitted proposals) — same division of
//! labor `ai::classify` already established for its own capability.
//!
//! Depends on nothing but `crate::core` and `crate::ai::*` (§11.5) — the
//! SAME boundary every other file under `src/ai/` keeps; the one exception
//! is this file's own `#[cfg(test)]` module (the boundary checker's own
//! "v2.1 M4" carve-out), which builds fixtures against a real
//! `crate::store::Sin90Store` so these tests exercise the actual
//! `AiSink`/`AiReadModel` implementation, not a second hand-rolled fake.

use std::collections::HashSet;

use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::core::{
    direction_is_terminal, task_is_terminal, week_is_open, Alloc, DirectionId, NewTask, Sin90Op,
    Task, TaskId, TaskStatus, Week, WeekId,
};

use super::ladder::{plan, run_item, Outcome, RunState, Step};
use super::ports::{
    AiCallRecord, AiReadModel, AiSink, Capability, Complexity, Engine, ModelAccess, ModelMessage,
    ModelPort, ModelReply, ModelRequest, ProposalDraft, Role, SinkError,
};

// ---------------------------------------------------------------- input

/// §11.4.3's "输入": `week_id` must name an existing Week that is currently
/// OPEN (planning/active) — anything else is rejected BEFORE a run is
/// created (mirrors `classify::ClassifyInputError`'s "checked synchronously,
/// never turns into a 202" posture).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposeInputError {
    UnknownWeek(WeekId),
    WeekNotOpen(WeekId, crate::core::WeekStatus),
    ReadFailed(String),
}

/// Resolves and validates the target week W (§11.4.3's "输入"). Lives here
/// (not the HTTP layer), same rationale `classify::select_targets` gives:
/// unit-testable against a fake `AiReadModel` without a server, and so the
/// HTTP handler stays a thin wrapper.
pub async fn select_week<R: AiReadModel>(
    read: &R,
    week_id: &WeekId,
) -> Result<Week, ProposeInputError> {
    let week = read
        .week(week_id)
        .await
        .map_err(|e| ProposeInputError::ReadFailed(e.to_string()))?
        .ok_or_else(|| ProposeInputError::UnknownWeek(week_id.clone()))?;
    if !week_is_open(week.status) {
        return Err(ProposeInputError::WeekNotOpen(week_id.clone(), week.status));
    }
    Ok(week)
}

// ---------------------------------------------------------------- candidates

/// One candidate (a task or a Direction) wrapped with the opaque key the
/// model is shown instead of a real id — same "候选以不透明短键呈现" posture
/// `classify::KeyedCandidate` uses, generalized over the three different key
/// prefixes propose needs (`p*`/`w*`/`g*`, §11.4.3).
#[derive(Debug, Clone)]
pub struct KeyedTask {
    pub key: String,
    pub task_id: TaskId,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct KeyedDirection {
    pub key: String,
    pub direction_id: DirectionId,
    pub title: String,
}

fn keyed_tasks(prefix: &str, tasks: &[Task]) -> Vec<KeyedTask> {
    tasks
        .iter()
        .enumerate()
        .map(|(i, t)| KeyedTask {
            key: format!("{prefix}{}", i + 1),
            task_id: t.id.clone(),
            title: t.title.clone(),
        })
        .collect()
}

/// §11.4.3's "缺口 Direction": a rhythm-allocated (`pct > 0`) Direction with
/// NO task currently in W — regardless of that task's status (2026-09-24
/// review L2: design's own wording is "W 里没有任何任务" — a Direction that
/// already has a `done`/`dropped`/`carried_over` task in W is not a gap
/// either, so the caller passes the FULL, unfiltered task list here, not
/// just the non-terminal ones `w_candidates` is built from). `alloc` is read
/// ONCE by the caller (`run_propose`; 2026-09-24 review L1: this function
/// used to re-read `rhythm_alloc` a second time itself) and deduplicated by
/// `direction_id` here (defensive: `AdjustRhythm`'s own validate already
/// rejects a duplicate id within one NEW batch, `check_alloc`, but nothing
/// re-checks an already-stored row). Each surviving id is resolved via
/// `AiReadModel::direction`'s PRECISE point lookup (2026-09-24 review L1) —
/// not `direction_candidates`'s capped, `updated_at`-ordered listing, which
/// could silently drop a real allocation target past its own limit.
///
/// `pub` (2026-09-24 review round 3, M-2): `http::ai_propose`'s dedup reuses
/// this SAME function to re-check "is this Direction STILL a gap" for a
/// pending `propose.create` proposal — a second, independently-written copy
/// of "gap" is exactly the kind of drift M5b's own "single source of truth"
/// posture already rejected for `p_candidates`/`carry_reflex`.
pub async fn gap_directions<R: AiReadModel>(
    read: &R,
    alloc: &[Alloc],
    week_tasks_all: &[Task],
) -> Result<Vec<KeyedDirection>, super::ports::ReadError> {
    let used: HashSet<&str> = week_tasks_all
        .iter()
        .filter_map(|t| t.direction_id.as_deref())
        .collect();
    let mut seen_direction_ids: HashSet<&str> = HashSet::new();
    let mut out = Vec::new();
    for a in alloc {
        if a.pct == 0
            || used.contains(a.direction_id.as_str())
            || !seen_direction_ids.insert(a.direction_id.as_str())
        {
            continue;
        }
        if let Some(c) = read.direction(&a.direction_id).await? {
            if !direction_is_terminal(c.status) {
                out.push(KeyedDirection {
                    key: format!("g{}", out.len() + 1),
                    direction_id: c.direction_id,
                    title: c.title,
                });
            }
        }
        // A direction with pct > 0 but that does not exist at all, or is
        // terminal, is simply not offered.
    }
    Ok(out)
}

// ---------------------------------------------------------------- reflex

/// §11.4.3's carry reflex: EVERY planned/in_progress task in P (all of it,
/// not a subset — the model may later choose to carry only some).
#[must_use]
pub fn carry_reflex(prev_tasks: &[Task]) -> Vec<TaskId> {
    prev_tasks
        .iter()
        .filter(|t| matches!(t.status, TaskStatus::Planned | TaskStatus::InProgress))
        .map(|t| t.id.clone())
        .collect()
}

fn status_tier(s: TaskStatus) -> u8 {
    match s {
        TaskStatus::InProgress => 0,
        TaskStatus::Planned => 1,
        // Backlog is the ONLY other non-terminal status `week_tasks`'s
        // non-terminal filter can hand this function (Done/Dropped/
        // CarriedOver are terminal — see `task_is_terminal` at every call
        // site); kept as its own tier rather than folded into `Planned` so
        // §11.4.3's literal "in_progress → planned → backlog" three-tier
        // order is exactly what this returns, not an approximation of it.
        TaskStatus::Backlog => 2,
        _ => 3,
    }
}

/// §11.4.3's reorder reflex: `in_progress → planned → backlog`, same-tier
/// ties broken by the task's OWN Direction's rhythm-alloc `pct` (descending;
/// no Direction or no alloc entry = `0`), then `created_at` ascending.
/// Operates on `week_tasks_w` AS GIVEN — the caller is responsible for
/// having already filtered it to non-terminal tasks (`week_tasks_non_terminal`).
#[must_use]
pub fn reorder_reflex(week_tasks_w: &[Task], alloc: &[Alloc]) -> Vec<TaskId> {
    let pct_of = |d: &Option<DirectionId>| -> u32 {
        d.as_deref()
            .and_then(|id| alloc.iter().find(|a| a.direction_id == id))
            .map_or(0, |a| a.pct)
    };
    let mut tasks: Vec<&Task> = week_tasks_w.iter().collect();
    tasks.sort_by(|a, b| {
        status_tier(a.status)
            .cmp(&status_tier(b.status))
            .then(pct_of(&b.direction_id).cmp(&pct_of(&a.direction_id)))
            .then(a.created_at.cmp(&b.created_at))
    });
    tasks.into_iter().map(|t| t.id.clone()).collect()
}

// ---------------------------------------------------------------- model step

const PROPOSE_SYSTEM_PROMPT: &str = "You are drafting scheduling suggestions for one week. \
You may: pick tasks from last week to carry over, reorder this week's tasks, and propose at \
most 3 brand-new tasks for under-allocated Directions. Respond with JSON only, matching the \
given schema. Omit anything you have no useful suggestion for (empty array is fine).";

const MAX_REASON_CHARS: usize = 200;
const MAX_NEW_TASKS: usize = 3;
const MAX_NEW_TASK_TITLE_CHARS: usize = 120;

/// One decided propose outcome, from ANY step of the ladder (model or the
/// combined reflex fallback): `order` is always a FULL permutation of
/// `w_candidates` (padded/reflex-computed — never partial), `carry`/
/// `new_tasks` may be empty (meaning "nothing to carry over" / "no new
/// tasks", not a failure).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposeDecision {
    pub carry: Vec<TaskId>,
    pub order: Vec<TaskId>,
    pub new_tasks: Vec<NewTask>,
    pub reason: String,
}

#[must_use]
pub fn propose_schema(
    p_candidates: &[KeyedTask],
    w_candidates: &[KeyedTask],
    g_candidates: &[KeyedDirection],
) -> Map<String, Value> {
    let keys = |c: &[KeyedTask]| -> Vec<Value> {
        c.iter().map(|k| Value::String(k.key.clone())).collect()
    };
    let carry_enum = keys(p_candidates);
    let order_enum = keys(w_candidates);
    let direction_enum: Vec<Value> = g_candidates
        .iter()
        .map(|k| Value::String(k.key.clone()))
        .collect();
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "carry": {"type": "array", "items": {"type": "string", "enum": carry_enum}},
            "order": {"type": "array", "items": {"type": "string", "enum": order_enum}},
            "new_tasks": {
                "type": "array",
                "maxItems": MAX_NEW_TASKS,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "title": {"type": "string", "minLength": 1, "maxLength": MAX_NEW_TASK_TITLE_CHARS},
                        "direction": {"type": "string", "enum": direction_enum}
                    },
                    "required": ["title", "direction"]
                }
            },
            "reason": {"type": "string", "maxLength": MAX_REASON_CHARS}
        },
        "required": ["carry", "order", "new_tasks", "reason"]
    });
    match schema {
        Value::Object(m) => m,
        _ => unreachable!("json!({{...}}) always builds a Value::Object"),
    }
}

#[must_use]
pub fn build_propose_request(
    p_candidates: &[KeyedTask],
    w_candidates: &[KeyedTask],
    g_candidates: &[KeyedDirection],
    engine: Engine,
) -> ModelRequest {
    let schema = propose_schema(p_candidates, w_candidates, g_candidates);
    let user = json!({
        "carry_candidates": p_candidates.iter().map(|k| json!({"key": k.key, "title": k.title})).collect::<Vec<_>>(),
        "reorder_candidates": w_candidates.iter().map(|k| json!({"key": k.key, "title": k.title})).collect::<Vec<_>>(),
        "gap_directions": g_candidates.iter().map(|k| json!({"key": k.key, "title": k.title})).collect::<Vec<_>>(),
    })
    .to_string();
    ModelRequest {
        messages: vec![
            ModelMessage {
                role: Role::System,
                content: PROPOSE_SYSTEM_PROMPT.to_string(),
            },
            ModelMessage {
                role: Role::User,
                content: user,
            },
        ],
        schema_name: "sin90_propose",
        schema,
        max_tokens: 512,
        complexity: if engine == Engine::Executive {
            Complexity::Complex
        } else {
            Complexity::Simple
        },
    }
}

/// Tolerates ONE layer of a ```` ```json ```` (or bare ```` ``` ````) fence,
/// same convention `classify::parse_classify_reply` uses — kept as its own
/// tiny copy, NOT widened to `pub` and shared like [`super::classify::
/// is_cf_format_char`] is (2026-09-24 review, same wording that function's
/// own doc uses): unlike the Cf table, where a future fix must apply to
/// every capability at once, this is three lines with no meaningful "table"
/// to drift — a second copy costs nothing and doesn't widen `classify`'s
/// public surface for it.
fn strip_json_fence(s: &str) -> &str {
    let t = s.trim();
    for prefix in ["```json", "```"] {
        if let Some(rest) = t.strip_prefix(prefix) {
            return rest.strip_suffix("```").unwrap_or(rest).trim();
        }
    }
    t
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelNewTask {
    title: String,
    direction: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelPropose {
    carry: Vec<String>,
    order: Vec<String>,
    new_tasks: Vec<ModelNewTask>,
    reason: String,
}

fn dedupe_or_bad_output<'a>(keys: impl Iterator<Item = &'a String>) -> Result<(), &'static str> {
    let mut seen = HashSet::new();
    for k in keys {
        if !seen.insert(k.as_str()) {
            return Err("bad_output");
        }
    }
    Ok(())
}

/// The program's recheck of the model's reply (§11.4.3): every key must be
/// in THIS run's candidate set, `order`/`carry`/`new_tasks` keys must not
/// repeat, titles (after stripping control characters) must be non-blank,
/// and at most [`MAX_NEW_TASKS`] new tasks. `order` is completed to a FULL
/// permutation of `w_candidates` — anything the model left out is appended
/// in `w_candidates`' own (current-sort-key) order (§11.4.3: "模型漏掉的按
/// 原顺序补在后面，重复即 bad_output").
pub fn parse_propose_reply(
    text: &str,
    p_candidates: &[KeyedTask],
    w_candidates: &[KeyedTask],
    g_candidates: &[KeyedDirection],
) -> Result<ProposeDecision, &'static str> {
    let parsed: ModelPropose =
        serde_json::from_str(strip_json_fence(text)).map_err(|_| "bad_output")?;
    if parsed.reason.chars().count() > MAX_REASON_CHARS {
        return Err("bad_output");
    }

    dedupe_or_bad_output(parsed.carry.iter())?;
    let mut carry = Vec::with_capacity(parsed.carry.len());
    for key in &parsed.carry {
        let c = p_candidates
            .iter()
            .find(|c| &c.key == key)
            .ok_or("bad_output")?;
        carry.push(c.task_id.clone());
    }

    dedupe_or_bad_output(parsed.order.iter())?;
    let mut explicit = Vec::with_capacity(parsed.order.len());
    let mut explicit_keys: HashSet<&str> = HashSet::with_capacity(parsed.order.len());
    for key in &parsed.order {
        let c = w_candidates
            .iter()
            .find(|c| &c.key == key)
            .ok_or("bad_output")?;
        explicit.push(c.task_id.clone());
        explicit_keys.insert(c.key.as_str());
    }
    let mut order = explicit;
    for c in w_candidates {
        if !explicit_keys.contains(c.key.as_str()) {
            order.push(c.task_id.clone());
        }
    }

    if parsed.new_tasks.len() > MAX_NEW_TASKS {
        return Err("bad_output");
    }
    let mut new_tasks = Vec::with_capacity(parsed.new_tasks.len());
    for nt in &parsed.new_tasks {
        // H2/L2 (2026-09-24 review): the SAME cleaning pass `build_rationale`
        // uses (`clean_ai_text` — control/Cf chars stripped, whitespace-class
        // control chars turned into a space instead of deleted, trimmed),
        // THEN recheck the length — a title that was 5000 chars of otherwise
        // -legal text must not sneak past `MAX_NEW_TASK_TITLE_CHARS` just
        // because the JSON schema's `maxLength` is (at best) a hint a local
        // model may not honor.
        let cleaned_title = clean_ai_text(&nt.title);
        if cleaned_title.is_empty() {
            return Err("bad_output");
        }
        if cleaned_title.chars().count() > MAX_NEW_TASK_TITLE_CHARS {
            return Err("bad_output");
        }
        let g = g_candidates
            .iter()
            .find(|g| g.key == nt.direction)
            .ok_or("bad_output")?;
        new_tasks.push(NewTask {
            title: cleaned_title,
            direction_id: Some(g.direction_id.clone()),
        });
    }

    Ok(ProposeDecision {
        carry,
        order,
        new_tasks,
        reason: parsed.reason,
    })
}

// ---------------------------------------------------------------- rationale

const MAX_RATIONALE_CHARS: usize = 280;

/// H2 (2026-09-24 review): the full set of characters `propose` strips from
/// AI-produced plain text (`rationale` and a new task's `title`) — plain
/// control characters, U+2028 (LINE SEPARATOR) / U+2029 (PARAGRAPH
/// SEPARATOR) (NOT covered by `char::is_control()`, but just as capable of
/// splitting a single-line rationale/title across lines a naive renderer
/// wasn't expecting), and `classify::is_cf_format_char`'s hand-picked
/// Unicode Cf subset (bidi overrides, zero-width characters, BOM) — same
/// source-of-truth convention `strip_json_fence`'s own doc above uses for
/// "kept as its own tiny copy, not a shared cross-file item" EXCEPT this one
/// specific function (`is_cf_format_char`) IS shared, because unlike a
/// three-line fence-stripping helper, a fix to the Cf table (e.g. widening
/// the bidi-override range) is exactly the kind of change that must apply to
/// every capability that strips Cf characters at once, not drift between
/// two hand-kept copies.
fn is_stripped_char(c: char) -> bool {
    c.is_control() || c == '\u{2028}' || c == '\u{2029}' || super::classify::is_cf_format_char(c)
}

/// L2 (2026-09-24 review round 3): whitespace-class control characters —
/// TAB/LF/VT/FF/CR (U+09-0D), NEL (U+85), and the two Unicode line/paragraph
/// separators [`is_stripped_char`] already treats as control-like (U+2028/
/// U+2029) — are REPLACED with a single ASCII space, never just deleted:
/// deleting `"hello\tworld"`'s tab would silently glue it into
/// `"helloworld"`, one word where the source had two. Every OTHER stripped
/// character (a plain non-whitespace control char, or one of
/// `is_cf_format_char`'s Cf code points — zero-width, BOM, bidi overrides)
/// has no width worth preserving and is deleted outright, same as before.
fn is_whitespace_class_control(c: char) -> bool {
    matches!(c as u32, 0x09..=0x0D | 0x85 | 0x2028 | 0x2029)
}

/// The ONE cleaning pass every piece of AI-produced plain text in this file
/// goes through (a new task's `title`, and a decision's `reason` via
/// [`build_rationale`]) — map whitespace-class control chars to `' '` FIRST,
/// then drop everything [`is_stripped_char`] still flags (control chars that
/// were NOT whitespace-class, plus the Cf subset), then collapse any RUN of
/// whitespace (however it got there — a converted control char, several in a
/// row, or ordinary repeated ASCII spaces the model typed) down to a single
/// `' '` and trim the result's leading/trailing whitespace (2026-09-24
/// review round 4, L-h: `str::split_whitespace().join(" ")` does both at
/// once — split on ANY Unicode whitespace, drop empty leading/trailing
/// pieces, rejoin with exactly one space). A single shared function, not two
/// copies that could drift (M5b's same "single source of truth" posture,
/// applied here to text cleaning instead of candidate filtering).
fn clean_ai_text(s: &str) -> String {
    let mapped: String = s
        .chars()
        .map(|c| {
            if is_whitespace_class_control(c) {
                ' '
            } else {
                c
            }
        })
        .filter(|c| !is_stripped_char(*c))
        .collect();
    mapped.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Same shape §11.4 公共's "提议形状" gives every capability:
/// `"<engine>: <理由>"`, cleaned via [`clean_ai_text`] (shared with the
/// new-task title cleaning above), truncated to 280 chars.
fn build_rationale(engine: Engine, reason: &str) -> String {
    let cleaned = clean_ai_text(reason);
    let s = format!("{}: {}", engine.as_str(), cleaned);
    if s.chars().count() > MAX_RATIONALE_CHARS {
        s.chars().take(MAX_RATIONALE_CHARS).collect()
    } else {
        s
    }
}

// ---------------------------------------------------------------- run driver

/// Which of the three §11.4.3 sub-proposals an [`AiRunItem`]-shaped item
/// result belongs to (the HTTP layer's `target` string).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposeKind {
    Carry,
    Reorder,
    Create,
}
impl ProposeKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Carry => "propose.carry",
            Self::Reorder => "propose.reorder",
            Self::Create => "propose.create",
        }
    }
}

/// Propose's OWN item-result vocabulary (2026-09-24 review, H3/M2) — NOT
/// `classify::ItemResult`, which has no way to say "a still-valid pending
/// proposal of this exact shape already covers this week; this run did not
/// even TRY" (`Skipped`, §11.4 公共's "去重"). Adding that to the SHARED
/// `classify::ItemResult` would force an unrelated capability's own
/// exhaustive `item_result_str` match to grow an arm it can never actually
/// produce (classify's own dedup bypasses `ItemResult` entirely, at the
/// `http::ai_classify` layer). The HTTP layer maps this to the SAME wire
/// vocabulary `classify::ItemResult` maps to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposeItemResult {
    /// A proposal was submitted; the id is `sin90_proposals.id`.
    Proposed(String),
    /// Nothing needed changing (the decision's own content, once mapped to
    /// ops, was empty for this kind) — a real decision was reached, it just
    /// had nothing to add here.
    Nothing,
    /// Capacity/budget/deadline hit — item left for a later run.
    Deferred,
    /// A produced decision failed `AiSink::submit`'s dry run (state moved
    /// between planning and submit).
    Rejected,
    /// The run aborted (§11.3.4) before or during this item.
    Aborted,
    /// H3 (design §11.4 公共's "去重"): a still-valid PENDING proposal of
    /// this exact shape already exists for this week — this run did not
    /// attempt to produce (or submit) a new one at all.
    Skipped,
}

#[derive(Debug, Clone)]
pub struct ProposeItem {
    pub kind: ProposeKind,
    pub result: ProposeItemResult,
}

/// §11.4 公共's "去重" (H3, 2026-09-24 review): computed by the HTTP layer
/// (needs `Sin90Store::list_pending_proposals`, not part of `AiReadModel`/
/// `AiSink`'s deliberately narrow surface — same reasoning
/// `ai_classify::dedup_targets` gives) and threaded into [`run_propose`] so
/// the ladder never even attempts a reorder/create this run already knows is
/// covered, and never re-offers a prev-week task some other still-pending
/// carry proposal already claims.
#[derive(Debug, Clone, Default)]
pub struct ProposeDedup {
    /// Prev-week task ids already covered by a still-valid PENDING
    /// `CarryOverTask` proposal for this week — excluded from `p_candidates`
    /// (and from the carry reflex) so they are never re-offered; any OTHER
    /// prev-week task not in this set is still a normal candidate.
    pub excluded_carry_task_ids: HashSet<TaskId>,
    /// A still-valid PENDING `ReorderTasks` proposal for this week exists
    /// whose `order` covers EXACTLY W's current non-terminal task set (the
    /// coordinator's own 2026-09-24 design clarification, `docs/DESIGN-
    /// LIFEOS.md` §11.4.3 — "reorder 的『仍有效』额外要求挂起提议的 order
    /// 恰好覆盖 W 当前全部非终态任务").
    pub skip_reorder: bool,
    /// M-b (2026-09-24 review round 4): Direction ids excluded from
    /// `g_candidates` — matching carry's own PER-ITEM exclusion posture, not
    /// reorder's blanket one. A Direction referenced by a still-valid
    /// pending `CreateTasks` proposal is excluded ONLY WHILE it is still a
    /// current gap (a Direction that stopped being a gap for its own
    /// reasons — e.g. it now has a task in W — was never a candidate to
    /// begin with, nothing to exclude). A pending proposal naming several
    /// Directions excludes exactly the ones still relevant, not all-or-
    /// nothing for the whole draft.
    pub excluded_create_direction_ids: HashSet<DirectionId>,
    /// L-4's short-circuit flag: true only once EVERY current gap Direction
    /// has ended up in `excluded_create_direction_ids` (2026-09-24 review
    /// round 4, M-b: "剔完为空才整类跳过" — skip the WHOLE class only after
    /// per-item exclusion leaves nothing behind). `run_propose` still
    /// applies `excluded_create_direction_ids` to a FRESH `gap_directions`
    /// read regardless of this flag — it exists purely so `run_propose` can
    /// skip that read (and the whole ladder) entirely when combined with an
    /// empty carry candidate set and `skip_reorder` (§11.4 公共's L-4).
    pub skip_create: bool,
}

fn mint_id() -> String {
    crate::core::ulid()
}

/// Submits one of the three sub-proposals, cloning `rec` with a FRESH id so
/// each submitted proposal gets its OWN call row (§11.3.5: "每条 AI 提议恰好
/// 对应一行 ok=1 调用记录") — even though all three may stem from the SAME
/// underlying model/reflex decision. The CALLER has already checked `ops` is
/// non-empty and this kind is not dedup-skipped.
///
/// `is_first` (2026-09-24 review M1): only the FIRST of the (up to three)
/// ACTUALLY-ATTEMPTED sub-proposals keeps `rec`'s `prompt_tokens`/
/// `completion_tokens`/`latency_ms` — every sibling row gets those zeroed
/// (`NULL`/`NULL`/`0`) REGARDLESS of whether it ends up `ok=1` (submitted)
/// or rejected. Without this, a single model call's token usage would be
/// double- or triple-counted if summed across `sin90_ai_calls` rows (three
/// rows, each claiming the SAME `prompt_tokens`, all tracing back to ONE
/// actual model request).
async fn submit_one<S: AiSink>(
    sink: &S,
    kind: ProposeKind,
    ops: Vec<Sin90Op>,
    rationale: &str,
    rec: &AiCallRecord,
    is_first: bool,
) -> ProposeItemResult {
    let draft = ProposalDraft {
        id: format!("ai-propose-{}", mint_id()),
        ops,
        rationale: Some(rationale.to_string()),
    };
    let draft_id = draft.id.clone();
    let mut this_rec = rec.clone();
    this_rec.id = mint_id();
    if !is_first {
        this_rec.prompt_tokens = None;
        this_rec.completion_tokens = None;
        this_rec.latency_ms = 0;
    }
    match sink
        .submit(Capability::Propose, draft, this_rec.clone())
        .await
    {
        Ok(()) => ProposeItemResult::Proposed(draft_id),
        Err(e) => {
            tracing::warn!(error = %e, kind = kind.as_str(), "propose: a decision failed submit's dry run (state moved)");
            this_rec.ok = false;
            this_rec.proposal_id = None;
            this_rec.error_kind = Some(match &e {
                SinkError::Invalid(_) => "rejected_by_precheck",
                SinkError::Store(_) => "submit_store_error",
            });
            // L-e (2026-09-24 review round 4): NOT literally R6's own scope
            // (§11.3.5's R6 is about a NON-producing step's `record_call`)
            // — this row started as a genuinely PRODUCED decision that
            // `submit`'s dry run then rejected, so it is being rewritten
            // into a failure record here, not recorded as one from the
            // start. It shares R6's same best-effort posture (`warn!`, not
            // fatal) for the SAME reason: the model/rule did its job
            // correctly, only the STORE-side write failed, and that must
            // not abort the run either.
            if let Err(record_err) = sink.record_call(this_rec).await {
                tracing::warn!(error = %record_err, kind = kind.as_str(), "propose: failed to record a rejected-at-submit call (not fatal, R6's same posture)");
            }
            ProposeItemResult::Rejected
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn propose_decide<M, S, R>(
    run_id: &str,
    week_id: &WeekId,
    p_candidates: &[KeyedTask],
    w_candidates: &[KeyedTask],
    g_candidates: &[KeyedDirection],
    reflex_carry: Vec<TaskId>,
    reflex_order: Vec<TaskId>,
    current_order: &[TaskId],
    dedup: &ProposeDedup,
    steps: &[Step],
    model: Option<&M>,
    sink: &S,
    read: &R,
) -> [ProposeItem; 3]
where
    M: ModelPort,
    S: AiSink,
    R: AiReadModel,
{
    let mut st = RunState::new(std::time::Instant::now());
    let outcome = run_item(
        run_id,
        Capability::Propose,
        steps,
        &mut st,
        model,
        sink,
        read,
        |engine| build_propose_request(p_candidates, w_candidates, g_candidates, engine),
        |reply: &ModelReply| {
            parse_propose_reply(&reply.text, p_candidates, w_candidates, g_candidates)
        },
        || None, // propose has no ReflexDecisive step (§11.3.3)
        || {
            // L3 (2026-09-24 review round 3, design §11.4.3): reflex ALWAYS
            // has an answer for propose — even "nothing needs to change" is
            // a real, completed decision, not "couldn't decide" (unlike
            // classify's R1/R2, which genuinely can come up empty-handed).
            // Always `Some`, never `None`: `run_item`'s `ReflexFallback` step
            // records `None` as a FAILED attempt (`ok=0, error_kind=
            // "no_match"`), which would misrepresent "reflex looked and
            // confirmed nothing to do" as a rule that fell through — the
            // model path's "decisive non-match" (classify's `none`/`low`
            // confidence) is ALSO `ok=1`, and propose's reflex should be
            // consistent with that, not with R1/R2's genuine "couldn't
            // decide" semantics.
            Some(ProposeDecision {
                carry: reflex_carry.clone(),
                order: reflex_order.clone(),
                new_tasks: Vec::new(),
                reason: "every planned/in_progress task from last week; tasks re-ordered by \
                         status and Direction quota"
                    .to_string(),
            })
        },
        crate::core::now_iso8601,
        mint_id,
    )
    .await;

    let by_kind = |k: ProposeKind, r: ProposeItemResult| ProposeItem { kind: k, result: r };

    match outcome {
        Outcome::Produced { value, engine, rec } => {
            let rationale = build_rationale(engine, &value.reason);
            let carry_ops: Vec<Sin90Op> = value
                .carry
                .iter()
                .map(|task_id| Sin90Op::CarryOverTask {
                    task_id: task_id.clone(),
                    to_week: week_id.clone(),
                })
                .collect();
            let reorder_ops: Vec<Sin90Op> = if value.order == current_order {
                Vec::new()
            } else {
                vec![Sin90Op::ReorderTasks {
                    week_id: week_id.clone(),
                    order: value.order.clone(),
                }]
            };
            let create_ops: Vec<Sin90Op> = if value.new_tasks.is_empty() {
                Vec::new()
            } else {
                vec![Sin90Op::CreateTasks {
                    week_id: week_id.clone(),
                    tasks: value.new_tasks.clone(),
                }]
            };

            // M1: exactly ONE of the (up to three) attempted sub-proposals —
            // whichever is first in carry/reorder/create priority order —
            // keeps `rec`'s usage/latency; H3: a dedup-skipped kind is never
            // "attempted" at all (no draft built, no DB write), regardless
            // of what the decision would otherwise have proposed for it.
            let carry_will_attempt = !carry_ops.is_empty();
            let reorder_will_attempt = !dedup.skip_reorder && !reorder_ops.is_empty();
            let create_will_attempt = !dedup.skip_create && !create_ops.is_empty();
            let is_first_carry = carry_will_attempt;
            let is_first_reorder = reorder_will_attempt && !carry_will_attempt;
            let is_first_create =
                create_will_attempt && !carry_will_attempt && !reorder_will_attempt;

            let carry_result = if carry_will_attempt {
                Some(
                    submit_one(
                        sink,
                        ProposeKind::Carry,
                        carry_ops,
                        &rationale,
                        &rec,
                        is_first_carry,
                    )
                    .await,
                )
            } else {
                None
            };
            let reorder_result = if dedup.skip_reorder {
                Some(ProposeItemResult::Skipped)
            } else if reorder_will_attempt {
                Some(
                    submit_one(
                        sink,
                        ProposeKind::Reorder,
                        reorder_ops,
                        &rationale,
                        &rec,
                        is_first_reorder,
                    )
                    .await,
                )
            } else {
                None
            };
            let create_result = if dedup.skip_create {
                Some(ProposeItemResult::Skipped)
            } else if create_will_attempt {
                Some(
                    submit_one(
                        sink,
                        ProposeKind::Create,
                        create_ops,
                        &rationale,
                        &rec,
                        is_first_create,
                    )
                    .await,
                )
            } else {
                None
            };

            if !carry_will_attempt && !reorder_will_attempt && !create_will_attempt {
                // L-e (2026-09-24 review round 4, corrected wording): a real
                // decision was reached (model or reflex), but NONE of the
                // three kinds ended up attempted — for WHATEVER reason each
                // one individually wasn't: its own content was empty
                // (carry had nothing to carry, reorder matched the current
                // order, create had no new tasks) OR it was dedup-skipped
                // (`dedup.skip_reorder`/`dedup.skip_create`). Either way
                // this is still `ok = 1` (§11.4.1's sibling posture for
                // classify's "none"/"low confidence": the model/rule did
                // its job, it just had nothing left to add here). `rec` was
                // never written by `run_item` itself (Produced never
                // writes — §11.3.5's atomicity), so THIS is the one and
                // only place it gets recorded.
                if let Err(e) = sink.record_call(rec).await {
                    tracing::warn!(error = %e, run_id = %run_id, "propose: failed to record a decisive no-op decision (R6, not fatal)");
                }
            }

            [
                by_kind(
                    ProposeKind::Carry,
                    carry_result.unwrap_or(ProposeItemResult::Nothing),
                ),
                by_kind(
                    ProposeKind::Reorder,
                    reorder_result.unwrap_or(ProposeItemResult::Nothing),
                ),
                by_kind(
                    ProposeKind::Create,
                    create_result.unwrap_or(ProposeItemResult::Nothing),
                ),
            ]
        }
        Outcome::Nothing => [
            by_kind(ProposeKind::Carry, ProposeItemResult::Nothing),
            by_kind(
                ProposeKind::Reorder,
                if dedup.skip_reorder {
                    ProposeItemResult::Skipped
                } else {
                    ProposeItemResult::Nothing
                },
            ),
            by_kind(
                ProposeKind::Create,
                if dedup.skip_create {
                    ProposeItemResult::Skipped
                } else {
                    ProposeItemResult::Nothing
                },
            ),
        ],
        Outcome::Deferred => [
            by_kind(ProposeKind::Carry, ProposeItemResult::Deferred),
            by_kind(ProposeKind::Reorder, ProposeItemResult::Deferred),
            by_kind(ProposeKind::Create, ProposeItemResult::Deferred),
        ],
        Outcome::Aborted => [
            by_kind(ProposeKind::Carry, ProposeItemResult::Aborted),
            by_kind(ProposeKind::Reorder, ProposeItemResult::Aborted),
            by_kind(ProposeKind::Create, ProposeItemResult::Aborted),
        ],
    }
}

/// Runs the whole propose capability for target week `week` (§11.4.3):
/// reads W's non-terminal tasks, P (the most recent still-open week before
/// W, if any) and its planned/in_progress tasks, the current rhythm
/// allocation, and the resulting "缺口 Direction" set, then drives ONE
/// ladder decision and splits it into up to three submitted proposals.
/// `week` is assumed already validated OPEN by [`select_week`] — this
/// function does not re-check it. `dedup` (H3) is computed by the caller
/// (the HTTP layer, which alone can see `Sin90Store::list_pending_
/// proposals`) — pass [`ProposeDedup::default()`] for "nothing pending".
pub async fn run_propose<M, S, R>(
    run_id: &str,
    week: &Week,
    dedup: &ProposeDedup,
    access: ModelAccess,
    model: Option<&M>,
    sink: &S,
    read: &R,
) -> Vec<ProposeItem>
where
    M: ModelPort,
    S: AiSink,
    R: AiReadModel,
{
    let settings = read.settings().await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "propose: settings read failed, defaulting to executive disabled");
        Default::default()
    });
    // L2 (2026-09-24 review): the FULL, unfiltered task list — needed by
    // `gap_directions`'s "W 里没有任何任务" check, which must count terminal
    // tasks too (a Direction with only a `done` task in W is still not a
    // gap). `week_tasks_w` (below) stays non-terminal-only — that is what
    // `w_candidates`/`current_order`/the reorder reflex actually operate on.
    let week_tasks_all_raw = read.week_tasks(&week.id).await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "propose: week_tasks read failed, treating as empty");
        Vec::new()
    });
    let week_tasks_w: Vec<Task> = week_tasks_all_raw
        .iter()
        .filter(|t| !task_is_terminal(t.status))
        .cloned()
        .collect();
    let current_order: Vec<TaskId> = week_tasks_w.iter().map(|t| t.id.clone()).collect();
    let w_candidates = keyed_tasks("w", &week_tasks_w);

    let prev_week = read
        .previous_open_week(&week.iso_week)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "propose: previous_open_week read failed, treating as none");
            None
        });
    let prev_tasks: Vec<Task> = match &prev_week {
        Some(p) => read.week_tasks(&p.id).await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "propose: previous week's week_tasks read failed, treating as empty");
            Vec::new()
        }),
        None => Vec::new(),
    };
    // M5b (2026-09-24 review): `p_candidates` (what the MODEL sees) is
    // derived FROM `carry_reflex`'s own output — not a second, independently
    // written status filter that could silently drift from reflex's actual
    // eligibility rule (`Planned | InProgress`, e.g. a `backlog` prev-week
    // task must never appear as a `p*` candidate either). H3's dedup
    // exclusion is applied to that SAME id list, once, right here — both
    // `reflex_carry` (below) and `p_candidates` end up built from identical
    // "reflex says carryable, minus what's already covered" data.
    let reflex_carry: Vec<TaskId> = carry_reflex(&prev_tasks)
        .into_iter()
        .filter(|id| !dedup.excluded_carry_task_ids.contains(id))
        .collect();
    // L-4 (2026-09-24 review round 3): if carry has NO candidates left after
    // H3's exclusion, AND reorder/create are BOTH already dedup-skipped,
    // there is NOTHING this run could possibly attempt regardless of what
    // the model/reflex would say — the model's own candidate universe for
    // carry is ALSO empty in that case (§11.4.3), so it could only ever echo
    // back `carry: []`. Skip the ladder ENTIRELY: no model call, no
    // `sin90_ai_calls` row at all (not even the "reflex reached an empty
    // decision, ok=1" row L3 gives the NORMAL empty-decision case — THIS is
    // "we didn't even try", a different, cheaper outcome).
    if reflex_carry.is_empty() && dedup.skip_reorder && dedup.skip_create {
        return vec![
            ProposeItem {
                kind: ProposeKind::Carry,
                result: ProposeItemResult::Nothing,
            },
            ProposeItem {
                kind: ProposeKind::Reorder,
                result: ProposeItemResult::Skipped,
            },
            ProposeItem {
                kind: ProposeKind::Create,
                result: ProposeItemResult::Skipped,
            },
        ];
    }

    let prev_tasks_by_id: std::collections::HashMap<&TaskId, &Task> =
        prev_tasks.iter().map(|t| (&t.id, t)).collect();
    let carryable_prev_tasks: Vec<Task> = reflex_carry
        .iter()
        .filter_map(|id| prev_tasks_by_id.get(id).map(|t| (*t).clone()))
        .collect();
    let p_candidates = keyed_tasks("p", &carryable_prev_tasks);

    // L1 (2026-09-24 review): read ONCE, shared by the reorder reflex AND
    // `gap_directions` (which used to re-read it a second time itself).
    let alloc = read.rhythm_alloc().await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "propose: rhythm_alloc read failed, treating as empty");
        Vec::new()
    });
    // M-b (2026-09-24 review round 4): PER-ITEM exclusion — a gap Direction
    // a still-valid pending create already covers is filtered OUT of
    // `g_candidates` here, not treated as an all-or-nothing reason to skip
    // the whole `CreateTasks` attempt (that remains `dedup.skip_create`'s
    // job, for the case EVERY current gap ends up excluded).
    let g_candidates: Vec<KeyedDirection> = gap_directions(read, &alloc, &week_tasks_all_raw)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "propose: gap_directions read failed, treating as empty");
            Vec::new()
        })
        .into_iter()
        .filter(|g| {
            !dedup
                .excluded_create_direction_ids
                .contains(&g.direction_id)
        })
        .collect();

    let reflex_order = reorder_reflex(&week_tasks_w, &alloc);

    let steps = plan(Capability::Propose, access, settings, model.is_some());
    propose_decide(
        run_id,
        &week.id,
        &p_candidates,
        &w_candidates,
        &g_candidates,
        reflex_carry,
        reflex_order,
        &current_order,
        dedup,
        &steps,
        model,
        sink,
        read,
    )
    .await
    .into_iter()
    .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::future::Future;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::ai::ports::ModelFailure;
    use crate::ai::NoModelPort;
    use crate::core::{
        Energy, ProposalSource, ProposalStatus, Sin90Proposal, TaskKind, WeekStatus,
    };
    use crate::store::test_hooks;
    use crate::store::Sin90Store;

    // ---- pure logic -----------------------------------------------------

    fn kt(key: &str, id: &str, title: &str) -> KeyedTask {
        KeyedTask {
            key: key.into(),
            task_id: id.into(),
            title: title.into(),
        }
    }
    fn kd(key: &str, id: &str, title: &str) -> KeyedDirection {
        KeyedDirection {
            key: key.into(),
            direction_id: id.into(),
            title: title.into(),
        }
    }

    #[test]
    fn parse_propose_reply_happy_path_maps_keys_to_real_ids() {
        let p = vec![kt("p1", "t-prev-1", "old task")];
        let w = vec![kt("w1", "t-w-1", "a"), kt("w2", "t-w-2", "b")];
        let g = vec![kd("g1", "d-gap", "Health")];
        let got = parse_propose_reply(
            r#"{"carry":["p1"],"order":["w2","w1"],"new_tasks":[{"title":"new one","direction":"g1"}],"reason":"ok"}"#,
            &p,
            &w,
            &g,
        )
        .unwrap();
        assert_eq!(got.carry, vec!["t-prev-1".to_string()]);
        assert_eq!(got.order, vec!["t-w-2".to_string(), "t-w-1".to_string()]);
        assert_eq!(got.new_tasks.len(), 1);
        assert_eq!(got.new_tasks[0].title, "new one");
        assert_eq!(got.new_tasks[0].direction_id.as_deref(), Some("d-gap"));
    }

    /// §11.4.3: "模型漏掉的按原顺序补在后面" — an order that names only SOME
    /// of `w_candidates` is padded with the rest, in their ORIGINAL
    /// (candidate-list) relative order.
    #[test]
    fn parse_propose_reply_pads_missing_order_entries_in_original_order() {
        let w = vec![
            kt("w1", "t1", "a"),
            kt("w2", "t2", "b"),
            kt("w3", "t3", "c"),
        ];
        let got = parse_propose_reply(
            r#"{"carry":[],"order":["w3"],"new_tasks":[],"reason":"x"}"#,
            &[],
            &w,
            &[],
        )
        .unwrap();
        assert_eq!(got.order, vec!["t3", "t1", "t2"]);
    }

    /// Positive control for the padding test: a FULL, explicit order is
    /// taken as-is (no padding needed) — proves the padding logic isn't
    /// accidentally always reordering.
    #[test]
    fn parse_propose_reply_full_explicit_order_unchanged() {
        let w = vec![kt("w1", "t1", "a"), kt("w2", "t2", "b")];
        let got = parse_propose_reply(
            r#"{"carry":[],"order":["w2","w1"],"new_tasks":[],"reason":"x"}"#,
            &[],
            &w,
            &[],
        )
        .unwrap();
        assert_eq!(got.order, vec!["t2", "t1"]);
    }

    #[test]
    fn parse_propose_reply_duplicate_order_key_is_bad_output() {
        let w = vec![kt("w1", "t1", "a"), kt("w2", "t2", "b")];
        assert_eq!(
            parse_propose_reply(
                r#"{"carry":[],"order":["w1","w1"],"new_tasks":[],"reason":"x"}"#,
                &[],
                &w,
                &[]
            ),
            Err("bad_output")
        );
    }

    #[test]
    fn parse_propose_reply_duplicate_carry_key_is_bad_output() {
        let p = vec![kt("p1", "t1", "a")];
        assert_eq!(
            parse_propose_reply(
                r#"{"carry":["p1","p1"],"order":[],"new_tasks":[],"reason":"x"}"#,
                &p,
                &[],
                &[]
            ),
            Err("bad_output")
        );
    }

    #[test]
    fn parse_propose_reply_unknown_keys_are_bad_output() {
        let p = vec![kt("p1", "t1", "a")];
        let w = vec![kt("w1", "t2", "b")];
        let g = vec![kd("g1", "d1", "Work")];
        assert_eq!(
            parse_propose_reply(
                r#"{"carry":["p9"],"order":[],"new_tasks":[],"reason":"x"}"#,
                &p,
                &w,
                &g
            ),
            Err("bad_output")
        );
        assert_eq!(
            parse_propose_reply(
                r#"{"carry":[],"order":["w9"],"new_tasks":[],"reason":"x"}"#,
                &p,
                &w,
                &g
            ),
            Err("bad_output")
        );
        assert_eq!(
            parse_propose_reply(
                r#"{"carry":[],"order":[],"new_tasks":[{"title":"x","direction":"g9"}],"reason":"x"}"#,
                &p,
                &w,
                &g
            ),
            Err("bad_output")
        );
    }

    #[test]
    fn parse_propose_reply_over_max_new_tasks_is_bad_output() {
        let g = vec![kd("g1", "d1", "Work")];
        let text = r#"{"carry":[],"order":[],"new_tasks":[
            {"title":"a","direction":"g1"},
            {"title":"b","direction":"g1"},
            {"title":"c","direction":"g1"},
            {"title":"d","direction":"g1"}
        ],"reason":"x"}"#;
        assert_eq!(parse_propose_reply(text, &[], &[], &g), Err("bad_output"));
    }

    /// §11.4.3's recheck: a title that is ONLY control characters (after
    /// stripping them, blank) is `bad_output`, even though the raw string
    /// was non-empty and would pass the schema's `minLength: 1`.
    #[test]
    fn parse_propose_reply_control_char_only_title_is_bad_output() {
        let g = vec![kd("g1", "d1", "Work")];
        let text = r#"{"carry":[],"order":[],"new_tasks":[{"title":"\u0007\u0007","direction":"g1"}],"reason":"x"}"#;
        assert_eq!(parse_propose_reply(text, &[], &[], &g), Err("bad_output"));

        // Positive control: control characters stripped but SOME real text
        // remains is accepted, and the stored title has them removed.
        let ok_text = "{\"carry\":[],\"order\":[],\"new_tasks\":[{\"title\":\"real\\u0007title\",\"direction\":\"g1\"}],\"reason\":\"x\"}";
        let got = parse_propose_reply(ok_text, &[], &[], &g).unwrap();
        assert_eq!(got.new_tasks[0].title, "realtitle");
    }

    /// H2 (2026-09-24 review): a title made of ONLY a zero-width space
    /// (U+200B, `classify::is_cf_format_char`'s Cf subset) or ONLY a bidi
    /// RLO override (U+202E, same subset) cleans down to empty and is
    /// `bad_output` — same posture as a title of only ASCII control
    /// characters. Positive control: ordinary text containing a literal
    /// `##` (`"a ## x"`) is accepted UNCHANGED — this is NOT `summarize`'s
    /// markdown-stripping `normalize`, so a hash in the middle of a title is
    /// just text.
    #[test]
    fn parse_propose_reply_title_cf_chars_are_stripped_ordinary_hashes_are_not() {
        let g = vec![kd("g1", "d1", "Work")];
        let zero_width_only = "{\"carry\":[],\"order\":[],\"new_tasks\":[{\"title\":\"\u{200B}\",\"direction\":\"g1\"}],\"reason\":\"x\"}";
        assert_eq!(
            parse_propose_reply(zero_width_only, &[], &[], &g),
            Err("bad_output")
        );

        let rlo_only = "{\"carry\":[],\"order\":[],\"new_tasks\":[{\"title\":\"\u{202E}\",\"direction\":\"g1\"}],\"reason\":\"x\"}";
        assert_eq!(
            parse_propose_reply(rlo_only, &[], &[], &g),
            Err("bad_output")
        );

        let ordinary_hash = r#"{"carry":[],"order":[],"new_tasks":[{"title":"a ## x","direction":"g1"}],"reason":"x"}"#;
        let got = parse_propose_reply(ordinary_hash, &[], &[], &g).unwrap();
        assert_eq!(got.new_tasks[0].title, "a ## x");
    }

    /// H2: U+2028 (LINE SEPARATOR) / U+2029 (PARAGRAPH SEPARATOR) are NOT
    /// `char::is_control()` but must be stripped anyway (they can split a
    /// single-line title/rationale across lines a naive renderer never
    /// expected); a title of ONLY these characters is `bad_output`.
    #[test]
    fn parse_propose_reply_title_strips_line_and_paragraph_separators() {
        let g = vec![kd("g1", "d1", "Work")];
        let only_separators =
            "{\"carry\":[],\"order\":[],\"new_tasks\":[{\"title\":\"\u{2028}\u{2029}\",\"direction\":\"g1\"}],\"reason\":\"x\"}";
        assert_eq!(
            parse_propose_reply(only_separators, &[], &[], &g),
            Err("bad_output")
        );

        // L2 (2026-09-24 review round 3): a whitespace-class separator
        // becomes a SPACE, not a deletion — "line<U+2028>break" must not
        // collapse into the single glued word "linebreak".
        let mixed =
            "{\"carry\":[],\"order\":[],\"new_tasks\":[{\"title\":\"line\u{2028}break\",\"direction\":\"g1\"}],\"reason\":\"x\"}";
        let got = parse_propose_reply(mixed, &[], &[], &g).unwrap();
        assert_eq!(got.new_tasks[0].title, "line break");
    }

    /// L2 (2026-09-24 review round 3): a title of ONLY ASCII whitespace
    /// cleans (trims) down to empty and is `bad_output` — same posture as a
    /// title of only control/Cf characters.
    #[test]
    fn parse_propose_reply_all_whitespace_title_is_bad_output() {
        let g = vec![kd("g1", "d1", "Work")];
        let text = r#"{"carry":[],"order":[],"new_tasks":[{"title":"   ","direction":"g1"}],"reason":"x"}"#;
        assert_eq!(parse_propose_reply(text, &[], &[], &g), Err("bad_output"));
    }

    /// L2: leading/trailing ASCII whitespace is trimmed from an otherwise
    /// valid title.
    #[test]
    fn parse_propose_reply_title_leading_trailing_whitespace_is_trimmed() {
        let g = vec![kd("g1", "d1", "Work")];
        let text = r#"{"carry":[],"order":[],"new_tasks":[{"title":"  real title  ","direction":"g1"}],"reason":"x"}"#;
        let got = parse_propose_reply(text, &[], &[], &g).unwrap();
        assert_eq!(got.new_tasks[0].title, "real title");
    }

    /// L2: a whitespace-CLASS control character (`\t`, `\n`, `\r`) inside a
    /// title is REPLACED with a space, not deleted — `"a\tb"` must clean to
    /// `"a b"` (two words), never the glued `"ab"`. Mutation target: revert
    /// `clean_ai_text`'s `map` step to a plain `filter` (deleting these
    /// characters like every other stripped one) and this goes red.
    #[test]
    fn parse_propose_reply_title_whitespace_class_control_chars_become_space() {
        let g = vec![kd("g1", "d1", "Work")];
        // JSON string escapes for TAB/LF/CR — a raw, unescaped control
        // character inside a JSON string is invalid JSON and would fail to
        // parse at all (never reaching the title-cleaning logic this test
        // means to exercise).
        for escape in ["\\t", "\\n", "\\r"] {
            let text = format!(
                r#"{{"carry":[],"order":[],"new_tasks":[{{"title":"a{escape}b","direction":"g1"}}],"reason":"x"}}"#
            );
            let got = parse_propose_reply(&text, &[], &[], &g).unwrap();
            assert_eq!(got.new_tasks[0].title, "a b", "input escape {escape:?}");
        }
    }

    /// L-h (2026-09-24 review round 4): a RUN of consecutive whitespace —
    /// several converted control chars in a row, or plain repeated ASCII
    /// spaces the model typed — collapses to exactly ONE space, on the
    /// title side. Mutation target: revert `clean_ai_text` to `.collect::
    /// <String>().trim().to_string()` (no `split_whitespace`/`join`) and
    /// this goes red (multiple spaces survive).
    #[test]
    fn parse_propose_reply_title_collapses_consecutive_whitespace() {
        let g = vec![kd("g1", "d1", "Work")];
        let text = "{\"carry\":[],\"order\":[],\"new_tasks\":[{\"title\":\"a   b\\t\\tc\",\"direction\":\"g1\"}],\"reason\":\"x\"}";
        let got = parse_propose_reply(text, &[], &[], &g).unwrap();
        assert_eq!(got.new_tasks[0].title, "a b c");
    }

    /// L-h: the SAME collapsing on the rationale side (`build_rationale`
    /// shares `clean_ai_text` with title cleaning — this is as much a
    /// regression for that sharing as for the collapsing itself).
    #[test]
    fn build_rationale_collapses_consecutive_whitespace() {
        let s = build_rationale(Engine::Local, "too   many    spaces");
        assert_eq!(s, "local: too many spaces");
    }

    /// H2: a title of 5000 legal characters must not sneak past
    /// `MAX_NEW_TASK_TITLE_CHARS` just because the JSON schema's own
    /// `maxLength` hint is (at best) advisory for a local model —
    /// `parse_propose_reply` re-checks the length itself, on the CLEANED
    /// (post-strip, post-trim) text.
    #[test]
    fn parse_propose_reply_over_length_title_after_cleaning_is_bad_output() {
        let g = vec![kd("g1", "d1", "Work")];
        let long_title = "x".repeat(5000);
        let text = format!(
            r#"{{"carry":[],"order":[],"new_tasks":[{{"title":"{long_title}","direction":"g1"}}],"reason":"x"}}"#
        );
        assert_eq!(parse_propose_reply(&text, &[], &[], &g), Err("bad_output"));

        // Positive control: exactly at the limit is accepted.
        let at_limit = "x".repeat(MAX_NEW_TASK_TITLE_CHARS);
        let ok_text = format!(
            r#"{{"carry":[],"order":[],"new_tasks":[{{"title":"{at_limit}","direction":"g1"}}],"reason":"x"}}"#
        );
        let got = parse_propose_reply(&ok_text, &[], &[], &g).unwrap();
        assert_eq!(
            got.new_tasks[0].title.chars().count(),
            MAX_NEW_TASK_TITLE_CHARS
        );
    }

    /// H2: `build_rationale` strips the SAME character set (reused from
    /// `classify::is_cf_format_char`, plus U+2028/2029) and trims — a
    /// rationale that is ONLY a zero-width space collapses to just the
    /// `"<engine>: "` prefix (empty reason), not a hidden character.
    #[test]
    fn build_rationale_strips_cf_and_line_separators_then_trims() {
        let s = build_rationale(Engine::Local, "\u{200B}\u{2028}real reason\u{2029}");
        assert_eq!(s, "local: real reason");
    }

    #[test]
    fn parse_propose_reply_tolerates_one_json_fence() {
        let w = vec![kt("w1", "t1", "a")];
        let got = parse_propose_reply(
            "```json\n{\"carry\":[],\"order\":[\"w1\"],\"new_tasks\":[],\"reason\":\"ok\"}\n```",
            &[],
            &w,
            &[],
        )
        .unwrap();
        assert_eq!(got.order, vec!["t1".to_string()]);
    }

    #[test]
    fn parse_propose_reply_rejects_unknown_field_and_over_length_reason() {
        assert_eq!(
            parse_propose_reply(
                r#"{"carry":[],"order":[],"new_tasks":[],"reason":"x","extra":1}"#,
                &[],
                &[],
                &[]
            ),
            Err("bad_output")
        );
        let long_reason = "x".repeat(201);
        let text = format!(r#"{{"carry":[],"order":[],"new_tasks":[],"reason":"{long_reason}"}}"#);
        assert_eq!(parse_propose_reply(&text, &[], &[], &[]), Err("bad_output"));
    }

    #[test]
    fn carry_reflex_takes_only_planned_and_in_progress() {
        fn task(id: &str, status: TaskStatus) -> Task {
            Task {
                id: id.into(),
                direction_id: None,
                week_id: Some("w".into()),
                parent_task_id: None,
                title: id.into(),
                status,
                kind: TaskKind::Other,
                energy: Energy::Mid,
                est_minutes: None,
                carried_from: None,
                created_at: "2026-09-24T00:00:00Z".into(),
                updated_at: "2026-09-24T00:00:00Z".into(),
            }
        }
        let tasks = vec![
            task("t1", TaskStatus::Planned),
            task("t2", TaskStatus::InProgress),
            task("t3", TaskStatus::Backlog),
            task("t4", TaskStatus::Done),
        ];
        let mut got = carry_reflex(&tasks);
        got.sort();
        assert_eq!(got, vec!["t1".to_string(), "t2".to_string()]);
    }

    fn tier_task(
        id: &str,
        status: TaskStatus,
        direction_id: Option<&str>,
        created_at: &str,
    ) -> Task {
        Task {
            id: id.into(),
            direction_id: direction_id.map(str::to_string),
            week_id: Some("w".into()),
            parent_task_id: None,
            title: id.into(),
            status,
            kind: TaskKind::Other,
            energy: Energy::Mid,
            est_minutes: None,
            carried_from: None,
            created_at: created_at.into(),
            updated_at: created_at.into(),
        }
    }

    #[test]
    fn reorder_reflex_orders_by_status_tier_then_pct_then_created_at() {
        let alloc = vec![
            Alloc {
                direction_id: "d-high".into(),
                pct: 80,
            },
            Alloc {
                direction_id: "d-low".into(),
                pct: 20,
            },
        ];
        let tasks = vec![
            tier_task(
                "backlog-1",
                TaskStatus::Backlog,
                None,
                "2026-09-01T00:00:00Z",
            ),
            tier_task(
                "planned-low",
                TaskStatus::Planned,
                Some("d-low"),
                "2026-09-02T00:00:00Z",
            ),
            tier_task(
                "planned-high",
                TaskStatus::Planned,
                Some("d-high"),
                "2026-09-03T00:00:00Z",
            ),
            tier_task(
                "in-progress-1",
                TaskStatus::InProgress,
                None,
                "2026-09-04T00:00:00Z",
            ),
        ];
        let got = reorder_reflex(&tasks, &alloc);
        assert_eq!(
            got,
            vec![
                "in-progress-1".to_string(),
                "planned-high".to_string(),
                "planned-low".to_string(),
                "backlog-1".to_string(),
            ]
        );
    }

    /// Positive control / tie-break check: same tier, same (missing) alloc
    /// pct — `created_at` ascending decides it.
    #[test]
    fn reorder_reflex_ties_broken_by_created_at() {
        let tasks = vec![
            tier_task("later", TaskStatus::Planned, None, "2026-09-02T00:00:00Z"),
            tier_task("earlier", TaskStatus::Planned, None, "2026-09-01T00:00:00Z"),
        ];
        let got = reorder_reflex(&tasks, &[]);
        assert_eq!(got, vec!["earlier".to_string(), "later".to_string()]);
    }

    // ---- end-to-end driver, against a REAL Sin90Store --------------------

    #[derive(Clone)]
    struct StubModel(
        Arc<Mutex<Result<ModelReply, ModelFailure>>>,
        Arc<Mutex<u32>>,
    );
    impl StubModel {
        fn always(r: Result<ModelReply, ModelFailure>) -> Self {
            Self(Arc::new(Mutex::new(r)), Arc::default())
        }
        fn calls(&self) -> u32 {
            *self.1.lock().unwrap()
        }
    }
    impl ModelPort for StubModel {
        fn complete(
            &self,
            _req: ModelRequest,
        ) -> impl Future<Output = Result<ModelReply, ModelFailure>> + Send {
            *self.1.lock().unwrap() += 1;
            let r = self.0.lock().unwrap().clone();
            async move { r }
        }
    }

    /// M5b (2026-09-24 review): captures the LAST `ModelRequest` it was
    /// asked to serve, so a test can inspect exactly what candidates the
    /// model was shown — not just what it was allowed to answer with.
    #[derive(Clone, Default)]
    struct CapturingModel {
        last_request: Arc<Mutex<Option<ModelRequest>>>,
        reply_text: Arc<Mutex<String>>,
    }
    impl CapturingModel {
        fn new(reply_text: &str) -> Self {
            Self {
                last_request: Arc::default(),
                reply_text: Arc::new(Mutex::new(reply_text.to_string())),
            }
        }
        fn last_user_message(&self) -> String {
            self.last_request
                .lock()
                .unwrap()
                .as_ref()
                .expect("complete was never called")
                .messages
                .iter()
                .find(|m| m.role == Role::User)
                .unwrap()
                .content
                .clone()
        }
    }
    impl ModelPort for CapturingModel {
        fn complete(
            &self,
            req: ModelRequest,
        ) -> impl Future<Output = Result<ModelReply, ModelFailure>> + Send {
            *self.last_request.lock().unwrap() = Some(req);
            let text = self.reply_text.lock().unwrap().clone();
            async move { Ok(reply(&text)) }
        }
    }

    fn reply(text: &str) -> ModelReply {
        ModelReply {
            text: text.to_string(),
            model_id: Some("test-model".into()),
            tier: crate::ai::ports::ServedTier::Local,
            prompt_tokens: None,
            completion_tokens: None,
        }
    }

    /// Seeds: Direction "Work" with a rhythm alloc of 100% (so it is
    /// eligible to be a "缺口 Direction" whenever W has no task under it); a
    /// PREVIOUS open week `P` (active) with one `planned` and one
    /// `in_progress` task; a target week `W` (planning) with two `planned`
    /// tasks, neither under "Work". Returns
    /// `(store, direction, prev_week, target_week, prev_task_ids, w_task_ids)`.
    async fn seed(
        store: &Sin90Store,
    ) -> (crate::core::Direction, Week, Week, Vec<String>, Vec<String>) {
        let direction = store
            .create_direction("Work", "2026-Q4", None)
            .await
            .unwrap();
        test_hooks::insert_rhythm(store, "rhythm-1").await.unwrap();
        sqlx::query("UPDATE sin90_rhythms SET allocations = ? WHERE id = ?")
            .bind(
                serde_json::to_string(&vec![Alloc {
                    direction_id: direction.id.clone(),
                    pct: 100,
                }])
                .unwrap(),
            )
            .bind("rhythm-1")
            .execute(store.pool())
            .await
            .unwrap();

        let prev = store.create_week("2026-W10").await.unwrap();
        store
            .transition_week(&prev.id, WeekStatus::Active)
            .await
            .unwrap();
        let prev_create = Sin90Proposal {
            id: "seed-prev-tasks".into(),
            status: ProposalStatus::Pending,
            source: ProposalSource::Rule,
            ops: vec![Sin90Op::CreateTasks {
                week_id: prev.id.clone(),
                tasks: vec![
                    NewTask {
                        title: "prev planned".into(),
                        direction_id: None,
                    },
                    NewTask {
                        title: "prev in progress".into(),
                        direction_id: None,
                    },
                ],
            }],
            rationale: None,
        };
        store.submit_proposal(&prev_create).await.unwrap();
        store.apply_proposal(&prev_create.id).await.unwrap();
        let prev_task_ids: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM sin90_tasks WHERE week_id = ? ORDER BY sort_key ASC",
        )
        .bind(&prev.id)
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert_eq!(prev_task_ids.len(), 2);
        let bump = Sin90Proposal {
            id: "seed-prev-bump".into(),
            status: ProposalStatus::Pending,
            source: ProposalSource::Rule,
            ops: vec![Sin90Op::TransitionTask {
                task_id: prev_task_ids[1].clone(),
                to: TaskStatus::InProgress,
            }],
            rationale: None,
        };
        store.submit_proposal(&bump).await.unwrap();
        store.apply_proposal(&bump.id).await.unwrap();

        let target = store.create_week("2026-W11").await.unwrap();
        let w_create = Sin90Proposal {
            id: "seed-w-tasks".into(),
            status: ProposalStatus::Pending,
            source: ProposalSource::Rule,
            ops: vec![Sin90Op::CreateTasks {
                week_id: target.id.clone(),
                tasks: vec![
                    NewTask {
                        title: "w task 1".into(),
                        direction_id: None,
                    },
                    NewTask {
                        title: "w task 2".into(),
                        direction_id: None,
                    },
                ],
            }],
            rationale: None,
        };
        store.submit_proposal(&w_create).await.unwrap();
        store.apply_proposal(&w_create.id).await.unwrap();
        let w_task_ids: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM sin90_tasks WHERE week_id = ? ORDER BY sort_key ASC",
        )
        .bind(&target.id)
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert_eq!(w_task_ids.len(), 2);

        (direction, prev, target, prev_task_ids, w_task_ids)
    }

    /// M5c (2026-09-24 review): a prev-week task that IS already classified
    /// (`direction_id` set — J20's own fixture leaves the source task
    /// unclassified, so its own `direction_id` assertion only proves
    /// "present", not "correct") carries that SAME Direction through
    /// `propose.carry` end to end — the new task's `task.created` event
    /// payload's `direction_id` must equal the SOURCE task's Direction, not
    /// merely be non-null. `apply_op`'s `CarryOverTask` branch (§2 #25)
    /// reads the source row's `direction_id` at APPLY time, not from
    /// anything `propose::propose_decide` itself constructs — so this test
    /// exercises the full path, not just the Op's own already-covered apply
    /// logic in isolation.
    #[tokio::test]
    async fn propose_carry_preserves_source_tasks_classified_direction() {
        let store = Sin90Store::open_memory().await.unwrap();
        let direction = store
            .create_direction("Health", "2026-Q4", None)
            .await
            .unwrap();
        let prev = store.create_week("2026-W25").await.unwrap();
        store
            .transition_week(&prev.id, WeekStatus::Active)
            .await
            .unwrap();
        // Already classified (`direction_id` set) AND already in P as
        // `planned` — created directly + moved via raw SQL, same convention
        // this file's other fixtures use for state with no one-step
        // production write path (`CreateTasks` never sets `direction_id` to
        // a REAL classified value in one step here; `AssignTaskDirection`
        // only works on inbox tasks, not ones already in a week).
        let classified_task = store
            .create_task(
                "already classified",
                Some(&direction.id),
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();
        sqlx::query("UPDATE sin90_tasks SET week_id = ?, status = 'planned' WHERE id = ?")
            .bind(&prev.id)
            .bind(&classified_task.id)
            .execute(store.pool())
            .await
            .unwrap();

        let target = store.create_week("2026-W26").await.unwrap();
        let reader = store.ai_reader();
        let model: Option<&NoModelPort> = None; // reflex: carries every planned/in_progress task.
        let items = run_propose(
            "run-carry-classified",
            &target,
            &ProposeDedup::default(),
            ModelAccess::LocalOnly,
            model,
            &store,
            &reader,
        )
        .await;
        let carry_id = match items
            .iter()
            .find(|i| i.kind == ProposeKind::Carry)
            .unwrap()
            .result
            .clone()
        {
            ProposeItemResult::Proposed(id) => id,
            other => panic!("expected Proposed, got {other:?}"),
        };
        store.apply_proposal(&carry_id).await.unwrap();

        let new_task_id: String =
            sqlx::query_scalar("SELECT id FROM sin90_tasks WHERE carried_from = ?")
                .bind(&classified_task.id)
                .fetch_one(store.pool())
                .await
                .unwrap();
        let events = store
            .list_events(Some("task"), Some(&new_task_id), None, None)
            .await
            .unwrap();
        let created_ev = events
            .iter()
            .find(|e| e.kind == "created")
            .expect("a task.created event for the carried-over task must exist");
        assert_eq!(
            created_ev.payload["direction_id"],
            Value::String(direction.id.clone()),
            "the carried-over task's event must carry the SOURCE task's own Direction"
        );
        // The new row itself also inherited it (CarryOverTask's own apply,
        // not just the event payload).
        let new_task_direction: Option<String> =
            sqlx::query_scalar("SELECT direction_id FROM sin90_tasks WHERE id = ?")
                .bind(&new_task_id)
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(new_task_direction.as_deref(), Some(direction.id.as_str()));
    }

    /// M5d (2026-09-24 review): `model = None` (T5.1.2's ONLY production
    /// path today) end to end with genuinely NOTHING to do — no P, W already
    /// empty, no rhythm allocation at all — produces `Nothing` for all three
    /// kinds, submits ZERO proposals, and the ladder itself (not
    /// `propose_decide`'s own manual `record_call`, which only fires on a
    /// `Produced` decision — unreachable here since there is no model step
    /// at all) writes exactly ONE `sin90_ai_calls` row for the whole
    /// decision, `error_kind = "no_match"` (`ReflexFallback`'s own
    /// bookkeeping in `ai::ladder::run_item`).
    #[tokio::test]
    async fn propose_nothing_to_do_produces_no_proposals_model_none() {
        let store = Sin90Store::open_memory().await.unwrap();
        let target = store.create_week("2026-W27").await.unwrap();
        let reader = store.ai_reader();
        let model: Option<&NoModelPort> = None;

        let items = run_propose(
            "run-nothing-to-do",
            &target,
            &ProposeDedup::default(),
            ModelAccess::LocalOnly,
            model,
            &store,
            &reader,
        )
        .await;
        for item in &items {
            assert_eq!(
                item.result,
                ProposeItemResult::Nothing,
                "{:?}: expected Nothing",
                item.kind
            );
        }

        let proposals: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_proposals")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(proposals, 0);

        // L3 (2026-09-24 review round 3): reflex ALWAYS has an answer for
        // propose — "nothing needs to change" is a completed decision, not a
        // failed rule lookup, so this records `ok=1` (consistent with the
        // model path's own "decisive non-match is still ok=1" posture),
        // `proposal_id = NULL` (no proposal was produced by it), NOT
        // `ok=0, error_kind="no_match"`.
        let calls: Vec<(bool, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT ok, error_kind, proposal_id FROM sin90_ai_calls WHERE run_id = 'run-nothing-to-do'",
        )
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(
            calls[0].0,
            "reflex reaching a real (empty) decision must be ok=1"
        );
        assert_eq!(calls[0].1, None);
        assert_eq!(calls[0].2, None);
    }

    /// M-1(b) (2026-09-24 review round 3): `ProposeDedup { skip_create: true,
    /// .. }` makes the CREATE kind `Skipped` even though the model DID
    /// return non-empty `new_tasks` — the dedup flag pre-empts submission
    /// entirely, no `CreateTasks` proposal is written, regardless of what
    /// the decision would otherwise have proposed. Carry/reorder (dedup NOT
    /// set for them) proceed normally, proving this is a per-kind override,
    /// not a run-wide one. Mutation target: in `propose_decide`, change
    /// `dedup.skip_create` to `false` unconditionally — this test's
    /// `Skipped` assertion goes red (and a `CreateTasks` proposal appears).
    #[tokio::test]
    async fn propose_decide_skip_create_flag_suppresses_submission() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (_direction, _prev, target, _prev_ids, _w_ids) = seed(&store).await;
        let reader = store.ai_reader();

        let model_text = r#"{"carry":["p1"],"order":["w2","w1"],"new_tasks":[{"title":"new gap task","direction":"g1"}],"reason":"catching up"}"#;
        let model = StubModel::always(Ok(reply(model_text)));
        let dedup = ProposeDedup {
            skip_create: true,
            ..ProposeDedup::default()
        };
        let items = run_propose(
            "run-skip-create",
            &target,
            &dedup,
            ModelAccess::LocalOnly,
            Some(&model),
            &store,
            &reader,
        )
        .await;

        let create_item = items
            .iter()
            .find(|i| i.kind == ProposeKind::Create)
            .unwrap();
        assert_eq!(create_item.result, ProposeItemResult::Skipped);
        // Carry/reorder are NOT dedup-flagged — they proceed normally,
        // proving `skip_create` is a per-kind override, not a run-wide one.
        let carry_item = items.iter().find(|i| i.kind == ProposeKind::Carry).unwrap();
        assert!(matches!(carry_item.result, ProposeItemResult::Proposed(_)));

        // (2026-09-24 review round 4: a `sin90_tasks WHERE title = 'new gap
        // task'` check was here and has been REMOVED — it was vacuously
        // true regardless of whether `skip_create` worked, since this test
        // never calls `apply_proposal` at all; `CreateTasks` only ever
        // writes to `sin90_tasks` on ACCEPT, never on `submit`. The
        // `create_proposals` check below, against `sin90_proposals`
        // directly, is the one that actually carries signal.)
        // Scoped to THIS decision's own title, not `ops LIKE '%create_tasks
        // %'` generically — `seed()` itself already submitted two
        // `CreateTasks` proposals of its own (for P's and W's fixture
        // tasks), which would otherwise make this assertion vacuous.
        let create_proposals: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sin90_proposals WHERE ops LIKE '%new gap task%'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(create_proposals, 0);
    }

    /// A `ModelPort` that panics if ever called — proves `run_propose`'s L-4
    /// short-circuit genuinely never reaches the ladder (same convention
    /// `classify::tests::PanicModel` uses for its own R1 short-circuit
    /// proof).
    struct PanicModel;
    impl ModelPort for PanicModel {
        async fn complete(&self, _req: ModelRequest) -> Result<ModelReply, ModelFailure> {
            panic!("run_propose: the model must never be called once L-4's early return fires")
        }
    }

    /// L-4 (2026-09-24 review round 3): when carry has NO candidates (empty
    /// P) AND reorder/create are both already dedup-skipped, `run_propose`
    /// must short-circuit BEFORE even building the ladder — no model call
    /// (`PanicModel` proves it), and no `sin90_ai_calls` row at all for this
    /// run (not even the `ok=1`, empty-decision row L-3 gives the NORMAL
    /// "reflex found nothing" case — this is a cheaper, distinct "we didn't
    /// even try" outcome). Mutation target: delete the early-return `if`
    /// block in `run_propose` — this test's `PanicModel` then actually gets
    /// called and the test panics instead of asserting cleanly.
    #[tokio::test]
    async fn propose_short_circuits_when_everything_is_already_covered() {
        let store = Sin90Store::open_memory().await.unwrap();
        // No P at all (no previous open week) — carry has zero candidates
        // regardless of dedup.
        let target = store.create_week("2026-W44").await.unwrap();
        let reader = store.ai_reader();
        let dedup = ProposeDedup {
            skip_reorder: true,
            skip_create: true,
            ..ProposeDedup::default()
        };

        let items = run_propose(
            "run-short-circuit",
            &target,
            &dedup,
            ModelAccess::LocalOnly,
            Some(&PanicModel),
            &store,
            &reader,
        )
        .await;

        let by_kind = |k: ProposeKind| items.iter().find(|i| i.kind == k).unwrap().result.clone();
        assert_eq!(by_kind(ProposeKind::Carry), ProposeItemResult::Nothing);
        assert_eq!(by_kind(ProposeKind::Reorder), ProposeItemResult::Skipped);
        assert_eq!(by_kind(ProposeKind::Create), ProposeItemResult::Skipped);

        let calls: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sin90_ai_calls WHERE run_id = 'run-short-circuit'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(calls, 0, "the short-circuit must not write ANY call row");
    }

    /// M-a (2026-09-24 review round 4): L-4's short-circuit must NEVER fire
    /// while carry still has candidates, even if `skip_reorder` AND
    /// `skip_create` are BOTH true — mutating the guard from `reflex_carry.
    /// is_empty() && dedup.skip_reorder && dedup.skip_create` down to just
    /// `dedup.skip_reorder && dedup.skip_create` (dropping the carry check
    /// entirely) left every pre-existing test green, because none of them
    /// exercised "carry has something to do AND reorder/create are both
    /// dedup-flagged" at once. `seed()`'s P has a genuinely carryable task,
    /// so `model = None`'s reflex path must still produce a real Carry
    /// proposal and a real call row — proving the ladder actually ran.
    #[tokio::test]
    async fn propose_never_short_circuits_while_carry_has_candidates() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (_direction, _prev, target, _prev_ids, _w_ids) = seed(&store).await;
        let reader = store.ai_reader();
        let model: Option<&NoModelPort> = None;
        let dedup = ProposeDedup {
            skip_reorder: true,
            skip_create: true,
            ..ProposeDedup::default()
        };

        let items = run_propose(
            "run-carry-not-short-circuited",
            &target,
            &dedup,
            ModelAccess::LocalOnly,
            model,
            &store,
            &reader,
        )
        .await;

        let carry_item = items.iter().find(|i| i.kind == ProposeKind::Carry).unwrap();
        assert!(
            matches!(carry_item.result, ProposeItemResult::Proposed(_)),
            "{:?}",
            carry_item.result
        );

        let calls: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sin90_ai_calls WHERE run_id = 'run-carry-not-short-circuited'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert!(
            calls >= 1,
            "the ladder must have actually run (at least one call row) when carry has candidates"
        );
    }

    /// J20: a stub model choosing carry/reorder/create all at once produces
    /// (up to) three PENDING proposals, every one of which passes the
    /// submit-time dry run; each accepts cleanly; `CreateTasks`'/
    /// `CarryOverTask`'s `task.created` payload carries `direction_id`
    /// (§2 #25).
    #[tokio::test]
    async fn propose_proposals_submit_and_apply() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (direction, _prev, target, prev_task_ids, w_task_ids) = seed(&store).await;
        let reader = store.ai_reader();

        let model_text = r#"{"carry":["p1"],"order":["w2","w1"],"new_tasks":[{"title":"new gap task","direction":"g1"}],"reason":"catching up"}"#;
        let model = StubModel::always(Ok(reply(model_text)));
        let items = run_propose(
            "run-propose-1",
            &target,
            &ProposeDedup::default(),
            ModelAccess::LocalOnly,
            Some(&model),
            &store,
            &reader,
        )
        .await;
        assert_eq!(items.len(), 3);
        // ONE model call produced all three sub-proposals (§11.4.3: a single
        // combined `{carry, order, new_tasks, reason}` reply, not one call
        // per sub-proposal).
        assert_eq!(model.calls(), 1);

        let mut proposal_ids = Vec::new();
        for item in &items {
            match &item.result {
                ProposeItemResult::Proposed(id) => proposal_ids.push(id.clone()),
                other => panic!("{:?}: expected Proposed, got {other:?}", item.kind),
            }
        }
        assert_eq!(proposal_ids.len(), 3, "carry + reorder + create all fired");

        for id in &proposal_ids {
            let stored = store.get_proposal(id).await.unwrap();
            assert_eq!(stored.status, ProposalStatus::Pending);
            store.apply_proposal(id).await.unwrap();
        }

        // Carry: the FIRST prev task (p1) is now carried_over, with a NEW
        // task in the target week.
        let carried_status: String =
            sqlx::query_scalar("SELECT status FROM sin90_tasks WHERE id = ?")
                .bind(&prev_task_ids[0])
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(carried_status, "carried_over");

        // Reorder: w2 now sorts before w1. Restricted to the two ORIGINAL W
        // tasks (not the whole week) — accepting carry BEFORE reorder gives
        // the newly carried-in task `sort_key = 0` too (§11.9 R9's known,
        // accepted quirk: carry's apply always mints `sort_key = 0`), which
        // would otherwise tie with whichever original task the reorder
        // proposal also placed first and make the raw `week_id` query's
        // relative order among all three rows implementation-defined.
        let ordered: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM sin90_tasks WHERE week_id = ? AND id IN (?, ?) ORDER BY sort_key ASC",
        )
        .bind(&target.id)
        .bind(&w_task_ids[0])
        .bind(&w_task_ids[1])
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert_eq!(ordered[0], w_task_ids[1]);
        assert_eq!(ordered[1], w_task_ids[0]);

        // Create: a new task under the gap Direction, `task.created`
        // carries `direction_id` (§2 #25).
        let created: (String, Option<String>) = sqlx::query_as(
            "SELECT title, direction_id FROM sin90_tasks WHERE title = 'new gap task'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(created.0, "new gap task");
        assert_eq!(created.1.as_deref(), Some(direction.id.as_str()));

        let events = store
            .list_events(Some("task"), None, None, None)
            .await
            .unwrap();
        let new_task_id: String =
            sqlx::query_scalar("SELECT id FROM sin90_tasks WHERE title = 'new gap task'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        let ev = events
            .iter()
            .find(|e| e.entity_id == new_task_id && e.kind == "created")
            .expect("a task.created event for the new task must exist");
        assert_eq!(
            ev.payload["direction_id"],
            Value::String(direction.id.clone())
        );

        // The carried-over task's OWN `task.created` event also carries
        // `direction_id` (here `null` — the seed tasks were never classified).
        let carried_new_id: String =
            sqlx::query_scalar("SELECT id FROM sin90_tasks WHERE carried_from = ?")
                .bind(&prev_task_ids[0])
                .fetch_one(store.pool())
                .await
                .unwrap();
        let carried_ev = events
            .iter()
            .find(|e| e.entity_id == carried_new_id && e.kind == "created")
            .expect("a task.created event for the carried-over task must exist");
        assert!(carried_ev.payload.get("direction_id").is_some());
    }

    /// A `ModelPort` that sleeps briefly before returning a fixed reply — for
    /// L-1's own "the first row keeps a REAL (non-zero) `latency_ms`"
    /// assertion below: `StubModel`'s instant, lock-and-clone `complete`
    /// measures as `0ms` on a fast machine (this WAS observed while writing
    /// this test — plain `StubModel` made the assertion fail every time, not
    /// flakily), which cannot be told apart from `submit_one`'s OWN
    /// intentional zeroing of a sibling row's `latency_ms`. A few
    /// milliseconds of real wall-clock delay makes the measured value
    /// reliably positive without slowing the rest of the suite (only this
    /// one test uses it).
    struct DelayedReplyModel(String);
    impl ModelPort for DelayedReplyModel {
        fn complete(
            &self,
            _req: ModelRequest,
        ) -> impl Future<Output = Result<ModelReply, ModelFailure>> + Send {
            let text = self.0.clone();
            async move {
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                Ok(ModelReply {
                    text,
                    model_id: Some("test-model".into()),
                    tier: crate::ai::ports::ServedTier::Local,
                    prompt_tokens: Some(100),
                    completion_tokens: Some(50),
                })
            }
        }
    }

    /// M1 (2026-09-24 review): ONE model call's token usage must not be
    /// double- (or triple-) counted across the (up to three) `sin90_ai_calls`
    /// rows one decision can produce — only the FIRST row (carry, since it is
    /// attempted first) keeps `prompt_tokens`/`completion_tokens`/
    /// `latency_ms`; the reorder and create rows (siblings of the SAME
    /// decision) get `NULL`/`NULL`/`0`. `SUM(prompt_tokens)` across the
    /// run's `ok=1` rows must equal the stub's real usage exactly, not a
    /// multiple of it. Mutation target: remove the `is_first` zeroing in
    /// `submit_one` and the `SUM` assertion goes red (300 instead of 100).
    #[tokio::test]
    async fn propose_multi_proposal_tokens_not_double_counted() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (_direction, _prev, target, _prev_ids, _w_ids) = seed(&store).await;
        let reader = store.ai_reader();

        let model_text = r#"{"carry":["p1"],"order":["w2","w1"],"new_tasks":[{"title":"new gap task","direction":"g1"}],"reason":"catching up"}"#;
        let model = DelayedReplyModel(model_text.to_string());
        let items = run_propose(
            "run-propose-tokens",
            &target,
            &ProposeDedup::default(),
            ModelAccess::LocalOnly,
            Some(&model),
            &store,
            &reader,
        )
        .await;
        assert_eq!(
            items
                .iter()
                .filter(|i| matches!(i.result, ProposeItemResult::Proposed(_)))
                .count(),
            3,
            "carry + reorder + create must all have fired for this assertion to be meaningful"
        );
        let carry_proposal_id = match &items
            .iter()
            .find(|i| i.kind == ProposeKind::Carry)
            .unwrap()
            .result
        {
            ProposeItemResult::Proposed(id) => id.clone(),
            other => panic!("expected Proposed, got {other:?}"),
        };

        let (sum_prompt, sum_completion): (i64, i64) = sqlx::query_as(
            "SELECT COALESCE(SUM(prompt_tokens), 0), COALESCE(SUM(completion_tokens), 0) \
             FROM sin90_ai_calls WHERE run_id = 'run-propose-tokens' AND ok = 1",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(
            sum_prompt, 100,
            "the model's usage must be counted ONCE across all 3 rows"
        );
        assert_eq!(sum_completion, 50);

        // Exactly one `ok=1` row keeps the non-null usage — and L-1 (2026-
        // 09-24 review round 3): that row's `proposal_id` must be the CARRY
        // proposal's (carry is attempted first, per §11.4.3's own
        // carry/reorder/create priority order), and it is the ONLY row with
        // `latency_ms > 0` (the siblings are zeroed, not just their token
        // counts).
        let usage_row: (Option<String>, i64) = sqlx::query_as(
            "SELECT proposal_id, latency_ms FROM sin90_ai_calls \
             WHERE run_id = 'run-propose-tokens' AND ok = 1 AND prompt_tokens IS NOT NULL",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(usage_row.0.as_deref(), Some(carry_proposal_id.as_str()));
        assert!(
            usage_row.1 > 0,
            "the first row's own latency must be real, not zeroed"
        );

        let positive_latency_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sin90_ai_calls \
             WHERE run_id = 'run-propose-tokens' AND ok = 1 AND latency_ms > 0",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(
            positive_latency_rows, 1,
            "exactly one row (the first-attempted one) keeps a real latency"
        );
    }

    /// M-3 (2026-09-24 review round 3): a wrapping `AiSink` that rejects ONLY
    /// the carry sub-proposal — proving the "partial rejection" scenario the
    /// M1 doc comments describe (`this_rec` in `submit_one`'s `Err` branch)
    /// is not just theoretical. Carry ends up `ok = 0, error_kind =
    /// "rejected_by_precheck"` yet STILL keeps `prompt_tokens` (it is
    /// attempted FIRST, per priority order, regardless of outcome);
    /// reorder/create succeed normally with `ok = 1` and `NULL` tokens
    /// (siblings). `SUM(prompt_tokens)` across the WHOLE run (not filtered
    /// to `ok = 1` — the usage-carrying row here is `ok = 0`) still equals
    /// the model's real usage exactly.
    struct RejectCarrySink<'a> {
        inner: &'a Sin90Store,
    }
    impl AiSink for RejectCarrySink<'_> {
        async fn submit(
            &self,
            cap: Capability,
            draft: ProposalDraft,
            rec: AiCallRecord,
        ) -> Result<(), SinkError> {
            let is_carry_batch = !draft.ops.is_empty()
                && draft
                    .ops
                    .iter()
                    .all(|op| matches!(op, Sin90Op::CarryOverTask { .. }));
            if is_carry_batch {
                return Err(SinkError::Invalid("simulated carry rejection".into()));
            }
            AiSink::submit(self.inner, cap, draft, rec).await
        }
        async fn record_call(&self, rec: AiCallRecord) -> Result<(), SinkError> {
            AiSink::record_call(self.inner, rec).await
        }
        async fn precheck(&self, cap: Capability, drafts: &[ProposalDraft]) -> Vec<bool> {
            AiSink::precheck(self.inner, cap, drafts).await
        }
    }

    /// Mutation targets (BOTH must independently turn this red): (1) remove
    /// the `sink.record_call(this_rec).await` call in `submit_one`'s `Err`
    /// branch — the carry row disappears entirely (`calls.len()` drops to
    /// 2, and the `SUM` assertion sees only reorder/create's `NULL` tokens,
    /// going to 0); (2) delete `this_rec.ok = false;` in that same branch —
    /// the carry row would keep whatever `ok` `rec` already had (`true`,
    /// since `run_item` sets it before returning `Produced`), so the
    /// `ok = 0` assertion on the carry row goes red.
    #[tokio::test]
    async fn propose_partial_rejection_carry_rejected_reorder_create_succeed() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (_direction, _prev, target, _prev_ids, _w_ids) = seed(&store).await;
        let reader = store.ai_reader();

        let model_text = r#"{"carry":["p1"],"order":["w2","w1"],"new_tasks":[{"title":"new gap task","direction":"g1"}],"reason":"catching up"}"#;
        let model = StubModel::always(Ok(ModelReply {
            text: model_text.to_string(),
            model_id: Some("test-model".into()),
            tier: crate::ai::ports::ServedTier::Local,
            prompt_tokens: Some(100),
            completion_tokens: Some(50),
        }));
        let sink = RejectCarrySink { inner: &store };
        let items = run_propose(
            "run-partial-reject",
            &target,
            &ProposeDedup::default(),
            ModelAccess::LocalOnly,
            Some(&model),
            &sink,
            &reader,
        )
        .await;

        let by_kind = |k: ProposeKind| items.iter().find(|i| i.kind == k).unwrap();
        assert!(matches!(
            by_kind(ProposeKind::Carry).result,
            ProposeItemResult::Rejected
        ));
        assert!(matches!(
            by_kind(ProposeKind::Reorder).result,
            ProposeItemResult::Proposed(_)
        ));
        assert!(matches!(
            by_kind(ProposeKind::Create).result,
            ProposeItemResult::Proposed(_)
        ));

        let rows: Vec<(bool, Option<String>, Option<i64>)> = sqlx::query_as(
            "SELECT ok, error_kind, prompt_tokens FROM sin90_ai_calls \
             WHERE run_id = 'run-partial-reject' ORDER BY at ASC, rowid ASC",
        )
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert_eq!(rows.len(), 3, "{rows:?}");
        // Carry (first, rejected): ok=0, error_kind=rejected_by_precheck,
        // KEEPS its usage (it was still the first ATTEMPT, regardless of
        // outcome).
        assert!(!rows[0].0);
        assert_eq!(rows[0].1.as_deref(), Some("rejected_by_precheck"));
        assert_eq!(rows[0].2, Some(100));
        // Reorder/create (siblings): ok=1, NULL tokens.
        for row in &rows[1..] {
            assert!(row.0, "{row:?}");
            assert_eq!(row.2, None, "{row:?}");
        }

        let sum_prompt: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(prompt_tokens), 0) FROM sin90_ai_calls WHERE run_id = 'run-partial-reject'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(
            sum_prompt, 100,
            "the model's usage must be counted ONCE across the whole run, even though the row \
             carrying it was rejected"
        );
    }

    /// M5a (2026-09-24 review): when the decided `order` is IDENTICAL to W's
    /// current `sort_key` order, NO `ReorderTasks` proposal is produced at
    /// all (§11.4.3: "与当前 sort_key 顺序相同 → 不产") — this is exercised
    /// here directly (the model explicitly echoes the CURRENT order back),
    /// not just implicitly via a scenario that happens to differ
    /// (`propose_proposals_submit_and_apply` above always produces a
    /// DIFFERENT order). Mutation target: change `propose_decide`'s
    /// `if value.order == current_order { Vec::new() } else { ... }` to
    /// unconditionally build the `ReorderTasks` op and this goes red (a
    /// proposal would be submitted for a no-op reorder).
    #[tokio::test]
    async fn propose_no_reorder_proposal_when_order_already_matches() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (_direction, _prev, target, _prev_ids, _w_ids) = seed(&store).await;
        let reader = store.ai_reader();
        // `seed` itself submits + applies a few proposals of its own — the
        // assertion below cares about the DELTA this run produces, not the
        // absolute count.
        let proposals_before: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_proposals")
            .fetch_one(store.pool())
            .await
            .unwrap();

        // "w1", "w2" is ALREADY W's current sort_key order (`seed`'s own
        // `CreateTasks` assigns sort_key 0/1 in that order) — echoing it
        // back verbatim means nothing needs to change.
        let model = StubModel::always(Ok(reply(
            r#"{"carry":[],"order":["w1","w2"],"new_tasks":[],"reason":"already in order"}"#,
        )));
        let items = run_propose(
            "run-no-reorder",
            &target,
            &ProposeDedup::default(),
            ModelAccess::LocalOnly,
            Some(&model),
            &store,
            &reader,
        )
        .await;
        let reorder_item = items
            .iter()
            .find(|i| i.kind == ProposeKind::Reorder)
            .unwrap();
        assert_eq!(reorder_item.result, ProposeItemResult::Nothing);

        let proposals_after: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_proposals")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(
            proposals_before, proposals_after,
            "no reorder (or anything else) must have been proposed"
        );
    }

    /// M3 (2026-09-24 review): the OLD version of this test used a stub
    /// model whose `carry` was ALWAYS `[]` regardless of scenario — so it
    /// would have stayed green even if `previous_open_week`'s open-status
    /// gate were completely broken (P being `reviewing` was never actually
    /// exercised as a DIFFERENCE). Rewritten against `model = None`
    /// (T5.1.2's production path — reflex only): P `active` (open) makes
    /// `carry_reflex` pick up its planned/in_progress tasks and a carry
    /// proposal IS produced; the SAME week, after transitioning P to
    /// `reviewing`, produces NOTHING for carry (`previous_open_week` no
    /// longer finds a P at all). Mutation target: delete the `week_is_open`
    /// filter in `AiReader::previous_open_week` and the SECOND assertion
    /// goes red (a carry proposal would be produced again).
    #[tokio::test]
    async fn propose_carry_requires_previous_week_open() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (_direction, prev, target, _prev_ids, _w_ids) = seed(&store).await;
        let reader = store.ai_reader();
        let model: Option<&NoModelPort> = None;

        // Positive control: P is `active` (open).
        let items = run_propose(
            "run-carry-open",
            &target,
            &ProposeDedup::default(),
            ModelAccess::LocalOnly,
            model,
            &store,
            &reader,
        )
        .await;
        let carry_item = items.iter().find(|i| i.kind == ProposeKind::Carry).unwrap();
        assert!(
            matches!(carry_item.result, ProposeItemResult::Proposed(_)),
            "{:?}",
            carry_item.result
        );

        // P is now `reviewing` — no longer open.
        store
            .transition_week(&prev.id, WeekStatus::Reviewing)
            .await
            .unwrap();
        let items2 = run_propose(
            "run-carry-closed",
            &target,
            &ProposeDedup::default(),
            ModelAccess::LocalOnly,
            model,
            &store,
            &reader,
        )
        .await;
        let carry_item2 = items2
            .iter()
            .find(|i| i.kind == ProposeKind::Carry)
            .unwrap();
        assert_eq!(carry_item2.result, ProposeItemResult::Nothing);
    }

    /// M5b (2026-09-24 review): a `backlog` task in P must never be offered
    /// to the MODEL as a `p*` carry candidate either — `p_candidates` is now
    /// derived FROM `carry_reflex`'s own output (not a second,
    /// independently-filtered condition that could silently drift), so this
    /// doubles as a regression for that refactor. Uses a `CapturingModel` to
    /// inspect the actual request payload, not just the reflex's own return
    /// value (`carry_reflex_takes_only_planned_and_in_progress` already
    /// covers that in isolation). Mutation target: change `p_candidates`'s
    /// construction back to a `matches!(t.status, Planned | InProgress)`
    /// filter applied directly to `prev_tasks` (bypassing `carry_reflex`'s
    /// own output) and this test still passes UNLESS that filter is also
    /// loosened — so the REAL regression case is checked by temporarily
    /// widening `carry_reflex`'s own status match to also include `Backlog`
    /// and confirming BOTH this test and `carry_reflex_takes_only_planned_
    /// and_in_progress` go red together (single source of truth).
    #[tokio::test]
    async fn propose_backlog_prev_task_never_offered_as_carry_candidate() {
        let store = Sin90Store::open_memory().await.unwrap();
        let prev = store.create_week("2026-W23").await.unwrap();
        store
            .transition_week(&prev.id, WeekStatus::Active)
            .await
            .unwrap();
        // A plain `backlog` task — never put into a week via `CreateTasks`/
        // `CarryOverTask` (both mint `planned`) — created directly and then
        // moved into P's week_id via raw SQL, same convention this file's
        // own `gap_directions` tests use for state with no production
        // one-step write path.
        let backlog_task = store
            .create_task(
                "backlog leftover",
                None,
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();
        sqlx::query("UPDATE sin90_tasks SET week_id = ? WHERE id = ?")
            .bind(&prev.id)
            .bind(&backlog_task.id)
            .execute(store.pool())
            .await
            .unwrap();
        // A genuinely carryable planned task, so `p_candidates` is non-empty
        // and this test can't pass vacuously.
        let planned_create = Sin90Proposal {
            id: "seed-planned".into(),
            status: ProposalStatus::Pending,
            source: ProposalSource::Rule,
            ops: vec![Sin90Op::CreateTasks {
                week_id: prev.id.clone(),
                tasks: vec![NewTask {
                    title: "planned leftover".into(),
                    direction_id: None,
                }],
            }],
            rationale: None,
        };
        store.submit_proposal(&planned_create).await.unwrap();
        store.apply_proposal(&planned_create.id).await.unwrap();

        let target = store.create_week("2026-W24").await.unwrap();
        let reader = store.ai_reader();
        let model = CapturingModel::new(
            r#"{"carry":[],"order":[],"new_tasks":[],"reason":"looked, nothing to add"}"#,
        );
        let _items = run_propose(
            "run-backlog-candidate",
            &target,
            &ProposeDedup::default(),
            ModelAccess::LocalOnly,
            Some(&model),
            &store,
            &reader,
        )
        .await;

        let sent = model.last_user_message();
        assert!(
            sent.contains("planned leftover"),
            "the genuinely carryable task must be offered: {sent}"
        );
        assert!(
            !sent.contains("backlog leftover"),
            "a backlog prev-week task must NEVER be offered as a carry candidate: {sent}"
        );
    }

    /// §11.4.3's "缺口 Direction": a rhythm-allocated (`pct > 0`) Direction
    /// that ALREADY has a task in W is not a gap — only Directions with NO
    /// task in W qualify, even though their quota is non-zero.
    #[tokio::test]
    async fn gap_directions_excludes_directions_with_an_existing_week_task() {
        let store = Sin90Store::open_memory().await.unwrap();
        let direction = store
            .create_direction("Work", "2026-Q4", None)
            .await
            .unwrap();
        test_hooks::insert_rhythm(&store, "rhythm-1").await.unwrap();
        sqlx::query("UPDATE sin90_rhythms SET allocations = ? WHERE id = ?")
            .bind(
                serde_json::to_string(&vec![Alloc {
                    direction_id: direction.id.clone(),
                    pct: 100,
                }])
                .unwrap(),
            )
            .bind("rhythm-1")
            .execute(store.pool())
            .await
            .unwrap();
        let reader = store.ai_reader();

        let week = store.create_week("2026-W15").await.unwrap();
        let task_under_work = store
            .create_task(
                "already under Work",
                Some(&direction.id),
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();
        // Put it INTO the week directly via raw SQL (no production route
        // creates an already-classified, in-week task in one step; the
        // `CreateTasks`/`AssignTaskDirection` split is a two-step process
        // elsewhere in this file's own `seed`, which is unnecessary
        // ceremony for this one column).
        sqlx::query("UPDATE sin90_tasks SET week_id = ?, status = 'planned' WHERE id = ?")
            .bind(&week.id)
            .bind(&task_under_work.id)
            .execute(store.pool())
            .await
            .unwrap();

        let alloc = reader.rhythm_alloc().await.unwrap();
        let week_tasks_w = reader.week_tasks(&week.id).await.unwrap();
        assert!(gap_directions(&reader, &alloc, &week_tasks_w)
            .await
            .unwrap()
            .is_empty());

        // Positive control: an OTHERWISE identical week (no task under
        // "Work") DOES surface it as a gap.
        let empty_week = store.create_week("2026-W16").await.unwrap();
        let empty_week_tasks = reader.week_tasks(&empty_week.id).await.unwrap();
        let gaps = gap_directions(&reader, &alloc, &empty_week_tasks)
            .await
            .unwrap();
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].direction_id, direction.id);
    }

    /// L2 (2026-09-24 review): design's own wording is "W 里没有任何任务" —
    /// a TERMINAL task (here `done`) under the Direction still counts as
    /// "there IS a task", so the Direction is NOT a gap, even though
    /// `w_candidates` (built from the NON-terminal task list) never sees it.
    /// Mutation target: pass the non-terminal-only list to `gap_directions`
    /// instead of the full one and this goes red (the Direction would
    /// wrongly surface as a gap).
    #[tokio::test]
    async fn gap_directions_excludes_directions_with_only_a_terminal_week_task() {
        let store = Sin90Store::open_memory().await.unwrap();
        let direction = store
            .create_direction("Work", "2026-Q4", None)
            .await
            .unwrap();
        test_hooks::insert_rhythm(&store, "rhythm-2").await.unwrap();
        sqlx::query("UPDATE sin90_rhythms SET allocations = ? WHERE id = ?")
            .bind(
                serde_json::to_string(&vec![Alloc {
                    direction_id: direction.id.clone(),
                    pct: 100,
                }])
                .unwrap(),
            )
            .bind("rhythm-2")
            .execute(store.pool())
            .await
            .unwrap();
        let reader = store.ai_reader();

        let week = store.create_week("2026-W17").await.unwrap();
        let done_task = store
            .create_task(
                "already done under Work",
                Some(&direction.id),
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();
        sqlx::query("UPDATE sin90_tasks SET week_id = ?, status = 'done' WHERE id = ?")
            .bind(&week.id)
            .bind(&done_task.id)
            .execute(store.pool())
            .await
            .unwrap();

        let alloc = reader.rhythm_alloc().await.unwrap();
        // `week_tasks` itself returns EVERY task in the week regardless of
        // status (only `run_propose`'s own non-terminal filter narrows it
        // for `w_candidates`) — this test passes it straight through, as
        // `run_propose` does for `gap_directions` specifically (L2).
        let all_week_tasks = reader.week_tasks(&week.id).await.unwrap();
        assert!(gap_directions(&reader, &alloc, &all_week_tasks)
            .await
            .unwrap()
            .is_empty());
    }

    /// J21 (2026-09-24 review M4 — a REAL full-table diff, not just a few
    /// hand-picked row counts): a `ReorderTasks` referencing `[w2,
    /// does-not-exist]` — a real task from W plus one that plainly does not
    /// exist — is rejected by `AiSink::submit`, and the WHOLE database is
    /// left BYTE-FOR-BYTE unchanged (`test_hooks::snapshot_all_tables`/
    /// `diff_snapshot_keys`, moved here from `store::ai_port`'s own J8 test
    /// so this test reuses the SAME implementation, not a second copy) —
    /// not one table, not even `sin90_ai_calls`/`sin90_proposals` alone.
    /// Positive control: the SAME shape with every task really in W changes
    /// EXACTLY `{sin90_ai_calls, sin90_events(entity=proposal),
    /// sin90_proposals}` — `sin90_tasks` (the dry run's OWN `ReorderTasks`
    /// apply) is untouched either way. Exercised directly against `AiSink`
    /// (not through the model/ladder) so the assertion is about the SINK's
    /// guarantee, independent of how a draft was produced —
    /// `propose_decide`'s own model-driven path is covered by
    /// `propose_proposals_submit_and_apply` above.
    #[tokio::test]
    async fn propose_invalid_rejected_at_submit() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (_direction, _prev, target, _prev_ids, w_task_ids) = seed(&store).await;
        let pool = store.pool().clone();

        let before = crate::store::test_hooks::snapshot_all_tables(&pool).await;
        let bad_draft = ProposalDraft {
            id: "bad-reorder".into(),
            ops: vec![Sin90Op::ReorderTasks {
                week_id: target.id.clone(),
                order: vec![w_task_ids[1].clone(), "does-not-exist".into()],
            }],
            rationale: None,
        };
        let rec = AiCallRecord {
            id: "call-bad".into(),
            run_id: "run-j21".into(),
            task_kind: Capability::Propose,
            engine: Engine::Reflex,
            fallback_from: None,
            served_tier: None,
            model_id: None,
            prompt_tokens: None,
            completion_tokens: None,
            latency_ms: 0,
            ok: true,
            error_kind: None,
            proposal_id: None,
            at: "2026-09-24T00:00:00Z".into(),
        };
        let err = AiSink::submit(&store, Capability::Propose, bad_draft, rec)
            .await
            .unwrap_err();
        assert!(matches!(err, SinkError::Invalid(_)));

        let after = crate::store::test_hooks::snapshot_all_tables(&pool).await;
        assert_eq!(
            crate::store::test_hooks::diff_snapshot_keys(&before, &after),
            Vec::<String>::new(),
            "a rejected submit must leave the WHOLE database byte-for-byte unchanged"
        );

        // Positive control: the SAME shape, but every task really is in W —
        // submit succeeds and changes EXACTLY the three allowed tables;
        // `sin90_tasks` (the op's OWN target) is untouched — `submit`'s dry
        // run never actually applies (§11.4 公共 H3).
        let before2 = after;
        let good_draft = ProposalDraft {
            id: "good-reorder".into(),
            ops: vec![Sin90Op::ReorderTasks {
                week_id: target.id.clone(),
                order: vec![w_task_ids[1].clone(), w_task_ids[0].clone()],
            }],
            rationale: None,
        };
        let rec2 = AiCallRecord {
            id: "call-good".into(),
            run_id: "run-j21".into(),
            task_kind: Capability::Propose,
            engine: Engine::Reflex,
            fallback_from: None,
            served_tier: None,
            model_id: None,
            prompt_tokens: None,
            completion_tokens: None,
            latency_ms: 0,
            ok: true,
            error_kind: None,
            proposal_id: None,
            at: "2026-09-24T00:00:00Z".into(),
        };
        AiSink::submit(&store, Capability::Propose, good_draft, rec2)
            .await
            .unwrap();
        let after2 = crate::store::test_hooks::snapshot_all_tables(&pool).await;
        let mut changed = crate::store::test_hooks::diff_snapshot_keys(&before2, &after2);
        changed.sort();
        assert_eq!(
            changed,
            vec![
                "sin90_ai_calls".to_string(),
                "sin90_events(entity=proposal)".to_string(),
                "sin90_proposals".to_string(),
            ],
            "submit's dry-run apply must leave sin90_tasks (and everything else) untouched"
        );
        assert_eq!(
            before2.get("sin90_tasks"),
            after2.get("sin90_tasks"),
            "the ReorderTasks op inside the submitted proposal must not have actually applied"
        );
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM sin90_proposals WHERE id = 'good-reorder'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(n, 1);
    }

    /// J8 (T5.4.1, originally added 2026-09-24 review at `store::ai_port`'s
    /// own test module — relocated here in the T5.4.1 layer split, since it
    /// exercises `run_propose` directly and so belongs at this layer, not
    /// the read-model layer; behavior and assertions are unchanged, only
    /// `snapshot_all_tables`/`diff_keys`'s local-alias calls became
    /// fully-qualified `crate::store::test_hooks::` calls and the
    /// `crate::ai::propose::`-qualified paths became unqualified, since both
    /// are already in scope here). The SAME judgement as
    /// `propose_invalid_rejected_at_submit` above, but through a full
    /// `run_propose` run (`model = None`, T5.1.2's production path — reflex
    /// only) instead of calling `AiSink::submit` directly. A previous OPEN
    /// week with one `planned` task makes the carry reflex deterministically
    /// produce a proposal; the submit's dry run (which actually calls
    /// `apply_op` under a SAVEPOINT before rolling it back, §11.4 公共 H3)
    /// must still leave `sin90_tasks` byte-for-byte unchanged. Mutation
    /// target: make `dry_run` commit instead of rolling back its
    /// `SAVEPOINT`, and `sin90_tasks` changes.
    #[tokio::test]
    async fn ai_boundary_tables_unchanged_via_full_propose_run() {
        let store = Sin90Store::open_memory().await.unwrap();
        let prev = store.create_week("2026-W28").await.unwrap();
        store
            .transition_week(&prev.id, crate::core::WeekStatus::Active)
            .await
            .unwrap();
        let seed = Sin90Proposal {
            id: "seed-prev-task".into(),
            status: ProposalStatus::Pending,
            source: crate::core::ProposalSource::Rule,
            ops: vec![Sin90Op::CreateTasks {
                week_id: prev.id.clone(),
                tasks: vec![crate::core::NewTask {
                    title: "carry me".into(),
                    direction_id: None,
                }],
            }],
            rationale: None,
        };
        store.submit_proposal(&seed).await.unwrap();
        store.apply_proposal(&seed.id).await.unwrap();
        let target = store.create_week("2026-W29").await.unwrap();
        let reader = store.ai_reader();
        let pool = store.pool().clone();

        let before = crate::store::test_hooks::snapshot_all_tables(&pool).await;
        let model: Option<&crate::ai::NoModelPort> = None;
        let items = run_propose(
            "run-j8-propose-full",
            &target,
            &ProposeDedup::default(),
            crate::ai::ModelAccess::LocalOnly,
            model,
            &store,
            &reader,
        )
        .await;
        let carry = items.iter().find(|i| i.kind == ProposeKind::Carry).unwrap();
        assert!(
            matches!(carry.result, ProposeItemResult::Proposed(_)),
            "{:?}",
            carry.result
        );

        let after = crate::store::test_hooks::snapshot_all_tables(&pool).await;
        let mut changed = crate::store::test_hooks::diff_snapshot_keys(&before, &after);
        changed.sort();
        assert_eq!(
            changed,
            vec![
                "sin90_ai_calls".to_string(),
                "sin90_events(entity=proposal)".to_string(),
                "sin90_proposals".to_string(),
            ],
            "a full propose run's submit must leave sin90_tasks untouched"
        );
        assert_eq!(
            before.get("sin90_tasks"),
            after.get("sin90_tasks"),
            "the produced CarryOverTask's dry-run apply must not have actually applied"
        );
    }

    /// J21's second clause: carrying an already-carried task (colliding with
    /// `idx_sin90_task_carried`'s uniqueness) is ALSO rejected at submit,
    /// leaving the store unchanged.
    #[tokio::test]
    async fn propose_double_carry_rejected_at_submit() {
        let store = Sin90Store::open_memory().await.unwrap();
        let (_direction, _prev, target, prev_task_ids, _w_ids) = seed(&store).await;

        let first = ProposalDraft {
            id: "carry-first".into(),
            ops: vec![Sin90Op::CarryOverTask {
                task_id: prev_task_ids[0].clone(),
                to_week: target.id.clone(),
            }],
            rationale: None,
        };
        let rec = |id: &str| AiCallRecord {
            id: id.into(),
            run_id: "run-double-carry".into(),
            task_kind: Capability::Propose,
            engine: Engine::Reflex,
            fallback_from: None,
            served_tier: None,
            model_id: None,
            prompt_tokens: None,
            completion_tokens: None,
            latency_ms: 0,
            ok: true,
            error_kind: None,
            proposal_id: None,
            at: "2026-09-24T00:00:00Z".into(),
        };
        AiSink::submit(&store, Capability::Propose, first, rec("call-1"))
            .await
            .unwrap();
        store.apply_proposal("carry-first").await.unwrap();

        let before: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_proposals")
            .fetch_one(store.pool())
            .await
            .unwrap();
        // L3 (2026-09-24 review): snapshot call rows/events too, not just
        // the proposals count — a rejected `submit` must write NOTHING at
        // all (§11.4 公共 H3: "整个事务回滚，不写任何行"), including no
        // `call-2` row (a rejected attempt is the CALLER's job to record via
        // `record_call`, not `submit`'s — `AiSink::submit` alone never
        // writes a row for its own failure).
        let calls_before: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_ai_calls")
            .fetch_one(store.pool())
            .await
            .unwrap();
        let events_before: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_events")
            .fetch_one(store.pool())
            .await
            .unwrap();
        let second = ProposalDraft {
            id: "carry-second".into(),
            ops: vec![Sin90Op::CarryOverTask {
                task_id: prev_task_ids[0].clone(),
                to_week: target.id.clone(),
            }],
            rationale: None,
        };
        let err = AiSink::submit(&store, Capability::Propose, second, rec("call-2"))
            .await
            .unwrap_err();
        assert!(matches!(err, SinkError::Invalid(_)));
        let after: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_proposals")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(
            before, after,
            "the double-carry must not have been submitted"
        );
        let calls_after: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_ai_calls")
            .fetch_one(store.pool())
            .await
            .unwrap();
        let events_after: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_events")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(
            calls_before, calls_after,
            "a rejected submit must not have written a call-2 row"
        );
        assert_eq!(
            events_before, events_after,
            "a rejected submit must not have written any event"
        );
        let call2_exists: i64 =
            sqlx::query_scalar("SELECT count(*) FROM sin90_ai_calls WHERE id = 'call-2'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(call2_exists, 0);
    }

    /// J22 (propose half): a draft carrying `AssignTaskDirection` (classify's
    /// op, not one of propose's three) is rejected by `allowed_ops` before
    /// any write-lock work — chosen because `AssignTaskDirection` is
    /// otherwise perfectly valid (an inbox task, an active Direction), so
    /// `allowed_ops` is the ONLY thing standing between this draft and
    /// success (same reasoning `ai_port.rs`'s own
    /// `allowed_ops_classify_rejects_other_op_kinds` documents).
    #[tokio::test]
    async fn propose_allowed_ops_rejects_classify_op() {
        let store = Sin90Store::open_memory().await.unwrap();
        let direction = store
            .create_direction("Work", "2026-Q4", None)
            .await
            .unwrap();
        let task = store
            .create_task("inbox task", None, None, TaskKind::Other, Energy::Mid, None)
            .await
            .unwrap();
        let draft = ProposalDraft {
            id: "p-wrong-cap".into(),
            ops: vec![Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: direction.id.clone(),
            }],
            rationale: None,
        };
        let rec = AiCallRecord {
            id: "call-wrong-cap".into(),
            run_id: "run-x".into(),
            task_kind: Capability::Propose,
            engine: Engine::Reflex,
            fallback_from: None,
            served_tier: None,
            model_id: None,
            prompt_tokens: None,
            completion_tokens: None,
            latency_ms: 0,
            ok: true,
            error_kind: None,
            proposal_id: None,
            at: "2026-09-24T00:00:00Z".into(),
        };
        let err = AiSink::submit(&store, Capability::Propose, draft, rec)
            .await
            .unwrap_err();
        assert!(matches!(err, SinkError::Invalid(_)));
        let still_inbox: Option<String> =
            sqlx::query_scalar("SELECT direction_id FROM sin90_tasks WHERE id = ?")
                .bind(&task.id)
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(
            still_inbox, None,
            "the rejected op must not have been applied"
        );
    }

    /// §11.4.3's "W 必须 open": a `closed`/`reviewing` week is rejected with
    /// 409-shaped input BEFORE any run starts; `select_week` is what the
    /// HTTP handler calls synchronously (mirrors `classify::select_targets`'
    /// role).
    #[tokio::test]
    async fn propose_select_week_rejects_non_open_and_unknown() {
        let store = Sin90Store::open_memory().await.unwrap();
        let reader = store.ai_reader();
        let week = store.create_week("2026-W20").await.unwrap();
        store
            .transition_week(&week.id, WeekStatus::Active)
            .await
            .unwrap();
        store
            .transition_week(&week.id, WeekStatus::Reviewing)
            .await
            .unwrap();

        assert_eq!(
            select_week(&reader, &week.id).await,
            Err(ProposeInputError::WeekNotOpen(
                week.id.clone(),
                WeekStatus::Reviewing
            ))
        );
        assert_eq!(
            select_week(&reader, &"ghost-week".to_string()).await,
            Err(ProposeInputError::UnknownWeek("ghost-week".to_string()))
        );

        // Positive control: `planning` and `active` both pass.
        let w2 = store.create_week("2026-W21").await.unwrap();
        assert_eq!(select_week(&reader, &w2.id).await.unwrap().id, w2.id);
        store
            .transition_week(&w2.id, WeekStatus::Active)
            .await
            .unwrap();
        assert_eq!(select_week(&reader, &w2.id).await.unwrap().id, w2.id);
    }

    /// `AiReadModel::previous_open_week`/`rhythm_alloc` against the REAL
    /// store: chronological (not insertion) order, and "no non-retired
    /// rhythm" -> empty allocation.
    #[tokio::test]
    async fn read_model_previous_open_week_and_rhythm_alloc() {
        let store = Sin90Store::open_memory().await.unwrap();
        let reader = store.ai_reader();
        assert_eq!(reader.rhythm_alloc().await.unwrap(), Vec::<Alloc>::new());

        // Out of creation order on purpose: W12 created AFTER W11 — a naive
        // "most recently CREATED" query would get this backwards.
        let w11 = store.create_week("2026-W11").await.unwrap();
        store
            .transition_week(&w11.id, WeekStatus::Active)
            .await
            .unwrap();
        let _w09_closed = {
            let w = store.create_week("2026-W09").await.unwrap();
            store
                .transition_week(&w.id, WeekStatus::Active)
                .await
                .unwrap();
            store
                .transition_week(&w.id, WeekStatus::Reviewing)
                .await
                .unwrap();
            store
                .transition_week(&w.id, WeekStatus::Closed)
                .await
                .unwrap();
            w
        };
        let w13 = store.create_week("2026-W13").await.unwrap();

        // From W13's perspective, the nearest OPEN week strictly before it
        // is W11 (W09 is closed, and W12 doesn't exist).
        let p = reader.previous_open_week(&w13.iso_week).await.unwrap();
        assert_eq!(p.map(|w| w.id), Some(w11.id.clone()));

        // From W11's perspective, there is no open week before it (W09 is closed).
        assert_eq!(
            reader.previous_open_week(&w11.iso_week).await.unwrap(),
            None
        );

        test_hooks::insert_rhythm(&store, "r1").await.unwrap();
        sqlx::query("UPDATE sin90_rhythms SET allocations = ?, status = 'retired' WHERE id = ?")
            .bind(r#"[{"direction_id":"d-old","pct":50}]"#)
            .bind("r1")
            .execute(store.pool())
            .await
            .unwrap();
        // A retired rhythm's allocation must NOT be returned.
        assert_eq!(reader.rhythm_alloc().await.unwrap(), Vec::<Alloc>::new());

        test_hooks::insert_rhythm(&store, "r2").await.unwrap();
        sqlx::query("UPDATE sin90_rhythms SET allocations = ? WHERE id = ?")
            .bind(r#"[{"direction_id":"d-new","pct":70}]"#)
            .bind("r2")
            .execute(store.pool())
            .await
            .unwrap();
        assert_eq!(
            reader.rhythm_alloc().await.unwrap(),
            vec![Alloc {
                direction_id: "d-new".into(),
                pct: 70
            }]
        );
    }

    /// H1 (2026-09-24 review): `previous_open_week` must NOT skip past a
    /// CLOSER closed week to find an OLDER still-open one — §11.4.3's "P" is
    /// the single NEAREST week before W, and P only exists if THAT nearest
    /// week is itself open. W05 (active, i.e. open) is further back than W12
    /// (closed); from W13's perspective the nearest week is W12, which is
    /// closed, so `previous_open_week` must answer `None` even though an
    /// older open week (W05) exists.
    #[tokio::test]
    async fn previous_open_week_does_not_skip_past_a_closer_closed_week() {
        let store = Sin90Store::open_memory().await.unwrap();
        let reader = store.ai_reader();

        let w05 = store.create_week("2026-W05").await.unwrap();
        store
            .transition_week(&w05.id, WeekStatus::Active)
            .await
            .unwrap();
        let w12 = store.create_week("2026-W12").await.unwrap();
        store
            .transition_week(&w12.id, WeekStatus::Active)
            .await
            .unwrap();
        store
            .transition_week(&w12.id, WeekStatus::Reviewing)
            .await
            .unwrap();
        store
            .transition_week(&w12.id, WeekStatus::Closed)
            .await
            .unwrap();
        let w13 = store.create_week("2026-W13").await.unwrap();

        assert_eq!(
            reader.previous_open_week(&w13.iso_week).await.unwrap(),
            None,
            "the NEAREST week (W12) is closed — must not fall through to the older-but-open W05"
        );

        // Positive control: querying from W12's OWN perspective (nearest
        // before it is W05, which IS open) correctly finds W05.
        assert_eq!(
            reader
                .previous_open_week(&w12.iso_week)
                .await
                .unwrap()
                .map(|w| w.id),
            Some(w05.id)
        );
    }
}
