pub mod background_stats;
pub mod conversation;
pub mod events;
pub mod generation;
mod goal;
pub mod input;
mod keys;
pub mod mcp_status;
mod multichat;
mod runtime;
mod system_prompt;

use crate::commands::CommandRegistry;
use crate::config::Config;
use crate::error::AppResult;
use crate::mcp::MCPManager;
use crate::prompts::{self, PromptFiles};
use crate::provider::{ChatMessage, LLMProvider, Role};
use crate::provider::estimate_tokens;
use crate::tools::registry::ToolRegistry;
use crate::commands::CommandSuggestion;
use crate::skills::{discovery::discover_all_skills, SkillDefinition};
use events::{
    AppEvent, GoalStage, Modal, PendingInteraction, QuestionRequest, QuestionState,
    ToolApprovalRequest, View,
};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

/// PID of the foreground child process (if any), for killing on abort.
pub static FOREGROUND_CHILD_PID: AtomicU32 = AtomicU32::new(0);
static TERMINAL_RESTORE_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn request_terminal_restore() {
    TERMINAL_RESTORE_REQUESTED.store(true, Ordering::SeqCst);
}

fn consume_terminal_restore_request() -> bool {
    TERMINAL_RESTORE_REQUESTED.swap(false, Ordering::SeqCst)
}

pub fn kill_foreground_child() {
    let pid = FOREGROUND_CHILD_PID.swap(0, Ordering::SeqCst);
    if pid != 0 {
        #[cfg(windows)]
        {
            // /T kills the entire process tree (child + grandchild processes).
            let _ = std::process::Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .spawn();
        }
        #[cfg(not(windows))]
        {
            let _ = std::process::Command::new("kill")
                .arg("-9")
                .arg(pid.to_string())
                .spawn();
        }
    }
}

/// The conversation an agent event targets, if it is a per-conversation event.
fn agent_event_target(event: &AppEvent) -> Option<conversation::ConversationId> {
    match event {
        AppEvent::AgentStarted(id)
        | AppEvent::AgentChunk(id, _)
        | AppEvent::AgentDone(id, _)
        | AppEvent::AgentError(id, _)
        | AppEvent::BeginAssistantMessage(id)
        | AppEvent::DiscardEmptyAssistantMessage(id)
        | AppEvent::AddMessage(id, _) => Some(*id),
        AppEvent::ToolStarted { conversation, .. }
        | AppEvent::ToolDone { conversation, .. }
        | AppEvent::ToolError { conversation, .. } => Some(*conversation),
        _ => None,
    }
}

pub struct App {
    pub config: Config,
    pub state: AppState,
    pub event_tx: mpsc::UnboundedSender<AppEvent>,
    event_rx: mpsc::UnboundedReceiver<AppEvent>,
    commands: CommandRegistry,
    pub mcp: Arc<tokio::sync::Mutex<MCPManager>>,
    tools: Arc<ToolRegistry>,
    prompts: PromptFiles,
    skills: Vec<SkillDefinition>,
    /// Launches agent turns (owns the tool registry / MCP / event channel for
    /// execution). The single place the agent loop is spawned.
    runtime: runtime::AgentRuntime,
}

pub struct AppState {
    /// All open conversations with one focused. The focused conversation's
    /// messages / provider / generation / session live inside it — there is no
    /// separate "live" copy on `App`/`AppState`.
    pub conversations: conversation::Conversations,
    pub input: input::InputState,
    pub status_message: String,
    pub scroll_offset: u32,
    pub modal: Option<Modal>,
    pub approved_tools: std::collections::HashSet<String>,
    /// The interaction currently on screen (tool approval / question)…
    pub pending_tool_approval: Option<ToolApprovalRequest>,
    pub pending_question: Option<QuestionRequest>,
    /// …and the ones parked behind it, FIFO. See `present_next_interaction`.
    pub pending_interactions: std::collections::VecDeque<PendingInteraction>,
    pub autocomplete: AutocompleteState,
    pub view: View,
    pub mcp_status: mcp_status::McpStatus,
    pub workspace_path: String,
    pub show_stats_panel: bool,
    pub attached_files: Vec<crate::provider::AttachedFile>,

    // Goal mode state
    pub goal: goal::GoalState,

    pub needs_terminal_restore: bool,
    pub background: background_stats::BackgroundCounters,
}

impl AppState {
    /// The focused conversation (whose messages/generation/session are shown).
    pub fn focused(&self) -> &conversation::Conversation {
        self.conversations.focused()
    }

    pub fn focused_mut(&mut self) -> &mut conversation::Conversation {
        self.conversations.focused_mut()
    }

    /// Append a message to the focused conversation. Convenience that avoids
    /// borrow conflicts when the content is derived from `self`.
    pub fn push_message(&mut self, message: ChatMessage) {
        self.focused_mut().messages.push(message);
    }

    /// Append a system message to the focused conversation.
    pub fn push_system(&mut self, content: &str) {
        self.focused_mut().messages.push(ChatMessage::system(content));
    }
}

#[derive(Debug, Clone, Default)]
pub struct AutocompleteState {
    pub visible: bool,
    pub items: Vec<CommandSuggestion>,
    pub selected: usize,
    pub scroll_offset: usize,
    pub file_mode: bool,
}

const AUTOCOMPLETE_VISIBLE: usize = 8;

impl App {
    pub async fn new(config: Config) -> AppResult<Self> {
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        let provider: Option<Arc<dyn LLMProvider>> = if config.provider.token.is_empty() {
            None
        } else {
            let ds = crate::provider::deepseek::DeepseekProvider::new(
                &config.provider,
                config.agent.rate_limit_ms,
                config.agent.max_retries,
            )?;
            Some(Arc::new(ds))
        };

        let mut mcp_manager = MCPManager::new();
        let mcp_init_ok = mcp_manager.initialize().await.is_ok();
        let mcp = Arc::new(tokio::sync::Mutex::new(mcp_manager));
        let tools = Arc::new(ToolRegistry::new());
        let prompts = prompts::load_prompt_files()?;

        let mut skills = discover_all_skills(&config.skills.paths);
        for skill in &mut skills {
            if config.skills.enabled.contains(&skill.slug) || config.skills.enabled.contains(&skill.name) {
                skill.enabled = true;
            }
        }
        tools.update_skills(skills.clone());

        let has_provider = provider.is_some();
        let main_conversation = conversation::Conversation {
            id: conversation::ConversationId::next(),
            kind: conversation::ConversationKind::Main,
            parent: None,
            title: String::new(),
            session_id: crate::session::create_session_id(),
            session_started_at: chrono::Utc::now().to_rfc3339(),
            messages: Vec::new(),
            provider,
            generation: generation::GenerationState::default(),
            agent_task: None,
        };

        let mut state = AppState {
            conversations: conversation::Conversations::new(main_conversation),
            input: input::InputState {
                history: crate::session::load_history(),
                ..Default::default()
            },
            status_message: if has_provider { "Ready" } else { "No token configured" }.to_string(),
            scroll_offset: 0,
            modal: None,
            approved_tools: crate::whitelist::load(),
            pending_tool_approval: None,
            pending_question: None,
            pending_interactions: std::collections::VecDeque::new(),
            autocomplete: AutocompleteState::default(),
            view: View::Chat,
            mcp_status: mcp_status::McpStatus::default(),
            show_stats_panel: true,
            workspace_path: std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
            attached_files: Vec::new(),

            goal: goal::GoalState::default(),
            needs_terminal_restore: false,
            background: background_stats::BackgroundCounters::default(),
        };

        if mcp_init_ok {
            let mgr = mcp.lock().await;
            let servers = mgr.get_servers_info();
            state.mcp_status.server_count = servers.len();
            state.mcp_status.connected_count = servers
                .iter()
                .filter(|s| s.enabled && s.status == "connected")
                .count();
            state.mcp_status.view.servers = servers;
            state.mcp_status.last_stats_update = Some(std::time::Instant::now());
        }

        let runtime =
            runtime::AgentRuntime::new(Arc::clone(&tools), Arc::clone(&mcp), event_tx.clone());

        Ok(Self {
            config,
            state,
            event_tx,
            event_rx,
            commands: CommandRegistry::new(),
            mcp,
            tools,
            prompts,
            skills,
            runtime,
        })
    }

    pub async fn run(&mut self) -> AppResult<()> {
        let mut terminal = crate::tui::init()?;
        let result = self.run_loop(&mut terminal).await;
        // Kill any running foreground child before restoring terminal.
        kill_foreground_child();
        crate::tui::restore(&mut terminal)?;
        // Abort every conversation's agent task so none keeps the runtime alive.
        for conv in self.state.conversations.iter_mut() {
            if let Some(handle) = conv.agent_task.take() {
                handle.abort();
            }
        }
        // Kill all background/PTY processes so spawn_blocking waiters unblock
        // and the tokio runtime can shut down cleanly.
        let _ = crate::tools::background::shutdown_all().await;
        // Terminate MCP server children — dropping them without close() used
        // to orphan one subprocess per stdio server on every exit.
        self.mcp.lock().await.shutdown_all().await;
        result
    }

    async fn run_loop(
        &mut self,
        terminal: &mut crate::tui::TuiTerminal,
    ) -> AppResult<()> {
        use crossterm::event::EventStream;
        use futures::StreamExt;

        let tick_rate = std::time::Duration::from_millis(120);
        let mut tick_interval = tokio::time::interval(tick_rate);
        let mut event_stream = EventStream::new();

        loop {
            let mut dirty = false;
            tokio::select! {
                _ = tick_interval.tick() => {
                    dirty |= self.tick_is_visual();
                    self.handle_event(AppEvent::Tick).await?;
                }
                Some(Ok(event)) = event_stream.next() => {
                    match event {
                        crossterm::event::Event::Key(key)
                            if matches!(
                                key.kind,
                                crossterm::event::KeyEventKind::Press
                                    | crossterm::event::KeyEventKind::Repeat
                            ) =>
                        {
                            dirty = true;
                            if self.handle_event(AppEvent::Key(key)).await? {
                                return Ok(());
                            }
                        }
                        crossterm::event::Event::Resize(w, h) => {
                            dirty = true;
                            self.handle_event(AppEvent::Resize(w, h)).await?;
                        }
                        _ => {}
                    }
                }
                Some(event) = self.event_rx.recv() => {
                    dirty = true;
                    if self.handle_event(event).await? {
                        return Ok(());
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    // Kill any running foreground child before restoring terminal.
                    kill_foreground_child();
                    let _ = self.state.background.shutdown_all().await;
                    return Ok(());
                }
            }

            // Drain whatever queued up while we were handling, then render
            // ONCE. Without this a streaming burst (one event per token) costs
            // one full render per token, and once render time exceeds chunk
            // arrival time the unbounded queue only ever grows.
            let mut drained = 0usize;
            while drained < 256 {
                match self.event_rx.try_recv() {
                    Ok(event) => {
                        drained += 1;
                        dirty = true;
                        if self.handle_event(event).await? {
                            return Ok(());
                        }
                    }
                    Err(_) => break,
                }
            }

            if self.state.view == View::Mcp {
                dirty |= self.state.mcp_status.refresh_view(&self.mcp).await;
            }
            dirty |= self.state.mcp_status.update_stats(&self.mcp).await;
            dirty |= self.state.background.refresh().await;

            if consume_terminal_restore_request() {
                self.state.needs_terminal_restore = true;
            }

            if self.state.needs_terminal_restore {
                self.state.needs_terminal_restore = false;
                let _ = crossterm::terminal::disable_raw_mode();
                let _ = crossterm::execute!(
                    terminal.backend_mut(),
                    crossterm::terminal::LeaveAlternateScreen,
                );
                let _ = crossterm::terminal::enable_raw_mode();
                let _ = crossterm::execute!(
                    terminal.backend_mut(),
                    crossterm::terminal::EnterAlternateScreen,
                    crossterm::cursor::DisableBlinking,
                    crossterm::cursor::Show,
                );
                terminal.clear()?;
                dirty = true;
            }

            // Idle ticks with nothing animating skip the draw entirely — this
            // is what turns the former ~8 renders/sec at rest into zero.
            if dirty {
                self.render(terminal)?;
            }
        }
    }

    /// Does a tick actually change pixels? The spinner and elapsed-time stats
    /// animate while any conversation is streaming; the landing logo animates
    /// while the chat is empty and no modal covers it.
    fn tick_is_visual(&self) -> bool {
        self.state.conversations.iter().any(|c| c.is_streaming())
            || (self.state.focused().messages.is_empty() && self.state.modal.is_none())
    }

    async fn handle_event(&mut self, event: AppEvent) -> AppResult<bool> {
        // Agent events for a background conversation are applied to its parked
        // record, not the focused chat. Sidechats/sub-agents route by KIND even
        // while focused (Tab can land on one): their terminal events must
        // finalize-and-flush into the parent, which the focused path never does.
        if let Some(target) = agent_event_target(&event) {
            let background_kind = self
                .state
                .conversations
                .get(target)
                .is_some_and(|c| c.is_background_kind());
            if target != self.state.conversations.focused_id() || background_kind {
                self.handle_background_event(target, event);
                return Ok(false);
            }
        }
        match event {
            AppEvent::Key(key) => return self.handle_key(key).await,
            AppEvent::AgentStarted(_) => {
                self.state.focused_mut().generation.begin(std::time::Instant::now());
                self.state.status_message = "Thinking...".to_string();
            }
            AppEvent::BeginAssistantMessage(_) => {
                self.state.focused_mut().begin_assistant_message();
            }
            AppEvent::DiscardEmptyAssistantMessage(_) => {
                self.state.focused_mut().discard_empty_assistant();
            }
            AppEvent::AgentChunk(_, chunk) => {
                self.state.focused_mut().append_chunk(&chunk);
            }
            AppEvent::AgentDone(_, _result) => {
                self.state.focused_mut().finish_turn("FINISHED");
                self.state.status_message = "Ready".to_string();
                self.record_gen_stats();
                self.auto_save_session();

                // --- GOAL cycle check ---
                if self.state.goal.mode && self.state.goal.stage == GoalStage::RunAgent1 {
                    // Agent 1 finished — get last assistant content for evaluation
                    let agent_result = self.state.focused_mut().messages
                        .iter()
                        .rev()
                        .find(|m| m.role == Role::Assistant)
                        .map(|m| m.content.clone())
                        .unwrap_or_default();

                    if !agent_result.is_empty() {
                        self.state.status_message = "Evaluating goal...".to_string();
                        self.state.goal.stage = GoalStage::RunEvaluator;
                        self.state.focused_mut().messages.push(ChatMessage::ui_system(
                            "🔍 Evaluating result against goal..."
                        ));
                        self.spawn_goal_evaluation(agent_result);
                    } else {
                        self.state.focused_mut().messages.push(ChatMessage::ui_system(
                            "⚠ Agent produced no output. Retrying..."
                        ));
                        self.retry_agent1().await;
                    }
                }
            }
            AppEvent::AgentError(_, err) => {
                self.state.status_message = err.clone();
                self.state.focused_mut().finish_turn("ABORTED");
                self.record_gen_stats();
                self.auto_save_session();

                // An error mid-cycle would otherwise leave goal mode stuck in a
                // "running" stage with nothing actually running.
                if self.state.goal.is_running() {
                    self.cancel_goal_cycle(&format!(
                        "⚠ Goal cycle stopped after an error: {err}. Use /goal to retry."
                    ));
                }
            }
            AppEvent::AddMessage(_, message) => {
                self.state.focused_mut().messages.push(message);
            }
            AppEvent::ToolStarted { name, .. } => {
                self.state.status_message = format!("Running {name}...");
            }
            AppEvent::ToolDone { result: _, .. } => {
                self.state.status_message = "Tool finished".to_string();
            }
            AppEvent::ToolError { error, .. } => {
                self.state.status_message = format!("Tool error: {error}");
            }
            AppEvent::RequestToolApproval(request) => {
                if self.state.approved_tools.contains(&request.tool_name) {
                    request.resolve(true).await;
                    self.state.focused_mut().generation.active = true;
                    self.state.status_message = format!("Running {} (auto-approved)", request.tool_name);
                } else if self.state.modal.is_some()
                    || self.state.pending_tool_approval.is_some()
                    || self.state.pending_question.is_some()
                {
                    // Another interaction is on screen — park this one. An
                    // overwrite would orphan the previous request's agent
                    // task on a `Notify` nobody would ever fire.
                    self.state
                        .pending_interactions
                        .push_back(PendingInteraction::Approval(request));
                    self.state.status_message = format!(
                        "{} interaction(s) queued",
                        self.state.pending_interactions.len()
                    );
                } else {
                    self.present_tool_approval(request);
                }
            }
            AppEvent::RequestQuestion(request, state) => {
                if self.state.modal.is_some()
                    || self.state.pending_tool_approval.is_some()
                    || self.state.pending_question.is_some()
                {
                    self.state
                        .pending_interactions
                        .push_back(PendingInteraction::Question(request, state));
                } else {
                    self.present_question(request, state);
                }
            }
            AppEvent::Tick => {
                if self.state.focused_mut().generation.active || (self.state.focused_mut().messages.is_empty() && self.state.modal.is_none()) {
                    self.state.focused_mut().generation.animation_tick = self.state.focused_mut().generation.animation_tick.wrapping_add(1);
                }
            }
            AppEvent::GoalEvaluationDone(outcome) => {
                self.handle_goal_evaluation_done(outcome).await;
            }
            AppEvent::SpawnSubAgent { parent, label, prompt } => {
                self.spawn_sub_agent(parent, label, prompt).await?;
            }
            AppEvent::SessionFetched { conversation, session_id, result } => {
                self.apply_fetched_session(conversation, &session_id, result).await;
            }
            AppEvent::McpOperationDone { message } => {
                self.state.mcp_status.view.status_message = message.clone();
                self.state.status_message = message.clone();
                self.state.focused_mut().messages.push(ChatMessage::ui_system(&message));
                // Force the next loop iteration to re-pull fresh server info.
                self.state.mcp_status.view.servers.clear();
                self.state.mcp_status.last_stats_update = None;
            }
            _ => {}
        }
        Ok(false)
    }

    /// Put a tool-approval request on screen (modal + current slot).
    fn present_tool_approval(&mut self, request: ToolApprovalRequest) {
        self.state.focused_mut().generation.active = false;
        self.state.status_message = format!("Approve tool {}?", request.tool_name);
        self.state.modal = Some(Modal::ToolApproval {
            tool_name: request.tool_name.clone(),
            arguments: request.arguments.clone(),
            scroll_offset: 0,
            always_allow: false,
        });
        self.state.pending_tool_approval = Some(request);
    }

    /// Put a question request on screen (modal + current slot).
    fn present_question(&mut self, request: QuestionRequest, state: QuestionState) {
        self.state.pending_question = Some(request);
        self.state.modal = Some(Modal::Question(state));
        self.state.focused_mut().generation.active = false;
        self.state.status_message = "Question pending...".to_string();
    }

    /// After a modal resolves, surface the next parked interaction (if any).
    /// Approvals whitelisted meanwhile resolve immediately instead of showing.
    pub(crate) async fn present_next_interaction(&mut self) {
        while let Some(next) = self.state.pending_interactions.pop_front() {
            match next {
                PendingInteraction::Approval(request) => {
                    if self.state.approved_tools.contains(&request.tool_name) {
                        request.resolve(true).await;
                        continue;
                    }
                    self.present_tool_approval(request);
                    return;
                }
                PendingInteraction::Question(request, state) => {
                    self.present_question(request, state);
                    return;
                }
            }
        }
    }

    /// Deny-and-drop every pending approval belonging to `conversation` — its
    /// turn is being cancelled, so leaving them queued (or on screen) would
    /// present approvals for a task that no longer exists.
    pub(crate) async fn purge_interactions_for(
        &mut self,
        conversation: conversation::ConversationId,
    ) {
        let mut kept = std::collections::VecDeque::new();
        while let Some(item) = self.state.pending_interactions.pop_front() {
            match item {
                PendingInteraction::Approval(request)
                    if request.conversation == conversation =>
                {
                    request.resolve(false).await;
                }
                other => kept.push_back(other),
            }
        }
        self.state.pending_interactions = kept;

        let current_is_target = self
            .state
            .pending_tool_approval
            .as_ref()
            .is_some_and(|r| r.conversation == conversation);
        if current_is_target {
            if let Some(request) = self.state.pending_tool_approval.take() {
                request.resolve(false).await;
            }
            if matches!(self.state.modal, Some(Modal::ToolApproval { .. })) {
                self.state.modal = None;
            }
            self.present_next_interaction().await;
        }
    }

    /// Cancel the focused conversation's in-flight turn: kill its foreground
    /// child, abort the agent task, deny its pending approvals, and reset the
    /// visible state. Shared by Esc and Ctrl+C.
    pub(crate) async fn cancel_focused_turn(&mut self) {
        // Kill the running foreground child process first.
        kill_foreground_child();
        if let Some(handle) = self.state.focused_mut().agent_task.take() {
            handle.abort();
        }
        let conversation = self.state.conversations.focused_id();
        self.purge_interactions_for(conversation).await;
        let killed = self.state.background.shutdown_all().await;
        self.state.focused_mut().generation.active = false;
        self.state.status_message = if killed > 0 {
            format!("Cancelled; killed {killed} background process(es)")
        } else {
            "Cancelled".to_string()
        };
        self.state.needs_terminal_restore = true;
        self.state.focused_mut().discard_empty_assistant();
        if self.state.goal.is_running() {
            self.cancel_goal_cycle("⏹ Goal cycle cancelled. Use /goal to start a new one.");
        }
    }

    /// Start an agent turn on the focused conversation. `user_message`, when
    /// given, is appended to the chat first and therefore reaches the model —
    /// this replaces an older `send_to_agent(input)` whose argument was
    /// silently discarded. The provider sees every non-`ui_only` message.
    async fn send_focused_turn(&mut self, user_message: Option<ChatMessage>) -> AppResult<()> {
        let provider = match &self.state.focused().provider {
            Some(p) => Arc::clone(p),
            None => {
                self.state.focused_mut().messages.push(ChatMessage::assistant(
                    "No provider configured. Set your DeepSeek token in config.",
                ));
                return Ok(());
            }
        };

        if let Some(message) = user_message {
            self.state.focused_mut().messages.push(message);
        }

        let conversation = self.state.conversations.focused_id();
        let messages: Vec<ChatMessage> = self
            .state
            .focused()
            .messages
            .iter()
            .filter(|m| !m.ui_only)
            .cloned()
            .collect();
        let system_prompt = system_prompt::build(
            &self.prompts,
            &self.skills,
            &self.tools,
            &self.mcp,
            &self.state.workspace_path,
        )
        .await;

        let spec = runtime::TurnSpec {
            conversation,
            provider,
            messages,
            system_prompt,
            model: self.config.provider.model.clone(),
            temperature: self.config.provider.temperature,
            max_tokens: self.config.provider.max_tokens,
            max_steps: self.config.agent.max_steps_per_turn.max(1),
            max_tools_per_step: self.config.agent.max_tools_per_step.max(1),
            auto_approve: false, // focused turn: interactive approval
        };

        let _ = self.event_tx.send(AppEvent::AgentStarted(conversation));
        let handle = self.runtime.spawn(spec);
        self.state.focused_mut().agent_task = Some(handle);

        Ok(())
    }



    async fn handle_load_session(&mut self, session_id: &str) -> AppResult<()> {
        use crate::session;

        match session::load_local(session_id, &self.config) {
            Ok(s) => {
                self.state.focused_mut().messages = s.messages;
                self.state.attached_files.clear();
                self.state.focused_mut().session_id = s.id;
                self.state.scroll_offset = 0;
                self.state.input.buffer.clear();
                self.state.input.cursor = 0;
                self.state.input.selection_anchor = None;
                self.state.autocomplete = Default::default();
                self.state.focused_mut().generation.active = false;
                if let Some(handle) = self.state.focused_mut().agent_task.take() {
                    handle.abort();
                }

                if let Some(provider) = &self.state.focused().provider {
                    let _ = provider.reset().await;
                }

                let count = self.state.focused_mut().messages.len();
                self.state.status_message = format!(
                    "Loaded local session {session_id} ({count} messages)"
                );
            }
            Err(_) => {
                let remote_provider = self.state.focused().provider.clone();
                if let Some(provider) = remote_provider {
                    // Fetch off the event loop — a slow network here used to
                    // freeze the whole TUI until the response (or timeout).
                    let conversation = self.state.conversations.focused_id();
                    let sid = session_id.to_string();
                    let event_tx = self.event_tx.clone();
                    self.state.status_message =
                        format!("Fetching remote session {session_id}...");
                    tokio::spawn(async move {
                        let result = provider
                            .fetch_remote_session_messages(&sid)
                            .await
                            .map_err(|e| e.to_string());
                        let _ = event_tx.send(AppEvent::SessionFetched {
                            conversation,
                            session_id: sid,
                            result,
                        });
                    });
                } else {
                    self.state.focused_mut().messages.push(ChatMessage::system(
                        &format!("Session {session_id} not found locally"),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Apply a remote-session fetch result delivered by `SessionFetched`.
    async fn apply_fetched_session(
        &mut self,
        conversation: conversation::ConversationId,
        session_id: &str,
        result: Result<Vec<ChatMessage>, String>,
    ) {
        use crate::session;

        let messages = match result {
            Ok(m) if m.is_empty() => {
                if let Some(conv) = self.state.conversations.get_mut(conversation) {
                    conv.messages.push(ChatMessage::system(&format!(
                        "Remote session {session_id} has no messages"
                    )));
                }
                return;
            }
            Ok(m) => m,
            Err(e) => {
                if let Some(conv) = self.state.conversations.get_mut(conversation) {
                    conv.messages.push(ChatMessage::system(&format!(
                        "Session {session_id} not found locally and remote fetch failed: {e}"
                    )));
                }
                return;
            }
        };

        let provider = self
            .state
            .conversations
            .get(conversation)
            .and_then(|c| c.provider.clone());
        let Some(conv) = self.state.conversations.get_mut(conversation) else {
            return;
        };
        conv.messages = messages;
        conv.session_id = session_id.to_string();
        conv.generation.active = false;
        if let Some(handle) = conv.agent_task.take() {
            handle.abort();
        }
        let count = conv.messages.len();
        let title = session::derive_title(&conv.messages);
        let snapshot = conv.messages.clone();

        if conversation == self.state.conversations.focused_id() {
            self.state.scroll_offset = 0;
            self.state.input.buffer.clear();
            self.state.input.cursor = 0;
            self.state.input.selection_anchor = None;
            self.state.autocomplete = Default::default();
        }
        if let Some(provider) = provider {
            // Local session-state reset only — no network.
            let _ = provider.reset().await;
        }

        let now = session::timestamp_now();
        if let Err(e) = session::save_local(
            &session::Session {
                version: session::SESSION_VERSION,
                id: session_id.to_string(),
                created_at: now.clone(),
                updated_at: now,
                workspace_root: std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                model_type: self.config.provider.model.clone(),
                messages: snapshot,
                tag: None,
            },
            &self.config,
        ) {
            tracing::warn!("Failed to save imported session: {e}");
        }
        self.state.status_message =
            format!("Imported remote session {session_id}: {title} ({count} msgs)");
    }

    fn record_gen_stats(&mut self) {
        let conv = self.state.focused_mut();
        let Some(elapsed) = conv.generation.take_elapsed() else {
            return;
        };
        let last_model = conv.generation.last_model.clone();
        let last_status = conv.generation.last_status.clone();
        if let Some(last) = conv.messages.iter_mut().last()
            && last.role == Role::Assistant && !last.content.is_empty() {
                let tokens = estimate_tokens(&last.content);
                last.total_tokens = Some(tokens);
                last.model = last_model;
                last.status = last_status;
                last.think_elapsed_secs = 0.0;
                last.references_count = 0;
                conv.generation.last_tokens = tokens;
                conv.generation.last_duration_secs = elapsed;
            }
    }

    fn auto_save_session(&self) {
        use crate::session;
        let id = &self.state.focused().session_id;
        let messages = &self.state.focused().messages;
        if messages.is_empty() {
            return;
        }
        let now = session::timestamp_now();
        let ws = &self.state.workspace_path;
        if let Err(e) = session::save_session(id, &now, messages, &self.config, ws) {
            tracing::warn!("Failed to auto-save session: {e}");
        }
    }

    fn render(&self, terminal: &mut crate::tui::TuiTerminal) -> AppResult<()> {
        crate::tui::render::render(terminal, &self.state, &self.config)
    }

    async fn open_whitelist_picker(&mut self) {
        use crate::app::events::{PickerItem, PickerMode, PickerKind, PickerState};
        let mut items: Vec<PickerItem> = Vec::new();
        let mut checked: Vec<usize> = Vec::new();
        let whitelist: HashSet<String> = crate::whitelist::load();

        // Built-in tools
        for def in self.tools.definitions() {
            let in_list = whitelist.contains(&def.name);
            items.push(PickerItem::new(
                format!("{}  {}", if in_list { "\u{2611}" } else { "\u{2610}" }, def.name),
                def.name.clone(),
            ));
            if in_list {
                checked.push(items.len() - 1);
            }
        }

        // MCP tools
        let mcp = self.mcp.lock().await;
        for full in mcp.get_all_tools() {
            let in_list = whitelist.contains(&full.full_name);
            items.push(PickerItem::new(
                format!("{}  {}", if in_list { "\u{2611}" } else { "\u{2610}" }, full.full_name),
                full.full_name.clone(),
            ));
            if in_list {
                checked.push(items.len() - 1);
            }
        }
        drop(mcp);

        if items.is_empty() {
            self.state.focused_mut().messages.push(crate::provider::ChatMessage::system(
                "No tools available to whitelist.",
            ));
            return;
        }

        let mut picker = PickerState::new_with_kind(
            " Tool Whitelist (Space to toggle, Enter to save)",
            items,
            PickerMode::Multi,
            PickerKind::Whitelist,
        );
        picker.checked = checked;
        picker.persistent_checked = whitelist.into_iter().collect();
        self.state.modal = Some(crate::app::events::Modal::Picker(picker));
    }

    async fn open_skill_picker(&mut self) {
        use crate::app::events::{PickerItem, PickerMode, PickerKind, PickerState};
        let mut items: Vec<PickerItem> = Vec::new();
        let mut checked: Vec<usize> = Vec::new();
        let enabled_slugs: HashSet<String> = self.config.skills.enabled.iter().cloned().collect();

        for skill in &self.skills {
            let is_enabled = enabled_slugs.contains(&skill.slug) || enabled_slugs.contains(&skill.name);
            let status = if is_enabled { "\u{2611}" } else { "\u{2610}" };
            items.push(PickerItem::new(
                format!(
                    "{}  {}  [{}] {}",
                    status, skill.name, skill.source, skill.description
                ),
                skill.slug.clone(),
            ));
            if is_enabled {
                checked.push(items.len() - 1);
            }
        }

        if items.is_empty() {
            self.state.focused_mut().messages.push(crate::provider::ChatMessage::system(
                "No skills found. Install skills with `npx skills add <owner/repo>` or create SKILL.md files in `.skills/` directory.",
            ));
            return;
        }

        let mut picker = PickerState::new_with_kind(
            " Skills (Space to toggle, Enter to save)",
            items,
            PickerMode::Multi,
            PickerKind::Skills,
        );
        picker.checked = checked;
        picker.persistent_checked = self.config.skills.enabled.clone();
        self.state.modal = Some(crate::app::events::Modal::Picker(picker));
    }

    async fn toggle_skill(&mut self, name: &str, enable: bool) {
        let mut changed = false;
        let name_lower = name.to_lowercase();
        for skill in &mut self.skills {
            if skill.slug.to_lowercase() == name_lower || skill.name.to_lowercase() == name_lower {
                if skill.enabled != enable {
                    skill.enabled = enable;
                    changed = true;
                }
                break;
            }
        }

        if changed {
            let enabled: Vec<String> = self.skills.iter()
                .filter(|s| s.enabled)
                .map(|s| s.slug.clone())
                .collect();
            self.config.skills.enabled = enabled.clone();
            let mut config = match crate::config::load() {
                Ok(c) => c,
                Err(_) => return,
            };
            config.skills.enabled = enabled;
            if let Err(e) = crate::config::save(&config) {
                tracing::warn!("Failed to save skills config: {e}");
            }
            self.tools.update_skills(self.skills.clone());

            let msg = if enable {
                format!("Skill '{name}' enabled. Its content will be included in the system prompt on next request.")
            } else {
                format!("Skill '{name}' disabled.")
            };
            self.state.focused_mut().messages.push(crate::provider::ChatMessage::system(&msg));
        } else if enable {
            self.state.focused_mut().messages.push(crate::provider::ChatMessage::system(
                &format!("Skill '{name}' not found. Use /skills list to see available skills."),
            ));
        }
    }

    async fn build_tools_display(&self) -> String {
        let mut lines = vec!["## Available Tools".to_string()];

        let builtin = self.tools.definitions();
        if builtin.is_empty() {
            lines.push("\n### Built-in tools".to_string());
            lines.push("- none".to_string());
        } else {
            lines.push(format!("\n### Built-in tools ({})", builtin.len()));
            for tool in &builtin {
                lines.push(format_tool_definition(&tool.name, &tool.description, &tool.parameters));
            }
        }

        let mcp = self.mcp.lock().await;
        let all_mcp = mcp.get_all_tools();
        if all_mcp.is_empty() {
            lines.push("\n### MCP tools".to_string());
            lines.push("- none".to_string());
        } else {
            let servers = mcp.get_servers_info();
            let enabled_count = servers.iter().filter(|s| s.enabled).count();
            lines.push(format!("\n### MCP tools ({all} total, {enabled} enabled, {conn} connected)",
                all = all_mcp.len(),
                enabled = enabled_count,
                conn = servers.iter().filter(|s| s.enabled && s.status == "connected").count(),
            ));
            for full in &all_mcp {
                let server_name = &full.tool.server_name;
                let server_info = servers.iter().find(|s| s.name == *server_name);
                let status = server_info.map(|s| s.status.as_str()).unwrap_or("unknown");
                lines.push(format!(
                    "  *Server: `{}` ({status})*",
                    server_name
                ));
                lines.push(format_tool_definition(
                    &full.full_name,
                    &full.tool.description,
                    &full.tool.input_schema,
                ));
            }
        }
        drop(mcp);

        lines.join("\n\n")
    }

    async fn build_background_processes_display(&self) -> String {
        let snapshots = crate::tools::background::process_snapshots().await;
        if snapshots.is_empty() {
            return "## Jobs\n\n- none".to_string();
        }

        let now = chrono::Utc::now();
        let running = snapshots
            .iter()
            .filter(|proc| matches!(proc.status, crate::tools::background::ProcessStatus::Running))
            .count();
        let interactive = snapshots.iter().filter(|proc| proc.interactive).count();
        let persistent = snapshots.iter().filter(|proc| proc.persistent).count();

        let mut lines = vec![format!(
            "## Jobs\n\n- total: {}\n- running: {}\n- interactive: {}\n- persistent: {}",
            snapshots.len(),
            running,
            interactive,
            persistent
        )];

        for proc in snapshots {
            let pid = proc
                .pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "-".to_string());
            let kind = if proc.interactive { "interactive" } else { "background" };
            let persist = if proc.persistent { " persistent" } else { "" };
            let age = format_duration_secs(now.signed_duration_since(proc.started_at).num_seconds().max(0) as u64);
            let idle = format_duration_secs(now.signed_duration_since(proc.last_activity_at).num_seconds().max(0) as u64);
            let ttl = match proc.ttl_secs {
                Some(0) => " ttl=off".to_string(),
                Some(ttl) => format!(" ttl={}", format_duration_secs(ttl)),
                None => String::new(),
            };
            let preview: String = proc.command.chars().take(120).collect();
            lines.push(format!(
                "- id={} pid={} [{}] {}{} {} age={} idle={}{}: `{}`",
                proc.id,
                pid,
                proc.shell,
                kind,
                persist,
                proc.status.label(),
                age,
                idle,
                ttl,
                preview
            ));
        }

        lines.join("\n")
    }

    fn expand_file_mentions(&self, input: &str) -> String {
        let workspace = std::path::Path::new(&self.state.workspace_path);
        let mentions = crate::cli::file_mentions::extract_mentions(input, workspace);

        if mentions.is_empty() {
            return input.to_string();
        }

        let mut result = input.to_string();
        for mention in &mentions {
            let tag = format!("@{}", mention.path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file"));
            let replacement = crate::cli::file_mentions::format_mention(mention);
            result = result.replace(&tag, &replacement);
        }

        result
    }
}

pub fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

pub fn format_duration_secs(seconds: u64) -> String {
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86_399 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

fn format_tool_definition(name: &str, description: &str, schema: &serde_json::Value) -> String {
    let mut result = format!("- `{name}`: {description}");

    if let Some(props) = schema
        .get("properties")
        .and_then(|p| p.as_object())
        && !props.is_empty() {
            result.push_str("\n  Parameters:");
            let required = schema
                .get("required")
                .and_then(|r| r.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect::<std::collections::HashSet<_>>()
                })
                .unwrap_or_default();
            let mut params: Vec<(&String, &serde_json::Value)> = props.iter().collect();
            params.sort_by(|a, b| {
                let a_req = required.contains(a.0);
                let b_req = required.contains(b.0);
                b_req.cmp(&a_req).then(a.0.cmp(b.0))
            });
            for (param_name, param_info) in params {
                let param_type = param_info
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("any");
                let param_desc = param_info
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("");
                let req_str = if required.contains(param_name) {
                    "required"
                } else {
                    "optional"
                };
                result.push_str(&format!(
                    "\n    \u{2022} `{param_name}` ({param_type}, {req_str}): {param_desc}"
                ));
            }
        }

    result
}
