use crate::app::AppState;
use crate::commands::{Command, CommandResult, with_args};
use crate::config::Config;

fn strip_verbatim(path: &str) -> String {
    // On Windows, std::env::current_dir() may return \\?\C:\... prefix.
    // Strip it for cleaner display.
    if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        path.to_string()
    }
}

pub struct CwdCommand {
    pub name: &'static str,
}

impl Command for CwdCommand {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "Change the current working directory"
    }

    fn usage(&self) -> &str {
        "/cwd <path>"
    }

    fn execute(&self, args: &str, state: &mut AppState, config: &Config) -> CommandResult {
        with_args(args, "/cwd <path>", |path| {
            let target = crate::util::expand_tilde(path);

            // Convert to absolute without requiring existence (Rust 1.79+)
            let absolute = std::path::absolute(&target).unwrap_or_else(|_| target.clone());

            match std::env::set_current_dir(&absolute) {
                Ok(()) => {
                    let cwd = std::env::current_dir()
                        .map(|p| strip_verbatim(&p.to_string_lossy()))
                        .unwrap_or_else(|_| absolute.to_string_lossy().to_string());
                    state.push_system(&format!("Changed directory to {cwd}"));
                    state.workspace_path = cwd;
                    // Новая папка — новый AGENTS.md. Перечитываем здесь, а не
                    // на каждом ходу: сборка промпта идёт на цикле событий.
                    if let Some(notice) = crate::app::reload_instructions(state, config) {
                        state.push_ui_system(&notice);
                    }
                    CommandResult::Handled
                }
                Err(e) => {
                    CommandResult::Error(format!("Cannot change to {}: {e}", absolute.display()))
                }
            }
        })
    }
}
