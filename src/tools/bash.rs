use super::*;
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use tokio::process::Command;

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash".to_string(),
            description: "Execute a bash command and return its output. Modes: (1) foreground (default) — waits for completion; (2) background=true — detached for long-running non-interactive servers/watchers, returns a process id; (3) interactive=true — runs in a pseudo-terminal so arrow-key menus, REPLs and CLI wizards work. IMPORTANT: ALWAYS use interactive=true for commands that show menus/prompts (npm create, bun create, npx create, npm init, gh auth, etc) — foreground mode will corrupt the terminal! Use shell_input to send keystrokes to interactive processes and shell_output/shell_kill/shell_list to manage them.".to_string(),
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
                    },
                    "persistent": {
                        "type": "boolean",
                        "description": "Keep the process alive across future user turns. Good for dev servers/watchers. Defaults to true for obvious dev-server commands."
                    },
                    "ttl_seconds": {
                        "type": "number",
                        "description": "Idle TTL for persistent jobs in seconds. Default 1800. Set 0 to disable auto-expire."
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
        let persistent = args["persistent"]
            .as_bool()
            .unwrap_or_else(|| looks_persistent_background_command(command));
        let ttl_secs = if persistent {
            Some(
                args["ttl_seconds"]
                    .as_u64()
                    .or_else(|| args["ttl_seconds"].as_f64().map(|value| value.max(0.0) as u64))
                    .unwrap_or(background::DEFAULT_PERSISTENT_TTL_SECS),
            )
        } else {
            None
        };
        let forced_interactive = looks_interactive_command(command) && !interactive;
        let interactive = interactive || forced_interactive;
        let background = if interactive { false } else { background };

        if interactive {
            let bash_args = vec!["-c".to_string(), command.to_string()];
            return spawn_interactive_bash(
                bash_args,
                command,
                wait_seconds,
                forced_interactive,
                persistent,
                ttl_secs,
            )
            .await;
        }

        if background {
            let mut cmd = Command::new("bash");
            cmd.arg("-c").arg(command);
            return spawn_background_bash(cmd, command, wait_seconds, persistent, ttl_secs).await;
        }

        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(command);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        // DETACHED_PROCESS: create child without a console so it cannot corrupt
        // our TUI's shared console state (Windows-specific). Non-interactive
        // commands work fine via pipes; interactive commands MUST use
        // interactive=true.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.as_std_mut().creation_flags(0x00000008);
        }

        let child = match cmd.spawn()
        {
            Ok(c) => c,
            Err(e) => return ToolResult::error(&format!("Failed to execute command: {e}")),
        };

        // Track PID so Escape/Ctrl+C can kill the child process.
        crate::app::FOREGROUND_CHILD_PID.store(child.id().unwrap_or(0), Ordering::SeqCst);

        let output = child.wait_with_output().await;

        crate::app::FOREGROUND_CHILD_PID.store(0, Ordering::SeqCst);
        crate::app::request_terminal_restore();

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
    persistent: bool,
    ttl_secs: Option<u64>,
) -> ToolResult {
    let result = background::spawn_background(
        cmd,
        command_str.to_string(),
        "bash".to_string(),
        wait_seconds,
        persistent,
        ttl_secs,
    )
    .await;
    crate::app::request_terminal_restore();
    match result {
        Ok(outcome) => {
            let mut msg = format!("Started bash job #{} ({})", outcome.id, job_mode_label(outcome.persistent, outcome.ttl_secs));
            if outcome.initial_output.is_empty() {
                msg.push_str("\n(no output yet)");
            } else {
                msg.push_str("\n");
                msg.push_str(&outcome.initial_output);
                if !outcome.initial_output.ends_with('\n') {
                    msg.push('\n');
                }
            }
            msg.push_str(&format!(
                "\nNext: `shell_output` id={} | `shell_kill` id={} | `/jobs`",
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
    forced_interactive: bool,
    persistent: bool,
    ttl_secs: Option<u64>,
) -> ToolResult {
    let result = background::spawn_interactive(
        "bash",
        &bash_args,
        None,
        command_str.to_string(),
        "bash".to_string(),
        wait_seconds,
        100,
        30,
        persistent,
        ttl_secs,
    )
    .await;
    crate::app::request_terminal_restore();
    match result {
        Ok(outcome) => {
            let mut msg = String::new();
            if forced_interactive {
                msg.push_str("Auto-upgraded to interactive PTY.\n");
            }
            msg.push_str(&format!(
                "Started interactive bash job #{} ({})\n",
                outcome.id,
                job_mode_label(outcome.persistent, outcome.ttl_secs)
            ));
            if outcome.initial_output.is_empty() {
                msg.push_str("(no output yet; poll with `shell_output`)\n");
            } else {
                msg.push_str(&outcome.initial_output);
                if !outcome.initial_output.ends_with('\n') {
                    msg.push('\n');
                }
            }
            msg.push_str(&format!(
                "\nNext: `shell_input` id={} | `shell_output` id={} | `shell_kill` id={} | `/jobs`",
                outcome.id, outcome.id, outcome.id
            ));
            ToolResult::success(&msg)
        }
        Err(e) => ToolResult::error(&e),
    }
}

fn job_mode_label(persistent: bool, ttl_secs: Option<u64>) -> String {
    if persistent {
        match ttl_secs {
            Some(0) => "persistent, ttl=off".to_string(),
            Some(ttl) => format!("persistent, idle ttl={}s", ttl),
            None => "persistent".to_string(),
        }
    } else {
        "ephemeral".to_string()
    }
}
