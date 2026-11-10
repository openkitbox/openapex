//! `/models`, `/variants`, `/agents`, `ctrl+t` and the `Tab` agent cycle,
//! wired onto [`App`].
//!
//! The decisions (grouping, filtering, who is next, which level follows which)
//! are pure and live in `ui::models`; this file only moves their results
//! between the dialog, the command channel and the prompt chrome.
//!
//! Flow: a command asks the executor for the catalogue (`ModelList` /
//! `VariantList` / `AgentList`), the answer opens a `Select` and records its
//! [`DialogPurpose`], `Selected(value)` is decoded per purpose into a
//! [`Switch`], and the server's acknowledgement (`Switched`) updates the
//! chrome. With no session yet (Home) there is nothing to POST to, so the
//! choice is applied to the chrome directly and carried into the first
//! session by `App::session_model`.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ox_client::rest::{AgentRef, ModelInfo, ModelVariant, ProviderRef};

use super::App;
use super::command::{
    AgentIntent, AppCommand, SessionModel, Switch, SwitchRequest, VariantRequest,
};
use crate::bootstrap::display_agent;
use crate::ui::dialog::Dialog;
use crate::ui::models::{self, DialogPurpose};
use crate::ui::toast::Toast;

impl App {
    pub(super) fn invalidate_variant_requests(&mut self) {
        self.variant_request_id = self.variant_request_id.wrapping_add(1);
        self.variant_session = None;
    }

    pub(super) fn on_models(&mut self, models: Vec<ModelInfo>, providers: Vec<ProviderRef>) {
        self.invalidate_variant_requests();
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
        self.dialog_purpose = Some(DialogPurpose::Model(providers, models));
        self.dirty = true;
    }

    pub(super) fn on_variant_failed(&mut self, request: VariantRequest, err: String) {
        if self.variant_response_matches(&request) {
            self.show_toast(Toast::error("Failed to load variants", err, Instant::now()));
        }
    }

    fn variant_response_matches(&self, request: &VariantRequest) -> bool {
        request.request_id == self.variant_request_id
            && self.variant_session == request.session
            && self.variant_navigation_generation == request.navigation_generation
            && self.session == request.session
            && self.navigation_generation == request.navigation_generation
            && self.prompt.chrome().provider_id.as_deref() == Some(request.provider_id.as_str())
            && self.prompt.chrome().model.as_deref() == Some(request.model_id.as_str())
    }

    /// Answer to `VariantList`: open the picker, or apply the next level when
    /// `ctrl+t` asked.
    pub(super) fn on_variants(&mut self, request: VariantRequest, variants: Vec<ModelVariant>) {
        if !self.variant_response_matches(&request) || self.dialog.is_some() || self.form_open() {
            return;
        }
        let VariantRequest {
            provider_id,
            model_id,
            provider_label,
            cycle,
            ..
        } = request;
        if cycle {
            // Nothing to cycle on a plain model: do not POST a no-op switch.
            if !models::has_named_variants(&variants) {
                return;
            }
            let next = models::cycle_variant(self.prompt.chrome().variant.as_deref(), &variants)
                .map(str::to_owned);
            self.apply_model(provider_id, model_id, provider_label, next);
            return;
        }
        if !models::has_named_variants(&variants) {
            // An explicit `/variants` request on a plain model means
            // "default": clear any named level rather than opening a useless
            // one-row picker.
            if models::model_switch_needed(
                self.prompt.chrome().provider_id.as_deref(),
                self.prompt.chrome().model.as_deref(),
                self.prompt.chrome().variant.as_deref(),
                &provider_id,
                &model_id,
                None,
            ) {
                self.apply_model(provider_id, model_id, provider_label, None);
            }
            return;
        }
        let current_variant = self.prompt.chrome().variant.clone();
        self.dialog = Some(Dialog::Select(models::variant_select(
            &variants,
            current_variant.as_deref(),
        )));
        self.dialog_purpose = Some(DialogPurpose::Variant {
            provider_id,
            model_id,
            provider_label,
        });
        self.dirty = true;
    }

    pub(super) fn on_agents(&mut self, agents: Vec<AgentRef>, intent: AgentIntent) {
        self.invalidate_variant_requests();
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
        self.invalidate_variant_requests();
        match self.dialog_purpose.take() {
            Some(DialogPurpose::Model(providers, catalog)) => {
                if let Some((provider_id, model_id)) = models::decode(&value) {
                    let provider_label = models::provider_label(&providers, provider_id).to_owned();
                    let preference = self.model_preference.as_ref().and_then(|prefs| {
                        prefs.lookup_model(provider_id, model_id).map(str::to_owned)
                    });
                    let current_variant = self.prompt.chrome();
                    let current_variant = models::selected_variant(
                        self.session.is_some(),
                        current_variant.provider_id.as_deref(),
                        current_variant.model.as_deref(),
                        current_variant.variant.as_deref(),
                        provider_id,
                        model_id,
                        preference.as_deref(),
                    );
                    let variants = catalog
                        .iter()
                        .find(|model| {
                            model.provider_id == provider_id && model.model_id == model_id
                        })
                        .map(|model| model.variants.clone())
                        .unwrap_or_default();
                    if models::has_named_variants(&variants) {
                        self.dialog = Some(Dialog::Select(models::variant_select(
                            &variants,
                            current_variant.as_deref(),
                        )));
                        self.dialog_purpose = Some(DialogPurpose::Variant {
                            provider_id: provider_id.to_owned(),
                            model_id: model_id.to_owned(),
                            provider_label,
                        });
                        self.dirty = true;
                    } else {
                        self.apply_model(
                            provider_id.to_owned(),
                            model_id.to_owned(),
                            models::provider_label(&providers, provider_id).to_owned(),
                            None,
                        );
                        self.dialog_purpose = None;
                    }
                }
            }
            Some(DialogPurpose::Variant {
                provider_id,
                model_id,
                provider_label,
            }) => {
                let variant = (value != "default").then_some(value);
                self.apply_model(provider_id, model_id, provider_label, variant);
            }
            Some(DialogPurpose::Agent(agents)) => {
                if let Some(agent) = agents.iter().find(|agent| agent.id == value) {
                    self.apply_switch(Switch::Agent {
                        id: agent.id.clone(),
                        label: display_agent(&agent.name),
                    });
                }
            }
            None => {}
        }
    }

    /// `/variants` and `ctrl+t`: ask for the current model's levels.
    pub(super) fn request_variants(&mut self, cycle: bool) {
        let chrome = self.prompt.chrome();
        let (Some(provider_id), Some(model_id)) =
            (chrome.provider_id.clone(), chrome.model.clone())
        else {
            return;
        };
        let provider_label = chrome
            .provider
            .clone()
            .unwrap_or_else(|| provider_id.clone());
        let request_id = self.variant_request_id.wrapping_add(1);
        self.variant_request_id = request_id;
        self.variant_session = self.session.clone();
        self.variant_navigation_generation = self.navigation_generation;
        self.request_variant_list(VariantRequest {
            provider_id,
            model_id,
            provider_label,
            cycle,
            request_id,
            session: self.variant_session.clone(),
            navigation_generation: self.variant_navigation_generation,
        });
    }

    /// The model a first session is created with. Home has no server chrome,
    /// so its saved preference (including explicit `default`) wins over the
    /// placeholder row; an existing session uses its live chrome instead. The
    /// row is updated to the resolved level so it never advertises a turn the
    /// server won't run.
    pub(super) fn session_model(&mut self) -> Option<SessionModel> {
        let chrome = self.prompt.chrome();
        let provider_id = chrome.provider_id.clone()?;
        let model_id = chrome.model.clone()?;
        let variant = models::session_model_variant(
            self.session.is_some(),
            chrome.variant.as_deref(),
            self.model_preference
                .as_ref()
                .and_then(|prefs| prefs.lookup_model(&provider_id, &model_id)),
        );
        if variant != chrome.variant {
            self.prompt.set_model(
                Some(provider_id.clone()),
                model_id.clone(),
                chrome.provider.clone(),
                variant.clone(),
            );
        }
        Some(SessionModel {
            provider_id,
            model_id,
            variant,
        })
    }

    fn request_variant_list(&mut self, request: VariantRequest) {
        let _ = self.commands.send(AppCommand::VariantList { request });
    }

    /// Build the switch and persist only on the successful boundary below.
    fn apply_model(
        &mut self,
        provider_id: String,
        model_id: String,
        provider_label: String,
        variant: Option<String>,
    ) {
        if !models::model_switch_needed(
            self.prompt.chrome().provider_id.as_deref(),
            self.prompt.chrome().model.as_deref(),
            self.prompt.chrome().variant.as_deref(),
            &provider_id,
            &model_id,
            variant.as_deref(),
        ) {
            // The server would answer this as a no-op. Still record an explicit
            // choice: Home writes immediately because selecting default must
            // override a saved level, and an existing session records it too.
            self.persist_session_model_preference(&SessionModel {
                provider_id,
                model_id,
                variant,
            });
            return;
        }
        self.apply_switch(Switch::Model {
            provider_id,
            model_id,
            variant,
            provider_label,
        });
    }

    /// Ask the server to apply `switch`, or apply it locally on Home where
    /// no session exists yet. A session switch only reaches the chrome after
    /// the server acknowledged it (`AppEvent::Switched`).
    fn apply_switch(&mut self, switch: Switch) {
        match self.session.clone() {
            Some(session) => {
                self.switch_request_id = self.switch_request_id.wrapping_add(1);
                self.switch_generation = self.switch_generation.wrapping_add(1);
                self.switch_session = Some(session.clone());
                let request = super::command::SwitchRequest {
                    session,
                    switch,
                    request_id: self.switch_request_id,
                    navigation_generation: self.navigation_generation,
                };
                let _ = self.commands.send(AppCommand::Switch { request });
            }
            None => self.on_switched(SwitchRequest {
                session: String::new(),
                switch,
                request_id: 0,
                navigation_generation: self.navigation_generation,
            }),
        }
    }

    pub(super) fn persist_session_model_preference(&mut self, model: &SessionModel) {
        let value = models::normalize_variant(model.variant.as_deref()).unwrap_or("default");
        if let Some(store) = self.model_preference.as_mut()
            && let Err(err) = store.update_model(&model.provider_id, &model.model_id, value)
        {
            tracing::warn!(%err, provider_id = %model.provider_id, model_id = %model.model_id, "failed to save model variant preference");
        }
    }

    pub(super) fn persist_switch_preference(&mut self, switch: &Switch) {
        let Switch::Model {
            provider_id,
            model_id,
            variant,
            ..
        } = switch
        else {
            return;
        };
        self.persist_session_model_preference(&SessionModel {
            provider_id: provider_id.clone(),
            model_id: model_id.clone(),
            variant: variant.clone(),
        });
    }
    /// The change took effect: the metadata row follows.
    pub(super) fn on_switched(&mut self, request: SwitchRequest) {
        if !self.switch_response_matches(&request) {
            return;
        }
        // A successful switch retires every chrome read sent before it, so a
        // slow (including failure-triggered) reread cannot overwrite this row.
        self.switch_session = None;
        self.switch_request_id = self.switch_request_id.wrapping_add(1);
        self.switch_generation = self.switch_generation.wrapping_add(1);
        self.invalidate_variant_requests();
        let switch = request.switch;

        // A Home selection is applied locally, so this success boundary is where
        // its preference is saved; SessionReady saving the same value again is
        // harmless. Existing sessions persist here too.
        self.persist_switch_preference(&switch);
        match switch {
            Switch::Model {
                provider_id,
                model_id,
                variant,
                provider_label,
            } => self
                .prompt
                .set_model(Some(provider_id), model_id, Some(provider_label), variant),
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

    /// `ctrl+t` cycles the current model's named levels, ending at `default`.
    /// The catalogue is fetched per press (like the agent cycle), so the
    /// levels are always the server's current ones.
    pub(super) fn cycle_variant_key(&mut self, key: &KeyEvent) -> bool {
        if self.prompt.autocomplete_visible()
            || key.kind == KeyEventKind::Release
            || key.code != KeyCode::Char('t')
            || !key.modifiers.contains(KeyModifiers::CONTROL)
        {
            return false;
        }
        self.request_variants(true);
        true
    }
}
