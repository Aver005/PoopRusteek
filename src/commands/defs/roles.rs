use crate::app::AppState;
use crate::app::gateway_view::Focus;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

/// `/roles` — какой маршрут у главной модели, заголовков, сводок и оценщика.
pub struct RolesCommand;

impl Command for RolesCommand {
    fn name(&self) -> &str {
        "roles"
    }

    fn description(&self) -> &str {
        "Which route serves chat, titles, summaries and the evaluator"
    }

    fn usage(&self) -> &str {
        "/roles"
    }

    fn execute(&self, _args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        CommandResult::OpenAccounts(Focus::Roles)
    }
}
