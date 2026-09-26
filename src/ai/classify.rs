//! `classify` (T5.2.1, design §11.4.1): inbox tasks → `AssignTaskDirection`
//! proposals. reflex rule R1 (decisive, over classification history) →
//! model (executive/local) → reflex rule R2 (fallback, title/candidate word
//! overlap) — the ladder itself (`plan`/`run_item`) is `super::ladder`; this
//! file is only the classify-specific pieces `run_item` is generic over:
//! `normalize_title` (R1's key), the two reflex rules, the model
//! request/schema/parse, and the per-item/per-run driver that turns a
//! [`Outcome`] into either a submitted proposal or a plain call record.
//!
//! Depends on nothing but `crate::core` and `crate::ai::*` (§11.5) — same
//! boundary every other file under `src/ai/` keeps; the one exception is this
//! file's own `#[cfg(test)]` module, which — per the boundary checker's own
//! carve-out (`tests/ai_boundary.rs`'s "v2.1 M4") — builds fixtures against a
//! real `crate::store::Sin90Store` so these tests exercise the actual
//! `AiSink`/`AiReadModel` implementation, not a second hand-rolled fake of it.

use std::collections::HashSet;

use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::core::{DirectionId, Sin90Op, Task, TaskId, TRIAGE_DIRECTION_ID};

use super::ladder::{plan, run_item, Outcome, RunState, Step};
use super::ports::{
    AiCallRecord, AiReadModel, AiSink, Capability, Complexity, DirectionCandidate, Engine,
    ModelAccess, ModelMessage, ModelPort, ModelReply, ModelRequest, ProposalDraft, Role, SinkError,
};

// ---------------------------------------------------------------- normalize

/// R1's key (§11.4.1): trim, collapse internal whitespace, lowercase. This is
/// the ONE canonical implementation — `store::ai_port::AiReadModel::title_history`
/// calls this directly (store is allowed to depend on `ai`; only the reverse
/// is forbidden, §11.5) rather than keeping its own approximation, so a title
/// is normalized exactly the same way on both sides of that read.
#[must_use]
pub fn normalize_title(title: &str) -> String {
    title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

// ---------------------------------------------------------------- candidates

/// A [`DirectionCandidate`] wrapped with the opaque key (`"d1"..."dn"`) the
/// model is shown instead of a real [`DirectionId`] (§11.4.1: "候选以不透明
/// 短键呈现，模型看不到、也就造不出 ULID").
#[derive(Debug, Clone)]
pub struct KeyedCandidate {
    pub key: String,
    pub direction_id: DirectionId,
    pub title: String,
    pub area_title: Option<String>,
}

/// Assigns `d1..dn` in the order `candidates` was already sorted in
/// (`AiReadModel::direction_candidates`'s `updated_at DESC`, §11.4.1).
#[must_use]
pub fn candidate_keys(candidates: &[DirectionCandidate]) -> Vec<KeyedCandidate> {
    candidates
        .iter()
        .enumerate()
        .map(|(i, c)| KeyedCandidate {
            key: format!("d{}", i + 1),
            direction_id: c.direction_id.clone(),
            title: c.title.clone(),
            area_title: c.area_title.clone(),
        })
        .collect()
}

// ---------------------------------------------------------------- reflex R1

/// R1 (§11.4.1, decisive): among the historically classified tasks with the
/// SAME normalized title (already filtered to non-terminal Directions by
/// `AiReadModel::title_history`), do they all point at the SAME Direction?
/// `None` = "undecided" (no history, or a split history) — NOT a "decisively
/// no match"; `run_item`'s `ReflexDecisive` step records that as `error_kind
/// = "undecided"`, `ok = 0` (§11.3.5 L5), not a failure.
#[must_use]
pub fn r1_reflex(history: &[DirectionId]) -> Option<DirectionId> {
    let mut it = history.iter();
    let first = it.next()?;
    if it.all(|d| d == first) {
        Some(first.clone())
    } else {
        None
    }
}

// ---------------------------------------------------------------- reflex R2

/// R2 (§11.4.1, fallback): task title vs. each candidate's `title + area`,
/// scored by ASCII words (length ≥ 3, case-folded) and CJK bigrams (adjacent
/// CJK code points) in common. A candidate "qualifies" only with ≥ 1 matching
/// word OR ≥ 2 matching bigrams (⚖️ design's own threshold); among qualifying
/// candidates the UNIQUE highest scorer wins — a tie, or no qualifying
/// candidate, is `None` ("no_match", `ok = 0`, same posture as R1's
/// undecided but recorded under a different `error_kind`, per `run_item`'s
/// `ReflexFallback` step).
#[must_use]
pub fn r2_reflex(title: &str, candidates: &[KeyedCandidate]) -> Option<DirectionId> {
    let (task_words, task_bigrams) = tokenize(title);
    if task_words.is_empty() && task_bigrams.is_empty() {
        return None;
    }
    // 2026-09-24 review (L1): design order is "find the unique highest score
    // FIRST, THEN check whether it clears the threshold" — NOT "only
    // candidates that individually clear the threshold compete for
    // highest". Scoring every candidate with ANY overlap (not just ones that
    // individually qualify) matters for cases like `"fix 学习"` against
    // `{"fix things", "学习计划"}`: both score 1 (one ASCII word hit, one CJK
    // bigram hit respectively) and neither is a `≥2` bigram match on its
    // own, but excluding the CJK candidate from the race (the OLD algorithm)
    // let `"fix things"` "win" as the only contender — the correct answer is
    // `no_match`, because the two are tied once compared on equal footing.
    let mut best_total = 0usize;
    let mut best: Option<(usize, usize, &KeyedCandidate)> = None; // (word_hits, bigram_hits, candidate)
    let mut tie = false;
    for c in candidates {
        let combined = match &c.area_title {
            Some(a) => format!("{} {}", c.title, a),
            None => c.title.clone(),
        };
        let (cwords, cbigrams) = tokenize(&combined);
        let word_hits = task_words.intersection(&cwords).count();
        let bigram_hits = task_bigrams.intersection(&cbigrams).count();
        let total = word_hits + bigram_hits;
        if total == 0 {
            continue; // nothing in common at all — never a contender
        }
        match total.cmp(&best_total) {
            std::cmp::Ordering::Greater => {
                best_total = total;
                best = Some((word_hits, bigram_hits, c));
                tie = false;
            }
            std::cmp::Ordering::Equal => {
                if best.is_some() {
                    tie = true;
                }
            }
            std::cmp::Ordering::Less => {}
        }
    }
    if tie {
        return None;
    }
    let (word_hits, bigram_hits, winner) = best?;
    // NOW apply the qualifying threshold — only to the (unique) winner.
    if word_hits >= 1 || bigram_hits >= 2 {
        Some(winner.direction_id.clone())
    } else {
        None
    }
}

/// A run qualifies as a "word" (§11.4.1's R2) only with length ≥ 3 AND at
/// least one ASCII letter (2026-09-24 review, L2) — a bare digit run like
/// `"2026"` is a year/id fragment, not a meaningful word to match on.
fn finalize_word(cur: &mut String, words: &mut HashSet<String>) {
    if cur.chars().count() >= 3 && cur.chars().any(|c| c.is_ascii_alphabetic()) {
        words.insert(std::mem::take(cur));
    } else {
        cur.clear();
    }
}

fn tokenize(s: &str) -> (HashSet<String>, HashSet<String>) {
    let mut words = HashSet::new();
    let mut bigrams = HashSet::new();
    let chars: Vec<char> = s.chars().collect();
    let mut cur = String::new();
    for i in 0..chars.len() {
        let c = chars[i];
        if c.is_ascii_alphanumeric() {
            cur.push(c.to_ascii_lowercase());
            continue;
        }
        finalize_word(&mut cur, &mut words);
        if is_cjk(c) && i + 1 < chars.len() && is_cjk(chars[i + 1]) {
            let mut bg = String::new();
            bg.push(c);
            bg.push(chars[i + 1]);
            bigrams.insert(bg);
        }
    }
    finalize_word(&mut cur, &mut words);
    (words, bigrams)
}

/// 2026-09-24 review (L3): extended past the BMP-only ranges to cover CJK
/// Extension B and beyond, plus the Compatibility Ideographs blocks — real
/// titles occasionally use rare/compat characters (personal/place names,
/// older text) that live outside Extension A.
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF     // CJK Unified Ideographs
        | 0x3400..=0x4DBF   // CJK Extension A
        | 0x3040..=0x30FF   // Hiragana + Katakana
        | 0xAC00..=0xD7A3   // Hangul syllables
        | 0xF900..=0xFAFF   // CJK Compatibility Ideographs
        | 0x20000..=0x2FA1F // CJK Extension B through F + Compatibility Supplement
    )
}

// ---------------------------------------------------------------- model step

const CLASSIFY_SYSTEM_PROMPT: &str = "You are sorting one task into the best-fitting Direction \
from a short candidate list. Pick exactly one candidate key, or \"none\" if nothing fits well. \
Respond with JSON only, matching the given schema.";

const MAX_REASON_CHARS: usize = 200;

/// One decided classification, from ANY step of the ladder (reflex or
/// model): `direction_id: None` means "this step decisively found no match"
/// (§11.4.1: model `none`/low confidence, or R1/R2 simply never producing
/// this shape at all — reflex always sets `Some`, see [`r1_reflex`]/
/// [`r2_reflex`]'s docs). `reason` is always populated so the caller can
/// build a rationale regardless of which step produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifyDecision {
    pub direction_id: Option<DirectionId>,
    pub reason: String,
    /// T5.2.3 (用户拍板 Q8「没把握就不要提」): `true` only when THIS `None`
    /// came from the model's self-reported `confidence` falling below
    /// [`CLASSIFY_CONFIDENCE_THRESHOLD`] — NOT when the model explicitly
    /// chose `"none"` (that is a decisive non-match on its own terms, not a
    /// confidence problem), and never `true` for a reflex decision (R1/R2
    /// always produce `Some`, see their own docs). `classify_one` uses this
    /// to tag the run item's `reason` as `"low_confidence"` specifically,
    /// distinct from every other cause of a bare `nothing`.
    pub low_confidence: bool,
    /// T5.2.3 review (M2): the model's raw self-reported `confidence`,
    /// carried only so `classify_one` can `tracing::info!` it (not persisted
    /// anywhere yet — see that log site's own doc). `None` for a
    /// reflex-produced decision (R1/R2 have no confidence concept).
    pub confidence: Option<&'static str>,
    /// T5.2.3 review (M2): the model's raw `choice` string (a candidate key
    /// or `"none"`), same purpose as `confidence` above. `None` for reflex.
    pub choice: Option<String>,
}

/// `response_format`'s JSON schema (§11.4.1): `choice` is a closed enum over
/// this run's candidate keys plus `"none"`; `additionalProperties: false`.
#[must_use]
pub fn classify_schema(candidates: &[KeyedCandidate]) -> Map<String, Value> {
    let mut choices: Vec<Value> = candidates
        .iter()
        .map(|c| Value::String(c.key.clone()))
        .collect();
    choices.push(Value::String("none".into()));
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "choice": {"type": "string", "enum": choices},
            "confidence": {"type": "string", "enum": ["low", "medium", "high"]},
            "reason": {"type": "string", "maxLength": MAX_REASON_CHARS}
        },
        "required": ["choice", "confidence", "reason"]
    });
    match schema {
        Value::Object(m) => m,
        _ => unreachable!("json!({{...}}) always builds a Value::Object"),
    }
}

#[must_use]
pub fn build_classify_request(
    task: &Task,
    candidates: &[KeyedCandidate],
    engine: Engine,
) -> ModelRequest {
    let schema = classify_schema(candidates);
    let candidates_json: Vec<Value> = candidates
        .iter()
        .map(|c| json!({"key": c.key, "title": c.title, "area": c.area_title}))
        .collect();
    let user = json!({
        "item": {"title": task.title},
        "candidates": candidates_json,
    })
    .to_string();
    ModelRequest {
        messages: vec![
            ModelMessage {
                role: Role::System,
                content: CLASSIFY_SYSTEM_PROMPT.to_string(),
            },
            ModelMessage {
                role: Role::User,
                content: user,
            },
        ],
        schema_name: "sin90_classify",
        schema,
        max_tokens: 256,
        complexity: if engine == Engine::Executive {
            Complexity::Complex
        } else {
            Complexity::Simple
        },
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelChoice {
    choice: String,
    confidence: String,
    reason: String,
}

/// Ordinal reading of the model's self-reported `confidence` (§11.4.1's
/// schema `confidence: enum[low, medium, high]`) — deriving `Ord` turns the
/// threshold check into a plain integer compare instead of a hand-rolled
/// string match, and makes "below the threshold" well-defined regardless of
/// which of the three levels the threshold itself sits at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Confidence {
    Low,
    Medium,
    High,
}

impl Confidence {
    /// T5.2.3 review (L1, was backwards): `None` for anything outside the
    /// three known levels — THIS function is what makes the caller
    /// (`parse_classify_reply`) reject that case as `bad_output`; nothing
    /// upstream has already filtered it out before this runs.
    fn parse(s: &str) -> Option<Self> {
        match s {
            "low" => Some(Confidence::Low),
            "medium" => Some(Confidence::Medium),
            "high" => Some(Confidence::High),
            _ => None,
        }
    }

    /// T5.2.3 review (M2): the inverse of [`Self::parse`], used only so
    /// `classify_one` can log the raw level string alongside `run_id`/
    /// `task_id`/`choice` (see that log site's own doc) without keeping a
    /// second copy of the original `String` around.
    fn as_str(self) -> &'static str {
        match self {
            Confidence::Low => "low",
            Confidence::Medium => "medium",
            Confidence::High => "high",
        }
    }
}

/// T5.2.3 (用户拍板 Q8「没把握就不要提」): the model step's self-reported
/// `confidence` must be AT LEAST this ordinal level for its `choice` to
/// become a proposal — strictly below it is a decisive non-match instead
/// (`nothing`, `ok = 1` — the model did its job, the call is recorded, only
/// the proposal is withheld; `AiRunItem.reason = "low_confidence"`, see
/// `classify_one`).
///
/// 🟡 占位默认值 (T5.2.3): kept as a named constant, not wired into
/// `sin90_settings`/`/settings/ai` yet — doing so would mean extending
/// `AiSettings`, `read_ai_settings`, the `PUT /settings/ai` body/route and
/// their own tests for a value nobody can tune sensibly today (see the note
/// below); that plumbing is exactly what `ai.executive_enabled` already
/// looks like, so the shape to copy is known whenever T5.5.1 supplies a real
/// number and a human-facing default is worth exposing.
///
/// `Confidence::Medium` — i.e. reject only `"low"` — is the CONSERVATIVE
/// placeholder called for here, not a tuned value: design §11.4.1's own Q8
/// note says small local models self-report confidence with poor
/// calibration (`low`/`medium`/`high` may not track real hit rate at all),
/// and the real threshold should come from T5.5.1's smoke-test hit rate per
/// level (J26), not a prior guess. It also happens to match this ladder's
/// behavior before this constant existed (`choice == "none" || confidence ==
/// "low"` was the old check) — no behavior regression, just a named,
/// reassignable threshold in place of an inline string comparison.
const CLASSIFY_CONFIDENCE_THRESHOLD: Confidence = Confidence::Medium;

/// Tolerates ONE layer of a ```` ```json ```` (or bare ```` ``` ````) fence
/// (§11.4.1) — models very commonly wrap JSON output in one even when told
/// not to.
fn strip_json_fence(s: &str) -> &str {
    let t = s.trim();
    for prefix in ["```json", "```"] {
        if let Some(rest) = t.strip_prefix(prefix) {
            return rest.strip_suffix("```").unwrap_or(rest).trim();
        }
    }
    t
}

/// The program's recheck of the model's reply (§11.4.1): malformed JSON, an
/// unknown field, an out-of-range `confidence`, an over-length `reason`, or a
/// `choice` that names neither `"none"` nor one of THIS run's candidate keys
/// (an invented key, or a real ULID the model tried to guess/copy) all map to
/// the SAME `Err("bad_output")` — `run_item` records that as a degrade, not a
/// crash. `choice == "none"` or `confidence` below
/// [`CLASSIFY_CONFIDENCE_THRESHOLD`] (T5.2.3) is a decisive non-match (`Ok`
/// with `direction_id: None`) — the model looked and either said "no" or
/// wasn't sure enough, which is a successful call, not a failure (§11.4.1's
/// "记 ok = 1"). The two causes are kept distinguishable via
/// `ClassifyDecision::low_confidence` even though both produce `None` here.
pub fn parse_classify_reply(
    text: &str,
    candidates: &[KeyedCandidate],
) -> Result<ClassifyDecision, &'static str> {
    let parsed: ModelChoice =
        serde_json::from_str(strip_json_fence(text)).map_err(|_| "bad_output")?;
    let Some(confidence) = Confidence::parse(&parsed.confidence) else {
        return Err("bad_output");
    };
    if parsed.reason.chars().count() > MAX_REASON_CHARS {
        return Err("bad_output");
    }
    if parsed.choice != "none" && !candidates.iter().any(|c| c.key == parsed.choice) {
        return Err("bad_output");
    }
    if parsed.choice == "none" {
        // Coordinator review (H1, 2026-09-26 round 2): this used to hardcode
        // `low_confidence: false` regardless of `confidence` — a model that
        // says "none" AND reports `low` confidence about that "none" is
        // EXACTLY the case Q8 exists for ("没把握就不要提"), so it must be
        // tagged `low_confidence` the same way a low-confidence real pick
        // is. T5.2.2's 待定 fallback is scoped to "the model had medium+
        // confidence about its `none`" — anything below threshold stays a
        // bare `nothing` (never reaches the fallback), same threshold the
        // real-key branch below already applies.
        return Ok(ClassifyDecision {
            direction_id: None,
            reason: parsed.reason,
            low_confidence: confidence < CLASSIFY_CONFIDENCE_THRESHOLD,
            confidence: Some(confidence.as_str()),
            choice: Some(parsed.choice.clone()),
        });
    }
    // T5.2.3: checked AFTER the invented-key check above — a bogus key paired
    // with low confidence must still be `bad_output`, not quietly downgraded
    // to a low-confidence `nothing`.
    if confidence < CLASSIFY_CONFIDENCE_THRESHOLD {
        return Ok(ClassifyDecision {
            direction_id: None,
            reason: parsed.reason,
            low_confidence: true,
            confidence: Some(confidence.as_str()),
            choice: Some(parsed.choice.clone()),
        });
    }
    // `.expect`: the membership check above already proved this key exists.
    let direction_id = candidates
        .iter()
        .find(|c| c.key == parsed.choice)
        .expect("choice was already checked against candidates")
        .direction_id
        .clone();
    Ok(ClassifyDecision {
        direction_id: Some(direction_id),
        reason: parsed.reason,
        low_confidence: false,
        confidence: Some(confidence.as_str()),
        choice: Some(parsed.choice.clone()),
    })
}

// ---------------------------------------------------------------- rationale

const MAX_RATIONALE_CHARS: usize = 280;

/// §11.4 公共's "提议形状": `rationale = "<engine>: <理由>"`, control
/// characters stripped, truncated to 280 chars.
fn build_rationale(engine: Engine, reason: &str) -> String {
    let cleaned: String = reason
        .chars()
        .filter(|c| !c.is_control() && !is_cf_format_char(*c))
        .collect();
    let s = format!("{}: {}", engine.as_str(), cleaned);
    if s.chars().count() > MAX_RATIONALE_CHARS {
        s.chars().take(MAX_RATIONALE_CHARS).collect()
    } else {
        s
    }
}

/// 2026-09-24 review (round 2, low): a hand-picked subset of Unicode
/// category Cf ("format") code points worth stripping from AI-produced text
/// a human will read (`rationale`) — bidi overrides (U+202A-U+202E,
/// U+2066-U+2069) can make a rationale string DISPLAY differently from its
/// actual byte content, and zero-width characters (U+200B-U+200F, U+2060-
/// U+2064, U+FEFF, the Arabic Letter Mark U+061C, soft hyphen U+00AD) can
/// hide content or split words invisibly. NOT the full Unicode Cf category
/// (that table is `ai::summarize`'s deliverable, T5.3.1, per design
/// §11.4.2's `normalize`) — just the characters relevant to a plain-text
/// rationale string with no markup semantics of its own.
///
/// `pub` (2026-09-24 review, T5.4.1 H2 — plain `pub`, not `pub(crate)`:
/// `pub(crate)`'s own `Path` is just `crate` with no further segment, which
/// trips the J7 boundary checker's "a `crate`-rooted path must go through
/// `core` or `ai`" rule — same convention `normalize_title`/`ItemResult`/
/// every other cross-module-reused item in this file already uses): `ai::
/// propose` reuses this SAME function for its own `rationale` and new-task
/// `title` cleaning rather than keeping a second, driftable copy — a fix to
/// this table (e.g. widening the bidi-override range) then applies to every
/// capability that strips Cf characters from AI-produced plain text at once.
pub fn is_cf_format_char(c: char) -> bool {
    matches!(c as u32,
        0x00AD              // soft hyphen
        | 0x061C            // Arabic letter mark
        | 0x200B..=0x200F   // zero-width space/ZWNJ/ZWJ/LRM/RLM
        | 0x202A..=0x202E   // LRE/RLE/PDF/LRO/RLO (bidi overrides)
        | 0x2060..=0x2064   // word joiner and friends
        | 0x2066..=0x2069   // LRI/RLI/FSI/PDI (bidi isolates)
        | 0xFEFF // BOM / zero-width no-break space
    )
}

// ---------------------------------------------------------------- run driver

/// Outcome of one item after the whole ladder ran, as this driver reports it
/// (§11.4 公共's `items: [{target, result, reason?}]` shape — the HTTP layer
/// maps this to that wire vocabulary; `reason` comes from [`ClassifyItem`],
/// not this enum, see T5.2.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemResult {
    /// A proposal was submitted; the id is `sin90_proposals.id`.
    Proposed(String),
    /// Every step ran (or none applied) without a decisive match.
    Nothing,
    /// Capacity/budget/deadline hit — item left for a later run.
    Deferred,
    /// A produced decision failed `AiSink::submit`'s dry run (state moved out
    /// from under it between planning and submit — e.g. the task was
    /// classified by something else in the same run's window). Distinct from
    /// `Nothing`: a decision WAS reached, it just could not be committed.
    Rejected,
    /// The run aborted (§11.3.4) before or during this item.
    Aborted,
}

#[derive(Debug, Clone)]
pub struct ClassifyItem {
    pub task_id: TaskId,
    pub result: ItemResult,
    /// T5.2.3: why `result` is `Nothing`, when that needs to be
    /// distinguishable on the wire (design §11.4 公共's `items[].reason`).
    /// Currently only ever `Some("low_confidence")`; every other cause of a
    /// bare `Nothing` (no candidates, R1/R2 undecided, model said `"none"`,
    /// a `bad_output` degrade with nothing left to fall back to, …) leaves
    /// this `None`, same as before this field existed.
    pub reason: Option<&'static str>,
}

/// T5.2.2 (design §2 #30): shared by every classify arm that ends in
/// `AssignTaskDirection` — a REAL match (the `Some(direction_id)` arm) or
/// the 待定 fallback (a decisive non-match, or the ladder producing nothing
/// at all) — so the "decision failed submit's dry run, state moved between
/// planning and submit" handling (§11.3.5) exists exactly once, not once
/// per call site. `rec` must already be the `ok=1` row this decision is
/// linked to; this function only clones it for the failure path.
/// Third element (M-b, T5.7.2 review round 2 follow-up): `true` only when
/// the failure was `SinkError::Store` — an infra hiccup that says NOTHING
/// about whether the model/rule actually looked at this task, as opposed to
/// `SinkError::Invalid` ("rejected_by_precheck": state moved, but a real
/// decision WAS reached). `classify_one`'s H2 eval-write gate below reads
/// this to decide whether a 待定-parked task's re-evaluation is genuine.
async fn submit_direction_assignment<S: AiSink>(
    task_id: &TaskId,
    direction_id: DirectionId,
    rationale: String,
    rec: AiCallRecord,
    sink: &S,
) -> (ItemResult, Option<&'static str>, bool) {
    let draft = ProposalDraft {
        id: format!("ai-classify-{}", crate::core::ulid()),
        ops: vec![Sin90Op::AssignTaskDirection {
            task_id: task_id.clone(),
            direction_id,
        }],
        rationale: Some(rationale),
    };
    let draft_id = draft.id.clone();
    // 2026-09-24 review (M1): a decision that fails `submit`'s dry run (state
    // moved between planning and submit — e.g. the task got classified by
    // something else in the same run's window) is not a dropped call — it
    // still gets a row, via `record_call`, since `submit` never wrote one.
    // `rec` is cloned BEFORE `submit` consumes it so there is something left
    // to record on the error path (§11.3.5: the model/rule did its job; the
    // STATE changed, not the call).
    let rec_on_failure = rec.clone();
    match sink.submit(Capability::Classify, draft, rec).await {
        Ok(()) => (ItemResult::Proposed(draft_id), None, false),
        Err(e) => {
            tracing::warn!(error = %e, task_id = %task_id, "classify: a decision failed submit's dry run (state moved)");
            let is_store_err = matches!(e, SinkError::Store(_));
            let mut failed = rec_on_failure;
            failed.ok = false;
            failed.proposal_id = None;
            failed.error_kind = Some(match &e {
                // §11.3.5's own name for this case.
                SinkError::Invalid(_) => "rejected_by_precheck",
                // An infra failure says nothing about the decision's
                // validity — a distinct kind so it is never confused with a
                // real precheck rejection.
                SinkError::Store(_) => "submit_store_error",
            });
            if let Err(record_err) = sink.record_call(failed).await {
                tracing::warn!(error = %record_err, task_id = %task_id, "classify: failed to record a rejected-at-submit call (R6, not fatal)");
            }
            (ItemResult::Rejected, None, is_store_err)
        }
    }
}

/// One task through the full classify ladder (§11.4.1). T5.2.2 (design §2
/// #30, Q4, narrowed by the coordinator's 2026-09-26 round-2 review):
/// "没有合适的 Direction 时" means the task is assigned to the reserved 待定
/// Direction ONLY when a MODEL step actually ran, replied, and decisively
/// said `"none"` at `medium`-or-above confidence (the `None if
/// !low_confidence` arm below) — every other "nothing decided" shape stays a
/// bare `Nothing`, retried on the NEXT trigger, never a proposal:
/// - candidates empty from the start (no non-terminal Direction exists at
///   all — a brand-new user) → `reason = "undetermined"` (M1).
/// - the whole ladder ran and NOT ONE step produced anything at all
///   (`Outcome::Nothing`: R1 undecided, R2 no_match, no model reachable or
///   every model step degraded away — offline, circuit open, standalone
///   with no model configured) → also `reason = "undetermined"` (H3). This
///   is deliberately NOT the same as a model explicitly answering "none":
///   here nothing ever looked at the task with enough information to decide
///   anything, so treating it as a confident non-match would overclaim.
/// - low confidence (T5.2.3, Q8 "没把握就不要提") stays `reason =
///   "low_confidence"`, unaffected — this now ALSO covers a low-confidence
///   `"none"` (H1: `parse_classify_reply`'s "none" branch used to hardcode
///   `low_confidence: false` regardless of the model's own reported
///   confidence; fixed to apply the SAME threshold the real-key branch
///   already used).
#[allow(clippy::too_many_arguments)]
async fn classify_one<M, S, R>(
    run_id: &str,
    task: &Task,
    candidates: &[KeyedCandidate],
    steps: &[Step],
    st: &mut RunState,
    model: Option<&M>,
    sink: &S,
    read: &R,
) -> (ItemResult, Option<&'static str>)
where
    M: ModelPort,
    S: AiSink,
    R: AiReadModel,
{
    if candidates.is_empty() {
        // M1 (coordinator review, 2026-09-26 round 2): no non-terminal
        // Direction to offer as a candidate at all (a brand-new user with
        // zero Directions) — the ladder is skipped entirely, same as before
        // T5.2.2 ever existed. NOT a 待定 fallback: no model ever ran, so
        // there is nothing "decisive" to fall back FROM. `reason =
        // "undetermined"` so a caller can tell this apart from a genuine
        // low-confidence model answer; retried on the next trigger once a
        // Direction exists.
        return (ItemResult::Nothing, Some("undetermined"));
    }
    let normalized = normalize_title(&task.title);
    let history = match read.title_history(&normalized).await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(error = %e, task_id = %task.id, "classify: title_history read failed, treating as no history");
            Vec::new()
        }
    };
    let r1 = r1_reflex(&history);
    let r2 = r2_reflex(&task.title, candidates);

    let outcome = run_item(
        run_id,
        Capability::Classify,
        steps,
        st,
        model,
        sink,
        read,
        |engine| build_classify_request(task, candidates, engine),
        |reply: &ModelReply| parse_classify_reply(&reply.text, candidates),
        || {
            r1.clone().map(|d| ClassifyDecision {
                direction_id: Some(d),
                reason: "a same-titled task is already classified into it".to_string(),
                low_confidence: false,
                confidence: None,
                choice: None,
            })
        },
        || {
            r2.clone().map(|d| ClassifyDecision {
                direction_id: Some(d),
                reason: "its title overlaps this Direction/Area the most".to_string(),
                low_confidence: false,
                confidence: None,
                choice: None,
            })
        },
        crate::core::now_iso8601,
        crate::core::ulid,
    )
    .await;

    // M2 (T5.7.2 review round 3): third element renamed `skip_eval` — it now
    // means "do not write an H2 `sin90_classify_evals` row for this outcome"
    // in general, not only "this was a `SinkError::Store` submit failure".
    // `submit_direction_assignment`'s own `is_store_err` (its narrower,
    // correctly-named local meaning) flows straight into it unchanged; the
    // `Outcome::Nothing { deterministic }` arm below is the new source of a
    // `false` here that ISN'T a store error.
    let (result, reason, skip_eval) = match outcome {
        Outcome::Produced { value, engine, rec } => {
            // T5.2.3: read BEFORE `value.direction_id` is matched on below —
            // that match only moves the `direction_id` field out of `value`
            // (a partial move), so `value.low_confidence` stays reachable,
            // but reading it up front keeps the arms below from having to
            // care about field-move ordering at all.
            let low_confidence = value.low_confidence;
            let confidence = value.confidence;
            let choice = value.choice.clone();
            let (result, reason, is_store_err) = match value.direction_id {
                Some(direction_id) => {
                    submit_direction_assignment(
                        &task.id,
                        direction_id,
                        build_rationale(engine, &value.reason),
                        rec,
                        sink,
                    )
                    .await
                }
                // T5.2.3 (Q8 "没把握就不要提"): a LOW-CONFIDENCE non-match
                // stays a bare `Nothing` — it must NOT fall back to 待定
                // either (that would still be "proposing something the
                // model wasn't sure about", just wearing a different
                // Direction). A decisive non-match is still `ok = 1`
                // (§11.4.1) — record it as a plain call, not a proposal.
                None if low_confidence => {
                    if let Err(e) = sink.record_call(rec).await {
                        tracing::warn!(error = %e, task_id = %task.id, "classify: failed to record a decisive no-match call (R6, not fatal)");
                    }
                    (ItemResult::Nothing, Some("low_confidence"), false)
                }
                // T5.2.2 (design §2 #30, Q4): the model looked and decisively
                // said "none" — fall back to 待定 instead of leaving the task
                // in the inbox. Reuses the SAME `ok=1` call row the model
                // step already produced: the model genuinely did its job
                // here (§11.4.1's "记 ok = 1"), it just found nothing, so the
                // resulting proposal's `source` is still derived from this
                // engine/tier (§11.4 公共's `source_for`) — not hardcoded to
                // `rule` — exactly like a real match would be.
                None => {
                    submit_direction_assignment(
                        &task.id,
                        TRIAGE_DIRECTION_ID.to_string(),
                        build_rationale(engine, &value.reason),
                        rec,
                        sink,
                    )
                    .await
                }
            };
            // T5.2.3 review M2: confidence isn't persisted anywhere yet — log
            // it so the data exists at all (run_id/task_id/confidence/choice/
            // whether a proposal came out of it). Only for an actual MODEL
            // step (`engine != Reflex`): reflex has no confidence concept, so
            // logging it there would just be noise. The eventual home for
            // this is a `confidence` column on `sin90_ai_calls` (once T5.5.1
            // needs real per-level hit-rate stats to pick
            // `CLASSIFY_CONFIDENCE_THRESHOLD` for real) — tracked as a
            // followup by the coordinator, not implemented here.
            if engine != Engine::Reflex {
                tracing::info!(
                    run_id = %run_id,
                    task_id = %task.id,
                    confidence = confidence.unwrap_or("n/a"),
                    choice = choice.as_deref().unwrap_or("n/a"),
                    proposed = matches!(result, ItemResult::Proposed(_)),
                    "classify: model step confidence (not persisted, see followup)"
                );
            }
            (result, reason, is_store_err)
        }
        // H3 (coordinator review, 2026-09-26 round 2): every step ran (R1
        // undecided, R2 no_match, no model reachable or every model step
        // degraded away — offline, circuit open, standalone with no model
        // configured) without a SINGLE step producing a value at all. This
        // is NOT the same as a model decisively saying "none" — nothing here
        // ever actually looked at the task with enough information to
        // decide anything, so it must NOT fall back to 待定 (that would
        // silently launder "we never got an answer" into "the answer is
        // no"). Stays a bare `Nothing`, `reason = "undetermined"`, retried
        // whole on the next trigger. Every failed step already wrote its own
        // non-producing call row inside `run_item`; nothing extra to record
        // here.
        //
        // M2 (T5.7.2 review round 3): `skip_eval = !deterministic` — a
        // genuine (if bad_output/tripwire-rejected) model reply DOES count
        // as "classify looked at this" (write the eval, `skip_eval = false`);
        // a ladder that never got a reply at all (Timeout/Unavailable/Busy/
        // circuit-open/no model configured/R1+R2 alone) must NOT (`skip_eval
        // = true`) — see `Outcome::Nothing`'s own doc in `ladder.rs`.
        Outcome::Nothing { deterministic } => {
            (ItemResult::Nothing, Some("undetermined"), !deterministic)
        }
        Outcome::Deferred => (ItemResult::Deferred, None, false),
        Outcome::Aborted => (ItemResult::Aborted, None, false),
    };

    // T5.7.2 review round 2 (H2), narrowed by M-b (review round 2 follow-up):
    // a task CURRENTLY parked in 待定 that this evaluation did NOT move to a
    // real Direction records "classify just looked at this and found nothing
    // new" so `AiReadModel::inbox`/`inbox_task`'s retry gate (`store/
    // ai_port.rs`) advances past this evaluation instead of re-selecting the
    // SAME task on every future run for as long as the same newest Direction
    // stays the newest one (the bug the gate's own exit condition fixes).
    //
    // M-b (T5.7.2 review round 2 follow-up), extended by M2 (review round 3):
    // this must ONLY fire when the MODEL truly replied — a decisive `none`, a
    // low-confidence non-match, a decisive-none rejected at submit because
    // the task was already accounted for, OR (M2) a reply that came back but
    // was rejected as `bad_output`/blocked by the privacy `tripwire`
    // (`Outcome::Nothing { deterministic: true }`, folded into `skip_eval =
    // false` above) — NOT for a `Nothing` the ladder never got a reply for at
    // all (`deterministic: false`: R1/R2 alone, no model reachable, every
    // step degraded on the TRANSPORT — Timeout/Unavailable/Busy/RateLimited/
    // NotReady/circuit-open/no model configured — `skip_eval = true`, so the
    // `(ItemResult::Nothing, Some("undetermined"))` arm below only ever
    // writes when `skip_eval` is already `false`), and NOT for a
    // `SinkError::Store` submit failure (`skip_eval` via `is_store_err` — an
    // infra hiccup that says nothing about whether the model examined the
    // task, unlike a `rejected_by_precheck` state-moved rejection, which
    // still counts: `core::proposal::validate`'s A3 always refuses `None`'s
    // repeated `AssignTaskDirection(t, 待定)` for a task ALREADY in 待定,
    // surfacing as `Rejected` — that IS a genuine "classify looked, found
    // nothing new" outcome). Mutation target: drop the `(ItemResult::Nothing,
    // Some("undetermined"))` arm from this `matches!` and
    // `classify_bad_output_for_a_triage_task_records_eval` goes red; drop
    // `!skip_eval` (or hardcode `deterministic` to `true` in `ladder.rs`) and
    // `classify_degrade_does_not_record_eval_but_new_direction_still_lifts_
    // retry` goes red instead.
    if task.direction_id.as_deref() == Some(TRIAGE_DIRECTION_ID)
        && !skip_eval
        && matches!(
            (&result, reason),
            (ItemResult::Nothing, Some("low_confidence"))
                | (ItemResult::Nothing, Some("undetermined"))
                | (ItemResult::Rejected, _)
        )
    {
        if let Err(e) = sink.record_classify_eval(&task.id).await {
            tracing::warn!(error = %e, task_id = %task.id, "classify: failed to record a 待定 retry evaluation (H2, not fatal)");
        }
    }

    (result, reason)
}

/// Runs every task in `tasks` through the classify ladder, one after another
/// (§11.3.4's abort/budget state — [`RunState`] — is threaded across the
/// whole run, not per item). `candidates`/`settings` are read ONCE for the
/// whole run (§11.4.1's "候选集" — up to 40 non-terminal Directions by
/// `updated_at DESC`).
pub async fn run_classify<M, S, R>(
    run_id: &str,
    tasks: &[Task],
    access: ModelAccess,
    model: Option<&M>,
    sink: &S,
    read: &R,
) -> Vec<ClassifyItem>
where
    M: ModelPort,
    S: AiSink,
    R: AiReadModel,
{
    let settings = read.settings().await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "classify: settings read failed, defaulting to executive disabled");
        Default::default()
    });
    let raw_candidates = read.direction_candidates(40).await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "classify: direction_candidates read failed, treating as empty");
        Vec::new()
    });
    let candidates = candidate_keys(&raw_candidates);
    let steps = plan(Capability::Classify, access, settings, model.is_some());
    let mut st = RunState::new(std::time::Instant::now());
    let mut out = Vec::with_capacity(tasks.len());
    for task in tasks {
        let (result, reason) = if st.aborted {
            (ItemResult::Aborted, None)
        } else {
            classify_one(
                run_id,
                task,
                &candidates,
                &steps,
                &mut st,
                model,
                sink,
                read,
            )
            .await
        };
        out.push(ClassifyItem {
            task_id: task.id.clone(),
            result,
            reason,
        });
    }
    out
}

// ---------------------------------------------------------------- input

/// ⚖️ §11.4.1: at most 20 explicit `task_ids` per trigger.
pub const MAX_CLASSIFY_TASK_IDS: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassifyInputError {
    /// 2026-09-24 review (round 2, low): `task_ids` was given but empty —
    /// omit the field entirely to ask for the auto-selected inbox instead.
    EmptyTaskIds,
    /// More than [`MAX_CLASSIFY_TASK_IDS`] ids were given (§11.4.1: rejected
    /// outright, never truncated).
    TooManyTaskIds,
    /// An explicitly-given id is not currently in the inbox.
    TaskNotInInbox(TaskId),
    /// 2026-09-24 review (M2): the same id appeared more than once.
    DuplicateTaskId(TaskId),
    /// The read model itself failed.
    ReadFailed(String),
}

/// §11.4.1's "输入": `task_ids` given → used as-is (≤ 20, every one must be
/// in the inbox); omitted → the oldest ≤ 20 inbox tasks. Lives here (not the
/// HTTP layer) so it is unit-testable against a fake [`AiReadModel`] without
/// standing up a server, and so `POST /ai/classify`'s handler is a thin
/// wrapper, not where this logic actually lives.
pub async fn select_targets<R: AiReadModel>(
    read: &R,
    task_ids: Option<&[TaskId]>,
) -> Result<Vec<Task>, ClassifyInputError> {
    match task_ids {
        None => read
            .inbox(MAX_CLASSIFY_TASK_IDS as u32)
            .await
            .map_err(|e| ClassifyInputError::ReadFailed(e.to_string())),
        Some(ids) => {
            // 2026-09-24 review (round 2, low): an explicit but EMPTY
            // `task_ids` is a client mistake worth a loud 400, not a silent
            // "run with zero targets" — `task_ids` omitted entirely (the
            // `None` arm above) is the correct way to ask for the
            // auto-selected inbox.
            if ids.is_empty() {
                return Err(ClassifyInputError::EmptyTaskIds);
            }
            if ids.len() > MAX_CLASSIFY_TASK_IDS {
                return Err(ClassifyInputError::TooManyTaskIds);
            }
            // 2026-09-24 review (M2): a repeated id would otherwise count
            // twice toward the 20-item cap and (if it produced a proposal)
            // race itself through the ladder — rejected outright rather than
            // silently deduped, same "loud, not lenient" posture `task_ids >
            // 20` already has.
            let mut seen = std::collections::HashSet::with_capacity(ids.len());
            for id in ids {
                if !seen.insert(id) {
                    return Err(ClassifyInputError::DuplicateTaskId(id.clone()));
                }
            }
            // 2026-09-24 review (L4): a point lookup per id, not
            // `inbox(10_000)` — see `AiReadModel::inbox_task`'s doc.
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                match read
                    .inbox_task(id)
                    .await
                    .map_err(|e| ClassifyInputError::ReadFailed(e.to_string()))?
                {
                    Some(t) => out.push(t),
                    None => return Err(ClassifyInputError::TaskNotInInbox(id.clone())),
                }
            }
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::future::Future;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::ai::ports::{ModelFailure, SinkError};
    use crate::core::{DirectionStatus, Energy, ProposalSource, ProposalStatus, TaskKind};
    use crate::store::Sin90Store;

    // ---- pure logic -----------------------------------------------------

    #[test]
    fn normalize_title_trims_collapses_and_lowercases() {
        assert_eq!(normalize_title("  Write   Report  "), "write report");
        assert_eq!(normalize_title("Already lower"), "already lower");
        assert_eq!(normalize_title(""), "");
    }

    #[test]
    fn r1_reflex_decisive_when_all_history_agrees() {
        let h = vec!["d1".to_string(), "d1".to_string(), "d1".to_string()];
        assert_eq!(r1_reflex(&h), Some("d1".to_string()));
    }

    #[test]
    fn r1_reflex_undecided_on_empty_or_split_history() {
        assert_eq!(r1_reflex(&[]), None);
        let split = vec!["d1".to_string(), "d2".to_string()];
        assert_eq!(r1_reflex(&split), None);
    }

    fn kc(key: &str, direction_id: &str, title: &str, area: Option<&str>) -> KeyedCandidate {
        KeyedCandidate {
            key: key.into(),
            direction_id: direction_id.into(),
            title: title.into(),
            area_title: area.map(str::to_string),
        }
    }

    #[test]
    fn r2_reflex_ascii_word_overlap_picks_unique_winner() {
        let candidates = vec![
            kc("d1", "dir-1", "Quarterly Budget Review", Some("Finance")),
            kc("d2", "dir-2", "Team Offsite Planning", Some("People")),
        ];
        assert_eq!(
            r2_reflex("Draft the quarterly budget numbers", &candidates),
            Some("dir-1".into())
        );
    }

    #[test]
    fn r2_reflex_cjk_bigram_overlap_needs_at_least_two() {
        let candidates = vec![kc("d1", "dir-1", "编码任务", None)];
        // Only one shared bigram ("编码") — below the >= 2 bigram threshold,
        // and no ASCII word hit either, so this stays a no-match.
        assert_eq!(r2_reflex("今天写编码笔记", &candidates), None);
        // Two shared bigrams ("编码", "任务") clears the threshold.
        assert_eq!(r2_reflex("编码任务收尾", &candidates), Some("dir-1".into()));
    }

    #[test]
    fn r2_reflex_tie_is_no_match() {
        let candidates = vec![
            kc("d1", "dir-1", "Budget Review", None),
            kc("d2", "dir-2", "Budget Planning", None),
        ];
        // "budget" matches both equally; neither "review" nor "planning"
        // appears in the task title, so the two candidates tie at 1.
        assert_eq!(r2_reflex("Budget follow-up", &candidates), None);
    }

    #[test]
    fn r2_reflex_below_threshold_no_match() {
        let candidates = vec![kc("d1", "dir-1", "Marketing", None)];
        assert_eq!(r2_reflex("Buy milk", &candidates), None);
    }

    /// 2026-09-24 review (L1): a sub-threshold candidate must still be
    /// COUNTED when determining whether the winner is unique — excluding it
    /// from the race (mutation target: reinstate the OLD `if word_hits == 0
    /// && bigram_hits < 2 { continue }` short-circuit before scoring) would
    /// let `"fix things"` "win" alone even though `"学习计划"` ties it once
    /// both are scored on equal footing, silently promoting a false match.
    #[test]
    fn r2_reflex_ties_across_ascii_and_cjk_candidates_is_no_match() {
        let candidates = vec![
            kc("d1", "dir-1", "fix things", None),
            kc("d2", "dir-2", "学习计划", None),
        ];
        assert_eq!(r2_reflex("fix 学习", &candidates), None);
    }

    /// 2026-09-24 review (L2): a purely numeric run (no ASCII letters) does
    /// not count as a "word" — `"2026"` matching another `"2026"` must not
    /// win on that alone.
    #[test]
    fn r2_reflex_purely_numeric_run_is_not_a_word() {
        let candidates = vec![kc("d1", "dir-1", "Plan 2026 roadmap", None)];
        // "roadmap" doesn't appear in the task title, so only the digit run
        // "2026" would overlap — and it must not count.
        assert_eq!(r2_reflex("Renew the 2026 lease", &candidates), None);
    }

    #[test]
    fn candidate_keys_are_sequential_d1_dn() {
        let raw = vec![
            DirectionCandidate {
                direction_id: "a".into(),
                title: "A".into(),
                status: DirectionStatus::Active,
                area_title: None,
            },
            DirectionCandidate {
                direction_id: "b".into(),
                title: "B".into(),
                status: DirectionStatus::Draft,
                area_title: Some("Area B".into()),
            },
        ];
        let keyed = candidate_keys(&raw);
        assert_eq!(keyed[0].key, "d1");
        assert_eq!(keyed[0].direction_id, "a");
        assert_eq!(keyed[1].key, "d2");
        assert_eq!(keyed[1].area_title.as_deref(), Some("Area B"));
    }

    /// 2026-09-24 review (round 2, low): `build_rationale` strips both
    /// plain control characters AND the hand-picked Cf subset (bidi
    /// overrides, zero-width characters) — a rationale is plain text a
    /// human reads; neither should survive into it. Mutation target: drop
    /// the `!is_cf_format_char(*c)` half of the filter and this goes red
    /// (the zero-width space and RLO survive).
    #[test]
    fn build_rationale_strips_control_and_cf_format_chars() {
        let reason = "matches\u{200B}Work\u{202E}reversed\u{FEFF}";
        let r = build_rationale(Engine::Local, reason);
        assert_eq!(r, "local: matchesWorkreversed");
        assert!(!r.contains('\u{200B}'));
        assert!(!r.contains('\u{202E}'));
        assert!(!r.contains('\u{FEFF}'));
    }

    #[test]
    fn classify_schema_lists_every_key_plus_none() {
        let candidates = vec![kc("d1", "dir-1", "X", None), kc("d2", "dir-2", "Y", None)];
        let schema = classify_schema(&candidates);
        assert_eq!(
            schema.get("additionalProperties"),
            Some(&Value::Bool(false))
        );
        let choice_enum = schema["properties"]["choice"]["enum"].as_array().unwrap();
        let choices: Vec<&str> = choice_enum.iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(choices, vec!["d1", "d2", "none"]);
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

    #[test]
    fn parse_classify_reply_accepts_medium_or_high_confidence_match() {
        let candidates = vec![kc("d1", "dir-1", "X", None)];
        let got = parse_classify_reply(
            r#"{"choice":"d1","confidence":"high","reason":"matches"}"#,
            &candidates,
        )
        .unwrap();
        assert_eq!(got.direction_id, Some("dir-1".to_string()));
        assert_eq!(got.reason, "matches");
    }

    #[test]
    fn parse_classify_reply_tolerates_one_json_fence() {
        let candidates = vec![kc("d1", "dir-1", "X", None)];
        let got = parse_classify_reply(
            "```json\n{\"choice\":\"d1\",\"confidence\":\"high\",\"reason\":\"ok\"}\n```",
            &candidates,
        )
        .unwrap();
        assert_eq!(got.direction_id, Some("dir-1".to_string()));
    }

    #[test]
    fn parse_classify_reply_none_or_low_confidence_is_decisive_no_match() {
        let candidates = vec![kc("d1", "dir-1", "X", None)];
        let none = parse_classify_reply(
            r#"{"choice":"none","confidence":"high","reason":"nothing fits"}"#,
            &candidates,
        )
        .unwrap();
        assert_eq!(none.direction_id, None);

        let low = parse_classify_reply(
            r#"{"choice":"d1","confidence":"low","reason":"maybe"}"#,
            &candidates,
        )
        .unwrap();
        assert_eq!(low.direction_id, None);
    }

    #[test]
    fn parse_classify_reply_rejects_invented_or_real_id_keys() {
        let candidates = vec![kc("d1", "dir-1", "X", None)];
        assert_eq!(
            parse_classify_reply(
                r#"{"choice":"d9","confidence":"high","reason":"x"}"#,
                &candidates
            ),
            Err("bad_output")
        );
        assert_eq!(
            parse_classify_reply(
                r#"{"choice":"01J9Z0000000000000000000","confidence":"high","reason":"x"}"#,
                &candidates
            ),
            Err("bad_output")
        );
    }

    #[test]
    fn parse_classify_reply_rejects_unknown_fields_and_bad_confidence() {
        let candidates = vec![kc("d1", "dir-1", "X", None)];
        assert_eq!(
            parse_classify_reply(
                r#"{"choice":"d1","confidence":"high","reason":"x","extra":1}"#,
                &candidates
            ),
            Err("bad_output")
        );
        assert_eq!(
            parse_classify_reply(
                r#"{"choice":"d1","confidence":"certain","reason":"x"}"#,
                &candidates
            ),
            Err("bad_output")
        );
    }

    // ---- end-to-end driver, against a REAL Sin90Store --------------------
    //
    // Per the boundary checker's own carve-out (`#[cfg(test)]` is skipped
    // entirely, `tests/ai_boundary.rs`'s "v2.1 M4"), these tests build
    // fixtures against `crate::store::Sin90Store` directly — the same
    // convention `store::ai_port`'s own J8/J9 tests use — so they exercise
    // the REAL `AiSink`/`AiReadModel` implementation end to end (submit's
    // dry run, `source_for`'s derivation, `apply_proposal`'s CAS), not a
    // second hand-rolled fake of it.

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

    /// A `ModelPort` that panics if ever called — proves R1's short circuit
    /// (J13) never reaches the model step.
    struct PanicModel;
    impl ModelPort for PanicModel {
        async fn complete(&self, _req: ModelRequest) -> Result<ModelReply, ModelFailure> {
            panic!("classify: the model must not be called when R1 is decisive")
        }
    }

    /// H2's own helper: picks whichever candidate KEY the request's own
    /// `candidates` array offers for a given title, instead of hardcoding
    /// `"d1"` — `direction_candidates`'s `updated_at DESC` ordering is not
    /// guaranteed stable across two Directions created in the same test with
    /// second-resolution timestamps, so a hardcoded key is fragile.
    struct PicksCandidateByTitle(&'static str);
    impl ModelPort for PicksCandidateByTitle {
        async fn complete(&self, req: ModelRequest) -> Result<ModelReply, ModelFailure> {
            let user = req
                .messages
                .iter()
                .find(|m| m.role == crate::ai::ports::Role::User)
                .expect("classify request always has a user message");
            let parsed: Value =
                serde_json::from_str(&user.content).expect("classify user message is JSON");
            let key = parsed["candidates"]
                .as_array()
                .expect("candidates is an array")
                .iter()
                .find(|c| c["title"].as_str() == Some(self.0))
                .unwrap_or_else(|| panic!("no candidate titled {:?} in {parsed}", self.0))["key"]
                .as_str()
                .expect("key is a string")
                .to_string();
            Ok(reply(&format!(
                r#"{{"choice":"{key}","confidence":"high","reason":"matches {}"}}"#,
                self.0
            )))
        }
    }

    /// Answers differently by `complexity` — `executive`'s `Model(Executive)`
    /// step requests `Complexity::Complex`, `local`'s `Model(Local)` step
    /// requests `Complexity::Simple` (`build_classify_request`'s own
    /// mapping) — for J16 (`classify_remote_down_local_up`).
    #[derive(Clone)]
    struct ByEngine {
        exec: Result<ModelReply, ModelFailure>,
        local: Result<ModelReply, ModelFailure>,
    }
    impl ModelPort for ByEngine {
        async fn complete(&self, req: ModelRequest) -> Result<ModelReply, ModelFailure> {
            if req.complexity == Complexity::Complex {
                self.exec.clone()
            } else {
                self.local.clone()
            }
        }
    }

    async fn inbox_task(store: &Sin90Store, title: &str) -> Task {
        store
            .create_task(title, None, None, TaskKind::Other, Energy::Mid, None)
            .await
            .unwrap()
    }

    /// Test-only scaffolding: this codebase has no Direction transition route
    /// or `Sin90Op` yet (design doesn't add one for T5.2.1 either — a
    /// Direction's status only ever matters here as a READ, via A5/R2's
    /// terminal check), so fixtures that need an abandoned Direction go
    /// straight at the row, same convention `store::ai_port`'s own tests use
    /// for state this crate has no write path for.
    async fn abandon_direction(store: &Sin90Store, id: &str) {
        sqlx::query("UPDATE sin90_directions SET status = 'abandoned' WHERE id = ?")
            .bind(id)
            .execute(store.pool())
            .await
            .unwrap();
    }

    /// J11: a fake model choosing the only candidate produces exactly one
    /// pending proposal; the task's `direction_id` is untouched until a
    /// human accepts it, at which point the task leaves the inbox and
    /// exactly one `task.direction_assigned` event (carrying `area_id`)
    /// exists.
    #[tokio::test]
    async fn classify_stub_proposes_and_data_unchanged() {
        let store = Sin90Store::open_memory().await.unwrap();
        let area = store.create_area("Work Life").await.unwrap();
        let direction = store
            .create_direction("Work", "2026-Q4", Some(&area.id))
            .await
            .unwrap();
        let task = inbox_task(&store, "Write the Q4 proposal doc").await;
        let reader = store.ai_reader();

        let model = StubModel::always(Ok(reply(
            r#"{"choice":"d1","confidence":"high","reason":"matches Work"}"#,
        )));
        let items = run_classify(
            "run-classify-1",
            std::slice::from_ref(&task),
            ModelAccess::LocalOnly,
            Some(&model),
            &store,
            &reader,
        )
        .await;

        assert_eq!(items.len(), 1);
        let proposal_id = match &items[0].result {
            ItemResult::Proposed(id) => id.clone(),
            other => panic!("expected Proposed, got {other:?}"),
        };

        let stored = store.get_proposal(&proposal_id).await.unwrap();
        assert_eq!(stored.status, ProposalStatus::Pending);
        assert_eq!(stored.source, ProposalSource::LocalBrain);
        assert_eq!(
            stored.ops,
            vec![Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: direction.id.clone(),
            }]
        );

        // Business data is untouched by a pending proposal.
        let still_inbox = reader.inbox(50).await.unwrap();
        assert!(still_inbox.iter().any(|t| t.id == task.id));

        // Human accept.
        let outcome = store.apply_proposal(&proposal_id).await.unwrap();
        assert!(!outcome.receipt.event_ids.is_empty());
        let after_inbox = reader.inbox(50).await.unwrap();
        assert!(!after_inbox.iter().any(|t| t.id == task.id));

        let events = store
            .list_events(Some("task"), Some(&task.id), None, None)
            .await
            .unwrap();
        let assigned: Vec<_> = events
            .iter()
            .filter(|e| e.kind == "direction_assigned")
            .collect();
        assert_eq!(assigned.len(), 1);
        // 2026-09-24 review (M7): the event's `area_id` must be the REAL
        // Area a human would see — not just "present" (`is_some()` alone
        // would pass even if the wrong id, or a stray string, leaked in).
        assert_eq!(
            assigned[0].payload["area_id"],
            Value::String(area.id.clone())
        );
        assert_eq!(
            assigned[0].payload["direction_id"],
            Value::String(direction.id.clone())
        );
    }

    /// J12 (coordinator review, 2026-09-26 round 2, H3): an invented/real-id
    /// key degrades (bad_output) past the single `local` model step to R2,
    /// which also has nothing to match (`Outcome::Nothing`) — this is NOT a
    /// model decisively saying "none" (the model's own answer was rejected
    /// as malformed), so it must NOT fall back to 待定 either: the item ends
    /// `Nothing`, `reason = "undetermined"`, a call row for the model step is
    /// recorded via `record_call`. Positive control: a valid key on the same
    /// fixture produces a proposal into the real Direction.
    #[tokio::test]
    async fn classify_rejects_invented_keys_and_degrades() {
        let store = Sin90Store::open_memory().await.unwrap();
        let _direction = store
            .create_direction("Finance", "2026-Q4", None)
            .await
            .unwrap();
        let task = inbox_task(&store, "Totally unrelated errand").await;
        let reader = store.ai_reader();

        let bad_model = StubModel::always(Ok(reply(
            r#"{"choice":"d9","confidence":"high","reason":"guessed"}"#,
        )));
        let items = run_classify(
            "run-classify-2",
            std::slice::from_ref(&task),
            ModelAccess::LocalOnly,
            Some(&bad_model),
            &store,
            &reader,
        )
        .await;
        assert_eq!(items[0].result, ItemResult::Nothing);
        assert_eq!(items[0].reason, Some("undetermined"));

        let calls: Vec<(String, bool, Option<String>)> = sqlx::query_as(
            "SELECT engine, ok, error_kind FROM sin90_ai_calls WHERE run_id = 'run-classify-2'",
        )
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert!(calls.iter().any(|(engine, ok, kind)| engine == "local"
            && !ok
            && kind.as_deref() == Some("bad_output")));
        assert!(
            !calls
                .iter()
                .any(|(engine, ok, _)| engine == "reflex" && *ok),
            "H3: no ladder-exhausted case may synthesize an extra ok=1 row: {calls:?}"
        );
        // Exactly one model call for the one item — degrading to R2 does not
        // re-try the model.
        assert_eq!(bad_model.calls(), 1);

        // Positive control: a valid key is accepted.
        let good_model = StubModel::always(Ok(reply(
            r#"{"choice":"d1","confidence":"high","reason":"fits"}"#,
        )));
        let items2 = run_classify(
            "run-classify-2b",
            std::slice::from_ref(&task),
            ModelAccess::LocalOnly,
            Some(&good_model),
            &store,
            &reader,
        )
        .await;
        assert!(matches!(items2[0].result, ItemResult::Proposed(_)));
    }

    // ---- T5.2.2: classify 兜底到「待定」(design §2 #30, J28) --------------

    /// J28 (design §2 #30, Q4): no candidate Direction is a good match (the
    /// model decisively says `"none"`) → the task is assigned to the
    /// reserved 待定 Direction via a NORMAL `AssignTaskDirection` proposal —
    /// not left in the inbox, and NOT a proposal to create a new Direction.
    /// Mutation target: delete the `None => submit_direction_assignment(...,
    /// TRIAGE_DIRECTION_ID, ...)` arm in `classify_one` (fall through to a
    /// bare `Nothing` again) — this goes red.
    #[tokio::test]
    async fn classify_fallback_no_match_assigns_triage_direction() {
        let store = Sin90Store::open_memory().await.unwrap();
        // A real candidate DOES exist (proves the model had a genuine
        // choice available and still decisively said "none" — this is not
        // just "candidates.is_empty()" in disguise, see the M1 test below
        // for that separate case).
        let _real_direction = store
            .create_direction("Finance", "2026-Q4", None)
            .await
            .unwrap();
        let task = inbox_task(&store, "Something that fits nothing on offer").await;
        let reader = store.ai_reader();

        let model = StubModel::always(Ok(reply(
            r#"{"choice":"none","confidence":"high","reason":"nothing fits"}"#,
        )));
        let items = run_classify(
            "run-fallback-triage",
            std::slice::from_ref(&task),
            ModelAccess::LocalOnly,
            Some(&model),
            &store,
            &reader,
        )
        .await;

        let proposal_id = match &items[0].result {
            ItemResult::Proposed(id) => id.clone(),
            other => panic!("expected a 待定 fallback Proposed, got {other:?}"),
        };
        let stored = store.get_proposal(&proposal_id).await.unwrap();
        assert_eq!(
            stored.ops,
            vec![Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: TRIAGE_DIRECTION_ID.to_string(),
            }],
            "a decisive non-match must assign 待定, never propose a new Direction"
        );
        // L1 (coordinator review, 2026-09-26 round 2): an `assert_ne!`
        // comparing this to `real_direction.id` used to sit here — always
        // true regardless of correctness, since a freshly-minted ULID can
        // never equal the fixed literal `TRIAGE_DIRECTION_ID` either way.
        // Removed; the `assert_eq!` above is the only assertion that can
        // actually fail.

        // Human accept works exactly like a real classification — 待定 is an
        // ordinary, non-terminal Direction as far as `AssignTaskDirection`'s
        // validate is concerned (§2 #30: protected by its fixed id, not by
        // any special-cased validate rule).
        let outcome = store.apply_proposal(&proposal_id).await.unwrap();
        assert!(!outcome.receipt.event_ids.is_empty());
        let after_inbox = reader.inbox(50).await.unwrap();
        assert!(!after_inbox.iter().any(|t| t.id == task.id));
    }

    /// J28 positive control for the whole T5.2.2 fallback: on the SAME kind
    /// of fixture, a REAL match still lands on the REAL Direction, never on
    /// 待定 — the fallback only ever fires on a decisive non-match, not on
    /// every classify run. Mirrors J11's own assertion shape.
    #[tokio::test]
    async fn classify_fallback_real_match_assigns_real_direction() {
        let store = Sin90Store::open_memory().await.unwrap();
        let real_direction = store
            .create_direction("Finance", "2026-Q4", None)
            .await
            .unwrap();
        let task = inbox_task(&store, "Reconcile the Q4 budget").await;
        let reader = store.ai_reader();

        let model = StubModel::always(Ok(reply(
            r#"{"choice":"d1","confidence":"high","reason":"matches Finance"}"#,
        )));
        let items = run_classify(
            "run-fallback-real-match",
            std::slice::from_ref(&task),
            ModelAccess::LocalOnly,
            Some(&model),
            &store,
            &reader,
        )
        .await;

        let proposal_id = match &items[0].result {
            ItemResult::Proposed(id) => id.clone(),
            other => panic!("expected Proposed into the real Direction, got {other:?}"),
        };
        let stored = store.get_proposal(&proposal_id).await.unwrap();
        assert_eq!(
            stored.ops,
            vec![Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: real_direction.id.clone(),
            }],
            "a real match must assign the REAL Direction, not 待定"
        );
    }

    /// H1 (coordinator review, 2026-09-26 round 2): the model decisively
    /// says `"none"` but reports `confidence: "low"` about it — this is
    /// EXACTLY the Q8 case ("没把握就不要提"), so it must NOT reach the 待定
    /// fallback: `result = Nothing`, `reason = Some("low_confidence")`, same
    /// posture as a low-confidence REAL pick. `parse_classify_reply` used to
    /// hardcode `low_confidence: false` for `choice == "none"` regardless of
    /// the model's own reported confidence. Mutation target: revert
    /// `parse_classify_reply`'s `"none"` branch to `low_confidence: false` —
    /// this goes red (`result` becomes `Proposed` into 待定).
    #[tokio::test]
    async fn classify_fallback_none_with_low_confidence_stays_nothing() {
        let store = Sin90Store::open_memory().await.unwrap();
        let _direction = store
            .create_direction("Finance", "2026-Q4", None)
            .await
            .unwrap();
        let task = inbox_task(&store, "Something unrelated to any candidate").await;
        let reader = store.ai_reader();

        let model = StubModel::always(Ok(reply(
            r#"{"choice":"none","confidence":"low","reason":"probably nothing but not sure"}"#,
        )));
        let items = run_classify(
            "run-h1-none-low-confidence",
            std::slice::from_ref(&task),
            ModelAccess::LocalOnly,
            Some(&model),
            &store,
            &reader,
        )
        .await;
        assert_eq!(items[0].result, ItemResult::Nothing);
        assert_eq!(items[0].reason, Some("low_confidence"));

        let proposal_count: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_proposals")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(
            proposal_count, 0,
            "no proposal — not even into 待定 — may be produced"
        );
    }

    /// H3 (coordinator review, 2026-09-26 round 2): the WHOLE ladder runs
    /// out (both engines down here — offline/circuit-open/standalone are the
    /// same shape) without a single step ever producing a decision at all.
    /// This is NOT a model decisively saying "none" — nothing ever looked at
    /// the task with enough information to decide anything — so it must stay
    /// `Nothing`, `reason = "undetermined"`, and produce NO proposal.
    /// Mutation target: restore `Outcome::Nothing`'s old
    /// `submit_direction_assignment(..., TRIAGE_DIRECTION_ID, ...)` arm in
    /// `classify_one` — this goes red (a 待定 proposal appears).
    #[tokio::test]
    async fn classify_fallback_ladder_exhausted_stays_undetermined() {
        let store = Sin90Store::open_memory().await.unwrap();
        let _direction = store
            .create_direction("Finance", "2026-Q4", None)
            .await
            .unwrap();
        let task = inbox_task(&store, "Totally unrelated errand").await;
        let reader = store.ai_reader();

        let both_down = ByEngine {
            exec: Err(ModelFailure::Unavailable {
                retryable: true,
                cause: crate::ai::ports::UnavailableCause::NoProvider,
            }),
            local: Err(ModelFailure::Timeout),
        };
        let items = run_classify(
            "run-h3-ladder-exhausted",
            std::slice::from_ref(&task),
            ModelAccess::RemoteAllowed,
            Some(&both_down),
            &store,
            &reader,
        )
        .await;
        assert_eq!(items[0].result, ItemResult::Nothing);
        assert_eq!(items[0].reason, Some("undetermined"));

        let proposal_count: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_proposals")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(proposal_count, 0);
    }

    /// M1 (coordinator review, 2026-09-26 round 2): a brand-new user with
    /// ZERO non-terminal Directions — the ladder is skipped entirely (no
    /// candidates for R1/R2/the model to work with), stays `Nothing`,
    /// `reason = "undetermined"`, no call row, no proposal — same as
    /// T5.2.2's original design line, just with the new `reason` tag added.
    /// Mutation target: restore `candidates.is_empty()`'s old
    /// `submit_direction_assignment(...)` call — this goes red.
    #[tokio::test]
    async fn classify_fallback_no_candidates_stays_undetermined() {
        let store = Sin90Store::open_memory().await.unwrap();
        let task = inbox_task(&store, "Anything at all").await;
        let reader = store.ai_reader();

        let items = run_classify(
            "run-m1-no-candidates",
            std::slice::from_ref(&task),
            ModelAccess::LocalOnly,
            Some(&PanicModel),
            &store,
            &reader,
        )
        .await;
        assert_eq!(items[0].result, ItemResult::Nothing);
        assert_eq!(items[0].reason, Some("undetermined"));

        let call_count: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_ai_calls")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(
            call_count, 0,
            "an empty candidate set must not write any call row either"
        );
    }

    /// H2 (coordinator review, 2026-09-26 round 2): repro straight from the
    /// review — "walk the dog" falls back to 待定 (decisive high-confidence
    /// "none"), the human accepts it, THEN the user creates the perfect real
    /// Direction "Dog walking", THEN a second "Walk the dog" arrives. R1's
    /// history must NOT count the first (待定-classified) task as evidence —
    /// if it did, R1 would decisively re-route the second task straight to
    /// 待定 too, and the model (which WOULD have picked "Dog walking") would
    /// never even be asked. Mutation target: delete the `AND d.id != ?`
    /// clause (or its bind) from `AiReadModel::title_history` — this goes
    /// red (the second task is wrongly routed to 待定 by R1 alone).
    #[tokio::test]
    async fn classify_fallback_title_history_excludes_triage_assignments() {
        let store = Sin90Store::open_memory().await.unwrap();
        // An unrelated real candidate must exist so the model actually runs
        // (M1: an empty candidate set short-circuits to `Nothing` before
        // ever reaching the model — this test is about R1's history, not
        // that separate case).
        let _unrelated = store
            .create_direction("Finance", "2026-Q4", None)
            .await
            .unwrap();
        let task1 = inbox_task(&store, "walk the dog").await;
        let reader = store.ai_reader();

        let none_model = StubModel::always(Ok(reply(
            r#"{"choice":"none","confidence":"high","reason":"nothing fits yet"}"#,
        )));
        let items1 = run_classify(
            "run-h2-first",
            std::slice::from_ref(&task1),
            ModelAccess::LocalOnly,
            Some(&none_model),
            &store,
            &reader,
        )
        .await;
        let proposal_id1 = match &items1[0].result {
            ItemResult::Proposed(id) => id.clone(),
            other => panic!("expected the 待定 fallback for task1, got {other:?}"),
        };
        store.apply_proposal(&proposal_id1).await.unwrap();
        let after1: String =
            sqlx::query_scalar("SELECT direction_id FROM sin90_tasks WHERE id = ?")
                .bind(&task1.id)
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(after1, TRIAGE_DIRECTION_ID);

        // The user now creates the Direction that genuinely fits.
        let dog_walking = store
            .create_direction("Dog walking", "2026-Q4", None)
            .await
            .unwrap();

        let task2 = inbox_task(&store, "Walk the dog").await; // same normalized title as task1
                                                              // Picks whichever candidate key the request ACTUALLY offers for
                                                              // "Dog walking" — not a hardcoded "d1" — because candidate ordering
                                                              // (`direction_candidates`'s `updated_at DESC`) is not guaranteed
                                                              // stable when "Finance" and "Dog walking" share the same
                                                              // second-resolution timestamp in a fast test run.
        let real_match_model = PicksCandidateByTitle("Dog walking");
        let items2 = run_classify(
            "run-h2-second",
            std::slice::from_ref(&task2),
            ModelAccess::LocalOnly,
            Some(&real_match_model),
            &store,
            &reader,
        )
        .await;
        let proposal_id2 = match &items2[0].result {
            ItemResult::Proposed(id) => id.clone(),
            other => {
                panic!("expected task2 to reach the model and match Dog walking, got {other:?}")
            }
        };
        let stored2 = store.get_proposal(&proposal_id2).await.unwrap();
        assert_eq!(
            stored2.ops,
            vec![Sin90Op::AssignTaskDirection {
                task_id: task2.id.clone(),
                direction_id: dog_walking.id.clone(),
            }],
            "R1 must not have shortcut task2 to 待定 using task1's history"
        );
    }

    // ---- M-b (T5.7.2 review round 2 follow-up): H2's eval-write gate only
    // ---- fires when the model actually replied, never for a Degrade ------

    /// A sink whose `submit` always fails with a given `SinkError` — a
    /// minimal fake, not a real `Sin90Store`, purely to unit-test
    /// `submit_direction_assignment`'s `is_store_err` derivation directly
    /// without needing to engineer a real race/infra failure through a live
    /// store.
    struct AlwaysFailsSubmit(fn() -> SinkError);
    impl AiSink for AlwaysFailsSubmit {
        async fn submit(
            &self,
            _cap: Capability,
            _draft: ProposalDraft,
            _rec: AiCallRecord,
        ) -> Result<(), SinkError> {
            Err((self.0)())
        }
        async fn record_call(&self, _rec: AiCallRecord) -> Result<(), SinkError> {
            Ok(())
        }
        async fn record_classify_eval(&self, _task_id: &str) -> Result<(), SinkError> {
            Ok(())
        }
        async fn precheck(&self, _cap: Capability, drafts: &[ProposalDraft]) -> Vec<bool> {
            vec![false; drafts.len()]
        }
    }

    fn any_call_rec() -> AiCallRecord {
        AiCallRecord {
            id: "c-any".into(),
            run_id: "run-any".into(),
            task_kind: Capability::Classify,
            engine: crate::ai::Engine::Local,
            fallback_from: None,
            served_tier: None,
            model_id: None,
            prompt_tokens: None,
            completion_tokens: None,
            latency_ms: 0,
            ok: true,
            error_kind: None,
            proposal_id: None,
            at: "2026-09-26T00:00:00Z".into(),
        }
    }

    /// `submit_direction_assignment`'s third return value is `true` ONLY for
    /// `SinkError::Store` — the infra-hiccup bucket M-b's H2 eval-write gate
    /// must never advance on. Mutation target: change `matches!(e,
    /// SinkError::Store(_))` to always return `false` (or delete the
    /// `is_store_err` plumbing entirely) and this goes red.
    #[tokio::test]
    async fn submit_direction_assignment_flags_store_errors_only() {
        let store_err_sink = AlwaysFailsSubmit(|| SinkError::Store("boom".into()));
        let (result, reason, is_store_err) = submit_direction_assignment(
            &"t1".to_string(),
            "d1".to_string(),
            "because".into(),
            any_call_rec(),
            &store_err_sink,
        )
        .await;
        assert_eq!(result, ItemResult::Rejected);
        assert_eq!(reason, None);
        assert!(is_store_err, "a SinkError::Store must flag is_store_err");

        let invalid_sink = AlwaysFailsSubmit(|| SinkError::Invalid("bad state".into()));
        let (result, reason, is_store_err) = submit_direction_assignment(
            &"t1".to_string(),
            "d1".to_string(),
            "because".into(),
            any_call_rec(),
            &invalid_sink,
        )
        .await;
        assert_eq!(result, ItemResult::Rejected);
        assert_eq!(reason, None);
        assert!(
            !is_store_err,
            "a SinkError::Invalid (rejected_by_precheck) must NOT flag is_store_err"
        );
    }

    /// The regression M-b fixes: BEFORE this fix, `Outcome::Nothing`'s
    /// `"undetermined"` reason was ALSO in `classify_one`'s H2 eval-write
    /// `matches!`, so a task the model never actually reached (every model
    /// step Timing out/Unavailable/circuit-open, `Outcome::Nothing`) wrote a
    /// `sin90_classify_evals` row exactly as if the model had genuinely
    /// looked and found nothing — laundering "we never got an answer" into
    /// "classify examined this and moved the retry floor forward" (the exact
    /// overclaim §2 #30/H3's own doc already forbids for the PROPOSAL side,
    /// just not, until now, for this side-channel). Pins BOTH halves: no row
    /// is written on a Timeout, AND the retry signal a genuinely NEW
    /// Direction provides is untouched by that (it was never gated on the
    /// eval row in the first place — `triage_entered_at`, N-H1, is the floor
    /// absent one). Mutation target: add `(ItemResult::Nothing, Some(
    /// "undetermined"))` back into the `matches!` and the `count == 0`
    /// assertion below goes red.
    #[tokio::test]
    async fn classify_degrade_does_not_record_eval_but_new_direction_still_lifts_retry() {
        let store = Sin90Store::open_memory().await.unwrap();
        let _direction = store
            .create_direction("Finance", "2026-Q4", None)
            .await
            .unwrap();
        let task = inbox_task(&store, "Ambiguous errand").await;
        let reader = store.ai_reader();

        // Route the task into 待定 first (a decisive model "none"), same
        // shape the H2 tests above use.
        let none_model = StubModel::always(Ok(reply(
            r#"{"choice":"none","confidence":"high","reason":"nothing fits yet"}"#,
        )));
        let items0 = run_classify(
            "run-mb-seed",
            std::slice::from_ref(&task),
            ModelAccess::LocalOnly,
            Some(&none_model),
            &store,
            &reader,
        )
        .await;
        let proposal_id0 = match &items0[0].result {
            ItemResult::Proposed(id) => id.clone(),
            other => panic!("expected the 待定 fallback, got {other:?}"),
        };
        store.apply_proposal(&proposal_id0).await.unwrap();
        crate::store::test_hooks::set_task_triage_entered_at(
            &store,
            &task.id,
            "2020-01-01T00:00:00Z",
        )
        .await
        .unwrap();

        async fn eval_count(store: &Sin90Store, task_id: &str) -> i64 {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM sin90_classify_evals WHERE task_id = ?",
            )
            .bind(task_id)
            .fetch_one(store.pool())
            .await
            .unwrap()
        }
        assert_eq!(
            eval_count(&store, &task.id).await,
            0,
            "no evaluation recorded yet"
        );

        // Re-run classify on the SAME (now-in-待定) task with every model
        // step degrading (Timeout) — R1 has no history (title_history
        // excludes triage assignments), the model Times out (Degrade, does
        // not open the circuit), R2 finds no title overlap with "Finance"
        // either: the whole ladder ends `Outcome::Nothing`, `reason =
        // "undetermined"`.
        let timeout_model = StubModel::always(Err(ModelFailure::Timeout));
        let mut triage_task = task.clone();
        triage_task.direction_id = Some(TRIAGE_DIRECTION_ID.to_string());
        let items1 = run_classify(
            "run-mb-degrade",
            std::slice::from_ref(&triage_task),
            ModelAccess::LocalOnly,
            Some(&timeout_model),
            &store,
            &reader,
        )
        .await;
        assert_eq!(items1[0].result, ItemResult::Nothing);
        assert_eq!(items1[0].reason, Some("undetermined"));
        assert_eq!(
            eval_count(&store, &task.id).await,
            0,
            "a Degrade (Timeout) must NOT record a classify eval — the model never actually replied"
        );

        // Positive control: the retry signal a genuinely NEW Direction
        // provides is untouched by the (correctly skipped) eval write —
        // `triage_entered_at` alone is enough of a floor.
        let fresh = store
            .create_direction("Freshly created", "2026-Q4", None)
            .await
            .unwrap();
        crate::store::test_hooks::set_direction_created_at(
            &store,
            &fresh.id,
            "2099-01-01T00:00:00Z",
        )
        .await
        .unwrap();
        assert!(
            crate::ai::AiReadModel::inbox_task(&reader, &task.id)
                .await
                .unwrap()
                .is_some(),
            "a new Direction must still lift the retry suppression despite the skipped eval write"
        );
    }

    /// M2 (T5.7.2 review round 3) positive control, paired with the Timeout
    /// negative control just above: a task CURRENTLY in 待定, re-evaluated by
    /// a model that DOES reply — with an invented candidate key, rejected as
    /// `bad_output` — DOES write a classify eval. The model genuinely looked
    /// (a real reply came back, R2 has nothing to fall back to either since
    /// the title has no overlap with the one real Direction), it was just
    /// unusable — exactly the "found nothing new" signal the retry gate
    /// needs to advance past, same as a low-confidence reply already does.
    /// Mutation target: delete `deterministic = true` from `ladder.rs`'s
    /// `Err(why)` parse-failure arm (or the `tripwire` arm) and the
    /// `count == 1` assertion below goes red.
    #[tokio::test]
    async fn classify_bad_output_for_a_triage_task_records_eval() {
        let store = Sin90Store::open_memory().await.unwrap();
        let _direction = store
            .create_direction("Finance", "2026-Q4", None)
            .await
            .unwrap();
        let task = inbox_task(&store, "Ambiguous errand").await;
        let reader = store.ai_reader();

        let none_model = StubModel::always(Ok(reply(
            r#"{"choice":"none","confidence":"high","reason":"nothing fits yet"}"#,
        )));
        let items0 = run_classify(
            "run-m2-bad-output-seed",
            std::slice::from_ref(&task),
            ModelAccess::LocalOnly,
            Some(&none_model),
            &store,
            &reader,
        )
        .await;
        let proposal_id0 = match &items0[0].result {
            ItemResult::Proposed(id) => id.clone(),
            other => panic!("expected the 待定 fallback, got {other:?}"),
        };
        store.apply_proposal(&proposal_id0).await.unwrap();
        crate::store::test_hooks::set_task_triage_entered_at(
            &store,
            &task.id,
            "2020-01-01T00:00:00Z",
        )
        .await
        .unwrap();

        async fn eval_count(store: &Sin90Store, task_id: &str) -> i64 {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM sin90_classify_evals WHERE task_id = ?",
            )
            .bind(task_id)
            .fetch_one(store.pool())
            .await
            .unwrap()
        }
        assert_eq!(
            eval_count(&store, &task.id).await,
            0,
            "no evaluation recorded yet"
        );

        // Re-run classify on the SAME (now-in-待定) task with the model
        // inventing a candidate key that does not exist — `parse_classify_
        // reply` rejects it as `bad_output`, degrading past the single
        // `local` model step to R2, which also has nothing to match: the
        // whole ladder ends `Outcome::Nothing { deterministic: true }`,
        // `reason = "undetermined"`.
        let bad_model = StubModel::always(Ok(reply(
            r#"{"choice":"d9","confidence":"high","reason":"guessed"}"#,
        )));
        let mut triage_task = task.clone();
        triage_task.direction_id = Some(TRIAGE_DIRECTION_ID.to_string());
        let items1 = run_classify(
            "run-m2-bad-output",
            std::slice::from_ref(&triage_task),
            ModelAccess::LocalOnly,
            Some(&bad_model),
            &store,
            &reader,
        )
        .await;
        assert_eq!(items1[0].result, ItemResult::Nothing);
        assert_eq!(items1[0].reason, Some("undetermined"));
        assert_eq!(
            eval_count(&store, &task.id).await,
            1,
            "a `bad_output` reply must record a classify eval — the model genuinely looked, \
             it just answered badly, unlike a Degrade/Timeout that never got a reply at all"
        );
    }

    /// M-c pin (H2's `low_confidence` branch): a task CURRENTLY in 待定,
    /// re-evaluated by a model that DOES reply but at low confidence, DOES
    /// write a classify eval — the model genuinely looked, it just wasn't
    /// sure, which is exactly the "found nothing new" signal the retry gate
    /// needs to advance past. Mutation target: delete `(ItemResult::Nothing,
    /// Some("low_confidence"))` from the `matches!` and this goes red.
    #[tokio::test]
    async fn classify_low_confidence_for_a_triage_task_records_eval() {
        let store = Sin90Store::open_memory().await.unwrap();
        let _direction = store
            .create_direction("Finance", "2026-Q4", None)
            .await
            .unwrap();
        let task = inbox_task(&store, "Ambiguous errand").await;
        let reader = store.ai_reader();

        let none_model = StubModel::always(Ok(reply(
            r#"{"choice":"none","confidence":"high","reason":"nothing fits yet"}"#,
        )));
        let items0 = run_classify(
            "run-mc-lowconf-seed",
            std::slice::from_ref(&task),
            ModelAccess::LocalOnly,
            Some(&none_model),
            &store,
            &reader,
        )
        .await;
        let proposal_id0 = match &items0[0].result {
            ItemResult::Proposed(id) => id.clone(),
            other => panic!("expected the 待定 fallback, got {other:?}"),
        };
        store.apply_proposal(&proposal_id0).await.unwrap();

        let mut triage_task = task.clone();
        triage_task.direction_id = Some(TRIAGE_DIRECTION_ID.to_string());
        let low_confidence_model = StubModel::always(Ok(reply(
            r#"{"choice":"none","confidence":"low","reason":"maybe, not sure"}"#,
        )));
        let items1 = run_classify(
            "run-mc-lowconf",
            std::slice::from_ref(&triage_task),
            ModelAccess::LocalOnly,
            Some(&low_confidence_model),
            &store,
            &reader,
        )
        .await;
        assert_eq!(items1[0].result, ItemResult::Nothing);
        assert_eq!(items1[0].reason, Some("low_confidence"));

        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM sin90_classify_evals WHERE task_id = ?")
                .bind(&task.id)
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(
            count, 1,
            "a low-confidence re-evaluation of a 待定 task must record a classify eval"
        );
    }

    /// M-c pin (H2's `task.direction_id == TRIAGE_DIRECTION_ID` precondition):
    /// a task that is NOT currently parked in 待定 must NEVER get a
    /// `sin90_classify_evals` row, no matter what `(result, reason)` this
    /// evaluation ends in — that table exists purely to advance the 待定
    /// retry gate, and writing it for an ordinary inbox task would be
    /// meaningless (nothing ever reads it for a non-待定 task) at best and a
    /// silent behavior change at worst. Mutation target: delete the
    /// `task.direction_id.as_deref() == Some(TRIAGE_DIRECTION_ID) &&` leg
    /// from the `if` and this goes red.
    #[tokio::test]
    async fn classify_never_records_eval_for_a_non_triage_task() {
        let store = Sin90Store::open_memory().await.unwrap();
        let _direction = store
            .create_direction("Work", "2026-Q4", None)
            .await
            .unwrap();
        let task = inbox_task(&store, "Something unrelated to any candidate").await;
        let reader = store.ai_reader();

        let low_confidence_model = StubModel::always(Ok(reply(
            r#"{"choice":"none","confidence":"low","reason":"probably nothing but not sure"}"#,
        )));
        let items = run_classify(
            "run-mc-precondition",
            std::slice::from_ref(&task),
            ModelAccess::LocalOnly,
            Some(&low_confidence_model),
            &store,
            &reader,
        )
        .await;
        assert_eq!(items[0].result, ItemResult::Nothing);
        assert_eq!(items[0].reason, Some("low_confidence"));

        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM sin90_classify_evals WHERE task_id = ?")
                .bind(&task.id)
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(
            count, 0,
            "a task not currently in 待定 must never get a classify eval row"
        );
    }

    // ---- T5.2.3: classify 低置信度不出提议 (J27) --------------------------

    /// J27 (negative half): the model picks a REAL candidate key but reports
    /// `confidence: "low"` — below [`CLASSIFY_CONFIDENCE_THRESHOLD`]
    /// (`Medium`) — so the item ends `Nothing` with NO proposal, exactly
    /// like an invented key or an explicit `"none"` would, but the call is
    /// still recorded as a normal `ok = 1` step (the model did its job; it
    /// just wasn't confident enough) and the run item is tagged
    /// `reason = "low_confidence"` so a caller can tell this apart from every
    /// other cause of `Nothing`. Mutation target: delete the `confidence <
    /// CLASSIFY_CONFIDENCE_THRESHOLD` branch in `parse_classify_reply` (or
    /// widen the threshold to `Low`) and this test goes red — a proposal
    /// appears instead.
    #[tokio::test]
    async fn classify_low_confidence_below_threshold_is_nothing_and_records_ok1_call() {
        let store = Sin90Store::open_memory().await.unwrap();
        let _direction = store
            .create_direction("Work", "2026-Q4", None)
            .await
            .unwrap();
        let task = inbox_task(&store, "Something unrelated to any candidate").await;
        let reader = store.ai_reader();

        let model = StubModel::always(Ok(reply(
            r#"{"choice":"d1","confidence":"low","reason":"maybe, not sure"}"#,
        )));
        let items = run_classify(
            "run-low-confidence-1",
            std::slice::from_ref(&task),
            ModelAccess::LocalOnly,
            Some(&model),
            &store,
            &reader,
        )
        .await;
        assert_eq!(items[0].result, ItemResult::Nothing);
        assert_eq!(items[0].reason, Some("low_confidence"));

        let calls: Vec<(String, bool, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT engine, ok, error_kind, proposal_id FROM sin90_ai_calls \
             WHERE run_id = 'run-low-confidence-1'",
        )
        .fetch_all(store.pool())
        .await
        .unwrap();
        let (_, ok, error_kind, proposal_id) = calls
            .iter()
            .find(|(engine, ..)| engine == "local")
            .expect("the model step's own call row must exist");
        assert!(
            ok,
            "a low-confidence call is a successful call (§11.4.1 ok=1)"
        );
        assert_eq!(*error_kind, None);
        assert_eq!(*proposal_id, None);

        let proposal_count: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_proposals")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(proposal_count, 0, "no proposal must be submitted at all");
    }

    /// J27 (positive control, exact boundary): the SAME fixture, but
    /// `confidence: "medium"` — AT the threshold, not below it — still
    /// proposes; `reason` is `None` (not tagged `low_confidence`) since the
    /// item didn't end `Nothing` at all. Proves the comparison is `<`
    /// (exclusive), not `<=`. Mutation target: change `parse_classify_reply`'s
    /// `confidence < CLASSIFY_CONFIDENCE_THRESHOLD` to `<=` and this goes red
    /// (the proposal disappears).
    #[tokio::test]
    async fn classify_low_confidence_medium_confidence_at_threshold_still_proposes() {
        let store = Sin90Store::open_memory().await.unwrap();
        let _direction = store
            .create_direction("Work", "2026-Q4", None)
            .await
            .unwrap();
        let task = inbox_task(&store, "Something unrelated to any candidate").await;
        let reader = store.ai_reader();

        let model = StubModel::always(Ok(reply(
            r#"{"choice":"d1","confidence":"medium","reason":"good enough"}"#,
        )));
        let items = run_classify(
            "run-low-confidence-2",
            std::slice::from_ref(&task),
            ModelAccess::LocalOnly,
            Some(&model),
            &store,
            &reader,
        )
        .await;
        assert!(matches!(items[0].result, ItemResult::Proposed(_)));
        assert_eq!(items[0].reason, None);
    }

    /// J27 (positive control, high confidence): same shape one level up —
    /// included alongside the `medium` boundary test so both non-rejected
    /// levels are exercised under this judgement's own filter name, not just
    /// inherited from J11/J12's fixtures (which predate the threshold
    /// existing as a named, comparable value).
    #[tokio::test]
    async fn classify_low_confidence_high_confidence_still_proposes() {
        let store = Sin90Store::open_memory().await.unwrap();
        let _direction = store
            .create_direction("Work", "2026-Q4", None)
            .await
            .unwrap();
        let task = inbox_task(&store, "Something unrelated to any candidate").await;
        let reader = store.ai_reader();

        let model = StubModel::always(Ok(reply(
            r#"{"choice":"d1","confidence":"high","reason":"fits well"}"#,
        )));
        let items = run_classify(
            "run-low-confidence-3",
            std::slice::from_ref(&task),
            ModelAccess::LocalOnly,
            Some(&model),
            &store,
            &reader,
        )
        .await;
        assert!(matches!(items[0].result, ItemResult::Proposed(_)));
        assert_eq!(items[0].reason, None);
    }

    /// J27/T5.2.2: an explicit `choice == "none"` (the model confidently says
    /// nothing fits) must NOT be tagged `"low_confidence"` — the two causes
    /// are different (§11.4.1: "none" is a decisive non-match on its own;
    /// low confidence is "wasn't sure enough about a real pick") and only
    /// one of them should carry the reason. Since T5.2.2 the two also
    /// diverge on `result` itself: a decisive "none" now falls back to the
    /// reserved 待定 Direction (`Proposed`, `reason: None`), while low
    /// confidence stays a bare `Nothing` with `reason: Some("low_confidence")`
    /// — see `classify_low_confidence_below_threshold_is_nothing_and_records_
    /// ok1_call` for that half. Mutation target: make `classify_one` tag the
    /// fallback proposal's reason as `Some("low_confidence")` whenever
    /// `direction_id` was `None`, regardless of `ClassifyDecision::
    /// low_confidence` — this test goes red (the `"none"` case would wrongly
    /// get the reason too).
    #[tokio::test]
    async fn classify_low_confidence_explicit_none_is_not_tagged_low_confidence() {
        let store = Sin90Store::open_memory().await.unwrap();
        let _direction = store
            .create_direction("Work", "2026-Q4", None)
            .await
            .unwrap();
        let task = inbox_task(&store, "Something unrelated to any candidate").await;
        let reader = store.ai_reader();

        let model = StubModel::always(Ok(reply(
            r#"{"choice":"none","confidence":"high","reason":"nothing fits"}"#,
        )));
        let items = run_classify(
            "run-low-confidence-4",
            std::slice::from_ref(&task),
            ModelAccess::LocalOnly,
            Some(&model),
            &store,
            &reader,
        )
        .await;
        let proposal_id = match &items[0].result {
            ItemResult::Proposed(id) => id.clone(),
            other => panic!("expected a 待定 fallback Proposed, got {other:?}"),
        };
        assert_eq!(items[0].reason, None);
        let stored = store.get_proposal(&proposal_id).await.unwrap();
        assert_eq!(
            stored.ops,
            vec![Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: TRIAGE_DIRECTION_ID.to_string(),
            }]
        );
    }

    /// J27: the confidence threshold is a MODEL-step concern only — reflex
    /// (R1, decisive) still proposes exactly as before, and the model is
    /// never even reached to report a confidence at all. Mirrors
    /// `classify_reflex_history_short_circuits_the_model` (J13) under this
    /// judgement's own filter name, framed around the new threshold rather
    /// than the ladder's step order. Mutation target: any change that routes
    /// R1's decision through the same "is this below threshold" check as the
    /// model step would panic here (`PanicModel` is never allowed to run),
    /// which is itself already a stronger signal than a red assertion.
    #[tokio::test]
    async fn classify_low_confidence_reflex_r1_unaffected_by_threshold() {
        let store = Sin90Store::open_memory().await.unwrap();
        let direction = store
            .create_direction("Work", "2026-Q4", None)
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
        let new_task = inbox_task(&store, "  Write   the Report  ").await; // same normalized title
        let reader = store.ai_reader();

        let items = run_classify(
            "run-low-confidence-5",
            std::slice::from_ref(&new_task),
            ModelAccess::LocalOnly,
            Some(&PanicModel),
            &store,
            &reader,
        )
        .await;
        assert!(matches!(items[0].result, ItemResult::Proposed(_)));
        assert_eq!(items[0].reason, None);
        let stored = match &items[0].result {
            ItemResult::Proposed(id) => store.get_proposal(id).await.unwrap(),
            _ => unreachable!(),
        };
        assert_eq!(stored.source, ProposalSource::Rule);
    }

    /// J27 (coordinator review, 2026-09-26 round 2, H3): an INVENTED key
    /// paired with `confidence: "low"` must still be `bad_output` (a real
    /// parsing/validity failure), NOT quietly reclassified as a
    /// low-confidence `nothing` — the invented-key check in
    /// `parse_classify_reply` runs BEFORE the confidence check specifically
    /// so this combination is never misdiagnosed as "the model looked and
    /// wasn't sure" when it actually named a key that does not exist.
    /// `bad_output` degrades to R2 (no match either, `Outcome::Nothing`) —
    /// the item ends `Nothing`, `reason = "undetermined"`, NOT
    /// `"low_confidence"` (that positively confirms this went through the
    /// `bad_output`/R2 path, not the confidence-threshold path) and NOT a
    /// 待定 fallback either (H3: the ladder never produced an actual
    /// decision to fall back from). Mutation target: reorder
    /// `parse_classify_reply` to check `confidence` before the
    /// candidate-membership check — the `bad_output` row disappears and
    /// `items[0].reason` would (wrongly) become `Some("low_confidence")`.
    #[tokio::test]
    async fn classify_low_confidence_invented_key_with_low_confidence_is_still_bad_output() {
        let store = Sin90Store::open_memory().await.unwrap();
        let _direction = store
            .create_direction("Work", "2026-Q4", None)
            .await
            .unwrap();
        let task = inbox_task(&store, "Totally unrelated errand").await;
        let reader = store.ai_reader();

        let model = StubModel::always(Ok(reply(
            r#"{"choice":"d9","confidence":"low","reason":"guessed and unsure"}"#,
        )));
        let items = run_classify(
            "run-low-confidence-6",
            std::slice::from_ref(&task),
            ModelAccess::LocalOnly,
            Some(&model),
            &store,
            &reader,
        )
        .await;
        assert_eq!(items[0].result, ItemResult::Nothing);
        assert_eq!(
            items[0].reason,
            Some("undetermined"),
            "an invented key degrades via bad_output/R2 exhaustion, not the low-confidence path"
        );

        let calls: Vec<(String, bool, Option<String>)> = sqlx::query_as(
            "SELECT engine, ok, error_kind FROM sin90_ai_calls WHERE run_id = 'run-low-confidence-6'",
        )
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert!(calls.iter().any(|(engine, ok, kind)| engine == "local"
            && !ok
            && kind.as_deref() == Some("bad_output")));
    }

    /// J16 (design §11.7's own name for this judgement): with
    /// `RemoteAllowed` + the executive switch on, `Model(Executive)` runs
    /// first (`build_classify_request`'s `Complexity::Complex`) — when it
    /// fails with `no_provider`, the ladder degrades to `Model(Local)`
    /// (`Complexity::Simple`), which succeeds and produces a proposal whose
    /// `source` is derived from the SERVED tier (`local`), i.e.
    /// `local_brain`, not from the fact that `executive` was REQUESTED.
    /// Positive control: when `local` ALSO fails, R2 has nothing to match
    /// "unrelated task" against (`Outcome::Nothing`) — H3 (coordinator
    /// review, 2026-09-26 round 2): this is a ladder-exhausted case, NOT a
    /// model decisively saying "none", so it stays `Nothing` (`reason =
    /// "undetermined"`), it does NOT fall back to 待定 — and the `local`
    /// step's own `ok=0` row still exists (the failure was recorded, not
    /// swallowed).
    #[tokio::test]
    async fn classify_remote_down_local_up() {
        let store = Sin90Store::open_memory().await.unwrap();
        store.put_ai_executive_enabled(true).await.unwrap();
        let _direction = store
            .create_direction("Work", "2026-Q4", None)
            .await
            .unwrap();
        let task = inbox_task(&store, "Write the Q4 proposal doc").await;
        let reader = store.ai_reader();

        let model = ByEngine {
            exec: Err(ModelFailure::Unavailable {
                retryable: true,
                cause: crate::ai::ports::UnavailableCause::NoProvider,
            }),
            local: Ok(reply(
                r#"{"choice":"d1","confidence":"high","reason":"fits"}"#,
            )),
        };
        let items = run_classify(
            "run-remote-down",
            std::slice::from_ref(&task),
            ModelAccess::RemoteAllowed,
            Some(&model),
            &store,
            &reader,
        )
        .await;
        let proposal_id = match &items[0].result {
            ItemResult::Proposed(id) => id.clone(),
            other => panic!("expected Proposed via local fallback, got {other:?}"),
        };
        let stored = store.get_proposal(&proposal_id).await.unwrap();
        assert_eq!(stored.source, ProposalSource::LocalBrain);

        let rows: Vec<(String, bool, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT engine, ok, error_kind, fallback_from FROM sin90_ai_calls \
             WHERE run_id = 'run-remote-down'",
        )
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert!(rows
            .iter()
            .any(|(engine, ok, kind, _)| engine == "executive"
                && !ok
                && kind.as_deref() == Some("unavailable.no_provider")));
        assert!(rows
            .iter()
            .any(|(engine, ok, _, fallback_from)| engine == "local"
                && *ok
                && fallback_from.as_deref() == Some("executive")));

        // Positive control: the local engine ALSO fails — no proposal, and
        // the local step's ok=0 row still exists (not silently dropped).
        let both_down = ByEngine {
            exec: Err(ModelFailure::Unavailable {
                retryable: true,
                cause: crate::ai::ports::UnavailableCause::NoProvider,
            }),
            local: Err(ModelFailure::Timeout),
        };
        let task2 = inbox_task(&store, "Totally unrelated errand").await;
        let items2 = run_classify(
            "run-remote-down-2",
            std::slice::from_ref(&task2),
            ModelAccess::RemoteAllowed,
            Some(&both_down),
            &store,
            &reader,
        )
        .await;
        assert_eq!(
            items2[0].result,
            ItemResult::Nothing,
            "H3: both engines down + R2 no-match is a ladder-exhausted case, NOT a 待定 fallback"
        );
        assert_eq!(items2[0].reason, Some("undetermined"));
        let local_row_exists: bool = sqlx::query_scalar(
            "SELECT count(*) > 0 FROM sin90_ai_calls \
             WHERE run_id = 'run-remote-down-2' AND engine = 'local' AND ok = 0",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert!(
            local_row_exists,
            "local's own failure must still be recorded"
        );
    }

    /// An `AiSink` that races a conflicting classification into the store
    /// the FIRST time `submit` is called — after `classify_one` has already
    /// decided but before the real `submit`'s dry run runs — so the dry run
    /// sees a task that is no longer in the inbox. Everything else delegates
    /// straight to the wrapped `Sin90Store`.
    struct RaceOnFirstSubmit<'a> {
        store: &'a Sin90Store,
        race_direction_id: String,
        raced: std::sync::atomic::AtomicBool,
    }
    impl<'a> AiSink for RaceOnFirstSubmit<'a> {
        async fn submit(
            &self,
            cap: Capability,
            draft: ProposalDraft,
            rec: crate::ai::AiCallRecord,
        ) -> Result<(), SinkError> {
            if !self.raced.swap(true, std::sync::atomic::Ordering::SeqCst) {
                if let [Sin90Op::AssignTaskDirection { task_id, .. }] = draft.ops.as_slice() {
                    self.store
                        .create_task(
                            "irrelevant, just to occupy a race window",
                            None,
                            None,
                            TaskKind::Other,
                            Energy::Mid,
                            None,
                        )
                        .await
                        .unwrap();
                    // Classify the SAME target task via an unrelated,
                    // already-accepted human proposal — by the time the REAL
                    // submit below runs its dry run, this task is no longer
                    // in the inbox.
                    let manual = crate::core::Sin90Proposal {
                        id: format!("race-{task_id}"),
                        status: ProposalStatus::Pending,
                        source: ProposalSource::Rule,
                        ops: vec![Sin90Op::AssignTaskDirection {
                            task_id: task_id.clone(),
                            direction_id: self.race_direction_id.clone(),
                        }],
                        rationale: None,
                    };
                    self.store.submit_proposal(&manual).await.unwrap();
                    self.store.apply_proposal(&manual.id).await.unwrap();
                }
            }
            AiSink::submit(self.store, cap, draft, rec).await
        }
        async fn record_call(&self, rec: crate::ai::AiCallRecord) -> Result<(), SinkError> {
            AiSink::record_call(self.store, rec).await
        }
        async fn record_classify_eval(&self, task_id: &str) -> Result<(), SinkError> {
            AiSink::record_classify_eval(self.store, task_id).await
        }
        async fn precheck(&self, cap: Capability, drafts: &[ProposalDraft]) -> Vec<bool> {
            AiSink::precheck(self.store, cap, drafts).await
        }
    }

    /// M1 (2026-09-24 review): a decision that fails `submit`'s dry run
    /// (state moved between planning and submit) still gets an `ok=0` call
    /// row, `error_kind = "rejected_by_precheck"` — not silently dropped.
    /// Mutation target: revert `classify_one`'s failure branch to just log +
    /// return `ItemResult::Rejected` without calling `record_call`, and the
    /// `ok=0` row assertion goes red.
    #[tokio::test]
    async fn classify_submit_rejected_by_race_still_records_ok0_call() {
        let store = Sin90Store::open_memory().await.unwrap();
        let direction = store
            .create_direction("Work", "2026-Q4", None)
            .await
            .unwrap();
        let race_direction = store
            .create_direction("Health", "2026-Q4", None)
            .await
            .unwrap();
        let task = inbox_task(&store, "Write the Q4 proposal doc").await;
        let reader = store.ai_reader();

        let model = StubModel::always(Ok(reply(
            r#"{"choice":"d1","confidence":"high","reason":"matches Work"}"#,
        )));
        let racer = RaceOnFirstSubmit {
            store: &store,
            race_direction_id: race_direction.id.clone(),
            raced: std::sync::atomic::AtomicBool::new(false),
        };
        let items = run_classify(
            "run-race-1",
            std::slice::from_ref(&task),
            ModelAccess::LocalOnly,
            Some(&model),
            &racer,
            &reader,
        )
        .await;
        assert_eq!(items[0].result, ItemResult::Rejected);

        // R1 (ReflexDecisive) writes its own `undecided` row first (no
        // history for this title) — the row THIS test cares about is the
        // model (`local`) step's.
        let rows: Vec<(String, bool, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT engine, ok, error_kind, proposal_id FROM sin90_ai_calls WHERE run_id = 'run-race-1'",
        )
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert_eq!(
            rows.len(),
            2,
            "one reflex-undecided row + one model row: {rows:?}"
        );
        let (_, ok, error_kind, proposal_id) = rows
            .iter()
            .find(|(engine, ..)| engine == "local")
            .expect("the model step's row must exist");
        assert!(!ok);
        assert_eq!(error_kind.as_deref(), Some("rejected_by_precheck"));
        assert_eq!(*proposal_id, None);

        // The race's OWN proposal really did classify the task — into the
        // race direction, not the one classify decided on.
        let final_direction: Option<String> =
            sqlx::query_scalar("SELECT direction_id FROM sin90_tasks WHERE id = ?")
                .bind(&task.id)
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(final_direction.as_deref(), Some(race_direction.id.as_str()));
        let _ = direction; // only needed so the model's chosen candidate exists
    }

    /// J13: a same-titled task already classified into a still-open
    /// Direction makes R1 decisive — the model is never reached (the fake
    /// panics on `complete`).
    #[tokio::test]
    async fn classify_reflex_history_short_circuits_the_model() {
        let store = Sin90Store::open_memory().await.unwrap();
        let direction = store
            .create_direction("Work", "2026-Q4", None)
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
        let new_task = inbox_task(&store, "  Write   the Report  ").await; // same normalized title
        let reader = store.ai_reader();

        let items = run_classify(
            "run-classify-3",
            std::slice::from_ref(&new_task),
            ModelAccess::LocalOnly,
            Some(&PanicModel),
            &store,
            &reader,
        )
        .await;
        let proposal_id = match &items[0].result {
            ItemResult::Proposed(id) => id.clone(),
            other => panic!("expected Proposed via R1, got {other:?}"),
        };
        let stored = store.get_proposal(&proposal_id).await.unwrap();
        assert_eq!(stored.source, ProposalSource::Rule);
    }

    /// J13 positive control (2026-09-24 review, M7): change the title by ONE
    /// character (breaks `normalize_title` equality) and R1 is no longer
    /// decisive — the model DOES get called. Proves `classify_reflex_
    /// history_short_circuits_the_model` isn't vacuous (e.g. the model
    /// simply never being reachable at all for unrelated reasons).
    #[tokio::test]
    async fn classify_reflex_history_one_char_off_title_calls_the_model() {
        let store = Sin90Store::open_memory().await.unwrap();
        let direction = store
            .create_direction("Work", "2026-Q4", None)
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
        // One character different ("reports", not "report") — normalizes to
        // a DIFFERENT string, so R1 has no matching history.
        let new_task = inbox_task(&store, "Write the reports").await;
        let reader = store.ai_reader();

        let model = StubModel::always(Ok(reply(
            r#"{"choice":"d1","confidence":"high","reason":"fits"}"#,
        )));
        let items = run_classify(
            "run-classify-3b",
            std::slice::from_ref(&new_task),
            ModelAccess::LocalOnly,
            Some(&model),
            &store,
            &reader,
        )
        .await;
        assert!(matches!(items[0].result, ItemResult::Proposed(_)));
        assert_eq!(model.calls(), 1, "R1 was undecided, so the model must run");
    }

    /// J14b: more than 20 explicit `task_ids` is rejected up front (no read,
    /// no run); exactly 20 (all in the inbox) is accepted.
    #[tokio::test]
    async fn classify_task_ids_over_limit_rejected() {
        let store = Sin90Store::open_memory().await.unwrap();
        let reader = store.ai_reader();
        let too_many: Vec<TaskId> = (0..21).map(|i| format!("t{i}")).collect();
        assert_eq!(
            select_targets(&reader, Some(&too_many)).await,
            Err(ClassifyInputError::TooManyTaskIds)
        );

        let mut ids = Vec::new();
        for i in 0..20 {
            ids.push(inbox_task(&store, &format!("task {i}")).await.id);
        }
        let targets = select_targets(&reader, Some(&ids)).await.unwrap();
        assert_eq!(targets.len(), 20);
    }

    /// 2026-09-24 review (round 2, low): an explicit but EMPTY `task_ids`
    /// is a 400, not a silent "run with zero targets" — omitting the field
    /// entirely (`None`) is the correct way to ask for auto-selection.
    /// Mutation target: remove the `ids.is_empty()` check and this goes
    /// from `Err` to `Ok(vec![])`.
    #[tokio::test]
    async fn classify_empty_task_ids_rejected() {
        let store = Sin90Store::open_memory().await.unwrap();
        let reader = store.ai_reader();
        let empty: Vec<TaskId> = Vec::new();
        assert_eq!(
            select_targets(&reader, Some(&empty)).await,
            Err(ClassifyInputError::EmptyTaskIds)
        );
    }

    /// M2 (2026-09-24 review): a repeated id in `task_ids` is rejected
    /// outright — chosen over silently deduping so the caller cannot be
    /// surprised that "20 ids" became "19 distinct targets". Mutation
    /// target: remove the `seen.insert` duplicate check in `select_targets`
    /// and this goes from `Err` to successfully resolving both entries.
    #[tokio::test]
    async fn classify_select_targets_rejects_duplicate_task_id() {
        let store = Sin90Store::open_memory().await.unwrap();
        let reader = store.ai_reader();
        let t = inbox_task(&store, "Only one task").await;
        let ids = vec![t.id.clone(), t.id.clone()];
        assert_eq!(
            select_targets(&reader, Some(&ids)).await,
            Err(ClassifyInputError::DuplicateTaskId(t.id))
        );
    }

    #[tokio::test]
    async fn classify_select_targets_rejects_task_not_in_inbox() {
        let store = Sin90Store::open_memory().await.unwrap();
        let reader = store.ai_reader();
        let direction = store
            .create_direction("Work", "2026-Q4", None)
            .await
            .unwrap();
        let already_classified = store
            .create_task(
                "done deal",
                Some(&direction.id),
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();
        let err = select_targets(&reader, Some(std::slice::from_ref(&already_classified.id)))
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ClassifyInputError::TaskNotInInbox(already_classified.id)
        );
    }

    /// §11.2.1 A2/A3/A5 through the FULL AI path (submit → accept), proving
    /// the new Op's guards apply exactly the same way whether the proposal
    /// came from a human or from classify: a task no longer in the inbox by
    /// accept time, or a Direction abandoned by accept time, both turn a
    /// once-valid pending proposal into a 422 at accept — never a silent
    /// half-apply.
    #[tokio::test]
    async fn assign_task_direction_cas_guards_hold_through_accept() {
        let store = Sin90Store::open_memory().await.unwrap();
        let d1 = store
            .create_direction("Work", "2026-Q4", None)
            .await
            .unwrap();
        let d2 = store
            .create_direction("Health", "2026-Q4", None)
            .await
            .unwrap();
        let task = inbox_task(&store, "Ambiguous task").await;

        // Two competing proposals for the SAME task (as if two runs, or a
        // human and classify, raced).
        let draft_a = ProposalDraft {
            id: "p-a".into(),
            ops: vec![Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: d1.id.clone(),
            }],
            rationale: None,
        };
        let draft_b = ProposalDraft {
            id: "p-b".into(),
            ops: vec![Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: d2.id.clone(),
            }],
            rationale: None,
        };
        let rec = |id: &str| crate::ai::AiCallRecord {
            id: id.into(),
            run_id: "run-cas".into(),
            task_kind: Capability::Classify,
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
        AiSink::submit(&store, Capability::Classify, draft_a, rec("call-a"))
            .await
            .unwrap();
        AiSink::submit(&store, Capability::Classify, draft_b, rec("call-b"))
            .await
            .unwrap();

        // Accept the first — task leaves the inbox.
        store.apply_proposal("p-a").await.unwrap();
        // 2026-09-24 review (J15): the rejected accept below must leave
        // EVERYTHING about "p-b" untouched — snapshot the event count and
        // "p-b"'s own status first so both are checkable afterward.
        let events_before: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_events")
            .fetch_one(store.pool())
            .await
            .unwrap();
        // The second is now stale (A3: task no longer in the inbox) — CAS.
        let err = store.apply_proposal("p-b").await.unwrap_err();
        assert!(matches!(
            err,
            crate::store::StoreError::Proposal(crate::core::ProposalError::NotInInbox { .. })
        ));
        let events_after: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_events")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(
            events_before, events_after,
            "a rejected accept (validate fails before any write) must append no new event"
        );
        let p_b_status = store.get_proposal("p-b").await.unwrap().status;
        assert_eq!(
            p_b_status,
            ProposalStatus::Pending,
            "a rejected accept must leave the proposal exactly as pending, not half-applying"
        );

        // A separate scenario: valid at submit time, but the target
        // Direction is abandoned before accept.
        let task2 = inbox_task(&store, "Another task").await;
        let d3 = store
            .create_direction("Side quest", "2026-Q4", None)
            .await
            .unwrap();
        let draft_c = ProposalDraft {
            id: "p-c".into(),
            ops: vec![Sin90Op::AssignTaskDirection {
                task_id: task2.id.clone(),
                direction_id: d3.id.clone(),
            }],
            rationale: None,
        };
        AiSink::submit(&store, Capability::Classify, draft_c, rec("call-c"))
            .await
            .unwrap();
        abandon_direction(&store, &d3.id).await;
        let err2 = store.apply_proposal("p-c").await.unwrap_err();
        assert!(matches!(
            err2,
            crate::store::StoreError::Proposal(crate::core::ProposalError::DirectionClosed { .. })
        ));
    }

    /// Precheck ("是否仍有效") reflects the SAME two scenarios without
    /// actually applying anything — this is what a run's dedup step relies
    /// on (§11.4 公共's "去重").
    #[tokio::test]
    async fn precheck_reflects_inbox_and_direction_closure() {
        let store = Sin90Store::open_memory().await.unwrap();
        let d1 = store
            .create_direction("Work", "2026-Q4", None)
            .await
            .unwrap();
        let task = inbox_task(&store, "Some task").await;
        let draft = ProposalDraft {
            id: "p-precheck".into(),
            ops: vec![Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: d1.id.clone(),
            }],
            rationale: None,
        };
        assert_eq!(
            AiSink::precheck(&store, Capability::Classify, std::slice::from_ref(&draft)).await,
            vec![true]
        );

        // (a) task classified by something else → no longer valid.
        let manual = crate::core::Sin90Proposal {
            id: "manual-1".into(),
            status: ProposalStatus::Pending,
            source: ProposalSource::Rule,
            ops: vec![Sin90Op::AssignTaskDirection {
                task_id: task.id.clone(),
                direction_id: d1.id.clone(),
            }],
            rationale: None,
        };
        store.submit_proposal(&manual).await.unwrap();
        store.apply_proposal("manual-1").await.unwrap();
        assert_eq!(
            AiSink::precheck(&store, Capability::Classify, std::slice::from_ref(&draft)).await,
            vec![false]
        );

        // (b) a fresh target whose Direction gets abandoned.
        let d2 = store
            .create_direction("Health", "2026-Q4", None)
            .await
            .unwrap();
        let task2 = inbox_task(&store, "Another one").await;
        let draft2 = ProposalDraft {
            id: "p-precheck-2".into(),
            ops: vec![Sin90Op::AssignTaskDirection {
                task_id: task2.id.clone(),
                direction_id: d2.id.clone(),
            }],
            rationale: None,
        };
        assert_eq!(
            AiSink::precheck(&store, Capability::Classify, std::slice::from_ref(&draft2)).await,
            vec![true]
        );
        abandon_direction(&store, &d2.id).await;
        assert_eq!(
            AiSink::precheck(&store, Capability::Classify, std::slice::from_ref(&draft2)).await,
            vec![false]
        );
    }

    /// `allowed_ops(Classify)` — J22's classify half: only
    /// `AssignTaskDirection` may be submitted under this capability.
    #[tokio::test]
    async fn allowed_ops_classify_rejects_other_op_kinds() {
        let store = Sin90Store::open_memory().await.unwrap();
        // `CreateArea` is otherwise perfectly valid (non-blank title, no
        // relational precondition) — chosen SPECIFICALLY so `allowed_ops` is
        // the ONLY thing standing between this draft and a successful
        // `submit`. An op that would ALSO fail `validate`/`apply_op` on its
        // own (e.g. an empty `ReorderTasks.order`) would make this test pass
        // for the wrong reason and stay green even if `allowed_ops` were
        // mistakenly opened up to every op — the mutation below is exactly
        // how that was caught during T5.2.1's own review.
        let draft = ProposalDraft {
            id: "p-wrong-op".into(),
            ops: vec![Sin90Op::CreateArea {
                title: "Somewhere".into(),
            }],
            rationale: None,
        };
        let rec = crate::ai::AiCallRecord {
            id: "call-wrong-op".into(),
            run_id: "run-x".into(),
            task_kind: Capability::Classify,
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
        let err = AiSink::submit(&store, Capability::Classify, draft, rec)
            .await
            .unwrap_err();
        assert!(matches!(err, SinkError::Invalid(_)));
        let areas: i64 = sqlx::query_scalar("SELECT count(*) FROM sin90_areas")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(areas, 0, "the rejected op must not have been applied");
    }

    /// `NoModelPort` proves it type-checks against `run_classify`'s generic
    /// `M` with `model: None` and is never actually invoked — the same shape
    /// `POST /ai/classify`'s handler uses in production (T5.1.2: no real
    /// adapter wired yet).
    #[tokio::test]
    async fn run_classify_with_no_model_port_still_uses_reflex() {
        let store = Sin90Store::open_memory().await.unwrap();
        let direction = store
            .create_direction("Work", "2026-Q4", None)
            .await
            .unwrap();
        store
            .create_task(
                "Write the memo",
                Some(&direction.id),
                None,
                TaskKind::Other,
                Energy::Mid,
                None,
            )
            .await
            .unwrap();
        let task = inbox_task(&store, "write the memo").await; // same normalized title
        let reader = store.ai_reader();
        let model: Option<&crate::ai::NoModelPort> = None;
        let items = run_classify(
            "run-no-model",
            std::slice::from_ref(&task),
            ModelAccess::LocalOnly,
            model,
            &store,
            &reader,
        )
        .await;
        assert!(matches!(items[0].result, ItemResult::Proposed(_)));
    }
}
