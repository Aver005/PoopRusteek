use crate::error::{AppError, AppResult};
use chrono::Local;
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

static ENABLED: AtomicBool = AtomicBool::new(false);
static LOGGER: OnceLock<DebugLogger> = OnceLock::new();

struct DebugLogger {
    file: Mutex<File>,
    path: PathBuf,
}

impl DebugLogger {
    fn new(path: PathBuf) -> AppResult<Self> {
        if let Some(parent) = path.parent() {
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
        })
    }

    fn write_line(&self, action: &str, message: &str) {
        if let Ok(mut file) = self.file.lock() {
            let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
            let _ = writeln!(file, "[{timestamp}] [{action}] {message}");
            let _ = file.flush();
        }
    }
}

/// Lazily opens `.dev/debug.log` on first use and reuses it afterward, so
/// toggling logging off and back on via `/debug` doesn't reopen (or
/// truncate) the file.
fn logger() -> AppResult<&'static DebugLogger> {
    if let Some(logger) = LOGGER.get() {
        return Ok(logger);
    }
    let path = Path::new(".dev").join("debug.log");
    let logger = DebugLogger::new(path)?;
    let _ = LOGGER.set(logger);
    Ok(LOGGER.get().expect("logger was just set"))
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
        logger.write_line(
            "logger.init",
            &format!("debug log enabled at {}", logger.path.display()),
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
        logger.write_line(action, message.as_ref());
    }
}

pub fn log_json<T: Serialize>(action: &str, value: &T) {
    match serde_json::to_string_pretty(value) {
        Ok(text) => log(action, text),
        Err(error) => log(action, format!("failed to serialize json: {error}")),
    }
}
