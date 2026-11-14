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

/// Сколько ждать конца вывода после выхода команды. Дольше — значит пайп
/// держит запущенный ею процесс.
const PIPE_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// Пометка для модели: команда вышла, но запущенный ею процесс держит вывод.
const PIPES_HELD_MARKER: &str = "\n[the command exited, but a process it started is still running and holding its output open; later output is not captured. Start servers and watchers with background=true]";

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
                // Raw bytes all the way through, as the doc above says. An
                // earlier version decoded each chunk lossily and re-encoded
                // it, which defeated whole-stream encoding detection: a
                // UTF-16 child (wsl.exe writes its diagnostics that way) has
                // its newline as `0A 00`, so `read_until` hands back
                // half-aligned chunks that cannot be decoded in isolation.
                // One decode of the finished buffer handles it — see
                // `util::decode_process_output` at the collection site.
                let remaining =
                    FOREGROUND_OUTPUT_CAP_BYTES.saturating_sub(budget.load(Ordering::Relaxed));
                if remaining == 0 {
                    truncated.store(true, Ordering::Relaxed);
                } else {
                    let take = remaining.min(chunk.len());
                    if take < chunk.len() {
                        truncated.store(true, Ordering::Relaxed);
                    }
                    // Cutting mid-character is fine here: it only happens at
                    // the 1 MiB cap, which already appends a marker, and the
                    // final decode is lossy by design.
                    dest.lock().unwrap().extend_from_slice(&chunk[..take]);
                    budget.fetch_add(take, Ordering::Relaxed);
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

        run_foreground(&self.shell, argv, command).await
    }
}

/// Что модель узнаёт о завершённой команде: вывод с stderr (предупреждения
/// компилятора живут там), код выхода при провале, пустой вывод — словами.
fn command_result(output: String, status: &std::process::ExitStatus) -> String {
    let mut result = output;
    if !status.success() {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(&match status.code() {
            Some(code) => format!("[exit code {code}]"),
            None => "[terminated by a signal]".to_string(),
        });
    } else if result.trim().is_empty() {
        result = "(command completed successfully with no output)".to_string();
    }
    result
}

/// Склеить потоки для ответа модели. Пустой поток разделителя не добавляет.
fn combine_output(stdout: &str, stderr: &str) -> String {
    let mut result = String::from(stdout);
    if !stderr.is_empty() {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(stderr);
    }
    result
}

/// Дождаться читателей пайпов в пределах [`PIPE_DRAIN_GRACE`]. `false` — кто-то
/// ещё держит пайп; недочитанных гасит падение их хэндлов.
async fn drain_readers(readers: &mut [Option<crate::util::AbortOnDrop<()>>]) -> bool {
    let all = async {
        for slot in readers.iter_mut() {
            if let Some(task) = slot.as_mut() {
                let _ = task.await;
                *slot = None;
            }
        }
    };
    tokio::time::timeout(PIPE_DRAIN_GRACE, all).await.is_ok()
}

/// Передний план: запустить, дождаться в пределах таймаута, вернуть вывод.
/// Ровный сосед `run_background` и `run_interactive`.
async fn run_foreground(shell: &Shell, argv: Vec<String>, command: &str) -> ToolResult {
    let mut cmd = Command::new(shell.name);
    cmd.args(&argv);
    // stdin не наследуется: в `--acp` это JSON-RPC вызывающего, в TUI —
    // клавиатура. Команда, ждущая ввода, получает EOF, а не чужие байты.
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    // Метка «запущено инструментом агента»: trust и secrets за ней отказывают.
    cmd.env(crate::secrets::human::AGENT_CHILD_ENV, "1");
    // Without this, dropping `child` (e.g. this future being cancelled)
    // would leave the process running with no supervisor left to kill
    // it — the same leak class as the unbounded-hang bug this fix
    // targets, just via a different path.
    cmd.kill_on_drop(true);
    // Своя группа: дерево команды гасится одним `kill(-pgid)`, и внуки
    // (cargo → rustc, npm → node) не переживают оболочку.
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return ToolResult::error(&format!("Failed to execute command: {e}")),
    };
    let pid = child.id().unwrap_or(0);
    // Объявлено после `child`, значит падает раньше: отменённый ход гасит всё
    // дерево до того, как `kill_on_drop` добьёт один корень.
    let mut tree = background::ProcessTree::adopt(&child);
    debug_log::log_json(
        "shell.foreground.spawned",
        &json!({
            "tool": shell.name,
            "display": shell.display,
            "command": command,
            "argv": argv,
            "pid": pid,
            "creation_flags_detached_process": false,
        }),
    );

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

    let reader = |pipe: Option<Box<dyn tokio::io::AsyncRead + Unpin + Send>>,
                  dest: &Arc<Mutex<Vec<u8>>>| {
        pipe.map(|pipe| {
            crate::util::AbortOnDrop::spawn(capped_pipe_reader(
                pipe,
                Arc::clone(dest),
                Arc::clone(&budget),
                Arc::clone(&truncated),
            ))
        })
    };
    let mut readers = [
        reader(
            child.stdout.take().map(|out| Box::new(out) as Box<_>),
            &stdout_buf,
        ),
        reader(
            child.stderr.take().map(|err| Box::new(err) as Box<_>),
            &stderr_buf,
        ),
    ];

    let timed_out = tokio::select! {
        _ = child.wait() => false,
        _ = tokio::time::sleep(std::time::Duration::from_secs(FOREGROUND_TIMEOUT_SECS)) => true,
    };

    if timed_out {
        debug_log::log(
            "shell.foreground_timeout",
            format!(
                "{} pid={pid} exceeded {FOREGROUND_TIMEOUT_SECS}s, killing tree",
                shell.name
            ),
        );
        tracing::warn!(
            "{} command timed out after {FOREGROUND_TIMEOUT_SECS}s (pid={pid}), killing process tree",
            shell.name
        );
        tree.kill();
    }
    let exit_status = child.wait().await.ok().filter(|_| !timed_out);
    // Корень собран: его PID может достаться чужому процессу, бить по нему нельзя.
    tree.release();

    // EOF приходит, только когда пайп закрыли все его держатели. Внук,
    // запущенный командой в фон (`server &`, `start /b`), держит его сколько угодно.
    // Держателя не убиваем: так держит пайп и нарочно запущенный `start
    // notepad`. Просто перестаём читать — недочитанных гасит падение хэндлов.
    let pipes_held = !drain_readers(&mut readers).await;

    // One decode per stream, on the complete buffer: this is the only
    // place that can tell UTF-16 child output from UTF-8.
    let stdout = crate::util::decode_process_output(&stdout_buf.lock().unwrap()).into_owned();
    let stderr = crate::util::decode_process_output(&stderr_buf.lock().unwrap()).into_owned();
    // Пометки — после обоих потоков: модель видит их последними, что бы ни вернулось.
    let mut output = combine_output(&stdout, &stderr);
    if truncated.load(Ordering::Relaxed) {
        output.push_str(FOREGROUND_TRUNCATION_MARKER);
    }
    if pipes_held {
        output.push_str(PIPES_HELD_MARKER);
    }
    debug_log::log_json(
        "shell.foreground.output",
        &json!({
            "tool": shell.name,
            "display": shell.display,
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
        let mut result = output;
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
            let result = command_result(output, &status);
            if !status.success() {
                debug_log::log_json(
                    "shell.foreground.nonzero_exit",
                    &json!({
                        "tool": shell.name,
                        "display": shell.display,
                        "command": command,
                        "argv": argv,
                        "pid": pid,
                        "exit_code": status.code(),
                        "result_chars": result.chars().count(),
                        "result": result,
                    }),
                );
                return ToolResult::error(&result);
            }
            ToolResult::success(&result)
        }
        None => {
            debug_log::log_json(
                "shell.foreground.missing_exit_status",
                &json!({
                    "tool": shell.name,
                    "display": shell.display,
                    "command": command,
                    "argv": argv,
                    "pid": pid,
                }),
            );
            ToolResult::error("Failed to execute command: could not determine exit status")
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

    fn exit_status(code: i32) -> std::process::ExitStatus {
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt;
        #[cfg(windows)]
        use std::os::windows::process::ExitStatusExt;
        #[cfg(unix)]
        return std::process::ExitStatus::from_raw(code << 8);
        #[cfg(windows)]
        return std::process::ExitStatus::from_raw(code as u32);
    }

    /// Предупреждения компилятора живут в stderr: успех не должен их терять.
    #[test]
    fn a_successful_command_keeps_its_stderr() {
        let result = command_result(combine_output("built", "warning: unused"), &exit_status(0));
        assert_eq!(result, "built\nwarning: unused");
    }

    /// Провал без вывода (`grep` ничего не нашёл) раньше был пустой строкой.
    #[test]
    fn a_failed_command_reports_its_exit_code() {
        assert_eq!(
            command_result(String::new(), &exit_status(1)),
            "[exit code 1]"
        );
        assert_eq!(
            command_result(combine_output("", "boom"), &exit_status(101)),
            "boom\n[exit code 101]"
        );
    }

    #[test]
    fn an_empty_success_says_so() {
        assert_eq!(
            command_result(combine_output("", "  "), &exit_status(0)),
            "(command completed successfully with no output)"
        );
    }

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

    /// Оболочка и команда, печатающая значение метки агента (на Windows — PowerShell).
    fn probe() -> (Shell, String) {
        let name = crate::secrets::human::AGENT_CHILD_ENV;
        if cfg!(windows) {
            (
                Shell::powershell(),
                format!("Write-Output \"[$env:{name}]\""),
            )
        } else {
            (Shell::bash(), format!("echo \"[${name}]\""))
        }
    }

    #[tokio::test]
    async fn a_foreground_command_is_marked_as_an_agent_child() {
        let (shell, command) = probe();
        let argv = (shell.make_args)(&command);
        let result = run_foreground(&shell, argv, &command).await;
        assert!(result.content.contains("[1]"), "{}", result.content);
    }

    #[tokio::test]
    async fn a_background_command_is_marked_as_an_agent_child() {
        // Свой запуск без оболочки инструмента: без консоли (DETACHED_PROCESS) PowerShell молчит.
        let name = crate::secrets::human::AGENT_CHILD_ENV;
        let (program, flag, command) = if cfg!(windows) {
            ("cmd", "/C", format!("echo [%{name}%]"))
        } else {
            ("sh", "-c", format!("echo \"[${name}]\""))
        };
        let mut cmd = Command::new(program);
        cmd.args([flag, command.as_str()]);
        let outcome =
            background::spawn_background(cmd, command, program.to_string(), 5.0, false, None)
                .await
                .unwrap();
        assert!(
            outcome.initial_output.contains("[1]"),
            "{}",
            outcome.initial_output
        );
    }

    // На Windows вывод ConPTY (служебные ESC-последовательности) путает определение UTF-16
    // в `sanitize_terminal_output`, поэтому проверка живёт на Unix; строка с меткой общая.
    #[cfg(unix)]
    #[test]
    fn a_pty_command_is_marked_as_an_agent_child() {
        let name = crate::secrets::human::AGENT_CHILD_ENV;
        let (program, flag, command) = if cfg!(windows) {
            ("cmd", "/C", format!("echo [%{name}%]"))
        } else {
            ("sh", "-c", format!("echo \"[${name}]\""))
        };
        let argv = vec![flag.to_string(), command.clone()];
        // Читатель PTY висит в блокирующем потоке: обычный рантайм теста ждал бы его вечно.
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let outcome = runtime
            .block_on(background::spawn_interactive(
                program,
                &argv,
                None,
                command,
                program.to_string(),
                5.0,
                100,
                30,
                false,
                None,
            ))
            .unwrap();
        runtime.shutdown_background();
        // Терминал у ребёнка настоящий, поэтому защищает именно метка, а не проверка TTY.
        assert!(
            outcome.initial_output.contains("[1]"),
            "{}",
            outcome.initial_output
        );
    }
}
