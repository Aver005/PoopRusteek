//! `/refetch-providers <ms|off>` — how often provider model lists are
//! re-fetched in the background — and `/cache-providers <ms|off>` — how
//! long a persisted list stays valid across restarts. Parsing only; the
//! effects (config save, timer reset, status text) live in
//! `app/providers.rs::apply_provider_models_action` behind
//! `CommandResult::ProviderModels`.

use crate::app::AppState;
use crate::commands::{Command, CommandResult, ProviderModelsAction};
use crate::config::Config;

/// Shared argument shape: empty = show, `off` = 0, otherwise milliseconds.
/// Pure — both commands and the tests go through this.
fn parse_ms_or_off(args: &str, usage: &str) -> Result<Option<u64>, String> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.eq_ignore_ascii_case("off") {
        return Ok(Some(0));
    }
    trimmed
        .parse::<u64>()
        .map(Some)
        .map_err(|_| format!("'{trimmed}' is not a duration in ms. Usage: {usage}"))
}

fn to_result(
    parsed: Result<Option<u64>, String>,
    set: impl FnOnce(u64) -> ProviderModelsAction,
) -> CommandResult {
    match parsed {
        Ok(None) => CommandResult::ProviderModels(ProviderModelsAction::Show),
        Ok(Some(ms)) => CommandResult::ProviderModels(set(ms)),
        Err(message) => CommandResult::Error(message),
    }
}

pub struct RefetchProvidersCommand;

impl Command for RefetchProvidersCommand {
    fn name(&self) -> &str {
        "refetch-providers"
    }

    fn description(&self) -> &str {
        "Set how often provider model lists are re-fetched"
    }

    fn usage(&self) -> &str {
        "/refetch-providers <ms|off>"
    }

    fn execute(&self, args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        to_result(
            parse_ms_or_off(args, self.usage()),
            ProviderModelsAction::SetRefetch,
        )
    }
}

pub struct CacheProvidersCommand;

impl Command for CacheProvidersCommand {
    fn name(&self) -> &str {
        "cache-providers"
    }

    fn description(&self) -> &str {
        "Set how long fetched provider model lists stay valid"
    }

    fn usage(&self) -> &str {
        "/cache-providers <ms|off>"
    }

    fn execute(&self, args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        to_result(
            parse_ms_or_off(args, self.usage()),
            ProviderModelsAction::SetCache,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ms_off_and_bare() {
        assert_eq!(parse_ms_or_off("", "u"), Ok(None));
        assert_eq!(parse_ms_or_off("  ", "u"), Ok(None));
        assert_eq!(parse_ms_or_off("off", "u"), Ok(Some(0)));
        assert_eq!(parse_ms_or_off("OFF", "u"), Ok(Some(0)));
        assert_eq!(parse_ms_or_off("180000", "u"), Ok(Some(180_000)));
        assert!(parse_ms_or_off("3m", "u").is_err());
        assert!(parse_ms_or_off("-5", "u").is_err());
    }
}
