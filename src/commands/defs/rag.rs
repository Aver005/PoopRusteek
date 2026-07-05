use crate::app::AppState;
use crate::commands::{Command, CommandResult, RagAction};
use crate::config::Config;

/// `/rag` — semantic matching (RAG) control: status/help, on/off switch,
/// full reload. The command only names the intent; the effects (config
/// save, service toggle, re-init) live in the `CommandResult` interpreter,
/// which has the `App`-level access this needs.
pub struct RagCommand;

impl Command for RagCommand {
    fn name(&self) -> &str {
        "rag"
    }

    fn description(&self) -> &str {
        "Semantic matching (RAG): status, on/off, full reload"
    }

    fn usage(&self) -> &str {
        "/rag [on|off|reload]"
    }

    fn execute(&self, args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        parse(args)
    }
}

fn parse(args: &str) -> CommandResult {
    match args.trim().to_ascii_lowercase().as_str() {
        "" | "status" => CommandResult::Rag(RagAction::Status),
        "on" => CommandResult::Rag(RagAction::SetEnabled(true)),
        "off" => CommandResult::Rag(RagAction::SetEnabled(false)),
        "reload" => CommandResult::Rag(RagAction::Reload),
        other => CommandResult::Error(format!(
            "Unknown subcommand '{other}'. Usage: /rag [on|off|reload]"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subcommands_map_to_the_right_actions() {
        assert!(matches!(parse(""), CommandResult::Rag(RagAction::Status)));
        assert!(matches!(parse("status"), CommandResult::Rag(RagAction::Status)));
        assert!(matches!(parse("on"), CommandResult::Rag(RagAction::SetEnabled(true))));
        assert!(matches!(parse("OFF"), CommandResult::Rag(RagAction::SetEnabled(false))));
        assert!(matches!(parse(" reload "), CommandResult::Rag(RagAction::Reload)));
    }

    #[test]
    fn unknown_subcommand_is_a_usage_error() {
        assert!(matches!(parse("frobnicate"), CommandResult::Error(_)));
    }
}
