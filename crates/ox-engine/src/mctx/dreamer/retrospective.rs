use std::collections::HashSet;
use std::sync::OnceLock;

use anyhow::Result;
use regex::Regex;

use crate::mctx::memory::CATEGORIES;

use super::turns::EligibleTurn;

use super::manifest::attr_of;
use super::text::head_chars;

/// The retrospective task's prefixed completion prompt. One element, or an
/// empty one: the whole task is "at most one durable lesson", so an answer that
/// emits a list is out of contract and rejected.
pub(super) const RETROSPECTIVE_SYSTEM_PROMPT: &str = "You are a memory curator for the magic-context system, reviewing the user's own turns from a recent working session.

Your job: find AT MOST ONE durable lesson — a correction the user made, a constraint they stated twice, or a convention they set — that every future session of this project should know. Ignore task chatter, one-off instructions, and anything that only makes sense for the work of the moment.

Rules:
1. One fact, phrased to stand alone: present tense, no pronouns pointing back at the conversation, no \"the user said\".
2. The category must be one of: PROJECT_RULES (how work is done here), ARCHITECTURE (how the system is built), CONSTRAINTS (a hard limit or a gotcha), CONFIG_VALUES (a specific value or path), NAMING (what something is called).
3. Only a lesson the user actually stated or corrected. Never infer one, never generalize from a single request, and never restate something ordinary.
4. Nothing durable in these turns is a valid answer.
5. Distill, do not transcribe: never quote the user, never include dates, and never preserve session-local anger.

Output ONE element at the very end and NOTHING else — no narration, no reasoning:
<memory category=\"CONSTRAINTS\">one self-contained fact</memory>

Or, when nothing is durable:
<memory/>";

/// Characters of one user turn the retrospective sees. A correction is usually
/// one or two sentences; the cap keeps a pasted stack trace out of the prompt.
const RETROSPECTIVE_TURN_CHARS: usize = 600;

/// Total characters of transcript one retrospective prompt carries. A cost
/// guard, not a model limit: the newest turns win the budget, so the oldest are
/// the ones dropped.
const RETROSPECTIVE_TOTAL_CHARS: usize = 8_000;

/// Cap on the one fact a run may record: a memory is injected into every future
/// request, so a chatty lesson must not become a wall of text.
const RETROSPECTIVE_MAX_CHARS: usize = 2_000;

// ---------------------------------------------------------------------------
// retrospective
// ---------------------------------------------------------------------------

/// The one durable lesson a retrospective run may record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RetrospectiveLesson {
    /// One of [`CATEGORIES`].
    pub(super) category: &'static str,
    /// The fact text, newline-collapsed and length-capped.
    pub(super) content: String,
}

/// The `retrospective` user prompt: the recent turns, oldest first.
///
/// `turns` arrives newest-first (the query's order) and the prompt reads them in
/// the order they happened. The character budget is spent from the newest end,
/// so an over-long window drops the oldest turns rather than the most recent
/// corrections — which are exactly the ones worth learning from.
pub(super) fn render_retrospective_prompt<'a>(
    project: &str,
    turns: &'a [EligibleTurn],
) -> (String, Vec<&'a str>) {
    let mut kept: Vec<&str> = Vec::new();
    let mut source_sessions = Vec::new();
    let mut budget = RETROSPECTIVE_TOTAL_CHARS;
    for turn in turns {
        let text = head_chars(turn.text.trim(), RETROSPECTIVE_TURN_CHARS);
        let cost = text.chars().count();
        if cost == 0 {
            continue;
        }
        if cost > budget {
            break;
        }
        budget -= cost;
        kept.push(text);
        if !source_sessions.contains(&turn.session_id.as_str()) {
            source_sessions.push(turn.session_id.as_str());
        }
    }
    kept.reverse();
    let mut parts = vec![format!(
        "## Project: {project}\n\nThe user's own turns from the recent working session, in the order they happened. Look for a durable lesson.\n\n<turns>"
    )];
    for text in kept {
        parts.push(format!("<turn>{text}</turn>"));
    }
    parts.push("</turns>".to_owned());
    (parts.join("\n"), source_sessions)
}

/// Parse the one `<memory …>…</memory>` element.
///
/// Three answers are legal: exactly one element carrying a known category, one
/// empty/self-closing element (`nothing durable`), or no element at all (also
/// `nothing durable`). Everything else is a parse failure and writes nothing —
/// an unknown category, an unclosed element (truncation), or two elements (the
/// task asked for at most one, so a list means the model drifted from the
/// contract).
pub(super) fn parse_retrospective(text: &str) -> Result<Option<RetrospectiveLesson>, String> {
    let element = memory_element_regex();
    let mut elements = element.captures_iter(text);
    let Some(captures) = elements.next() else {
        if memory_open_regex().is_match(text) {
            return Err("retrospective answer carries an unclosed <memory> element".to_owned());
        }
        return Ok(None);
    };
    if elements.next().is_some() {
        return Err("retrospective answer carries more than one <memory> element".to_owned());
    }
    let attrs = captures.get(1).map_or("", |group| group.as_str());
    let body = captures.get(2).map_or("", |group| group.as_str());
    // Newlines become single spaces: a memory is one line in every prefix it
    // is rendered into, and a model answer is often indented. An element with
    // no fact in it is the contract's "nothing durable", whatever it says about
    // a category.
    let collapsed = collapse_lines(body);
    let content = head_chars(&collapsed, RETROSPECTIVE_MAX_CHARS).to_owned();
    if content.is_empty() {
        return Ok(None);
    }
    let Some(category) = attr_of(attrs, "category") else {
        return Err("retrospective <memory> element is missing a category".to_owned());
    };
    let category = category.trim();
    let Some(category) = CATEGORIES.iter().find(|known| **known == category) else {
        return Err(format!(
            "retrospective <memory> element carries an unknown category: {category}"
        ));
    };
    Ok(Some(RetrospectiveLesson { category, content }))
}

/// Why [`validate_retrospective_lesson`] rejected a retrospective lesson
/// (upstream dreamer/retrospective-learnings.ts:125-134).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LessonRejection {
    /// A quoted run of >=4 characters — transcription, not distillation.
    RawQuote,
    /// A 19xx/20xx year literal — session-local chronology.
    DateLiteral,
    /// Session-local frustration phrasing.
    FrustrationMarker,
    /// Shares a verbatim word-run of `run_cap` words with a source turn.
    SourceOverlap,
}

/// The longest verbatim word-run a lesson may share with a source turn
/// (upstream `MAX_SOURCE_WORD_RUN`, retrospective-learnings.ts:68).
const MAX_SOURCE_WORD_RUN: usize = 7;

/// Words of one lesson the overlap check reads (upstream
/// `MAX_OVERLAP_LEARNING_WORDS`, :74). A lesson is one line by contract and
/// already capped by [`RETROSPECTIVE_MAX_CHARS`]; the cap is defense.
const MAX_OVERLAP_LEARNING_WORDS: usize = 200;

/// The strongest correction phrases, lowercased. A reduction of the upstream
/// `FRUSTRATION_MARKER_REGEX` (:14-15): that regex also counts "this is
/// wrong", "you missed", "stop claiming/using", repeated no/wrong/again/stop
/// runs, and `???` bursts.
const FRUSTRATION_MARKERS: [&str; 5] = [
    "not what i asked",
    "i already said",
    "you ignored",
    "that's wrong",
    "stop doing",
];

/// Validate one parsed retrospective lesson against the upstream
/// "distill, do not transcribe" gate (retrospective-learnings.ts:65-67 and
/// :125-134): a durable memory must be the third-person lesson, never the
/// user's own words. A lesson that quotes the user, carries a date literal,
/// preserves session-local anger, or shares a verbatim word-run with a source
/// turn is rejected — the caller drops it and logs, never writes.
///
/// Two deliberate reductions from the upstream regexes (documented, not
/// accidental): the frustration check keeps the five strongest correction
/// phrases plus a `!!!` burst; the date check keys on bare 19xx/20xx year
/// literals where upstream requires a full date shape.
pub(crate) fn validate_retrospective_lesson(
    content: &str,
    source_user_texts: &[&str],
) -> Result<(), LessonRejection> {
    if has_raw_quote(content) {
        return Err(LessonRejection::RawQuote);
    }
    if has_year_literal(content) {
        return Err(LessonRejection::DateLiteral);
    }
    if has_frustration_marker(content) {
        return Err(LessonRejection::FrustrationMarker);
    }
    if has_source_overlap(content, source_user_texts) {
        return Err(LessonRejection::SourceOverlap);
    }
    Ok(())
}

/// A run of >=4 characters between any two double-quote-class characters
/// (`" “ ”`, interchangeable as openers/closers in the upstream
/// `RAW_QUOTE_REGEX`, :36), or between two apostrophes. An unpaired
/// apostrophe — a contraction — never matches; two contractions in one
/// sentence can, which the upstream regex also does.
fn has_raw_quote(content: &str) -> bool {
    fn paired_run(text: &str, is_quote: impl Fn(char) -> bool) -> bool {
        let mut inside = false;
        let mut run = 0usize;
        for ch in text.chars() {
            if is_quote(ch) {
                if inside && run >= 4 {
                    return true;
                }
                inside = true;
                run = 0;
            } else if inside {
                run += 1;
            }
        }
        false
    }
    paired_run(content, |ch| matches!(ch, '"' | '“' | '”')) || paired_run(content, |ch| ch == '\'')
}

/// A bare 19xx/20xx year literal: any four-digit ASCII run carrying one of
/// the two century prefixes (upstream `DATE_REGEX`, :37-38, reduced — see
/// [`validate_retrospective_lesson`]).
fn has_year_literal(content: &str) -> bool {
    content
        .split(|ch: char| !ch.is_ascii_digit())
        .any(|run| run.len() == 4 && (run.starts_with("19") || run.starts_with("20")))
}

/// A correction phrase, or a `!!!` punctuation burst.
fn has_frustration_marker(content: &str) -> bool {
    let lowered = content.to_lowercase();
    lowered.contains("!!!")
        || FRUSTRATION_MARKERS
            .iter()
            .any(|marker| lowered.contains(marker))
}

/// Lowercase alphanumeric words — the token convention the mctx search's
/// `is_token_char` uses, and the upstream `toWords` (:76-83) minus the
/// Unicode letter classes ox's tokenizer never carried.
fn overlap_words(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(|word| word.to_lowercase())
        .collect()
}

/// The shared-run length that means transcription: an absolute cap of
/// [`MAX_SOURCE_WORD_RUN`], or half the lesson's own length for very short
/// lessons, floored at 3 (upstream :102-105).
fn source_run_cap(lesson_words: usize) -> usize {
    MAX_SOURCE_WORD_RUN.min(3.max(lesson_words.div_ceil(2)))
}

/// True when the lesson shares a contiguous run of `run_cap` words with any
/// source turn: some runCap-gram of the lesson occurs verbatim in the source
/// (upstream `hasHighSourceOverlap`, :99-123). Sources are never truncated —
/// a lifted run at any offset is caught.
fn has_source_overlap(content: &str, source_user_texts: &[&str]) -> bool {
    let mut lesson = overlap_words(content);
    lesson.truncate(MAX_OVERLAP_LEARNING_WORDS);
    let run_cap = source_run_cap(lesson.len());
    if lesson.len() < run_cap {
        return false;
    }
    let grams: HashSet<String> = lesson.windows(run_cap).map(|run| run.join(" ")).collect();
    source_user_texts.iter().any(|source| {
        overlap_words(source)
            .windows(run_cap)
            .any(|run| grams.contains(&run.join(" ")))
    })
}

/// Every non-empty line of `text`, trimmed and joined by single spaces.
fn collapse_lines(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(line);
    }
    out
}

// Regex accessors return a clone so the lazy-DFA cache is dropped after the
// call instead of living on per worker thread (see `historian_validate.rs`).
fn memory_element_regex() -> Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?is)<memory\b([^>]*?)(?:/>|>(.*?)</memory\s*>)")
            .expect("retrospective memory element regex")
    })
    .clone()
}

fn memory_open_regex() -> Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<memory\b").expect("retrospective memory open regex"))
        .clone()
}
