//! `summarize` (T5.3.1, design §11.4.2): drafts a weekly Review's `body` — a
//! program-rendered "本周数字" facts block (numbers straight from T4.3.1's
//! weekly draft, via [`ports::SummarizeDraft`]) plus an AI narrative that may
//! reference those numbers ONLY through `{{fN}}`/`{{tN}}` placeholders the
//! program itself renders, never a digit the model typed.
//!
//! **Layer B of 3** (2026-09-26 review, stacked as `feat/t5.3.1a-summarize-
//! store` → `feat/t5.3.1b-summarize-core` → `feat/t5.3.1-summarize`): this
//! file currently holds only the CORE pure functions — the "数字只来自草稿"
//! mechanism itself ([`facts`] through [`digit_runs`]) — ported from the
//! frozen design's scratch crate `t501-check/src/summarize.rs` (§11.12, "已
//! check + test") and adapted to this crate's real [`SummarizeDraft`] shape
//! (`ai::ports`, resolved titles + `auto_draft_md`, not the scratch's
//! placeholder tuples). The request/parse/rationale/run-driver half (Layer
//! C) is added on top by a later commit on `feat/t5.3.1-summarize`.
//!
//! `facts()` is the one function whose SHAPE had to change from the scratch
//! signature: this crate's real weekly draft carries raw ids, not resolved
//! titles, and `ai/` cannot look one up itself (§11.5: it may not name
//! `crate::store`). [`ports::AiReadModel::weekly_draft`]'s store-side
//! implementation does that id→title join and hands back [`ports::
//! SummarizeDraft`] instead — `facts()` here takes that directly, with no
//! `title_of` closure (the scratch signature `facts(draft, title_of)` is not
//! reachable from this module at all).

use super::ports::SummarizeDraft;

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
}
