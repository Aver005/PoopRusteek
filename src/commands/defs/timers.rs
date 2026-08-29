use crate::app::AppState;
use crate::commands::{Command, CommandResult, TimerCommandAction};
use crate::config::Config;

pub struct TimersCommand;

impl Command for TimersCommand {
    fn name(&self) -> &str {
        "timers"
    }

    fn description(&self) -> &str {
        "Show or cancel the agent's pending timers"
    }

    fn usage(&self) -> &str {
        "/timers [list|cancel <id>]"
    }

    fn execute(&self, args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        let trimmed = args.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("list") {
            return CommandResult::Timers(TimerCommandAction::List);
        }

        let mut parts = trimmed.split_whitespace();
        match parts.next().unwrap_or_default() {
            "cancel" => {
                let Some(id_raw) = parts.next() else {
                    return CommandResult::Error("Usage: /timers cancel <id>".to_string());
                };
                match id_raw.parse::<u64>() {
                    Ok(id) => CommandResult::Timers(TimerCommandAction::Cancel(id)),
                    Err(_) => CommandResult::Error(format!("Invalid timer id: {id_raw}")),
                }
            }
            other => CommandResult::Error(format!(
                "Unknown /timers action: {other}. Use `/timers` or `/timers cancel <id>`."
            )),
        }
    }
}
