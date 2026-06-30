pub mod conversation;
pub mod events;
pub mod generation;
mod goal;
pub mod input;
pub mod mcp_status;

use crate::commands::CommandRegistry;
use crate::config::Config;
use crate::error::AppResult;
use crate::mcp::MCPManager;
use crate::prompts::{self, PromptFiles};
use crate::provider::{ChatMessage, LLMProvider, Role};
use crate::commands::CommandResult;
use crate::provider::estimate_tokens;
use crate::tools::registry::ToolRegistry;
use crate::commands::CommandSuggestion;
use crate::skills::{discovery::discover_all_skills, SkillDefinition};
use events::{AppEvent, GoalStage, GoalVerdict, Modal, QuestionRequest, ToolApprovalRequest, View};
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
                .args(&["/F", "/T", "/PID", &pid.to_string()])
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

/// A short display title for a conversation, from its first user message.
fn conversation_title(messages: &[ChatMessage]) -> String {
    messages
        .iter()
        .find(|m| m.role == Role::User)
        .map(|m| m.content.chars().take(40).collect::<String>().replace('\n', " "))
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "(empty chat)".to_string())
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
    provider: Option<Arc<dyn LLMProvider>>,
    commands: CommandRegistry,
    pub mcp: Arc<tokio::sync::Mutex<MCPManager>>,
    tools: Arc<ToolRegistry>,
    prompts: PromptFiles,
    skills: Vec<SkillDefinition>,
    agent_task: Option<tokio::task::JoinHandle<()>>,
}

pub struct AppState {
    pub messages: Vec<ChatMessage>,
    pub input: input::InputState,
    pub generation: generation::GenerationState,
    pub status_message: String,
    pub scroll_offset: u32,
    pub error: Option<String>,
    pub modal: Option<Modal>,
    pub approved_tools: std::collections::HashSet<String>,
    pub pending_tool_approval: Option<ToolApprovalRequest>,
    pub pending_question: Option<QuestionRequest>,
    pub autocomplete: AutocompleteState,
    pub current_session_id: String,
    pub view: View,
    pub mcp_status: mcp_status::McpStatus,
    pub workspace_path: String,
    pub session_started_at: String,
    pub show_stats_panel: bool,
    pub attached_files: Vec<crate::provider::AttachedFile>,

    // Goal mode state
    pub goal: goal::GoalState,

    pub needs_terminal_restore: bool,
    pub running_background_count: usize,
    pub running_interactive_count: usize,
    pub running_persistent_count: usize,

    /// Identity of the focused conversation (whose live state is the fields
    /// above). Background conversations are parked in `background`.
    pub focused_id: conversation::ConversationId,
    /// Kind of the focused conversation (round-tripped through park/activate).
    pub focused_kind: conversation::ConversationKind,
    /// Non-focused conversations (sidechats / sub-agents / parallel sessions)
    /// that keep streaming on their own tasks.
    pub background: Vec<conversation::Conversation>,
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

        let mut state = AppState {
            messages: Vec::new(),
            input: input::InputState {
                history: crate::session::load_history(),
                ..Default::default()
            },
            generation: generation::GenerationState::default(),
            status_message: if provider.is_some() { "Ready" } else { "No token configured" }.to_string(),
            scroll_offset: 0,
            error: None,
            modal: None,
            approved_tools: crate::whitelist::load(),
            pending_tool_approval: None,
            pending_question: None,
            autocomplete: AutocompleteState::default(),
            current_session_id: crate::session::create_session_id(),
            view: View::Chat,
            mcp_status: mcp_status::McpStatus::default(),
            show_stats_panel: true,
            workspace_path: std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
            session_started_at: chrono::Utc::now().to_rfc3339(),
            attached_files: Vec::new(),

            goal: goal::GoalState::default(),
            needs_terminal_restore: false,
            running_background_count: 0,
            running_interactive_count: 0,
            running_persistent_count: 0,

            focused_id: conversation::ConversationId::next(),
            focused_kind: conversation::ConversationKind::Main,
            background: Vec::new(),
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

        Ok(Self {
            config,
            state,
            event_tx,
            event_rx,
            provider,
            commands: CommandRegistry::new(),
            mcp,
            tools,
            prompts,
            skills,
            agent_task: None,
        })
    }

    pub async fn run(&mut self) -> AppResult<()> {
        let mut terminal = crate::tui::init()?;
        let result = self.run_loop(&mut terminal).await;
        // Kill any running foreground child before restoring terminal.
        kill_foreground_child();
        crate::tui::restore(&mut terminal)?;
        // Kill any lingering agent task so it can't keep the runtime alive.
        if let Some(handle) = self.agent_task.take() {
            handle.abort();
        }
        // Abort background conversations (sidechats / sub-agents) too.
        for conv in &mut self.state.background {
            if let Some(handle) = conv.agent_task.take() {
                handle.abort();
            }
        }
        // Kill all background/PTY processes so spawn_blocking waiters unblock
        // and the tokio runtime can shut down cleanly.
        let _ = crate::tools::background::shutdown_all().await;
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
            tokio::select! {
                _ = tick_interval.tick() => {
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
                            if self.handle_event(AppEvent::Key(key)).await? {
                                return Ok(());
                            }
                        }
                        crossterm::event::Event::Resize(w, h) => {
                            self.handle_event(AppEvent::Resize(w, h)).await?;
                        }
                        _ => {}
                    }
                }
                Some(event) = self.event_rx.recv() => {
                    if self.handle_event(event).await? {
                        return Ok(());
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    // Kill any running foreground child before restoring terminal.
                    kill_foreground_child();
                    let _ = self.shutdown_background_processes().await;
                    return Ok(());
                }
            }

            self.refresh_mcp_view().await;
            self.update_mcp_stats().await;
            self.update_background_stats().await;

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
            }

            self.render(terminal)?;
        }
    }

    async fn update_mcp_stats(&mut self) {
        const MCP_STATS_INTERVAL_SECS: u64 = 2;
        if let Some(last) = self.state.mcp_status.last_stats_update {
            if last.elapsed().as_secs() < MCP_STATS_INTERVAL_SECS {
                return;
            }
        }
        let mcp = self.mcp.lock().await;
        let servers = mcp.get_servers_info();
        self.state.mcp_status.server_count = servers.len();
        self.state.mcp_status.connected_count = servers
            .iter()
            .filter(|s| s.enabled && s.status == "connected")
            .count();
        self.state.mcp_status.view.servers = servers;
        self.state.mcp_status.last_stats_update = Some(std::time::Instant::now());
    }

    async fn update_background_stats(&mut self) {
        let _ = crate::tools::background::expire_persistent_idle_processes().await;
        let _ = crate::tools::background::prune_finished_processes().await;
        let (total, interactive, persistent) = crate::tools::background::running_process_counts().await;
        self.state.running_background_count = total;
        self.state.running_interactive_count = interactive;
        self.state.running_persistent_count = persistent;
    }

    async fn shutdown_background_processes(&mut self) -> usize {
        let killed = crate::tools::background::shutdown_all().await;
        self.state.running_background_count = 0;
        self.state.running_interactive_count = 0;
        self.state.running_persistent_count = 0;
        killed
    }

    async fn cleanup_background_before_user_turn(&mut self) -> usize {
        if self.state.running_background_count == 0 {
            return 0;
        }
        let killed = crate::tools::background::shutdown_nonpersistent().await;
        self.update_background_stats().await;
        killed
    }

    async fn kill_background_job(&mut self, id: u64) -> String {
        match crate::tools::background::kill_process(id).await {
            Some(Ok(())) => {
                let _ = crate::tools::background::remove_process(id).await;
                self.update_background_stats().await;
                format!("Stopped job #{id}.")
            }
            Some(Err(error)) => format!("Failed to stop job #{id}: {error}"),
            None => format!("No job with id={id}."),
        }
    }

    async fn prune_background_jobs(&mut self) -> String {
        let (finished, expired) = crate::tools::background::prune_jobs().await;
        self.update_background_stats().await;
        if finished == 0 && expired == 0 {
            "No jobs pruned.".to_string()
        } else {
            format!("Pruned jobs: finished={finished}, expired={expired}.")
        }
    }

    async fn refresh_mcp_view(&mut self) {
        if self.state.view != View::Mcp || !self.state.mcp_status.view.servers.is_empty() {
            return;
        }
        let mcp = self.mcp.lock().await;
        self.state.mcp_status.view.servers = mcp.get_servers_info();
    }

    async fn handle_event(&mut self, event: AppEvent) -> AppResult<bool> {
        // Agent events for a background conversation are applied to its parked
        // record, not the focused chat.
        if let Some(target) = agent_event_target(&event) {
            if target != self.state.focused_id {
                self.handle_background_event(target, event);
                return Ok(false);
            }
        }
        match event {
            AppEvent::Key(key) => return self.handle_key(key).await,
            AppEvent::AgentStarted(_) => {
                self.state.generation.begin(std::time::Instant::now());
                self.state.status_message = "Thinking...".to_string();
            }
            AppEvent::BeginAssistantMessage(_) => {
                let should_push = self
                    .state
                    .messages
                    .last()
                    .is_none_or(|message| message.role != Role::Assistant || !message.content.is_empty());
                if should_push {
                    self.state.messages.push(ChatMessage::assistant(""));
                }
            }
            AppEvent::DiscardEmptyAssistantMessage(_) => {
                if self
                    .state
                    .messages
                    .last()
                    .is_some_and(|message| message.role == Role::Assistant && message.content.is_empty())
                {
                    self.state.messages.pop();
                }
            }
            AppEvent::AgentChunk(_, chunk) => {
                if let Some(last) = self.state.messages.last_mut() {
                    if last.role == Role::Assistant {
                        last.content.push_str(&chunk);
                    }
                }
            }
            AppEvent::AgentDone(_, _result) => {
                self.state.generation.active = false;
                self.state.status_message = "Ready".to_string();
                self.state.generation.last_status = Some("FINISHED".to_string());
                self.agent_task = None;
                self.record_gen_stats();
                if self
                    .state
                    .messages
                    .last()
                    .is_some_and(|message| message.role == Role::Assistant && message.content.is_empty())
                {
                    self.state.messages.pop();
                }
                self.auto_save_session();

                // --- GOAL cycle check ---
                if self.state.goal.mode {
                    match self.state.goal.stage.clone() {
                        GoalStage::RunAgent1 => {
                            // Agent 1 finished — get last assistant content for evaluation
                            let agent_result = self.state.messages
                                .iter()
                                .rev()
                                .find(|m| m.role == Role::Assistant)
                                .map(|m| m.content.clone())
                                .unwrap_or_default();

                            if !agent_result.is_empty() {
                                self.state.status_message = "Evaluating goal...".to_string();
                                self.state.goal.stage = GoalStage::RunEvaluator;
                                self.state.messages.push(ChatMessage::system(
                                    "🔍 Evaluating result against goal..."
                                ));
                                self.run_goal_evaluation(agent_result).await;
                            } else {
                                self.state.messages.push(ChatMessage::system(
                                    "⚠ Agent produced no output. Retrying..."
                                ));
                                self.retry_agent1().await;
                            }
                        }
                        GoalStage::RunEvaluator => {
                            // Evaluator finished — parse the verdict
                            self.handle_goal_verdict().await;
                        }
                        _ => {}
                    }
                }
            }
            AppEvent::AgentError(_, err) => {
                self.state.generation.active = false;
                self.state.error = Some(err.clone());
                self.state.status_message = err.clone();
                self.state.generation.last_status = Some("ABORTED".to_string());
                self.agent_task = None;
                self.record_gen_stats();
                if self
                    .state
                    .messages
                    .last()
                    .is_some_and(|message| message.role == Role::Assistant && message.content.is_empty())
                {
                    self.state.messages.pop();
                }
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
                self.state.messages.push(message);
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
                    self.state.generation.active = true;
                    self.state.status_message = format!("Running {} (auto-approved)", request.tool_name);
                } else {
                    self.state.generation.active = false;
                    self.state.status_message = format!("Approve tool {}?", request.tool_name);
                    self.state.modal = Some(Modal::ToolApproval {
                        tool_name: request.tool_name.clone(),
                        arguments: request.arguments.clone(),
                        scroll_offset: 0,
                        always_allow: false,
                    });
                    self.state.pending_tool_approval = Some(request);
                }
            }
            AppEvent::RequestQuestion(request, state) => {
                self.state.pending_question = Some(request);
                self.state.modal = Some(Modal::Question(state));
                self.state.generation.active = false;
                self.state.status_message = "Question pending...".to_string();
            }
            AppEvent::Tick => {
                if self.state.generation.active || (self.state.messages.is_empty() && self.state.modal.is_none()) {
                    self.state.generation.animation_tick = self.state.generation.animation_tick.wrapping_add(1);
                }
            }
            AppEvent::GoalEvaluationDone(verdict) => {
                self.handle_goal_verdict_from(verdict).await;
            }
            AppEvent::GoalCycleFinished => {
                self.state.status_message = "Goal achieved!".to_string();
            }
            _ => {}
        }
        Ok(false)
    }

    async fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> AppResult<bool> {
        use crossterm::event::{KeyCode, KeyModifiers};

        // Global Tab/Up/Down are intercepted by autocomplete when visible.
        let ac_visible = self.state.autocomplete.visible
            && !self.state.autocomplete.items.is_empty()
            && self.state.modal.is_none();
        if ac_visible {
            match key.code {
                KeyCode::Tab => {
                    let n = self.state.autocomplete.items.len();
                    self.state.autocomplete.selected =
                        (self.state.autocomplete.selected + 1) % n;
                    self.clamp_autocomplete_scroll();
                    return Ok(false);
                }
                KeyCode::BackTab => {
                    let n = self.state.autocomplete.items.len();
                    self.state.autocomplete.selected =
                        (self.state.autocomplete.selected + n - 1) % n;
                    self.clamp_autocomplete_scroll();
                    return Ok(false);
                }
                KeyCode::Down => {
                    let n = self.state.autocomplete.items.len();
                    self.state.autocomplete.selected =
                        (self.state.autocomplete.selected + 1) % n;
                    self.clamp_autocomplete_scroll();
                    return Ok(false);
                }
                KeyCode::Up => {
                    let n = self.state.autocomplete.items.len();
                    self.state.autocomplete.selected =
                        (self.state.autocomplete.selected + n - 1) % n;
                    self.clamp_autocomplete_scroll();
                    return Ok(false);
                }
                KeyCode::Enter if !self.state.generation.active => {
                    self.accept_autocomplete();
                    return Ok(false);
                }
                _ => {}
            }
        }

        if let Some(modal) = self.state.modal.clone() {
            match modal {
                Modal::ToolApproval { tool_name, arguments, scroll_offset, always_allow } => {
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                            if always_allow {
                                self.state.approved_tools.insert(tool_name.clone());
                            }
                            if let Some(request) = self.state.pending_tool_approval.take() {
                                request.resolve(true).await;
                            }
                            self.state.modal = None;
                            self.state.generation.active = true;
                            self.state.status_message = format!("Running {}", tool_name);
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                            if let Some(request) = self.state.pending_tool_approval.take() {
                                request.resolve(false).await;
                            }
                            self.state.modal = None;
                            self.state.generation.active = true;
                            self.state.status_message = format!("Denied {}", tool_name);
                        }
                        KeyCode::Char('a') | KeyCode::Char('A') => {
                            self.state.modal = Some(Modal::ToolApproval {
                                tool_name, arguments,
                                scroll_offset,
                                always_allow: !always_allow,
                            });
                        }
                        KeyCode::Up => {
                            let new_offset = scroll_offset.saturating_sub(3);
                            self.state.modal = Some(Modal::ToolApproval {
                                tool_name, arguments,
                                scroll_offset: new_offset,
                                always_allow,
                            });
                        }
                        KeyCode::Down => {
                            let arg_lines = arguments.lines().count();
                            let max_visible = 12usize;
                            let max_scroll = arg_lines.saturating_sub(max_visible);
                            let new_offset = (scroll_offset + 3).min(max_scroll);
                            self.state.modal = Some(Modal::ToolApproval {
                                tool_name, arguments,
                                scroll_offset: new_offset,
                                always_allow,
                            });
                        }
                        _ => {}
                    }
                    return Ok(false);
                }
                Modal::Picker(mut picker) => {
                    // Ctrl+A: toggle select all filtered items
                    if matches!(key.code, KeyCode::Char('a') | KeyCode::Char('A'))
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        let filtered_count = picker.items.len();
                        if filtered_count > 0 && picker.persistent_checked.len() >= filtered_count {
                            picker.persistent_checked.clear();
                        } else {
                            picker.persistent_checked = picker.items.iter().map(|item| item.value.clone()).collect();
                        }
                        picker.sync_checked();
                        self.state.modal = Some(Modal::Picker(picker));
                        return Ok(false);
                    }
                    match key.code {
                        KeyCode::Char(c) if !matches!(c, 'j' | 'k' | ' ') => {
                            let mut s = picker.search.clone();
                            s.push(c);
                            picker.update_search(s);
                            self.state.modal = Some(Modal::Picker(picker));
                            return Ok(false);
                        }
                        KeyCode::Backspace => {
                            let mut s = picker.search.clone();
                            s.pop();
                            picker.update_search(s);
                            self.state.modal = Some(Modal::Picker(picker));
                            return Ok(false);
                        }
                        _ => {}
                    }
                    let kind = picker.kind.clone();
                    let action = events::handle_picker_key(&mut picker, key.code);
                    match action {
                        events::PickerAction::Selected(indices) => {
                            match kind {
                                events::PickerKind::Whitelist => {
                                    let selected_names: HashSet<String> = indices
                                        .iter()
                                        .filter_map(|&i| picker.items.get(i))
                                        .map(|item| item.value.clone())
                                        .collect();
                                    if let Err(e) = crate::whitelist::save(&selected_names) {
                                        tracing::warn!("Failed to save whitelist: {e}");
                                    }
                                    self.state.approved_tools = selected_names;
                                    self.state.modal = None;
                                }
                                events::PickerKind::Skills => {
                                    let mut config = match crate::config::load() {
                                        Ok(c) => c,
                                        Err(_) => {
                                            self.state.modal = None;
                                            return Ok(false);
                                        }
                                    };
                                    let enabled: Vec<String> = indices
                                        .iter()
                                        .filter_map(|&i| picker.items.get(i))
                                        .map(|item| item.value.clone())
                                        .collect();
                                    config.skills.enabled = enabled.clone();
                                    if let Err(e) = crate::config::save(&config) {
                                        tracing::warn!("Failed to save skills config: {e}");
                                    }
                                    self.config.skills.enabled = enabled.clone();
                                    for skill in &mut self.skills {
                                        skill.enabled = enabled.contains(&skill.slug) || enabled.contains(&skill.name);
                                    }
                                    self.tools.update_skills(self.skills.clone());
                                    self.state.modal = None;
                                }
                                events::PickerKind::Chats => {
                                    let target = indices
                                        .first()
                                        .and_then(|&i| picker.items.get(i))
                                        .and_then(|item| item.value.parse::<u64>().ok())
                                        .map(conversation::ConversationId);
                                    self.state.modal = None;
                                    if let Some(target) = target {
                                        self.switch_to(target);
                                    }
                                }
                                _ => {
                                    if let Some(idx) = indices.first() {
                                        if let Some(item) = picker.items.get(*idx) {
                                            let id = item.value.clone();
                                            self.state.modal = None;
                                            self.handle_load_session(&id).await?;
                                        }
                                    }
                                    self.state.modal = None;
                                }
                            }
                        }
                        events::PickerAction::Cancelled => {
                            self.state.modal = None;
                        }
                        events::PickerAction::None => {
                            self.state.modal = Some(Modal::Picker(picker));
                        }
                    }
                    return Ok(false);
                }
                Modal::Question(mut qs) => {
                    let result = events::handle_question_key(&mut qs, key.code);
                    if let Some(answer) = result {
                        if let Some(request) = self.state.pending_question.take() {
                            request.resolve(answer).await;
                        }
                        self.state.modal = None;
                        self.state.generation.active = true;
                        self.state.status_message = "Answer received".to_string();
                    } else {
                        self.state.modal = Some(Modal::Question(qs));
                        self.state.status_message = "Answering question...".to_string();
                    }
                    return Ok(false);
                }
            }
        }

        if self.state.view == View::Mcp {
            return self.handle_mcp_key(key).await;
        }

        match key.code {
            KeyCode::Char(c)
                if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(c, 'c' | 'C') =>
            {
                if self.state.generation.active {
                    // Kill the running foreground child process first.
                    kill_foreground_child();
                    if let Some(handle) = self.agent_task.take() {
                        handle.abort();
                    }
                    let killed = self.shutdown_background_processes().await;
                    self.state.generation.active = false;
                    self.state.status_message = if killed > 0 {
                        format!("Cancelled; killed {killed} background process(es)")
                    } else {
                        "Cancelled".to_string()
                    };
                    self.state.needs_terminal_restore = true;
                    if self
                        .state
                        .messages
                        .last()
                        .is_some_and(|message| message.role == Role::Assistant && message.content.is_empty())
                    {
                        self.state.messages.pop();
                    }
                    if self.state.goal.is_running() {
                        self.cancel_goal_cycle("⏹ Goal cycle cancelled. Use /goal to start a new one.");
                    }
                    return Ok(false);
                }
                let _ = self.shutdown_background_processes().await;
                return Ok(true);
            }
            KeyCode::Esc => {
                if self.state.generation.active {
                    // Kill the running foreground child process first.
                    kill_foreground_child();
                    // Cancel the current agent turn: abort the spawned task,
                    // reset is_generating so the user can type a new message.
                    if let Some(handle) = self.agent_task.take() {
                        handle.abort();
                    }
                    let killed = self.shutdown_background_processes().await;
                    self.state.generation.active = false;
                    self.state.status_message = if killed > 0 {
                        format!("Cancelled; killed {killed} background process(es)")
                    } else {
                        "Cancelled".to_string()
                    };
                    self.state.needs_terminal_restore = true;
                    if self
                        .state
                        .messages
                        .last()
                        .is_some_and(|message| message.role == Role::Assistant && message.content.is_empty())
                    {
                        self.state.messages.pop();
                    }
                    if self.state.goal.is_running() {
                        self.cancel_goal_cycle("⏹ Goal cycle cancelled. Use /goal to start a new one.");
                    }
                } else if self.state.messages.is_empty() {
                    let _ = self.shutdown_background_processes().await;
                    return Ok(true);
                } else {
                    // Esc with no active turn clears the chat; if a goal was mid
                    // setup, clear that too so we don't orphan its state.
                    if self.state.goal.mode {
                        self.state.goal.deactivate();
                    }
                    self.state.messages.clear();
                    self.state.scroll_offset = 0;
                }
            }
            KeyCode::Tab => {
                self.cycle_focus(1);
            }
            KeyCode::BackTab => {
                self.cycle_focus(-1);
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.show_stats_panel = !self.state.show_stats_panel;
            }
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.messages.clear();
                self.state.scroll_offset = 0;
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.input.select_all();
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.state.input.insert_newline();
            }
            KeyCode::Enter if !self.state.generation.active => {
                let buf = &self.state.input.buffer;
                let ends_with_backslash = buf
                    .chars()
                    .last()
                    .is_some_and(|c| c == '\\')
                    && self.state.input.cursor == buf.chars().count();
                if ends_with_backslash {
                    // Replace the trailing backslash with a newline (line continuation).
                    self.state.input.buffer.pop();
                    self.state.input.cursor -= 1;
                    self.state.input.insert_newline();
                } else {
                            let input = self.state.input.buffer.trim().to_string();
                            // Empty submission while defining a goal: nudge instead of silently ignoring it.
                            if input.is_empty()
                                && self.state.goal.mode
                                && matches!(self.state.goal.stage, GoalStage::Inactive | GoalStage::WaitForGoal)
                            {
                                let what = if matches!(self.state.goal.stage, GoalStage::Inactive) {
                                    "prompt"
                                } else {
                                    "goal"
                                };
                                self.state.messages.push(ChatMessage::system(&format!(
                                    "Your {what} is empty — type something before pressing Enter."
                                )));
                                self.state.input.buffer.clear();
                                self.state.input.cursor = 0;
                            }
                            if !input.is_empty() {
                                self.state.input.buffer.clear();
                                self.state.input.cursor = 0;
                                self.state.input.selection_anchor = None;
                                self.state.autocomplete = AutocompleteState::default();
                                self.state.input.history_index = None;
                                crate::session::append_history(&input);

                                // --- GOAL mode: intercept non-command input ---
                                if self.state.goal.mode && !input.starts_with('/') {
                                    match self.state.goal.stage {
                                        GoalStage::Inactive => {
                                            // First input in goal mode = the prompt
                                            self.state.goal.prompt = input.clone();
                                            self.state.goal.stage = GoalStage::WaitForGoal;
                                            self.state.messages.push(ChatMessage::user(&input));
                                            self.state.messages.push(ChatMessage::system(
                                                "🎯 Goal mode: now define your GOAL (what must be achieved)",
                                            ));
                                            return Ok(false);
                                        }
                                        GoalStage::WaitForGoal => {
                                            // Second input = the goal
                                            self.state.goal.text = input.clone();
                                            self.state.messages.push(ChatMessage::user(&format!(
                                                "GOAL: {}", input
                                            )));

                                            // Without a provider the worker can't run; advancing
                                            // the stage would wedge the cycle, so bail cleanly.
                                            if self.provider.is_none() {
                                                self.cancel_goal_cycle(
                                                    "No provider configured — cannot run the goal. Set your DeepSeek token, then /goal to retry.",
                                                );
                                                return Ok(false);
                                            }

                                            self.state.goal.stage = GoalStage::RunAgent1;
                                            self.state.goal.iteration = 1;

                                            // Build the agent 1 prompt: user's prompt + goal
                                            let agent1_prompt = format!(
                                                "{}\n\nIMPORTANT - GOAL to achieve: {}",
                                                self.state.goal.prompt, self.state.goal.text
                                            );
                                            self.send_to_agent(agent1_prompt).await?;
                                            return Ok(false);
                                        }
                                        GoalStage::RunAgent1 | GoalStage::RunEvaluator => {
                                            // Block input while goal cycle is active
                                            self.state.messages.push(ChatMessage::system(
                                                "Goal cycle in progress. Wait for it to finish or type /goal to cancel.",
                                            ));
                                            return Ok(false);
                                        }
                                        GoalStage::Done => {
                                            // After goal is done, regular input resumes
                                            self.state.goal.mode = false;
                                            self.state.goal.stage = GoalStage::Inactive;
                                        }
                                    }
                                }

                                if input.starts_with('/') {
                            let result =
                                self.commands.execute(&input, &mut self.state, &self.config);
                            match result {
                                CommandResult::Handled => {}
                                CommandResult::NeedsAgent(msg) => {
                                    let killed = self.cleanup_background_before_user_turn().await;
                                    if killed > 0 {
                                        self.state.messages.push(ChatMessage::system(&format!(
                                            "Cleaned {killed} ephemeral job(s) before the new turn."
                                        )));
                                    }
                                    self.state.messages.push(ChatMessage::user(&input));
                                    self.send_to_agent(msg).await?;
                                }
                                CommandResult::LoadSession(id) => {
                                    self.handle_load_session(&id).await?;
                                }
                                CommandResult::Quit => {
                                    let _ = self.shutdown_background_processes().await;
                                    return Ok(true);
                                }
                                CommandResult::ResetProvider => {
                                    if let Ok(config) = crate::config::load() {
                                        self.config = config;
                                    }
                                    self.provider = if self.config.provider.token.is_empty() {
                                        None
                                    } else {
                                        crate::provider::deepseek::DeepseekProvider::new(
                                            &self.config.provider,
                                            self.config.agent.rate_limit_ms,
                                            self.config.agent.max_retries,
                                        ).ok().map(|ds| Arc::new(ds) as Arc<dyn LLMProvider>)
                                    };
                                    self.state.current_session_id =
                                        crate::session::create_session_id();
                                }
                                CommandResult::TtlUpdate(ttl) => {
                                    self.config.mcp.cache_ttl = ttl;
                                    {
                                        let mut mcp = self.mcp.lock().await;
                                        mcp.set_cache_ttl(ttl);
                                    }
                                    self.state.messages.push(ChatMessage::system(
                                        &format!("MCP cache TTL set to {ttl}s"),
                                    ));
                                }
                                CommandResult::ReloadMcp => {
                                    self.state.messages.push(ChatMessage::system(
                                        "Reloading all MCP servers...",
                                    ));
                                    let mut mcp = self.mcp.lock().await;
                                    mcp.reload_all().await;
                                    self.state.mcp_status.view.servers = mcp.get_servers_info();
                                    self.state.messages.push(ChatMessage::system(
                                        "MCP servers reloaded",
                                    ));
                                }
                                CommandResult::ShowTools => {
                                    let tools_text = self.build_tools_display().await;
                                    self.state.messages.push(ChatMessage::system(&tools_text));
                                }
                                CommandResult::Jobs(action) => {
                                    let jobs_text = match action {
                                        crate::commands::JobCommandAction::List => {
                                            self.build_background_processes_display().await
                                        }
                                        crate::commands::JobCommandAction::Kill(id) => {
                                            self.kill_background_job(id).await
                                        }
                                        crate::commands::JobCommandAction::Prune => {
                                            self.prune_background_jobs().await
                                        }
                                    };
                                    self.state.messages.push(ChatMessage::system(&jobs_text));
                                }
                                CommandResult::ShowSkills => {
                                    self.open_skill_picker().await;
                                }
                                CommandResult::ToggleSkill(name, enable) => {
                                    self.toggle_skill(&name, enable).await;
                                }
                                CommandResult::OpenWhitelist => {
                                    self.open_whitelist_picker().await;
                                }
                                CommandResult::Sidechat(question) => {
                                    self.spawn_sidechat(question).await?;
                                }
                                CommandResult::NewChat => {
                                    self.new_conversation();
                                }
                                CommandResult::OpenChats => {
                                    self.open_chats_picker().await;
                                }
                                CommandResult::Error(err) => {
                                    self.state.messages.push(ChatMessage::system(&err));
                                }
                            }
                        } else {
                            let killed = self.cleanup_background_before_user_turn().await;
                            if killed > 0 {
                                self.state.messages.push(ChatMessage::system(&format!(
                                    "Cleaned {killed} ephemeral job(s) before the new turn."
                                )));
                            }
                            let mut expanded = self.expand_file_mentions(&input);
                            if !self.state.attached_files.is_empty() {
                                let attach_header = if expanded.trim().is_empty() {
                                    String::new()
                                } else {
                                    format!("\n\n")
                                };
                                let attach_section: String = self.state.attached_files
                                    .iter()
                                    .filter_map(|f| {
                                        let content = std::fs::read_to_string(&f.path).ok()?;
                                        let header = format!("File: {} ({}):", f.display_name, crate::app::format_size(f.size));
                                        Some(format!("```\n{}\n{}\n```", header, content))
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                if !attach_section.is_empty() {
                                    expanded.push_str(&attach_header);
                                    expanded.push_str(&attach_section);
                                }
                                self.state.attached_files.clear();
                            }
                            self.state.messages.push(ChatMessage::user(&expanded));
                            self.send_to_agent(expanded).await?;
                        }
                    }
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.input.insert_char(c);
            }
            KeyCode::Backspace => {
                self.state.input.backspace();
            }
            KeyCode::Delete => {
                self.state.input.delete_forward();
            }
            KeyCode::Left => {
                let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                self.state.input.move_left(shift, ctrl);
            }
            KeyCode::Right => {
                let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                self.state.input.move_right(shift, ctrl);
            }
            KeyCode::Home => {
                self.state.input.move_home(key.modifiers.contains(KeyModifiers::SHIFT));
            }
            KeyCode::End => {
                self.state.input.move_end(key.modifiers.contains(KeyModifiers::SHIFT));
            }
            KeyCode::Up if !self.state.generation.active
                && self.state.input.buffer.chars().take(self.state.input.cursor).filter(|&c| c == '\n').count() == 0 =>
            {
                self.state.input.history_prev();
            }
            KeyCode::Down if !self.state.generation.active
                && self.state.input.buffer.chars().skip(self.state.input.cursor).filter(|&c| c == '\n').count() == 0 =>
            {
                self.state.input.history_next();
            }
            KeyCode::Up => {
                self.state.scroll_offset = self.state.scroll_offset.saturating_add(1);
            }
            KeyCode::Down => {
                self.state.scroll_offset = self.state.scroll_offset.saturating_sub(1);
            }
            KeyCode::PageUp => {
                self.state.scroll_offset = self.state.scroll_offset.saturating_add(10);
            }
            KeyCode::PageDown => {
                self.state.scroll_offset = self.state.scroll_offset.saturating_sub(10);
            }
            _ => {}
        }
        self.refresh_autocomplete();
        Ok(false)
    }

    async fn handle_mcp_key(&mut self, key: crossterm::event::KeyEvent) -> AppResult<bool> {
        use crossterm::event::KeyCode;

        if self.state.mcp_status.view.servers.is_empty() {
            let mcp = self.mcp.lock().await;
            self.state.mcp_status.view.servers = mcp.get_servers_info();
        }

        let details_open = self.state.mcp_status.view.details_server.is_some();

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.state.mcp_status.view.active = false;
                self.state.mcp_status.view.details_server = None;
                self.state.view = View::Chat;
            }
            KeyCode::Up | KeyCode::Char('k') if !details_open => {
                self.state.mcp_status.view.selected = self.state.mcp_status.view.selected.saturating_sub(1);
                self.clamp_mcp_scroll();
            }
            KeyCode::Down | KeyCode::Char('j') if !details_open => {
                let max = self.state.mcp_status.view.servers.len().saturating_sub(1);
                self.state.mcp_status.view.selected = self.state.mcp_status.view.selected.min(max);
                self.state.mcp_status.view.selected += 1;
                self.state.mcp_status.view.selected = self.state.mcp_status.view.selected.min(max);
                self.clamp_mcp_scroll();
            }
            KeyCode::Enter => {
                if details_open {
                    self.state.mcp_status.view.details_server = None;
                } else if let Some(info) = self.state.mcp_status.view.servers.get(self.state.mcp_status.view.selected) {
                    self.state.mcp_status.view.details_server = Some(info.name.clone());
                    self.state.mcp_status.view.scroll_offset = 0;
                }
            }
            KeyCode::Char(' ') if !details_open => {
                if let Some(info) = self.state.mcp_status.view.servers.get(self.state.mcp_status.view.selected).cloned() {
                    let name = info.name.clone();
                    let mut mcp = self.mcp.lock().await;
                    if let Err(e) = mcp.toggle_server(&name).await {
                        self.state.mcp_status.view.status_message = format!("Toggle failed: {e}");
                    } else {
                        self.state.mcp_status.view.status_message = format!(
                            "{} {}",
                            name,
                            if info.enabled { "disabled" } else { "enabled" },
                        );
                    }
                    self.state.mcp_status.view.servers = mcp.get_servers_info();
                }
            }
            KeyCode::Char('r') if !details_open => {
                if let Some(info) = self.state.mcp_status.view.servers.get(self.state.mcp_status.view.selected).cloned() {
                    let name = info.name.clone();
                    self.state.mcp_status.view.status_message = format!("Reconnecting {name}...");
                    let mut mcp = self.mcp.lock().await;
                    if let Err(e) = mcp.reconnect_server(&name).await {
                        self.state.mcp_status.view.status_message = format!("Reconnect failed: {e}");
                    } else {
                        self.state.mcp_status.view.status_message = format!("{name} reconnected");
                    }
                    self.state.mcp_status.view.servers = mcp.get_servers_info();
                }
            }
            KeyCode::Char('d') if !details_open => {
                if let Some(info) = self.state.mcp_status.view.servers.get(self.state.mcp_status.view.selected).cloned() {
                    let name = info.name.clone();
                    let mut mcp = self.mcp.lock().await;
                    if let Err(e) = mcp.remove_server(&name).await {
                        self.state.mcp_status.view.status_message = format!("Remove failed: {e}");
                    } else {
                        self.state.mcp_status.view.status_message = format!("{name} removed");
                    }
                    self.state.mcp_status.view.servers = mcp.get_servers_info();
                    self.state.mcp_status.view.selected = self.state.mcp_status.view.selected.min(
                        self.state.mcp_status.view.servers.len().saturating_sub(1),
                    );
                }
            }
            KeyCode::Up | KeyCode::Char('k') if details_open => {
                self.state.mcp_status.view.scroll_offset = self.state.mcp_status.view.scroll_offset.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') if details_open => {
                self.state.mcp_status.view.scroll_offset += 1;
            }
            _ => {}
        }
        Ok(false)
    }

    fn clamp_mcp_scroll(&self) {
        // no-op for now, we manage scroll in the renderer
    }

    fn refresh_autocomplete(&mut self) {
        let buf = self.state.input.buffer.clone();

        // Check for @-triggered file path completion
        if let Some(at_pos) = buf.rfind('@') {
            let after_at = &buf[at_pos + 1..];
            let path_part = after_at.split_whitespace().next().unwrap_or("");
            if !path_part.is_empty() && !self.state.generation.active && self.state.modal.is_none() {
                let cwd = std::env::current_dir().unwrap_or_default();
                let search_path = if path_part.contains('/') || path_part.contains('\\') {
                    std::path::Path::new(path_part).to_path_buf()
                } else {
                    cwd.join(path_part)
                };
                let parent = search_path.parent().unwrap_or(&cwd);
                let prefix = search_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_lowercase();

                let mut items = Vec::new();
                if let Ok(read_dir) = std::fs::read_dir(
                    if path_part.contains('/') || path_part.contains('\\') {
                        if parent.is_absolute() {
                            parent.to_path_buf()
                        } else {
                            cwd.join(parent)
                        }
                    } else {
                        cwd.clone()
                    },
                ) {
                    for entry in read_dir.flatten() {
                        let name = entry.file_name().to_string_lossy().to_string();
                        if name.to_lowercase().starts_with(&prefix) {
                            let full_path = entry.path();
                            let display = full_path.to_string_lossy().to_string();
                            let is_dir = full_path.is_dir();
                            let suffix = if is_dir { "/" } else { "" };
                            let desc = if is_dir {
                                "dir".to_string()
                            } else {
                                let meta = full_path.metadata().ok();
                                let size = meta.map(|m| m.len()).unwrap_or(0);
                                format_size(size)
                            };
                            items.push(CommandSuggestion {
                                name: format!("{}{}", name, suffix),
                                description: desc,
                                usage: display.clone(),
                            });
                        }
                    }
                }
                items.sort_by(|a, b| {
                    let a_dir = a.usage.ends_with('/');
                    let b_dir = b.usage.ends_with('/');
                    a_dir.cmp(&b_dir).then(a.name.cmp(&b.name))
                });
                self.state.autocomplete.items = items;
                self.state.autocomplete.visible = !self.state.autocomplete.items.is_empty();
                self.state.autocomplete.selected = 0;
                self.state.autocomplete.scroll_offset = 0;
                self.state.autocomplete.file_mode = true;
                return;
            }
        }

        // Command autocomplete
        let query_main = buf
            .strip_prefix('/')
            .map(|rest| rest.split_whitespace().next().unwrap_or(""))
            .unwrap_or("");
        let active = buf.starts_with('/')
            && !self.state.generation.active
            && self.state.modal.is_none()
            && !buf[1..].contains(char::is_whitespace);
        if !active {
            self.state.autocomplete.visible = false;
            self.state.autocomplete.items.clear();
            self.state.autocomplete.selected = 0;
            return;
        }
        let items = self.commands.suggest(query_main);
        self.state.autocomplete.items = items;
        self.state.autocomplete.visible = !self.state.autocomplete.items.is_empty();
        self.state.autocomplete.selected = 0;
        self.state.autocomplete.scroll_offset = 0;
        self.state.autocomplete.file_mode = false;
    }

    fn clamp_autocomplete_scroll(&mut self) {
        let n = self.state.autocomplete.items.len();
        if n <= AUTOCOMPLETE_VISIBLE {
            return;
        }
        let sel = self.state.autocomplete.selected;
        let off = &mut self.state.autocomplete.scroll_offset;
        if sel < *off {
            *off = sel;
        } else if sel >= *off + AUTOCOMPLETE_VISIBLE {
            *off = sel + 1 - AUTOCOMPLETE_VISIBLE;
        }
    }

    fn accept_autocomplete(&mut self) {
        if !self.state.autocomplete.visible || self.state.autocomplete.items.is_empty() {
            return;
        }
        let idx = self
            .state
            .autocomplete
            .selected
            .min(self.state.autocomplete.items.len() - 1);
        let suggestion = &self.state.autocomplete.items[idx];

        if self.state.autocomplete.file_mode {
            let path = std::path::Path::new(&suggestion.usage);
            if path.is_dir() {
                let at_pos = self.state.input.buffer.rfind('@').unwrap_or(0);
                let before_at = self.state.input.buffer[..at_pos].to_string();
                let new_buf = format!("{}@{}/", before_at, suggestion.name.trim_end_matches('/'));
                self.state.input.buffer = new_buf;
                self.state.input.cursor = self.state.input.buffer.chars().count();
                self.state.input.selection_anchor = None;
                self.state.autocomplete = AutocompleteState::default();
            } else {
                let resolved = if path.is_relative() {
                    let cwd = std::env::current_dir().unwrap_or_default();
                    cwd.join(path)
                } else {
                    path.to_path_buf()
                };
                if resolved.is_file() {
                    let display_name = resolved
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("file")
                        .to_string();
                    let meta = resolved.metadata().ok();
                    let ext = resolved
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_lowercase();
                    let is_image =
                        matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg");
                    self.state
                        .attached_files
                        .push(crate::provider::AttachedFile {
                            display_name,
                            path: resolved.to_string_lossy().to_string(),
                            size: meta.map(|m| m.len()).unwrap_or(0),
                            is_image,
                        });
                }
                let at_pos = self.state.input.buffer.rfind('@').unwrap_or(0);
                let after_at = &self.state.input.buffer[at_pos + 1..];
                let path_text = after_at.split_whitespace().next().unwrap_or("");
                let before = self.state.input.buffer[..at_pos].to_string();
                let after = self.state.input.buffer[at_pos + 1 + path_text.len()..].to_string();
                let new_buf = format!("{}{}", before, after);
                self.state.input.buffer = new_buf;
                self.state.input.cursor = self.state.input.buffer.chars().count();
                self.state.input.selection_anchor = None;
                self.state.autocomplete = AutocompleteState::default();
                self.state.status_message =
                    format!("{} files attached", self.state.attached_files.len());
            }
        } else {
            let new_buf = format!("/{} ", suggestion.name);
            self.state.input.buffer = new_buf;
            self.state.input.cursor = self.state.input.buffer.chars().count();
            self.state.input.selection_anchor = None;
            self.state.autocomplete = AutocompleteState::default();
        }
    }


    async fn build_system_prompt(&self) -> String {
        let workspace = &self.state.workspace_path;
        let user = std::env::var("USERNAME")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "user".to_string());
        let os = std::env::consts::OS.to_string();

        let builtin_tools = self.tools.definitions();
        let builtin_section = if builtin_tools.is_empty() {
            "- none".to_string()
        } else {
            builtin_tools
                .iter()
                .map(|tool| format_tool_definition(&tool.name, &tool.description, &tool.parameters))
                .collect::<Vec<_>>()
                .join("\n\n")
        };

        let mcp = self.mcp.lock().await;
        let all_mcp_tools = mcp.get_all_tools();
        let all_mcp_resources = mcp.get_all_resources();
        let mcp_tool_section = if all_mcp_tools.is_empty() {
            "- none".to_string()
        } else {
            all_mcp_tools
                .iter()
                .map(|full| {
                    format_tool_definition(
                        &full.full_name,
                        &full.tool.description,
                        &full.tool.input_schema,
                    )
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        };
        let mcp_resource_section = if all_mcp_resources.is_empty() {
            String::new()
        } else {
            let mut lines = vec!["\n\n### Available MCP resources:".to_string()];
            for r in &all_mcp_resources {
                let desc = r.description.as_deref().unwrap_or("");
                let name = if r.name.is_empty() { &r.uri } else { &r.name };
                lines.push(format!("- `{}`: {} ({})", name, desc, r.uri));
            }
            lines.join("\n")
        };
        let mcp_section = format!("{mcp_tool_section}{mcp_resource_section}");
        drop(mcp);

        let base_prompt = self
            .prompts
            .base_prompt
            .replace("{{user}}", &user)
            .replace("{{folder}}", &workspace)
            .replace("{{os}}", &os);
        let tools_prompt = self
            .prompts
            .tools_prompt
            .replace("{{builtin_tools}}", &builtin_section)
            .replace("{{mcp_tools}}", &mcp_section);

        let skills_section = crate::skills::discovery::load_enabled_skills_content(&self.skills);

        format!(
            "{}\n\n{}{}",
            base_prompt.trim(),
            tools_prompt.trim(),
            skills_section
        )
    }

    async fn send_to_agent(&mut self, _input: String) -> AppResult<()> {
        let provider = match &self.provider {
            Some(p) => Arc::clone(p),
            None => {
                self.state.messages.push(ChatMessage::assistant(
                    "No provider configured. Set your DeepSeek token in config.",
                ));
                return Ok(());
            }
        };

        let event_tx = self.event_tx.clone();
        let messages: Vec<ChatMessage> = self.state.messages.clone();
        let system_prompt = self.build_system_prompt().await;

        let model = self.config.provider.model.clone();
        let temperature = self.config.provider.temperature;
        let max_tokens = self.config.provider.max_tokens;
        let max_steps = self.config.agent.max_steps_per_turn.max(1);
        let max_tools_per_step = self.config.agent.max_tools_per_step.max(1);
        let tools = Arc::clone(&self.tools);
        let mcp = Arc::clone(&self.mcp);
        let conversation = self.state.focused_id;

        let _ = event_tx.send(AppEvent::AgentStarted(conversation));

        let handle = tokio::spawn(crate::agent::runner::run_agent_loop(
            conversation,
            provider,
            tools,
            mcp,
            messages,
            system_prompt,
            model,
            temperature,
            max_tokens,
            max_steps,
            max_tools_per_step,
            false, // focused turn: interactive approval
            event_tx,
        ));

        self.agent_task = Some(handle);

        Ok(())
    }

    /// Spawn a one-shot `/btw` sidechat: an isolated background conversation
    /// (its own forked session, auto-approved tools) whose answer is appended
    /// to the chat when it finishes. Streams independently of the main turn.
    async fn spawn_sidechat(&mut self, question: String) -> AppResult<()> {
        let provider = match &self.provider {
            Some(p) => p.fork(),
            None => {
                self.state.messages.push(ChatMessage::system(
                    "No provider configured. Set your DeepSeek token in config.",
                ));
                return Ok(());
            }
        };

        let id = conversation::ConversationId::next();
        let sidechat_messages = vec![ChatMessage::user(&question)];
        let system_prompt = self.build_system_prompt().await;
        let model = self.config.provider.model.clone();
        let temperature = self.config.provider.temperature;
        let max_tokens = self.config.provider.max_tokens;
        // Keep sidechats lightweight.
        let max_steps = self.config.agent.max_steps_per_turn.clamp(1, 6);
        let max_tools_per_step = self.config.agent.max_tools_per_step.max(1);
        let tools = Arc::clone(&self.tools);
        let mcp = Arc::clone(&self.mcp);
        let event_tx = self.event_tx.clone();

        let mut generation = generation::GenerationState::default();
        generation.begin(std::time::Instant::now());

        let handle = tokio::spawn(crate::agent::runner::run_agent_loop(
            id,
            Arc::clone(&provider),
            tools,
            mcp,
            sidechat_messages.clone(),
            system_prompt,
            model,
            temperature,
            max_tokens,
            max_steps,
            max_tools_per_step,
            true, // background: auto-approve, never block on a modal
            event_tx,
        ));

        self.state.background.push(conversation::Conversation {
            id,
            kind: conversation::ConversationKind::Sidechat,
            title: question.clone(),
            session_id: crate::session::create_session_id(),
            session_started_at: chrono::Utc::now().to_rfc3339(),
            messages: sidechat_messages,
            provider: Some(provider),
            generation,
            agent_task: Some(handle),
        });

        self.state.messages.push(ChatMessage::system(&format!(
            "◈ btw started — {question}"
        )));
        Ok(())
    }

    /// Apply an agent event that targets a background conversation (not the
    /// focused one). Streaming events mutate the parked conversation; terminal
    /// events finalize it (sidechats flush their answer into the chat).
    fn handle_background_event(&mut self, target: conversation::ConversationId, event: AppEvent) {
        enum Finish {
            None,
            Done,
            Error(String),
        }
        let mut finish = Finish::None;

        if let Some(conv) = self.state.background.iter_mut().find(|c| c.id == target) {
            match event {
                AppEvent::AgentStarted(_) => {
                    conv.generation.begin(std::time::Instant::now());
                }
                AppEvent::BeginAssistantMessage(_) => {
                    let needs_push = conv
                        .messages
                        .last()
                        .is_none_or(|m| m.role != Role::Assistant || !m.content.is_empty());
                    if needs_push {
                        conv.messages.push(ChatMessage::assistant(""));
                    }
                }
                AppEvent::AgentChunk(_, chunk) => {
                    if let Some(last) = conv.messages.last_mut() {
                        if last.role == Role::Assistant {
                            last.content.push_str(&chunk);
                        }
                    }
                }
                AppEvent::AddMessage(_, message) => conv.messages.push(message),
                AppEvent::DiscardEmptyAssistantMessage(_) => {
                    if conv
                        .messages
                        .last()
                        .is_some_and(|m| m.role == Role::Assistant && m.content.is_empty())
                    {
                        conv.messages.pop();
                    }
                }
                AppEvent::AgentDone(_, _) => {
                    conv.generation.active = false;
                    conv.agent_task = None;
                    // A sidechat flushes its answer and disappears; a parked
                    // session just stops streaming and waits to be switched to.
                    if conv.kind == conversation::ConversationKind::Sidechat {
                        finish = Finish::Done;
                    }
                }
                AppEvent::AgentError(_, err) => {
                    conv.generation.active = false;
                    conv.agent_task = None;
                    if conv.kind == conversation::ConversationKind::Sidechat {
                        finish = Finish::Error(err);
                    }
                }
                // Tool status from background turns is not surfaced live.
                _ => {}
            }
        }

        match finish {
            Finish::Done => self.finish_sidechat(target, None),
            Finish::Error(err) => self.finish_sidechat(target, Some(err)),
            Finish::None => {}
        }
    }

    /// Remove a finished background conversation and flush its result (answer or
    /// error) into the focused chat as a `◈ btw` block.
    fn finish_sidechat(&mut self, target: conversation::ConversationId, error: Option<String>) {
        let Some(pos) = self.state.background.iter().position(|c| c.id == target) else {
            return;
        };
        let conv = self.state.background.remove(pos);
        let block = if let Some(err) = error {
            format!("◈ btw — {}\n\n⚠ {err}", conv.title)
        } else {
            let answer = conv
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Assistant)
                .map(|m| m.content.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "(no answer)".to_string());
            format!("◈ btw — {}\n\n{answer}", conv.title)
        };
        self.state.messages.push(ChatMessage::system(&block));
        self.auto_save_session();
    }

    /// Pack the focused conversation's live state into a parked record.
    fn park_focused(&mut self) -> conversation::Conversation {
        conversation::Conversation {
            id: self.state.focused_id,
            kind: self.state.focused_kind,
            title: conversation_title(&self.state.messages),
            session_id: std::mem::take(&mut self.state.current_session_id),
            session_started_at: std::mem::take(&mut self.state.session_started_at),
            messages: std::mem::take(&mut self.state.messages),
            provider: self.provider.take(),
            generation: std::mem::take(&mut self.state.generation),
            agent_task: self.agent_task.take(),
        }
    }

    /// Make a parked conversation the focused/live one (it keeps its running
    /// task — switching never drops a stream).
    fn activate(&mut self, conv: conversation::Conversation) {
        self.state.focused_id = conv.id;
        self.state.focused_kind = conv.kind;
        self.state.current_session_id = conv.session_id;
        self.state.session_started_at = conv.session_started_at;
        self.state.messages = conv.messages;
        self.provider = conv.provider;
        self.state.generation = conv.generation;
        self.agent_task = conv.agent_task;
        self.state.scroll_offset = 0;
        self.state.status_message = if self.state.generation.active {
            "Thinking...".to_string()
        } else {
            "Ready".to_string()
        };
    }

    /// Open a fresh parallel session and focus it; the current one keeps
    /// running in the background.
    fn new_conversation(&mut self) {
        if self.state.modal.is_some() {
            return;
        }
        let new_provider = self.provider.as_ref().map(|p| p.fork());
        let parked = self.park_focused();
        self.state.background.push(parked);

        self.state.focused_id = conversation::ConversationId::next();
        self.state.focused_kind = conversation::ConversationKind::Session;
        self.state.current_session_id = crate::session::create_session_id();
        self.state.session_started_at = chrono::Utc::now().to_rfc3339();
        self.state.messages = Vec::new();
        self.provider = new_provider;
        self.state.generation = generation::GenerationState::default();
        self.agent_task = None;
        self.state.scroll_offset = 0;
        self.state.status_message = if self.provider.is_some() {
            "Ready".to_string()
        } else {
            "No provider".to_string()
        };
        self.state.messages.push(ChatMessage::system(
            "New chat opened. /chats or Tab to switch between chats.",
        ));
    }

    /// Switch focus to a background conversation by id, parking the current one.
    fn switch_to(&mut self, target: conversation::ConversationId) {
        if target == self.state.focused_id || self.state.modal.is_some() {
            return;
        }
        let Some(pos) = self.state.background.iter().position(|c| c.id == target) else {
            return;
        };
        let parked = self.park_focused();
        let incoming = self.state.background.remove(pos);
        self.state.background.push(parked);
        self.activate(incoming);
    }

    /// Cycle focus to the next (`+1`) / previous (`-1`) conversation by id order.
    fn cycle_focus(&mut self, dir: i64) {
        if self.state.modal.is_some() || self.state.background.is_empty() {
            return;
        }
        let mut ids: Vec<conversation::ConversationId> =
            self.state.background.iter().map(|c| c.id).collect();
        ids.push(self.state.focused_id);
        ids.sort_by_key(|c| c.0);
        let cur = ids
            .iter()
            .position(|&i| i == self.state.focused_id)
            .unwrap_or(0);
        let len = ids.len() as i64;
        let next = ids[(((cur as i64 + dir) % len + len) % len) as usize];
        self.switch_to(next);
    }

    /// Open the `/chats` picker listing the focused conversation + background
    /// ones, with a streaming marker; selecting one switches focus.
    async fn open_chats_picker(&mut self) {
        use crate::app::events::{Modal, PickerItem, PickerKind, PickerMode, PickerState};

        let mut items: Vec<(conversation::ConversationId, PickerItem)> = Vec::new();
        let focused_label = format!(
            "● {}{}",
            conversation_title(&self.state.messages),
            if self.state.generation.active { "  [streaming]" } else { "" }
        );
        items.push((
            self.state.focused_id,
            PickerItem::new(focused_label, self.state.focused_id.0.to_string()),
        ));
        for conv in &self.state.background {
            let label = format!(
                "  {}{}",
                if conv.title.is_empty() { "(chat)".to_string() } else { conv.title.clone() },
                if conv.is_streaming() { "  [streaming]" } else { "" }
            );
            items.push((conv.id, PickerItem::new(label, conv.id.0.to_string())));
        }
        items.sort_by_key(|(id, _)| id.0);

        let picker = PickerState::new_with_kind(
            "Chats — Enter to switch",
            items.into_iter().map(|(_, item)| item).collect(),
            PickerMode::Single,
            PickerKind::Chats,
        );
        self.state.modal = Some(Modal::Picker(picker));
    }

    /// Abort any in-flight goal cycle and leave goal mode fully reset, so the
    /// user is never stuck behind a "cycle in progress" that isn't running.
    /// No-op when goal mode is off.
    fn cancel_goal_cycle(&mut self, reason: &str) {
        if !self.state.goal.mode {
            return;
        }
        self.state.messages.push(ChatMessage::system(reason));
        self.state.goal.deactivate();
    }

    async fn run_goal_evaluation(&mut self, agent_result: String) {
        let provider = match &self.provider {
            Some(p) => Arc::clone(p),
            None => {
                self.cancel_goal_cycle(
                    "No provider configured for evaluation — goal cancelled. Set your token, then /goal to retry.",
                );
                return;
            }
        };

        let system_prompt = self.prompts
            .goal_evaluator_prompt
            .clone();

        let user_msg = format!(
            "## Goal\n{}\n\n## Completed Work\n{}\n\n## Evaluation",
            self.state.goal.text, agent_result
        );

        let eval_messages = vec![
            ChatMessage::system(&system_prompt),
            ChatMessage::user(&user_msg),
        ];

        let request = crate::provider::CompletionRequest {
            messages: eval_messages.clone(),
            model: self.config.provider.model.clone(),
            temperature: self.config.provider.temperature.min(0.5),
            max_tokens: self.config.provider.max_tokens,
            stream: false,
        };

        match provider.complete(request).await {
            Ok(response) => {
                let verdict = goal::parse_goal_verdict(&response.content);
                let save_eval_session = |this: &Self| {
                    let _ = crate::session::save_session_with_tag(
                        &this.state.goal.agent2_session_id,
                        &this.state.session_started_at,
                        &eval_messages,
                        &this.config,
                        &this.state.workspace_path,
                        Some("__goal_system__".to_string()),
                    );
                };

                match self.state.goal.apply_verdict(verdict) {
                    goal::GoalOutcome::Succeeded { summary } => {
                        self.state.messages.push(ChatMessage::system(
                            &format!("✅ Goal achieved!\n\n{}", summary)
                        ));
                        save_eval_session(self);
                    }
                    goal::GoalOutcome::GaveUp { iteration } => {
                        save_eval_session(self);
                        self.cancel_goal_cycle(&format!(
                            "🛑 Goal not achieved after {iteration} attempts — stopping. Use /goal to try a different approach."
                        ));
                    }
                    goal::GoalOutcome::Retry { iteration, issues, feedback, swapped_agent1, swapped_agent2 } => {
                        if swapped_agent2 {
                            self.state.messages.push(ChatMessage::system(
                                "🔄 Swapping evaluator (agent 2) to new session after 5 failures."
                            ));
                        }
                        // Save under the (possibly swapped) evaluator session id.
                        save_eval_session(self);

                        self.state.messages.push(ChatMessage::system(
                            &format!(
                                "❌ Goal not achieved (attempt {}).\nIssues: {}\nFeedback: {}",
                                iteration, issues, feedback
                            )
                        ));
                        if swapped_agent1 {
                            self.state.messages.push(ChatMessage::system(
                                "🔄 Swapping agent to new session after 3 failures."
                            ));
                        }
                        self.retry_agent1_with_feedback(&feedback).await;
                    }
                }
            }
            Err(e) => {
                self.state.messages.push(ChatMessage::system(
                    &format!("⚠ Evaluation failed: {e}. Retrying agent 1...")
                ));
                self.state.goal.stage = GoalStage::RunAgent1;
                self.retry_agent1().await;
            }
        }
    }

    async fn handle_goal_verdict(&mut self) {
        // Find the evaluator's result from the last assistant message
        let eval_result = self.state.messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .map(|m| m.content.clone())
            .unwrap_or_default();

        if eval_result.is_empty() {
            self.state.messages.push(ChatMessage::system(
                "⚠ Evaluator produced no output. Retrying evaluation..."
            ));
            return;
        }

        // Re-run evaluation via the event-based path
        let verdict = goal::parse_goal_verdict(&eval_result);
        let event = AppEvent::GoalEvaluationDone(verdict);
        let _ = self.event_tx.send(event);
    }

    async fn handle_goal_verdict_from(&mut self, verdict: GoalVerdict) {
        match self.state.goal.apply_verdict(verdict) {
            goal::GoalOutcome::Succeeded { summary } => {
                self.state.messages.push(ChatMessage::system(
                    &format!("✅ Goal achieved!\n\n{}", summary)
                ));
            }
            goal::GoalOutcome::GaveUp { iteration } => {
                self.cancel_goal_cycle(&format!(
                    "🛑 Goal not achieved after {iteration} attempts — stopping. Use /goal to try a different approach."
                ));
            }
            goal::GoalOutcome::Retry { iteration, issues, feedback, swapped_agent1, swapped_agent2 } => {
                if swapped_agent2 {
                    self.state.messages.push(ChatMessage::system(
                        "🔄 Swapping evaluator (agent 2) to new session after 5 failures."
                    ));
                }
                self.state.messages.push(ChatMessage::system(
                    &format!(
                        "❌ Goal not achieved (attempt {}).\nIssues: {}\nFeedback: {}",
                        iteration, issues, feedback
                    )
                ));
                if swapped_agent1 {
                    self.state.messages.push(ChatMessage::system(
                        "🔄 Swapping agent to new session after 3 failures."
                    ));
                }
                self.retry_agent1_with_feedback(&feedback).await;
            }
        }
    }

    async fn retry_agent1(&mut self) {
        let prompt = format!(
            "Continue working on the original request.\n\nOriginal prompt: {}\n\nGoal: {}",
            self.state.goal.prompt, self.state.goal.text
        );
        self.state.messages.push(ChatMessage::user(&format!(
            "[Continuing goal cycle - attempt {}]",
            self.state.goal.iteration
        )));
        self.send_to_agent(prompt).await.unwrap_or_else(|e| {
            self.state.messages.push(ChatMessage::system(
                &format!("⚠ Failed to retry agent: {e}")
            ));
        });
    }

    async fn retry_agent1_with_feedback(&mut self, feedback: &str) {
        let prompt = format!(
            "The reviewer found issues with the previous attempt. Please fix them.\n\n\
            Original request: {}\n\nGoal: {}\n\n\
            Issues to fix:\n{}\n\n\
            Previous conversation history is above. Focus on fixing the issues.",
            self.state.goal.prompt, self.state.goal.text, feedback
        );
        self.send_to_agent(prompt).await.unwrap_or_else(|e| {
            self.state.messages.push(ChatMessage::system(
                &format!("⚠ Failed to retry agent: {e}")
            ));
        });
    }

    async fn handle_load_session(&mut self, session_id: &str) -> AppResult<()> {
        use crate::session;

        match session::load_local(session_id, &self.config) {
            Ok(s) => {
                self.state.messages = s.messages;
                self.state.attached_files.clear();
                self.state.current_session_id = s.id;
                self.state.scroll_offset = 0;
                self.state.input.buffer.clear();
                self.state.input.cursor = 0;
                self.state.input.selection_anchor = None;
                self.state.autocomplete = Default::default();
                self.state.generation.active = false;
                self.state.error = None;
                if let Some(handle) = self.agent_task.take() {
                    handle.abort();
                }

                if let Some(provider) = &self.provider {
                    let _ = provider.reset().await;
                }

                let count = self.state.messages.len();
                self.state.status_message = format!(
                    "Loaded local session {session_id} ({count} messages)"
                );
            }
            Err(_) => {
                if let Some(provider) = &self.provider {
                    match provider.fetch_remote_session_messages(session_id).await {
                        Ok(messages) => {
                            if messages.is_empty() {
                                self.state.messages.push(ChatMessage::system(
                                    &format!("Remote session {session_id} has no messages"),
                                ));
                                return Ok(());
                            }
                            self.state.messages = messages;
                            self.state.current_session_id = session_id.to_string();
                            self.state.scroll_offset = 0;
                            self.state.input.buffer.clear();
                            self.state.input.cursor = 0;
                            self.state.input.selection_anchor = None;
                            self.state.autocomplete = Default::default();
                            self.state.generation.active = false;
                            self.state.error = None;
                            if let Some(handle) = self.agent_task.take() {
                                handle.abort();
                            }

                            let _ = provider.reset().await;

                            let count = self.state.messages.len();
                            let title = session::derive_title(&self.state.messages);
                            let now = session::timestamp_now();
                            if let Err(e) = session::save_local(
                                &session::Session {
                                    version: 1,
                                    id: session_id.to_string(),
                                    created_at: now.clone(),
                                    updated_at: now,
                                    workspace_root: std::env::current_dir()
                                        .unwrap_or_default()
                                        .to_string_lossy()
                                        .to_string(),
                                    model_type: self.config.provider.model.clone(),
                                    messages: self.state.messages.clone(),
                                    tag: None,
                                },
                                &self.config,
                            ) {
                                tracing::warn!("Failed to save imported session: {e}");
                            }
                            self.state.status_message = format!(
                                "Imported remote session {session_id}: {title} ({count} msgs)"
                            );
                        }
                        Err(e) => {
                            self.state.messages.push(ChatMessage::system(
                                &format!("Session {session_id} not found locally and remote fetch failed: {e}"),
                            ));
                        }
                    }
                } else {
                    self.state.messages.push(ChatMessage::system(
                        &format!("Session {session_id} not found locally"),
                    ));
                }
            }
        }
        Ok(())
    }

    fn record_gen_stats(&mut self) {
        if let Some(elapsed) = self.state.generation.take_elapsed() {
            if let Some(last) = self.state.messages.iter_mut().last() {
                if last.role == Role::Assistant && !last.content.is_empty() {
                    let tokens = estimate_tokens(&last.content);
                    last.total_tokens = Some(tokens);
                    last.model.clone_from(&self.state.generation.last_model);
                    last.status = self.state.generation.last_status.clone();
                    last.think_elapsed_secs = 0.0;
                    last.references_count = 0;
                    self.state.generation.last_tokens = tokens;
                    self.state.generation.last_duration_secs = elapsed;
                }
            }
        }
    }

    fn auto_save_session(&self) {
        use crate::session;
        let id = &self.state.current_session_id;
        let messages = &self.state.messages;
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
            self.state.messages.push(crate::provider::ChatMessage::system(
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
            self.state.messages.push(crate::provider::ChatMessage::system(
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
            self.state.messages.push(crate::provider::ChatMessage::system(&msg));
        } else if enable {
            self.state.messages.push(crate::provider::ChatMessage::system(
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
    {
        if !props.is_empty() {
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
    }

    result
}
