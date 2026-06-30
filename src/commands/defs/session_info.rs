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
        use crate::provider::Role;

        let conv = state.focused();
        let id = conv.session_id.clone();
        let count = conv.messages.len();
        let model = config.provider.model.clone();

        let title = if count > 0 {
            session::derive_title(&conv.messages)
        } else {
            "Empty conversation".to_string()
        };

        let msg_stats = if count > 0 {
            let user_msgs = conv.messages.iter().filter(|m| m.role == Role::User).count();
            let asst_msgs = conv.messages.iter().filter(|m| m.role == Role::Assistant).count();
            let tool_msgs = conv.messages.iter().filter(|m| m.role == Role::Tool).count();
            let finished = conv.messages.iter().filter(|m| m.status.as_deref() == Some("FINISHED")).count();
            let aborted = conv.messages.iter().filter(|m| m.status.as_deref() == Some("ABORTED")).count();
            let think_total: f64 = conv.messages.iter()
                .filter(|m| m.role == Role::Assistant)
                .map(|m| m.think_elapsed_secs)
                .sum();
            let total_tokens: u32 = conv.messages.iter()
                .filter(|m| m.role == Role::Assistant)
                .flat_map(|m| m.total_tokens)
                .sum();
            format!(
                "Messages: {count} (U:{user_msgs} A:{asst_msgs} T:{tool_msgs})
Status: {finished} finished, {aborted} aborted
Tokens: {total_tokens} total
Think: {think_total:.1}s total
Session model: {}",
                conv.generation.last_model
            )
        } else {
            "Empty conversation".to_string()
        };

        state.push_system(&format!(
            "---\nSession: {id}\nTitle: {title}\nModel: {model}\nType: local\n{msg_stats}\n---"
        ));
        CommandResult::Handled
    }
}
