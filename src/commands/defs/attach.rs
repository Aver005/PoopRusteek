use crate::app::AppState;
use crate::commands::{Command, CommandResult, with_args};
use crate::config::Config;

/// Строка статуса после прикрепления — общая с автодополнением `@`.
pub fn attached_status(count: usize) -> String {
    match count {
        1 => "1 file attached".to_string(),
        n => format!("{n} files attached"),
    }
}

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
        with_args(args, "/attach <path1> [path2] ...", |args| {
            let workspace = state.workspace();
            let mut attached = false;
            for raw_path in parse_paths(args) {
                match crate::app::attachments::resolve(&raw_path, &workspace) {
                    Ok(file) => {
                        state.attached_files.push(file);
                        attached = true;
                    }
                    Err(reason) => state.push_system(&reason),
                }
            }
            if attached {
                state.status_message = attached_status(state.attached_files.len());
            }
            CommandResult::Handled
        })
    }
}

fn parse_paths(input: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    for ch in input.chars() {
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
