use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;
use crate::provider::Role;

pub struct ExportCommand;

impl Command for ExportCommand {
    fn name(&self) -> &str {
        "export"
    }

    fn description(&self) -> &str {
        "Export current chat to Markdown file"
    }

    fn usage(&self) -> &str {
        "/export [path]"
    }

    fn execute(&self, args: &str, state: &mut AppState, _config: &Config) -> CommandResult {
        if state.focused_mut().messages.is_empty() {
            return CommandResult::Error("No messages to export".to_string());
        }

        let path = if args.trim().is_empty() {
            let exports_dir = Config::data_dir().join("exports");
            std::fs::create_dir_all(&exports_dir).unwrap_or_default();
            exports_dir.join(format!("{}.md", state.focused().session_id))
        } else {
            let custom = std::path::PathBuf::from(args.trim());
            if let Some(parent) = custom.parent() {
                std::fs::create_dir_all(parent).unwrap_or_default();
            }
            custom
        };

        let mut md = String::new();
        md.push_str("# Pooprusteek Chat Export\n\n");
        md.push_str(&format!("- **Session:** {}\n", state.focused().session_id));
        md.push_str(&format!("- **Exported:** {}\n", chrono::Utc::now().to_rfc3339()));
        md.push_str(&format!("- **Messages:** {}\n", state.focused_mut().messages.len()));
        md.push_str("\n---\n\n");

        for msg in &state.focused_mut().messages {
            let role_label = match msg.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            };

            md.push_str(&format!("## {role_label}\n"));
            if let Some(ref name) = msg.name {
                md.push_str(&format!("- name: {name}\n"));
            }
            if let Some(ref id) = msg.tool_call_id {
                md.push_str(&format!("- tool_call_id: {id}\n"));
            }
            md.push_str(&format!("- created_at: {}\n", msg.created_at));
            md.push('\n');
            md.push_str(&msg.content);
            md.push_str("\n\n---\n\n");
        }

        match std::fs::write(&path, md) {
            Ok(_) => {
                let display = path.display().to_string();
                let count = state.focused().messages.len();
                state.push_system(&format!("Exported {count} messages to {display}"));
                CommandResult::Handled
            }
            Err(e) => CommandResult::Error(format!("Failed to export: {e}")),
        }
    }
}
