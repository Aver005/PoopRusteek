use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;
use crate::provider::{ChatMessage, Role};
use crate::session::SESSION_VERSION;

pub struct ImportCommand;

impl Command for ImportCommand {
    fn name(&self) -> &str {
        "import"
    }

    fn description(&self) -> &str {
        "Import chat from Markdown file (creates new session tagged 'Imported')"
    }

    fn usage(&self) -> &str {
        "/import <path>"
    }

    fn execute(&self, args: &str, state: &mut AppState, config: &Config) -> CommandResult {
        let path = args.trim();
        if path.is_empty() {
            return CommandResult::Error("Usage: /import <path-to-file.md>".to_string());
        }

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => return CommandResult::Error(format!("Failed to read file: {e}")),
        };

        let messages = match parse_markdown_export(&content) {
            Ok(msgs) => msgs,
            Err(e) => return CommandResult::Error(format!("Failed to parse export: {e}")),
        };

        if messages.is_empty() {
            return CommandResult::Error("No messages found in export file".to_string());
        }

        let now = chrono::Utc::now().to_rfc3339();
        let session_id = crate::session::create_session_id();
        let workspace_root = std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        let session = crate::session::Session {
            version: SESSION_VERSION,
            id: session_id.clone(),
            created_at: now.clone(),
            updated_at: now,
            workspace_root,
            model_type: config.provider.model.clone(),
            messages: messages.clone(),
            tag: Some("Imported".to_string()),
        };

        if let Err(e) = crate::session::save_local(&session, config) {
            return CommandResult::Error(format!("Failed to save imported session: {e}"));
        }

        state.focused_mut().messages = messages;
        state.focused_mut().session_id = session_id.clone();
        state.scroll_offset = 0;
        state.input.buffer.clear();
        state.input.cursor = 0;
        state.input.selection_anchor = None;
        state.focused_mut().generation.active = false;

        state.status_message = format!(
            "Imported session {} ({} messages, tagged Imported)",
            &session_id[..session_id.len().min(17)],
            state.focused_mut().messages.len(),
        );

        CommandResult::Handled
    }
}

fn parse_markdown_export(content: &str) -> Result<Vec<ChatMessage>, String> {
    let mut messages = Vec::new();
    let mut current_role: Option<Role> = None;
    let mut current_name: Option<String> = None;
    let mut current_tool_call_id: Option<String> = None;
    let mut current_created_at: Option<String> = None;
    let mut current_content = String::new();

    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            if let Some(role) = current_role.take() {
                let created_at = current_created_at.clone().unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
                messages.push(ChatMessage {
                    role,
                    content: current_content.trim().to_string(),
                    name: current_name.take(),
                    tool_call_id: current_tool_call_id.take(),
                    display_content: None,
                    tool_error: false,
                    ui_only: false,
                    created_at,
                    total_tokens: None,
                    model: String::new(),
                    status: None,
                    think_elapsed_secs: 0.0,
                    references_count: 0,
                    search_triggered: false,
                });
                current_content.clear();
                current_created_at = None;
            }

            current_role = match rest.trim() {
                "system" => Some(Role::System),
                "user" => Some(Role::User),
                "assistant" => Some(Role::Assistant),
                "tool" => Some(Role::Tool),
                _ => {
                    return Err(format!("Unknown role: {rest}"));
                }
            };
        } else if let Some(val) = line.strip_prefix("- name: ") {
            current_name = Some(val.trim().to_string());
        } else if let Some(val) = line.strip_prefix("- tool_call_id: ") {
            current_tool_call_id = Some(val.trim().to_string());
        } else if let Some(val) = line.strip_prefix("- created_at: ") {
            current_created_at = Some(val.trim().to_string());
        } else if line == "---" {
            // separator, skip
        } else if current_role.is_some()
            && (!current_content.is_empty() || !line.trim().is_empty()) {
                if current_content.is_empty() {
                    current_content.push_str(line.trim_end());
                } else {
                    current_content.push('\n');
                    current_content.push_str(line.trim_end());
                }
            }
    }

    if let Some(role) = current_role {
        let created_at = current_created_at.unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
        messages.push(ChatMessage {
            role,
            content: current_content.trim().to_string(),
            name: current_name.take(),
            tool_call_id: current_tool_call_id.take(),
            display_content: None,
            tool_error: false,
            ui_only: false,
            created_at,
            total_tokens: None,
            model: String::new(),
            status: None,
            think_elapsed_secs: 0.0,
            references_count: 0,
            search_triggered: false,
        });
    }

    Ok(messages)
}
