use crate::app::AppState;
use crate::commands::{with_args, Command, CommandResult};
use crate::config::Config;

/// `/search <query>` — semantic + keyword search across the indexed
/// message history of every saved session. The search itself runs off the
/// event loop; results flush back into the chat as a UI-only message.
pub struct SearchCommand;

impl Command for SearchCommand {
    fn name(&self) -> &str {
        "search"
    }

    fn description(&self) -> &str {
        "Search past conversations (semantic + keyword)"
    }

    fn usage(&self) -> &str {
        "/search <query>"
    }

    fn execute(&self, args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        with_args(args, self.usage(), |query| {
            CommandResult::SearchHistory(query.to_string())
        })
    }
}
