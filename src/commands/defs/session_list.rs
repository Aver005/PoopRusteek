use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;
use crate::session;

pub struct SessionListCommand;

impl Command for SessionListCommand {
    fn name(&self) -> &str {
        "sessions"
    }

    fn description(&self) -> &str {
        "List local sessions"
    }

    fn usage(&self) -> &str {
        "/sessions"
    }

    fn execute(&self, _args: &str, state: &mut AppState, _config: &Config) -> CommandResult {
        let sessions = match session::list_sessions(_config) {
            Ok(s) => s,
            Err(e) => return CommandResult::Error(format!("Failed to list sessions: {e}")),
        };

        if sessions.is_empty() {
            state.messages.push(crate::provider::ChatMessage::system(
                "No local sessions found.",
            ));
            return CommandResult::Handled;
        }

        let mut lines = Vec::new();
        lines.push("--- Local sessions ---".to_string());
        for s in &sessions {
            lines.push(format!(
                "  {} | {} msgs | {}",
                s.id, s.message_count, s.title
            ));
        }
        lines.push(format!("Total: {} sessions ---", sessions.len()));

        state.messages.push(crate::provider::ChatMessage::system(&lines.join("\n")));
        CommandResult::Handled
    }
}
