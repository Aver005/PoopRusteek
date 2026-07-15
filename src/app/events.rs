use crate::app::conversation::ConversationId;
use crate::provider::ChatMessage;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};

// The modal/view state machines (Onboarding/Picker/Confirm/Modal/Delete/
// Question states + their key reducers) live in `app::view_state` since the
// 2026-07-15 split; re-exported here so existing `app::events::…` imports
// keep resolving. This file keeps the cross-layer event vocabulary.
pub use crate::app::view_state::*;

#[derive(Debug, Clone)]
pub struct QuestionRequest {
    answer: Arc<Mutex<Option<String>>>,
    notify: Arc<Notify>,
}

impl QuestionRequest {
    pub fn new() -> Self {
        Self {
            answer: Arc::new(Mutex::new(None)),
            notify: Arc::new(Notify::new()),
        }
    }

    pub async fn wait(&self) -> Option<String> {
        loop {
            {
                let answer = self.answer.lock().await;
                if answer.is_some() {
                    return answer.clone();
                }
            }
            self.notify.notified().await;
        }
    }

    pub async fn resolve(&self, value: String) {
        {
            let mut answer = self.answer.lock().await;
            *answer = Some(value);
        }
        self.notify.notify_waiters();
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum GoalStage {
    #[default]
    Inactive,
    WaitForGoal,
    RunAgent1,
    RunEvaluator,
    Done,
}

#[derive(Debug, Clone)]
pub struct GoalVerdict {
    pub success: bool,
    pub summary: String,
    pub issues: String,
    pub feedback: String,
}

/// What the spawned goal-evaluator task reports back to the event loop.
/// Carries the evaluator's own messages so the verdict handler can persist the
/// evaluation session under the (possibly swapped) evaluator session id.
#[derive(Debug, Clone)]
pub enum GoalEvalOutcome {
    Verdict {
        verdict: GoalVerdict,
        eval_messages: Vec<ChatMessage>,
    },
    Failed(String),
}

#[derive(Debug, Clone)]
pub enum AppEvent {
    // TUI events
    Key(crossterm::event::KeyEvent),
    // Mouse wheel scrolls the focused window (chat transcript, MCP details,
    // approval modal, search results). Clicks are ignored for now.
    Mouse(crossterm::event::MouseEvent),
    // A bracketed paste (whole clipboard as one chunk). Routed to whatever
    // text field currently has focus so multi-line pastes never fire an early
    // submit. See `keys::paste`.
    Paste(String),
    // The new size only triggers a redraw (ratatui re-queries the terminal
    // size when rendering); the dimensions themselves aren't read anywhere.
    #[expect(
        dead_code,
        reason = "resize payload unused — redraw reads terminal size directly"
    )]
    Resize(u16, u16),
    Tick,

    // Agent events — each carries the conversation it belongs to so background
    // (sidechat / sub-agent) turns stream into the right place.
    AgentStarted(ConversationId),
    AgentChunk(ConversationId, String),
    AgentDone(ConversationId, AgentResult),
    AgentError(ConversationId, String),
    BeginAssistantMessage(ConversationId),
    DiscardEmptyAssistantMessage(ConversationId),
    AddMessage(ConversationId, ChatMessage),

    // Tool events
    ToolStarted {
        conversation: ConversationId,
        name: String,
    },
    ToolDone {
        conversation: ConversationId,
        // Every receiver currently discards this with `result: _` — the
        // status line shows a generic "Tool finished" rather than a preview.
        #[expect(
            dead_code,
            reason = "tool-result preview not surfaced by any receiver yet"
        )]
        result: String,
    },
    ToolError {
        conversation: ConversationId,
        error: String,
    },
    RequestToolApproval(ToolApprovalRequest),
    RequestQuestion(QuestionRequest, QuestionState),

    // Goal events
    GoalEvaluationDone(GoalEvalOutcome),

    /// A model `task` tool call asked for a detached sub-agent.
    SpawnSubAgent {
        parent: ConversationId,
        label: String,
        prompt: String,
    },

    /// Result of a background remote-session fetch (started by `/load`).
    SessionFetched {
        conversation: ConversationId,
        session_id: String,
        result: Result<Vec<ChatMessage>, String>,
    },

    /// Result of a background check (started by `/load`) of whether a local
    /// session's previously-linked remote DeepSeek session is still alive.
    SessionAvailabilityChecked {
        conversation: ConversationId,
        session: crate::session::Session,
        remote_id: String,
        parent_message_id: Option<i64>,
        alive: bool,
    },

    /// A detached MCP admin operation (reload / toggle / reconnect) finished.
    McpOperationDone {
        message: String,
    },

    /// The background MCP startup (spawned in `App::new`) finished
    /// connecting every discovered server — refresh the cached counts and
    /// server list now instead of waiting for the next 2s stats poll.
    /// Deliberately quiet (no chat/status message): startup never announced
    /// itself before it was backgrounded either.
    McpInitialized,

    /// An OAuth authorization flow started from `/mcp auth` finished. On
    /// `Ok`, the token is already persisted and the handler kicks off a
    /// `reconnect_server` for `server` itself — that reconnect's own
    /// `McpOperationDone` reports the final connect outcome.
    McpOAuthResult {
        server: String,
        result: Result<(), String>,
    },

    /// Background fetch of the remote session list for the `/delete` picker.
    RemoteSessionsListed {
        result: Result<Vec<crate::provider::RemoteSessionInfo>, String>,
    },
    /// Result of a background `LLMProvider::list_models` fetch (started by
    /// `/models`). With `switch_to: Some(id)` the id is validated against
    /// the list and applied (or rejected, 404-style); with `None` the model
    /// picker opens.
    ModelsListed {
        result: Result<Vec<String>, String>,
        switch_to: Option<String>,
    },
    /// A background session-deletion batch finished.
    SessionsDeleted {
        deleted: usize,
        failed: Vec<String>,
    },

    /// Progress/status from the semantic skill-matcher background init
    /// (first-run model download, readiness, failure). Status-line only.
    SemanticStatus(String),

    /// An ERROR-level log fired (forwarded by `logging::ErrorSignalLayer`).
    /// Drives the red in-UI error marker; the full text is in `errors.log`.
    ErrorLogged {
        message: String,
    },

    /// A `/search` lookup finished on its blocking thread. `query` echoes
    /// the request so stale replies (user already searched again) can be
    /// recognized and dropped.
    HistorySearchDone {
        query: String,
        matches: Vec<crate::semantic::history::HistoryMatch>,
    },

    // API server lifecycle (`/serve`, `--serve`). Each event carries the
    // launch generation so a replaced server's late events can't clobber
    // the handle of its successor.
    /// The server task bound its listener and is accepting requests.
    ServerStarted {
        generation: u64,
        addr: std::net::SocketAddr,
    },
    /// The server task could not start (bind failure).
    ServerFailed {
        generation: u64,
        error: String,
    },
    /// The server's accept loop exited (shutdown request or handle drop).
    ServerStopped {
        generation: u64,
    },
    /// One access-log line from the API server. Only emitted when the
    /// server was spawned with `request_log` on (proxy mode) — the TUI
    /// never receives it.
    ServerRequestLog {
        line: String,
    },

    /// A background provider-model refresh (startup, periodic refetch,
    /// provider add) finished — see `provider::model_cache`.
    ProviderModelsRefreshed {
        summary: String,
        failed: usize,
    },

    /// A self-update pass (manual `/update` or the startup auto-check)
    /// finished — see `crate::update`. With `notable: false` (startup check
    /// found nothing to do) only the status line changes; otherwise the
    /// message also lands in the focused chat.
    UpdateStatus {
        message: String,
        notable: bool,
    },
}

// Populated at the `AgentDone` send site but every current receiver
// discards the payload (`AgentDone(_, _)`); goal-mode independently
// re-derives the same text by scanning the message list instead of reading
// this. Kept for future use rather than deleted in this pass. `allow`, not
// `expect`: under `cargo test` the fields count as read (test-only paths),
// so an `expect` is unfulfilled in that build.
#[allow(
    dead_code,
    reason = "AgentDone payload not read by any current receiver"
)]
#[derive(Debug, Clone)]
pub struct AgentResult {
    pub text: String,
    pub tool_calls: Vec<ToolCallInfo>,
}

#[expect(
    dead_code,
    reason = "AgentDone payload not read by any current receiver"
)]
#[derive(Debug, Clone)]
pub struct ToolCallInfo {
    pub name: String,
    pub arguments: serde_json::Value,
    pub result: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolApprovalRequest {
    /// The conversation whose agent task is parked on this approval — lets the
    /// app auto-deny leftovers when that conversation's turn is cancelled.
    pub conversation: ConversationId,
    pub tool_name: String,
    pub arguments: String,
    decision: Arc<Mutex<Option<bool>>>,
    notify: Arc<Notify>,
}

impl ToolApprovalRequest {
    pub fn new(conversation: ConversationId, tool_name: String, arguments: String) -> Self {
        Self {
            conversation,
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

/// A user interaction (tool approval / question) requested while another modal
/// is already on screen. Parked in FIFO order until the current one resolves —
/// overwriting the pending slot would leave the previous agent task waiting on
/// a `Notify` that nobody will ever fire.
#[derive(Debug, Clone)]
pub enum PendingInteraction {
    Approval(ToolApprovalRequest),
    Question(QuestionRequest, QuestionState),
}

#[derive(Debug, Clone, PartialEq)]
pub enum View {
    Chat,
    Mcp,
    Onboarding,
    /// `/providers` — the provider-management panel.
    Providers,
    /// `/search` — the history-search screen.
    Search,
    /// `/themes` — the theme gallery + create wizard.
    Themes,
}
