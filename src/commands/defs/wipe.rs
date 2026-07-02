use crate::app::AppState;
use crate::app::events::ConfirmAction;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

pub struct WipeCommand;

impl Command for WipeCommand {
    fn name(&self) -> &str {
        "wipe"
    }

    fn description(&self) -> &str {
        "Factory reset — delete ALL local Pooprusteek data"
    }

    fn usage(&self) -> &str {
        "/wipe"
    }

    fn execute(&self, _args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        CommandResult::OpenConfirm(ConfirmAction::Wipe)
    }
}
