pub mod events;

use crate::commands::CommandRegistry;
use crate::config::Config;
use crate::error::AppResult;
use crate::mcp::MCPManager;
use crate::mcp::types::McpViewState;
use crate::prompts::{self, PromptFiles};
use crate::provider::{ChatMessage, CompletionRequest, LLMProvider, Role};
use crate::commands::CommandResult;
use crate::provider::estimate_tokens;
use crate::tools::registry::ToolRegistry;
use crate::agent::tool_parser::{parse_tool_calls, stream_visible_text, strip_tool_calls};
use crate::commands::CommandSuggestion;
use crate::skills::{discovery::discover_all_skills, SkillDefinition};
use events::{AgentResult, AppEvent, Modal, QuestionRequest, QuestionState, ToolApprovalRequest, View};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc;

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
    pub input_buffer: String,
    pub input_cursor: usize,
    pub input_selection_anchor: Option<usize>,
    pub is_generating: bool,
    pub status_message: String,
    pub scroll_offset: u32,
    pub error: Option<String>,
    pub modal: Option<Modal>,
    pub approved_tools: std::collections::HashSet<String>,
    pub pending_tool_approval: Option<ToolApprovalRequest>,
    pub pending_question: Option<QuestionRequest>,
    pub animation_tick: u64,
    pub autocomplete: AutocompleteState,
    pub current_session_id: String,
    pub view: View,
    pub mcp_view: McpViewState,
    pub mcp_server_count: usize,
    pub mcp_server_connected_count: usize,
    pub workspace_path: String,
    pub generation_start_time: Option<std::time::Instant>,
    pub last_gen_tokens: u32,
    pub last_gen_duration_secs: f64,
    pub session_started_at: String,
    pub show_stats_panel: bool,
    pub last_mcp_stats_update: Option<std::time::Instant>,
    pub input_history: Vec<String>,
    pub history_index: Option<usize>,
    pub unsent_input: String,
}

#[derive(Debug, Clone, Default)]
pub struct AutocompleteState {
    pub visible: bool,
    pub items: Vec<CommandSuggestion>,
    pub selected: usize,
    pub scroll_offset: usize,
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
            input_buffer: String::new(),
            input_cursor: 0,
            input_selection_anchor: None,
            is_generating: false,
            status_message: if provider.is_some() { "Ready" } else { "No token configured" }.to_string(),
            scroll_offset: 0,
            error: None,
            modal: None,
            approved_tools: crate::whitelist::load(),
            pending_tool_approval: None,
            pending_question: None,
            animation_tick: 0,
            autocomplete: AutocompleteState::default(),
            current_session_id: crate::session::create_session_id(),
            view: View::Chat,
            mcp_view: McpViewState::default(),
            mcp_server_count: 0,
            mcp_server_connected_count: 0,
            show_stats_panel: true,
            workspace_path: std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
            generation_start_time: None,
            last_gen_tokens: 0,
            last_gen_duration_secs: 0.0,
            session_started_at: chrono::Utc::now().to_rfc3339(),
            last_mcp_stats_update: None,
            input_history: crate::session::load_history(),
            history_index: None,
            unsent_input: String::new(),
        };

        if mcp_init_ok {
            let mgr = mcp.lock().await;
            let servers = mgr.get_servers_info();
            state.mcp_server_count = servers.len();
            state.mcp_server_connected_count = servers
                .iter()
                .filter(|s| s.enabled && s.status == "connected")
                .count();
            state.mcp_view.servers = servers;
            state.last_mcp_stats_update = Some(std::time::Instant::now());
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
        crate::tui::restore(&mut terminal)?;
        // Kill any lingering agent task so it can't keep the runtime alive.
        if let Some(handle) = self.agent_task.take() {
            handle.abort();
        }
        // Kill all background/PTY processes so spawn_blocking waiters unblock
        // and the tokio runtime can shut down cleanly.
        crate::tools::background::shutdown_all().await;
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
                    return Ok(());
                }
            }

            self.refresh_mcp_view().await;
            self.update_mcp_stats().await;
            self.render(terminal)?;
        }
    }

    async fn update_mcp_stats(&mut self) {
        const MCP_STATS_INTERVAL_SECS: u64 = 2;
        if let Some(last) = self.state.last_mcp_stats_update {
            if last.elapsed().as_secs() < MCP_STATS_INTERVAL_SECS {
                return;
            }
        }
        let mcp = self.mcp.lock().await;
        let servers = mcp.get_servers_info();
        self.state.mcp_server_count = servers.len();
        self.state.mcp_server_connected_count = servers
            .iter()
            .filter(|s| s.enabled && s.status == "connected")
            .count();
        self.state.mcp_view.servers = servers;
        self.state.last_mcp_stats_update = Some(std::time::Instant::now());
    }

    async fn refresh_mcp_view(&mut self) {
        if self.state.view != View::Mcp || !self.state.mcp_view.servers.is_empty() {
            return;
        }
        let mcp = self.mcp.lock().await;
        self.state.mcp_view.servers = mcp.get_servers_info();
    }

    async fn handle_event(&mut self, event: AppEvent) -> AppResult<bool> {
        match event {
            AppEvent::Key(key) => return self.handle_key(key).await,
            AppEvent::Quit => return Ok(true),
            AppEvent::AgentStarted => {
                self.state.is_generating = true;
                self.state.status_message = "Thinking...".to_string();
                self.state.generation_start_time = Some(std::time::Instant::now());
            }
            AppEvent::BeginAssistantMessage => {
                let should_push = self
                    .state
                    .messages
                    .last()
                    .is_none_or(|message| message.role != Role::Assistant || !message.content.is_empty());
                if should_push {
                    self.state.messages.push(ChatMessage::assistant(""));
                }
            }
            AppEvent::DiscardEmptyAssistantMessage => {
                if self
                    .state
                    .messages
                    .last()
                    .is_some_and(|message| message.role == Role::Assistant && message.content.is_empty())
                {
                    self.state.messages.pop();
                }
            }
            AppEvent::AgentChunk(chunk) => {
                if let Some(last) = self.state.messages.last_mut() {
                    if last.role == Role::Assistant {
                        last.content.push_str(&chunk);
                    }
                }
            }
            AppEvent::AgentDone(_result) => {
                self.state.is_generating = false;
                self.state.status_message = "Ready".to_string();
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
            }
            AppEvent::AgentError(err) => {
                self.state.is_generating = false;
                self.state.error = Some(err.clone());
                self.state.status_message = err;
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
            }
            AppEvent::AddMessage(message) => {
                self.state.messages.push(message);
            }
            AppEvent::ToolStarted { name, id: _ } => {
                self.state.status_message = format!("Running {name}...");
            }
            AppEvent::ToolDone { id: _, result: _ } => {
                self.state.status_message = "Tool finished".to_string();
            }
            AppEvent::ToolError { id: _, error } => {
                self.state.status_message = format!("Tool error: {error}");
            }
            AppEvent::RequestToolApproval(request) => {
                if self.state.approved_tools.contains(&request.tool_name) {
                    request.resolve(true).await;
                    self.state.is_generating = true;
                    self.state.status_message = format!("Running {} (auto-approved)", request.tool_name);
                } else {
                    self.state.is_generating = false;
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
                self.state.is_generating = false;
                self.state.status_message = "Question pending...".to_string();
            }
            AppEvent::PushModal(modal) => {
                self.state.modal = Some(modal);
            }
            AppEvent::PopModal => {
                self.state.modal = None;
            }
            AppEvent::Notification(n) => {
                self.state.status_message = n.message;
            }
            AppEvent::Tick => {
                if self.state.is_generating || (self.state.messages.is_empty() && self.state.modal.is_none()) {
                    self.state.animation_tick = self.state.animation_tick.wrapping_add(1);
                }
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
                KeyCode::Enter if !self.state.is_generating => {
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
                            self.state.is_generating = true;
                            self.state.status_message = format!("Running {}", tool_name);
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                            if let Some(request) = self.state.pending_tool_approval.take() {
                                request.resolve(false).await;
                            }
                            self.state.modal = None;
                            self.state.is_generating = true;
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
                Modal::Confirm { .. } => {
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                            self.state.modal = None;
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                            self.state.modal = None;
                        }
                        _ => {}
                    }
                    return Ok(false);
                }
                Modal::Input { .. } => {
                    match key.code {
                        KeyCode::Esc => {
                            self.state.modal = None;
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
                        self.state.is_generating = true;
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
                if self.state.is_generating {
                    // Ctrl+C while generating cancels the current turn (like Esc).
                    if let Some(handle) = self.agent_task.take() {
                        handle.abort();
                    }
                    self.state.is_generating = false;
                    self.state.status_message = "Cancelled".to_string();
                    if self
                        .state
                        .messages
                        .last()
                        .is_some_and(|message| message.role == Role::Assistant && message.content.is_empty())
                    {
                        self.state.messages.pop();
                    }
                    return Ok(false);
                }
                return Ok(true);
            }
            KeyCode::Esc => {
                if self.state.is_generating {
                    // Cancel the current agent turn: abort the spawned task,
                    // reset is_generating so the user can type a new message.
                    if let Some(handle) = self.agent_task.take() {
                        handle.abort();
                    }
                    self.state.is_generating = false;
                    self.state.status_message = "Cancelled".to_string();
                    if self
                        .state
                        .messages
                        .last()
                        .is_some_and(|message| message.role == Role::Assistant && message.content.is_empty())
                    {
                        self.state.messages.pop();
                    }
                } else if self.state.messages.is_empty() {
                    return Ok(true);
                } else {
                    self.state.messages.clear();
                    self.state.scroll_offset = 0;
                }
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.show_stats_panel = !self.state.show_stats_panel;
            }
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.messages.clear();
                self.state.scroll_offset = 0;
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.input_selection_anchor = Some(0);
                self.state.input_cursor = self.state.input_buffer.chars().count();
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                insert_newline(&mut self.state);
            }
            KeyCode::Enter if !self.state.is_generating => {
                let buf = &self.state.input_buffer;
                let ends_with_backslash = buf
                    .chars()
                    .last()
                    .is_some_and(|c| c == '\\')
                    && self.state.input_cursor == buf.chars().count();
                if ends_with_backslash {
                    self.state.input_buffer.pop();
                    self.state.input_cursor -= 1;
                    let byte_pos =
                        char_to_byte_pos(&self.state.input_buffer, self.state.input_cursor);
                    self.state.input_buffer.insert(byte_pos, '\n');
                    self.state.input_cursor += 1;
                    self.state.input_selection_anchor = None;
                } else {
                    let input = self.state.input_buffer.trim().to_string();
                    if !input.is_empty() {
                        self.state.input_buffer.clear();
                        self.state.input_cursor = 0;
                        self.state.input_selection_anchor = None;
                        self.state.autocomplete = AutocompleteState::default();
                        self.state.history_index = None;
                        crate::session::append_history(&input);

                        if input.starts_with('/') {
                            let result =
                                self.commands.execute(&input, &mut self.state, &self.config);
                            match result {
                                CommandResult::Handled => {}
                                CommandResult::NeedsAgent(msg) => {
                                    self.state.messages.push(ChatMessage::user(&input));
                                    self.send_to_agent(msg).await?;
                                }
                                CommandResult::LoadSession(id) => {
                                    self.handle_load_session(&id).await?;
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
                                    self.state.mcp_view.servers = mcp.get_servers_info();
                                    self.state.messages.push(ChatMessage::system(
                                        "MCP servers reloaded",
                                    ));
                                }
                                CommandResult::ShowTools => {
                                    let tools_text = self.build_tools_display().await;
                                    self.state.messages.push(ChatMessage::system(&tools_text));
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
                                CommandResult::Error(err) => {
                                    self.state.messages.push(ChatMessage::system(&err));
                                }
                            }
                        } else {
                            let expanded = self.expand_file_mentions(&input);
                            self.state.messages.push(ChatMessage::user(&expanded));
                            self.send_to_agent(expanded).await?;
                        }
                    }
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.delete_selection_if_any();
                let byte_pos = char_to_byte_pos(&self.state.input_buffer, self.state.input_cursor);
                self.state.input_buffer.insert(byte_pos, c);
                self.state.input_cursor += 1;
                self.state.input_selection_anchor = None;
            }
            KeyCode::Backspace => {
                if self.state.input_selection_anchor.is_some() {
                    self.delete_selection_if_any();
                } else {
                    let char_count = self.state.input_buffer.chars().count();
                    if self.state.input_cursor > 0 && self.state.input_cursor <= char_count {
                        self.state.input_cursor -= 1;
                        let byte_pos = char_to_byte_pos(&self.state.input_buffer, self.state.input_cursor);
                        if byte_pos < self.state.input_buffer.len() {
                            self.state.input_buffer.remove(byte_pos);
                        }
                    }
                }
            }
            KeyCode::Delete => {
                if self.state.input_selection_anchor.is_some() {
                    self.delete_selection_if_any();
                } else {
                    let char_count = self.state.input_buffer.chars().count();
                    if self.state.input_cursor < char_count {
                        let byte_pos =
                            char_to_byte_pos(&self.state.input_buffer, self.state.input_cursor);
                        if byte_pos < self.state.input_buffer.len() {
                            self.state.input_buffer.remove(byte_pos);
                        }
                    }
                }
            }
            KeyCode::Left => {
                let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                if !shift {
                    self.state.input_selection_anchor = None;
                } else if self.state.input_selection_anchor.is_none() {
                    self.state.input_selection_anchor = Some(self.state.input_cursor);
                }
                let char_count = self.state.input_buffer.chars().count();
                let clamped = self.state.input_cursor.min(char_count);
                let new_cursor = if ctrl {
                    prev_word_start(&self.state.input_buffer, clamped)
                } else {
                    clamped.saturating_sub(1)
                };
                self.state.input_cursor = new_cursor;
            }
            KeyCode::Right => {
                let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                if !shift {
                    self.state.input_selection_anchor = None;
                } else if self.state.input_selection_anchor.is_none() {
                    self.state.input_selection_anchor = Some(self.state.input_cursor);
                }
                let char_count = self.state.input_buffer.chars().count();
                let new_cursor = if ctrl {
                    next_word_end(&self.state.input_buffer, self.state.input_cursor)
                } else if self.state.input_cursor < char_count {
                    self.state.input_cursor + 1
                } else {
                    self.state.input_cursor
                };
                self.state.input_cursor = new_cursor;
            }
            KeyCode::Home => {
                if !key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.state.input_selection_anchor = None;
                } else if self.state.input_selection_anchor.is_none() {
                    self.state.input_selection_anchor = Some(self.state.input_cursor);
                }
                self.state.input_cursor = 0;
            }
            KeyCode::End => {
                if !key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.state.input_selection_anchor = None;
                } else if self.state.input_selection_anchor.is_none() {
                    self.state.input_selection_anchor = Some(self.state.input_cursor);
                }
                self.state.input_cursor = self.state.input_buffer.chars().count();
            }
            KeyCode::Up if !self.state.is_generating
                && self.state.input_buffer.chars().take(self.state.input_cursor).filter(|&c| c == '\n').count() == 0 =>
            {
                let history = &self.state.input_history;
                if !history.is_empty() {
                    let idx = match self.state.history_index {
                        None => {
                            self.state.unsent_input = self.state.input_buffer.clone();
                            Some(history.len() - 1)
                        }
                        Some(i) if i > 0 => Some(i - 1),
                        _ => None,
                    };
                    if let Some(i) = idx {
                        self.state.input_buffer = history[i].clone();
                        self.state.input_cursor = self.state.input_buffer.chars().count();
                        self.state.input_selection_anchor = None;
                        self.state.history_index = Some(i);
                    }
                }
            }
            KeyCode::Down if !self.state.is_generating
                && self.state.input_buffer.chars().skip(self.state.input_cursor).filter(|&c| c == '\n').count() == 0 =>
            {
                let next = self.state.history_index.map(|i| i + 1);
                let history_len = self.state.input_history.len();
                match next {
                    Some(i) if i < history_len => {
                        self.state.input_buffer = self.state.input_history[i].clone();
                        self.state.input_cursor = self.state.input_buffer.chars().count();
                        self.state.history_index = Some(i);
                    }
                    _ => {
                        self.state.input_buffer = std::mem::take(&mut self.state.unsent_input);
                        self.state.input_cursor = self.state.input_buffer.chars().count();
                        self.state.history_index = None;
                    }
                }
                self.state.input_selection_anchor = None;
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

        if self.state.mcp_view.servers.is_empty() {
            let mcp = self.mcp.lock().await;
            self.state.mcp_view.servers = mcp.get_servers_info();
        }

        let details_open = self.state.mcp_view.details_server.is_some();

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.state.mcp_view.active = false;
                self.state.mcp_view.details_server = None;
                self.state.view = View::Chat;
            }
            KeyCode::Up | KeyCode::Char('k') if !details_open => {
                self.state.mcp_view.selected = self.state.mcp_view.selected.saturating_sub(1);
                self.clamp_mcp_scroll();
            }
            KeyCode::Down | KeyCode::Char('j') if !details_open => {
                let max = self.state.mcp_view.servers.len().saturating_sub(1);
                self.state.mcp_view.selected = self.state.mcp_view.selected.min(max);
                self.state.mcp_view.selected += 1;
                self.state.mcp_view.selected = self.state.mcp_view.selected.min(max);
                self.clamp_mcp_scroll();
            }
            KeyCode::Enter => {
                if details_open {
                    self.state.mcp_view.details_server = None;
                } else if let Some(info) = self.state.mcp_view.servers.get(self.state.mcp_view.selected) {
                    self.state.mcp_view.details_server = Some(info.name.clone());
                    self.state.mcp_view.scroll_offset = 0;
                }
            }
            KeyCode::Char(' ') if !details_open => {
                if let Some(info) = self.state.mcp_view.servers.get(self.state.mcp_view.selected).cloned() {
                    let name = info.name.clone();
                    let mut mcp = self.mcp.lock().await;
                    if let Err(e) = mcp.toggle_server(&name).await {
                        self.state.mcp_view.status_message = format!("Toggle failed: {e}");
                    } else {
                        self.state.mcp_view.status_message = format!(
                            "{} {}",
                            name,
                            if info.enabled { "disabled" } else { "enabled" },
                        );
                    }
                    self.state.mcp_view.servers = mcp.get_servers_info();
                }
            }
            KeyCode::Char('r') if !details_open => {
                if let Some(info) = self.state.mcp_view.servers.get(self.state.mcp_view.selected).cloned() {
                    let name = info.name.clone();
                    self.state.mcp_view.status_message = format!("Reconnecting {name}...");
                    let mut mcp = self.mcp.lock().await;
                    if let Err(e) = mcp.reconnect_server(&name).await {
                        self.state.mcp_view.status_message = format!("Reconnect failed: {e}");
                    } else {
                        self.state.mcp_view.status_message = format!("{name} reconnected");
                    }
                    self.state.mcp_view.servers = mcp.get_servers_info();
                }
            }
            KeyCode::Char('d') if !details_open => {
                if let Some(info) = self.state.mcp_view.servers.get(self.state.mcp_view.selected).cloned() {
                    let name = info.name.clone();
                    let mut mcp = self.mcp.lock().await;
                    if let Err(e) = mcp.remove_server(&name).await {
                        self.state.mcp_view.status_message = format!("Remove failed: {e}");
                    } else {
                        self.state.mcp_view.status_message = format!("{name} removed");
                    }
                    self.state.mcp_view.servers = mcp.get_servers_info();
                    self.state.mcp_view.selected = self.state.mcp_view.selected.min(
                        self.state.mcp_view.servers.len().saturating_sub(1),
                    );
                }
            }
            KeyCode::Up | KeyCode::Char('k') if details_open => {
                self.state.mcp_view.scroll_offset = self.state.mcp_view.scroll_offset.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') if details_open => {
                self.state.mcp_view.scroll_offset += 1;
            }
            _ => {}
        }
        Ok(false)
    }

    fn clamp_mcp_scroll(&self) {
        // no-op for now, we manage scroll in the renderer
    }

    fn refresh_autocomplete(&mut self) {
        let buf = self.state.input_buffer.clone();
        let query_main = buf
            .strip_prefix('/')
            .map(|rest| rest.split_whitespace().next().unwrap_or(""))
            .unwrap_or("");
        let active = buf.starts_with('/')
            && !self.state.is_generating
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
        let suggestion = self.state.autocomplete.items[idx].name.clone();
        let new_buf = format!("/{} ", suggestion);
        self.state.input_buffer = new_buf;
        self.state.input_cursor = self.state.input_buffer.chars().count();
        self.state.input_selection_anchor = None;
        self.state.autocomplete = AutocompleteState::default();
    }

    fn delete_selection_if_any(&mut self) -> Option<usize> {
        if let Some(anchor) = self.state.input_selection_anchor.take() {
            let cursor = self.state.input_cursor;
            let (start, end) = if anchor <= cursor {
                (anchor, cursor)
            } else {
                (cursor, anchor)
            };
            let bs = char_to_byte_pos(&self.state.input_buffer, start);
            let be = char_to_byte_pos(&self.state.input_buffer, end);
            self.state.input_buffer.drain(bs..be);
            self.state.input_cursor = start;
        }
        None
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
        let mut messages: Vec<ChatMessage> = self.state.messages.clone();
        let system_prompt = self.build_system_prompt().await;

        let model = self.config.provider.model.clone();
        let temperature = self.config.provider.temperature;
        let max_tokens = self.config.provider.max_tokens;
        let max_steps = self.config.agent.max_steps_per_turn.max(1);
        let max_tools_per_step = self.config.agent.max_tools_per_step.max(1);
        let tools = Arc::clone(&self.tools);
        let mcp = Arc::clone(&self.mcp);

        let _ = event_tx.send(AppEvent::AgentStarted);

        let handle = tokio::spawn(async move {
            let mut collected_tool_calls = Vec::new();

            for _step in 0..max_steps {
                let _ = event_tx.send(AppEvent::BeginAssistantMessage);
                let mut request_messages = Vec::with_capacity(messages.len() + 1);
                request_messages.push(ChatMessage::system(&system_prompt));
                request_messages.extend(messages.clone());

                let request = CompletionRequest {
                    messages: request_messages,
                    model: model.clone(),
                    temperature,
                    max_tokens,
                    stream: true,
                };

                let (tx, mut rx) = mpsc::unbounded_channel();
                if let Err(error) = provider.complete_stream(request, tx).await {
                    let _ = event_tx.send(AppEvent::AgentError(error.to_string()));
                    return;
                }

                let mut full_response = String::new();
                let mut streamed_visible = String::new();
                // Guard against a hung stream: if no chunk arrives for 120s,
                // abort so is_generating doesn't stay true forever.
                let idle_timeout = std::time::Duration::from_secs(120);
                loop {
                    match tokio::time::timeout(idle_timeout, rx.recv()).await {
                        Err(_) => {
                            let _ = event_tx.send(AppEvent::AgentError(
                                "Stream timed out (no data for 120s). Cancelling turn.".to_string(),
                            ));
                            return;
                        }
                        Ok(None) => break,
                        Ok(Some(chunk)) => {
                            if !chunk.content.is_empty() {
                                full_response.push_str(&chunk.content);
                                let next_visible = stream_visible_text(&full_response);
                                if next_visible.starts_with(&streamed_visible) {
                                    let delta = &next_visible[streamed_visible.len()..];
                                    if !delta.is_empty() {
                                        let _ = event_tx.send(AppEvent::AgentChunk(delta.to_string()));
                                    }
                                } else if !next_visible.is_empty() {
                                    let _ = event_tx.send(AppEvent::AddMessage(ChatMessage::system(
                                        "⚠ Streaming sync issue — agent will continue",
                                    )));
                                }
                                streamed_visible = next_visible;
                            }
                            if matches!(chunk.finish_reason.as_deref(), Some("stop")) {
                                break;
                            }
                        }
                    }
                }

                let tool_calls = parse_tool_calls(&full_response);
                let visible_text = strip_tool_calls(&full_response);

                if tool_calls.is_empty() {
                    if !visible_text.is_empty() {
                        messages.push(ChatMessage::assistant(&visible_text));
                    } else {
                        let _ = event_tx.send(AppEvent::DiscardEmptyAssistantMessage);
                    }

                    let _ = event_tx.send(AppEvent::AgentDone(AgentResult {
                        text: visible_text,
                        tool_calls: collected_tool_calls,
                    }));
                    return;
                }

                // There are tool calls — always push an assistant message (even if
                // visible text is empty) so tool results are bracketed by assistant
                // turns. Otherwise consecutive tool messages pile up and the provider's
                // trailing-tool-batch formatter re-emits all of them as TOOL RESULT
                // blocks every step, causing the model to retry and the count to grow.
                messages.push(ChatMessage::assistant(&visible_text));
                if visible_text.is_empty() {
                    let _ = event_tx.send(AppEvent::DiscardEmptyAssistantMessage);
                }

                for tool_call in tool_calls.into_iter().take(max_tools_per_step) {
                    let tool_id = uuid::Uuid::new_v4().to_string();

                    let (tool_result, is_error) = if tool_call.name == "question" {
                        let question_text = tool_call.arguments["question"]
                            .as_str()
                            .unwrap_or("(no question)");
                        let options: Vec<String> = tool_call.arguments["options"]
                            .as_array()
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default();
                        let allow_custom = tool_call.arguments["allow_custom"]
                            .as_bool()
                            .unwrap_or(false);

                        let qs = QuestionState::new(
                            question_text.to_string(),
                            options,
                            allow_custom,
                        );
                        let request = QuestionRequest::new();
                        let _ = event_tx
                            .send(AppEvent::RequestQuestion(request.clone(), qs));

                        let _ = event_tx.send(AppEvent::ToolStarted {
                            name: tool_call.name.clone(),
                            id: tool_id.clone(),
                        });

                        match request.wait().await {
                            Some(answer) if !answer.is_empty() => {
                                (format!("User answered: {answer}"), false)
                            }
                            _ => {
                                ("User cancelled the question".to_string(), true)
                            }
                        }
                    } else {
                        let arguments_preview =
                            serde_json::to_string_pretty(&tool_call.arguments)
                                .unwrap_or_else(|_| tool_call.arguments.to_string());
                        let approval = ToolApprovalRequest::new(
                            tool_call.name.clone(),
                            arguments_preview,
                        );
                        let _ = event_tx
                            .send(AppEvent::RequestToolApproval(approval.clone()));
                        let approved = approval.wait().await;

                        if approved {
                            let _ = event_tx.send(AppEvent::ToolStarted {
                                name: tool_call.name.clone(),
                                id: tool_id.clone(),
                            });
                            if tool_call.name.starts_with("mcp__") {
                                let mut mcp = mcp.lock().await;
                                match mcp
                                    .call_tool(
                                        &tool_call.name,
                                        tool_call.arguments.clone(),
                                    )
                                    .await
                                {
                                    Ok(result) => (result.content, result.is_error),
                                    Err(error) => (error.to_string(), true),
                                }
                            } else {
                                let result = tools
                                    .execute(
                                        &tool_call.name,
                                        tool_call.arguments.clone(),
                                    )
                                    .await;
                                (result.content, result.is_error)
                            }
                        } else {
                            ("Execution denied by user.".to_string(), true)
                        }
                    };

                    let preview = summarize_tool_result(&tool_result);
                    let display = preview.clone();

                    let tool_message = ChatMessage::tool_with_display(
                        &tool_id,
                        &tool_call.name,
                        &tool_result,
                        &display,
                        is_error,
                    );
                    messages.push(tool_message.clone());
                    collected_tool_calls.push(events::ToolCallInfo {
                        name: tool_call.name.clone(),
                        arguments: tool_call.arguments.clone(),
                        result: Some(tool_result.clone()),
                    });
                    let _ = event_tx.send(AppEvent::AddMessage(tool_message));

                    if is_error {
                        let _ = event_tx.send(AppEvent::ToolError {
                            id: tool_id,
                            error: preview,
                        });
                    } else {
                        let _ = event_tx.send(AppEvent::ToolDone {
                            id: tool_id,
                            result: preview,
                        });
                    }
                }
            }

            let _ = event_tx.send(AppEvent::AgentError(
                "Reached max agent steps before producing a final answer".to_string(),
            ));
        });

        self.agent_task = Some(handle);

        Ok(())
    }

    async fn handle_load_session(&mut self, session_id: &str) -> AppResult<()> {
        use crate::session;

        match session::load_local(session_id, &self.config) {
            Ok(s) => {
                self.state.messages = s.messages;
                self.state.current_session_id = s.id;
                self.state.scroll_offset = 0;
                self.state.input_buffer.clear();
                self.state.input_cursor = 0;
                self.state.input_selection_anchor = None;
                self.state.autocomplete = Default::default();
                self.state.is_generating = false;
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
                            self.state.input_buffer.clear();
                            self.state.input_cursor = 0;
                            self.state.input_selection_anchor = None;
                            self.state.autocomplete = Default::default();
                            self.state.is_generating = false;
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
        if let Some(start) = self.state.generation_start_time.take() {
            let elapsed = start.elapsed().as_secs_f64();
            if let Some(last) = self.state.messages.iter_mut().last() {
                if last.role == Role::Assistant && !last.content.is_empty() {
                    let tokens = estimate_tokens(&last.content);
                    last.total_tokens = Some(tokens);
                    self.state.last_gen_tokens = tokens;
                    self.state.last_gen_duration_secs = elapsed;
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

pub fn char_to_byte_pos(s: &str, char_pos: usize) -> usize {
    s.char_indices()
        .nth(char_pos)
        .map(|(i, _)| i)
        .unwrap_or_else(|| s.len())
}

fn char_is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn prev_word_start(s: &str, cursor: usize) -> usize {
    if cursor == 0 {
        return 0;
    }
    let chars: Vec<char> = s.chars().collect();
    let mut i = cursor;
    while i > 0 && chars[i - 1].is_whitespace() {
        i -= 1;
    }
    if i == 0 {
        return 0;
    }
    let word = char_is_word(chars[i - 1]);
    while i > 0 && char_is_word(chars[i - 1]) == word && !chars[i - 1].is_whitespace() {
        i -= 1;
    }
    i
}

fn next_word_end(s: &str, cursor: usize) -> usize {
    let total = s.chars().count();
    if cursor >= total {
        return total;
    }
    let chars: Vec<char> = s.chars().collect();
    let mut i = cursor;
    while i < total && chars[i].is_whitespace() {
        i += 1;
    }
    if i >= total {
        return total;
    }
    let word = char_is_word(chars[i]);
    while i < total && !chars[i].is_whitespace() && char_is_word(chars[i]) == word {
        i += 1;
    }
    i
}

fn insert_newline(state: &mut crate::app::AppState) {
    if let Some(anchor) = state.input_selection_anchor.take() {
        let (start, end) = if anchor <= state.input_cursor {
            (anchor, state.input_cursor)
        } else {
            (state.input_cursor, anchor)
        };
        let bs = char_to_byte_pos(&state.input_buffer, start);
        let be = char_to_byte_pos(&state.input_buffer, end);
        state.input_buffer.drain(bs..be);
        state.input_cursor = start;
    }
    let byte_pos = char_to_byte_pos(&state.input_buffer, state.input_cursor);
    state.input_buffer.insert(byte_pos, '\n');
    state.input_cursor += 1;
    state.input_selection_anchor = None;
}

fn summarize_tool_result(result: &str) -> String {
    const MAX_LEN: usize = 160;
    let compact = result.lines().take(3).collect::<Vec<_>>().join(" ");
    if compact.len() > MAX_LEN {
        crate::util::truncate_with_ellipsis(&compact, MAX_LEN)
    } else if compact.is_empty() {
        "No visible output.".to_string()
    } else {
        compact
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
