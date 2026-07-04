use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

pub struct ResetCommand;

impl Command for ResetCommand {
    fn name(&self) -> &str {
        "reset"
    }

    fn description(&self) -> &str {
        "Reset session completely"
    }

    fn usage(&self) -> &str {
        "/reset"
    }

    fn execute(&self, _args: &str, state: &mut AppState, _config: &Config) -> CommandResult {
        state.clear_chat_view();
        state.attached_files.clear();
        state.input.buffer.clear();
        state.input.cursor = 0;
        state.input.selection_anchor = None;
        state.focused_mut().generation.active = false;
        state.status_message = "Ready".to_string();

        state.push_system("Session reset. How can I help you?");

        CommandResult::ResetProvider
    }
}
