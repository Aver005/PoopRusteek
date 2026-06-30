use super::*;
use serde_json::{json, Value};

/// Parse the `id` argument, accepting both JSON integers and floats — LLMs
/// sometimes emit `12.0` for an integer id.
fn parse_id(args: &Value) -> Option<u64> {
    args["id"]
        .as_u64()
        .or_else(|| args["id"].as_f64().map(|f| f as u64))
}

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
        let Some(id) = parse_id(&args) else {
            return ToolResult::error("Missing or invalid 'id' argument");
        };

        match background::read_output(id).await {
            Some((output, status)) => {
                let mut msg = format!("Job #{} · {}\n", id, status.label());
                if output.is_empty() {
                    msg.push_str("(no new output since last read)");
                } else {
                    msg.push_str(&output);
                    if !output.ends_with('\n') {
                        msg.push('\n');
                    }
                }
                if matches!(status, background::ProcessStatus::Finished(_)) {
                    let _ = background::remove_process(id).await;
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
        let Some(id) = parse_id(&args) else {
            return ToolResult::error("Missing or invalid 'id' argument");
        };

        match background::kill_process(id).await {
            Some(Ok(())) => {
                let (output, status) = match background::read_output(id).await {
                    Some(v) => v,
                    None => (String::new(), background::ProcessStatus::Finished(None)),
                };
                let _ = background::remove_process(id).await;
                let mut msg = format!("Stopped job #{} · {}\n", id, status.label());
                if !output.is_empty() {
                    msg.push_str("Final output:\n");
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
        let _ = background::prune_finished_processes().await;
        let procs = background::process_snapshots().await;
        if procs.is_empty() {
            return ToolResult::success("No jobs.");
        }
        let now = chrono::Utc::now();
        let mut msg = String::from("Jobs:\n");
        for proc in procs {
            let preview: String = proc.command.chars().take(80).collect();
            let kind = if proc.interactive { "interactive" } else { "background" };
            let persist = if proc.persistent { " persistent" } else { "" };
            let age = crate::app::format_duration_secs(
                now.signed_duration_since(proc.started_at).num_seconds().max(0) as u64,
            );
            let idle = crate::app::format_duration_secs(
                now.signed_duration_since(proc.last_activity_at).num_seconds().max(0) as u64,
            );
            let ttl = match proc.ttl_secs {
                Some(0) => " ttl=off".to_string(),
                Some(ttl) => format!(" ttl={}", crate::app::format_duration_secs(ttl)),
                None => String::new(),
            };
            msg.push_str(&format!(
                "- #{} pid={} [{}] {}{} {} age={} idle={}{}: {}\n",
                proc.id,
                proc.pid.map(|pid| pid.to_string()).unwrap_or_else(|| "-".to_string()),
                proc.shell,
                kind,
                persist,
                proc.status.label(),
                age,
                idle,
                ttl,
                preview
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
        let Some(id) = parse_id(&args) else {
            return ToolResult::error("Missing or invalid 'id' argument");
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
                ToolResult::success(&format!("Sent input to job #{id}: {summary}. Poll with `shell_output` to see the result."))
            }
            Err(e) => ToolResult::error(&e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_id_accepts_int_and_float() {
        assert_eq!(parse_id(&json!({"id": 5})), Some(5));
        assert_eq!(parse_id(&json!({"id": 5.0})), Some(5));
        assert_eq!(parse_id(&json!({"id": 5.9})), Some(5)); // float truncates
    }

    #[test]
    fn parse_id_rejects_missing_and_non_numeric() {
        assert_eq!(parse_id(&json!({})), None);
        assert_eq!(parse_id(&json!({"id": "5"})), None); // strings not accepted
        assert_eq!(parse_id(&json!({"id": null})), None);
    }

    #[test]
    fn key_sequence_maps_known_keys() {
        assert_eq!(key_sequence("up"), Some(b"\x1b[A".to_vec()));
        assert_eq!(key_sequence("arrowup"), Some(b"\x1b[A".to_vec()));
        assert_eq!(key_sequence("enter"), Some(b"\r".to_vec()));
        assert_eq!(key_sequence("tab"), Some(b"\t".to_vec()));
        assert_eq!(key_sequence("ctrl+c"), Some(b"\x03".to_vec()));
        assert_eq!(key_sequence("delete"), Some(b"\x1b[3~".to_vec()));
    }

    #[test]
    fn key_sequence_unknown_or_wrong_case_is_none() {
        assert_eq!(key_sequence("f13"), None);
        assert_eq!(key_sequence("UP"), None); // matching is case-sensitive
        assert_eq!(key_sequence(""), None);
    }
}
