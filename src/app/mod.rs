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
    pub is_generating: bool,
    pub status_message: String,
    pub scroll_offset: u32,
    pub error: Option<String>,
    pub modal: Option<Modal>,
    pub approved_tools: std::collections::HashSet<String>,
    pub pending_tool_approval: Option<ToolApprovalRequest>,
    pub animation_tick: u64,
}

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
            is_generating: false,
            status_message: if provider.is_some() { "Ready" } else { "No token configured" }.to_string(),
            scroll_offset: 0,
            error: None,
            modal: None,
            approved_tools: std::collections::HashSet::new(),
            pending_tool_approval: None,
            animation_tick: 0,
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
                self.state.is_generating = false;
                self.state.status_message = format!("Approve tool {}?", request.tool_name);
                self.state.modal = Some(Modal::ToolApproval {
                    tool_name: request.tool_name.clone(),
                    arguments: request.arguments.clone(),
                });
                self.state.pending_tool_approval = Some(request);
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

        if let Some(modal) = self.state.modal.clone() {
            match modal {
                Modal::ToolApproval { tool_name, .. } => {
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                            self.state.approved_tools.insert(tool_name.clone());
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
            KeyCode::Enter if !self.state.is_generating => {
                let input = self.state.input_buffer.trim().to_string();
                if !input.is_empty() {
                    self.state.input_buffer.clear();
                    self.state.input_cursor = 0;

                    if input.starts_with('/') {
                        let result = self.commands.execute(&input, &mut self.state, &self.config);
                        match result {
                            CommandResult::Handled => {}
                            CommandResult::NeedsAgent(msg) => {
                                self.state.messages.push(ChatMessage::user(&input));
                                self.send_to_agent(msg).await?;
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
            KeyCode::Char(c) => {
                let byte_pos = char_to_byte_pos(&self.state.input_buffer, self.state.input_cursor);
                self.state.input_buffer.insert(byte_pos, c);
                self.state.input_cursor += 1;
            }
            KeyCode::Backspace => {
                if self.state.input_cursor > 0 {
                    self.state.input_cursor -= 1;
                    let byte_pos = char_to_byte_pos(&self.state.input_buffer, self.state.input_cursor);
                    self.state.input_buffer.remove(byte_pos);
                }
            }
            KeyCode::Delete => {
                let char_count = self.state.input_buffer.chars().count();
                if self.state.input_cursor < char_count {
                    let byte_pos = char_to_byte_pos(&self.state.input_buffer, self.state.input_cursor);
                    self.state.input_buffer.remove(byte_pos);
                }
            }
            KeyCode::Left => {
                self.state.input_cursor = self.state.input_cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                let char_count = self.state.input_buffer.chars().count();
                if self.state.input_cursor < char_count {
                    self.state.input_cursor += 1;
                }
            }
            KeyCode::Home => self.state.input_cursor = 0,
            KeyCode::End => self.state.input_cursor = self.state.input_buffer.chars().count(),
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
        Ok(false)
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
                            let _ = event_tx.send(AppEvent::AgentError(
                                "Streaming state desynchronized while hiding tool blocks".to_string(),
                            ));
                            return;
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
                    let display = if is_error {
                        format!(
                            "Tool {} failed. Raw output is attached internally.\n{}",
                            tool_call.name, preview
                        )
                    } else {
                        format!(
                            "Tool {} completed. Raw output is attached internally.\n{}",
                            tool_call.name, preview
                        )
                    };

                    let tool_message = ChatMessage::tool_with_display(
                        &tool_id,
                        &tool_call.name,
                        &tool_result,
                        &display,
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

fn char_to_byte_pos(s: &str, char_pos: usize) -> usize {
    s.char_indices()
        .nth(char_pos)
        .map(|(i, _)| i)
        .unwrap_or_else(|| s.len())
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
