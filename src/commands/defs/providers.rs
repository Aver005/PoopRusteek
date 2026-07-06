use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

pub struct ProvidersCommand;

const USAGE: &str = "/providers | /providers add | /providers add <name> [openai|anthropic] <base_url> [model] [api_key]";

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
        CommandResult::Error(format!("Usage: {USAGE}"))
    }
}
