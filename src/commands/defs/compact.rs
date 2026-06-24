use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

pub struct CompactCommand;

impl Command for CompactCommand {
    fn name(&self) -> &str {
        "compact"
    }

    fn description(&self) -> &str {
        "Compact context by summarizing history"
    }

    fn execute(&self, _args: &str, state: &mut AppState, _config: &Config) -> CommandResult {
        if state.messages.is_empty() {
            state.messages.push(crate::provider::ChatMessage::system("Nothing to compact."));
            return CommandResult::Handled;
        }

        let user_msgs: Vec<&str> = state.messages
            .iter()
            .filter(|m| m.role == crate::provider::Role::User)
            .map(|m| m.content.as_str())
            .collect();

        let summary = format!(
            "[Context compacted] Previous conversation had {} messages. User topics: {}",
            state.messages.len(),
            user_msgs.join("; ")
        );

        state.messages.clear();
        state.messages.push(crate::provider::ChatMessage::system(&summary));
        state.scroll_offset = 0;

        CommandResult::Handled
    }
}
