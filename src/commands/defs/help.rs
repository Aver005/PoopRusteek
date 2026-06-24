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

    fn execute(&self, _args: &str, state: &mut AppState, _config: &Config) -> CommandResult {
        let help_text = "\
Available commands:
  /help       — Show this help
  /clear      — Clear chat history
  /compact    — Compact context (summarize history)
  /reset      — Reset session
  /version    — Show version info
  /quit       — Exit application

Keyboard shortcuts:
  Ctrl+C      — Quit
  Ctrl+L      — Clear chat
  Enter       — Send message
  Up/Down     — Scroll chat";

        state.messages.push(crate::provider::ChatMessage::system(help_text));
        CommandResult::Handled
    }
}
