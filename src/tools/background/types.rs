//! Shared vocabulary for background processes: status, the process handle, and
//! the small helpers (output sanitizing, force-kill) used by both the registry
//! and the spawners.

use regex::Regex;
use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::mpsc;

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

/// Control messages sent to a process's supervisor task.
#[derive(Debug)]
pub(crate) enum BgCmd {
    Kill(tokio::sync::oneshot::Sender<std::io::Result<()>>),
}

pub(crate) type OutputBuffer = Arc<std::sync::Mutex<Vec<u8>>>;
pub(crate) type StatusSlot = Arc<std::sync::Mutex<ProcessStatus>>;
pub(crate) type StdinWriter = Arc<std::sync::Mutex<Box<dyn Write + Send>>>;
pub(crate) type ActivitySlot = Arc<std::sync::Mutex<chrono::DateTime<chrono::Utc>>>;

pub const DEFAULT_PERSISTENT_TTL_SECS: u64 = 30 * 60;
pub(crate) const MAX_BUFFER_BYTES: usize = 256 * 1024;

static ANSI_ESCAPE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"\x1b(?:\[[0-9;?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\)|[@-Z\\-_])")
        .expect("hardcoded regex is valid")
});

pub struct BackgroundHandle {
    pub pid: Option<u32>,
    pub command: String,
    pub shell: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub last_activity_at: ActivitySlot,
    pub(crate) buffer: OutputBuffer,
    pub(crate) overflow: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) status: StatusSlot,
    pub(crate) cmd_tx: mpsc::UnboundedSender<BgCmd>,
    pub(crate) writer: Option<StdinWriter>,
    pub(crate) interactive: bool,
    pub(crate) persistent: bool,
    pub(crate) ttl_secs: Option<u64>,
}

impl BackgroundHandle {
    pub(crate) async fn status(&self) -> ProcessStatus {
        self.status.lock().unwrap().clone()
    }

    pub(crate) async fn drain_output(&self) -> String {
        let bytes = {
            let mut b = self.buffer.lock().unwrap();
            std::mem::take(&mut *b)
        };
        let text = sanitize_terminal_output(&bytes);
        // swap(false) so the marker fires once per actual overflow instead of
        // on every subsequent read — a sticky flag would keep telling the
        // model output was dropped long after the buffer caught up.
        if self.overflow.swap(false, Ordering::Relaxed) {
            format!("{text}\n[output buffer overflowed — some output was dropped]\n")
        } else {
            text
        }
    }

    pub(crate) async fn kill(&self) -> std::io::Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = self.cmd_tx.send(BgCmd::Kill(tx));
        rx.await.unwrap_or(Ok(()))
    }

    pub(crate) fn touch(&self) {
        *self.last_activity_at.lock().unwrap() = chrono::Utc::now();
    }
}

pub(crate) fn sanitize_terminal_output(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let without_ansi = ANSI_ESCAPE_RE.replace_all(&text, "");
    without_ansi
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim_matches('\n')
        .to_string()
}

pub(crate) fn is_running_status(status: &ProcessStatus) -> bool {
    matches!(status, ProcessStatus::Running)
}

/// Force-kill a background job's process (tree on Windows, process group on
/// Unix) by pid. Async so it never blocks a tokio runtime worker — this is
/// called from supervisor tasks reached via `.await`, and the old
/// `std::process::Command::status()` was a synchronous blocking syscall on
/// those workers.
pub(crate) async fn force_kill_pid(pid: u32) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        // /T kills the entire process tree (child + grandchild processes).
        let status = tokio::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .status()
            .await?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "taskkill failed for pid={pid} with status {status}"
            )))
        }
    }
    #[cfg(not(windows))]
    {
        // Background jobs are spawned in their own process group (pgid ==
        // pid, see `process_group(0)` in spawn.rs), so `-<pid>` targets the
        // whole group: killing just the leader (e.g. `bash -c "npm run
        // dev"`) would orphan grandchildren (node/vite) that keep the port
        // bound. Fall back to a plain-pid kill if group-kill errors — e.g.
        // the process never got its own group, or already exited.
        let group_status = tokio::process::Command::new("kill")
            .args(["-9", &format!("-{pid}")])
            .status()
            .await?;
        if group_status.success() {
            return Ok(());
        }
        let status = tokio::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()
            .await?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "kill -9 failed for pid={pid} with status {status}"
            )))
        }
    }
}

pub struct SpawnOutcome {
    pub id: u64,
    pub initial_output: String,
    pub persistent: bool,
    pub ttl_secs: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ProcessSnapshot {
    pub id: u64,
    pub pid: Option<u32>,
    pub shell: String,
    pub command: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub last_activity_at: chrono::DateTime<chrono::Utc>,
    pub status: ProcessStatus,
    pub interactive: bool,
    pub persistent: bool,
    pub ttl_secs: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_label_variants() {
        assert_eq!(ProcessStatus::Running.label(), "running");
        assert_eq!(ProcessStatus::Finished(Some(0)).label(), "finished(exit=0)");
        assert_eq!(ProcessStatus::Finished(Some(2)).label(), "finished(exit=2)");
        assert_eq!(ProcessStatus::Finished(None).label(), "finished");
    }

    #[test]
    fn sanitize_strips_ansi_and_normalizes_newlines() {
        let raw = b"\x1b[31mred\x1b[0m\r\nline2\r";
        assert_eq!(sanitize_terminal_output(raw), "red\nline2");
    }

    #[test]
    fn sanitize_trims_surrounding_newlines() {
        assert_eq!(sanitize_terminal_output(b"\n\nhello\n\n"), "hello");
    }

    #[test]
    fn is_running_only_for_running() {
        assert!(is_running_status(&ProcessStatus::Running));
        assert!(!is_running_status(&ProcessStatus::Finished(None)));
    }

    fn test_handle(overflowed: bool) -> BackgroundHandle {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<BgCmd>();
        BackgroundHandle {
            pid: None,
            command: "test".to_string(),
            shell: "bash".to_string(),
            started_at: chrono::Utc::now(),
            last_activity_at: Arc::new(std::sync::Mutex::new(chrono::Utc::now())),
            buffer: Arc::new(std::sync::Mutex::new(b"hi\n".to_vec())),
            overflow: Arc::new(std::sync::atomic::AtomicBool::new(overflowed)),
            status: Arc::new(std::sync::Mutex::new(ProcessStatus::Running)),
            cmd_tx,
            writer: None,
            interactive: false,
            persistent: false,
            ttl_secs: None,
        }
    }

    #[tokio::test]
    async fn overflow_marker_is_one_shot() {
        let handle = test_handle(true);
        let first = handle.drain_output().await;
        assert!(first.contains("[output buffer overflowed — some output was dropped]"));

        // No new bytes and no fresh overflow: the flag must not still be set,
        // so a second read must NOT repeat the marker.
        *handle.buffer.lock().unwrap() = b"more\n".to_vec();
        let second = handle.drain_output().await;
        assert!(!second.contains("overflowed"));
        assert_eq!(second, "more");
    }

    #[tokio::test]
    async fn drain_output_without_overflow_has_no_marker() {
        let handle = test_handle(false);
        let out = handle.drain_output().await;
        assert_eq!(out, "hi");
        assert!(!out.contains("overflowed"));
    }
}
