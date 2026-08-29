use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

/// `/instructions` — что агент подхватил из `AGENTS.md` и родни, перечитать
/// это и выключить. Выключатель нужен именно потому, что текст приходит из
/// чужого репозитория и едет в системный промпт каждым запросом.
pub struct InstructionsCommand;

impl Command for InstructionsCommand {
    fn name(&self) -> &str {
        "instructions"
    }

    fn description(&self) -> &str {
        "Show, reload or disable project instruction files"
    }

    fn usage(&self) -> &str {
        "/instructions [reload|on|off]"
    }

    fn execute(&self, args: &str, state: &mut AppState, config: &Config) -> CommandResult {
        match args.trim() {
            "on" | "off" => CommandResult::Instructions(args.trim() == "on"),
            "reload" => {
                let notice = crate::app::reload_instructions(state, config);
                state.push_ui_system(
                    &notice.unwrap_or_else(|| "No project instruction files found.".to_string()),
                );
                CommandResult::Handled
            }
            "" => {
                state.push_ui_system(&status(state, config));
                CommandResult::Handled
            }
            other => CommandResult::Error(format!(
                "Unknown argument '{other}'. Usage: /instructions [reload|on|off]"
            )),
        }
    }
}

/// Статус в одном сообщении: включено ли, сколько весит секция, откуда она и
/// где искать. Размер здесь не украшение — эта секция едет в каждый запрос.
fn status(state: &AppState, config: &Config) -> String {
    if !config.instructions.enabled {
        return "Project instructions: OFF (`/instructions on` to enable).".to_string();
    }
    let mut lines = vec![format!(
        "Project instructions: ON, {} bytes in the system prompt (budget {}).",
        state.instructions_section.len(),
        config.instructions.max_bytes
    )];
    if state.instructions_section.is_empty() {
        lines.push(format!(
            "Nothing found. Looked for {} from {} up to the repository root, plus {}.",
            crate::instructions::PROJECT_FILES.join(" / "),
            state.workspace_path,
            crate::instructions::global_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "the home directory".to_string()),
        ));
    }
    lines.push(
        "`/instructions reload` after editing a file; `/instructions off` to stop sending them."
            .to_string(),
    );
    lines.join("\n")
}
