use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

/// `/themes` — the theme gallery (live preview, instant apply), the
/// step-by-step create wizard, and direct switching by name. The command
/// only names the intent; the effects live in the `CommandResult`
/// interpreter, which has the `App`-level access (mutable config) this
/// needs.
pub struct ThemesCommand;

impl Command for ThemesCommand {
    fn name(&self) -> &str {
        "themes"
    }

    fn description(&self) -> &str {
        "Pick a color theme (live preview) or build your own with a step-by-step wizard"
    }

    fn usage(&self) -> &str {
        "/themes | /themes new | /themes <name>"
    }

    fn execute(&self, args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        parse(args)
    }
}

fn parse(args: &str) -> CommandResult {
    let args = args.trim();
    match args {
        "" => CommandResult::OpenThemes,
        "new" | "add" | "create" => CommandResult::OpenThemeWizard,
        name => CommandResult::SetTheme(name.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_opens_the_gallery_and_new_opens_the_wizard() {
        assert!(matches!(parse(""), CommandResult::OpenThemes));
        assert!(matches!(parse("  "), CommandResult::OpenThemes));
        for alias in ["new", "add", "create"] {
            assert!(
                matches!(parse(alias), CommandResult::OpenThemeWizard),
                "{alias}"
            );
        }
    }

    #[test]
    fn anything_else_is_a_theme_name() {
        assert!(matches!(parse(" dracula "), CommandResult::SetTheme(name) if name == "dracula"));
    }
}
