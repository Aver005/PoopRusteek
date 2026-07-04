use super::*;
use serde_json::{json, Value};

pub struct QuestionTool;

#[async_trait]
impl Tool for QuestionTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: crate::tools::QUESTION_TOOL_NAME.to_string(),
            description: "Ask the user a question. Use yes_no for simple Y/N confirmation, or multiple_choice for selecting from a list of options with optional custom input.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "The question text to display to the user"
                    },
                    "type": {
                        "type": "string",
                        "enum": ["yes_no", "multiple_choice"],
                        "description": "\"yes_no\" for a yes/no prompt; \"multiple_choice\" for selecting from options"
                    },
                    "options": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Required for multiple_choice. Options the user can pick from."
                    },
                    "allow_custom": {
                        "type": "boolean",
                        "description": "Only for multiple_choice. Lets the user type their own answer."
                    }
                },
                "required": ["question", "type"]
            }),
        }
    }

    async fn execute(&self, _args: Value) -> ToolResult {
        ToolResult::error("question tool must be executed from the agent loop")
    }
}
