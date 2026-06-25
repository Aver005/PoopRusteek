use crate::app::events::{Modal, PickerItem, PickerMode, PickerState};
use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;
use crate::session;

pub struct SessionListCommand;

impl Command for SessionListCommand {
    fn name(&self) -> &str {
        "sessions"
    }

    fn description(&self) -> &str {
        "List local sessions"
    }

    fn usage(&self) -> &str {
        "/sessions"
    }

    fn execute(&self, _args: &str, state: &mut AppState, _config: &Config) -> CommandResult {
        let sessions = match session::list_sessions(_config) {
            Ok(s) => s,
            Err(e) => return CommandResult::Error(format!("Failed to list sessions: {e}")),
        };

        if sessions.is_empty() {
            state.messages.push(crate::provider::ChatMessage::system(
                "No local sessions found.",
            ));
            return CommandResult::Handled;
        }

        let items: Vec<PickerItem> = sessions
            .iter()
            .map(|s| {
                let date = s.updated_at.split('T').next().unwrap_or(&s.updated_at);
                let text = format!("{} [{}, {} msgs]", s.title, date, s.message_count);
                PickerItem::new(&text, s.id.clone())
            })
            .collect();

        state.modal = Some(Modal::Picker(PickerState::new(
            "\u{1F4C2} Sessions",
            items,
            PickerMode::Single,
        )));
        CommandResult::Handled
    }
}
