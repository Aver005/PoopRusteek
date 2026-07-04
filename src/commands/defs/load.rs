use crate::app::AppState;
use crate::commands::{with_args, Command, CommandResult};
use crate::config::Config;

pub struct LoadCommand;

impl Command for LoadCommand {
    fn name(&self) -> &str {
        "load"
    }

    fn description(&self) -> &str {
        "Load a session by ID (local file or DeepSeek remote)"
    }

    fn usage(&self) -> &str {
        "/load <session_id>"
    }

    fn execute(&self, args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        with_args(args, "/load <session_id>", |id| {
            CommandResult::LoadSession(id.to_string())
        })
    }
}
