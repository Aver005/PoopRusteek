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

        let title = if count > 0 {
            session::derive_title(&state.messages)
        } else {
            "Empty conversation".to_string()
        };

        let msg_stats = if count > 0 {
            let user_msgs = state.messages.iter().filter(|m| m.role == crate::provider::Role::User).count();
            let asst_msgs = state.messages.iter().filter(|m| m.role == crate::provider::Role::Assistant).count();
            let tool_msgs = state.messages.iter().filter(|m| m.role == crate::provider::Role::Tool).count();
            let finished = state.messages.iter().filter(|m| m.status.as_deref() == Some("FINISHED")).count();
            let aborted = state.messages.iter().filter(|m| m.status.as_deref() == Some("ABORTED")).count();
            let think_total: f64 = state.messages.iter()
                .filter(|m| m.role == crate::provider::Role::Assistant)
                .map(|m| m.think_elapsed_secs)
                .sum();
            let total_tokens: u32 = state.messages.iter()
                .filter(|m| m.role == crate::provider::Role::Assistant)
                .flat_map(|m| m.total_tokens)
                .sum();
            format!(
                "Messages: {count} (U:{user_msgs} A:{asst_msgs} T:{tool_msgs})
Status: {finished} finished, {aborted} aborted
Tokens: {total_tokens} total
Think: {think_total:.1}s total
Session model: {}",
                state.last_model_name
            )
        } else {
            "Empty conversation".to_string()
        };

        state.messages.push(crate::provider::ChatMessage::system(&format!(
            "---\nSession: {id}\nTitle: {title}\nModel: {model}\nType: local\n{msg_stats}\n---"
        )));
        CommandResult::Handled
    }
}
