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
        state.focused_mut().messages.clear();
        state.attached_files.clear();
        state.scroll_offset = 0;
        state.input.buffer.clear();
        state.input.cursor = 0;
        state.input.selection_anchor = None;
        state.autocomplete = Default::default();
        state.focused_mut().generation.active = false;
        state.status_message = "Ready".to_string();

        state.focused_mut().messages.push(crate::provider::ChatMessage::system(
            "Session reset. How can I help you?"
        ));

        CommandResult::ResetProvider
    }
}
