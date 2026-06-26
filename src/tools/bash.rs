use super::*;
use serde_json::{json, Value};
use tokio::process::Command;

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash".to_string(),
            description: "Execute a bash command and return its output. Modes: (1) foreground (default) — waits for completion; (2) background=true — detached for long-running non-interactive servers/watchers, returns a process id; (3) interactive=true — runs in a pseudo-terminal so arrow-key menus, REPLs and CLI wizards work. Use shell_input to send keystrokes to interactive processes and shell_output/shell_kill/shell_list to manage them.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The bash command to execute"
                    },
                    "background": {
                        "type": "boolean",
                        "description": "If true, run detached and return immediately with a process id. Use for long-running non-interactive servers/watchers."
                    },
                    "interactive": {
                        "type": "boolean",
                        "description": "If true, run in a PTY so interactive TUI menus and prompts work. Returns a process id; drive it with shell_input. Implies detached."
                    },
                    "wait_seconds": {
                        "type": "number",
                        "description": "Only with background=true or interactive=true. Seconds to capture initial output before returning. Default 2, max 10."
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let command = match args["command"].as_str() {
            Some(cmd) => cmd,
            None => return ToolResult::error("Missing 'command' argument"),
        };

        let interactive = args["interactive"].as_bool().unwrap_or(false);
        let background = args["background"].as_bool().unwrap_or(false);
        let wait_seconds = args["wait_seconds"]
            .as_f64()
            .unwrap_or(2.0)
            .clamp(0.0, 10.0);

        if interactive {
            let bash_args = vec!["-c".to_string(), command.to_string()];
            return spawn_interactive_bash(bash_args, command, wait_seconds).await;
        }

        if background {
            let mut cmd = Command::new("bash");
            cmd.arg("-c").arg(command);
            return spawn_background_bash(cmd, command, wait_seconds).await;
        }

        let output = Command::new("bash")
            .arg("-c")
            .arg(command)
            .output()
            .await;

        match output {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);

                if output.status.success() {
                    ToolResult::success(&stdout)
                } else {
                    let mut result = String::new();
                    if !stdout.is_empty() {
                        result.push_str(&stdout);
                    }
                    if !stderr.is_empty() {
                        if !result.is_empty() {
                            result.push('\n');
                        }
                        result.push_str(&stderr);
                    }
                    ToolResult::error(&result)
                }
            }
            Err(e) => ToolResult::error(&format!("Failed to execute command: {e}")),
        }
    }
}

async fn spawn_background_bash(
    cmd: Command,
    command_str: &str,
    wait_seconds: f64,
) -> ToolResult {
    match background::spawn_background(cmd, command_str.to_string(), "bash".to_string(), wait_seconds).await {
        Ok(outcome) => {
            let mut msg = format!(
                "[Background] Process started. id={} shell=bash\nCommand: {}\nStatus: {}\nInitial output (captured {}s):\n",
                outcome.id, command_str, outcome.status.label(), wait_seconds
            );
            if outcome.initial_output.is_empty() {
                msg.push_str("(no output yet)\n");
            } else {
                msg.push_str(&outcome.initial_output);
                if !outcome.initial_output.ends_with('\n') {
                    msg.push('\n');
                }
            }
            msg.push_str(&format!(
                "\nUse `shell_output` with id={} to read new output, `shell_kill` with id={} to stop it, `shell_list` to see all background processes.",
                outcome.id, outcome.id
            ));
            ToolResult::success(&msg)
        }
        Err(e) => ToolResult::error(&e),
    }
}

async fn spawn_interactive_bash(
    bash_args: Vec<String>,
    command_str: &str,
    wait_seconds: f64,
) -> ToolResult {
    match background::spawn_interactive(
        "bash",
        &bash_args,
        None,
        command_str.to_string(),
        "bash".to_string(),
        wait_seconds,
        100,
        30,
    )
    .await
    {
        Ok(outcome) => {
            let mut msg = format!(
                "[Interactive PTY] Process started. id={} shell=bash\nCommand: {}\nStatus: {}\nInitial output (captured {}s):\n",
                outcome.id, command_str, outcome.status.label(), wait_seconds
            );
            if outcome.initial_output.is_empty() {
                msg.push_str("(no output yet — the menu/prompt may still be rendering; poll with shell_output)\n");
            } else {
                msg.push_str(&outcome.initial_output);
                if !outcome.initial_output.ends_with('\n') {
                    msg.push('\n');
                }
            }
            msg.push_str(&format!(
                "\nThis is an interactive process. Use `shell_input` with id={} to send keystrokes (arrow keys, enter, text), `shell_output` with id={} to read new output, `shell_kill` with id={} to stop it.",
                outcome.id, outcome.id, outcome.id
            ));
            ToolResult::success(&msg)
        }
        Err(e) => ToolResult::error(&e),
    }
}
