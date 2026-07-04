pub mod registry;
pub mod shell;
pub mod question;
pub mod task;
pub mod background;
pub mod shell_control;
pub mod skill;

use async_trait::async_trait;
use serde_json::Value;

/// Tool names the agent loops special-case *before* registry dispatch (the
/// `question`/`task` tools are declared like any other tool so the model
/// sees them, but are never executed through `ToolRegistry::execute`).
/// Dispatch sites must compare against these constants, not string
/// literals, so a rename can't silently break the special-casing.
pub const QUESTION_TOOL_NAME: &str = "question";
pub const TASK_TOOL_NAME: &str = "task";

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

pub fn looks_interactive_command(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    [
        "bun create",
        "npm create",
        "npx create",
        "npm init",
        "pnpm create",
        "yarn create",
        "gh auth",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

pub fn looks_persistent_background_command(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    [
        "vite",
        "next dev",
        "bun dev",
        "bun run dev",
        "npm run dev",
        "pnpm dev",
        "pnpm run dev",
        "yarn dev",
        "webpack serve",
        "cargo watch",
        "cargo leptos watch",
        "trunk serve",
        "serve ",
        "http-server",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    async fn execute(&self, args: Value) -> ToolResult;
}
