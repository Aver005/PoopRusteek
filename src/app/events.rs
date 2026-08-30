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

/// Что произошло внутри одного хода агента. Выделено из `AppEvent`: у этих
/// событий три потребителя — фокусный чат, фоновый и безголовый харнесс.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Ход начался.
    Started,
    /// Открыть пустое сообщение ассистента под стрим.
    BeginAssistantMessage,
    /// Кусок стрима в открытое сообщение.
    Chunk(String),
    /// Готовое сообщение в историю (результат инструмента, системная заметка).
    Message(ChatMessage),
    /// Закрыть сообщение ассистента: прикрепить объявленные им вызовы и
    /// сбросить его, только если не осталось ни текста, ни вызовов.
    EndAssistantMessage {
        tool_calls: Vec<crate::provider::ToolCall>,
    },
    /// Ход завершён.
    Done(AgentResult),
    /// Ход оборвался.
    Failed(String),
    /// Пошёл вызов инструмента.
    ToolStarted {
        name: String,
    },
    ToolDone {
        // Every receiver currently ignores this (matched out with `..`) — the
        // status line shows a generic "Tool finished" rather than a preview.
        #[expect(
            dead_code,
            reason = "tool-result preview not surfaced by any receiver yet"
        )]
        result: String,
    },
    ToolError {
        error: String,
    },
    /// Ступень 1 очистила старые тела инструментов в копии хода.
    /// Ту же правку применяет каждый потребитель.
    ToolOutputCleared {
        /// (id вызова, маркер) на каждый очищенный результат.
        cleared: Vec<(String, String)>,
        freed_tokens: u32,
    },
    /// Ступень 2 сбросила серверную сессию: следующий запрос засеет новую
    /// уже очищенной историей.
    SessionReset {
        /// Сколько накопил сервер до сброса.
        before_tokens: u32,
        /// Чем засеется новая сессия.
        after_tokens: u32,
    },
    /// Насколько было заполнено окно для только что отправленного запроса.
    ContextUsage(u32),
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
    /// Отрисовка сама спрашивает размер у терминала; высота нужна клавишам —
    /// они приходят раньше кадра, а шаг страницы обязан совпасть с экраном.
    Resize {
        rows: u16,
    },
    Tick,

    /// Событие одного хода агента. Все трое потребителей — фокус, фон
    /// и харнесс — применяют его через `app::reduce`.
    Agent {
        conversation: ConversationId,
        event: AgentEvent,
    },
    /// `/compact` finished. `messages` is `Some` only when the summary was
    /// accepted; on refusal the history is left exactly as it was and only the
    /// status line changes.
    CompactFinished {
        conversation: ConversationId,
        messages: Option<Vec<ChatMessage>>,
        status: String,
    },
    /// The active provider answered with its model's context window. Sent
    /// once at startup; absent whenever the provider cannot say.
    ContextWindowLearned(u32),

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
    /// Результат отката: работа шла на `spawn_blocking`, потому что чтение
    /// копии и запись файла блокирующие, а `handle_event` — цикл событий.
    UndoFinished(Result<String, String>),
    UpdateStatus {
        message: String,
        notable: bool,
    },
}

// Populated at the `AgentEvent::Done` send site but every current receiver
// discards the payload (`AgentEvent::Done(_)`); goal-mode independently
// re-derives the same text by scanning the message list instead of reading
// this. Kept for future use rather than deleted in this pass. `allow`, not
// `expect`: under `cargo test` the fields count as read (test-only paths),
// so an `expect` is unfulfilled in that build.
#[allow(
    dead_code,
    reason = "AgentEvent::Done payload not read by any current receiver"
)]
#[derive(Debug, Clone)]
pub struct AgentResult {
    pub text: String,
    pub tool_calls: Vec<ToolCallInfo>,
}

#[expect(
    dead_code,
    reason = "AgentEvent::Done payload not read by any current receiver"
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
    /// Что именно можно разрешить «всегда» — см. `tools::approval_scope`.
    pub scope: Option<crate::whitelist::Scope>,
    decision: Arc<Mutex<Option<bool>>>,
    notify: Arc<Notify>,
}

impl ToolApprovalRequest {
    pub fn new(
        conversation: ConversationId,
        tool_name: String,
        arguments: String,
        scope: Option<crate::whitelist::Scope>,
    ) -> Self {
        Self {
            conversation,
            tool_name,
            arguments,
            scope,
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
