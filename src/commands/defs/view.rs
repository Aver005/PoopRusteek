//! `/panel` и `/landing` — вид экрана. Сами команды только разбирают аргумент:
//! состояние, конфиг и запись меняет интерпретатор (`App::set_panel`, `App::set_landing`).

use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::{Config, LandingStyle, PanelMode};

pub struct PanelCommand;

impl Command for PanelCommand {
    fn name(&self) -> &str {
        "panel"
    }

    fn description(&self) -> &str {
        "Show, hide or auto-size the side panel (Ctrl+B toggles)"
    }

    fn usage(&self) -> &str {
        "/panel [on|off|auto]"
    }

    fn execute(&self, args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        match args.trim().to_ascii_lowercase().as_str() {
            "" => CommandResult::SetPanel(None),
            "on" | "show" | "shown" => CommandResult::SetPanel(Some(PanelMode::Shown)),
            "off" | "hide" | "hidden" => CommandResult::SetPanel(Some(PanelMode::Hidden)),
            "auto" => CommandResult::SetPanel(Some(PanelMode::Auto)),
            _ => CommandResult::Error(format!("Usage: {}", self.usage())),
        }
    }
}

pub struct LandingCommand;

impl Command for LandingCommand {
    fn name(&self) -> &str {
        "landing"
    }

    fn description(&self) -> &str {
        "Switch the home screen: center (input in the middle) or sheet"
    }

    fn usage(&self) -> &str {
        "/landing [center|sheet]"
    }

    fn execute(&self, args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        match args.trim().to_ascii_lowercase().as_str() {
            "" => CommandResult::SetLanding(None),
            "center" => CommandResult::SetLanding(Some(LandingStyle::Center)),
            "sheet" => CommandResult::SetLanding(Some(LandingStyle::Sheet)),
            _ => CommandResult::Error(format!("Usage: {}", self.usage())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::conversation::Conversation;

    fn run(command: &dyn Command, args: &str) -> CommandResult {
        let mut state = AppState::new(Conversation::fresh_main(None));
        command.execute(args, &mut state, &Config::default())
    }

    #[test]
    fn panel_arguments_map_to_modes() {
        assert!(matches!(
            run(&PanelCommand, ""),
            CommandResult::SetPanel(None)
        ));
        assert!(matches!(
            run(&PanelCommand, " OFF "),
            CommandResult::SetPanel(Some(PanelMode::Hidden))
        ));
        assert!(matches!(
            run(&PanelCommand, "auto"),
            CommandResult::SetPanel(Some(PanelMode::Auto))
        ));
        assert!(matches!(
            run(&PanelCommand, "wide"),
            CommandResult::Error(_)
        ));
    }

    #[test]
    fn landing_arguments_map_to_styles() {
        assert!(matches!(
            run(&LandingCommand, ""),
            CommandResult::SetLanding(None)
        ));
        assert!(matches!(
            run(&LandingCommand, "sheet"),
            CommandResult::SetLanding(Some(LandingStyle::Sheet))
        ));
        assert!(matches!(
            run(&LandingCommand, "list"),
            CommandResult::Error(_)
        ));
    }
}
