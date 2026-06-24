pub mod events;

use crate::config::Config;
use crate::error::AppResult;
use crate::provider::ChatMessage;
use events::{AppEvent, View};
use tokio::sync::mpsc;

pub struct App {
    pub config: Config,
    pub state: AppState,
    pub event_tx: mpsc::UnboundedSender<AppEvent>,
    event_rx: mpsc::UnboundedReceiver<AppEvent>,
}

pub struct AppState {
    pub current_view: View,
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

        let state = AppState {
            current_view: View::Chat,
            messages: Vec::new(),
            input_buffer: String::new(),
            input_cursor: 0,
            is_generating: false,
            status_message: "Ready".to_string(),
            scroll_offset: 0,
            error: None,
        };

        Ok(Self {
            config,
            state,
            event_tx,
            event_rx,
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

        let tick_rate = std::time::Duration::from_millis(100);
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
            }
            AppEvent::AgentChunk(chunk) => {
                if let Some(last) = self.state.messages.last_mut() {
                    if last.role == crate::provider::Role::Assistant {
                        last.content.push_str(&chunk);
                    }
                }
            }
            AppEvent::AgentDone(result) => {
                self.state.is_generating = false;
                self.state.status_message = "Ready".to_string();
                if !result.text.is_empty() {
                    self.state.messages.push(ChatMessage::assistant(&result.text));
                }
            }
            AppEvent::AgentError(err) => {
                self.state.is_generating = false;
                self.state.error = Some(err);
                self.state.status_message = "Error".to_string();
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
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                // Copy mode - TODO
            }
            KeyCode::Enter if !self.state.is_generating => {
                let input = self.state.input_buffer.trim().to_string();
                if !input.is_empty() {
                    self.state.messages.push(crate::provider::ChatMessage::user(&input));
                    self.state.input_buffer.clear();
                    self.state.input_cursor = 0;
                    self.send_to_agent(input).await?;
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

    async fn send_to_agent(&mut self, _input: String) -> AppResult<()> {
        // TODO: Implement agent loop
        // For now, just echo back
        self.state.messages.push(crate::provider::ChatMessage::assistant(
            "Agent not yet implemented. This is a placeholder response."
        ));
        Ok(())
    }

    fn render(&self, terminal: &mut crate::tui::TuiTerminal) -> AppResult<()> {
        crate::tui::render::render(terminal, &self.state, &self.config)
    }
}
