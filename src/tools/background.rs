use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc};

#[derive(Debug, Clone)]
pub enum ProcessStatus {
    Running,
    Finished(Option<i32>),
}

impl ProcessStatus {
    pub fn label(&self) -> String {
        match self {
            Self::Running => "running".to_string(),
            Self::Finished(code) => match code {
                Some(c) => format!("finished(exit={c})"),
                None => "finished".to_string(),
            },
        }
    }
}

#[derive(Debug)]
enum BgCmd {
    Kill(tokio::sync::oneshot::Sender<std::io::Result<()>>),
}

type OutputBuffer = Arc<std::sync::Mutex<Vec<u8>>>;
type StatusSlot = Arc<std::sync::Mutex<ProcessStatus>>;
type StdinWriter = Arc<std::sync::Mutex<Box<dyn Write + Send>>>;

pub struct BackgroundHandle {
    pub id: u64,
    pub command: String,
    pub shell: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    buffer: OutputBuffer,
    overflow: Arc<std::sync::atomic::AtomicBool>,
    status: StatusSlot,
    cmd_tx: mpsc::UnboundedSender<BgCmd>,
    writer: Option<StdinWriter>,
    interactive: bool,
}

impl BackgroundHandle {
    pub async fn status(&self) -> ProcessStatus {
        self.status.lock().unwrap().clone()
    }

    pub async fn drain_output(&self) -> String {
        let bytes = {
            let mut b = self.buffer.lock().unwrap();
            std::mem::take(&mut *b)
        };
        let text = String::from_utf8_lossy(&bytes).to_string();
        if self.overflow.load(Ordering::Relaxed) {
            format!("{text}\n[output buffer overflowed — some output was dropped]\n")
        } else {
            text
        }
    }

    pub async fn kill(&self) -> std::io::Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = self.cmd_tx.send(BgCmd::Kill(tx));
        rx.await.unwrap_or(Ok(()))
    }
}

const MAX_BUFFER_BYTES: usize = 256 * 1024;

pub struct BackgroundRegistry {
    next_id: u64,
    procs: HashMap<u64, Arc<BackgroundHandle>>,
}

impl BackgroundRegistry {
    fn new() -> Self {
        Self {
            next_id: 1,
            procs: HashMap::new(),
        }
    }
}

static REGISTRY: std::sync::OnceLock<Mutex<BackgroundRegistry>> = std::sync::OnceLock::new();

fn registry() -> &'static Mutex<BackgroundRegistry> {
    REGISTRY.get_or_init(|| Mutex::new(BackgroundRegistry::new()))
}

async fn insert_handle(
    command_str: String,
    shell: String,
    buffer: OutputBuffer,
    overflow: Arc<std::sync::atomic::AtomicBool>,
    status: StatusSlot,
    cmd_tx: mpsc::UnboundedSender<BgCmd>,
    writer: Option<StdinWriter>,
    interactive: bool,
) -> u64 {
    let mut reg = registry().lock().await;
    let id = reg.next_id;
    reg.next_id += 1;
    reg.procs.insert(
        id,
        Arc::new(BackgroundHandle {
            id,
            command: command_str,
            shell,
            started_at: chrono::Utc::now(),
            buffer,
            overflow,
            status,
            cmd_tx,
            writer,
            interactive,
        }),
    );
    id
}

pub struct SpawnOutcome {
    pub id: u64,
    pub initial_output: String,
    pub status: ProcessStatus,
    pub interactive: bool,
}

/// Detached pipe-based background process (stdin = null, stdout/stderr piped).
/// Use for long-running non-interactive commands (servers, watchers).
pub async fn spawn_background(
    mut command: Command,
    command_str: String,
    shell: String,
    capture_secs: f64,
) -> Result<SpawnOutcome, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child: Child = command
        .spawn()
        .map_err(|e| format!("Failed to spawn command: {e}"))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let buffer: OutputBuffer = Arc::new(std::sync::Mutex::new(Vec::new()));
    let overflow = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let status: StatusSlot = Arc::new(std::sync::Mutex::new(ProcessStatus::Running));
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<BgCmd>();

    if let Some(out) = stdout {
        let buf = Arc::clone(&buffer);
        let ovf = Arc::clone(&overflow);
        tokio::spawn(async move {
            let mut reader = BufReader::new(out);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let mut b = buf.lock().unwrap();
                        if b.len() + line.len() <= MAX_BUFFER_BYTES {
                            b.extend_from_slice(line.as_bytes());
                        } else {
                            ovf.store(true, Ordering::Relaxed);
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }
    if let Some(err) = stderr {
        let buf = Arc::clone(&buffer);
        let ovf = Arc::clone(&overflow);
        tokio::spawn(async move {
            let mut reader = BufReader::new(err);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let mut b = buf.lock().unwrap();
                        if b.len() + line.len() <= MAX_BUFFER_BYTES {
                            b.extend_from_slice(line.as_bytes());
                        } else {
                            ovf.store(true, Ordering::Relaxed);
                        }
                    }
                    Err(_) => break,
                }
            }
        });
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
                                let res = child.kill().await;
                                let _ = tx.send(res);
                            }
                            None => {
                                let _ = child.kill().await;
                                break;
                            }
                        }
                    }
                }
            }
        });
    }

    let id = insert_handle(
        command_str,
        shell,
        Arc::clone(&buffer),
        Arc::clone(&overflow),
        Arc::clone(&status),
        cmd_tx,
        None,
        false,
    )
    .await;

    let capped = capture_secs.clamp(0.0, 10.0);
    if capped > 0.0 {
        tokio::time::sleep(std::time::Duration::from_secs_f64(capped)).await;
    }

    let initial_output = {
        let mut b = buffer.lock().unwrap();
        String::from_utf8_lossy(&std::mem::take(&mut *b)).to_string()
    };
    let status_snap = status.lock().unwrap().clone();

    Ok(SpawnOutcome {
        id,
        initial_output,
        status: status_snap,
        interactive: false,
    })
}

/// Detached PTY-based interactive process. The command runs in a pseudo-terminal
/// so arrow-key menus, REPLs and other TUI selectors work. Use `shell_input` to
/// send keystrokes; `shell_output`/`shell_kill`/`shell_list` work as usual.
pub async fn spawn_interactive(
    program: &str,
    args: &[String],
    cwd: Option<&str>,
    command_str: String,
    shell: String,
    capture_secs: f64,
    cols: u16,
    rows: u16,
) -> Result<SpawnOutcome, String> {
    use portable_pty::{
        ChildKiller, CommandBuilder, PtySize, PtySystem, native_pty_system,
    };

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

    // Blocking reader: PTY output is synchronous Read.
    {
        let buffer = Arc::clone(&buffer);
        let overflow = Arc::clone(&overflow);
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
                        let res = killer.kill();
                        let _ = tx.send(res);
                    }
                }
            }
            // Channel closed (handle dropped) — ensure the process is reaped.
            let _ = killer.kill();
        });
    }

    let writer: StdinWriter = Arc::new(std::sync::Mutex::new(writer));

    let id = insert_handle(
        command_str,
        shell,
        Arc::clone(&buffer),
        Arc::clone(&overflow),
        Arc::clone(&status),
        cmd_tx,
        Some(Arc::clone(&writer)),
        true,
    )
    .await;

    let capped = capture_secs.clamp(0.0, 10.0);
    if capped > 0.0 {
        tokio::time::sleep(std::time::Duration::from_secs_f64(capped)).await;
    }

    let initial_output = {
        let mut b = buffer.lock().unwrap();
        String::from_utf8_lossy(&std::mem::take(&mut *b)).to_string()
    };
    let status_snap = status.lock().unwrap().clone();

    Ok(SpawnOutcome {
        id,
        initial_output,
        status: status_snap,
        interactive: true,
    })
}

pub async fn read_output(id: u64) -> Option<(String, ProcessStatus)> {
    let handle = {
        let reg = registry().lock().await;
        reg.procs.get(&id).cloned()
    }?;
    let status = handle.status().await;
    let out = handle.drain_output().await;
    Some((out, status))
}

pub async fn write_input(id: u64, bytes: &[u8]) -> Result<(), String> {
    let handle = {
        let reg = registry().lock().await;
        reg.procs.get(&id).cloned()
    };
    let handle = handle.ok_or_else(|| format!("No background process with id={id}"))?;
    if !handle.interactive {
        return Err("Process is not interactive; start it with interactive=true to send input.".to_string());
    }
    let writer = handle.writer.as_ref().ok_or_else(|| {
        "Process has no stdin writer (this should not happen for interactive processes).".to_string()
    })?;
    let mut guard = writer.lock().unwrap();
    guard
        .write_all(bytes)
        .map_err(|e| format!("Failed to write to process stdin: {e}"))
}

pub async fn kill_process(id: u64) -> Option<std::io::Result<()>> {
    let handle = {
        let reg = registry().lock().await;
        reg.procs.get(&id).cloned()
    }?;
    Some(handle.kill().await)
}

pub async fn list_processes() -> Vec<(u64, String, String, String, bool)> {
    let reg = registry().lock().await;
    let mut out = Vec::new();
    for (id, h) in reg.procs.iter() {
        let status = h.status().await.label();
        out.push((*id, h.shell.clone(), h.command.clone(), status, h.interactive));
    }
    out.sort_by_key(|(id, _, _, _, _)| *id);
    out
}

/// Kill every background/PTY process and clear the registry. Called on app
/// shutdown so spawn_blocking waiters unblock and the tokio runtime can exit.
pub async fn shutdown_all() {
    let handles: Vec<Arc<BackgroundHandle>> = {
        let mut reg = registry().lock().await;
        reg.procs.drain().map(|(_, h)| h).collect()
    };
    for handle in &handles {
        let _ = handle.kill().await;
    }
    // Give the OS a moment to actually terminate the children so the
    // blocking wait() calls in the spawned tasks return before we drop
    // the runtime.
    if !handles.is_empty() {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}
