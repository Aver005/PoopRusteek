use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

pub struct HomeCommand;

impl Command for HomeCommand {
    fn name(&self) -> &str {
        "home"
    }

    fn description(&self) -> &str {
        "Go to home screen"
    }

    fn usage(&self) -> &str {
        "/home"
    }

    fn execute(&self, _args: &str, state: &mut AppState, _config: &Config) -> CommandResult {
        state.focused_mut().messages.clear();
        state.scroll_offset = 0;
        state.autocomplete = Default::default();
        CommandResult::Handled
    }
}
