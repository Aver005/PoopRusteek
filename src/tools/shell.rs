//! Unified shell command tool.
//!
//! `bash` and `powershell` were near-identical 250-line tools differing only in
//! the executable, its argv, and a few labels. They are now one [`ShellTool`]
//! parameterized by a [`Shell`] adapter — two adapters (`bash`, `powershell`)
//! justify the seam; a third shell would be one more `Shell` constructor.

use super::*;
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use tokio::process::Command;

const INTERACTIVE_BASE: &str = "If true, run in a PTY so interactive TUI menus and prompts work. Returns a process id; drive it with shell_input. Implies detached.";

/// Per-shell differences: the executable, how it takes a command, and labels.
pub struct Shell {
    /// Tool name, job label, and executable (all the same per shell).
    name: &'static str,
    /// Human display name used in result messages (e.g. "PowerShell").
    display: &'static str,
    /// Description of the `command` parameter.
    command_desc: &'static str,
    /// Extra text appended to the `interactive` parameter description.
    interactive_hint: &'static str,
    /// Build the argv (after the executable) that runs `command`.
    make_args: fn(&str) -> Vec<String>,
}

fn bash_args(command: &str) -> Vec<String> {
    vec!["-c".to_string(), command.to_string()]
}

fn powershell_args(command: &str) -> Vec<String> {
    vec!["-NoProfile".to_string(), "-Command".to_string(), command.to_string()]
}

impl Shell {
    pub fn bash() -> Self {
        Self {
            name: "bash",
            display: "bash",
            command_desc: "The bash command to execute",
            interactive_hint: "",
            make_args: bash_args,
        }
    }

    pub fn powershell() -> Self {
        Self {
            name: "powershell",
            display: "PowerShell",
            command_desc: "The PowerShell command to execute",
            interactive_hint: " Use for npm create vite, npm init, gh auth login, or any command that opens a selector/REPL.",
            make_args: powershell_args,
        }
    }
}

/// A shell command tool (`bash` or `powershell`), backed by a [`Shell`] adapter.
pub struct ShellTool {
    shell: Shell,
}

impl ShellTool {
    pub fn bash() -> Self {
        Self { shell: Shell::bash() }
    }

    pub fn powershell() -> Self {
        Self { shell: Shell::powershell() }
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn definition(&self) -> ToolDefinition {
        let s = &self.shell;
        ToolDefinition {
            name: s.name.to_string(),
            description: format!(
                "Execute a {} command and return its output. Modes: (1) foreground (default) — waits for completion; (2) background=true — detached for long-running non-interactive servers/watchers, returns a process id; (3) interactive=true — runs in a pseudo-terminal so arrow-key menus, REPLs and CLI wizards work. IMPORTANT: ALWAYS use interactive=true for commands that show menus/prompts (npm create, bun create, npx create, npm init, gh auth, etc) — foreground mode will corrupt the terminal! Use shell_input to send keystrokes to interactive processes and shell_output/shell_kill/shell_list to manage them.",
                s.display
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": s.command_desc
                    },
                    "background": {
                        "type": "boolean",
                        "description": "If true, run detached and return immediately with a process id. Use for long-running non-interactive servers/watchers."
                    },
                    "interactive": {
                        "type": "boolean",
                        "description": format!("{}{}", INTERACTIVE_BASE, s.interactive_hint)
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
        let wait_seconds = args["wait_seconds"].as_f64().unwrap_or(2.0).clamp(0.0, 10.0);
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

        let argv = (self.shell.make_args)(command);

        if interactive {
            return run_interactive(
                &self.shell,
                argv,
                command,
                wait_seconds,
                forced_interactive,
                persistent,
                ttl_secs,
            )
            .await;
        }

        if background {
            let mut cmd = Command::new(self.shell.name);
            cmd.args(&argv);
            return run_background(&self.shell, cmd, command, wait_seconds, persistent, ttl_secs).await;
        }

        let mut cmd = Command::new(self.shell.name);
        cmd.args(&argv);
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

        let child = match cmd.spawn() {
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

async fn run_background(
    shell: &Shell,
    cmd: Command,
    command_str: &str,
    wait_seconds: f64,
    persistent: bool,
    ttl_secs: Option<u64>,
) -> ToolResult {
    let result = background::spawn_background(
        cmd,
        command_str.to_string(),
        shell.name.to_string(),
        wait_seconds,
        persistent,
        ttl_secs,
    )
    .await;
    crate::app::request_terminal_restore();
    match result {
        Ok(outcome) => {
            let mut msg = format!(
                "Started {} job #{} ({})",
                shell.display,
                outcome.id,
                job_mode_label(outcome.persistent, outcome.ttl_secs)
            );
            if outcome.initial_output.is_empty() {
                msg.push_str("\n(no output yet)");
            } else {
                msg.push('\n');
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

async fn run_interactive(
    shell: &Shell,
    argv: Vec<String>,
    command_str: &str,
    wait_seconds: f64,
    forced_interactive: bool,
    persistent: bool,
    ttl_secs: Option<u64>,
) -> ToolResult {
    let result = background::spawn_interactive(
        shell.name,
        &argv,
        None,
        command_str.to_string(),
        shell.name.to_string(),
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
                "Started interactive {} job #{} ({})\n",
                shell.display,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_builds_dash_c_argv() {
        assert_eq!((Shell::bash().make_args)("ls -la"), vec!["-c", "ls -la"]);
    }

    #[test]
    fn powershell_builds_noprofile_command_argv() {
        assert_eq!(
            (Shell::powershell().make_args)("Get-ChildItem"),
            vec!["-NoProfile", "-Command", "Get-ChildItem"]
        );
    }

    #[test]
    fn definitions_name_each_shell() {
        assert_eq!(ShellTool::bash().definition().name, "bash");
        assert_eq!(ShellTool::powershell().definition().name, "powershell");
    }

    #[test]
    fn powershell_interactive_desc_has_extra_hint() {
        let def = ShellTool::powershell().definition();
        let desc = def.parameters["properties"]["interactive"]["description"]
            .as_str()
            .unwrap();
        assert!(desc.starts_with(INTERACTIVE_BASE));
        assert!(desc.contains("npm create vite"));
        // bash has no extra hint
        let bash_def = ShellTool::bash().definition();
        assert_eq!(
            bash_def.parameters["properties"]["interactive"]["description"],
            INTERACTIVE_BASE
        );
    }

    #[test]
    fn job_mode_label_variants() {
        assert_eq!(job_mode_label(false, None), "ephemeral");
        assert_eq!(job_mode_label(true, None), "persistent");
        assert_eq!(job_mode_label(true, Some(0)), "persistent, ttl=off");
        assert_eq!(job_mode_label(true, Some(30)), "persistent, idle ttl=30s");
    }
}
