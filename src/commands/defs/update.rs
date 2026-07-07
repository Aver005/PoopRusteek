use crate::app::AppState;
use crate::commands::{Command, CommandResult, UpdateAction};
use crate::config::Config;

/// `/update` — check the rolling `latest` release and self-update on hash
/// mismatch. The command only names the intent; the effects (network check,
/// binary swap, in-flight guard) live in the `CommandResult` interpreter,
/// which has the `App`-level access this needs.
pub struct UpdateCommand;

impl Command for UpdateCommand {
    fn name(&self) -> &str {
        "update"
    }

    fn description(&self) -> &str {
        "Self-update from the latest dev release"
    }

    fn usage(&self) -> &str {
        "/update"
    }

    fn execute(&self, _args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        CommandResult::Update(UpdateAction::Run)
    }
}

/// `/autoupdate` — the startup auto-update switch (`[update] auto`).
pub struct AutoUpdateCommand;

impl Command for AutoUpdateCommand {
    fn name(&self) -> &str {
        "autoupdate"
    }

    fn description(&self) -> &str {
        "Auto-update on startup: status, on/off"
    }

    fn usage(&self) -> &str {
        "/autoupdate [on|off]"
    }

    fn execute(&self, args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        parse_autoupdate(args)
    }
}

fn parse_autoupdate(args: &str) -> CommandResult {
    match args.trim().to_ascii_lowercase().as_str() {
        "" | "status" => CommandResult::Update(UpdateAction::AutoStatus),
        "on" => CommandResult::Update(UpdateAction::SetAuto(true)),
        "off" => CommandResult::Update(UpdateAction::SetAuto(false)),
        other => CommandResult::Error(format!(
            "Unknown subcommand '{other}'. Usage: /autoupdate [on|off]"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autoupdate_subcommands_map_to_the_right_actions() {
        assert!(matches!(
            parse_autoupdate(""),
            CommandResult::Update(UpdateAction::AutoStatus)
        ));
        assert!(matches!(
            parse_autoupdate("status"),
            CommandResult::Update(UpdateAction::AutoStatus)
        ));
        assert!(matches!(
            parse_autoupdate("on"),
            CommandResult::Update(UpdateAction::SetAuto(true))
        ));
        assert!(matches!(
            parse_autoupdate("OFF"),
            CommandResult::Update(UpdateAction::SetAuto(false))
        ));
    }

    #[test]
    fn autoupdate_unknown_subcommand_is_a_usage_error() {
        assert!(matches!(
            parse_autoupdate("hourly"),
            CommandResult::Error(_)
        ));
    }
}
