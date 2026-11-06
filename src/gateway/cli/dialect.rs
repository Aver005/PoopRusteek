//! Что различает CLI: argv, окружение, разбор потока. Общий ход вызова живёт в `adapter`.
//! Разбор не знает про процессы, поэтому проверяется строками из фикстур.

use super::discover::Program;
use super::launch::{Launch, Scratch};
use super::process::Limits;
use crate::gateway::backend::Capabilities;
use crate::gateway::handle::LimitSnapshot;
use crate::gateway::types::{AccountId, CliTool};
use crate::provider::Usage;
use crate::provider::failure::FailureClass;
use std::ffi::OsString;
use std::path::Path;

/// Всё, из чего собирается один запуск.
pub struct Plan<'a> {
    pub program: &'a Program,
    /// Папка входа аккаунта. `None` — общий вход пользователя.
    pub home: Option<&'a Path>,
    /// Пусто — модель по умолчанию самого CLI.
    pub model: &'a str,
    pub system_file: &'a Path,
    /// Текст ввода (идёт в stdin).
    pub prompt: String,
    pub scratch: &'a Scratch,
    pub parent_env: Vec<(OsString, OsString)>,
}

/// Удачный итог по терминальному событию CLI.
#[derive(Debug, Clone)]
pub struct Success {
    pub usage: Usage,
    pub finish_reason: String,
    pub context_window: Option<u32>,
    pub upstream_id: Option<String>,
    pub limits: Option<LimitSnapshot>,
}

/// Отказ по сообщению самого CLI.
#[derive(Debug, Clone)]
pub struct Failure {
    pub class: FailureClass,
    pub status: Option<u16>,
    pub message: String,
    pub limits: Option<LimitSnapshot>,
    /// Текст не совпал ни с одним образцом: в журнал он идёт целиком.
    pub unrecognized: bool,
}

pub type Terminal = Result<Success, Failure>;

/// Результат разбора одной строки.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Feed {
    /// Текст для получателя (пусто — ничего нового).
    pub text: String,
    /// Терминальное событие: успех или отказ, читать больше нечего.
    pub done: bool,
}

impl Feed {
    pub fn text(text: String) -> Self {
        Self { text, done: false }
    }

    pub fn done(text: String) -> Self {
        Self { text, done: true }
    }
}

pub trait Parser: Send {
    /// Строки, не являющиеся JSON, пропускаются (мусор в stdout не роняет вызов).
    fn feed(&mut self, line: &str) -> Feed;

    /// Итог, когда пришло терминальное событие.
    fn terminal(&mut self) -> Option<Terminal>;

    /// Свежий снимок лимитов, если поток его уже показал (нужен и при обрыве).
    fn limits(&self) -> Option<LimitSnapshot>;
}

pub trait Dialect: Send + Sync {
    fn tool(&self) -> CliTool;
    fn capabilities(&self) -> Capabilities;
    fn limits(&self) -> Limits;
    fn launch(&self, plan: &Plan<'_>) -> Launch;

    /// Страховка перед запуском: запрет инструментов нельзя потерять молча (инвариант 11).
    fn verify(&self, launch: &Launch) -> Result<(), String>;
    fn parser(&self, account: &AccountId) -> Box<dyn Parser>;

    /// Дешёвая проверка входа без токенов, если у CLI она есть: аргументы и разбор вывода.
    fn auth_check(&self) -> Option<AuthCheck> {
        None
    }

    /// Откуда брать список моделей аккаунта.
    fn models(&self) -> ModelSource;
}

/// Список моделей: фиксированные имена CLI или его команда каталога.
pub enum ModelSource {
    Aliases(&'static [&'static str]),
    Command(&'static [&'static str]),
}

/// Команда проверки входа и разбор её вывода в состояние входа.
pub struct AuthCheck {
    pub args: &'static [&'static str],
    pub parse: fn(&str) -> crate::gateway::backend::AuthState,
}

/// Рабочая папка запуска пуста: проекта модель не видит.
pub fn require_empty_cwd(launch: &Launch) -> Result<(), String> {
    let shown = launch.cwd.display();
    match std::fs::read_dir(&launch.cwd).map(Iterator::count) {
        Ok(0) => Ok(()),
        Ok(_) => Err(format!("working directory {shown} is not empty")),
        Err(error) => Err(format!("working directory {shown}: {error}")),
    }
}

/// Целое поле JSON как `u32` (отрицательное и огромное — ноль).
pub fn json_u32(value: Option<&serde_json::Value>) -> u32 {
    value
        .and_then(serde_json::Value::as_u64)
        .map_or(0, |v| u32::try_from(v).unwrap_or(u32::MAX))
}

/// Эпоха в секундах (или миллисекундах) как момент времени.
pub fn epoch_to_time(value: &serde_json::Value) -> Option<std::time::SystemTime> {
    let raw = value.as_f64()?;
    if !raw.is_finite() || raw <= 0.0 {
        return None;
    }
    // Значения выше 1e11 — уже миллисекунды.
    let secs = if raw > 1e11 { raw / 1000.0 } else { raw };
    std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs_f64(secs))
}
