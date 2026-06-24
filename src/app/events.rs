use crate::provider::ChatMessage;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};

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
    RequestToolApproval(ToolApprovalRequest),

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

#[derive(Debug, Clone)]
pub struct ToolApprovalRequest {
    pub tool_name: String,
    pub arguments: String,
    decision: Arc<Mutex<Option<bool>>>,
    notify: Arc<Notify>,
}

impl ToolApprovalRequest {
    pub fn new(tool_name: String, arguments: String) -> Self {
        Self {
            tool_name,
            arguments,
            decision: Arc::new(Mutex::new(None)),
            notify: Arc::new(Notify::new()),
        }
    }

    pub async fn wait(&self) -> bool {
        loop {
            {
                let decision = self.decision.lock().await;
                if let Some(value) = *decision {
                    return value;
                }
            }
            self.notify.notified().await;
        }
    }

    pub async fn resolve(&self, value: bool) {
        {
            let mut decision = self.decision.lock().await;
            *decision = Some(value);
        }
        self.notify.notify_waiters();
    }
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
