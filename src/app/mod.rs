pub mod background_stats;
pub mod conversation;
pub mod events;
pub mod generation;
mod goal;
pub mod input;
mod keys;
pub mod mcp_add;
pub mod mcp_status;
mod multichat;
mod persist;
mod pickers;
pub mod providers;
mod runtime;
pub mod search;
mod serve;
mod sessions;
mod system_prompt;
pub mod themes;

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
    AppEvent, ConfirmAction, GoalStage, Modal, OnboardingState, PendingInteraction, QuestionRequest,
    QuestionState, ToolApprovalRequest, View,
};
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
    /// Local semantic matcher over skills + MCP tools (also backs the
    /// `tool_search` builtin). The app refreshes its MCP corpus whenever
    /// the server set changes.
    semantic: Arc<crate::semantic::SemanticService>,
    /// Launches agent turns (owns the tool registry / MCP / event channel for
    /// execution). The single place the agent loop is spawned.
    runtime: runtime::AgentRuntime,
    /// Serialized off-loop writer for session/history files — see
    /// `app::persist` for why ordering matters.
    persister: persist::Persister,
    /// The running API server (`/serve on`, `--serve`), if any.
    server: Option<crate::server::ServerHandle>,
    /// Monotonic server-launch counter — see `AppEvent::ServerStarted`.
    server_generation: u64,
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
    pub onboarding: OnboardingState,
    pub mcp_status: mcp_status::McpStatus,
    pub providers_view: providers::ProvidersViewState,
    pub search: search::SearchViewState,
    pub themes: themes::ThemesViewState,
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

    /// Reset the focused conversation's chat view: wipe its messages and
    /// clear the scroll / autocomplete state. Shared by `/clear`, `/home`,
    /// and `/reset`.
    pub fn clear_chat_view(&mut self) {
        self.focused_mut().messages.clear();
        self.scroll_offset = 0;
        self.autocomplete = AutocompleteState::default();
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

        let provider: Option<Arc<dyn LLMProvider>> = crate::provider::build_provider(&config);

        let mcp = Arc::new(tokio::sync::Mutex::new(MCPManager::new()));
        let tools = Arc::new(ToolRegistry::new());
        let prompts = prompts::load_prompt_files();

        let mut skills = discover_all_skills(&config.skills.paths);
        for skill in &mut skills {
            if config.skills.enabled.contains(&skill.slug) || config.skills.enabled.contains(&skill.name) {
                skill.enabled = true;
            }
        }
        tools.update_skills(skills.clone());

        let has_provider = provider.is_some();
        let main_conversation = conversation::Conversation::fresh_main(provider);

        let state = AppState {
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
            view: if has_provider { View::Chat } else { View::Onboarding },
            onboarding: OnboardingState::default(),
            mcp_status: mcp_status::McpStatus::default(),
            providers_view: providers::ProvidersViewState::default(),
            search: search::SearchViewState::default(),
            themes: themes::ThemesViewState::default(),
            show_stats_panel: true,
            workspace_path: std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
            attached_files: Vec::new(),

            goal: goal::GoalState::default(),
            needs_terminal_restore: false,
            background: background_stats::BackgroundCounters::default(),
        };

        // Semantic matcher: background init (first run downloads the
        // embedding model); turns get skill/MCP-tool hints once it's ready.
        // `tool_search` is registered unconditionally — before readiness it
        // degrades to lexical matching over the raw tool list.
        let semantic =
            crate::semantic::SemanticService::start(&config, skills.clone(), event_tx.clone());
        tools.register_semantic_tools(Arc::clone(&semantic));

        // MCP discovery + connect runs in the background so the first frame
        // never waits on a slow or unreachable server (a synchronous
        // `initialize().await` here used to hold a blank terminal for up to
        // ~60s). `startup_initialize` takes the manager lock only for short
        // await-free merges, so event-loop paths that `lock().await` the
        // manager (sending a turn, the /mcp view) stay responsive while
        // servers connect. Counts and the semantic MCP corpus catch up via
        // the 2s stats poll and `McpInitialized`.
        {
            let mcp = Arc::clone(&mcp);
            let semantic = Arc::clone(&semantic);
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                crate::mcp::manager::startup_initialize(&mcp).await;
                let tools = mcp.lock().await.get_all_tools();
                semantic.update_mcp_tools(tools);
                let _ = event_tx.send(AppEvent::McpInitialized);
            });
        }

        let runtime = runtime::AgentRuntime::new(
            Arc::clone(&tools),
            Arc::clone(&mcp),
            Arc::clone(&semantic),
            event_tx.clone(),
        );

        let persister = persist::Persister::start(Arc::clone(&semantic));

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
            semantic,
            runtime,
            persister,
            server: None,
            server_generation: 0,
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
        // The API server dies with the app — nobody is left to receive its
        // lifecycle events, so a hard abort beats a graceful shutdown here.
        if let Some(server) = self.server.take() {
            server.abort();
        }
        // Ephemeral conversations' remote sessions die with them. Bounded —
        // exiting must not hang on a dead network.
        let discards: Vec<_> = self
            .state
            .conversations
            .iter()
            .filter(|c| c.is_background_kind())
            .filter_map(|c| c.provider.clone())
            .map(|provider| async move {
                let _ = provider.discard_remote_session().await;
            })
            .collect();
        if !discards.is_empty() {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                futures::future::join_all(discards),
            )
            .await;
        }
        // Queued session/history writes must land before exit — per-turn
        // saves run on the persist worker now, and quitting right after a
        // turn completes must not lose that turn's save. Bounded like the
        // remote discards above: exiting must not hang on a wedged disk.
        self.persister.flush(std::time::Duration::from_secs(3)).await;
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
        self.state.view == View::Onboarding
            || self.state.conversations.iter().any(|c| c.is_streaming())
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
                if self.state.view == View::Onboarding
                    || self.state.focused_mut().generation.active
                    || (self.state.focused_mut().messages.is_empty() && self.state.modal.is_none())
                {
                    self.state.focused_mut().generation.animation_tick =
                        self.state.focused_mut().generation.animation_tick.wrapping_add(1);
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
            AppEvent::SessionAvailabilityChecked { conversation, session, remote_id, parent_message_id, alive } => {
                self.apply_session_availability(conversation, session, remote_id, parent_message_id, alive).await;
            }
            AppEvent::RemoteSessionsListed { result } => {
                if let Some(Modal::DeleteSessions(st)) = self.state.modal.as_mut() {
                    match result {
                        Ok(sessions) => st.merge_remote(sessions),
                        Err(e) => st.remote_status = events::RemoteListStatus::Failed(e),
                    }
                }
            }
            AppEvent::ModelsListed { result, switch_to } => {
                self.handle_models_listed(result, switch_to);
            }
            AppEvent::SessionsDeleted { deleted, failed } => {
                let mut message = format!(
                    "🗑 Deleted {deleted} session cop{}",
                    if deleted == 1 { "y" } else { "ies" }
                );
                if !failed.is_empty() {
                    message.push_str(&format!(
                        "; {} failed:\n  {}",
                        failed.len(),
                        failed.join("\n  ")
                    ));
                }
                self.state.status_message = if failed.is_empty() {
                    "Sessions deleted".to_string()
                } else {
                    "Some session deletions failed".to_string()
                };
                self.state.focused_mut().messages.push(ChatMessage::ui_system(&message));
            }
            AppEvent::McpOperationDone { message } => {
                self.state.mcp_status.view.status_message = message.clone();
                self.state.status_message = message.clone();
                self.state.focused_mut().messages.push(ChatMessage::ui_system(&message));
                // Force the next loop iteration to re-pull fresh server info.
                self.state.mcp_status.view.servers.clear();
                self.state.mcp_status.last_stats_update = None;
                // The tool set likely changed (add/reload/toggle/reconnect
                // all funnel through this event) — re-embed the MCP corpus.
                // Fetch under a short-lived lock off the event loop.
                let mcp = Arc::clone(&self.mcp);
                let semantic = Arc::clone(&self.semantic);
                tokio::spawn(async move {
                    let tools = mcp.lock().await.get_all_tools();
                    semantic.update_mcp_tools(tools);
                });
            }
            AppEvent::McpInitialized => {
                // Startup connects finished — force the next loop iteration
                // to re-pull fresh server counts instead of waiting out the
                // stats-poll throttle. The semantic MCP corpus was already
                // updated by the startup task itself.
                self.state.mcp_status.view.servers.clear();
                self.state.mcp_status.last_stats_update = None;
            }
            AppEvent::McpOAuthResult { server, result } => {
                match result {
                    Ok(()) => {
                        self.state.mcp_status.view.status_message = format!("{server} authorized, reconnecting...");
                        // The token is already persisted (oauth_store::save) —
                        // reconnect picks it up via `build_client`'s
                        // `with_bearer_header` call and reports its own
                        // outcome through the existing `McpOperationDone`.
                        let mcp = Arc::clone(&self.mcp);
                        let event_tx = self.event_tx.clone();
                        tokio::spawn(async move {
                            let message = match mcp.lock().await.reconnect_server(&server).await {
                                Err(e) => format!("Reconnect after authorization failed: {e}"),
                                Ok(_) => format!("{server} authorized and reconnected"),
                            };
                            let _ = event_tx.send(AppEvent::McpOperationDone { message });
                        });
                    }
                    Err(e) => {
                        self.state.mcp_status.view.status_message = format!("Authorization failed: {e}");
                    }
                }
            }
            AppEvent::SemanticStatus(message) => {
                self.state.status_message = message;
            }
            AppEvent::ServerStarted { generation, addr } => {
                if let Some(handle) = &mut self.server
                    && handle.generation == generation
                {
                    handle.bound_addr = Some(addr);
                    let message = format!(
                        "API server listening on http://{addr}/v1 ({} dialect).",
                        handle.api.label()
                    );
                    self.state.status_message = message.clone();
                    self.state.focused_mut().messages.push(ChatMessage::ui_system(&message));
                }
            }
            AppEvent::ServerFailed { generation, error } => {
                if self.server.as_ref().is_some_and(|handle| handle.generation == generation) {
                    self.server = None;
                }
                let message = format!("API server failed to start: {error}");
                self.state.status_message = message.clone();
                self.state.focused_mut().messages.push(ChatMessage::ui_system(&message));
            }
            AppEvent::ServerStopped { generation } => {
                // Only the *current* server's stop clears the handle; a
                // replaced server (port change restart) reports late.
                if self.server.as_ref().is_some_and(|handle| handle.generation == generation) {
                    self.server = None;
                    self.state.status_message = "API server stopped".to_string();
                    self.state.focused_mut().messages.push(ChatMessage::ui_system("API server stopped."));
                }
            }
            AppEvent::HistorySearchDone { query, matches } => {
                let search = &mut self.state.search;
                // Only the reply to the latest query counts.
                if search.last_query == query {
                    search.searching = false;
                    search.matches = matches;
                    search.reset_selection();
                    search.status = if search.matches.is_empty() {
                        "No matches — the index may still be building (see /rag)".to_string()
                    } else {
                        format!("{} matches", search.matches.len())
                    };
                    // Land the user straight on the results.
                    if !search.matches.is_empty() {
                        search.focus = search::SearchFocus::Results;
                    }
                }
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
            self.effective_mcp_schema_mode(),
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

    /// MCP schema mode as the system prompt should see it. Without
    /// semantic matching there is no `tool_search` worth relying on, so
    /// deferring schemas would leave the model with no good path to them —
    /// force full inlining in that case.
    fn effective_mcp_schema_mode(&self) -> crate::config::McpSchemaMode {
        if self.config.semantic.enabled {
            self.config.semantic.mcp_schemas
        } else {
            crate::config::McpSchemaMode::Full
        }
    }

    /// Open the generic confirm modal for `/logout` or `/wipe`.
    pub(crate) fn open_confirm(&mut self, action: ConfirmAction) {
        use events::{ConfirmState};
        let cs = match action {
            ConfirmAction::Logout => ConfirmState::logout(),
            ConfirmAction::Wipe => ConfirmState::wipe(),
        };
        self.state.modal = Some(Modal::Confirm(cs));
    }

    /// Cancel every conversation's in-flight turn (focused + all background).
    async fn cancel_all_turns(&mut self) {
        kill_foreground_child();
        for conv in self.state.conversations.iter_mut() {
            if let Some(handle) = conv.agent_task.take() {
                handle.abort();
            }
            conv.generation.active = false;
        }
        if let Some(req) = self.state.pending_tool_approval.take() {
            req.resolve(false).await;
        }
        if let Some(req) = self.state.pending_question.take() {
            req.resolve(String::new()).await;
        }
        self.state.pending_interactions.clear();
    }

    /// Execute `/logout`: clear token from config+disk, return to onboarding.
    pub(crate) async fn execute_logout(&mut self) {
        self.cancel_all_turns().await;
        self.config.provider.token = String::new();
        if let Err(e) = crate::config::save(&self.config) {
            tracing::warn!("Logout: failed to save config: {e}");
        }
        self.reset_to_onboarding("Logged out".to_string());
    }

    /// Execute `/wipe`: delete all app-owned data dirs, factory-reset in memory.
    pub(crate) async fn execute_wipe(&mut self) {
        self.cancel_all_turns().await;
        // Drain queued writes first — a still-in-flight session save could
        // otherwise re-create files inside the directories deleted below.
        self.persister.flush(std::time::Duration::from_secs(3)).await;
        let roots = wipe_roots();
        let mut errors: Vec<String> = Vec::new();
        for root in &roots {
            match std::fs::remove_dir_all(root) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => errors.push(format!("{}: {e}", root.display())),
            }
        }
        self.config = crate::config::Config::default();
        self.state.approved_tools.clear();
        self.state.input.history.clear();
        if errors.is_empty() {
            self.reset_to_onboarding("All local data wiped".to_string());
        } else {
            // Chat messages are invisible on the onboarding view — surface via status.
            tracing::warn!("Wipe errors: {}", errors.join("; "));
            self.reset_to_onboarding(format!(
                "Wiped with errors — {} path(s) failed, see log",
                errors.len()
            ));
        }
    }

    /// Drop provider, swap to a single fresh empty conversation, land on onboarding.
    fn reset_to_onboarding(&mut self, status: String) {
        self.state.conversations =
            conversation::Conversations::new(conversation::Conversation::fresh_main(None));
        self.state.onboarding = OnboardingState::default();
        self.state.modal = None;
        self.state.view = View::Onboarding;
        self.state.status_message = status;
        self.state.scroll_offset = 0;
        self.state.input.buffer.clear();
        self.state.input.cursor = 0;
        self.state.input.selection_anchor = None;
        self.state.autocomplete = AutocompleteState::default();
    }

    /// Rebuild the focused conversation's provider from the current config
    /// (used after config edits and `/providers` switches) and start a
    /// fresh session id so the next turn can't thread onto state that
    /// belonged to the previous provider.
    pub(crate) fn rebuild_provider(&mut self) {
        self.state.focused_mut().provider = crate::provider::build_provider(&self.config);
        self.state.focused_mut().session_id = crate::session::create_session_id();
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

    fn render(&self, terminal: &mut crate::tui::TuiTerminal) -> AppResult<()> {
        crate::tui::render::render(terminal, &self.state, &self.config)
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

/// Returns the deduplicated list of directories that own all app-owned data.
/// On Windows config and data typically live in the same parent; on Linux they
/// may differ (XDG_CONFIG_HOME vs XDG_DATA_HOME).
fn wipe_roots() -> Vec<std::path::PathBuf> {
    let config_dir = crate::config::Config::path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(crate::config::Config::path);
    let mut roots = vec![config_dir, crate::config::Config::data_dir()];
    roots.sort();
    roots.dedup();
    roots
}

#[cfg(test)]
mod wipe_tests {
    use super::wipe_roots;

    #[test]
    fn wipe_roots_no_duplicates() {
        let roots = wipe_roots();
        let mut deduped = roots.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(roots.len(), deduped.len(), "wipe_roots must return deduped paths");
        assert!(!roots.is_empty());
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
