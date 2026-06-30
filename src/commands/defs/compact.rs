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

    fn usage(&self) -> &str {
        "/compact"
    }

    fn execute(&self, _args: &str, state: &mut AppState, _config: &Config) -> CommandResult {
        if state.focused_mut().messages.is_empty() {
            state.focused_mut().messages.push(crate::provider::ChatMessage::system("Nothing to compact."));
            return CommandResult::Handled;
        }

        let (count, topics) = {
            let msgs = &state.focused().messages;
            let topics = msgs
                .iter()
                .filter(|m| m.role == crate::provider::Role::User)
                .map(|m| m.content.clone())
                .collect::<Vec<_>>()
                .join("; ");
            (msgs.len(), topics)
        };

        let summary = format!(
            "[Context compacted] Previous conversation had {count} messages. User topics: {topics}"
        );

        state.focused_mut().messages.clear();
        state.push_system(&summary);
        state.scroll_offset = 0;

        CommandResult::Handled
    }
}
