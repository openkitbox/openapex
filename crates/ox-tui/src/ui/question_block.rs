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
