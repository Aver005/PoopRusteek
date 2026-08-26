use crate::app::AppState;
use crate::commands::{Command, CommandResult, save_config_then};
use crate::config::{Config, ContextConfig};

/// `/default-compact` — the mode `/compact` uses in a chat that has not
/// picked one of its own (`[context] compact_mode`). Unlike `/compact`, this
/// only writes config: no model call, nothing to interpret later.
pub struct DefaultCompactCommand;

impl Command for DefaultCompactCommand {
    fn name(&self) -> &str {
        "default-compact"
    }

    fn description(&self) -> &str {
        "Default /compact mode for chats that haven't picked one (1, 2 or 3)"
    }

    fn usage(&self) -> &str {
        "/default-compact [1|2|3]"
    }

    fn execute(&self, args: &str, state: &mut AppState, config: &Config) -> CommandResult {
        let mode = match parse_mode(args) {
            Ok(Some(mode)) => mode,
            // No argument: report the current default instead of erroring.
            Ok(None) => {
                let message = status_message(config);
                state.push_system(&message);
                return CommandResult::Handled;
            }
            Err(usage) => return CommandResult::Error(usage),
        };

        let mut cfg = config.clone();
        cfg.context.compact_mode = mode;

        save_config_then(&cfg, || {
            state.push_system(&format!(
                "Default compact mode updated: {mode} — {}",
                mode_description(mode)
            ));
            CommandResult::Handled
        })
    }
}

const USAGE: &str = "Usage: /default-compact [1|2|3]";

/// `Ok(None)` = no argument (report the current default), `Ok(Some(m))` = set
/// mode `m`, `Err` = the usage error to show.
fn parse_mode(args: &str) -> Result<Option<u8>, String> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    match trimmed.parse::<u8>() {
        Ok(mode) if (1..=ContextConfig::MAX_COMPACT_MODE).contains(&mode) => Ok(Some(mode)),
        _ => Err(format!("{USAGE}\n{}", modes_list())),
    }
}

fn status_message(config: &Config) -> String {
    let current = config.context.effective_compact_mode();
    format!(
        "Default compact mode: {current} — {}\n\n{}\n{USAGE}",
        mode_description(current),
        modes_list()
    )
}

fn mode_description(mode: u8) -> &'static str {
    match mode {
        2 => "summarise the oldest half in chunks, keep the rest verbatim",
        3 => "summarise everything before the last turn",
        _ => "keep the first and last turn verbatim, summarise the middle",
    }
}

fn modes_list() -> String {
    (1..=ContextConfig::MAX_COMPACT_MODE)
        .map(|m| format!("  {m} — {}", mode_description(m)))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_argument_reports_the_current_default_instead_of_failing() {
        assert_eq!(parse_mode(""), Ok(None));
        assert_eq!(parse_mode("   "), Ok(None));

        let mut config = Config::default();
        assert!(status_message(&config).contains("Default compact mode: 1"));
        config.context.compact_mode = 2;
        assert!(status_message(&config).contains("Default compact mode: 2"));
        // A stale out-of-range value reports the fallback, not itself.
        config.context.compact_mode = 7;
        assert!(status_message(&config).contains("Default compact mode: 1"));
    }

    #[test]
    fn every_valid_mode_is_accepted() {
        assert_eq!(parse_mode("1"), Ok(Some(1)));
        assert_eq!(parse_mode(" 2 "), Ok(Some(2)));
        assert_eq!(parse_mode("3"), Ok(Some(3)));
    }

    #[test]
    fn out_of_range_and_garbage_are_usage_errors() {
        for bad in ["0", "4", "abc", "-1"] {
            assert!(parse_mode(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn every_mode_has_its_own_description() {
        assert_ne!(mode_description(1), mode_description(2));
        assert_ne!(mode_description(2), mode_description(3));
        assert_eq!(
            modes_list().lines().count(),
            ContextConfig::MAX_COMPACT_MODE as usize
        );
    }
}
