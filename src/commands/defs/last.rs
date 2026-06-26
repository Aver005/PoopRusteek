use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

pub struct LastCommand;

impl Command for LastCommand {
    fn name(&self) -> &str {
        "last"
    }

    fn description(&self) -> &str {
        "Open the most recent session, or start a fresh chat"
    }

    fn usage(&self) -> &str {
        "/last"
    }

    fn execute(&self, _args: &str, _state: &mut AppState, config: &Config) -> CommandResult {
        match crate::session::list_sessions(config) {
            Ok(sessions) if !sessions.is_empty() => {
                CommandResult::LoadSession(sessions[0].id.clone())
            }
            _ => CommandResult::Handled,
        }
    }
}
