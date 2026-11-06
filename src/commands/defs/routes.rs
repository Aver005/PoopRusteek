use crate::app::AppState;
use crate::app::gateway_view::Focus;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

/// `/routes` — тот же экран, фокус на цепочках маршрутов.
pub struct RoutesCommand;

impl Command for RoutesCommand {
    fn name(&self) -> &str {
        "routes"
    }

    fn description(&self) -> &str {
        "Route chains: which account answers first, and who is next"
    }

    fn usage(&self) -> &str {
        "/routes"
    }

    fn execute(&self, _args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        CommandResult::OpenAccounts(Focus::Routes)
    }
}
