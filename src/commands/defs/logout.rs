use crate::app::AppState;
use crate::app::events::ConfirmAction;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

pub struct LogoutCommand;

impl Command for LogoutCommand {
    fn name(&self) -> &str {
        "logout"
    }

    fn description(&self) -> &str {
        "Log out — remove the saved DeepSeek token"
    }

    fn usage(&self) -> &str {
        "/logout"
    }

    fn execute(&self, _args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        CommandResult::OpenConfirm(ConfirmAction::Logout)
    }
}
