use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;
use crate::session;

pub struct SessionInfoCommand;

impl Command for SessionInfoCommand {
    fn name(&self) -> &str {
        "session"
    }

    fn description(&self) -> &str {
        "Show current session info"
    }

    fn usage(&self) -> &str {
        "/session"
    }

    fn execute(&self, _args: &str, state: &mut AppState, config: &Config) -> CommandResult {
        let id = &state.current_session_id;
        let count = state.messages.len();
        let model = &config.provider.model;
        let info = format!(
            "ID: {id}\nMessages: {count}\nModel: {model}\nType: local"
        );

        let title = if count > 0 {
            session::derive_title(&state.messages)
        } else {
            "Empty conversation".to_string()
        };

        state.messages.push(crate::provider::ChatMessage::system(&format!(
            "---\nSession: {id}\nTitle: {title}\n{info}\n---"
        )));
        CommandResult::Handled
    }
}
