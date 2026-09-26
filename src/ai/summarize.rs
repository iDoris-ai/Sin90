//! `summarize` (T5.3.1, design §11.4.2): drafts a weekly Review's `body` — a
//! program-rendered "本周数字" facts block (numbers straight from T4.3.1's
//! weekly draft, via [`ports::SummarizeDraft`]) plus an AI narrative that may
//! reference those numbers ONLY through `{{fN}}`/`{{tN}}` placeholders the
//! program itself renders, never a digit the model typed.
//!
//! Stacked across three branches (2026-09-26 review): `feat/t5.3.1a-
//! summarize-store` (the store-side plumbing — `weekly_draft_on`,
//! `SummarizeDraft`/`auto_draft_md`, `AiReadModel::{weekly_draft,
//! done_titles}`) → `feat/t5.3.1b-summarize-core` (this file's CORE pure
//! functions, [`facts`] through [`digit_runs`] — the "数字只来自草稿"
//! mechanism itself, ported from the frozen design's scratch crate
//! `t501-check/src/summarize.rs`, §11.12) → `feat/t5.3.1-summarize` (this
//! commit: the request/parse/rationale pieces and the [`run_summarize`]
//! driver that ties the ladder, `AiReadModel`, and `AiSink` together — same
//! shape `ai::classify`/`ai::propose` already established for their own
//! capabilities).
//!
//! `facts()` is the one CORE function whose SHAPE had to change from the
//! scratch signature: this crate's real weekly draft carries raw ids, not
//! resolved titles, and `ai/` cannot look one up itself (§11.5: it may not
//! name `crate::store`). [`ports::AiReadModel::weekly_draft`]'s store-side
//! implementation does that id→title join (and fills in `auto_draft_md`,
//! T4.3.2's own rendering of the SAME draft) and hands back [`ports::
//! SummarizeDraft`] instead — `facts()` here takes that directly, with no
//! `title_of` closure (the scratch signature `facts(draft, title_of)` is not
//! reachable from this module at all).

use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::core::{body_sha256, Review, ReviewKind, ReviewStatus, Sin90Op};

use super::ladder::{plan, run_item, Outcome, RunState};
use super::ports::{
    AiReadModel, AiSink, Capability, Complexity, Engine, ModelAccess, ModelMessage, ModelPort,
    ModelReply, ModelRequest, ProposalDraft, Role, SinkError, SummarizeDraft,
};

// ==================================================================== facts

pub const FACTS_HEADING_WORD: &str = "本周数字";

/// Label for the "no direction"/"no area" bucket — [`ports::SummarizeBucket
/// ::label`] is `None` ONLY for it (a non-empty id whose title lookup missed
/// falls back to the raw id instead, `AiReader::weekly_draft`'s own doc,
/// 2026-09-26 review Low).
const UNASSIGNED_LABEL: &str = "未分类";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fact {
    pub key: String,
    pub label: String,
    pub value: String,
}

fn hm(minutes: i64) -> String {
    let m = minutes.max(0);
    format!("{} 小时 {} 分钟", m / 60, m % 60)
}

/// §11.4.2's mechanism, step 1: every number in [`SummarizeDraft`] becomes
/// one [`Fact`] — this is the ENTIRE surface through which a number can ever
/// reach the rendered body (via [`render_facts`] directly, or via
/// [`fill_narrative`]'s `{{fN}}` substitution) — nothing else in this module
/// ever formats a number into the body. Labels are passed through
/// [`sanitize_inline`] (they came from user-editable Area/Direction/Routine
/// titles) before being embedded in a Chinese-bracket label — a title
/// containing `「」` or control characters must not be able to forge what
/// looks like a second fact.
#[must_use]
pub fn facts(d: &SummarizeDraft) -> Vec<Fact> {
    let mut out = Vec::new();
    let mut push = |label: String, value: String| {
        let key = format!("f{}", out.len() + 1);
        out.push(Fact { key, label, value });
    };
    for b in &d.by_area {
        let name = sanitize_inline(b.label.as_deref().unwrap_or(UNASSIGNED_LABEL));
        push(format!("领域「{name}」投入"), hm(b.minutes));
    }
    for b in &d.by_direction {
        let name = sanitize_inline(b.label.as_deref().unwrap_or(UNASSIGNED_LABEL));
        push(format!("方向「{name}」投入"), hm(b.minutes));
    }
    push("完成任务数".to_string(), d.tasks_done.to_string());
    for r in &d.routines {
        let name = sanitize_inline(&r.label);
        push(
            format!("节律「{name}」"),
            format!("触发 {} 次 / 完成 {} 次", r.fired, r.completed),
        );
    }
    out
}

#[must_use]
pub fn render_unit(f: &Fact) -> String {
    format!("〔{}：{}〕", f.label, f.value)
}

/// §11.4.2's facts block — COMPLETELY program-rendered, byte for byte a
/// function of `fs` (itself a pure function of [`SummarizeDraft`]). This is
/// one of the TWO program renderings [`is_program_only`] compares the
/// current body against (§11.4.2's revised "可改写条件" ①; ② is
/// [`SummarizeDraft::auto_draft_md`], T4.3.2's own rendering of the SAME
/// draft).
#[must_use]
pub fn render_facts(week: &str, fs: &[Fact]) -> String {
    let mut s = format!("## {week} {FACTS_HEADING_WORD}\n\n");
    for f in fs {
        s.push_str(&format!("- {}：{}\n", f.label, f.value));
    }
    s
}

// ------------------------------------------------------------ normalization

/// Unicode general category Cf (format), as code-point ranges (Unicode 15).
/// The FULL table (unlike `ai::classify::is_cf_format_char`'s hand-picked
/// subset for a plain-text `rationale` — that function's own doc names this
/// module's full table as T5.3.1's deliverable, design §11.4.2's `normalize`
/// step).
const CF_RANGES: &[(u32, u32)] = &[
    (0x00AD, 0x00AD),
    (0x0600, 0x0605),
    (0x061C, 0x061C),
    (0x06DD, 0x06DD),
    (0x070F, 0x070F),
    (0x0890, 0x0891),
    (0x08E2, 0x08E2),
    (0x180E, 0x180E),
    (0x200B, 0x200F),
    (0x202A, 0x202E),
    (0x2060, 0x2064),
    (0x2066, 0x206F),
    (0xFEFF, 0xFEFF),
    (0xFFF9, 0xFFFB),
    (0x110BD, 0x110BD),
    (0x110CD, 0x110CD),
    (0x13430, 0x1343F),
    (0x1BCA0, 0x1BCA3),
    (0x1D173, 0x1D17A),
    (0xE0001, 0xE0001),
    (0xE0020, 0xE007F),
];

#[must_use]
pub fn is_format_char(c: char) -> bool {
    let u = c as u32;
    CF_RANGES.iter().any(|&(a, b)| (a..=b).contains(&u))
}

/// Controls that may never appear: every Cc except `\n`, plus the Unicode
/// line/paragraph separators (CommonMark and many renderers break lines on a
/// lone `\r`; Rust's `lines()` does not — the mismatch is the bypass).
#[must_use]
pub fn is_forbidden_control(c: char) -> bool {
    (c.is_control() && c != '\n') || c == '\u{2028}' || c == '\u{2029}'
}

/// Delete Cf, collapse every run of non-`\n` whitespace to one ASCII space.
#[must_use]
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars().filter(|c| !is_format_char(*c)) {
        if c != '\n' && c.is_whitespace() {
            if !in_ws {
                out.push(' ');
            }
            in_ws = true;
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

/// Data (area/direction/routine/task titles) rendered into the body: drop
/// forbidden controls and Cf, collapse ALL whitespace (incl. `\n`) to one
/// space, neutralize inline Markdown/HTML and the unit/title brackets.
#[must_use]
pub fn sanitize_inline(s: &str) -> String {
    let flat: String = s
        .chars()
        .filter(|c| !is_forbidden_control(*c) || *c == '\r')
        .map(|c| if c == '\r' { ' ' } else { c })
        .collect();
    let flat = normalize(&flat).replace('\n', " ");
    let mut out = String::new();
    for c in flat.trim().chars() {
        match c {
            '\\' | '`' | '*' | '_' | '[' | ']' | '(' | ')' | '#' | '|' | '~' | '!' => {
                out.push('\\');
                out.push(c);
            }
            '<' => out.push('＜'),
            '>' => out.push('＞'),
            '&' => out.push('＆'),
            '〔' => out.push('［'),
            '〕' => out.push('］'),
            '「' => out.push('『'),
            '」' => out.push('』'),
            _ => out.push(c),
        }
    }
    out
}

// -------------------------------------------------------------- narrative

#[derive(Debug, PartialEq, Eq)]
pub enum NarrativeError {
    ForbiddenControl(u32),
    LiteralDigit(char),
    CjkNumeralWithMeasure(String),
    MeasureAfterUnit(String),
    /// `〔〕「」` typed by the model — only the program may produce them.
    ReservedBracket(char),
    UnknownPlaceholder(String),
    MentionsFactsHeading,
    Unterminated,
    TooLong,
    Empty,
}

pub const MAX_NARRATIVE_CHARS: usize = 2000;
/// 2026-09-26 review (M1): adds the traditional variants 兩/貳/參/陸/億 and
/// the colloquial tens 廿/卅/卌 (twenty/thirty/forty) to the simplified set
/// design §11.4.2's own table already had — a model is not guaranteed to
/// write simplified Chinese, and none of these are rare enough to ignore.
const CJK_NUMERALS: &[char] = &[
    '〇', '零', '一', '二', '两', '三', '四', '五', '六', '七', '八', '九', '十', '百', '千', '万',
    '亿', '壹', '贰', '叁', '肆', '伍', '陆', '柒', '捌', '玖', '拾', '佰', '仟', '萬', '兩', '貳',
    '參', '陸', '億', '廿', '卅', '卌',
];
/// The rule's exact claim: no CJK-numeral run followed (after at most one
/// space, post-normalization) by one of THESE.
pub const MEASURE: &[&str] = &[
    "小时", "分钟", "个", "件", "次", "项", "天", "周", "%", "％", "倍", "成",
];
const RESERVED: &[char] = &['〔', '〕', '「', '」'];

fn measure_follows(rest: &str) -> Option<String> {
    let r = rest.strip_prefix(' ').unwrap_or(rest);
    MEASURE
        .iter()
        .find(|m| r.starts_with(**m))
        .map(|m| (*m).to_string())
}

fn check_literal(seg: &str) -> Result<(), NarrativeError> {
    if let Some(c) = seg.chars().find(|c| c.is_numeric()) {
        return Err(NarrativeError::LiteralDigit(c));
    }
    if let Some(c) = seg.chars().find(|c| RESERVED.contains(c)) {
        return Err(NarrativeError::ReservedBracket(c));
    }
    let chars: Vec<char> = seg.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if CJK_NUMERALS.contains(&chars[i]) {
            let start = i;
            while i < chars.len() && CJK_NUMERALS.contains(&chars[i]) {
                i += 1;
            }
            let rest: String = chars[i..].iter().take(3).collect();
            if let Some(m) = measure_follows(&rest) {
                return Err(NarrativeError::CjkNumeralWithMeasure(
                    chars[start..i].iter().collect::<String>() + &m,
                ));
            }
        } else {
            i += 1;
        }
    }
    Ok(())
}

/// `fN` / `tN` with N = ASCII digits, no sign, no leading zero, N ≥ 1.
fn parse_key(key: &str) -> Option<(char, usize)> {
    let mut cs = key.chars();
    let kind = cs.next()?;
    let num = cs.as_str();
    if !(kind == 'f' || kind == 't')
        || num.is_empty()
        || num.starts_with('0')
        || !num.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    num.parse().ok().map(|n| (kind, n))
}

fn plain_line(l: &str) -> String {
    let mut t = l.trim_start();
    loop {
        let before = t;
        t = t.trim_start_matches(['#', '-', '*', '+', '>', '|', '`', '~', '=', '_']);
        t = t.trim_start();
        if t == before {
            break;
        }
    }
    t.replace('<', "＜").replace('&', "＆")
}

/// Validate + render the narrative. `titles[i]` backs `{{t(i+1)}}`.
pub fn fill_narrative(
    narr: &str,
    fs: &[Fact],
    titles: &[String],
) -> Result<String, NarrativeError> {
    if let Some(c) = narr.chars().find(|c| is_forbidden_control(*c)) {
        return Err(NarrativeError::ForbiddenControl(c as u32));
    }
    let narr = normalize(narr);
    if narr.trim().is_empty() {
        return Err(NarrativeError::Empty);
    }
    if narr.chars().count() > MAX_NARRATIVE_CHARS {
        return Err(NarrativeError::TooLong);
    }
    let squashed: String = narr.chars().filter(|c| !c.is_whitespace()).collect();
    if squashed.contains(FACTS_HEADING_WORD) {
        return Err(NarrativeError::MentionsFactsHeading);
    }
    let mut out = String::new();
    let mut rest = narr.as_str();
    while let Some(open) = rest.find("{{") {
        let (lit, after) = rest.split_at(open);
        check_literal(lit)?;
        out.push_str(lit);
        let after = &after[2..];
        let close = after.find("}}").ok_or(NarrativeError::Unterminated)?;
        let key = &after[..close];
        let rendered = match parse_key(key) {
            Some(('f', _)) => fs.iter().find(|f| f.key == key).map(render_unit),
            Some(('t', n)) => titles
                .get(n - 1)
                .map(|t| format!("「{}」", sanitize_inline(t))),
            _ => None,
        }
        .ok_or_else(|| NarrativeError::UnknownPlaceholder(key.to_string()))?;
        rest = &after[close + 2..];
        if key.starts_with('f') {
            if let Some(m) = measure_follows(rest) {
                return Err(NarrativeError::MeasureAfterUnit(format!(
                    "{{{{{key}}}}}{m}"
                )));
            }
        }
        out.push_str(&rendered);
    }
    check_literal(rest)?;
    out.push_str(rest);
    let paras: Vec<String> = out
        .split('\n')
        .map(plain_line)
        .filter(|l| !l.is_empty())
        .collect();
    Ok(paras.join("\n\n"))
}

#[must_use]
pub fn compose_body(facts_md: &str, narrative: Option<&str>) -> String {
    match narrative {
        Some(n) => format!("{facts_md}\n## 叙述\n\n{n}\n"),
        None => facts_md.to_string(),
    }
}

/// Q7 (design §11.4.2's "可改写条件", REVISED 2026-09-26 review C1): the
/// current body, after trimming TRAILING whitespace, is either empty or
/// EXACTLY equal (candidates trimmed the same way) to ONE of `candidates` —
/// no fuzzy matching, one extra/missing/changed character on EITHER
/// candidate counts as human text. `candidates` is always `[render_facts(当前
/// 草稿), SummarizeDraft::auto_draft_md]` at the call site (Layer C,
/// `run_summarize`) — ① `ai::summarize`'s own facts block, ② T4.3.2's
/// `render_weekly_draft_markdown` rendering of the SAME draft (the review's
/// own C1 finding: a review auto-created by a Routine firing starts out
/// equal to ②, never ①, so recognizing only ① meant summarize could NEVER
/// touch an auto-created draft). Taking a slice rather than exactly two
/// `&str` params keeps this function honest about not caring how many
/// program rendering candidates exist, or in what order.
#[must_use]
pub fn is_program_only(body: &str, candidates: &[&str]) -> bool {
    let trimmed = body.trim_end();
    trimmed.is_empty() || candidates.iter().any(|c| c.trim_end() == trimmed)
}

#[must_use]
pub fn digit_runs(s: &str) -> Vec<String> {
    let mut v = Vec::new();
    let mut cur = String::new();
    for c in s.chars() {
        if c.is_numeric() {
            cur.push(c);
        } else if !cur.is_empty() {
            v.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        v.push(cur);
    }
    v
}

// ---------------------------------------------------------------- model step

const SUMMARIZE_SYSTEM_PROMPT: &str = "You are writing the narrative paragraph(s) of a weekly \
review. Never type a digit yourself, including Chinese numerals — to reference a number, use the \
placeholder {{fN}} exactly as given; to reference a completed task, use {{tN}}. Never type the \
brackets 〔〕「」 yourself, only the program may render them. Do not use headings, lists, tables, \
quotes, or HTML — plain paragraphs only. Avoid \"Chinese-numeral + measure-word\" phrasing like \
\"这一周\"/\"一个\"/\"一次\" even for everyday counts that are not from the facts — prefer \"本周\"/\
\"某个\"/\"再次\" instead. Respond with JSON only, matching the given schema.";

/// `response_format`'s JSON schema (§11.4.2): `{narrative: string ≤ 2000}`,
/// `additionalProperties: false`. The `maxLength` here is a hint a local
/// model may not honor — [`fill_narrative`] re-checks [`MAX_NARRATIVE_CHARS`]
/// itself regardless (same posture `ai::propose`'s own H2/L2 note gives its
/// new-task title length check).
#[must_use]
pub fn summarize_schema() -> Map<String, Value> {
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "narrative": {"type": "string", "maxLength": MAX_NARRATIVE_CHARS}
        },
        "required": ["narrative"]
    });
    match schema {
        Value::Object(m) => m,
        _ => unreachable!("json!({{...}}) always builds a Value::Object"),
    }
}

#[must_use]
pub fn build_summarize_request(fs: &[Fact], titles: &[String], engine: Engine) -> ModelRequest {
    let schema = summarize_schema();
    let facts_json: Vec<Value> = fs
        .iter()
        .map(|f| json!({"key": f.key, "label": f.label, "value": f.value}))
        .collect();
    let titles_json: Vec<Value> = titles
        .iter()
        .enumerate()
        .map(|(i, t)| json!({"key": format!("t{}", i + 1), "title": t}))
        .collect();
    let user = json!({"facts": facts_json, "tasks": titles_json}).to_string();
    ModelRequest {
        messages: vec![
            ModelMessage {
                role: Role::System,
                content: SUMMARIZE_SYSTEM_PROMPT.to_string(),
            },
            ModelMessage {
                role: Role::User,
                content: user,
            },
        ],
        schema_name: "sin90_summarize",
        schema,
        max_tokens: 1024,
        complexity: if engine == Engine::Executive {
            Complexity::Complex
        } else {
            Complexity::Simple
        },
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelNarrative {
    narrative: String,
}

/// Tolerates ONE layer of a ```` ```json ```` (or bare ```` ``` ````) fence —
/// same convention `ai::classify::strip_json_fence` uses, kept as its own
/// tiny copy (design §11.4.2's own precedent for `ai::propose::clean_ai_text`
/// vs. `is_cf_format_char`: a three-line fence-stripper is not worth sharing
/// across files, unlike a Unicode table).
fn strip_json_fence(s: &str) -> &str {
    let t = s.trim();
    for prefix in ["```json", "```"] {
        if let Some(rest) = t.strip_prefix(prefix) {
            return rest.strip_suffix("```").unwrap_or(rest).trim();
        }
    }
    t
}

/// The program's recheck of the model's reply (§11.4.2): malformed JSON, an
/// unknown field, or a narrative [`fill_narrative`] rejects all map to the
/// SAME `Err("bad_output")` — `run_item` records that as a degrade, not a
/// crash (mirrors `ai::classify::parse_classify_reply`'s own posture).
pub fn parse_summarize_reply(
    text: &str,
    fs: &[Fact],
    titles: &[String],
) -> Result<Option<String>, &'static str> {
    let parsed: ModelNarrative =
        serde_json::from_str(strip_json_fence(text)).map_err(|_| "bad_output")?;
    fill_narrative(&parsed.narrative, fs, titles)
        .map(Some)
        .map_err(|_e| "bad_output")
}

// ---------------------------------------------------------------- rationale

fn build_rationale(engine: Engine, produced_narrative: bool) -> String {
    let reason = if produced_narrative {
        "drafted this week's review body (facts block + narrative) from the current weekly draft"
    } else {
        "drafted this week's review body (facts block only) from the current weekly draft"
    };
    format!("{}: {}", engine.as_str(), reason)
}

// ---------------------------------------------------------------- input

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummarizeInputError {
    UnknownReview(String),
    /// §11.4.2's "输入": only `kind = weekly` is supported — `daily`/`rhythm`
    /// is a client mistake, not "nothing to summarize this week".
    UnsupportedKind(ReviewKind),
    NotDraft(String, ReviewStatus),
    ReadFailed(String),
}

/// Resolves and validates the target Review (§11.4.2's "输入") — lives here,
/// not the HTTP layer, same rationale `classify::select_targets`/
/// `propose::select_week` give: unit-testable against a fake `AiReadModel`
/// without a server, and so `POST /ai/summarize`'s handler stays a thin
/// wrapper.
pub async fn select_review<R: AiReadModel>(
    read: &R,
    review_id: &str,
) -> Result<Review, SummarizeInputError> {
    let review = read
        .review(review_id)
        .await
        .map_err(|e| SummarizeInputError::ReadFailed(e.to_string()))?
        .ok_or_else(|| SummarizeInputError::UnknownReview(review_id.to_string()))?;
    if review.kind != ReviewKind::Weekly {
        return Err(SummarizeInputError::UnsupportedKind(review.kind));
    }
    if review.status != ReviewStatus::Draft {
        return Err(SummarizeInputError::NotDraft(
            review.id.clone(),
            review.status,
        ));
    }
    Ok(review)
}

// ---------------------------------------------------------------- run driver

/// §11.4 公共's `items: [{target, result}]` shape for summarize — the HTTP
/// layer maps this to that wire vocabulary (mirrors `classify::ItemResult`/
/// `propose::ProposeItemResult`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummarizeItemResult {
    /// A proposal was submitted; the id is `sin90_proposals.id`.
    Proposed(String),
    /// A decision was reached but the composed body is byte-identical to the
    /// current one (§11.4.2's "比较并交换") — nothing to propose.
    Nothing,
    /// Capacity/budget/deadline hit — left for a later run.
    Deferred,
    /// A produced decision failed `AiSink::submit`'s dry run (state moved —
    /// e.g. the review was finalized in the same run's window).
    Rejected,
    /// The run aborted (§11.3.4) before or during this item.
    Aborted,
    /// Q7/J19 (§11.4.2's "可改写条件"): the current body is not empty and
    /// does not exactly match ANY of the program's own renderings (①
    /// `render_facts`, ② `SummarizeDraft::auto_draft_md`) — this run never
    /// even called the ladder, let alone the model.
    Skipped,
    /// 2026-09-26 review (Low): `AiReadModel::weekly_draft` itself failed —
    /// distinguishable from [`Self::Nothing`] (a real decision that had
    /// nothing to add) and [`Self::Skipped`] (Q7's human-text gate): this
    /// run never even got the numbers to decide anything. Carries the
    /// read model's own error message for logging/diagnosis. Maps to the
    /// wire's `"aborted"` (the closed `proposed|nothing|deferred|rejected|
    /// skipped|aborted` vocabulary has no room for a new word; "this run
    /// produced nothing because something stopped it" is `aborted`'s own
    /// meaning) — HTTP layer, `item_result_str`.
    ReadFailed(String),
}

/// Runs the whole summarize capability for `review` (§11.4.2). `review` is
/// assumed already validated `kind = weekly, status = draft` by
/// [`select_review`] — this function does not re-check either.
pub async fn run_summarize<M, S, R>(
    run_id: &str,
    review: &Review,
    access: ModelAccess,
    model: Option<&M>,
    sink: &S,
    read: &R,
) -> SummarizeItemResult
where
    M: ModelPort,
    S: AiSink,
    R: AiReadModel,
{
    let draft = match read.weekly_draft(&review.period).await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, review_id = %review.id, "summarize: weekly_draft read failed");
            return SummarizeItemResult::ReadFailed(e.to_string());
        }
    };
    let fs = facts(&draft);
    let facts_md = render_facts(&draft.week, &fs);

    // Q7/§11.4.2's "可改写条件" (J19, REVISED 2026-09-26 review C1): checked
    // BEFORE the ladder ever runs — human text under (or instead of) EITHER
    // program rendering means this run attempts nothing at all, not even a
    // reflex-only rewrite. `candidates` is BOTH program renderings of this
    // SAME draft: `ai::summarize`'s own facts block, and T4.3.2's
    // `render_weekly_draft_markdown` output (`auto_draft_md`) — a review
    // auto-created by a Routine firing starts out equal to the SECOND one,
    // never the first, so checking only `facts_md` (the pre-review shape)
    // meant summarize could never touch an auto-created draft at all.
    if !is_program_only(&review.body, &[&facts_md, &draft.auto_draft_md]) {
        return SummarizeItemResult::Skipped;
    }

    let titles = read.done_titles(&review.period).await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, review_id = %review.id, "summarize: done_titles read failed, treating as empty");
        Vec::new()
    });
    let base_body_sha256 = body_sha256(&review.body);

    let settings = read.settings().await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "summarize: settings read failed, defaulting to executive disabled");
        Default::default()
    });
    let steps = plan(Capability::Summarize, access, settings, model.is_some());
    let mut st = RunState::new(std::time::Instant::now());

    let fs_for_build = fs.clone();
    let titles_for_build = titles.clone();
    let fs_for_parse = fs.clone();
    let titles_for_parse = titles.clone();
    let outcome: Outcome<Option<String>> = run_item(
        run_id,
        Capability::Summarize,
        &steps,
        &mut st,
        model,
        sink,
        read,
        move |engine| build_summarize_request(&fs_for_build, &titles_for_build, engine),
        move |reply: &ModelReply| {
            parse_summarize_reply(&reply.text, &fs_for_parse, &titles_for_parse)
        },
        || None, // summarize has no ReflexDecisive step (§11.3.3)
        // L3-equivalent (design §11.4.2/§11.3.3): reflex ALWAYS has an
        // answer for summarize — a facts-only body (no narrative) is a
        // real, completed decision, not "couldn't decide". `Some(None)`:
        // the OUTER `Some` means "decisive", the inner `None` means "no
        // narrative" (mirrors `ai::propose`'s own `Some(ProposeDecision {
        // .. })` always-decisive reflex fallback).
        || Some(None::<String>),
        crate::core::now_iso8601,
        crate::core::ulid,
    )
    .await;

    match outcome {
        Outcome::Produced { value, engine, rec } => {
            let produced_narrative = value.is_some();
            let body = compose_body(&facts_md, value.as_deref());
            let new_hash = body_sha256(&body);
            if new_hash == base_body_sha256 {
                // §11.4.2's "比较并交换": identical body -> no proposal, but
                // the decision itself was real (`ok = 1`, R6's own posture —
                // `run_item` never writes a `Produced` row itself, §11.3.5's
                // atomicity, so this IS the one place it gets recorded).
                if let Err(e) = sink.record_call(rec).await {
                    tracing::warn!(error = %e, run_id = %run_id, "summarize: failed to record a no-op decision (R6, not fatal)");
                }
                return SummarizeItemResult::Nothing;
            }
            let draft_op = ProposalDraft {
                id: format!("ai-summarize-{}", crate::core::ulid()),
                ops: vec![Sin90Op::DraftReviewBody {
                    review_id: review.id.clone(),
                    base_body_sha256: base_body_sha256.clone(),
                    body,
                }],
                rationale: Some(build_rationale(engine, produced_narrative)),
            };
            let draft_id = draft_op.id.clone();
            // M1-style (mirrors classify/propose's own posture): `rec` is
            // cloned BEFORE `submit` consumes it so there is something left
            // to record on the error path.
            let rec_on_failure = rec.clone();
            match sink.submit(Capability::Summarize, draft_op, rec).await {
                Ok(()) => SummarizeItemResult::Proposed(draft_id),
                Err(e) => {
                    tracing::warn!(error = %e, review_id = %review.id, "summarize: a decision failed submit's dry run (state moved)");
                    let mut failed = rec_on_failure;
                    failed.ok = false;
                    failed.proposal_id = None;
                    failed.error_kind = Some(match &e {
                        SinkError::Invalid(_) => "rejected_by_precheck",
                        SinkError::Store(_) => "submit_store_error",
                    });
                    if let Err(record_err) = sink.record_call(failed).await {
                        tracing::warn!(error = %record_err, review_id = %review.id, "summarize: failed to record a rejected-at-submit call (R6, not fatal)");
                    }
                    SummarizeItemResult::Rejected
                }
            }
        }
        // Unreachable in practice (the reflex fallback above is always
        // `Some`, so `run_item` never falls through to here) — kept so this
        // `match` stays exhaustive against `Outcome<T>`'s real shape rather
        // than assuming the always-decisive property holds forever.
        Outcome::Nothing { .. } => SummarizeItemResult::Nothing,
        Outcome::Deferred => SummarizeItemResult::Deferred,
        Outcome::Aborted => SummarizeItemResult::Aborted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::ports::{SummarizeBucket, SummarizeRoutineRow};

    fn draft() -> SummarizeDraft {
        SummarizeDraft {
            week: "2026-W39".into(),
            by_area: vec![
                SummarizeBucket {
                    label: Some("Coding".into()),
                    minutes: 18 * 60,
                },
                SummarizeBucket {
                    label: Some("Web3 业务".into()),
                    minutes: 120,
                },
            ],
            by_direction: vec![],
            tasks_done: 7,
            routines: vec![SummarizeRoutineRow {
                label: "r1".into(),
                fired: 3,
                completed: 2,
            }],
            auto_draft_md: String::new(),
        }
    }
    fn fs() -> Vec<Fact> {
        facts(&draft())
    }
    fn titles() -> Vec<String> {
        vec![
            "发布 v0.5".into(),
            "修 bug\r## 伪造标题".into(),
            "**加粗** [链接](http://e.x) <b>x</b> 〔假：十八小时〕".into(),
        ]
    }
    fn run(s: &str) -> Result<String, NarrativeError> {
        fill_narrative(s, &fs(), &titles())
    }

    /// Removes every `〔...〕` unit substring — no nesting is possible
    /// (`render_unit` never emits one), so a simple in/out toggle suffices.
    /// Test-only: used by `summarize_a_value_never_appears_outside_its_own_unit`
    /// to check what's LEFT after every legitimate unit is stripped away.
    fn strip_units(s: &str) -> String {
        let mut out = String::new();
        let mut in_unit = false;
        for c in s.chars() {
            match c {
                '〔' => in_unit = true,
                '〕' => in_unit = false,
                _ if !in_unit => out.push(c),
                _ => {}
            }
        }
        out
    }

    // ---- facts()/render_facts(): §11.4.2's "数字只来自草稿" mechanism, step 1

    #[test]
    fn summarize_facts_come_from_the_draft_verbatim() {
        let md = render_facts("2026-W39", &fs());
        assert!(md.contains("领域「Coding」投入：18 小时 0 分钟"));
        assert!(md.contains("完成任务数：7"));
        assert!(md.contains("触发 3 次 / 完成 2 次"));
    }

    /// The "no area/direction" bucket renders as `UNASSIGNED_LABEL`, not an
    /// empty pair of quotes — a label an attacker (or a coincidence) could
    /// otherwise make collide with a real title.
    #[test]
    fn summarize_unassigned_bucket_uses_the_fixed_label_not_an_empty_title() {
        let d = SummarizeDraft {
            week: "2026-W40".into(),
            by_area: vec![SummarizeBucket {
                label: None,
                minutes: 30,
            }],
            by_direction: vec![],
            tasks_done: 0,
            routines: vec![],
            auto_draft_md: String::new(),
        };
        let f = facts(&d);
        assert_eq!(f[0].label, format!("领域「{UNASSIGNED_LABEL}」投入"));
    }

    /// A label sourced from a user-editable title cannot forge a fact unit —
    /// `sanitize_inline` neutralizes `「」`/control chars in the LABEL itself,
    /// same as it does for a task title substituted via `{{tN}}`.
    #[test]
    fn summarize_facts_labels_are_sanitized() {
        let d = SummarizeDraft {
            week: "2026-W40".into(),
            by_area: vec![SummarizeBucket {
                label: Some("恶意「伪造」\r标题".into()),
                minutes: 10,
            }],
            by_direction: vec![],
            tasks_done: 0,
            routines: vec![],
            auto_draft_md: String::new(),
        };
        let f = facts(&d);
        assert!(f[0].label.starts_with("领域「"), "{}", f[0].label);
        assert!(!f[0].label.contains('\r'), "{}", f[0].label);
        // The title's OWN fake corner brackets became full-width ones — they
        // can no longer be confused with the real wrapping brackets around
        // the whole label.
        assert!(f[0].label.contains("『伪造』"), "{}", f[0].label);
        assert!(!f[0].label.contains("「伪造」"), "{}", f[0].label);
    }

    /// 2026-09-26 review (Low, stronger than the original "!contains(\"达到2\")"
    /// which only ruled out ONE specific adjacent phrasing): after stripping
    /// EVERY legitimate `〔...〕` unit out of the rendered narrative, f2's raw
    /// VALUE must not appear anywhere in what's left — the value only ever
    /// reaches the body wrapped in its own unit, never bare.
    #[test]
    fn summarize_a_value_never_appears_outside_its_own_unit() {
        let n = run("编码投入达到{{f2}}，效果拔群。").unwrap();
        let f2 = &fs()[1];
        assert!(n.contains(&render_unit(f2)), "{n}");
        let stripped = strip_units(&n);
        assert!(
            !stripped.contains(f2.value.as_str()),
            "f2's value leaked outside its own unit: stripped={stripped:?} full={n:?}"
        );
    }

    // ---- J17: round-2 bypasses, each must be rejected (negative controls) --

    /// v2.1 H1: every bypass the round-2 probe found, each must be rejected.
    #[test]
    fn summarize_round2_bypasses_are_rejected() {
        use NarrativeError::*;
        type Expect = fn(&NarrativeError) -> bool;
        let cases: &[(&str, Expect)] = &[
            ("〔领域「Coding」投入：十八 小时〕", |e| {
                matches!(e, ReservedBracket(_))
            }),
            (
                "〔领域「Coding」投入：翻倍〕，〔完成任务数：全部〕",
                |e| matches!(e, ReservedBracket(_)),
            ),
            ("「Coding」很忙", |e| matches!(e, ReservedBracket(_))),
            ("编码投入十八 小时", |e| {
                matches!(e, CjkNumeralWithMeasure(_))
            }),
            ("编码投入十八\u{200B}小时", |e| {
                matches!(e, CjkNumeralWithMeasure(_))
            }),
            ("编码投入十八\u{3000}小时", |e| {
                matches!(e, CjkNumeralWithMeasure(_))
            }),
            ("编码投入拾捌小时", |e| {
                matches!(e, CjkNumeralWithMeasure(_))
            }),
            ("编码投入兩小时", |e| {
                matches!(e, CjkNumeralWithMeasure(_))
            }),
            ("编码投入廿个任务", |e| {
                matches!(e, CjkNumeralWithMeasure(_))
            }),
            ("完成任务{{f3}} 小时", |e| {
                matches!(e, MeasureAfterUnit(_))
            }),
            ("完成任务{{f3}}\u{200B}小时", |e| {
                matches!(e, MeasureAfterUnit(_))
            }),
            ("完成任务{{f3}}小时", |e| {
                matches!(e, MeasureAfterUnit(_))
            }),
            ("本\u{200B}周数字（修正）", |e| {
                matches!(e, MentionsFactsHeading)
            }),
            ("本\u{3000}周数字（修正）", |e| {
                matches!(e, MentionsFactsHeading)
            }),
            ("开头\r## 数据修正\r- 领域：{{f2}}", |e| {
                matches!(e, ForbiddenControl(0x0D))
            }),
            ("开头\u{2028}## 数据修正", |e| {
                matches!(e, ForbiddenControl(0x2028))
            }),
            ("开头\u{2029}## 数据修正", |e| {
                matches!(e, ForbiddenControl(0x2029))
            }),
            ("{{t+1}}", |e| matches!(e, UnknownPlaceholder(_))),
            ("{{t01}}", |e| matches!(e, UnknownPlaceholder(_))),
            ("{{ f1 }}", |e| matches!(e, UnknownPlaceholder(_))),
            ("Ⅻ小时", |e| matches!(e, LiteralDigit(_))),
            ("进度约&frac12;", |e| matches!(e, LiteralDigit(_))),
            ("编码 99 小时", |e| matches!(e, LiteralDigit('9'))),
            ("编码９９小时", |e| matches!(e, LiteralDigit('９'))),
            ("完成了七个任务", |e| {
                matches!(e, CjkNumeralWithMeasure(_))
            }),
            ("{{f99}}", |e| matches!(e, UnknownPlaceholder(_))),
            (
                "## 本周数字（修正）\n- 领域「Coding」投入：{{f2}}",
                |e| matches!(e, MentionsFactsHeading),
            ),
        ];
        for (s, ok) in cases {
            let r = run(s);
            assert!(matches!(&r, Err(e) if ok(e)), "{s:?} => {r:?}");
        }
        // Positive control: the SAME reserved-bracket case with the bracket
        // removed is accepted (proves the assertion above isn't vacuous).
        assert!(run("Coding 很忙").is_ok());
    }

    #[test]
    fn summarize_markdown_and_html_flattened() {
        let n = run("## 修正\n- 领域投入：{{f2}}\n> 引用\n| a | b |\n<h>数据修正</h>").unwrap();
        for line in n.lines().filter(|l| !l.is_empty()) {
            assert!(!line.starts_with(['#', '-', '>', '|', '*', '<']), "{line}");
            assert!(!line.contains('<'), "{line}");
        }
    }

    #[test]
    fn summarize_titles_are_sanitized_when_substituted() {
        let n = run("重点是{{t2}}与{{t3}}。").unwrap();
        assert!(!n.contains('\r'));
        assert_eq!(n.lines().count(), 1, "{n}");
        assert!(n.contains("\\#\\# 伪造标题"));
        assert!(n.contains("\\*\\*加粗\\*\\*"));
        assert!(n.contains("＜b＞"));
        assert!(!n.contains("〔假"));
        let n1 = run("重点是{{t1}}。").unwrap();
        assert_eq!(n1, "重点是「发布 v0.5」。");
    }

    #[test]
    fn summarize_known_false_positives_are_what_m3_says() {
        // M3: these ARE rejected (degrade to reflex); J26 measures how often.
        for s in [
            "这一周推进顺利。",
            "完成了一个重要里程碑。",
            "又一次把方向理清。",
        ] {
            assert!(
                matches!(run(s), Err(NarrativeError::CjkNumeralWithMeasure(_))),
                "{s}"
            );
        }
        assert!(run("一起推进，统一节奏。").is_ok());
    }

    // ---- is_program_only: multi-candidate Q7 gate (2026-09-26 review C1) ---

    #[test]
    fn summarize_is_program_only_accepts_empty_or_any_candidate_exactly() {
        let facts_md = "## 2026-W39 本周数字\n\n- x：y\n";
        let auto_md = "# Weekly Review Draft — 2026-W39\n\n...\n";
        let candidates = [facts_md, auto_md];

        assert!(is_program_only("", &candidates));
        assert!(is_program_only(facts_md, &candidates));
        assert!(is_program_only(auto_md, &candidates));
        // Trailing whitespace on the BODY is tolerated (design: "去尾部空白
        // 后比较").
        assert!(is_program_only(&format!("{auto_md}\n\n"), &candidates));

        // Negative controls: one changed character on EITHER candidate is
        // human text — no fuzzy matching.
        assert!(!is_program_only(&format!("{facts_md}extra"), &candidates));
        assert!(!is_program_only(
            &auto_md.replace("2026", "2027"),
            &[auto_md]
        ));
        assert!(!is_program_only("totally unrelated text", &candidates));
        // Leading whitespace is NOT stripped (only trailing) — a body that
        // starts with whitespace before real content is not "empty".
        assert!(!is_program_only(&format!("  {facts_md}"), &candidates));
    }

    #[test]
    fn summarize_digit_runs_extracts_every_maximal_ascii_digit_span() {
        assert_eq!(digit_runs("a12b345c"), vec!["12", "345"]);
        assert_eq!(digit_runs("no digits here"), Vec::<String>::new());
        assert_eq!(digit_runs("7"), vec!["7"]);
    }

    // ---- parse_summarize_reply: code fence + strict schema -----------------

    #[test]
    fn summarize_parse_reply_tolerates_one_json_fence() {
        let wrapped = "```json\n{\"narrative\": \"重点是{{t1}}。\"}\n```";
        let out = parse_summarize_reply(wrapped, &fs(), &titles()).unwrap();
        assert_eq!(out.as_deref(), Some("重点是「发布 v0.5」。"));
    }

    #[test]
    fn summarize_parse_reply_rejects_unknown_field() {
        let bad = r#"{"narrative": "ok", "extra": 1}"#;
        assert_eq!(
            parse_summarize_reply(bad, &fs(), &titles()),
            Err("bad_output")
        );
    }

    #[test]
    fn summarize_parse_reply_rejects_malformed_json() {
        assert_eq!(
            parse_summarize_reply("not json at all", &fs(), &titles()),
            Err("bad_output")
        );
    }

    // ---- select_review: §11.4.2's "输入" ------------------------------------

    use crate::ai::ports::{AiSettings, ReadError, SettingsRead};
    use crate::core::{Alloc, DirectionId, Task, Week, WeekId};

    #[derive(Default)]
    struct FakeRead {
        review: Option<Review>,
    }
    impl SettingsRead for FakeRead {
        async fn settings(&self) -> Result<AiSettings, ReadError> {
            Ok(AiSettings::default())
        }
    }
    impl AiReadModel for FakeRead {
        async fn inbox(&self, _limit: u32) -> Result<Vec<Task>, ReadError> {
            Ok(Vec::new())
        }
        async fn inbox_task(&self, _id: &str) -> Result<Option<Task>, ReadError> {
            Ok(None)
        }
        async fn direction_candidates(
            &self,
            _limit: u32,
        ) -> Result<Vec<super::super::ports::DirectionCandidate>, ReadError> {
            Ok(Vec::new())
        }
        async fn direction(
            &self,
            _id: &DirectionId,
        ) -> Result<Option<super::super::ports::DirectionCandidate>, ReadError> {
            Ok(None)
        }
        async fn title_history(&self, _normalized: &str) -> Result<Vec<DirectionId>, ReadError> {
            Ok(Vec::new())
        }
        async fn review(&self, _id: &str) -> Result<Option<Review>, ReadError> {
            Ok(self.review.clone())
        }
        async fn week_tasks(&self, _week_id: &WeekId) -> Result<Vec<Task>, ReadError> {
            Ok(Vec::new())
        }
        async fn week(&self, _id: &WeekId) -> Result<Option<Week>, ReadError> {
            Ok(None)
        }
        async fn previous_open_week(&self, _iso_week: &str) -> Result<Option<Week>, ReadError> {
            Ok(None)
        }
        async fn rhythm_alloc(&self) -> Result<Vec<Alloc>, ReadError> {
            Ok(Vec::new())
        }
        async fn weekly_draft(&self, _iso_week: &str) -> Result<SummarizeDraft, ReadError> {
            Ok(SummarizeDraft {
                week: "2026-W39".into(),
                by_area: Vec::new(),
                by_direction: Vec::new(),
                tasks_done: 0,
                routines: Vec::new(),
                auto_draft_md: String::new(),
            })
        }
        async fn done_titles(&self, _iso_week: &str) -> Result<Vec<String>, ReadError> {
            Ok(Vec::new())
        }
    }

    fn weekly_review(status: ReviewStatus, kind: ReviewKind) -> Review {
        Review {
            id: "r1".into(),
            kind,
            status,
            week_id: None,
            period: "2026-W39".into(),
            body: String::new(),
            body_ref: None,
            created_at: "2026-09-24T00:00:00Z".into(),
            updated_at: "2026-09-24T00:00:00Z".into(),
        }
    }

    #[tokio::test]
    async fn summarize_preconditions_select_review_accepts_weekly_draft() {
        let read = FakeRead {
            review: Some(weekly_review(ReviewStatus::Draft, ReviewKind::Weekly)),
        };
        let r = select_review(&read, "r1").await.unwrap();
        assert_eq!(r.id, "r1");
    }

    #[tokio::test]
    async fn summarize_preconditions_select_review_rejects_unknown_daily_and_finalized() {
        let unknown = FakeRead { review: None };
        assert_eq!(
            select_review(&unknown, "nope").await,
            Err(SummarizeInputError::UnknownReview("nope".into()))
        );

        let daily = FakeRead {
            review: Some(weekly_review(ReviewStatus::Draft, ReviewKind::Daily)),
        };
        assert_eq!(
            select_review(&daily, "r1").await,
            Err(SummarizeInputError::UnsupportedKind(ReviewKind::Daily))
        );

        let finalized = FakeRead {
            review: Some(weekly_review(ReviewStatus::Finalized, ReviewKind::Weekly)),
        };
        assert_eq!(
            select_review(&finalized, "r1").await,
            Err(SummarizeInputError::NotDraft(
                "r1".into(),
                ReviewStatus::Finalized
            ))
        );
    }

    // ==================================================================
    // run_summarize against a REAL Sin90Store/AiReader (same boundary
    // carve-out `ai::classify`/`ai::propose`'s own `#[cfg(test)] mod tests`
    // already use, §11.5's checker doc — "单元测试可以用真实 store 建夹具").
    // ==================================================================

    use crate::ai::{ModelFailure, NoModelPort};
    use crate::core::{Energy, FireTrigger, NewReview, RoutineKind, ScheduleBlockStatus, TaskKind};
    use crate::store::Sin90Store;
    use std::future::Future;

    /// A `ModelPort` that always returns the SAME canned reply/failure —
    /// same shape `ai::classify`'s own `StubModel` uses.
    struct StubModel(Result<ModelReply, ModelFailure>);
    impl ModelPort for StubModel {
        fn complete(
            &self,
            _req: ModelRequest,
        ) -> impl Future<Output = Result<ModelReply, ModelFailure>> + Send {
            let r = self.0.clone();
            async move { r }
        }
    }

    fn ok_reply(text: &str) -> ModelReply {
        ModelReply {
            text: text.to_string(),
            model_id: Some("m".into()),
            tier: crate::ai::ServedTier::Local,
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
        }
    }

    async fn create_review_routine(store: &Sin90Store) -> crate::core::Routine {
        store
            .create_routine(&crate::core::NewRoutine {
                title: "Weekly review".into(),
                area_id: None,
                direction_id: None,
                kind: RoutineKind::Review,
                cron: "0 18 * * SUN".into(),
                tz: None,
                target_count: None,
                target_minutes: None,
            })
            .await
            .unwrap()
    }

    /// J17's own end-to-end claim, against a REAL fixture (2026-09-26 review
    /// H2: "不再手工构造 SummarizeDraft") — one completed block, one done
    /// task, driven through the real `AiReader`, then through the FULL
    /// `run_summarize` with a model producing a narrative that cites both a
    /// fact and a task title. Every digit in the resulting body must trace
    /// back to `render_facts`'s own output or a cited task title.
    #[tokio::test]
    async fn summarize_numbers_come_from_draft() {
        let store = Sin90Store::open_memory().await.unwrap();
        let area = store.create_area("Coding").await.unwrap();
        let direction = store
            .create_direction("Ship it", "2026-Q4", Some(&area.id))
            .await
            .unwrap();
        let in_week = "2026-09-24T10:00:00Z"; // 2026-W39
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
            .create_task("Ship it v1", None, None, TaskKind::Other, Energy::Mid, None)
            .await
            .unwrap();
        for to in [
            crate::core::TaskStatus::Planned,
            crate::core::TaskStatus::InProgress,
            crate::core::TaskStatus::Done,
        ] {
            store.transition_task(&task.id, to).await.unwrap();
        }
        crate::store::test_hooks::set_last_event_at(&store, "task", &task.id, in_week)
            .await
            .unwrap();

        let review = store
            .create_review(&NewReview {
                kind: ReviewKind::Weekly,
                period: "2026-W39".into(),
            })
            .await
            .unwrap();
        let reader = store.ai_reader();

        let draft = AiReadModel::weekly_draft(&reader, "2026-W39")
            .await
            .unwrap();
        let f = facts(&draft);
        let facts_md = render_facts(&draft.week, &f);
        let model = StubModel(Ok(ok_reply(
            &json!({"narrative": "编码方向投入{{f1}}，重点推进了{{t1}}。"}).to_string(),
        )));

        let result = run_summarize(
            "run-j17",
            &review,
            ModelAccess::LocalOnly,
            Some(&model),
            &store,
            &reader,
        )
        .await;
        let SummarizeItemResult::Proposed(id) = result else {
            panic!("expected Proposed, got {result:?}");
        };
        let stored = store.get_proposal(&id).await.unwrap();
        let [Sin90Op::DraftReviewBody { body, .. }] = stored.ops.as_slice() else {
            panic!("expected exactly one DraftReviewBody op: {:?}", stored.ops);
        };
        assert!(body.contains(&facts_md), "{body}");

        let allowed: Vec<String> = digit_runs(&facts_md)
            .into_iter()
            .chain(digit_runs("Ship it v1"))
            .collect();
        assert!(
            digit_runs(body).iter().all(|d| allowed.contains(d)),
            "every digit in the body must trace back to render_facts or a cited task title: \
             body={body:?} allowed={allowed:?}"
        );
        // Positive control: this isn't vacuous — tampering the body invents
        // a digit `allowed` does not contain.
        let tampered = body.replace("1 小时 30 分钟", "9 小时 30 分钟");
        assert!(!digit_runs(&tampered).iter().all(|d| allowed.contains(d)));
    }

    /// H2 (2026-09-26 review): the FULL positive path, end to end — a
    /// model-produced narrative survives `fill_narrative`, gets composed
    /// with the facts block, and lands in a submitted proposal whose
    /// `source` is `local_brain` (served locally, no privacy switch
    /// involved).
    #[tokio::test]
    async fn summarize_model_narrative_end_to_end_positive_path() {
        let store = Sin90Store::open_memory().await.unwrap();
        let review = store
            .create_review(&NewReview {
                kind: ReviewKind::Weekly,
                period: "2026-W39".into(),
            })
            .await
            .unwrap();
        let reader = store.ai_reader();
        let model = StubModel(Ok(ok_reply(
            &json!({"narrative": "本周整体推进顺利，团队保持了良好的节奏。"}).to_string(),
        )));

        let result = run_summarize(
            "run-positive",
            &review,
            ModelAccess::LocalOnly,
            Some(&model),
            &store,
            &reader,
        )
        .await;
        let SummarizeItemResult::Proposed(id) = result else {
            panic!("expected Proposed, got {result:?}");
        };

        let source: String = sqlx::query_scalar("SELECT source FROM sin90_proposals WHERE id = ?")
            .bind(&id)
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(source, "local_brain");

        // L2 (2026-09-26 review round 2): confirm the model's OWN narrative
        // actually landed in the proposed body — the `source`/`ok` checks
        // above prove the ladder took the model branch, but not that the
        // text it produced survived into what gets accepted.
        let stored = store.get_proposal(&id).await.unwrap();
        let [Sin90Op::DraftReviewBody { body, .. }] = stored.ops.as_slice() else {
            panic!("expected exactly one DraftReviewBody op: {:?}", stored.ops);
        };
        assert!(body.contains("本周整体推进顺利"), "{body}");

        let (engine, ok): (String, bool) = sqlx::query_as(
            "SELECT engine, ok FROM sin90_ai_calls WHERE run_id = 'run-positive' AND ok = 1",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(engine, "local");
        assert!(ok);
    }

    /// H2 (2026-09-26 review): negative controls through the FULL ladder —
    /// a model reply `fill_narrative` rejects degrades to reflex, and the
    /// model step's OWN `sin90_ai_calls` row records `bad_output` while the
    /// final produced proposal's `source` is `rule` (reflex).
    #[tokio::test]
    async fn summarize_model_bad_output_degrades_to_reflex_end_to_end() {
        for bad_narrative in [
            "编码 99 小时都在写代码。", // literal digit
            "完成了七个任务。",         // CJK numeral + measure word
            "〔伪造：十八小时〕",       // hand-written unit
        ] {
            let store = Sin90Store::open_memory().await.unwrap();
            let review = store
                .create_review(&NewReview {
                    kind: ReviewKind::Weekly,
                    period: "2026-W39".into(),
                })
                .await
                .unwrap();
            let reader = store.ai_reader();
            let model = StubModel(Ok(ok_reply(
                &json!({"narrative": bad_narrative}).to_string(),
            )));

            let result = run_summarize(
                "run-negative",
                &review,
                ModelAccess::LocalOnly,
                Some(&model),
                &store,
                &reader,
            )
            .await;
            let SummarizeItemResult::Proposed(id) = result else {
                panic!("{bad_narrative:?}: expected Proposed (via reflex), got {result:?}");
            };

            let source: String =
                sqlx::query_scalar("SELECT source FROM sin90_proposals WHERE id = ?")
                    .bind(&id)
                    .fetch_one(store.pool())
                    .await
                    .unwrap();
            assert_eq!(source, "rule", "{bad_narrative:?}");

            let bad_output_rows: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM sin90_ai_calls
                 WHERE run_id = 'run-negative' AND engine = 'local'
                   AND ok = 0 AND error_kind = 'bad_output'",
            )
            .fetch_one(store.pool())
            .await
            .unwrap();
            assert_eq!(bad_output_rows, 1, "{bad_narrative:?}");
        }
    }

    // ---- C1 (2026-09-26 review): the auto-draft gate, end to end -----------

    /// The full C1 flow: a review-kind Routine fires, auto-creating a weekly
    /// draft whose body is T4.3.2's `auto_draft_md` — summarize must be able
    /// to take that draft over and produce a proposal (candidate ②'s whole
    /// point).
    #[tokio::test]
    async fn summarize_auto_draft_from_routine_fire_can_be_rewritten() {
        let store = Sin90Store::open_memory().await.unwrap();
        let routine = create_review_routine(&store).await;
        // 2026-09-26 review round 2 (H1): `scheduled_for` is real "now", not
        // a hand-picked date — a hardcoded `2026-W39` goes stale (and this
        // test would silently stop exercising a real fire) the moment
        // calendar time passes it, which for `2026-W39` was already true
        // past 2026-09-28.
        let now = crate::core::now_iso8601();
        let outcome = store
            .record_routine_fire(
                "fire-1",
                &format!("routine.{}", routine.id),
                &now,
                FireTrigger::Tick,
            )
            .await
            .unwrap();
        let crate::store::RoutineFireOutcome::Recorded { auto_review, .. } = outcome else {
            panic!("expected Recorded, got {outcome:?}");
        };
        let auto_review = auto_review.expect("must auto-create a draft");

        let reader = store.ai_reader();
        let review = store.get_review(&auto_review.review_id).await.unwrap();
        assert!(!review.body.is_empty(), "sanity: the auto-draft has a body");

        let model: Option<&NoModelPort> = None; // reflex-only, same as production today
        let result = run_summarize(
            "run-auto-draft",
            &review,
            ModelAccess::LocalOnly,
            model,
            &store,
            &reader,
        )
        .await;
        assert!(
            matches!(result, SummarizeItemResult::Proposed(_)),
            "the auto-created draft (candidate ②) must be recognized as program-only: {result:?}"
        );
    }

    /// A human adding even ONE character under the auto-draft makes it
    /// human text — no fuzzy matching (§11.4.2's revised "可改写条件").
    #[tokio::test]
    async fn summarize_auto_draft_with_human_addition_is_skipped() {
        let store = Sin90Store::open_memory().await.unwrap();
        let routine = create_review_routine(&store).await;
        // H1 (2026-09-26 review round 2): real "now", not a hardcoded date.
        let now = crate::core::now_iso8601();
        let outcome = store
            .record_routine_fire(
                "fire-1",
                &format!("routine.{}", routine.id),
                &now,
                FireTrigger::Tick,
            )
            .await
            .unwrap();
        let crate::store::RoutineFireOutcome::Recorded { auto_review, .. } = outcome else {
            panic!("expected Recorded, got {outcome:?}");
        };
        let auto_review = auto_review.unwrap();
        let original = store.get_review(&auto_review.review_id).await.unwrap();
        store
            .update_review_body(&auto_review.review_id, &format!("{}x", original.body))
            .await
            .unwrap();
        let touched = store.get_review(&auto_review.review_id).await.unwrap();

        let reader = store.ai_reader();
        let model: Option<&NoModelPort> = None;
        let result = run_summarize(
            "run-auto-draft-touched",
            &touched,
            ModelAccess::LocalOnly,
            model,
            &store,
            &reader,
        )
        .await;
        assert_eq!(result, SummarizeItemResult::Skipped);
    }

    /// A completed task AFTER the auto-draft was rendered changes the
    /// numbers `weekly_draft` would now compute — a fresh `auto_draft_md`
    /// no longer matches the STORED one, so the stored draft is (correctly,
    /// conservatively) treated as stale human-equivalent text, not silently
    /// overwritten with numbers the stored draft never claimed (§11.4.2's
    /// own documented trade-off: "旧数字块（数字已变）也会被当成人写").
    #[tokio::test]
    async fn summarize_auto_draft_stale_after_new_completion_is_skipped() {
        let store = Sin90Store::open_memory().await.unwrap();
        let routine = create_review_routine(&store).await;
        // H1 (2026-09-26 review round 2): real "now", not a hardcoded date —
        // the follow-up completion below must land in the SAME week this
        // resolves to, not a hardcoded date that happens to share a week
        // with the old hardcoded `scheduled_for`.
        let now = crate::core::now_iso8601();
        let outcome = store
            .record_routine_fire(
                "fire-1",
                &format!("routine.{}", routine.id),
                &now,
                FireTrigger::Tick,
            )
            .await
            .unwrap();
        let crate::store::RoutineFireOutcome::Recorded { auto_review, .. } = outcome else {
            panic!("expected Recorded, got {outcome:?}");
        };
        let auto_review = auto_review.unwrap();

        // A NEW completion, inside the SAME week as `now` above, AFTER the
        // draft was already rendered and stored — reusing `now` itself
        // (rather than a second hardcoded date) guarantees it lands in the
        // same ISO week regardless of when this test actually runs.
        let task = store
            .create_task("late task", None, None, TaskKind::Other, Energy::Mid, None)
            .await
            .unwrap();
        for to in [
            crate::core::TaskStatus::Planned,
            crate::core::TaskStatus::InProgress,
            crate::core::TaskStatus::Done,
        ] {
            store.transition_task(&task.id, to).await.unwrap();
        }
        crate::store::test_hooks::set_last_event_at(&store, "task", &task.id, &now)
            .await
            .unwrap();

        let review = store.get_review(&auto_review.review_id).await.unwrap();
        let reader = store.ai_reader();
        let model: Option<&NoModelPort> = None;
        let result = run_summarize(
            "run-auto-draft-stale",
            &review,
            ModelAccess::LocalOnly,
            model,
            &store,
            &reader,
        )
        .await;
        assert_eq!(result, SummarizeItemResult::Skipped);
    }

    // ---- §11.4.2's "比较并交换" ------------------------------------------

    /// §11.4.2's "比较并交换": re-running against a review whose body is
    /// ALREADY exactly the reflex fallback's own output (nothing changed
    /// since) must produce `Nothing`, not a redundant second proposal.
    #[tokio::test]
    async fn summarize_no_op_when_body_already_matches_reflex_output() {
        let store = Sin90Store::open_memory().await.unwrap();
        let review = store
            .create_review(&NewReview {
                kind: ReviewKind::Weekly,
                period: "2026-W39".into(),
            })
            .await
            .unwrap();
        let reader = store.ai_reader();
        let model: Option<&NoModelPort> = None;

        let first = run_summarize(
            "run-1",
            &review,
            ModelAccess::LocalOnly,
            model,
            &store,
            &reader,
        )
        .await;
        let SummarizeItemResult::Proposed(proposal_id) = first else {
            panic!("expected Proposed, got {first:?}");
        };
        store.apply_proposal(&proposal_id).await.unwrap();
        let applied = store.get_review(&review.id).await.unwrap();
        assert!(applied.body.contains(FACTS_HEADING_WORD));

        let second = run_summarize(
            "run-2",
            &applied,
            ModelAccess::LocalOnly,
            model,
            &store,
            &reader,
        )
        .await;
        assert_eq!(second, SummarizeItemResult::Nothing);

        // Positive control (2026-09-26 review, Low: "改用同一份草稿" — NOT a
        // different period/week): reset the SAME review's body back to
        // empty via the ordinary human PATCH path, still `2026-W39`, still
        // the identical draft numbers — proves `Nothing` above was a real
        // comparison, not this review always coming back `Nothing`
        // regardless of body.
        store.update_review_body(&review.id, "").await.unwrap();
        let reset = store.get_review(&review.id).await.unwrap();
        let third = run_summarize(
            "run-3",
            &reset,
            ModelAccess::LocalOnly,
            model,
            &store,
            &reader,
        )
        .await;
        assert!(matches!(third, SummarizeItemResult::Proposed(_)));
    }

    // ---- Low: weekly_draft read failure is distinguishable -----------------

    /// 2026-09-26 review (Low): a malformed `period` (which `weekly_draft`
    /// rejects, `store::weekly_draft_on`'s own `StoreError::Invalid`) must
    /// come back as `ReadFailed`, not silently `Nothing` — the caller
    /// (`http::ai_summarize`) can tell "nothing to do" apart from "the read
    /// itself broke".
    #[tokio::test]
    async fn summarize_weekly_draft_read_failure_is_distinguishable_from_nothing() {
        let store = Sin90Store::open_memory().await.unwrap();
        let mut review = store
            .create_review(&NewReview {
                kind: ReviewKind::Weekly,
                period: "2026-W39".into(),
            })
            .await
            .unwrap();
        // A malformed period `AiReadModel::weekly_draft` will reject —
        // `select_review` never validates `period` itself (that's a store
        // invariant from `create_review`), so this simulates a corrupted
        // row without needing raw SQL.
        review.period = "not-a-week".into();

        let reader = store.ai_reader();
        let model: Option<&NoModelPort> = None;
        let result = run_summarize(
            "run-read-fail",
            &review,
            ModelAccess::LocalOnly,
            model,
            &store,
            &reader,
        )
        .await;
        assert!(
            matches!(result, SummarizeItemResult::ReadFailed(_)),
            "{result:?}"
        );
        assert_ne!(result, SummarizeItemResult::Nothing);
    }
}
