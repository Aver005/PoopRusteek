use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::{Config, ToolProtocol};

pub struct ProvidersCommand;

const USAGE: &str = "/providers | /providers add | /providers add <name> [openai|anthropic] <base_url> [model] [api_key] | /providers tools <name> <native|prompt>";

impl Command for ProvidersCommand {
    fn name(&self) -> &str {
        "providers"
    }

    fn description(&self) -> &str {
        "Manage LLM providers (built-in DeepSeek + OpenAI-compatible endpoints)"
    }

    fn usage(&self) -> &str {
        USAGE
    }

    fn execute(&self, args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        let args = args.trim();
        if args.is_empty() {
            return CommandResult::OpenProviders;
        }
        if let Some(rest) = args.strip_prefix("add") {
            let rest = rest.trim();
            return CommandResult::OpenProviderAdd((!rest.is_empty()).then(|| rest.to_string()));
        }
        if let Some(rest) = args.strip_prefix("tools") {
            return parse_tools(rest);
        }
        CommandResult::Error(format!("Usage: {USAGE}"))
    }
}

/// `tools <name> <native|prompt>` — как запись объявляет инструменты.
fn parse_tools(rest: &str) -> CommandResult {
    let mut words = rest.split_whitespace();
    let (Some(name), Some(mode), None) = (words.next(), words.next(), words.next()) else {
        return CommandResult::Error("Usage: /providers tools <name> <native|prompt>".to_string());
    };
    let tools = match mode {
        "native" => ToolProtocol::Native,
        "prompt" => ToolProtocol::Prompt,
        other => {
            return CommandResult::Error(format!(
                "Unknown tool protocol '{other}' — use native or prompt"
            ));
        }
    };
    CommandResult::SetProviderTools {
        name: name.to_string(),
        tools,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_subcommand_parses_both_modes_and_rejects_the_rest() {
        assert!(matches!(
            parse_tools(" lm native"),
            CommandResult::SetProviderTools { ref name, tools: ToolProtocol::Native } if name == "lm"
        ));
        assert!(matches!(
            parse_tools("lm prompt"),
            CommandResult::SetProviderTools {
                tools: ToolProtocol::Prompt,
                ..
            }
        ));
        assert!(matches!(parse_tools("lm"), CommandResult::Error(_)));
        assert!(matches!(parse_tools("lm fancy"), CommandResult::Error(_)));
        assert!(matches!(
            parse_tools("lm native x"),
            CommandResult::Error(_)
        ));
    }
}
