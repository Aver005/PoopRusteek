use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;
use crate::provider::AttachedFile;

pub struct AttachCommand;

impl Command for AttachCommand {
    fn name(&self) -> &str {
        "attach"
    }

    fn description(&self) -> &str {
        "Attach files to the current message"
    }

    fn usage(&self) -> &str {
        "/attach <path1> [path2] ..."
    }

    fn execute(&self, args: &str, state: &mut AppState, _config: &Config) -> CommandResult {
        let args = args.trim();
        if args.is_empty() {
            return CommandResult::Error("Usage: /attach <path1> [path2] ...".to_string());
        }

        let paths = parse_paths(args);
        let mut attached = 0u32;

        for raw_path in &paths {
            let path = std::path::Path::new(raw_path);
            let resolved = if path.is_relative() {
                let cwd = std::env::current_dir().unwrap_or_default();
                cwd.join(path)
            } else {
                path.to_path_buf()
            };

            if !resolved.exists() {
                let _ = state.focused_mut().messages.push(crate::provider::ChatMessage::system(
                    &format!("File not found: {raw_path}"),
                ));
                continue;
            }
            if !resolved.is_file() {
                let _ = state.focused_mut().messages.push(crate::provider::ChatMessage::system(
                    &format!("Not a file: {raw_path}"),
                ));
                continue;
            }

            let metadata = match resolved.metadata() {
                Ok(m) => m,
                Err(e) => {
                    let _ = state.focused_mut().messages.push(crate::provider::ChatMessage::system(
                        &format!("Cannot read {raw_path}: {e}"),
                    ));
                    continue;
                }
            };

            let display_name = resolved
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(raw_path)
                .to_string();

            let ext = resolved.extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            let is_image = matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg");

            state.attached_files.push(AttachedFile {
                display_name,
                path: resolved.to_string_lossy().to_string(),
                size: metadata.len(),
                is_image,
            });
            attached += 1;
        }

        if attached > 0 {
            if state.attached_files.len() == 1 {
                state.status_message = format!("1 file attached");
            } else {
                state.status_message = format!("{} files attached", state.attached_files.len());
            }
        }

        CommandResult::Handled
    }
}

fn parse_paths(input: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut chars = input.chars().peekable();
    let mut current = String::new();
    let mut in_quotes = false;

    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
            }
            ' ' if !in_quotes => {
                if !current.is_empty() {
                    paths.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        paths.push(current);
    }
    paths
}
