use crate::app::AppState;
use crate::commands::{save_config_then, Command, CommandResult};
use crate::config::Config;

pub struct RateCommand;

impl Command for RateCommand {
    fn name(&self) -> &str {
        "rate"
    }

    fn description(&self) -> &str {
        "Set rate limit: ms between requests, or max requests/min"
    }

    fn usage(&self) -> &str {
        "/rate <ms> | <N>/min | off  (both may be set independently)"
    }

    fn execute(&self, args: &str, state: &mut AppState, config: &Config) -> CommandResult {
        const USAGE: &str = "Usage: /rate <milliseconds> | <N>/min | off";
        let trimmed = args.trim();

        if trimmed.is_empty() {
            let message = format!(
                "Current rate limit: {}\n\n{}",
                config.agent.rate_limit_display(),
                USAGE
            );
            state.push_system(&message);
            return CommandResult::Handled;
        }

        let mut cfg = config.clone();

        if trimmed.eq_ignore_ascii_case("off") {
            cfg.agent.rate_limit_ms = 0;
            cfg.agent.rate_limit_per_minute = 0;
        } else if let Some(count) = trimmed
            .strip_suffix("/min")
            .or_else(|| trimmed.strip_suffix("rpm"))
        {
            let per_minute: u32 = match count.trim().parse() {
                Ok(v) => v,
                Err(_) => return CommandResult::Error(USAGE.to_string()),
            };
            cfg.agent.rate_limit_per_minute = per_minute;
        } else {
            let ms: u64 = match trimmed.parse() {
                Ok(v) => v,
                Err(_) => return CommandResult::Error(USAGE.to_string()),
            };
            cfg.agent.rate_limit_ms = ms;
        }

        save_config_then(&cfg, || {
            state.push_system(&format!(
                "Rate limit updated: {}",
                cfg.agent.rate_limit_display()
            ));
            CommandResult::ResetProvider
        })
    }
}
