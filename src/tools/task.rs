use super::*;
use serde_json::{json, Value};

/// `task` — spawn an isolated sub-agent. This struct only carries the schema the
/// model sees; the actual spawn is special-cased in the agent loop (like
/// `question`), so a foreground sub-agent's result can be returned inline and a
/// background one can be tracked. If it is ever executed directly it means a
/// sub-agent tried to spawn another — which is not allowed.
pub struct TaskTool;

#[async_trait]
impl Tool for TaskTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "task".to_string(),
            description: "Delegate a focused subtask to an isolated sub-agent with its own fresh context. The sub-agent has the same tools, works independently, and returns only its final answer (its intermediate steps don't clutter this conversation). Use for self-contained research/analysis/multi-step work. By default it runs to completion and returns the result here; set background=true to launch it without waiting (you'll be notified when it finishes).".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "description": {
                        "type": "string",
                        "description": "A short label for the subtask (a few words)."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "The full task / instructions for the sub-agent."
                    },
                    "background": {
                        "type": "boolean",
                        "description": "If true, launch detached and return immediately instead of waiting for the result."
                    }
                },
                "required": ["prompt"]
            }),
        }
    }

    async fn execute(&self, _args: Value) -> ToolResult {
        ToolResult::error("Sub-agents cannot spawn further sub-agents.")
    }
}
