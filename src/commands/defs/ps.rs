use crate::app::AppState;
use crate::commands::{Command, CommandResult, JobCommandAction};
use crate::config::Config;

pub struct PsCommand;

impl Command for PsCommand {
    fn name(&self) -> &str {
        "ps"
    }

    fn description(&self) -> &str {
        "Alias for /jobs list"
    }

    fn usage(&self) -> &str {
        "/ps"
    }

    fn execute(&self, _args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        CommandResult::Jobs(JobCommandAction::List)
    }
}
