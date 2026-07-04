use crate::app::AppState;
use crate::commands::{save_config_then, Command, CommandResult};
use crate::config::Config;

pub struct RetryCommand;

impl Command for RetryCommand {
    fn name(&self) -> &str {
        "retry"
    }

    fn description(&self) -> &str {
        "Set max retries on request failure"
    }

    fn usage(&self) -> &str {
        "/retry <number|on|off|-1>  (-1 = infinite, 0/off = disabled)"
    }

    fn execute(&self, args: &str, _state: &mut AppState, config: &Config) -> CommandResult {
        let trimmed = args.trim().to_ascii_lowercase();
        let n: i32 = match trimmed.as_str() {
            "off" | "0" => 0,
            "on" | "" | "-1" => -1,
            s => match s.parse() {
                Ok(v) if v >= -1 => v,
                _ => {
                    return CommandResult::Error(
                        "Usage: /retry <number|on|off|-1>".to_string(),
                    )
                }
            },
        };
        let mut cfg = config.clone();
        cfg.agent.max_retries = n;
        save_config_then(&cfg, || CommandResult::ResetProvider)
    }
}
