use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

/// `/search [query]` — open the history-search screen (`View::Search`):
/// semantic + keyword search across every saved session, with sort/filter
/// controls. With a query the lookup starts immediately; bare `/search`
/// opens the screen with an empty query line.
pub struct SearchCommand;

impl Command for SearchCommand {
    fn name(&self) -> &str {
        "search"
    }

    fn description(&self) -> &str {
        "Search past conversations (semantic + keyword)"
    }

    fn usage(&self) -> &str {
        "/search [query]"
    }

    fn execute(&self, args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        CommandResult::SearchHistory(args.trim().to_string())
    }
}
