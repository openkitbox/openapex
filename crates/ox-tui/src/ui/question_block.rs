//! A question call drawn with its answers: upstream `Question`
//! (`routes/session/index.tsx:3649-3683`, `parseQuestions` /
//! `parseQuestionAnswers` at 3787-3800) inside the shared `BlockTool` frame
//! (`block_tool`). Pure: the call's input and metadata borrowed in, rows out.
//!
//! Layout: `# Questions`, the block's gap, then per question (input
//! `questions[].question`, items without a question string skipped) the
//! question in `text.muted` and the answer in `text.base`: the answer's
//! strings joined with `, `, or `(no answer)` when it is missing or empty.
//! One blank row (`gap={1}`) separates the groups.
//!
//! Deviations:
//! - Long questions and answers wrap at whitespace by display width
//!   (`block_tool::wrap`) instead of opentui's cell wrap.
//! - Without an `answers` array in the metadata the caller keeps the inline
//!   `→ Asked N questions` row (`tool_text`), as upstream's `InlineTool`.

use crate::theme::Palette;
use ox_schema::support::Record;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use serde_json::Value;

use super::block_tool::{self, BlockSpec};

/// The answer line for a question nobody answered.
const NO_ANSWER: &str = "(no answer)";

/// The block for an answered question call, or `None` when the metadata
/// carries no `answers` array (the caller keeps the inline row).
#[must_use]
pub fn render(
    input: &Record,
    metadata: Option<&Record>,
    width: u16,
    palette: &Palette,
) -> Option<Vec<Line<'static>>> {
    let Some(Value::Array(answers)) = metadata?.get("answers") else {
        return None;
    };
    let questions = questions(input);
    let room = block_tool::content_width(width);
    let mut body = Vec::new();
    for (index, question) in questions.iter().enumerate() {
        if index > 0 {
            body.push(Line::default());
        }
        push_wrapped(&mut body, question, palette.dim, room);
        let answer = answers.get(index).and_then(answer_text);
        push_wrapped(
            &mut body,
            answer.as_deref().unwrap_or(NO_ANSWER),
            palette.text,
            room,
        );
    }
    let spec = BlockSpec {
        title: Some("# Questions"),
        ..BlockSpec::default()
    };
    Some(block_tool::lines(&spec, body, width))
}

/// `parseQuestions`: each `questions[].question` that is a non-empty string.
fn questions(input: &Record) -> Vec<&str> {
    let Some(Value::Array(items)) = input.get("questions") else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| item.get("question").and_then(Value::as_str))
        .filter(|question| !question.is_empty())
        .collect()
}

/// `parseQuestionAnswers` + `format`: the string entries joined with `, `,
/// `None` when there are none (a non-array answer has none).
fn answer_text(answer: &Value) -> Option<String> {
    let Value::Array(values) = answer else {
        return None;
    };
    let strings: Vec<&str> = values.iter().filter_map(Value::as_str).collect();
    (!strings.is_empty()).then(|| strings.join(", "))
}

fn push_wrapped(body: &mut Vec<Line<'static>>, text: &str, style: Style, width: usize) {
    let rows = block_tool::wrap(text, width);
    body.extend(
        rows.into_iter()
            .map(|row| Line::from(Span::styled(row, style))),
    );
}

#[cfg(test)]
mod tests {
    use ox_schema::support::Record;
    use serde_json::json;

    use crate::theme;
    use crate::ui::edit_block::tests::texts;

    fn rec(value: serde_json::Value) -> Record {
        let serde_json::Value::Object(map) = value else {
            panic!("an object: {value}");
        };
        map.into_iter().collect()
    }

    fn input() -> Record {
        rec(json!({"questions": [
            {"question": "Which language?"},
            {"header": "skipped, no question"},
            {"question": "Which targets?"},
            {"question": "Anything else?"},
        ]}))
    }

    #[test]
    fn answered_questions_draw_a_questions_block() {
        let metadata = rec(json!({"answers": [["Rust"], ["linux", "macos", 3], "bogus"]}));
        let palette = theme::palette();
        let lines = super::render(&input(), Some(&metadata), 60, palette).expect("a block");
        let rows = texts(&lines);
        let trimmed: Vec<&str> = rows.iter().map(|row| row.trim()).collect();
        assert_eq!(
            trimmed,
            [
                "",
                "# Questions",
                "",
                "Which language?",
                "Rust",
                "",
                "Which targets?",
                "linux, macos",
                "",
                "Anything else?",
                "(no answer)",
                "",
            ],
            "{rows:?}"
        );
        let span = |row: usize, text: &str| {
            lines[row]
                .spans
                .iter()
                .find(|s| s.content.trim() == text)
                .unwrap_or_else(|| panic!("{text:?} in row {row}: {rows:?}"))
                .style
                .fg
        };
        assert_eq!(span(3, "Which language?"), palette.dim.fg, "text.muted");
        assert_eq!(span(4, "Rust"), palette.text.fg, "text.base");
        assert_eq!(span(10, "(no answer)"), palette.text.fg, "text.base");
    }

    #[test]
    fn long_text_wraps_inside_the_band() {
        let input = rec(json!({"questions": [{"question": "word ".repeat(30)}]}));
        let metadata = rec(json!({"answers": [[]]}));
        let rows =
            texts(&super::render(&input, Some(&metadata), 40, theme::palette()).expect("a block"));
        assert!(rows.len() > 6, "the question wraps: {rows:?}");
        assert!(
            rows.iter()
                .all(|row| unicode_width::UnicodeWidthStr::width(row.as_str()) <= 39),
            "{rows:?}"
        );
    }

    #[test]
    fn no_answers_array_keeps_the_inline_row() {
        let palette = theme::palette();
        assert!(
            super::render(&input(), None, 60, palette).is_none(),
            "without metadata the inline row stays"
        );
        let metadata = rec(json!({"answers": "Rust"}));
        assert!(
            super::render(&input(), Some(&metadata), 60, palette).is_none(),
            "a non-array answers field is ignored"
        );
    }
}
