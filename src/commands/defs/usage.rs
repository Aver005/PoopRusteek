use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

/// `/usage` — расход за сутки по аккаунтам.
pub struct UsageCommand;

impl Command for UsageCommand {
    fn name(&self) -> &str {
        "usage"
    }

    fn description(&self) -> &str {
        "Calls and tokens per account over the last 24 hours"
    }

    fn usage(&self) -> &str {
        "/usage"
    }

    fn execute(&self, _args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        CommandResult::ShowUsage
    }
}
