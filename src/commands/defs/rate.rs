use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

pub struct RateCommand;

impl Command for RateCommand {
    fn name(&self) -> &str {
        "rate"
    }

    fn description(&self) -> &str {
        "Set rate limit between requests (ms)"
    }

    fn usage(&self) -> &str {
        "/rate <milliseconds>  (0 to disable)"
    }

    fn execute(&self, args: &str, _state: &mut AppState, config: &Config) -> CommandResult {
        let ms: u64 = match args.trim().parse() {
            Ok(v) => v,
            Err(_) => return CommandResult::Error("Usage: /rate <milliseconds>".to_string()),
        };
        let mut cfg = config.clone();
        cfg.agent.rate_limit_ms = ms;
        if let Err(e) = crate::config::save(&cfg) {
            return CommandResult::Error(format!("Failed to save config: {e}"));
        }
        CommandResult::ResetProvider
    }
}
