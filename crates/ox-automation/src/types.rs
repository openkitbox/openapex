//! Wire-facing automation types.
//!
//! These types mirror upstream `Automation` schema (`automation.ts`) so tools,
//! HTTP and the scheduler share one validation point. Unknown keys are rejected
//! at deserialization: dropping a key such as `to.subagent` would silently
//! change where an unattended prompt runs.

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// When an automation fires. Wall-clock triggers carry an IANA zone; `event`
/// triggers subscribe to a connected MCP server instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Trigger {
    At {
        at: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tz: Option<String>,
    },
    Every {
        every: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tz: Option<String>,
    },
    /// A cron expression without either its own zone or a configured one is
    /// invalid: the next occurrence cannot be computed deterministically.
    Cron {
        cron: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tz: Option<String>,
    },
    Event {
        server: String,
        event: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<Value>,
    },
}

impl Trigger {
    /// The explicitly configured zone, if the trigger carries one.
    #[must_use]
    pub fn tz(&self) -> Option<&str> {
        match self {
            Self::At { tz, .. } | Self::Every { tz, .. } | Self::Cron { tz, .. } => tz.as_deref(),
            Self::Event { .. } => None,
        }
    }

    /// Validate the trigger before it is persisted. `default_tz` is the
    /// configured fallback (the server's local zone is resolved by the caller).
    pub fn validate(&self, default_tz: Option<&str>) -> Result<()> {
        match self {
            Self::At { at, tz } => {
                ensure!(!at.trim().is_empty(), "trigger.at must not be empty");
                validate_tz(tz.as_deref().or(default_tz))
            }
            Self::Every { every, tz } => {
                ensure!(!every.trim().is_empty(), "trigger.every must not be empty");
                validate_tz(tz.as_deref().or(default_tz))
            }
            Self::Cron { cron, tz } => {
                ensure!(!cron.trim().is_empty(), "trigger.cron must not be empty");
                let Some(zone) = tz.as_deref().or(default_tz) else {
                    bail!("trigger.cron requires a timezone");
                };
                validate_tz(Some(zone))
            }
            Self::Event { server, event, .. } => {
                ensure!(
                    !server.trim().is_empty(),
                    "trigger.server must not be empty"
                );
                ensure!(!event.trim().is_empty(), "trigger.event must not be empty");
                Ok(())
            }
        }
    }
}

fn validate_tz(tz: Option<&str>) -> Result<()> {
    let Some(tz) = tz else { return Ok(()) };
    ensure!(!tz.trim().is_empty(), "timezone must not be empty");
    ensure!(
        jiff::tz::TimeZone::get(tz).is_ok(),
        "unknown IANA timezone: {tz}"
    );
    Ok(())
}

/// Where a prompt action goes. Exactly one field is present: the serde
/// `deny_unknown_fields` on each variant makes `{session, subagent}` fail
/// instead of silently selecting one target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Target {
    Session { session: String },
    Subagent { subagent: String },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetFields {
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    subagent: Option<String>,
}

impl<'de> Deserialize<'de> for Target {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let fields = TargetFields::deserialize(deserializer)?;
        match (fields.session, fields.subagent) {
            (Some(session), None) => Ok(Self::Session { session }),
            (None, Some(subagent)) => Ok(Self::Subagent { subagent }),
            _ => Err(serde::de::Error::custom(
                "target must have exactly one of session or subagent",
            )),
        }
    }
}

/// What an automation does when it fires.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Action {
    Prompt {
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<Target>,
    },
    Code {
        code: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<Value>,
    },
    Shell {
        command: String,
    },
}

impl Action {
    /// Reject empty programs and commands before they reach an executor.
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Prompt { prompt, to } => {
                ensure!(!prompt.trim().is_empty(), "action.prompt must not be empty");
                if let Some(Target::Session { session }) = to {
                    ensure!(
                        !session.trim().is_empty(),
                        "target.session must not be empty"
                    );
                }
                if let Some(Target::Subagent { subagent }) = to {
                    ensure!(
                        !subagent.trim().is_empty(),
                        "target.subagent must not be empty"
                    );
                }
                Ok(())
            }
            Self::Code { code, .. } => {
                ensure!(!code.trim().is_empty(), "action.code must not be empty");
                Ok(())
            }
            Self::Shell { command } => {
                ensure!(
                    !command.trim().is_empty(),
                    "action.command must not be empty"
                );
                Ok(())
            }
        }
    }
}

/// Lifecycle of an automation, persisted on the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Active,
    Paused,
    Done,
}

impl Status {
    /// Stable database/wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Done => "done",
        }
    }

    /// Parse a persisted status, failing closed on an unknown value.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "paused" => Some(Self::Paused),
            "done" => Some(Self::Done),
            _ => None,
        }
    }
}

/// Result of the most recent run, persisted on the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Fired,
    Late,
    Skipped,
    Disabled,
    Failed,
    Expired,
}

impl Outcome {
    /// Stable database/wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fired => "fired",
            Self::Late => "late",
            Self::Skipped => "skipped",
            Self::Disabled => "disabled",
            Self::Failed => "failed",
            Self::Expired => "expired",
        }
    }

    /// Parse a persisted outcome, failing closed on an unknown value.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "fired" => Some(Self::Fired),
            "late" => Some(Self::Late),
            "skipped" => Some(Self::Skipped),
            "disabled" => Some(Self::Disabled),
            "failed" => Some(Self::Failed),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }
}

/// One optional hand-off from a `code` action. A code program cannot choose a
/// session by id; it may only name a permitted subagent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeSend {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<CodeSendTarget>,
    pub prompt: String,
}

/// The only target a code action may address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeSendTarget {
    pub subagent: String,
}

/// The strict return shape of a `code` action. `null` means "do nothing".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send: Option<CodeSend>,
}

impl CodeResult {
    /// Decode a runtime return value, rejecting unknown keys and empty sends.
    /// A rejected send must commit neither state nor `done`, so validation is
    /// all-or-nothing.
    pub fn decode(value: &Value) -> Result<Option<Self>> {
        if value.is_null() {
            return Ok(None);
        }
        let result: Self = serde_json::from_value(value.clone())?;
        if let Some(send) = &result.send {
            ensure!(
                !send.prompt.trim().is_empty(),
                "send.prompt must not be empty"
            );
            if let Some(to) = &send.to {
                ensure!(
                    !to.subagent.trim().is_empty(),
                    "send.to.subagent must not be empty"
                );
            }
        }
        Ok(Some(result))
    }
}

/// Creation and update timestamps in epoch milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeInfo {
    pub created: i64,
    pub updated: i64,
}

/// Public automation snapshot, shared by HTTP, tools and the scheduler.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Info {
    pub id: String,
    pub name: String,
    #[serde(rename = "projectID")]
    pub project_id: String,
    #[serde(rename = "sessionID", skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub directory: String,
    pub trigger: Trigger,
    pub tz: String,
    pub action: Action,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_due_at: Option<i64>,
    pub run_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_outcome: Option<Outcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<Value>,
    pub time: TimeInfo,
}
