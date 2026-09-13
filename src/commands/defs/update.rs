use crate::app::AppState;
use crate::commands::{Command, CommandResult, UpdateAction};
use crate::config::{Config, UpdateChannel};

/// `/update` — check the configured channel and self-update. The command only
/// names the intent; the effects (network check, binary swap, in-flight guard)
/// live in the `CommandResult` interpreter, which has the `App`-level access
/// this needs.
pub struct UpdateCommand;

impl Command for UpdateCommand {
    fn name(&self) -> &str {
        "update"
    }

    fn description(&self) -> &str {
        "Self-update; `channel stable|dev` picks the release channel"
    }

    fn usage(&self) -> &str {
        "/update [channel [stable|dev]]"
    }

    fn execute(&self, args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        parse_update(args)
    }
}

fn parse_update(args: &str) -> CommandResult {
    let mut words = args.split_whitespace();
    match (words.next(), words.next(), words.next()) {
        (None, _, _) => CommandResult::Update(UpdateAction::Run),
        (Some(word), None, _) if word.eq_ignore_ascii_case("channel") => {
            CommandResult::Update(UpdateAction::ChannelStatus)
        }
        (Some(word), Some(value), None) if word.eq_ignore_ascii_case("channel") => {
            match UpdateChannel::parse(value) {
                Some(channel) => CommandResult::Update(UpdateAction::SetChannel(channel)),
                None => CommandResult::Error(format!(
                    "Unknown channel '{value}'. Usage: /update channel [stable|dev]"
                )),
            }
        }
        _ => CommandResult::Error("Usage: /update [channel [stable|dev]]".to_string()),
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
    fn update_arguments_map_to_the_right_actions() {
        assert!(matches!(
            parse_update(""),
            CommandResult::Update(UpdateAction::Run)
        ));
        assert!(matches!(
            parse_update("channel"),
            CommandResult::Update(UpdateAction::ChannelStatus)
        ));
        assert!(matches!(
            parse_update("Channel DEV"),
            CommandResult::Update(UpdateAction::SetChannel(UpdateChannel::Dev))
        ));
        assert!(matches!(
            parse_update("channel stable"),
            CommandResult::Update(UpdateAction::SetChannel(UpdateChannel::Stable))
        ));
    }

    #[test]
    fn update_bad_arguments_are_usage_errors() {
        for bad in ["now", "channel nightly", "channel dev extra"] {
            assert!(
                matches!(parse_update(bad), CommandResult::Error(_)),
                "{bad:?} must be rejected"
            );
        }
    }

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
