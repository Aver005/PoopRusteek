use crate::app::AppState;
use crate::commands::{Command, CommandResult, with_args};
use crate::config::Config;

pub struct SkillsCommand;

impl Command for SkillsCommand {
    fn name(&self) -> &str {
        "skills"
    }

    fn description(&self) -> &str {
        "Manage skills — list, enable, disable, or pick skills to include with requests"
    }

    fn usage(&self) -> &str {
        "/skills — open skill picker\n/skills list — list all skills\n/skills enable <name> — enable a skill\n/skills disable <name> — disable a skill"
    }

    fn execute(&self, args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        let args = args.trim();

        if args.is_empty() {
            return CommandResult::ShowSkills;
        }

        let parts: Vec<&str> = args.splitn(2, ' ').collect();
        match parts[0] {
            "list" => CommandResult::ShowSkills,
            "enable" | "disable" => with_args(
                parts.get(1).unwrap_or(&""),
                "/skills enable <name> or /skills disable <name>",
                |name| CommandResult::ToggleSkill(name.to_string(), parts[0] == "enable"),
            ),
            _ => CommandResult::Error(format!(
                "Unknown subcommand: {}. Usage:\n  /skills — open skill picker\n  /skills list — list all skills\n  /skills enable <name> — enable a skill\n  /skills disable <name> — disable a skill",
                parts[0]
            )),
        }
    }
}
