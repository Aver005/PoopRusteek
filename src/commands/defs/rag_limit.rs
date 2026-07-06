use crate::app::AppState;
use crate::commands::{Command, CommandResult, RagAction};
use crate::config::{Config, RagLimit};

/// `/rag-limit` — the embedder's batch cap, i.e. the ONNX memory guard.
/// Like `/rag`, the command only names the intent; persisting the config,
/// re-sizing against RAM, and updating the live embedder happen in the
/// `CommandResult` interpreter, which has the `App`-level access this needs.
pub struct RagLimitCommand;

impl Command for RagLimitCommand {
    fn name(&self) -> &str {
        "rag-limit"
    }

    fn description(&self) -> &str {
        "Embedder batch cap (RAM guard): auto, off, or a fixed number"
    }

    fn usage(&self) -> &str {
        "/rag-limit [<N>|auto|off]"
    }

    fn execute(&self, args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        parse(args)
    }
}

const USAGE: &str = "Usage: /rag-limit [<N>|auto|off]  (N ≥ 1)";

fn parse(args: &str) -> CommandResult {
    let trimmed = args.trim();
    match trimmed.to_ascii_lowercase().as_str() {
        "" | "status" => CommandResult::Rag(RagAction::LimitStatus),
        "auto" => CommandResult::Rag(RagAction::SetLimit(RagLimit::Auto)),
        "off" => CommandResult::Rag(RagAction::SetLimit(RagLimit::Off)),
        _ => match trimmed.parse::<usize>() {
            Ok(n) if n >= 1 => CommandResult::Rag(RagAction::SetLimit(RagLimit::Fixed(n))),
            _ => CommandResult::Error(USAGE.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_and_status_show_status() {
        assert!(matches!(
            parse(""),
            CommandResult::Rag(RagAction::LimitStatus)
        ));
        assert!(matches!(
            parse(" status "),
            CommandResult::Rag(RagAction::LimitStatus)
        ));
    }

    #[test]
    fn keywords_map_to_modes() {
        assert!(matches!(
            parse("AUTO"),
            CommandResult::Rag(RagAction::SetLimit(RagLimit::Auto))
        ));
        assert!(matches!(
            parse("off"),
            CommandResult::Rag(RagAction::SetLimit(RagLimit::Off))
        ));
    }

    #[test]
    fn positive_integer_is_a_fixed_cap() {
        assert!(matches!(
            parse("16"),
            CommandResult::Rag(RagAction::SetLimit(RagLimit::Fixed(16)))
        ));
    }

    #[test]
    fn zero_and_garbage_are_usage_errors() {
        assert!(matches!(parse("0"), CommandResult::Error(_)));
        assert!(matches!(parse("-4"), CommandResult::Error(_)));
        assert!(matches!(parse("lots"), CommandResult::Error(_)));
    }
}
