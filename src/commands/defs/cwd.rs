use crate::app::AppState;
use crate::commands::{Command, CommandResult};
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

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix('~') {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_default();
        if rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\') {
            format!("{home}{rest}")
        } else {
            format!("{home}\\{rest}")
        }
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

    fn execute(&self, args: &str, state: &mut AppState, _config: &Config) -> CommandResult {
        let path = args.trim();
        if path.is_empty() {
            return CommandResult::Error("Usage: /cwd <path>".to_string());
        }

        let expanded = expand_tilde(path);
        let target = std::path::Path::new(&expanded);

        // Convert to absolute without requiring existence (Rust 1.79+)
        let absolute = std::path::absolute(target)
            .unwrap_or_else(|_| target.to_path_buf());

        match std::env::set_current_dir(&absolute) {
            Ok(()) => {
                let cwd = std::env::current_dir()
                    .map(|p| strip_verbatim(&p.to_string_lossy()))
                    .unwrap_or_else(|_| absolute.to_string_lossy().to_string());
                state.messages.push(crate::provider::ChatMessage::system(
                    &format!("Changed directory to {cwd}"),
                ));
                state.workspace_path = cwd.clone();
                CommandResult::Handled
            }
            Err(e) => CommandResult::Error(format!(
                "Cannot change to {}: {e}",
                absolute.display()
            )),
        }
    }
}
