use crate::error::{AppError, AppResult};
use chrono::Local;
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

static LOGGER: OnceLock<Option<DebugLogger>> = OnceLock::new();

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

pub fn init(enabled: bool) -> AppResult<()> {
    let logger = if enabled {
        let path = Path::new(".dev").join("debug.log");
        let logger = DebugLogger::new(path)?;
        logger.write_line("logger.init", &format!("debug log enabled at {}", logger.path.display()));
        Some(logger)
    } else {
        None
    };

    let _ = LOGGER.set(logger);
    Ok(())
}

pub fn log(action: &str, message: impl AsRef<str>) {
    if let Some(Some(logger)) = LOGGER.get() {
        logger.write_line(action, message.as_ref());
    }
}

pub fn log_json<T: Serialize>(action: &str, value: &T) {
    match serde_json::to_string_pretty(value) {
        Ok(text) => log(action, text),
        Err(error) => log(action, format!("failed to serialize json: {error}")),
    }
}
