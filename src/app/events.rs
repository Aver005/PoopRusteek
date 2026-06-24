use crate::provider::ChatMessage;

#[derive(Debug, Clone)]
pub enum AppEvent {
    // TUI events
    Key(crossterm::event::KeyEvent),
    Resize(u16, u16),
    Tick,

    // Agent events
    AgentStarted,
    AgentChunk(String),
    AgentDone(AgentResult),
    AgentError(String),
    BeginAssistantMessage,
    DiscardEmptyAssistantMessage,
    AddMessage(ChatMessage),

    // Tool events
    ToolStarted { name: String, id: String },
    ToolProgress { id: String, message: String },
    ToolDone { id: String, result: String },
    ToolError { id: String, error: String },

    // UI events
    SwitchView(View),
    PushModal(Modal),
    PopModal,
    Notification(Notification),

    // App lifecycle
    Quit,
}

#[derive(Debug, Clone)]
pub struct AgentResult {
    pub text: String,
    pub tool_calls: Vec<ToolCallInfo>,
}

#[derive(Debug, Clone)]
pub struct ToolCallInfo {
    pub name: String,
    pub arguments: serde_json::Value,
    pub result: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum View {
    Chat,
    Sessions,
    Settings,
    Help,
}

#[derive(Debug, Clone)]
pub enum Modal {
    Confirm {
        message: String,
        on_confirm: String,
    },
    ToolApproval {
        tool_name: String,
        arguments: String,
    },
    Input {
        prompt: String,
    },
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub kind: NotificationKind,
    pub message: String,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NotificationKind {
    Info,
    Success,
    Warning,
    Error,
}
