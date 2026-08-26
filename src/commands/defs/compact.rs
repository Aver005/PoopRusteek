use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::{Config, ContextConfig};

/// `/compact` — run the compaction ladder by hand. The command only names
/// the intent: the summary needs a model call, which a synchronous command
/// cannot make, so the work happens in the `CommandResult` interpreter.
pub struct CompactCommand;

impl Command for CompactCommand {
    fn name(&self) -> &str {
        "compact"
    }

    fn description(&self) -> &str {
        "Compact context: 1 = ends verbatim, 2 = chunked, 3 = all but last turn"
    }

    fn usage(&self) -> &str {
        "/compact [1|2|3]"
    }

    fn execute(&self, args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        parse(args)
    }
}

const USAGE: &str = "Usage: /compact [1|2|3]  \
     (1 = first and last turn verbatim, 2 = summarise the oldest half in chunks, \
     3 = everything before the last turn)";

fn parse(args: &str) -> CommandResult {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return CommandResult::Compact(None);
    }
    match trimmed.parse::<u8>() {
        Ok(mode) if (1..=ContextConfig::MAX_COMPACT_MODE).contains(&mode) => {
            CommandResult::Compact(Some(mode))
        }
        _ => CommandResult::Error(USAGE.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_command_runs_with_the_chats_own_mode() {
        assert!(matches!(parse(""), CommandResult::Compact(None)));
        assert!(matches!(parse("   "), CommandResult::Compact(None)));
    }

    #[test]
    fn every_valid_mode_is_accepted() {
        assert!(matches!(parse("1"), CommandResult::Compact(Some(1))));
        assert!(matches!(parse(" 2 "), CommandResult::Compact(Some(2))));
        assert!(matches!(parse("3"), CommandResult::Compact(Some(3))));
    }

    #[test]
    fn out_of_range_and_garbage_are_usage_errors() {
        for bad in ["0", "4", "abc", "1 2", "-1"] {
            match parse(bad) {
                CommandResult::Error(msg) => assert!(msg.contains("1|2|3"), "{bad}: {msg}"),
                _ => panic!("{bad:?} should be rejected"),
            }
        }
    }
}
