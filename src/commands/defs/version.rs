use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

pub struct VersionCommand;

impl Command for VersionCommand {
    fn name(&self) -> &str {
        "version"
    }

    fn description(&self) -> &str {
        "Show version info"
    }

    fn execute(&self, _args: &str, state: &mut AppState, _config: &Config) -> CommandResult {
        let version = env!("CARGO_PKG_VERSION");
        let info = format!(
            "Pooprusteek v{version}\nRust TUI coding agent powered by DeepSeek"
        );
        state.messages.push(crate::provider::ChatMessage::system(&info));
        CommandResult::Handled
    }
}
