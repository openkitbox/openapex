//! Port of upstream `packages/core/src/tool/plugin/question.ts`: ask the user
//! one or more structured questions and wait for the answers.
//!
//! The tool creates a [`form`] entry and awaits it; a client answers through
//! `POST /api/session/{id}/form/{formID}/reply` (or the TUI's form UI). That
//! keeps the tool free of transport concerns and lets several clients answer
//! whichever form they can see.
//!
//! A form that nobody answers does not hang the turn: [`InactiveClient`]
//! cancels any form still pending, and the model gets the same "ask the user
//! freeform instead" guidance upstream produces for a non-interactive client.

use serde_json::{Value, json};
use std::time::Duration;

use crate::contract::{Tool, ToolContext, ToolOutput};
use crate::form::{self, Answer, Field, FormValue, Option_, Outcome};
use ox_schema::wire::ToolDef;

use super::{fail, object_param, ok_text};

/// How long the tool waits before giving up. A human answering a form takes
/// seconds; five minutes is generous without parking a turn forever.
const WAIT: Duration = Duration::from_secs(300);
/// Upstream `options` bounds.
const MIN_OPTIONS: usize = 2;
const MAX_OPTIONS: usize = 6;
pub struct QuestionTool;

impl Tool for QuestionTool {
    fn name(&self) -> &str {
        "question"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "question",
            "Ask the user questions during execution and wait for the answers.\n\n\
             When to use: gather preferences or requirements, clarify ambiguous instructions, \
             get a decision on an implementation choice, or offer directions to take.\n\
             When NOT to use: ask for facts a tool can inspect, ask before trying something \
             reversible, or ask a question whose answer is already in the conversation. Inspect \
             first, then ask only for preferences, tradeoffs, or irreversible decisions.\n\n\
             A \"Type your own answer\" option is added automatically; do not add a separate \
             free-form option. If you recommend an option, put it first and mark its label with \
             \"(Recommended)\".",
            object_param(
                vec![(
                    "questions",
                    json!({
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 4,
                        "description": "Questions to ask, in order.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": {
                                    "type": "string",
                                    "minLength": 1,
                                    "description": "The specific decision to resolve.",
                                },
                                "header": {
                                    "type": "string",
                                    "description": "Short label shown as the field title, at most 30 characters.",
                                },
                                "multiple": {
                                    "type": "boolean",
                                    "description": "Allow selecting more than one option. Defaults to false.",
                                },
                                "options": {
                                    "type": "array",
                                    "minItems": MIN_OPTIONS,
                                    "maxItems": MAX_OPTIONS,
                                    "description": "Choices offered to the user.",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": {
                                                "type": "string",
                                                "minLength": 1,
                                                "description": "Option text shown to the user.",
                                            },
                                            "description": {
                                                "type": "string",
                                                "description": "Optional detail explaining the option.",
                                            },
                                        },
                                        "required": ["label"],
                                        "additionalProperties": false,
                                    },
                                },
                            },
                            "required": ["question", "options"],
                            "additionalProperties": false,
                        },
                    }),
                )],
                &["questions"],
            ),
        )
    }

    fn execute(
        &self,
        ctx: ToolContext,
        input: Value,
    ) -> std::pin::Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(async move { run(&ctx, &input).await })
    }
}

/// One decoded question.
struct Question {
    question: String,
    header: Option<String>,
    multiple: bool,
    options: Vec<Option_>,
}

fn decode(input: &Value) -> Result<Vec<Question>, ToolOutput> {
    let Some(Value::Array(items)) = input.get("questions") else {
        return Err(super::invalid_arguments(
            "question",
            &[(
                "questions",
                "Missing or not an array of questions".to_owned(),
            )],
            input,
        ));
    };
    if items.is_empty() {
        return Err(super::invalid_arguments(
            "question",
            &[("questions", "At least one question is required".to_owned())],
            input,
        ));
    }
    let mut questions = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let key = format!("questions[{index}]");
        let Some(question) = item.get("question").and_then(Value::as_str) else {
            return Err(super::invalid_arguments(
                "question",
                &[(key.as_str(), "Missing string field `question`".to_owned())],
                input,
            ));
        };
        if question.trim().is_empty() {
            return Err(super::invalid_arguments(
                "question",
                &[(key.as_str(), "`question` must not be empty".to_owned())],
                input,
            ));
        }
        let Some(options) = item.get("options").and_then(Value::as_array) else {
            return Err(super::invalid_arguments(
                "question",
                &[(key.as_str(), "Missing array field `options`".to_owned())],
                input,
            ));
        };
        if options.len() < MIN_OPTIONS || options.len() > MAX_OPTIONS {
            return Err(super::invalid_arguments(
                "question",
                &[(
                    key.as_str(),
                    format!("`options` must hold {MIN_OPTIONS} to {MAX_OPTIONS} entries"),
                )],
                input,
            ));
        }
        let mut decoded = Vec::with_capacity(options.len());
        for (option_index, option) in options.iter().enumerate() {
            let Some(label) = option.get("label").and_then(Value::as_str) else {
                return Err(super::invalid_arguments(
                    "question",
                    &[(
                        key.as_str(),
                        format!("options[{option_index}] is missing string field `label`"),
                    )],
                    input,
                ));
            };
            decoded.push(Option_ {
                value: label.to_owned(),
                label: label.to_owned(),
                description: option
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            });
        }
        questions.push(Question {
            question: question.to_owned(),
            header: item
                .get("header")
                .and_then(Value::as_str)
                .map(str::to_owned),
            multiple: item
                .get("multiple")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            options: decoded,
        });
    }
    Ok(questions)
}

/// Upstream `toField`: one form field per question, keyed `q<index>`.
fn to_field(question: &Question, index: usize) -> Field {
    let key = format!("q{index}");
    if question.multiple {
        Field::Multiselect {
            key,
            title: question.header.clone(),
            description: Some(question.question.clone()),
            required: Some(true),
            options: question.options.clone(),
            custom: Some(true),
        }
    } else {
        Field::String {
            key,
            title: question.header.clone(),
            description: Some(question.question.clone()),
            required: Some(true),
            options: Some(question.options.clone()),
            custom: Some(true),
        }
    }
}

async fn run(ctx: &ToolContext, input: &Value) -> ToolOutput {
    let questions = match decode(input) {
        Ok(questions) => questions,
        Err(output) => return output,
    };
    let fields: Vec<Field> = questions
        .iter()
        .enumerate()
        .map(|(index, question)| to_field(question, index))
        .collect();
    let pending = form::shared().create(
        &ctx.session_id,
        &ctx.directory.to_string_lossy(),
        "Questions",
        fields,
    );
    let form_id = pending.info().id.clone();

    // An interrupt ends the wait without answering.
    let outcome = tokio::select! {
        outcome = pending.wait(WAIT) => outcome,
        () = ctx.cancellation.cancelled() => {
            // Cancelling settles the waiter, and the `form.cancelled` broadcast
            // plus the pending-only listing keep the form invisible to clients.
            let _ = form::shared().cancel(&ctx.session_id, &form_id, Some("interrupted".to_owned()));
            return fail(CANCELLED_SENTINEL);
        }
    };
    // Every client answers through the HTTP endpoints, which read the same
    // registry, so the count of unanswered fields is enough to decide.
    let Some(outcome) = outcome else {
        let _ = form::shared().cancel(
            &ctx.session_id,
            &form_id,
            Some("nobody answered".to_owned()),
        );
        return fail(NOT_AVAILABLE_SENTINEL);
    };
    match outcome {
        Outcome::Answered(answer) => {
            // The settled form stays in the registry (upstream keeps it for a
            // retention window): a duplicate reply must find it and conflict
            // with `FormAlreadySettledError`, not 404.
            ok_text(to_model_content(&questions, &answer))
        }
        Outcome::Cancelled(message) => {
            match message {
                // A dismissal with a message is ordinary model-facing feedback
                // (upstream: the model continues instead of the step ending).
                Some(message) if !message.is_empty() => fail(message),
                _ => fail(CANCELLED_SENTINEL),
            }
        }
    }
}

/// Upstream `toModelContent`: `"question"="answer", ...`.
fn to_model_content(questions: &[Question], answer: &Answer) -> String {
    let answered = questions
        .iter()
        .enumerate()
        .map(|(index, question)| {
            let value = answer
                .get(&format!("q{index}"))
                .map(FormValue::display)
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| "Unanswered".to_owned());
            format!("\"{}\"=\"{value}\"", question.question)
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "User has answered your questions: {answered}. You can now continue with the user's \
         answers in mind."
    )
}

/// Upstream's sentinel when no interactive client is attached.
pub const NOT_AVAILABLE_SENTINEL: &str =
    "(question is only available in the interactive shell; ask the user freeform instead)";
/// Upstream `CancelledError`'s model-facing text.
pub const CANCELLED_SENTINEL: &str = "(user cancelled the question)";

/// Build the answer value a client's reply body carries for one question.
pub fn single_option_answer(field: &Field, value: &str) -> FormValue {
    match field {
        Field::Multiselect { .. } => FormValue::Multiple(vec![value.to_owned()]),
        _ => FormValue::String(value.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    /// Each test needs its own session id: `form::shared()` is process-wide
    /// and tests run in parallel, so a shared id lets one test answer another's
    /// form.
    fn ctx() -> ToolContext {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        ToolContext {
            session_id: format!("ses_question_{id}"),
            directory: std::env::temp_dir(),
            call_id: "call_q".to_owned(),
            cancellation: CancellationToken::new(),
        }
    }

    fn ask_input() -> Value {
        json!({
            "questions": [{
                "question": "Which layer should change?",
                "header": "Layer",
                "options": [
                    {"label": "API"},
                    {"label": "Storage", "description": "the database layer"}
                ]
            }]
        })
    }

    #[tokio::test]
    async fn a_reply_reaches_the_model_as_answers() {
        let ctx = ctx();
        let input = ask_input();
        let tool = QuestionTool;
        let session = ctx.session_id.clone();
        let task = tokio::spawn(async move { tool.execute(ctx, input).await });
        // The form appears in the registry; answer it like a client would.
        let form_id = loop {
            let forms = form::shared().list(&session);
            if let Some(info) = forms.first() {
                break info.id.clone();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let mut answer = Answer::new();
        answer.insert("q0".to_owned(), FormValue::String("API".to_owned()));
        assert!(form::shared().reply(&session, &form_id, answer).is_ok());
        let output = task.await.expect("join");
        assert_eq!(output.error, None);
        assert_eq!(
            output.content,
            json!(
                "User has answered your questions: \"Which layer should change?\"=\"API\". You can now continue with the user's answers in mind."
            )
        );
    }

    #[tokio::test]
    async fn a_dismissal_with_a_message_becomes_model_feedback() {
        let ctx = ctx();
        let input = ask_input();
        let tool = QuestionTool;
        let session = ctx.session_id.clone();
        let task = tokio::spawn(async move { tool.execute(ctx, input).await });
        let form_id = loop {
            let forms = form::shared().list(&session);
            if let Some(info) = forms.first() {
                break info.id.clone();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        assert!(
            form::shared()
                .cancel(&session, &form_id, Some("pick for me".to_owned()))
                .is_ok()
        );
        let output = task.await.expect("join");
        assert_eq!(output.error.as_deref(), Some("pick for me"));
    }

    #[tokio::test]
    async fn a_bare_dismissal_uses_the_cancel_sentinel() {
        let ctx = ctx();
        let input = ask_input();
        let tool = QuestionTool;
        let session = ctx.session_id.clone();
        let task = tokio::spawn(async move { tool.execute(ctx, input).await });
        let form_id = loop {
            let forms = form::shared().list(&session);
            if let Some(info) = forms.first() {
                break info.id.clone();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        assert!(form::shared().cancel(&session, &form_id, None).is_ok());
        let output = task.await.expect("join");
        assert_eq!(output.error.as_deref(), Some(CANCELLED_SENTINEL));
    }

    #[tokio::test]
    async fn an_interrupt_cancels_the_form() {
        let ctx = ctx();
        let token = ctx.cancellation.clone();
        let session = ctx.session_id.clone();
        let tool = QuestionTool;
        let task = tokio::spawn(async move { tool.execute(ctx, ask_input()).await });
        loop {
            if !form::shared().list(&session).is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        token.cancel();
        let output = task.await.expect("join");
        assert_eq!(output.error.as_deref(), Some(CANCELLED_SENTINEL));
        assert!(form::shared().list(&session).is_empty(), "form removed");
    }

    #[tokio::test]
    async fn a_question_needs_between_two_and_six_options() {
        let output = QuestionTool
            .execute(
                ctx(),
                json!({
                    "questions": [{
                        "question": "Pick",
                        "options": [{"label": "only one"}]
                    }]
                }),
            )
            .await;
        assert!(
            output
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("`options` must hold 2 to 6 entries")
        );
    }

    #[tokio::test]
    async fn multiple_selection_produces_a_multiselect_field() {
        let question = Question {
            question: "Which files?".to_owned(),
            header: None,
            multiple: true,
            options: vec![Option_ {
                value: "a".to_owned(),
                label: "a".to_owned(),
                description: None,
            }],
        };
        assert!(matches!(to_field(&question, 0), Field::Multiselect { .. }));
        let single = Question {
            multiple: false,
            ..question
        };
        assert!(matches!(to_field(&single, 0), Field::String { .. }));
    }
}
