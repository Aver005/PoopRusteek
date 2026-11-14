use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

pub struct HomeCommand;

impl Command for HomeCommand {
    fn name(&self) -> &str {
        "home"
    }

    fn description(&self) -> &str {
        "Go to home screen"
    }

    fn usage(&self) -> &str {
        "/home"
    }

    fn execute(&self, _args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        CommandResult::GoHome
    }
}
