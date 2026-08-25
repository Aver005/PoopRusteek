//! Spawning background processes: detached pipe-based jobs and PTY-based
//! interactive jobs. Each spawns reader/waiter/kill tasks, registers a handle,
//! and returns an initial output snapshot.

use super::registry::insert_handle;
use super::types::{
    ActivitySlot, BgCmd, MAX_BUFFER_BYTES, OutputBuffer, ProcessStatus, SpawnOutcome, StatusSlot,
    StdinWriter, force_kill_pid, sanitize_terminal_output,
};
use std::io::Read;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

/// Drain a child's stdout/stderr pipe into `buffer` line-by-line until EOF.
///
/// Uses `read_until` on raw bytes rather than `read_line` on a `String`:
/// `read_line` fails the whole read with `InvalidData` on the first non-UTF8
/// byte, and the old call sites treated that error like EOF (`break`) — a
/// child that ever wrote one invalid byte (binary output, a mid-multibyte
/// truncated write, a non-UTF8 locale) would silently go quiet forever while
/// still showing as `running`. Reading raw bytes can't fail on encoding, so
/// the reader survives arbitrary output; `from_utf8_lossy` is applied once
/// here (replacement chars for anything invalid) and again defensively by
/// `sanitize_terminal_output` at drain time.
async fn pipe_reader_loop<R: AsyncRead + Unpin + Send + 'static>(
    pipe: R,
    buffer: OutputBuffer,
    overflow: Arc<std::sync::atomic::AtomicBool>,
    activity: ActivitySlot,
) {
    let mut reader = BufReader::new(pipe);
    let mut raw_line: Vec<u8> = Vec::new();
    loop {
        raw_line.clear();
        match reader.read_until(b'\n', &mut raw_line).await {
            Ok(0) => break,
            Ok(_) => {
                // Raw bytes: decoding per line would break whole-stream
                // encoding detection (a UTF-16 child's newline is `0A 00`, so
                // chunks arrive half-aligned). `sanitize_terminal_output`
                // decodes the finished buffer once.
                let mut b = buffer.lock().unwrap();
                if b.len() + raw_line.len() <= MAX_BUFFER_BYTES {
                    b.extend_from_slice(&raw_line);
                } else {
                    overflow.store(true, Ordering::Relaxed);
                }
                *activity.lock().unwrap() = chrono::Utc::now();
            }
            // read_until on raw bytes only errors on genuine I/O failure
            // (e.g. broken pipe), never on encoding — safe to stop here.
            Err(_) => break,
        }
    }
}

/// Detached pipe-based background process (stdin = null, stdout/stderr piped).
/// Use for long-running non-interactive commands (servers, watchers).
pub async fn spawn_background(
    mut command: Command,
    command_str: String,
    shell: String,
    capture_secs: f64,
    persistent: bool,
    ttl_secs: Option<u64>,
) -> Result<SpawnOutcome, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // DETACHED_PROCESS: prevent console sharing on Windows so the child
    // cannot corrupt the TUI's alternate screen buffer or console mode.
    #[cfg(windows)]
    {
        command.as_std_mut().creation_flags(0x00000008);
    }

    // Put the child in its own process group (pgid == its pid) so
    // `force_kill_pid` can kill `-<pid>` (the whole group) instead of just
    // the leader. Jobs are typically `bash -c "npm run dev"`: killing only
    // the leader orphans grandchildren (node/vite) that keep the port bound.
    // Windows achieves the equivalent via `taskkill /T` (process tree).
    #[cfg(unix)]
    {
        command.process_group(0);
    }

    let mut child: Child = command
        .spawn()
        .map_err(|e| format!("Failed to spawn command: {e}"))?;
    let child_pid = child.id();

    // Bind to the kill-on-close job so a force-close of pooprusteek can't
    // orphan this DETACHED_PROCESS child (see `win_job`).
    #[cfg(windows)]
    if let Some(handle) = child.raw_handle() {
        super::types::win_job::assign_child_handle(handle);
    }

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let buffer: OutputBuffer = Arc::new(std::sync::Mutex::new(Vec::new()));
    let overflow = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let status: StatusSlot = Arc::new(std::sync::Mutex::new(ProcessStatus::Running));
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<BgCmd>();
    let started_at = chrono::Utc::now();
    let activity: ActivitySlot = Arc::new(std::sync::Mutex::new(started_at));

    if let Some(out) = stdout {
        let buf = Arc::clone(&buffer);
        let ovf = Arc::clone(&overflow);
        let activity = Arc::clone(&activity);
        tokio::spawn(pipe_reader_loop(out, buf, ovf, activity));
    }
    if let Some(err) = stderr {
        let buf = Arc::clone(&buffer);
        let ovf = Arc::clone(&overflow);
        let activity = Arc::clone(&activity);
        tokio::spawn(pipe_reader_loop(err, buf, ovf, activity));
    }

    {
        let status = Arc::clone(&status);
        tokio::spawn(async move {
            let mut child = child;
            loop {
                tokio::select! {
                    result = child.wait() => {
                        let code = result.ok().and_then(|s| s.code());
                        *status.lock().unwrap() = ProcessStatus::Finished(code);
                        break;
                    }
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(BgCmd::Kill(tx)) => {
                                let res = match child_pid {
                                    Some(pid) => force_kill_pid(pid).await,
                                    None => child.kill().await,
                                };
                                let _ = tx.send(res);
                            }
                            None => {
                                let _ = match child_pid {
                                    Some(pid) => force_kill_pid(pid).await,
                                    None => child.kill().await,
                                };
                                break;
                            }
                        }
                    }
                }
            }
        });
    }

    let id = insert_handle(
        child_pid,
        command_str,
        shell,
        started_at,
        Arc::clone(&activity),
        Arc::clone(&buffer),
        Arc::clone(&overflow),
        Arc::clone(&status),
        cmd_tx,
        None,
        false,
        persistent,
        ttl_secs,
    )
    .await;

    let capped = capture_secs.clamp(0.0, 10.0);
    if capped > 0.0 {
        tokio::time::sleep(std::time::Duration::from_secs_f64(capped)).await;
    }

    let initial_output = {
        let mut b = buffer.lock().unwrap();
        sanitize_terminal_output(&std::mem::take(&mut *b))
    };

    Ok(SpawnOutcome {
        id,
        initial_output,
        persistent,
        ttl_secs,
    })
}

/// Detached PTY-based interactive process. The command runs in a pseudo-terminal
/// so arrow-key menus, REPLs and other TUI selectors work. Use `shell_input` to
/// send keystrokes; `shell_output`/`shell_kill`/`shell_list` work as usual.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_interactive(
    program: &str,
    args: &[String],
    cwd: Option<&str>,
    command_str: String,
    shell: String,
    capture_secs: f64,
    cols: u16,
    rows: u16,
    persistent: bool,
    ttl_secs: Option<u64>,
) -> Result<SpawnOutcome, String> {
    use portable_pty::{ChildKiller, CommandBuilder, PtySize, PtySystem, native_pty_system};

    let pty_system: Box<dyn PtySystem + Send> = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| e.to_string())?;

    let mut cmd = CommandBuilder::new(program);
    for arg in args {
        cmd.arg(arg);
    }
    if let Some(dir) = cwd {
        cmd.cwd(dir);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");

    let child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
    let child_pid = child.process_id();
    // Bind the PTY child to the kill-on-close job so a force-close can't orphan
    // it (see `win_job`). Only the pid is available here.
    #[cfg(windows)]
    if let Some(pid) = child_pid {
        super::types::win_job::assign_pid(pid);
    }
    let killer: Box<dyn ChildKiller + Send + Sync> = child.clone_killer();

    let reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
    let writer = pair.master.take_writer().map_err(|e| e.to_string())?;

    // Slave is no longer needed after spawn; keep master alive in the wait task.
    drop(pair.slave);
    let master = pair.master;

    let buffer: OutputBuffer = Arc::new(std::sync::Mutex::new(Vec::new()));
    let overflow = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let status: StatusSlot = Arc::new(std::sync::Mutex::new(ProcessStatus::Running));
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<BgCmd>();
    let started_at = chrono::Utc::now();
    let activity: ActivitySlot = Arc::new(std::sync::Mutex::new(started_at));

    // Blocking reader: PTY output is synchronous Read.
    {
        let buffer = Arc::clone(&buffer);
        let overflow = Arc::clone(&overflow);
        let activity = Arc::clone(&activity);
        tokio::task::spawn_blocking(move || {
            let mut reader = reader;
            let mut chunk = [0u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut b = buffer.lock().unwrap();
                        if b.len() + n <= MAX_BUFFER_BYTES {
                            b.extend_from_slice(&chunk[..n]);
                        } else {
                            overflow.store(true, Ordering::Relaxed);
                        }
                        *activity.lock().unwrap() = chrono::Utc::now();
                    }
                    Err(_) => break,
                }
            }
        });
    }

    // Waiter: holds child + master so the pty stays open until the process exits.
    {
        let status = Arc::clone(&status);
        tokio::task::spawn_blocking(move || {
            let mut child = child;
            let exit = child.wait();
            let code = exit.ok().map(|s| s.exit_code() as i32);
            *status.lock().unwrap() = ProcessStatus::Finished(code);
            // master drops here -> reader EOFs -> reader task exits.
            drop(master);
        });
    }

    // Kill command handler.
    {
        tokio::spawn(async move {
            let mut killer = killer;
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    BgCmd::Kill(tx) => {
                        let res = match child_pid {
                            Some(pid) => force_kill_pid(pid).await,
                            None => killer.kill(),
                        };
                        let _ = tx.send(res);
                    }
                }
            }
            // Channel closed (handle dropped) — ensure the process is reaped.
            let _ = match child_pid {
                Some(pid) => force_kill_pid(pid).await,
                None => killer.kill(),
            };
        });
    }

    let writer: StdinWriter = Arc::new(std::sync::Mutex::new(writer));

    let id = insert_handle(
        child_pid,
        command_str,
        shell,
        started_at,
        Arc::clone(&activity),
        Arc::clone(&buffer),
        Arc::clone(&overflow),
        Arc::clone(&status),
        cmd_tx,
        Some(Arc::clone(&writer)),
        true,
        persistent,
        ttl_secs,
    )
    .await;

    let capped = capture_secs.clamp(0.0, 10.0);
    if capped > 0.0 {
        tokio::time::sleep(std::time::Duration::from_secs_f64(capped)).await;
    }

    let initial_output = {
        let mut b = buffer.lock().unwrap();
        sanitize_terminal_output(&std::mem::take(&mut *b))
    };

    Ok(SpawnOutcome {
        id,
        initial_output,
        persistent,
        ttl_secs,
    })
}
