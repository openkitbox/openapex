//! The form dialog (upstream `routes/session/form.tsx` + `util/form.ts`).
//!
//! Two halves, both free of I/O:
//!
//! - [`FormState`] is the field-editing state machine. `on_key` mutates it and
//!   says what the dialog should do ([`FormOutcome`]); validation is a port of
//!   `formValidateValue` for the keywords `ox-client` models (required,
//!   number bounds, option membership).
//! - [`draw`] paints the state onto a `Buffer`. Upstream does not open a
//!   centred modal for a form: the form *replaces the prompt* — a raised box
//!   with a primary left rule, anchored above the status line. ox does the
//!   same, drawn over the session view.
//!
//! Deviations from upstream, all deliberate:
//! - Text input is a single line with append / backspace / `ctrl+u` /
//!   `ctrl+w`; there is no in-line cursor movement or multi-line growth.
//! - `external` fields are acknowledged with Enter; ox does not open a
//!   browser or copy the URL.
//! - A field whose `type` `ox-client` does not know ([`FormField::Unsupported`])
//!   is shown read-only as "field type not supported" and takes no input, no
//!   validation and no part in the answer map; Enter moves on, so the rest of
//!   the form can still be submitted or cancelled.
//! - `when` conditions, string `format`/`pattern`/length limits and
//!   multiselect `minItems`/`maxItems` are not modelled by `ox-client`, so
//!   they are neither shown nor validated here (the server still validates).

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ox_client::form::{Answer, FormField, FormInfo, FormOption, FormValue};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Widget};
use unicode_width::UnicodeWidthStr;

use super::layout;
use super::paste;
use crate::theme::Palette;

/// What a key did to the form.
#[derive(Debug, Clone, PartialEq)]
pub enum FormOutcome {
    /// The key was not bound; nothing to repaint.
    Idle,
    /// The form stays open and changed.
    Changed,
    /// The answers passed validation; the form is done.
    Submit(Answer),
    /// Esc on a screen that dismisses the whole form.
    Cancel,
}

/// One selectable row (`FormRow`).
#[derive(Debug, Clone, PartialEq)]
struct Row {
    value: FormValue,
    label: String,
    description: Option<String>,
}

/// An open form: the pending `Form.Info` plus the user's progress.
#[derive(Debug, Clone, PartialEq)]
pub struct FormState {
    id: String,
    session: String,
    title: String,
    message: Option<String>,
    fields: Arc<[FormField]>,
    /// Active tab; `fields.len()` is the review ("Submit") tab.
    tab: usize,
    answers: HashMap<String, FormValue>,
    /// The free-form text typed into a field's "Type your own answer" row.
    custom: HashMap<String, String>,
    /// Highlighted row of the active field.
    selected: usize,
    /// Editing the custom row (select fields only; textual fields always edit).
    editing: bool,
    /// The text buffer of whatever is being edited.
    input: String,
    error: String,
    done: Option<FormOutcome>,
}

impl FormState {
    pub fn new(info: &FormInfo) -> Self {
        let mut answers = HashMap::new();
        let mut custom = HashMap::new();
        for field in &info.fields {
            let default = match field {
                FormField::String { default, .. } => default.clone().map(FormValue::String),
                FormField::Number { default, .. } | FormField::Integer { default, .. } => {
                    default.map(FormValue::Number)
                }
                FormField::Boolean { default, .. } => default.map(FormValue::Boolean),
                FormField::Multiselect { default, .. } => default.clone().map(FormValue::Multiple),
                FormField::External { .. } | FormField::Unsupported => None,
            };
            if let Some(value) = default {
                if let (
                    FormField::String {
                        options: Some(options),
                        custom: allow,
                        ..
                    },
                    FormValue::String(text),
                ) = (field, &value)
                    && *allow == Some(true)
                    && !options.iter().any(|option| option.value == *text)
                {
                    custom.insert(field.key().to_owned(), text.clone());
                }
                answers.insert(field.key().to_owned(), value);
            }
        }
        let message = info
            .metadata
            .as_ref()
            .and_then(|meta| meta.get("message"))
            .and_then(|value| value.as_str())
            .map(str::to_owned);
        let mut state = Self {
            id: info.id.clone(),
            session: info.session_id.clone(),
            title: info.title.clone(),
            message,
            fields: info.fields.clone().into(),
            tab: 0,
            answers,
            custom,
            selected: 0,
            editing: false,
            input: String::new(),
            error: String::new(),
            done: None,
        };
        state.enter_tab(0);
        state
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn session(&self) -> &str {
        &self.session
    }

    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The transient validation message, empty when there is none.
    #[must_use]
    pub fn error(&self) -> &str {
        &self.error
    }

    // ---- shape -----------------------------------------------------------

    /// One boolean or option field answers on Enter with no tabs.
    fn single(&self) -> bool {
        matches!(
            &*self.fields,
            [FormField::Boolean { .. }
                | FormField::String {
                    options: Some(_),
                    ..
                }]
        )
    }

    fn tabs(&self) -> usize {
        if self.single() {
            1
        } else {
            self.fields.len() + 1
        }
    }

    fn confirm(&self) -> bool {
        !self.single() && self.tab >= self.fields.len()
    }

    fn current(&self) -> Option<&FormField> {
        if self.confirm() {
            None
        } else {
            self.fields.get(self.tab)
        }
    }

    fn textual_now(&self) -> bool {
        self.current().is_some_and(textual)
    }

    // ---- keys ------------------------------------------------------------

    pub fn on_key(&mut self, ev: KeyEvent) -> FormOutcome {
        self.done = None;
        let handled = if self.confirm() {
            self.confirm_key(ev)
        } else if self.editing || self.textual_now() {
            self.input_key(ev)
        } else {
            self.select_key(ev)
        };
        match self.done.take() {
            Some(outcome) => outcome,
            None if handled => FormOutcome::Changed,
            None => FormOutcome::Idle,
        }
    }

    /// Bracketed paste into the field being typed in (a text field, or the
    /// "own answer" row once editing). Newlines fold to spaces; anywhere else
    /// the paste is ignored.
    pub fn on_paste(&mut self, text: &str) -> FormOutcome {
        let text = paste::single_line(text);
        if text.is_empty() || self.confirm() || !(self.editing || self.textual_now()) {
            return FormOutcome::Idle;
        }
        let fields = Arc::clone(&self.fields);
        let Some(field) = fields.get(self.tab) else {
            return FormOutcome::Idle;
        };
        self.input.push_str(&text);
        if !textual(field) {
            self.update_custom(field);
        }
        FormOutcome::Changed
    }

    /// Tab / arrow / vim keys that move between tabs.
    fn nav_key(&mut self, ev: KeyEvent) -> bool {
        let tabs = self.tabs();
        let back = match ev.code {
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => true,
            KeyCode::Tab if ev.modifiers.contains(KeyModifiers::SHIFT) => true,
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => false,
            _ => return false,
        };
        let next = if back {
            (self.tab + tabs - 1) % tabs
        } else {
            (self.tab + 1) % tabs
        };
        self.enter_tab(next);
        true
    }

    fn confirm_key(&mut self, ev: KeyEvent) -> bool {
        match ev.code {
            KeyCode::Esc => self.done = Some(FormOutcome::Cancel),
            KeyCode::Enter => self.submit(),
            _ => return self.nav_key(ev),
        }
        true
    }

    fn select_key(&mut self, ev: KeyEvent) -> bool {
        let fields = Arc::clone(&self.fields);
        let Some(field) = fields.get(self.tab) else {
            return false;
        };
        if matches!(field, FormField::Unsupported) {
            return match ev.code {
                KeyCode::Esc => {
                    self.done = Some(FormOutcome::Cancel);
                    true
                }
                KeyCode::Enter => {
                    self.enter_tab(self.tab + 1);
                    true
                }
                _ => self.nav_key(ev),
            };
        }
        if matches!(field, FormField::External { .. }) {
            return match ev.code {
                KeyCode::Esc => {
                    self.done = Some(FormOutcome::Cancel);
                    true
                }
                KeyCode::Enter => {
                    self.answers
                        .insert(field.key().to_owned(), FormValue::Boolean(true));
                    self.error.clear();
                    self.enter_tab(self.tab + 1);
                    true
                }
                _ => self.nav_key(ev),
            };
        }
        let rows = self.rows(field);
        let has_custom = custom_of(field);
        let total = rows.len() + usize::from(has_custom);
        let on_other = has_custom && self.selected == rows.len();

        // Typing on the "own answer" row starts editing it (not space, which
        // toggles in a multiselect).
        if on_other
            && let Some(c) = printable(&ev)
            && c != ' '
        {
            self.editing = true;
            self.input = self.custom.get(field.key()).cloned().unwrap_or_default();
            self.input.push(c);
            self.update_custom(field);
            return true;
        }

        match ev.code {
            KeyCode::Esc => self.done = Some(FormOutcome::Cancel),
            KeyCode::Char(d @ '1'..='9') if (d as usize - '0' as usize) <= total.min(9) => {
                self.selected = d as usize - '1' as usize;
                self.select_option(field);
            }
            KeyCode::Up | KeyCode::Char('k') if total > 0 => {
                self.selected = (self.selected + total - 1) % total;
            }
            KeyCode::Down | KeyCode::Char('j') if total > 0 => {
                self.selected = (self.selected + 1) % total;
            }
            KeyCode::Enter => self.select_option(field),
            KeyCode::Char(' ') if matches!(field, FormField::Multiselect { .. }) => {
                self.select_option(field);
            }
            _ => return self.nav_key(ev),
        }
        true
    }

    fn input_key(&mut self, ev: KeyEvent) -> bool {
        let fields = Arc::clone(&self.fields);
        let Some(field) = fields.get(self.tab) else {
            return false;
        };
        let textual = textual(field);
        let editing_custom = !textual;
        let mut edited = true;
        match (ev.code, ev.modifiers) {
            (KeyCode::Esc, _) => {
                if textual {
                    self.done = Some(FormOutcome::Cancel);
                } else {
                    self.editing = false;
                }
                edited = false;
            }
            (KeyCode::Tab, m) if !m.contains(KeyModifiers::SHIFT) => {
                self.submit_input(field, 1);
                edited = false;
            }
            (KeyCode::BackTab, _) | (KeyCode::Tab, _) => {
                self.submit_input(field, -1);
                edited = false;
            }
            (KeyCode::Enter, _) => {
                edited = false;
                if textual {
                    self.submit_input(field, 1);
                } else {
                    let text = self.input.trim().to_owned();
                    let multi = matches!(field, FormField::Multiselect { .. });
                    if !self.commit_input(field, &text) || multi || text.is_empty() {
                        return true;
                    }
                    if self.single() {
                        self.reply_single(field, FormValue::String(text));
                    } else {
                        self.enter_tab(self.tab + 1);
                    }
                }
            }
            (KeyCode::Up, _) if editing_custom && self.selected > 0 => {
                self.editing = false;
                self.selected -= 1;
                edited = false;
            }
            (KeyCode::Backspace, _) => {
                self.input.pop();
            }
            (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => self.input.clear(),
            (KeyCode::Char('w'), m) if m.contains(KeyModifiers::CONTROL) => {
                let kept = self.input.trim_end().rfind(' ').map_or(0, |at| at + 1);
                self.input.truncate(kept);
            }
            _ => match printable(&ev) {
                Some(c) => self.input.push(c),
                None => return false,
            },
        }
        if edited && editing_custom {
            self.update_custom(field);
        }
        true
    }

    // ---- actions ---------------------------------------------------------

    /// Move to `index` (a tab, possibly the review tab) and load its input.
    fn enter_tab(&mut self, index: usize) {
        self.tab = index;
        self.editing = false;
        self.error.clear();
        self.input.clear();
        let fields = Arc::clone(&self.fields);
        let Some(field) = fields.get(index).filter(|_| !self.confirm()) else {
            self.selected = 0;
            return;
        };
        self.selected = self.selected_for(field, self.answers.get(field.key()));
        if textual(field) {
            self.input = match self.custom.get(field.key()).filter(|text| !text.is_empty()) {
                Some(text) => text.clone(),
                None => self
                    .answers
                    .get(field.key())
                    .map(|value| self.display_value(field, value, ""))
                    .unwrap_or_default(),
            };
        }
    }

    /// Enter on the highlighted row of a select field.
    fn select_option(&mut self, field: &FormField) {
        let rows = self.rows(field);
        let key = field.key();
        if custom_of(field) && self.selected == rows.len() {
            let text = self.custom.get(key).cloned().unwrap_or_default();
            let multi = matches!(field, FormField::Multiselect { .. });
            if multi && !text.is_empty() && self.custom_picked(field) {
                self.toggle(field, &text);
                return;
            }
            self.editing = true;
            self.input = text;
            return;
        }
        let Some(row) = rows.get(self.selected) else {
            return;
        };
        if matches!(field, FormField::Multiselect { .. }) {
            if let FormValue::String(value) = &row.value {
                self.toggle(field, value);
            }
            return;
        }
        self.pick(field, row.value.clone(), None);
    }

    fn pick(&mut self, field: &FormField, value: FormValue, custom: Option<String>) {
        if let Some(invalid) = validate(field, Some(&value)) {
            self.error = invalid;
            return;
        }
        let single = self.single();
        self.answer(field, Some(value.clone()));
        if let Some(text) = custom {
            self.custom.insert(field.key().to_owned(), text);
        }
        if single {
            self.reply_single(field, value);
        } else {
            self.enter_tab(self.tab + 1);
        }
    }

    fn reply_single(&mut self, field: &FormField, value: FormValue) {
        let mut answer = Answer::new();
        answer.insert(field.key().to_owned(), value);
        self.done = Some(FormOutcome::Submit(answer));
    }

    fn toggle(&mut self, field: &FormField, item: &str) {
        let mut values = multiple(self.answers.get(field.key()));
        match values.iter().position(|value| value == item) {
            Some(at) => {
                values.remove(at);
            }
            None => values.push(item.to_owned()),
        }
        self.answer(field, Some(FormValue::Multiple(values)));
    }

    /// Store `value` (or clear it). An emptied optional multiselect is
    /// unanswered rather than `[]`.
    fn answer(&mut self, field: &FormField, value: Option<FormValue>) {
        let value = match value {
            Some(FormValue::Multiple(items))
                if items.is_empty()
                    && matches!(field, FormField::Multiselect { .. })
                    && !required_of(field) =>
            {
                None
            }
            other => other,
        };
        match value {
            Some(value) => self.answers.insert(field.key().to_owned(), value),
            None => self.answers.remove(field.key()),
        };
        self.error.clear();
    }

    /// Live-update the custom answer from `self.input` while it is edited.
    fn update_custom(&mut self, field: &FormField) {
        let key = field.key();
        let previous = self.custom.get(key).cloned();
        if previous.as_deref() == Some(self.input.as_str()) {
            return;
        }
        let next = self.input.clone();
        self.custom.insert(key.to_owned(), next.clone());
        let value = if matches!(field, FormField::Multiselect { .. }) {
            Some(FormValue::Multiple(set_multiselect_custom(
                self.answers.get(key),
                previous.as_deref(),
                &next,
            )))
        } else {
            (!next.is_empty()).then_some(FormValue::String(next))
        };
        self.answer(field, value);
    }

    /// Commit the edited text as the answer. `false` keeps the user here with
    /// `error` set.
    fn commit_input(&mut self, field: &FormField, text: &str) -> bool {
        let key = field.key();
        let textual = textual(field);
        let multi = matches!(field, FormField::Multiselect { .. });
        if text.is_empty() {
            let value = (!textual && multi).then(|| {
                FormValue::Multiple(set_multiselect_custom(
                    self.answers.get(key),
                    self.custom.get(key).map(String::as_str),
                    "",
                ))
            });
            if (textual || !multi)
                && let Some(invalid) = validate(field, value.as_ref())
            {
                self.error = invalid;
                return false;
            }
            self.answer(field, value);
            self.custom.insert(key.to_owned(), String::new());
            self.editing = false;
            return true;
        }
        if textual {
            let value = match field {
                FormField::Number { .. } | FormField::Integer { .. } => match text.parse::<f64>() {
                    Ok(number) => FormValue::Number(number),
                    Err(_) => FormValue::Number(f64::NAN),
                },
                _ => FormValue::String(text.to_owned()),
            };
            if let Some(invalid) = validate(field, Some(&value)) {
                self.error = invalid;
                return false;
            }
            self.answer(field, Some(value));
        } else if multi {
            let next = set_multiselect_custom(
                self.answers.get(key),
                self.custom.get(key).map(String::as_str),
                text,
            );
            self.answer(field, Some(FormValue::Multiple(next)));
        } else {
            let value = FormValue::String(text.to_owned());
            if let Some(invalid) = validate(field, Some(&value)) {
                self.error = invalid;
                return false;
            }
            self.answer(field, Some(value));
        }
        let configured = matches!(
            field,
            FormField::String { options: Some(options), .. } if options.iter().any(|option| option.value == text)
        );
        self.custom.insert(
            key.to_owned(),
            if configured {
                String::new()
            } else {
                text.to_owned()
            },
        );
        self.editing = false;
        true
    }

    fn submit_input(&mut self, field: &FormField, direction: i32) {
        let text = self.input.trim().to_owned();
        let tabs = self.tabs();
        let step = |tab: usize| {
            if direction < 0 {
                (tab + tabs - 1) % tabs
            } else {
                (tab + 1) % tabs
            }
        };
        if !self.commit_input(field, &text) {
            if direction < 0 {
                self.enter_tab(step(self.tab));
            }
            return;
        }
        if !self.single() {
            self.enter_tab(step(self.tab));
        }
    }

    /// Validate everything and finish with the answer map.
    fn submit(&mut self) {
        if let Some(open) = self.fields.iter().find(|field| {
            matches!(field, FormField::External { .. })
                && self.answers.get(field.key()) != Some(&FormValue::Boolean(true))
        }) {
            self.error = format!("External action must be acknowledged: {}", label_of(open));
            return;
        }
        if let Some(message) = self
            .fields
            .iter()
            .filter(|field| !matches!(field, FormField::External { .. }))
            .find_map(|field| validate(field, self.answers.get(field.key())))
        {
            self.error = message;
            return;
        }
        let answer = self
            .fields
            .iter()
            .filter_map(|field| {
                self.answers
                    .get(field.key())
                    .map(|value| (field.key().to_owned(), value.clone()))
            })
            .collect();
        self.done = Some(FormOutcome::Submit(answer));
    }

    // ---- derived ---------------------------------------------------------

    fn rows(&self, field: &FormField) -> Vec<Row> {
        match field {
            FormField::Boolean { .. } => ["Yes", "No"]
                .into_iter()
                .zip([true, false])
                .map(|(label, value)| Row {
                    value: FormValue::Boolean(value),
                    label: label.to_owned(),
                    description: None,
                })
                .collect(),
            FormField::String {
                options: Some(options),
                ..
            } => options_to_rows(options),
            FormField::Multiselect { options, key, .. } => {
                let mut rows = options_to_rows(options);
                // Values answered but not configured (typed customs) stay listed.
                let typed = self.custom.get(key);
                if let Some(FormValue::Multiple(values)) = self.answers.get(key) {
                    for value in values {
                        if !options.iter().any(|option| option.value == *value)
                            && Some(value) != typed
                        {
                            rows.push(Row {
                                value: FormValue::String(value.clone()),
                                label: value.clone(),
                                description: None,
                            });
                        }
                    }
                }
                rows
            }
            _ => Vec::new(),
        }
    }

    fn selected_for(&self, field: &FormField, value: Option<&FormValue>) -> usize {
        let Some(value) = value else { return 0 };
        if matches!(value, FormValue::Multiple(_)) {
            return 0;
        }
        let rows = self.rows(field);
        if let Some(at) = rows.iter().position(|row| row.value == *value) {
            return at;
        }
        if matches!(value, FormValue::String(_)) && custom_of(field) {
            return rows.len();
        }
        0
    }

    fn custom_picked(&self, field: &FormField) -> bool {
        let Some(text) = self.custom.get(field.key()).filter(|text| !text.is_empty()) else {
            return false;
        };
        match self.answers.get(field.key()) {
            Some(FormValue::Multiple(values)) => values.contains(text),
            Some(FormValue::String(value)) => value == text,
            _ => false,
        }
    }

    fn picked(&self, field: &FormField, row: &Row) -> bool {
        match self.answers.get(field.key()) {
            Some(FormValue::Multiple(values)) => {
                matches!(&row.value, FormValue::String(value) if values.contains(value))
            }
            Some(value) => *value == row.value,
            None => false,
        }
    }

    fn display_value(
        &self,
        field: &FormField,
        value: &FormValue,
        empty_multiselect: &str,
    ) -> String {
        let rows = self.rows(field);
        let label = |item: &FormValue| match rows.iter().find(|row| row.value == *item) {
            Some(row) => row.label.clone(),
            None => plain(item),
        };
        match value {
            FormValue::Multiple(items) if items.is_empty() => empty_multiselect.to_owned(),
            FormValue::Multiple(items) => items
                .iter()
                .map(|item| label(&FormValue::String(item.clone())))
                .collect::<Vec<_>>()
                .join(", "),
            other => label(other),
        }
    }

    fn answered(&self) -> usize {
        self.fields
            .iter()
            .filter(|field| match self.answers.get(field.key()) {
                None => false,
                Some(value) if matches!(field, FormField::External { .. }) => {
                    *value == FormValue::Boolean(true)
                }
                Some(value) => validate(field, Some(value)).is_none(),
            })
            .count()
    }
}

// ---- field accessors -------------------------------------------------------

fn title_of(field: &FormField) -> Option<&str> {
    match field {
        FormField::String { title, .. }
        | FormField::Number { title, .. }
        | FormField::Integer { title, .. }
        | FormField::Boolean { title, .. }
        | FormField::Multiselect { title, .. }
        | FormField::External { title, .. } => title.as_deref(),
        FormField::Unsupported => None,
    }
}

fn description_of(field: &FormField) -> Option<&str> {
    match field {
        FormField::String { description, .. }
        | FormField::Number { description, .. }
        | FormField::Integer { description, .. }
        | FormField::Boolean { description, .. }
        | FormField::Multiselect { description, .. }
        | FormField::External { description, .. } => description.as_deref(),
        FormField::Unsupported => None,
    }
}

fn label_of(field: &FormField) -> &str {
    title_of(field).unwrap_or(match field {
        FormField::External { url, .. } => url,
        FormField::Unsupported => "Unsupported field",
        other => other.key(),
    })
}

fn required_of(field: &FormField) -> bool {
    match field {
        FormField::String { required, .. }
        | FormField::Number { required, .. }
        | FormField::Integer { required, .. }
        | FormField::Boolean { required, .. }
        | FormField::Multiselect { required, .. } => required.unwrap_or(false),
        FormField::External { .. } | FormField::Unsupported => false,
    }
}

/// Numbers and option-less strings are typed, not picked.
fn textual(field: &FormField) -> bool {
    matches!(
        field,
        FormField::Number { .. }
            | FormField::Integer { .. }
            | FormField::String { options: None, .. }
    )
}

/// Does the field offer a "Type your own answer" row?
fn custom_of(field: &FormField) -> bool {
    match field {
        FormField::String {
            options: Some(_),
            custom,
            ..
        }
        | FormField::Multiselect { custom, .. } => *custom == Some(true),
        _ => false,
    }
}

fn options_to_rows(options: &[FormOption]) -> Vec<Row> {
    options
        .iter()
        .map(|option| Row {
            value: FormValue::String(option.value.clone()),
            label: option.label.clone(),
            description: option.description.clone(),
        })
        .collect()
}

fn multiple(value: Option<&FormValue>) -> Vec<String> {
    match value {
        Some(FormValue::Multiple(items)) => items.clone(),
        _ => Vec::new(),
    }
}

/// `formSetMultiselectCustom`: swap the previous typed value for the new one.
fn set_multiselect_custom(
    value: Option<&FormValue>,
    previous: Option<&str>,
    next: &str,
) -> Vec<String> {
    let mut items = multiple(value);
    if let Some(at) = previous.and_then(|previous| items.iter().position(|item| item == previous)) {
        items.remove(at);
    }
    if !next.is_empty() && !items.iter().any(|item| item == next) {
        items.push(next.to_owned());
    }
    items
}

fn printable(ev: &KeyEvent) -> Option<char> {
    match ev.code {
        KeyCode::Char(c)
            if !c.is_control()
                && !ev
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            Some(c)
        }
        _ => None,
    }
}

fn plain(value: &FormValue) -> String {
    match value {
        FormValue::String(text) => text.clone(),
        FormValue::Number(number) => number.to_string(),
        FormValue::Boolean(flag) => flag.to_string(),
        FormValue::Multiple(items) => items.join(", "),
    }
}

/// Port of `formValidateValue` for the keywords `ox-client` models.
#[must_use]
pub fn validate(field: &FormField, value: Option<&FormValue>) -> Option<String> {
    let required = required_of(field);
    let Some(value) = value else {
        return required.then(|| "Answer required".to_owned());
    };
    let multiselect = matches!(field, FormField::Multiselect { .. });
    let empty = match value {
        FormValue::String(text) => text.is_empty(),
        FormValue::Multiple(items) => items.is_empty(),
        _ => false,
    };
    if required && empty {
        return Some(
            if multiselect {
                "Select at least one option"
            } else {
                "Answer required"
            }
            .to_owned(),
        );
    }
    match field {
        FormField::String {
            options, custom, ..
        } => {
            let FormValue::String(text) = value else {
                return Some("Expected text".to_owned());
            };
            match options {
                Some(options)
                    if *custom != Some(true) && !options.iter().any(|o| o.value == *text) =>
                {
                    Some("Select an available option".to_owned())
                }
                _ => None,
            }
        }
        FormField::Number {
            minimum, maximum, ..
        }
        | FormField::Integer {
            minimum, maximum, ..
        } => {
            let FormValue::Number(number) = value else {
                return Some("Expected a number".to_owned());
            };
            if !number.is_finite() {
                return Some("Expected a number".to_owned());
            }
            if matches!(field, FormField::Integer { .. }) && number.fract() != 0.0 {
                return Some("Expected an integer".to_owned());
            }
            if let Some(min) = minimum
                && number < min
            {
                return Some(format!("Must be at least {min}"));
            }
            if let Some(max) = maximum
                && number > max
            {
                return Some(format!("Must be at most {max}"));
            }
            None
        }
        FormField::Boolean { .. } => {
            (!matches!(value, FormValue::Boolean(_))).then(|| "Expected yes or no".to_owned())
        }
        FormField::Multiselect {
            options, custom, ..
        } => {
            let FormValue::Multiple(items) = value else {
                return Some("Expected selections".to_owned());
            };
            (*custom != Some(true)
                && items
                    .iter()
                    .any(|item| !options.iter().any(|o| o.value == *item)))
            .then(|| "Select only available options".to_owned())
        }
        FormField::External { .. } | FormField::Unsupported => None,
    }
}

// ---- rendering --------------------------------------------------------------

const INDENT: &str = "  ";

struct Look {
    text: Style,
    dim: Style,
    primary: Style,
    error: Style,
    success: Style,
}

/// Lines of the form body plus the bits `draw` needs to window and place the
/// cursor.
struct Doc {
    lines: Vec<Line<'static>>,
    /// The lines that may scroll away when the terminal is short.
    region: Range<usize>,
    /// The lines inside `region` that must stay visible.
    focus: Range<usize>,
    /// `(line, column)` relative to the content origin.
    cursor: Option<(usize, u16)>,
}

impl Doc {
    fn push(&mut self, line: Line<'static>) {
        self.lines.push(line);
    }

    fn blank(&mut self) {
        self.lines.push(Line::default());
    }

    /// Drop region lines until at most `max` lines remain, keeping `focus`.
    fn fit(&mut self, max: usize) {
        if self.lines.len() <= max {
            return;
        }
        let region = self.region.clone();
        let hidden = (self.lines.len() - max).min(region.len().saturating_sub(1));
        if hidden == 0 {
            self.lines.truncate(max);
            return;
        }
        let visible = region.len() - hidden;
        let focus_start = self.focus.start.saturating_sub(region.start);
        let focus_end = self.focus.end.saturating_sub(region.start);
        let skip = focus_end.saturating_sub(visible).min(focus_start);
        let (from, to) = (region.start + skip, region.start + skip + visible);
        self.cursor = self.cursor.and_then(|(line, col)| {
            let moved = if line < region.start {
                Some(line)
            } else if line >= region.end {
                Some(line - hidden)
            } else if (from..to).contains(&line) {
                Some(line - skip)
            } else {
                None
            };
            moved.map(|line| (line, col))
        });
        let mut index = 0;
        self.lines.retain(|_| {
            let keep = index < region.start || index >= region.end || (from..to).contains(&index);
            index += 1;
            keep
        });
        self.lines.truncate(max);
    }
}

fn truncate_label(label: &str, max: usize) -> String {
    if label.chars().count() > max {
        let head: String = label.chars().take(max - 1).collect();
        format!("{}…", head.trim_end())
    } else {
        label.to_owned()
    }
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for line in text.split('\n') {
        if line.is_empty() {
            out.push(String::new());
            continue;
        }
        let options =
            textwrap::Options::new(width).word_splitter(textwrap::WordSplitter::NoHyphenation);
        out.extend(textwrap::wrap(line, options).into_iter().map(String::from));
    }
    out
}

/// The last `width` columns of `text`, so a long answer keeps its tail (and
/// the cursor) in view.
fn tail(text: &str, width: usize) -> String {
    let mut taken = 0;
    let mut kept: Vec<char> = Vec::new();
    for c in text.chars().rev() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if taken + w > width {
            break;
        }
        taken += w;
        kept.push(c);
    }
    kept.into_iter().rev().collect()
}

fn line(indent: usize, spans: Vec<Span<'static>>) -> Line<'static> {
    let mut all = vec![Span::raw(" ".repeat(indent))];
    all.extend(spans);
    Line::from(all)
}

impl FormState {
    fn action_label(&self) -> &'static str {
        if self.confirm() {
            return "submit";
        }
        match self.current() {
            Some(FormField::External { key, .. }) => {
                if self.answers.get(key) == Some(&FormValue::Boolean(true)) {
                    "continue"
                } else {
                    "acknowledge"
                }
            }
            Some(field @ FormField::Multiselect { .. }) => {
                let rows = self.rows(field);
                let on_other = custom_of(field) && self.selected == rows.len();
                if on_other && self.editing {
                    "done"
                } else if on_other && self.input_or_custom(field).is_empty() {
                    "edit"
                } else {
                    "toggle"
                }
            }
            Some(FormField::Unsupported) => "continue",
            _ if self.single() => "submit",
            _ => "confirm",
        }
    }

    fn input_or_custom(&self, field: &FormField) -> &str {
        if self.editing {
            &self.input
        } else {
            self.custom.get(field.key()).map_or("", String::as_str)
        }
    }

    fn document(&self, width: usize, screen_width: u16, look: &Look) -> Doc {
        let mut doc = Doc {
            lines: Vec::new(),
            region: 0..0,
            focus: 0..0,
            cursor: None,
        };
        doc.blank();
        for text in wrap(&self.title, width) {
            doc.push(line(2, vec![Span::styled(text, look.dim)]));
        }
        if let Some(message) = &self.message {
            doc.blank();
            for text in wrap(message, width) {
                doc.push(line(2, vec![Span::styled(text, look.text)]));
            }
        }
        if !self.single() {
            doc.blank();
            self.tab_bar(&mut doc, screen_width, look);
        }
        doc.blank();
        match self.current() {
            Some(field @ FormField::External { .. }) => {
                self.external_body(field, &mut doc, width, look)
            }
            Some(FormField::Unsupported) => {
                doc.push(line(
                    2,
                    vec![Span::styled("This field type is not supported", look.text)],
                ));
                doc.push(line(
                    2,
                    vec![Span::styled(
                        "It is skipped; the rest still submits",
                        look.dim,
                    )],
                ));
            }
            Some(field) => self.field_body(field, &mut doc, width, look),
            None => self.review_body(&mut doc, look),
        }
        doc.blank();
        doc.push(self.footer(width + 5, look));
        doc.blank();
        doc
    }

    fn tab_bar(&self, doc: &mut Doc, screen_width: u16, look: &Look) {
        let labels: Vec<String> = self
            .fields
            .iter()
            .map(|f| truncate_label(label_of(f), 24))
            .collect();
        let needed: usize =
            labels.iter().map(|l| l.chars().count() + 3).sum::<usize>() + "Submit".len() + 3;
        if needed <= usize::from(screen_width).saturating_sub(8) {
            let mut spans = Vec::new();
            for (index, label) in labels.into_iter().enumerate() {
                let style = if index == self.tab {
                    look.text.add_modifier(Modifier::BOLD)
                } else {
                    look.dim
                };
                spans.push(Span::styled(label, style));
                spans.push(Span::raw("  "));
            }
            let submit = if self.confirm() {
                look.text.add_modifier(Modifier::BOLD)
            } else {
                look.dim
            };
            spans.push(Span::styled("Submit", submit));
            doc.push(line(2, spans));
        } else {
            let head = if self.confirm() {
                "Review".to_owned()
            } else {
                format!(
                    "Field {} of {}",
                    self.tab.min(self.fields.len().saturating_sub(1)) + 1,
                    self.fields.len()
                )
            };
            let mut spans = vec![Span::styled(head, look.dim)];
            if !self.fields.is_empty() {
                spans.push(Span::raw("   "));
                spans.push(Span::styled(
                    format!("· {}/{} completed", self.answered(), self.fields.len()),
                    look.dim,
                ));
            }
            doc.push(line(2, spans));
        }
    }

    fn external_body(&self, field: &FormField, doc: &mut Doc, width: usize, look: &Look) {
        if let Some(title) = title_of(field) {
            doc.push(line(2, vec![Span::styled(title.to_owned(), look.text)]));
            doc.blank();
        }
        if let Some(text) = description_of(field) {
            for part in wrap(text, width) {
                doc.push(line(2, vec![Span::styled(part, look.dim)]));
            }
            doc.blank();
        }
        if let FormField::External { url, .. } = field {
            doc.push(line(2, vec![Span::styled(url.clone(), look.primary)]));
        }
    }

    fn field_body(&self, field: &FormField, doc: &mut Doc, width: usize, look: &Look) {
        let heading = description_of(field).unwrap_or_else(|| label_of(field));
        for part in wrap(heading, width) {
            doc.push(line(2, vec![Span::styled(part, look.text)]));
        }
        doc.blank();
        if textual(field) {
            let placeholder = placeholder_of(field);
            let text = tail(&self.input, width.saturating_sub(1));
            let at = doc.lines.len();
            if self.input.is_empty() {
                doc.push(line(3, vec![Span::styled(placeholder, look.dim)]));
                doc.cursor = Some((at, 3));
            } else {
                let col = 3 + text.width() as u16;
                doc.push(line(3, vec![Span::styled(text, look.text)]));
                doc.cursor = Some((at, col));
            }
            return;
        }
        let multi = matches!(field, FormField::Multiselect { .. });
        let rows = self.rows(field);
        doc.region.start = doc.lines.len();
        for (index, row) in rows.iter().enumerate() {
            let active = index == self.selected;
            let start = doc.lines.len();
            let picked = self.picked(field, row);
            let number = if active { look.primary } else { look.dim };
            let mut spans = vec![
                Span::styled(format!("{}.", index + 1), number),
                Span::raw(" "),
            ];
            if multi {
                let style = if active {
                    look.primary
                } else if picked {
                    look.success
                } else {
                    look.dim
                };
                spans.push(Span::styled(
                    format!("[{}] ", if picked { "✓" } else { " " }),
                    style,
                ));
            }
            spans.push(Span::styled(
                row.label.clone(),
                if active { look.primary } else { look.text },
            ));
            if !multi && picked {
                spans.push(Span::styled(" ✓", look.success));
            }
            doc.push(line(2, spans));
            if let Some(text) = &row.description {
                let pad = if multi { 7 } else { 3 };
                for part in wrap(text, width.saturating_sub(pad)) {
                    doc.push(line(2 + pad, vec![Span::styled(part, look.dim)]));
                }
            }
            if active {
                doc.focus = start..doc.lines.len();
            }
        }
        if custom_of(field) {
            let index = rows.len();
            let other = self.selected == index;
            let start = doc.lines.len();
            let typed = self.custom.get(field.key()).map_or("", String::as_str);
            let picked = self.custom_picked(field);
            let number = if other { look.primary } else { look.dim };
            let mut spans = vec![
                Span::styled(format!("{}.", index + 1), number),
                Span::raw(" "),
            ];
            let mut col = 2 + format!("{}.", index + 1).width() + 1;
            if multi {
                let checked = picked || (other && self.editing);
                let style = if other {
                    look.primary
                } else if checked {
                    look.success
                } else {
                    look.dim
                };
                spans.push(Span::styled(
                    format!("[{}] ", if checked { "✓" } else { " " }),
                    style,
                ));
                col += 4;
            }
            if self.editing {
                let room = width.saturating_sub(col - 2 + 1);
                let text = tail(&self.input, room);
                let at = doc.lines.len();
                doc.cursor = Some((at, (col + text.width()) as u16));
                if text.is_empty() {
                    spans.push(Span::styled("Type your own answer", look.dim));
                } else {
                    spans.push(Span::styled(text, look.primary));
                }
            } else {
                let shown = if typed.is_empty() {
                    "Type your own answer"
                } else {
                    typed
                };
                spans.push(Span::styled(
                    shown.to_owned(),
                    if other { look.primary } else { look.text },
                ));
                if !multi && picked {
                    spans.push(Span::styled("✓", look.success));
                }
            }
            doc.push(line(2, spans));
            if other {
                doc.focus = start..doc.lines.len();
            }
        }
        doc.region.end = doc.lines.len();
    }

    fn review_body(&self, doc: &mut Doc, look: &Look) {
        doc.region.start = doc.lines.len();
        for field in self.fields.iter() {
            let mut spans = vec![
                Span::styled(
                    format!("{}:", truncate_label(label_of(field), 40)),
                    look.dim,
                ),
                Span::raw(" "),
            ];
            if matches!(field, FormField::Unsupported) {
                spans.push(Span::styled("(field type not supported)", look.dim));
            } else if matches!(field, FormField::External { .. }) {
                if self.answers.get(field.key()) == Some(&FormValue::Boolean(true)) {
                    spans.push(Span::styled("Acknowledged", look.success));
                } else {
                    spans.push(Span::styled("(acknowledgement required)", look.error));
                }
            } else {
                let value = self.answers.get(field.key());
                let invalid = validate(field, value);
                let missing = value.is_none() && required_of(field);
                let (text, style) = match (&invalid, value) {
                    (Some(message), _) => (message.clone(), look.error),
                    (None, Some(value)) => (self.display_value(field, value, "(none)"), look.text),
                    (None, None) if missing => ("(required)".to_owned(), look.error),
                    (None, None) => ("(not answered)".to_owned(), look.dim),
                };
                spans.push(Span::styled(text, if missing { look.error } else { style }));
            }
            doc.push(line(2, spans));
        }
        doc.focus = doc.region.start..doc.region.start;
        doc.region.end = doc.lines.len();
    }

    fn footer(&self, inner_width: usize, look: &Look) -> Line<'static> {
        let key = |symbol: &str, action: &str| {
            vec![
                Span::styled(format!("{symbol} "), look.text),
                Span::styled(action.to_owned(), look.dim),
            ]
        };
        let mut items: Vec<Vec<Span<'static>>> = Vec::new();
        if !self.single() {
            items.push(key("⇆", "tab"));
        }
        let external = matches!(
            self.current(),
            Some(FormField::External { .. } | FormField::Unsupported)
        );
        if !self.confirm() && !self.textual_now() && !external && !self.editing {
            items.push(key("↑↓", "select"));
        }
        items.push(key("enter", self.action_label()));
        let close = if self.editing && !self.textual_now() {
            "close"
        } else {
            "dismiss"
        };
        items.push(key("esc", close));
        let mut spans = vec![Span::raw(INDENT)];
        for (index, item) in items.into_iter().enumerate() {
            if index > 0 {
                spans.push(Span::raw("  "));
            }
            spans.extend(item);
        }
        if !self.error.is_empty() {
            let used: usize = spans.iter().map(|s| s.content.width()).sum();
            let room = inner_width.saturating_sub(3 + used + 1);
            let message = tail_end(&self.error, room);
            let pad = inner_width.saturating_sub(3 + used + message.width());
            if !message.is_empty() {
                spans.push(Span::raw(" ".repeat(pad)));
                spans.push(Span::styled(message, look.error));
            }
        }
        Line::from(spans)
    }
}

/// The first `width` columns of `text`.
fn tail_end(text: &str, width: usize) -> String {
    let mut taken = 0;
    let mut out = String::new();
    for c in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if taken + w > width {
            break;
        }
        taken += w;
        out.push(c);
    }
    out
}

fn placeholder_of(field: &FormField) -> String {
    match field {
        FormField::String {
            placeholder: Some(text),
            ..
        } if !text.is_empty() => text.clone(),
        FormField::Number {
            minimum, maximum, ..
        }
        | FormField::Integer {
            minimum, maximum, ..
        } => match (minimum, maximum) {
            (Some(min), Some(max)) => format!("{min}-{max}"),
            (Some(min), None) => format!("at least {min}"),
            (None, Some(max)) => format!("at most {max}"),
            (None, None) => "Type your answer".to_owned(),
        },
        _ => "Type your answer".to_owned(),
    }
}

/// Paint the form over the bottom of `area`: a raised box with a primary left
/// rule, one row above the status line (upstream puts it where the prompt
/// was). Returns where the input cursor goes while text is being typed.
pub fn draw(buf: &mut Buffer, area: Rect, form: &FormState, pal: &Palette) -> Option<Position> {
    let outer = layout::padded(area, area.width);
    if outer.width < 12 || area.height < 4 {
        return None;
    }
    let look = Look {
        text: pal.text,
        dim: pal.dim,
        primary: pal.accent.remove_modifier(Modifier::BOLD),
        error: pal.warn,
        success: pal.accent.remove_modifier(Modifier::BOLD),
    };
    let text_width = usize::from(outer.width).saturating_sub(6);
    let mut doc = form.document(text_width, area.width, &look);
    doc.fit(usize::from(area.height - 1));

    let height = doc.lines.len() as u16;
    let bottom = area.bottom() - 1;
    let surface = Rect {
        x: outer.x,
        y: bottom - height,
        width: outer.width,
        height,
    };
    Clear.render(surface, buf);
    buf.set_style(surface, pal.raised_base);
    let rule = look.primary;
    let inner = surface.width - 1;
    for (index, line) in doc.lines.iter().enumerate() {
        let y = surface.y + index as u16;
        buf.set_string(surface.x, y, "┃", rule);
        buf.set_line(surface.x + 1, y, line, inner);
    }
    doc.cursor.map(|(line, col)| {
        Position::new(
            (surface.x + 1 + col).min(surface.right() - 1),
            surface.y + line as u16,
        )
    })
}
