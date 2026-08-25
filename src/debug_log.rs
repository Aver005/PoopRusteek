//! Opt-in developer log (`--debug-log`, `/debug`). Two sinks share one
//! writer: the human `[ts] [action] message` lines developers read in
//! `.dev/debug.log`, and a machine-readable JSONL stream the test harness
//! (`src/harness`) consumes as a turn trace. The agent loop is already
//! instrumented for the human sink, so the harness gets its trace without a
//! second, drift-prone set of instrumentation points.

use crate::error::{AppError, AppResult};
use chrono::Local;
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

static ENABLED: AtomicBool = AtomicBool::new(false);
static LOGGER: OnceLock<DebugLogger> = OnceLock::new();
/// Where and in which shape to open the log. Set by [`configure`] before
/// first use; absent means the developer default (`.dev/debug.log`, human).
static SINK: OnceLock<(PathBuf, Format)> = OnceLock::new();
/// Monotonic record counter. Millisecond timestamps tie under load, and the
/// harness needs a total order to reason about step/tool sequencing.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Line shape of the log file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `[2026-08-25 12:00:00.000] [action] message` — for human reading.
    Human,
    /// One JSON object per line: `{seq, ts, action, message|data}`.
    Jsonl,
}

struct DebugLogger {
    file: Mutex<File>,
    path: PathBuf,
    format: Format,
}

impl DebugLogger {
    fn new(path: PathBuf, format: Format) -> AppResult<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| AppError::Custom(e.to_string()))?;
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| AppError::Custom(e.to_string()))?;

        Ok(Self {
            file: Mutex::new(file),
            path,
            format,
        })
    }

    /// `payload` is the already-rendered body: a message string in human
    /// mode, a JSON value in JSONL mode.
    fn write_record(&self, action: &str, payload: Payload<'_>) {
        let Ok(mut file) = self.file.lock() else {
            return;
        };
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        match self.format {
            Format::Human => {
                let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
                let body = match payload {
                    Payload::Message(text) => std::borrow::Cow::Borrowed(text),
                    // Human mode keeps the old pretty-printed shape.
                    Payload::Data(value) => serde_json::to_string_pretty(value)
                        .unwrap_or_else(|e| format!("failed to serialize json: {e}"))
                        .into(),
                };
                let _ = writeln!(file, "[{timestamp}] [{action}] {body}");
            }
            Format::Jsonl => {
                let record = match payload {
                    Payload::Message(text) => serde_json::json!({
                        "seq": seq,
                        "ts": Local::now().to_rfc3339(),
                        "action": action,
                        "message": text,
                    }),
                    Payload::Data(value) => serde_json::json!({
                        "seq": seq,
                        "ts": Local::now().to_rfc3339(),
                        "action": action,
                        "data": value,
                    }),
                };
                // A record that cannot serialize is dropped rather than
                // written half-formed: the harness parses this file line by
                // line and a broken line would poison the whole trace.
                if let Ok(line) = serde_json::to_string(&record) {
                    let _ = writeln!(file, "{line}");
                }
            }
        }
        let _ = file.flush();
    }
}

enum Payload<'a> {
    Message(&'a str),
    Data(&'a serde_json::Value),
}

/// Lazily opens the log file on first use and reuses it afterward, so
/// toggling logging off and back on via `/debug` doesn't reopen (or
/// truncate) the file.
fn logger() -> AppResult<&'static DebugLogger> {
    if let Some(logger) = LOGGER.get() {
        return Ok(logger);
    }
    let (path, format) = match SINK.get() {
        Some((path, format)) => (path.clone(), *format),
        None => (Path::new(".dev").join("debug.log"), Format::Human),
    };
    let logger = DebugLogger::new(path, format)?;
    let _ = LOGGER.set(logger);
    Ok(LOGGER.get().expect("logger was just set"))
}

/// Point the log at a specific file and line format. Must be called before
/// the first `log`/`log_json` (the harness does it during startup); later
/// calls are ignored because the file is already open.
pub fn configure(path: PathBuf, format: Format) {
    let _ = SINK.set((path, format));
}

/// Called once at startup with the `--debug-log` CLI flag's value.
pub fn init(enabled: bool) -> AppResult<()> {
    set_enabled(enabled)
}

/// Toggles debug logging at runtime (used by the `/debug` command). Safe to
/// call repeatedly in either direction.
pub fn set_enabled(enabled: bool) -> AppResult<()> {
    if enabled {
        let logger = logger()?;
        logger.write_record(
            "logger.init",
            Payload::Message(&format!("debug log enabled at {}", logger.path.display())),
        );
    }
    ENABLED.store(enabled, Ordering::Relaxed);
    Ok(())
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn log(action: &str, message: impl AsRef<str>) {
    if ENABLED.load(Ordering::Relaxed)
        && let Some(logger) = LOGGER.get()
    {
        logger.write_record(action, Payload::Message(message.as_ref()));
    }
}

pub fn log_json<T: Serialize>(action: &str, value: &T) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    match serde_json::to_value(value) {
        Ok(value) => {
            if let Some(logger) = LOGGER.get() {
                logger.write_record(action, Payload::Data(&value));
            }
        }
        Err(error) => log(action, format!("failed to serialize json: {error}")),
    }
}
