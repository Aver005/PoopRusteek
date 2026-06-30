pub mod conversation;
pub mod events;
pub mod generation;
mod goal;
pub mod input;
mod keys;
pub mod mcp_status;
mod multichat;

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
use events::{AppEvent, GoalStage, Modal, QuestionRequest, ToolApprovalRequest, View};
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
            AppEvent::SpawnSubAgent { parent, label, prompt } => {
                self.spawn_sub_agent(parent, label, prompt).await?;
            }
            _ => {}
        }
        Ok(false)
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
