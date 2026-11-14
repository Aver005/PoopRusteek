use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

/// `/resume` и `/sessions` — один экран выбора сессии под двумя именами.
pub struct SessionListCommand {
    pub name: &'static str,
}

impl Command for SessionListCommand {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "Pick a saved session to continue (Tab: every project)"
    }

    fn usage(&self) -> &str {
        if self.name == "sessions" {
            "/sessions"
        } else {
            "/resume"
        }
    }

    fn execute(&self, _args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        CommandResult::OpenResume
    }
}
