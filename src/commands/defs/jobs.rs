use crate::app::AppState;
use crate::commands::{Command, CommandResult, JobCommandAction};
use crate::config::Config;

pub struct JobsCommand;

impl Command for JobsCommand {
    fn name(&self) -> &str {
        "jobs"
    }

    fn description(&self) -> &str {
        "Manage background jobs"
    }

    fn usage(&self) -> &str {
        "/jobs [list|kill <id>|prune]"
    }

    fn execute(&self, args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        let trimmed = args.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("list") {
            return CommandResult::Jobs(JobCommandAction::List);
        }

        let mut parts = trimmed.split_whitespace();
        match parts.next().unwrap_or_default() {
            "kill" => {
                let Some(id_raw) = parts.next() else {
                    return CommandResult::Error("Usage: /jobs kill <id>".to_string());
                };
                match id_raw.parse::<u64>() {
                    Ok(id) => CommandResult::Jobs(JobCommandAction::Kill(id)),
                    Err(_) => CommandResult::Error(format!("Invalid job id: {id_raw}")),
                }
            }
            "prune" => CommandResult::Jobs(JobCommandAction::Prune),
            other => CommandResult::Error(format!(
                "Unknown /jobs action: {other}. Use `/jobs`, `/jobs kill <id>`, or `/jobs prune`."
            )),
        }
    }
}
