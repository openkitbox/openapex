//! Pure mapping between ox's catalog/selection and ACP's select options.

use agent_client_protocol::Error;
use agent_client_protocol::v1::schema::{
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption, SessionMode,
    SessionModeId, SessionModeState,
};
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use serde::Deserialize;

use ox_schema::support::ModelRef;
use ox_tools::agent::DEFAULT_AGENT;

const DEFAULT_VARIANT: &str = "default";
const PROVIDER_ESCAPE: &AsciiSet = &CONTROLS.add(b'/').add(b'%');

#[derive(Deserialize)]
pub(crate) struct Model {
    #[serde(flatten)]
    reference: ModelRef,
    name: String,
    variants: Vec<Variant>,
    enabled: bool,
}

#[derive(Deserialize)]
struct Variant {
    id: String,
}

#[derive(Deserialize)]
pub(crate) struct Agent {
    id: String,
    name: String,
    mode: String,
    hidden: bool,
}

pub(crate) struct Catalog {
    pub(crate) models: Vec<Model>,
    pub(crate) agents: Vec<Agent>,
}

pub(crate) enum Change<'a> {
    Model(ModelRef),
    Agent(&'a str),
}

impl Catalog {
    pub(crate) fn options(
        &self,
        model: Option<&ModelRef>,
        agent: Option<&str>,
    ) -> Vec<SessionConfigOption> {
        let mut options = Vec::with_capacity(3);
        if let Some(current) = model {
            let choices: Vec<_> = self
                .selectable_models()
                .map(|model| {
                    SessionConfigSelectOption::new(
                        model_value(&model.reference),
                        format!("{}/{}", model.reference.provider_id, model.name),
                    )
                })
                .collect();
            if !choices.is_empty() {
                options.push(
                    SessionConfigOption::select("model", "Model", model_value(current), choices)
                        .category(SessionConfigOptionCategory::Model),
                );
            }
            if let Some(effort) = self.effort_option(current) {
                options.push(effort);
            }
        }
        let choices: Vec<_> = self
            .selectable_agents()
            .map(|mode| SessionConfigSelectOption::new(mode.id.clone(), mode.name.clone()))
            .collect();
        if !choices.is_empty() {
            options.push(
                SessionConfigOption::select(
                    "mode",
                    "Session Mode",
                    agent.unwrap_or(DEFAULT_AGENT).to_owned(),
                    choices,
                )
                .category(SessionConfigOptionCategory::Mode),
            );
        }
        options
    }

    pub(crate) fn modes(&self, agent: Option<&str>) -> SessionModeState {
        let modes = self
            .selectable_agents()
            .map(|mode| SessionMode::new(SessionModeId::new(mode.id.as_str()), mode.name.clone()))
            .collect();
        SessionModeState::new(SessionModeId::new(agent.unwrap_or(DEFAULT_AGENT)), modes)
    }

    pub(crate) fn resolve<'a>(
        &'a self,
        model: Option<&ModelRef>,
        id: &str,
        value: &str,
    ) -> Result<Change<'a>, Error> {
        match id {
            "model" => {
                let (provider, id) = value
                    .split_once('/')
                    .ok_or_else(|| invalid("expected a provider-qualified model"))?;
                let provider = percent_decode_str(provider)
                    .decode_utf8()
                    .map_err(|_| invalid("invalid model provider"))?;
                let selected = self
                    .selectable_models()
                    .find(|item| item.reference.provider_id == provider && item.reference.id == id)
                    .ok_or_else(|| invalid("unknown model selection"))?;
                let mut selected = selected.reference.clone();
                selected.variant = model
                    .filter(|current| {
                        current.provider_id == selected.provider_id && current.id == selected.id
                    })
                    .and_then(|current| current.variant.clone());
                Ok(Change::Model(selected))
            }
            "mode" => Ok(Change::Agent(self.agent(value)?)),
            "effort" => {
                let current = model.ok_or_else(|| invalid("no current model"))?;
                let definition = self
                    .reasoning_model(current)
                    .ok_or_else(|| invalid("model has no reasoning options"))?;
                if value != DEFAULT_VARIANT
                    && !definition
                        .variants
                        .iter()
                        .any(|variant| variant.id == value)
                {
                    return Err(invalid("unknown reasoning variant"));
                }
                Ok(Change::Model(ModelRef {
                    variant: Some(value.to_owned()),
                    ..current.clone()
                }))
            }
            _ => Err(invalid("unknown session configuration option")),
        }
    }

    pub(crate) fn agent(&self, value: &str) -> Result<&str, Error> {
        self.selectable_agents()
            .find(|agent| agent.id == value)
            .map(|agent| agent.id.as_str())
            .ok_or_else(|| invalid("unknown session mode"))
    }

    fn effort_option(&self, current: &ModelRef) -> Option<SessionConfigOption> {
        let model = self.reasoning_model(current)?;
        let mut choices = vec![SessionConfigSelectOption::new(DEFAULT_VARIANT, "Default")];
        choices.extend(
            model
                .variants
                .iter()
                .filter(|variant| variant.id != DEFAULT_VARIANT)
                .map(|variant| {
                    SessionConfigSelectOption::new(variant.id.clone(), variant.id.clone())
                }),
        );
        Some(
            SessionConfigOption::select(
                "effort",
                "Reasoning",
                current
                    .variant
                    .as_deref()
                    .unwrap_or(DEFAULT_VARIANT)
                    .to_owned(),
                choices,
            )
            .category(SessionConfigOptionCategory::ThoughtLevel),
        )
    }

    fn reasoning_model(&self, current: &ModelRef) -> Option<&Model> {
        self.selectable_models().find(|model| {
            model.reference.provider_id == current.provider_id
                && model.reference.id == current.id
                && !model.variants.is_empty()
        })
    }

    fn selectable_models(&self) -> impl Iterator<Item = &Model> {
        self.models.iter().filter(|model| {
            let reference = &model.reference;
            model.enabled && !reference.provider_id.is_empty() && !reference.id.is_empty()
        })
    }

    fn selectable_agents(&self) -> impl Iterator<Item = &Agent> {
        self.agents
            .iter()
            .filter(|agent| !agent.hidden && agent.mode != "subagent")
    }
}

fn model_value(model: &ModelRef) -> String {
    format!(
        "{}/{}",
        utf8_percent_encode(&model.provider_id, PROVIDER_ESCAPE),
        model.id
    )
}

fn invalid(message: &'static str) -> Error {
    Error::invalid_params().data(message)
}
