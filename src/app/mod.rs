pub mod events;

use crate::commands::CommandRegistry;
use crate::config::Config;
use crate::error::AppResult;
use crate::mcp::MCPManager;
use crate::provider::{ChatMessage, CompletionRequest, LLMProvider, Role};
use crate::commands::CommandResult;
use crate::tools::registry::ToolRegistry;
use events::{AgentResult, AppEvent};
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct App {
    pub config: Config,
    pub state: AppState,
    pub event_tx: mpsc::UnboundedSender<AppEvent>,
    event_rx: mpsc::UnboundedReceiver<AppEvent>,
    provider: Option<Arc<dyn LLMProvider>>,
    commands: CommandRegistry,
    pub mcp: MCPManager,
    tools: ToolRegistry,
}

pub struct AppState {
    pub messages: Vec<ChatMessage>,
    pub input_buffer: String,
    pub input_cursor: usize,
    pub is_generating: bool,
    pub status_message: String,
    pub scroll_offset: u32,
    pub error: Option<String>,
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

        let mut mcp = MCPManager::new();
        if let Err(e) = mcp.initialize().await {
            tracing::warn!("MCP initialization failed: {e}");
        }

        let state = AppState {
            messages: Vec::new(),
            input_buffer: String::new(),
            input_cursor: 0,
            is_generating: false,
            status_message: if provider.is_some() { "Ready" } else { "No token configured" }.to_string(),
            scroll_offset: 0,
            error: None,
        };

        Ok(Self {
            config,
            state,
            event_tx,
            event_rx,
            provider,
            commands: CommandRegistry::new(),
            mcp,
            tools: ToolRegistry::new(),
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

        let tick_rate = std::time::Duration::from_millis(50);
        let mut tick_interval = tokio::time::interval(tick_rate);
        let mut event_stream = EventStream::new();

        loop {
            tokio::select! {
                _ = tick_interval.tick() => {
                    self.handle_event(AppEvent::Tick).await?;
                }
                Some(Ok(event)) = event_stream.next() => {
                    match event {
                        crossterm::event::Event::Key(key) => {
                            self.handle_event(AppEvent::Key(key)).await?;
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
                self.state.status_message = "Generating...".to_string();
                self.state.messages.push(ChatMessage {
                    role: Role::Assistant,
                    content: String::new(),
                    name: None,
                    tool_call_id: None,
                });
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
            }
            AppEvent::AgentError(err) => {
                self.state.is_generating = false;
                self.state.error = Some(err.clone());
                self.state.status_message = err;
            }
            AppEvent::ToolStarted { name, id: _ } => {
                self.state.status_message = format!("Running {name}...");
            }
            AppEvent::ToolDone { id: _, result: _ } => {
                self.state.status_message = "Ready".to_string();
            }
            AppEvent::ToolError { id: _, error } => {
                self.state.status_message = format!("Tool error: {error}");
            }
            AppEvent::Notification(n) => {
                self.state.status_message = n.message;
            }
            _ => {}
        }
        Ok(false)
    }

    async fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> AppResult<bool> {
        use crossterm::event::{KeyCode, KeyModifiers};

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
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
                self.state.input_buffer.insert(self.state.input_cursor, c);
                self.state.input_cursor += 1;
            }
            KeyCode::Backspace => {
                if self.state.input_cursor > 0 {
                    self.state.input_cursor -= 1;
                    self.state.input_buffer.remove(self.state.input_cursor);
                }
            }
            KeyCode::Delete => {
                if self.state.input_cursor < self.state.input_buffer.len() {
                    self.state.input_buffer.remove(self.state.input_cursor);
                }
            }
            KeyCode::Left => {
                self.state.input_cursor = self.state.input_cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                if self.state.input_cursor < self.state.input_buffer.len() {
                    self.state.input_cursor += 1;
                }
            }
            KeyCode::Home => self.state.input_cursor = 0,
            KeyCode::End => self.state.input_cursor = self.state.input_buffer.len(),
            KeyCode::Up => {
                self.state.scroll_offset = self.state.scroll_offset.saturating_sub(1);
            }
            KeyCode::Down => {
                self.state.scroll_offset = self.state.scroll_offset.saturating_add(1);
            }
            KeyCode::PageUp => {
                self.state.scroll_offset = self.state.scroll_offset.saturating_sub(10);
            }
            KeyCode::PageDown => {
                self.state.scroll_offset = self.state.scroll_offset.saturating_add(10);
            }
            _ => {}
        }
        Ok(false)
    }

    fn build_system_prompt(&self) -> String {
        let mut prompt = String::from(
            "You are Pooprusteek, a helpful AI coding assistant. \
             You can help with coding, file operations, and terminal commands.\n"
        );

        let mcp_tools = self.mcp.get_dynamic_tool_names();
        if !mcp_tools.is_empty() {
            prompt.push_str("\nYou have access to the following MCP tools:\n");
            for tool_name in &mcp_tools {
                if let Some(desc) = self.mcp.get_tool_description(tool_name) {
                    prompt.push_str(&format!("  - {tool_name}: {desc}\n"));
                }
            }
            prompt.push_str("\nTo use an MCP tool, respond with: [TOOL:tool_name] {\"arg\": \"value\"}\n");
        }

        let builtin_tools = self.tools.names();
        if !builtin_tools.is_empty() {
            prompt.push_str("\nYou have access to the following built-in tools:\n");
            for tool_name in &builtin_tools {
                prompt.push_str(&format!("  - {tool_name}\n"));
            }
            prompt.push_str("\nTo use a built-in tool, respond with: [TOOL:tool_name] {\"arg\": \"value\"}\n");
        }

        prompt
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

        let system_prompt = self.build_system_prompt();
        messages.insert(0, ChatMessage::system(&system_prompt));

        let model = self.config.provider.model.clone();
        let temperature = self.config.provider.temperature;
        let max_tokens = self.config.provider.max_tokens;

        let mcp_tool_names: Vec<String> = self.mcp.get_dynamic_tool_names();
        let builtin_tool_names: Vec<String> = self.tools.names();

        let _ = event_tx.send(AppEvent::AgentStarted);

        tokio::spawn(async move {
            let request = CompletionRequest {
                messages,
                model,
                temperature,
                max_tokens,
                stream: true,
            };

            let (tx, mut rx) = mpsc::unbounded_channel();

            let stream_result = provider.complete_stream(request, tx).await;
            if let Err(e) = stream_result {
                let _ = event_tx.send(AppEvent::AgentError(e.to_string()));
                return;
            }

            let mut full_response = String::new();

            while let Some(chunk) = rx.recv().await {
                if !chunk.content.is_empty() {
                    full_response.push_str(&chunk.content);
                    let _ = event_tx.send(AppEvent::AgentChunk(chunk.content));
                }
                if let Some(reason) = &chunk.finish_reason {
                    if reason == "stop" {
                        break;
                    }
                }
            }

            let _ = event_tx.send(AppEvent::AgentDone(AgentResult {
                text: full_response.clone(),
                tool_calls: Vec::new(),
            }));

            let _ = full_response;
            let _ = mcp_tool_names;
            let _ = builtin_tool_names;
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
