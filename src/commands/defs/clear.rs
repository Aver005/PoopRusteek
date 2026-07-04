use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

pub struct ClearCommand;

impl Command for ClearCommand {
    fn name(&self) -> &str {
        "clear"
    }

    fn description(&self) -> &str {
        "Clear chat history"
    }

    fn usage(&self) -> &str {
        "/clear"
    }

    fn execute(&self, _args: &str, state: &mut AppState, _config: &Config) -> CommandResult {
        state.clear_chat_view();
        state.attached_files.clear();
        CommandResult::Handled
    }
}
