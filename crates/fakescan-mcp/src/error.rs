//! Tool failures the model can act on: a stable `code`, a message, and a hint.

use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    pub message: String,
    pub code: &'static str,
    pub hint: Option<String>,
}

impl ToolError {
    pub fn new(
        message: impl Into<String>,
        code: &'static str,
        hint: impl Into<Option<String>>,
    ) -> Self {
        Self {
            message: message.into(),
            code,
            hint: hint.into(),
        }
    }

    pub fn json(&self) -> Value {
        let mut out = json!({
            "error": self.message,
            "code": self.code,
        });
        if let Some(hint) = &self.hint {
            out["hint"] = json!(hint);
        }
        out
    }
}

pub fn missing(tool: &str, name: &str) -> ToolError {
    ToolError::new(
        format!("{tool}: {name} is required"),
        "missing_argument",
        Some(format!(
            "Pass {name}=...; see the tool description for the exact call."
        )),
    )
}

pub fn invalid(tool: &str, detail: impl Into<String>) -> ToolError {
    ToolError::new(
        format!("{tool}: {}", detail.into()),
        "invalid_argument",
        None,
    )
}
