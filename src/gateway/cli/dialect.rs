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
    /// Заметка для журнала: ответ принят, но CLI сообщил что-то непонятное.
    pub note: Option<String>,
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

    /// То же для побочной команды (`models`, `auth status`): точный argv и изоляция окружения.
    fn verify_side(&self, launch: &Launch, command: &[&str]) -> Result<(), String>;

    /// Побочная команда в окружении вызова: те же папки и очистка, другие аргументы, пустой ввод.
    fn launch_side(&self, plan: &Plan<'_>, command: &[&str]) -> Launch {
        let mut launch = self.launch(plan);
        launch.args = plan.program.prefix.clone();
        launch.args.extend(command.iter().map(OsString::from));
        launch.stdin.clear();
        launch
    }

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

/// Модель уходит в argv: пустая — модель по умолчанию, остальное без `-` в начале, пробелов и управляющих.
pub fn check_model(model: &str) -> Result<(), String> {
    let bad = model.starts_with('-')
        || model.chars().count() > MAX_MODEL_LEN
        || model.chars().any(|c| c.is_whitespace() || c.is_control());
    if bad {
        return Err(format!(
            "model id {:?} is not allowed in a command line: refusing to start",
            crate::util::truncate_at_char_boundary(model, 60)
        ));
    }
    Ok(())
}

/// Длиннее любого настоящего идентификатора модели.
const MAX_MODEL_LEN: usize = 200;

/// Сколько раз аргумент встречается ровно в таком виде.
pub fn count_arg(args: &[OsString], flag: &str) -> usize {
    args.iter().filter(|a| *a == flag).count()
}

/// Значение флага: аргумент сразу за единственным вхождением.
pub fn value_after<'a>(args: &'a [OsString], flag: &str) -> Option<&'a std::ffi::OsStr> {
    let at = args.iter().position(|a| a == flag)?;
    args.get(at + 1).map(OsString::as_os_str)
}

/// Значение формы `--флаг=значение`; несколько таких — ошибка.
pub fn attached_value(args: &[OsString], flag: &str) -> Result<Option<String>, String> {
    let prefix = format!("{flag}=");
    let mut found = args.iter().filter_map(|a| {
        a.to_str()?
            .strip_prefix(prefix.as_str())
            .map(str::to_string)
    });
    let first = found.next();
    if found.next().is_some() {
        return Err(format!("{flag} appears more than once: refusing to start"));
    }
    Ok(first)
}

/// Каждый охранный флаг ровно один раз: пропажа и дубль одинаково опасны.
pub fn require_once(args: &[OsString], tool: &str, flags: &[&str]) -> Result<(), String> {
    for flag in flags {
        match count_arg(args, flag) {
            1 => {}
            0 => return Err(format!("{tool} launch lacks {flag}: refusing to start")),
            n => {
                return Err(format!(
                    "{tool} launch repeats {flag} ({n} times): refusing to start"
                ));
            }
        }
    }
    Ok(())
}

/// Аргументы запуска побуквенно равны ожидаемым: ничто лишнее не перекрывает запрет.
pub fn require_exact(own: &[OsString], expected: &[OsString], tool: &str) -> Result<(), String> {
    if own == expected {
        return Ok(());
    }
    Err(format!(
        "{tool} launch deviates from the expected command line: refusing to start"
    ))
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
