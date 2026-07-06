use crate::app::AppState;
use crate::app::events::{Modal, PickerItem, PickerMode, PickerState};
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
        let sessions = match session::list_sessions() {
            Ok(s) => s,
            Err(e) => return CommandResult::Error(format!("Failed to list sessions: {e}")),
        };

        // Filter out system sessions (GOAL evaluator sessions)
        let user_sessions: Vec<_> = sessions
            .into_iter()
            .filter(|s| s.tag.as_deref() != Some("__goal_system__"))
            .collect();

        if user_sessions.is_empty() {
            state.push_system("No local sessions found.");
            return CommandResult::Handled;
        }

        let items: Vec<PickerItem> = user_sessions
            .iter()
            .map(|s| {
                let date = s.updated_at.split('T').next().unwrap_or(&s.updated_at);
                let model_tag = if s.model_type.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", s.model_type)
                };
                let broken_marker = if s.broken { "\u{26A0} " } else { "" };
                let text = format!(
                    "{broken_marker}{} [{}, {} msgs{}]",
                    s.title, date, s.message_count, model_tag
                );
                PickerItem::new(&text, s.id.clone()).warn(s.broken)
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
