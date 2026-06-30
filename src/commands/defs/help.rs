use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

pub struct HelpCommand;

impl Command for HelpCommand {
    fn name(&self) -> &str {
        "help"
    }

    fn description(&self) -> &str {
        "Show available commands"
    }

    fn usage(&self) -> &str {
        "/help"
    }

    fn execute(&self, _args: &str, state: &mut AppState, _config: &Config) -> CommandResult {
        let help_text = "\
Available commands:
  /help       — Show this help
  /clear      — Clear chat history
  /compact    — Compact context (summarize history)
  /jobs       — List/kill/prune background jobs
  /ps         — Alias for /jobs
  /reset      — Reset session
  /version    — Show version info
  /quit       — Exit application

Keyboard shortcuts:
  Ctrl+C        — Quit
  Ctrl+L         — Clear chat
  Enter          — Send message
  Shift+Enter    — New line in prompt
  `\\` + Enter   — New line in prompt (line continuation)
  Up/Down        — Scroll chat
  Ctrl+Left/Right — Move by word
  Ctrl+Shift+Left/Right — Select by word
  Shift+Left/Right — Select by char
  Ctrl+A         — Select all";

        state.focused_mut().messages.push(crate::provider::ChatMessage::system(help_text));
        CommandResult::Handled
    }
}
