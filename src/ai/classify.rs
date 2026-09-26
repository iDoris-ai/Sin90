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

use crate::core::{DirectionId, Sin90Op, Task, TaskId};

use super::ladder::{plan, run_item, Outcome, RunState, Step};
use super::ports::{
    AiReadModel, AiSink, Capability, Complexity, DirectionCandidate, Engine, ModelAccess,
    ModelMessage, ModelPort, ModelReply, ModelRequest, ProposalDraft, Role, SinkError,
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
        return Ok(ClassifyDecision {
            direction_id: None,
            reason: parsed.reason,
            low_confidence: false,
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

/// One task through the full classify ladder (§11.4.1): §11.4's public
/// "候选为空" short circuit happens here — with zero non-terminal Directions
/// in the whole system, R1 can never be decisive either (its own history is
/// filtered to non-terminal Directions too), so skipping the ladder entirely
/// writes no call row and produces no proposal, exactly as designed.
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
        return (ItemResult::Nothing, None);
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

    match outcome {
        Outcome::Produced { value, engine, rec } => {
            // T5.2.3: read BEFORE `value.direction_id` is matched on below —
            // that match only moves the `direction_id` field out of `value`
            // (a partial move), so `value.low_confidence` stays reachable,
            // but reading it up front keeps the arms below from having to
            // care about field-move ordering at all.
            let low_confidence = value.low_confidence;
            let confidence = value.confidence;
            let choice = value.choice.clone();
            let (result, reason) = match value.direction_id {
                Some(direction_id) => {
                    let draft = ProposalDraft {
                        id: format!("ai-classify-{}", crate::core::ulid()),
                        ops: vec![Sin90Op::AssignTaskDirection {
                            task_id: task.id.clone(),
                            direction_id,
                        }],
                        rationale: Some(build_rationale(engine, &value.reason)),
                    };
                    let draft_id = draft.id.clone();
                    // 2026-09-24 review (M1): a decision that fails `submit`'s
                    // dry run (state moved between planning and submit — e.g. the
                    // task got classified by something else in the same run's
                    // window) is not a dropped call — it still gets a row, via
                    // `record_call`, since `submit` never wrote one. `rec` is
                    // cloned BEFORE `submit` consumes it so there is something
                    // left to record on the error path (§11.3.5: the model/rule
                    // did its job; the STATE changed, not the call).
                    let rec_on_failure = rec.clone();
                    match sink.submit(Capability::Classify, draft, rec).await {
                        Ok(()) => (ItemResult::Proposed(draft_id), None),
                        Err(e) => {
                            tracing::warn!(error = %e, task_id = %task.id, "classify: a decision failed submit's dry run (state moved)");
                            let mut failed = rec_on_failure;
                            failed.ok = false;
                            failed.proposal_id = None;
                            failed.error_kind = Some(match &e {
                                // §11.3.5's own name for this case.
                                SinkError::Invalid(_) => "rejected_by_precheck",
                                // An infra failure says nothing about the
                                // decision's validity — a distinct kind so it is
                                // never confused with a real precheck rejection.
                                SinkError::Store(_) => "submit_store_error",
                            });
                            if let Err(record_err) = sink.record_call(failed).await {
                                tracing::warn!(error = %record_err, task_id = %task.id, "classify: failed to record a rejected-at-submit call (R6, not fatal)");
                            }
                            (ItemResult::Rejected, None)
                        }
                    }
                }
                None => {
                    // A decisive non-match is still `ok = 1` (§11.4.1) — record
                    // it as a plain call, not a proposal.
                    if let Err(e) = sink.record_call(rec).await {
                        tracing::warn!(error = %e, task_id = %task.id, "classify: failed to record a decisive no-match call (R6, not fatal)");
                    }
                    // T5.2.3: tag WHY this is `Nothing` only when it was the
                    // confidence check that produced it — `choice == "none"`
                    // (or R1/R2/no-candidates/bad_output-with-nothing-left)
                    // all still fold into a bare `Nothing` with no reason.
                    let reason = low_confidence.then_some("low_confidence");
                    (ItemResult::Nothing, reason)
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
            (result, reason)
        }
        Outcome::Nothing => (ItemResult::Nothing, None),
        Outcome::Deferred => (ItemResult::Deferred, None),
        Outcome::Aborted => (ItemResult::Aborted, None),
    }
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

    /// J12: an invented/real-id key degrades (bad_output) past the single
    /// `local` model step to R2, which also has nothing to match — the item
    /// ends `Nothing`; a call row for the model step is recorded via
    /// `record_call`. Positive control: a valid key on the same fixture
    /// produces a proposal.
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

        let calls: Vec<(String, bool, Option<String>)> = sqlx::query_as(
            "SELECT engine, ok, error_kind FROM sin90_ai_calls WHERE run_id = 'run-classify-2'",
        )
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert!(calls.iter().any(|(engine, ok, kind)| engine == "local"
            && !ok
            && kind.as_deref() == Some("bad_output")));
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

    /// J27: an explicit `choice == "none"` (the model confidently says
    /// nothing fits) must NOT be tagged `"low_confidence"` even though it
    /// also ends `Nothing` — the two causes are different (§11.4.1: "none"
    /// is a decisive non-match on its own; low confidence is "wasn't sure
    /// enough about a real pick") and only one of them should carry the new
    /// reason. Mutation target: make `classify_one` tag `Nothing` as
    /// `Some("low_confidence")` whenever `direction_id` is `None`, regardless
    /// of `ClassifyDecision::low_confidence` — this test goes red (the
    /// `"none"` case would wrongly get the reason too).
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
        assert_eq!(items[0].result, ItemResult::Nothing);
        assert_eq!(items[0].reason, None);
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

    /// J27: an INVENTED key paired with `confidence: "low"` must still be
    /// `bad_output` (a real parsing/validity failure), NOT quietly reclassified
    /// as a low-confidence `nothing` — the invented-key check in
    /// `parse_classify_reply` runs BEFORE the confidence check specifically so
    /// this combination is never misdiagnosed as "the model looked and wasn't
    /// sure" when it actually named a key that does not exist. Mutation
    /// target: reorder `parse_classify_reply` to check `confidence` before the
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
            items[0].reason, None,
            "an invented key degrades via bad_output, not the low-confidence path"
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
    /// Positive control: when `local` ALSO fails, the item ends `Nothing`
    /// (R2 has nothing to match "unrelated task" against) and the `local`
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
        assert_eq!(items2[0].result, ItemResult::Nothing);
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
