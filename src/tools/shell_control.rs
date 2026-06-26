use super::*;
use serde_json::{json, Value};

pub struct ShellOutputTool;

#[async_trait]
impl Tool for ShellOutputTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "shell_output".to_string(),
            description: "Read new output from a background shell process started with bash/powershell background=true. Returns output accumulated since the last read (drained) and the current process status.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "number",
                        "description": "The background process id returned by the background bash/powershell call"
                    }
                },
                "required": ["id"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let id = match args["id"].as_u64() {
            Some(id) => id,
            None => match args["id"].as_f64() {
                Some(f) => f as u64,
                None => return ToolResult::error("Missing or invalid 'id' argument"),
            },
        };

        match background::read_output(id).await {
            Some((output, status)) => {
                let mut msg = format!("Background process id={} status: {}\n", id, status.label());
                if output.is_empty() {
                    msg.push_str("(no new output since last read)");
                } else {
                    msg.push_str(&output);
                    if !output.ends_with('\n') {
                        msg.push('\n');
                    }
                }
                ToolResult::success(&msg)
            }
            None => ToolResult::error(&format!("No background process with id={id}")),
        }
    }
}

pub struct ShellKillTool;

#[async_trait]
impl Tool for ShellKillTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "shell_kill".to_string(),
            description: "Terminate a background shell process started with bash/powershell background=true.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "number",
                        "description": "The background process id to terminate"
                    }
                },
                "required": ["id"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let id = match args["id"].as_u64() {
            Some(id) => id,
            None => match args["id"].as_f64() {
                Some(f) => f as u64,
                None => return ToolResult::error("Missing or invalid 'id' argument"),
            },
        };

        match background::kill_process(id).await {
            Some(Ok(())) => {
                let (output, status) = match background::read_output(id).await {
                    Some(v) => v,
                    None => (String::new(), background::ProcessStatus::Finished(None)),
                };
                let mut msg = format!("Background process id={} terminated. Final status: {}\n", id, status.label());
                if !output.is_empty() {
                    msg.push_str("Remaining output:\n");
                    msg.push_str(&output);
                    if !output.ends_with('\n') {
                        msg.push('\n');
                    }
                }
                ToolResult::success(&msg)
            }
            Some(Err(e)) => ToolResult::error(&format!("Failed to kill process id={id}: {e}")),
            None => ToolResult::error(&format!("No background process with id={id}")),
        }
    }
}

pub struct ShellListTool;

#[async_trait]
impl Tool for ShellListTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "shell_list".to_string(),
            description: "List all background shell processes started with bash/powershell background=true, with their ids, shells, commands and statuses.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {},
            }),
        }
    }

    async fn execute(&self, _args: Value) -> ToolResult {
        let procs = background::list_processes().await;
        if procs.is_empty() {
            return ToolResult::success("No background processes.");
        }
        let mut msg = String::from("Background processes:\n");
        for (id, shell, command, status, interactive) in procs {
            let preview: String = command.chars().take(80).collect();
            let kind = if interactive { "interactive" } else { "background" };
            msg.push_str(&format!(
                "- id={id} [{shell}] {kind} {status}: {preview}\n"
            ));
        }
        ToolResult::success(&msg)
    }
}

pub struct ShellInputTool;

fn key_sequence(key: &str) -> Option<Vec<u8>> {
    let bytes = match key {
        "up" | "arrowup" => b"\x1b[A".as_slice(),
        "down" | "arrowdown" => b"\x1b[B".as_slice(),
        "right" | "arrowright" => b"\x1b[C".as_slice(),
        "left" | "arrowleft" => b"\x1b[D".as_slice(),
        "enter" | "return" => b"\r".as_slice(),
        "esc" | "escape" => b"\x1b".as_slice(),
        "tab" => b"\t".as_slice(),
        "space" => b" ".as_slice(),
        "backspace" => b"\x7f".as_slice(),
        "delete" => b"\x1b[3~".as_slice(),
        "home" => b"\x1b[H".as_slice(),
        "end" => b"\x1b[F".as_slice(),
        "pageup" => b"\x1b[5~".as_slice(),
        "pagedown" => b"\x1b[6~".as_slice(),
        "ctrl+c" | "ctrlc" => b"\x03".as_slice(),
        "ctrl+d" | "ctrld" => b"\x04".as_slice(),
        "ctrl+z" | "ctrlz" => b"\x1a".as_slice(),
        _ => return None,
    };
    Some(bytes.to_vec())
}

#[async_trait]
impl Tool for ShellInputTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "shell_input".to_string(),
            description: "Send keystrokes or text to an INTERACTIVE background process (started with bash/powershell interactive=true). Use `text` to type a string and `keys` for special keys (arrow keys, enter, etc.). Applied in order: text first, then each key. Essential for driving CLI menus/wizards (e.g. npm create vite, npm init, gh auth login).".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "number",
                        "description": "The interactive background process id returned by the interactive bash/powershell call"
                    },
                    "text": {
                        "type": "string",
                        "description": "Literal text to type into the process. Optional. For yes/no prompts or typed answers."
                    },
                    "keys": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Special keys to press, applied after `text`. Each item is one of: up, down, left, right, enter, esc, tab, space, backspace, delete, home, end, pageup, pagedown, ctrl+c, ctrl+d, ctrl+z."
                    }
                },
                "required": ["id"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let id = match args["id"].as_u64() {
            Some(id) => id,
            None => match args["id"].as_f64() {
                Some(f) => f as u64,
                None => return ToolResult::error("Missing or invalid 'id' argument"),
            },
        };

        let text = args["text"].as_str().unwrap_or("");
        let keys: &[serde_json::Value] = match args["keys"].as_array() {
            Some(arr) => arr.as_slice(),
            None => &[],
        };

        if text.is_empty() && keys.is_empty() {
            return ToolResult::error("Provide at least one of `text` or `keys`.");
        }

        let mut bytes = Vec::new();
        if !text.is_empty() {
            bytes.extend_from_slice(text.as_bytes());
        }
        let mut unknown: Vec<String> = Vec::new();
        for k in keys {
            if let Some(ks) = k.as_str().and_then(key_sequence) {
                bytes.extend_from_slice(&ks);
            } else if let Some(s) = k.as_str() {
                unknown.push(s.to_string());
            }
        }

        if !unknown.is_empty() {
            return ToolResult::error(&format!(
                "Unknown key(s): {}. Valid: up, down, left, right, enter, esc, tab, space, backspace, delete, home, end, pageup, pagedown, ctrl+c, ctrl+d, ctrl+z.",
                unknown.join(", ")
            ));
        }

        match background::write_input(id, &bytes).await {
            Ok(()) => {
                let mut summary = String::new();
                if !text.is_empty() {
                    summary.push_str(&format!("typed {text:?}"));
                }
                if !keys.is_empty() {
                    let names: Vec<String> = keys
                        .iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect();
                    if !summary.is_empty() {
                        summary.push_str(" + ");
                    }
                    summary.push_str(&format!("keys=[{}]", names.join(", ")));
                }
                ToolResult::success(&format!("Sent input to process id={id}: {summary}. Poll with shell_output to see the result."))
            }
            Err(e) => ToolResult::error(&e),
        }
    }
}
