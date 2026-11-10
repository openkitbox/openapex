//! `/models`, `/agents` and the `Tab` agent cycle, wired onto [`App`].
//!
//! The decisions (grouping, filtering, who is next) are pure and live in
//! `ui::models`; this file only moves their results between the dialog, the
//! command channel and the prompt chrome.
//!
//! Flow: a command asks the executor for the catalogue (`ModelList` /
//! `AgentList`), the answer opens a `Select` and records its
//! [`DialogPurpose`], `Selected(value)` is decoded per purpose into a
//! [`Switch`], and the server's acknowledgement (`Switched`) updates the
//! chrome. With no session yet (Home) there is nothing to POST to, so the
//! choice is applied to the chrome directly.

use crossterm::event::{KeyCode, KeyEvent};
use ox_client::rest::{AgentRef, ModelInfo, ProviderRef};

use super::App;
use super::command::{AgentIntent, AppCommand, Switch};
use crate::bootstrap::display_agent;
use crate::ui::dialog::Dialog;
use crate::ui::models::{self, DialogPurpose};

impl App {
    pub(super) fn on_models(&mut self, models: Vec<ModelInfo>, providers: Vec<ProviderRef>) {
        if self.form_open() {
            return;
        }
        let chrome = self.prompt.chrome();
        let current = models::current_model_value(
            &models,
            &providers,
            chrome.model.as_deref(),
            chrome.provider.as_deref(),
        );
        self.dialog = Some(Dialog::Select(models::model_select(
            &models, &providers, current,
        )));
        self.dialog_purpose = Some(DialogPurpose::Model(providers));
        self.dirty = true;
    }

    pub(super) fn on_agents(&mut self, agents: Vec<AgentRef>, intent: AgentIntent) {
        let agents = models::primary_agents(agents);
        match intent {
            AgentIntent::Dialog => {
                if self.form_open() {
                    return;
                }
                let current = self.prompt.chrome().agent.as_deref();
                self.dialog = Some(Dialog::Select(models::agent_select(&agents, current)));
                self.dialog_purpose = Some(DialogPurpose::Agent(agents));
                self.dirty = true;
            }
            AgentIntent::Cycle(step) => {
                let current = self.prompt.chrome().agent.as_deref();
                if let Some(next) = models::next_agent(&agents, current, step) {
                    let switch = Switch::Agent {
                        id: next.id.clone(),
                        label: display_agent(&next.name),
                    };
                    self.apply_switch(switch);
                }
            }
        }
    }

    /// `DialogOutcome::Selected`: act on the value according to the purpose
    /// the dialog was opened for.
    pub(super) fn on_selected(&mut self, value: String) {
        let switch = match self.dialog_purpose.take() {
            Some(DialogPurpose::Model(providers)) => {
                models::decode(&value).map(|(provider_id, model_id)| Switch::Model {
                    provider_id: provider_id.to_owned(),
                    model_id: model_id.to_owned(),
                    variant: None,
                    provider_label: models::provider_label(&providers, provider_id).to_owned(),
                })
            }
            Some(DialogPurpose::Agent(agents)) => agents
                .iter()
                .find(|agent| agent.id == value)
                .map(|agent| Switch::Agent {
                    id: agent.id.clone(),
                    label: display_agent(&agent.name),
                }),
            None => None,
        };
        if let Some(switch) = switch {
            self.apply_switch(switch);
        }
    }

    /// Ask the server to apply `switch`, or apply it locally on Home where
    /// no session exists yet.
    fn apply_switch(&mut self, switch: Switch) {
        match self.session.clone() {
            Some(session) => {
                let _ = self.commands.send(AppCommand::Switch { session, switch });
            }
            None => self.on_switched(switch),
        }
    }

    /// The change took effect: the metadata row follows.
    pub(super) fn on_switched(&mut self, switch: Switch) {
        match switch {
            Switch::Model {
                model_id,
                provider_label,
                ..
            } => self.prompt.set_model(model_id, Some(provider_label)),
            Switch::Agent { label, .. } => self.prompt.set_agent(label),
        }
        self.dirty = true;
    }

    /// `Tab` / `shift+tab` with the slash menu closed cycle the agent. The
    /// list is fetched per press, so a single-agent server makes it a no-op
    /// in `on_agents` (`next_agent` needs two).
    pub(super) fn cycle_agent_key(&mut self, key: &KeyEvent) -> bool {
        if self.prompt.autocomplete_visible() {
            return false;
        }
        let step = match key.code {
            KeyCode::Tab => 1,
            KeyCode::BackTab => -1,
            _ => return false,
        };
        let _ = self
            .commands
            .send(AppCommand::AgentList(AgentIntent::Cycle(step)));
        true
    }
}
