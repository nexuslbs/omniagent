use serde::{Deserialize, Serialize};

/// An action — a saved tool invocation with its parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    pub id: String,
    pub name: String,
    pub tool_name: String,
    pub params: serde_json::Value,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub is_builtin: bool,
}
