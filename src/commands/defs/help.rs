//! `/help` — lists every registered command. The list is generated from the
//! live `CommandRegistry` (see `HelpEntry` / `register_defaults` in
//! `commands/mod.rs`) rather than hand-maintained, so it can never drift out
//! of sync with the commands that actually exist.

use crate::app::AppState;
use crate::commands::{Command, CommandResult, HelpEntry};
use crate::config::Config;

pub struct HelpCommand {
    entries: Vec<HelpEntry>,
}

impl HelpCommand {
    pub fn new(entries: Vec<HelpEntry>) -> Self {
        Self { entries }
    }

    fn build_help_text(&self) -> String {
        // Prefer `usage` as the left column when it's a single line (it
        // already spells out any arguments, e.g. "/load <session_id>").
        // A few commands (`/mcp`, `/skills`) have multi-line usage strings
        // meant for their own error messages, not a one-line table — for
        // those, fall back to the first line so the list stays aligned.
        let left_columns: Vec<String> = self
            .entries
            .iter()
            .map(|e| {
                let first_line = e.usage.lines().next().unwrap_or("");
                if first_line.is_empty() {
                    format!("/{}", e.name)
                } else {
                    first_line.to_string()
                }
            })
            .collect();

        let width = left_columns.iter().map(|s| s.len()).max().unwrap_or(0);

        let mut out = String::from("Available commands:\n");
        for (entry, left) in self.entries.iter().zip(&left_columns) {
            let description = &entry.description;
            out.push_str(&format!("  {left:<width$} — {description}\n"));
        }

        out.push_str(
            "\nKeyboard shortcuts:\n\
             \x20 Ctrl+C         — Cancel generation (if active), otherwise quit\n\
             \x20 Ctrl+L         — Clear chat\n\
             \x20 Enter          — Send message\n\
             \x20 Shift+Enter    — New line in prompt\n\
             \x20 `\\` + Enter   — New line in prompt (line continuation)\n\
             \x20 Up/Down        — Scroll chat\n\
             \x20 Ctrl+Left/Right — Move by word\n\
             \x20 Ctrl+Shift+Left/Right — Select by word\n\
             \x20 Shift+Left/Right — Select by char\n\
             \x20 Ctrl+A         — Select all",
        );

        out
    }
}

impl Command for HelpCommand {
    fn name(&self) -> &str {
        "help"
    }

    fn description(&self) -> &str {
        "Show available commands"
    }

    fn usage(&self) -> &str {
        "/help"
    }

    fn execute(&self, _args: &str, state: &mut AppState, _config: &Config) -> CommandResult {
        let help_text = self.build_help_text();
        state.push_system(&help_text);
        CommandResult::Handled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entries() -> Vec<HelpEntry> {
        vec![
            HelpEntry {
                name: "clear".to_string(),
                description: "Clear chat history".to_string(),
                usage: "/clear".to_string(),
            },
            HelpEntry {
                name: "help".to_string(),
                description: "Show available commands".to_string(),
                usage: "/help".to_string(),
            },
        ]
    }

    #[test]
    fn help_text_mentions_every_entry() {
        let cmd = HelpCommand::new(sample_entries());
        let text = cmd.build_help_text();
        assert!(text.contains("/clear"));
        assert!(text.contains("/help"));
        assert!(text.contains("Clear chat history"));
    }

    #[test]
    fn help_text_ctrl_c_line_is_accurate() {
        let cmd = HelpCommand::new(sample_entries());
        let text = cmd.build_help_text();
        assert!(text.contains("Ctrl+C"));
        assert!(text.to_lowercase().contains("cancel"));
        // Must not claim Ctrl+C *only* quits — it cancels generation first.
        assert!(!text.contains("Ctrl+C         — Quit"));
    }

    #[test]
    fn help_text_is_empty_entries_safe() {
        let cmd = HelpCommand::new(Vec::new());
        let text = cmd.build_help_text();
        assert!(text.contains("Available commands:"));
    }

    #[test]
    fn help_text_shows_argument_placeholders_from_usage() {
        let entries = vec![HelpEntry {
            name: "load".to_string(),
            description: "Load a session by ID".to_string(),
            usage: "/load <session_id>".to_string(),
        }];
        let cmd = HelpCommand::new(entries);
        let text = cmd.build_help_text();
        assert!(text.contains("/load <session_id>"));
    }

    #[test]
    fn help_text_falls_back_to_first_line_for_multiline_usage() {
        let entries = vec![HelpEntry {
            name: "mcp".to_string(),
            description: "Open MCP server management".to_string(),
            usage: "/mcp — open management view\n/mcp ttl <secs> — set cache TTL".to_string(),
        }];
        let cmd = HelpCommand::new(entries);
        let text = cmd.build_help_text();
        assert!(text.contains("/mcp — open management view"));
        // The second usage line must not leak into the aligned table.
        assert!(!text.contains("/mcp ttl <secs> — set cache TTL"));
    }
}
