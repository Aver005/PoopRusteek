use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

/// `/btw <question>` — ask a quick one-shot question in an isolated background
/// sidechat (its own forked session) without interrupting the current turn. The
/// answer is appended to the chat when it finishes.
pub struct BtwCommand;

impl Command for BtwCommand {
    fn name(&self) -> &str {
        "btw"
    }

    fn description(&self) -> &str {
        "Ask a quick one-shot side-question in the background"
    }

    fn usage(&self) -> &str {
        "/btw <question>"
    }

    fn execute(&self, args: &str, state: &mut AppState, _config: &Config) -> CommandResult {
        let question = args.trim();
        if question.is_empty() {
            state.focused_mut().messages.push(crate::provider::ChatMessage::system(
                "Usage: /btw <question>",
            ));
            return CommandResult::Handled;
        }
        CommandResult::Sidechat(question.to_string())
    }
}
