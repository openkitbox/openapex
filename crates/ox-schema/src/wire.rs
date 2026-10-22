//! Tool-schema wire types: the OpenAI `tools` array shapes.
//!
//! These live in the contract layer rather than in the engine's
//! `cardinal::wire` so a tool crate can describe itself without depending on
//! the engine. `cardinal::wire` re-exports them for the chat request.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Literal `"function"` discriminator used by [`ToolDef`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FunctionType {
    #[serde(rename = "function")]
    Function,
}

/// A tool offered to the model (OpenAI tools array entry).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    /// Always `"function"`.
    #[serde(rename = "type")]
    pub kind: FunctionType,
    pub function: FunctionDef,
}

impl ToolDef {
    /// A function tool with a JSON-Schema `parameters` object.
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
    ) -> Self {
        Self {
            kind: FunctionType::Function,
            function: FunctionDef {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

/// The schema part of a [`ToolDef`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    /// JSON Schema describing the `arguments` object.
    pub parameters: Value,
}
