pub mod registry;
pub mod bash;
pub mod powershell;
pub mod question;
pub mod background;
pub mod shell_control;

use async_trait::async_trait;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

impl ToolResult {
    pub fn success(content: &str) -> Self {
        Self {
            content: content.to_string(),
            is_error: false,
        }
    }

    pub fn error(content: &str) -> Self {
        Self {
            content: content.to_string(),
            is_error: true,
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    async fn execute(&self, args: Value) -> ToolResult;
}
