use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

pub struct DebugCommand;

impl Command for DebugCommand {
    fn name(&self) -> &str {
        "debug"
    }

    fn description(&self) -> &str {
        "Toggle debug logging to .dev/debug.log"
    }

    fn usage(&self) -> &str {
        "/debug [on|off]  (no args = switch)"
    }

    fn execute(&self, args: &str, state: &mut AppState, _config: &Config) -> CommandResult {
        let trimmed = args.trim().to_ascii_lowercase();
        let target = match trimmed.as_str() {
            "on" => true,
            "off" => false,
            "" => !crate::debug_log::is_enabled(),
            _ => return CommandResult::Error("Usage: /debug [on|off]".to_string()),
        };

        if let Err(e) = crate::debug_log::set_enabled(target) {
            return CommandResult::Error(format!("Failed to toggle debug logging: {e}"));
        }

        let message = if target {
            "Debug logging enabled — writing to .dev/debug.log"
        } else {
            "Debug logging disabled"
        };
        state
            .focused_mut()
            .messages
            .push(crate::provider::ChatMessage::system(message));

        CommandResult::Handled
    }
}
