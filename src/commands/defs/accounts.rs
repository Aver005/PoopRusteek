use crate::app::AppState;
use crate::app::gateway_view::Focus;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

/// `/accounts` — главный экран: аккаунты, лимиты, маршруты, последнее решение.
pub struct AccountsCommand;

const USAGE: &str = "/accounts | /accounts add | /accounts enable | /accounts disable";

impl Command for AccountsCommand {
    fn name(&self) -> &str {
        "accounts"
    }

    fn description(&self) -> &str {
        "Accounts, limits and routes (subscriptions, API keys, DeepSeek)"
    }

    fn usage(&self) -> &str {
        USAGE
    }

    fn execute(&self, args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        match args.trim() {
            "" => CommandResult::OpenAccounts(Focus::Accounts),
            "add" => CommandResult::OpenAccountAdd(None),
            "enable" => CommandResult::EnableGateway,
            "disable" => CommandResult::DisableGateway,
            _ => CommandResult::Error(format!("Usage: {USAGE}")),
        }
    }
}
