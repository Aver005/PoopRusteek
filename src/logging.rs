//! Tracing setup + the bridge from ERROR-level logs to the in-UI red error
//! indicator.
//!
//! Three sinks, layered:
//! - **`pooprusteek.log`** — everything at the env-filter level (default
//!   `pooprusteek=info`). The general-purpose log.
//! - **`errors.log`** — ERROR events only, from any target. The at-a-glance
//!   "what went wrong" file. In TUI mode the raw process **stderr fd** is
//!   also redirected here, so output that bypasses `tracing` entirely — ONNX
//!   Runtime's C++ logger, panics — lands in the file instead of corrupting
//!   the ratatui frame (invariant #10).
//! - **the app** — a custom layer forwards each ERROR into the event channel
//!   as [`AppEvent::ErrorLogged`], which drives the red panel/status marker.
//!
//! The stderr redirect is best-effort and TUI-only: `--acp` owns stdout for
//! JSON-RPC and `--proxy` uses stdout as its event log, so neither should
//! have stderr moved out from under it.

use crate::app::events::AppEvent;
use crate::config::Config;
use std::sync::OnceLock;
use tokio::sync::mpsc::UnboundedSender;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, fmt};

/// Sink for the ERROR-signal layer. Set once, after the app's event channel
/// exists (see [`set_error_sink`]); errors logged before that only reach the
/// log files — there is no UI yet to mark.
static ERROR_SINK: OnceLock<UnboundedSender<AppEvent>> = OnceLock::new();

/// Wire the ERROR-signal layer to the app's event channel. Idempotent — a
/// second call is ignored, so a `/rag reload`-style re-init can't detach it.
pub fn set_error_sink(tx: UnboundedSender<AppEvent>) {
    let _ = ERROR_SINK.set(tx);
}

/// Configure tracing. `redirect_stderr` should be true only in TUI mode.
pub fn setup(redirect_stderr: bool) {
    let dir = Config::data_dir();
    let _ = std::fs::create_dir_all(&dir);

    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("pooprusteek=info"));

    // Falls back to no layer (not a crash) when a file can't be opened —
    // silence beats a corrupted frame, same as the pre-layer behavior.
    let main_layer = open_append(&dir.join("pooprusteek.log")).map(|f| {
        fmt::layer()
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(f))
            .with_filter(env_filter)
    });
    let errors_layer = open_append(&dir.join("errors.log")).map(|f| {
        fmt::layer()
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(f))
            .with_filter(LevelFilter::ERROR)
    });
    let signal_layer = ErrorSignalLayer.with_filter(LevelFilter::ERROR);

    tracing_subscriber::registry()
        .with(main_layer)
        .with(errors_layer)
        .with(signal_layer)
        .init();

    if redirect_stderr {
        // Best-effort; on failure the app simply keeps its normal stderr.
        let _ = redirect_stderr_to(&dir.join("errors.log"));
    }
}

fn open_append(path: &std::path::Path) -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

/// Forwards every ERROR event into [`ERROR_SINK`]. No writer of its own — it
/// only signals; the text is written by the `errors.log`/`pooprusteek.log`
/// fmt layers.
struct ErrorSignalLayer;

impl<S> Layer<S> for ErrorSignalLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        // The per-layer LevelFilter already gates this; belt-and-braces so
        // the layer stays correct if ever attached without that filter.
        if *event.metadata().level() != Level::ERROR {
            return;
        }
        let Some(tx) = ERROR_SINK.get() else {
            return;
        };
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let message = if visitor.0.is_empty() {
            event.metadata().target().to_string()
        } else {
            visitor.0
        };
        // Non-blocking and thread-safe. Drop on failure and NEVER log here —
        // logging inside the log path would recurse.
        let _ = tx.send(AppEvent::ErrorLogged { message });
    }
}

/// Pulls the `message` field (the format string of `error!(...)`) out of a
/// tracing event.
#[derive(Default)]
struct MessageVisitor(String);

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

/// Point the OS-level stderr at `path`. Anything the process (or a loaded
/// native library) writes to fd 2 / `STD_ERROR_HANDLE` afterwards goes to the
/// file. Done early, before ONNX Runtime's DLL loads, so its C++ logger
/// inherits the redirected handle.
#[cfg(unix)]
fn redirect_stderr_to(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    // SAFETY: dup2 duplicates a valid fd onto STDERR_FILENO; both are valid.
    // After the call fd 2 references the same open file description, so
    // dropping `f` (closing its own fd) leaves stderr pointed at the file.
    let r = unsafe { libc::dup2(f.as_raw_fd(), libc::STDERR_FILENO) };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
fn redirect_stderr_to(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, SetStdHandle};
    let f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    // SAFETY: SetStdHandle points STD_ERROR_HANDLE at our valid file handle.
    let ok = unsafe { SetStdHandle(STD_ERROR_HANDLE, f.as_raw_handle() as _) };
    // The handle must outlive the process; leak the File so it isn't closed.
    std::mem::forget(f);
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn redirect_stderr_to(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The errors.log layer must record ERROR events and drop lower levels —
    /// the whole point of the file. Uses a scoped subscriber so it doesn't
    /// touch (or need) the process-global default.
    #[test]
    fn errors_layer_records_only_error_level() {
        let dir = std::env::temp_dir().join("pooprusteek_logging_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("errors_only.log");
        let _ = std::fs::remove_file(&path);

        let file = open_append(&path).expect("open temp errors log");
        let layer = fmt::layer()
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .with_filter(LevelFilter::ERROR);
        let subscriber = tracing_subscriber::registry().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("info line that must be filtered out");
            tracing::warn!("warn line that must be filtered out");
            tracing::error!("boom the embedder died");
        });

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains("boom the embedder died"),
            "errors.log should contain the ERROR event, got: {contents:?}"
        );
        assert!(
            !contents.contains("must be filtered out"),
            "errors.log must not contain info/warn, got: {contents:?}"
        );
        let _ = std::fs::remove_file(&path);
    }
}
