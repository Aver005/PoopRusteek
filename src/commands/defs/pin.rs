use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

/// `/pin <аккаунт>` — чат на один аккаунт, маршрут не меняется; `/pin off` отпускает.
pub struct PinCommand;

impl Command for PinCommand {
    fn name(&self) -> &str {
        "pin"
    }

    fn description(&self) -> &str {
        "Keep this chat on one account without changing the route"
    }

    fn usage(&self) -> &str {
        "/pin <account>[/model] | /pin off"
    }

    fn execute(&self, args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        CommandResult::Pin(args.trim().to_string())
    }
}
