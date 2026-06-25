pub mod events;

use crate::commands::CommandRegistry;
use crate::config::Config;
use crate::error::AppResult;
use crate::mcp::MCPManager;
use crate::prompts::{self, PromptFiles};
use crate::provider::{ChatMessage, CompletionRequest, LLMProvider, Role};
use crate::commands::CommandResult;
use crate::tools::registry::ToolRegistry;
use crate::agent::tool_parser::{parse_tool_calls, stream_visible_text, strip_tool_calls};
use crate::commands::CommandSuggestion;
use events::{AgentResult, AppEvent, Modal, ToolApprovalRequest};
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
    pub animation_tick: u64,
    pub autocomplete: AutocompleteState,
    pub current_session_id: String,
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
            let ds = crate::provider::deepseek::DeepseekProvider::new(&config.provider)?;
            Some(Arc::new(ds))
        };

        let mut mcp_manager = MCPManager::new();
        if let Err(e) = mcp_manager.initialize().await {
            tracing::warn!("MCP initialization failed: {e}");
        }
        let mcp = Arc::new(tokio::sync::Mutex::new(mcp_manager));
        let tools = Arc::new(ToolRegistry::new());
        let prompts = prompts::load_prompt_files()?;

        let state = AppState {
            messages: Vec::new(),
            input_buffer: String::new(),
            input_cursor: 0,
            input_selection_anchor: None,
            is_generating: false,
            status_message: if provider.is_some() { "Ready" } else { "No token configured" }.to_string(),
            scroll_offset: 0,
            error: None,
            modal: None,
            approved_tools: std::collections::HashSet::new(),
            pending_tool_approval: None,
            animation_tick: 0,
            autocomplete: AutocompleteState::default(),
            current_session_id: crate::session::create_session_id(),
        };

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
        })
    }

    pub async fn run(&mut self) -> AppResult<()> {
        let mut terminal = crate::tui::init()?;
        let result = self.run_loop(&mut terminal).await;
        crate::tui::restore(&mut terminal)?;
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
            }

            self.render(terminal)?;
        }
    }

    async fn handle_event(&mut self, event: AppEvent) -> AppResult<bool> {
        match event {
            AppEvent::Key(key) => return self.handle_key(key).await,
            AppEvent::Quit => return Ok(true),
            AppEvent::AgentStarted => {
                self.state.is_generating = true;
                self.state.status_message = "Thinking...".to_string();
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
            }
        }

        match key.code {
            KeyCode::Char(c)
                if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(c, 'c' | 'C') =>
            {
                return Ok(true);
            }
            KeyCode::Esc => {
                return Ok(true);
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
                                    if let Some(provider) = &self.provider {
                                        let _ = provider.reset().await;
                                    }
                                    self.state.current_session_id =
                                        crate::session::create_session_id();
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
        let workspace = std::env::current_dir()
            .ok()
            .map(|dir| dir.display().to_string())
            .unwrap_or_else(|| ".".to_string());
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
                .map(|tool| format!("- `{}`: {}", tool.name, tool.description))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let mcp = self.mcp.lock().await;
        let dynamic_tool_names = mcp.get_dynamic_tool_names();
        let mcp_section = if dynamic_tool_names.is_empty() {
            "- none".to_string()
        } else {
            dynamic_tool_names
                .iter()
                .map(|tool_name| {
                    let description = mcp
                        .get_tool_description(tool_name)
                        .unwrap_or_else(|| "No description".to_string());
                    format!("- `{tool_name}`: {description}")
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
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

        format!("{}\n\n{}", base_prompt.trim(), tools_prompt.trim())
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

        tokio::spawn(async move {
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
                while let Some(chunk) = rx.recv().await {
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

                let tool_calls = parse_tool_calls(&full_response);
                let visible_text = strip_tool_calls(&full_response);

                if !visible_text.is_empty() {
                    messages.push(ChatMessage::assistant(&visible_text));
                } else {
                    let _ = event_tx.send(AppEvent::DiscardEmptyAssistantMessage);
                }

                if tool_calls.is_empty() {
                    let _ = event_tx.send(AppEvent::AgentDone(AgentResult {
                        text: visible_text,
                        tool_calls: collected_tool_calls,
                    }));
                    return;
                }

                for tool_call in tool_calls.into_iter().take(max_tools_per_step) {
                    let tool_id = uuid::Uuid::new_v4().to_string();
                    let arguments_preview = serde_json::to_string_pretty(&tool_call.arguments)
                        .unwrap_or_else(|_| tool_call.arguments.to_string());
                    let approval = ToolApprovalRequest::new(
                        tool_call.name.clone(),
                        arguments_preview,
                    );
                    let _ = event_tx.send(AppEvent::RequestToolApproval(approval.clone()));
                    let approved = approval.wait().await;

                    let execution = if approved {
                        let _ = event_tx.send(AppEvent::ToolStarted {
                            name: tool_call.name.clone(),
                            id: tool_id.clone(),
                        });
                        if tool_call.name.starts_with("mcp__") {
                            let mut mcp = mcp.lock().await;
                            match mcp.call_tool(&tool_call.name, tool_call.arguments.clone()).await {
                                Ok(result) => (result.content, result.is_error),
                                Err(error) => (error.to_string(), true),
                            }
                        } else {
                            let result = tools.execute(&tool_call.name, tool_call.arguments.clone()).await;
                            (result.content, result.is_error)
                        }
                    } else {
                        ("Execution denied by user.".to_string(), true)
                    };

                    let (tool_result, is_error) = execution;
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

    fn auto_save_session(&self) {
        use crate::session;
        let id = &self.state.current_session_id;
        let messages = &self.state.messages;
        if messages.is_empty() {
            return;
        }
        let now = session::timestamp_now();
        if let Err(e) = session::save_session(id, &now, messages, &self.config) {
            tracing::warn!("Failed to auto-save session: {e}");
        }
    }

    fn render(&self, terminal: &mut crate::tui::TuiTerminal) -> AppResult<()> {
        crate::tui::render::render(terminal, &self.state, &self.config)
    }

    fn expand_file_mentions(&self, input: &str) -> String {
        let workspace = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let mentions = crate::cli::file_mentions::extract_mentions(input, &workspace);

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
        format!("{}...", &compact[..MAX_LEN])
    } else if compact.is_empty() {
        "No visible output.".to_string()
    } else {
        compact
    }
}
