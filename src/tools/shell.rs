//! Unified shell command tool.
//!
//! `bash` and `powershell` were near-identical 250-line tools differing only in
//! the executable, its argv, and a few labels. They are now one [`ShellTool`]
//! parameterized by a [`Shell`] adapter — two adapters (`bash`, `powershell`)
//! justify the seam; a third shell would be one more `Shell` constructor.

use super::*;
use crate::debug_log;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

const INTERACTIVE_BASE: &str = "If true, run in a PTY so interactive TUI menus and prompts work. Returns a process id; drive it with shell_input. Implies detached.";

/// Wall-clock budget for a foreground (non-background, non-interactive) shell
/// call. Commands the persistent-background heuristic doesn't catch (e.g. a
/// dev server started without `background=true`) would otherwise hang the
/// turn forever on `wait_with_output`. On expiry the process tree is killed
/// and whatever output was captured so far is returned as an error.
const FOREGROUND_TIMEOUT_SECS: u64 = 300;

/// Combined stdout+stderr budget for a foreground call, past which further
/// output is discarded (but still drained so the child never blocks writing
/// to a full pipe). Mirrors the intent of `background::MAX_BUFFER_BYTES` but
/// applied once, synchronously, at the end of a single call rather than
/// polled across `shell_output` reads.
const FOREGROUND_OUTPUT_CAP_BYTES: usize = 1024 * 1024;

/// Marker appended to the captured output when [`FOREGROUND_OUTPUT_CAP_BYTES`]
/// is exceeded. Text says "1 MiB" rather than interpolating the constant
/// because 1 MiB is the whole point of the cap; the `const _` assertion below
/// fails the build if the two are ever changed independently.
const FOREGROUND_TRUNCATION_MARKER: &str = "\n[output truncated at 1 MiB]";
const _: () = assert!(FOREGROUND_OUTPUT_CAP_BYTES == 1024 * 1024);

/// Read `pipe` to EOF in raw-byte chunks, appending to `dest` only while the
/// process-wide `budget` (shared across stdout+stderr) has room; past the cap
/// bytes are still consumed (never left unread, or the child would block on a
/// full pipe) but discarded, and `truncated` is set once. Byte-oriented
/// (`read_until` rather than `read_line`) so a foreground command that emits
/// non-UTF8 (binary output, a truncated multibyte write) can't kill the
/// reader the way `read_line`'s `InvalidData` would.
async fn capped_pipe_reader<R: tokio::io::AsyncRead + Unpin>(
    pipe: R,
    dest: Arc<Mutex<Vec<u8>>>,
    budget: Arc<AtomicUsize>,
    truncated: Arc<AtomicBool>,
) {
    let mut reader = BufReader::new(pipe);
    let mut chunk: Vec<u8> = Vec::new();
    loop {
        chunk.clear();
        match reader.read_until(b'\n', &mut chunk).await {
            Ok(0) => break,
            Ok(_) => {
                let text = String::from_utf8_lossy(&chunk);
                let remaining =
                    FOREGROUND_OUTPUT_CAP_BYTES.saturating_sub(budget.load(Ordering::Relaxed));
                if remaining == 0 {
                    truncated.store(true, Ordering::Relaxed);
                } else {
                    let take = remaining.min(text.len());
                    if take < text.len() {
                        truncated.store(true, Ordering::Relaxed);
                    }
                    let boundary = crate::util::truncate_at_char_boundary(&text, take);
                    dest.lock().unwrap().extend_from_slice(boundary.as_bytes());
                    budget.fetch_add(boundary.len(), Ordering::Relaxed);
                }
            }
            // read_until on raw bytes only errors on genuine I/O failure,
            // never on encoding — safe to stop here (mirrors
            // background::spawn::pipe_reader_loop).
            Err(_) => break,
        }
    }
}

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

/// Windows PowerShell 5.1 (the `powershell.exe` on PATH on every Windows box;
/// pwsh 7 is opt-in) writes piped/redirected stdout in the *OEM* codepage —
/// cp866 on ru-Windows, not UTF-8 — regardless of the console's active code
/// page. Our side reads that pipe as raw bytes and decodes with
/// `from_utf8_lossy`, so any non-ASCII (Cyrillic filenames, `git status`
/// output, etc.) comes through as mojibake. Forcing `$OutputEncoding` (how
/// PowerShell encodes objects written to the pipeline/redirected streams)
/// and `[Console]::OutputEncoding` to UTF8 before running the real command
/// makes the child emit UTF-8 bytes instead, matching what `from_utf8_lossy`
/// expects. Verified empirically: piping Cyrillic through an unpatched
/// command yields cp866 bytes (mojibake as UTF-8); with this prefix the
/// bytes round-trip as valid UTF-8.
const POWERSHELL_UTF8_PREFIX: &str =
    "$OutputEncoding=[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; ";

fn powershell_args(command: &str) -> Vec<String> {
    vec![
        "-NoProfile".to_string(),
        "-Command".to_string(),
        format!("{POWERSHELL_UTF8_PREFIX}{command}"),
    ]
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
        Self {
            shell: Shell::bash(),
        }
    }

    pub fn powershell() -> Self {
        Self {
            shell: Shell::powershell(),
        }
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
                    .or_else(|| {
                        args["ttl_seconds"]
                            .as_f64()
                            .map(|value| value.max(0.0) as u64)
                    })
                    .unwrap_or(background::DEFAULT_PERSISTENT_TTL_SECS),
            )
        } else {
            None
        };
        let forced_interactive = looks_interactive_command(command) && !interactive;
        let interactive = interactive || forced_interactive;
        let background = if interactive { false } else { background };

        let argv = (self.shell.make_args)(command);
        debug_log::log_json(
            "shell.execute.request",
            &json!({
                "tool": self.shell.name,
                "display": self.shell.display,
                "command": command,
                "argv": argv,
                "interactive": interactive,
                "background": background,
                "forced_interactive": forced_interactive,
                "wait_seconds": wait_seconds,
                "persistent": persistent,
                "ttl_secs": ttl_secs,
            }),
        );

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
            return run_background(
                &self.shell,
                cmd,
                command,
                wait_seconds,
                persistent,
                ttl_secs,
            )
            .await;
        }

        let mut cmd = Command::new(self.shell.name);
        cmd.args(&argv);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        // Without this, dropping `child` (e.g. this future being cancelled)
        // would leave the process running with no supervisor left to kill
        // it — the same leak class as the unbounded-hang bug this fix
        // targets, just via a different path.
        cmd.kill_on_drop(true);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return ToolResult::error(&format!("Failed to execute command: {e}")),
        };
        let pid = child.id().unwrap_or(0);
        debug_log::log_json(
            "shell.foreground.spawned",
            &json!({
                "tool": self.shell.name,
                "display": self.shell.display,
                "command": command,
                "argv": argv,
                "pid": pid,
                "creation_flags_detached_process": false,
            }),
        );

        // Track PID so Escape/Ctrl+C can kill the child process.
        crate::app::FOREGROUND_CHILD_PID.store(pid, Ordering::SeqCst);

        // Incremental, capped readers: a foreground call with no timeout/cap
        // (the old `wait_with_output`) hangs the whole turn forever on a
        // command the persistent-background heuristic doesn't catch (e.g.
        // `npm run dev` invoked without background=true), and buffers
        // unbounded output (`cat big.bin`) in memory. Both streams are read
        // here (never left un-drained, or the child would block writing to
        // a full pipe) but capped at a combined budget.
        let stdout_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let stderr_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let budget = Arc::new(AtomicUsize::new(0));
        let truncated = Arc::new(AtomicBool::new(false));

        let stdout_task = child.stdout.take().map(|out| {
            tokio::spawn(capped_pipe_reader(
                out,
                Arc::clone(&stdout_buf),
                Arc::clone(&budget),
                Arc::clone(&truncated),
            ))
        });
        let stderr_task = child.stderr.take().map(|err| {
            tokio::spawn(capped_pipe_reader(
                err,
                Arc::clone(&stderr_buf),
                Arc::clone(&budget),
                Arc::clone(&truncated),
            ))
        });

        let timed_out = tokio::select! {
            _ = child.wait() => false,
            _ = tokio::time::sleep(std::time::Duration::from_secs(FOREGROUND_TIMEOUT_SECS)) => true,
        };

        let exit_status = if timed_out {
            debug_log::log(
                "shell.foreground_timeout",
                format!(
                    "{} pid={pid} exceeded {FOREGROUND_TIMEOUT_SECS}s, killing tree",
                    self.shell.name
                ),
            );
            tracing::warn!(
                "{} command timed out after {FOREGROUND_TIMEOUT_SECS}s (pid={pid}), killing process tree",
                self.shell.name
            );
            if pid != 0
                && let Err(e) = background::force_kill_pid(pid).await
            {
                tracing::warn!(
                    "Failed to kill timed-out {} pid={pid}: {e}",
                    self.shell.name
                );
            }
            None
        } else {
            child.wait().await.ok()
        };

        // Let the readers observe EOF (the pipes close once the process
        // exits or is killed) so the captured output is complete rather
        // than a mid-flight snapshot.
        if let Some(task) = stdout_task {
            let _ = task.await;
        }
        if let Some(task) = stderr_task {
            let _ = task.await;
        }

        crate::app::FOREGROUND_CHILD_PID.store(0, Ordering::SeqCst);

        let mut stdout = String::from_utf8_lossy(&stdout_buf.lock().unwrap()).into_owned();
        let stderr = String::from_utf8_lossy(&stderr_buf.lock().unwrap()).into_owned();
        if truncated.load(Ordering::Relaxed) {
            // Appended to `stdout` specifically (not `stderr`) because the
            // success path below returns stdout only — a marker attached to
            // stderr would be invisible on success, silently hiding the
            // fact that output was dropped.
            stdout.push_str(FOREGROUND_TRUNCATION_MARKER);
        }
        debug_log::log_json(
            "shell.foreground.output",
            &json!({
                "tool": self.shell.name,
                "display": self.shell.display,
                "command": command,
                "pid": pid,
                "timed_out": timed_out,
                "exit_status": exit_status.as_ref().map(|status| status.code()),
                "exit_success": exit_status.as_ref().map(|status| status.success()),
                "stdout_chars": stdout.chars().count(),
                "stderr_chars": stderr.chars().count(),
                "stdout": stdout,
                "stderr": stderr,
                "truncated": truncated.load(Ordering::Relaxed),
            }),
        );

        if timed_out {
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
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(&format!(
                "[command timed out after {FOREGROUND_TIMEOUT_SECS}s and was killed]"
            ));
            return ToolResult::error(&result);
        }

        match exit_status {
            Some(status) => {
                if status.success() {
                    if stdout.trim().is_empty() && stderr.trim().is_empty() {
                        debug_log::log_json(
                            "shell.foreground.empty_success",
                            &json!({
                                "tool": self.shell.name,
                                "display": self.shell.display,
                                "command": command,
                                "argv": argv,
                                "pid": pid,
                                "exit_code": status.code(),
                                "note": "command exited successfully but produced no stdout/stderr",
                            }),
                        );
                        if self.shell.name == "powershell" {
                            // Genuinely empty output (e.g. a filter that matched
                            // nothing) is common and not a failure — flagging it
                            // as `ToolResult::error` used to make the agent treat
                            // a normal zero-result command as broken.
                            return ToolResult::success(
                                "(command completed successfully with no output)",
                            );
                        }
                    }
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
                    debug_log::log_json(
                        "shell.foreground.nonzero_exit",
                        &json!({
                            "tool": self.shell.name,
                            "display": self.shell.display,
                            "command": command,
                            "argv": argv,
                            "pid": pid,
                            "exit_code": status.code(),
                            "result_chars": result.chars().count(),
                            "result": result,
                        }),
                    );
                    ToolResult::error(&result)
                }
            }
            None => {
                debug_log::log_json(
                    "shell.foreground.missing_exit_status",
                    &json!({
                        "tool": self.shell.name,
                        "display": self.shell.display,
                        "command": command,
                        "argv": argv,
                        "pid": pid,
                    }),
                );
                ToolResult::error("Failed to execute command: could not determine exit status")
            }
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
    debug_log::log_json(
        "shell.background.request",
        &json!({
            "tool": shell.name,
            "display": shell.display,
            "command": command_str,
            "wait_seconds": wait_seconds,
            "persistent": persistent,
            "ttl_secs": ttl_secs,
        }),
    );
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
            debug_log::log_json(
                "shell.background.result",
                &json!({
                    "tool": shell.name,
                    "display": shell.display,
                    "command": command_str,
                    "job_id": outcome.id,
                    "persistent": outcome.persistent,
                    "ttl_secs": outcome.ttl_secs,
                    "initial_output_chars": outcome.initial_output.chars().count(),
                    "initial_output": outcome.initial_output,
                }),
            );
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
        Err(e) => {
            debug_log::log_json(
                "shell.background.error",
                &json!({
                    "tool": shell.name,
                    "display": shell.display,
                    "command": command_str,
                    "error": e,
                }),
            );
            ToolResult::error(&e)
        }
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
    debug_log::log_json(
        "shell.interactive.request",
        &json!({
            "tool": shell.name,
            "display": shell.display,
            "argv": argv,
            "command": command_str,
            "wait_seconds": wait_seconds,
            "forced_interactive": forced_interactive,
            "persistent": persistent,
            "ttl_secs": ttl_secs,
        }),
    );
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
            debug_log::log_json(
                "shell.interactive.result",
                &json!({
                    "tool": shell.name,
                    "display": shell.display,
                    "command": command_str,
                    "job_id": outcome.id,
                    "persistent": outcome.persistent,
                    "ttl_secs": outcome.ttl_secs,
                    "initial_output_chars": outcome.initial_output.chars().count(),
                    "initial_output": outcome.initial_output,
                }),
            );
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
        Err(e) => {
            debug_log::log_json(
                "shell.interactive.error",
                &json!({
                    "tool": shell.name,
                    "display": shell.display,
                    "command": command_str,
                    "argv": argv,
                    "error": e,
                }),
            );
            ToolResult::error(&e)
        }
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
        let argv = (Shell::powershell().make_args)("Get-ChildItem");
        assert_eq!(argv[0], "-NoProfile");
        assert_eq!(argv[1], "-Command");
        // The real command must survive verbatim, appended after the prefix.
        assert!(argv[2].ends_with("Get-ChildItem"));
    }

    #[test]
    fn powershell_args_prepend_utf8_output_encoding() {
        let argv = (Shell::powershell().make_args)("echo hi");
        assert_eq!(argv.len(), 3);
        assert_eq!(argv[2], format!("{POWERSHELL_UTF8_PREFIX}echo hi"));
        assert!(argv[2].contains("OutputEncoding"));
        assert!(argv[2].contains("UTF8"));
    }

    #[test]
    fn bash_args_are_not_prefixed_with_encoding_override() {
        // The cp866 workaround is PowerShell-specific (OEM codepage on
        // Windows); bash must not carry it.
        let argv = (Shell::bash().make_args)("echo hi");
        assert_eq!(argv, vec!["-c", "echo hi"]);
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

    async fn run_capped_reader(
        input: &[u8],
        budget: &Arc<AtomicUsize>,
        truncated: &Arc<AtomicBool>,
    ) -> Vec<u8> {
        let dest: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        capped_pipe_reader(
            input,
            Arc::clone(&dest),
            Arc::clone(budget),
            Arc::clone(truncated),
        )
        .await;
        dest.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn capped_pipe_reader_passes_through_under_budget() {
        let budget = Arc::new(AtomicUsize::new(0));
        let truncated = Arc::new(AtomicBool::new(false));
        let out = run_capped_reader(b"hello\nworld\n", &budget, &truncated).await;
        assert_eq!(out, b"hello\nworld\n");
        assert!(!truncated.load(Ordering::Relaxed));
        assert_eq!(budget.load(Ordering::Relaxed), 12);
    }

    #[tokio::test]
    async fn capped_pipe_reader_truncates_past_shared_budget() {
        // Budget already 90% consumed by a sibling stream (e.g. stdout) —
        // the cap is combined, so this reader must respect what's left.
        let budget = Arc::new(AtomicUsize::new(FOREGROUND_OUTPUT_CAP_BYTES - 5));
        let truncated = Arc::new(AtomicBool::new(false));
        let out = run_capped_reader(b"0123456789\n", &budget, &truncated).await;
        assert_eq!(out.len(), 5); // only the remaining 5 bytes of budget
        assert!(truncated.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn capped_pipe_reader_drains_without_writing_once_budget_is_full() {
        let budget = Arc::new(AtomicUsize::new(FOREGROUND_OUTPUT_CAP_BYTES));
        let truncated = Arc::new(AtomicBool::new(false));
        // A reader that returns Ok(0) only at true EOF: feed several lines,
        // all of which must be discarded (not appended) once the shared
        // budget is already exhausted, while still reading to completion.
        let out = run_capped_reader(b"line one\nline two\nline three\n", &budget, &truncated).await;
        assert!(out.is_empty());
        assert!(truncated.load(Ordering::Relaxed));
        // Budget must not grow past the cap even though more was read.
        assert_eq!(budget.load(Ordering::Relaxed), FOREGROUND_OUTPUT_CAP_BYTES);
    }

    #[tokio::test]
    async fn capped_pipe_reader_two_streams_share_one_budget() {
        // Simulates stdout+stderr both writing under the same combined cap:
        // the second reader must see the budget already consumed by the first.
        let budget = Arc::new(AtomicUsize::new(0));
        let truncated = Arc::new(AtomicBool::new(false));
        let small_cap_input = vec![b'x'; FOREGROUND_OUTPUT_CAP_BYTES - 3];
        let stdout_out = run_capped_reader(&small_cap_input, &budget, &truncated).await;
        assert_eq!(stdout_out.len(), FOREGROUND_OUTPUT_CAP_BYTES - 3);
        assert!(!truncated.load(Ordering::Relaxed));

        let stderr_out = run_capped_reader(b"overflow!\n", &budget, &truncated).await;
        assert_eq!(stderr_out.len(), 3); // only 3 bytes of shared budget left
        assert!(truncated.load(Ordering::Relaxed));
    }

    #[test]
    fn truncation_marker_text_matches_the_configured_cap() {
        assert_eq!(
            FOREGROUND_TRUNCATION_MARKER,
            "\n[output truncated at 1 MiB]"
        );
        assert!(FOREGROUND_TRUNCATION_MARKER.contains("1 MiB"));
    }

    #[test]
    fn timeout_marker_text_includes_the_configured_seconds() {
        let marker = format!("[command timed out after {FOREGROUND_TIMEOUT_SECS}s and was killed]");
        assert_eq!(marker, "[command timed out after 300s and was killed]");
    }
}
