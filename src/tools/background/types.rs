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
    pub id: u64,
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
        if self.overflow.load(Ordering::Relaxed) {
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

pub(crate) fn force_kill_pid(pid: u32) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        let status = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .status()?;
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
        let status = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()?;
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
    pub status: ProcessStatus,
    pub interactive: bool,
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
}
