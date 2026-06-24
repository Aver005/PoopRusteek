pub mod loop_runner;
pub mod context;
pub mod streaming;
pub mod tool_parser;

pub struct AgentResult {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
}

pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

impl ToolCall {
    pub fn new(name: &str, arguments: serde_json::Value) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            arguments,
        }
    }
}
