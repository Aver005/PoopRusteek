use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

pub struct QuitCommand;

impl Command for QuitCommand {
    fn name(&self) -> &str {
        "quit"
    }

    fn description(&self) -> &str {
        "Exit application"
    }

    fn execute(&self, _args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        std::process::exit(0);
    }
}
