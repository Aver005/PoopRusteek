use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

pub struct WhitelistCommand;

impl Command for WhitelistCommand {
    fn name(&self) -> &str {
        "whitelist"
    }

    fn description(&self) -> &str {
        "Manage tool whitelist"
    }

    fn usage(&self) -> &str {
        "/whitelist"
    }

    fn execute(&self, _args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        CommandResult::OpenWhitelist
    }
}
