use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

/// `/new` — open a fresh parallel chat (its own session) and focus it. The
/// current chat keeps running in the background.
pub struct NewChatCommand;

impl Command for NewChatCommand {
    fn name(&self) -> &str {
        "new"
    }

    fn description(&self) -> &str {
        "Open a new parallel chat and switch to it"
    }

    fn usage(&self) -> &str {
        "/new"
    }

    fn execute(&self, _args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        CommandResult::NewChat
    }
}

/// `/chats` — open the switcher to jump between parallel chats (Tab/Shift+Tab
/// also cycle them). Background chats keep streaming.
pub struct ChatsCommand;

impl Command for ChatsCommand {
    fn name(&self) -> &str {
        "chats"
    }

    fn description(&self) -> &str {
        "Switch between parallel chats"
    }

    fn usage(&self) -> &str {
        "/chats"
    }

    fn execute(&self, _args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        CommandResult::OpenChats
    }
}
