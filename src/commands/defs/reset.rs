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

    fn execute(&self, _args: &str, state: &mut AppState, _config: &Config) -> CommandResult {
        state.messages.clear();
        state.scroll_offset = 0;
        state.input_buffer.clear();
        state.input_cursor = 0;
        state.is_generating = false;
        state.status_message = "Ready".to_string();
        state.error = None;

        state.messages.push(crate::provider::ChatMessage::system(
            "Session reset. How can I help you?"
        ));

        CommandResult::Handled
    }
}
