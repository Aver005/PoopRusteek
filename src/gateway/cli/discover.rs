//! Поиск бинаря CLI и проверка версии. Оболочки npm (`.cmd`, `.ps1`) не запускаем:
//! `CreateProcess` их не берёт, а OpenCode ищем по пути настоящего `opencode.exe`.

use crate::gateway::types::CliTool;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Подмена бинаря (разработка, `mock-cli`): `POOPRUSTEEK_CLI_CLAUDE`, `…_OPENCODE`.
pub const OVERRIDE_PREFIX: &str = "POOPRUSTEEK_CLI_";

/// Дольше этого `--version` не ждём.
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);

/// Версия `major.minor.patch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32, pub u32, pub u32);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

impl Version {
    /// Первое `число.число.число` в тексте (`2.1.318 (Claude Code)`, `opencode v2.2.5`).
    pub fn parse(text: &str) -> Option<Self> {
        text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
            .find_map(|token| {
                let mut parts = token.trim_matches('.').split('.');
                let major = parts.next()?.parse().ok()?;
                let minor = parts.next()?.parse().ok()?;
                let patch = parts.next()?.parse().ok()?;
                Some(Self(major, minor, patch))
            })
    }
}

/// Откуда взят бинарь.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Override,
    Path,
    KnownDir,
}

/// Что запускать: программа и аргументы перед нашими (у `mock-cli` они есть).
#[derive(Debug, Clone)]
pub struct Program {
    pub path: PathBuf,
    pub prefix: Vec<OsString>,
    pub source: Source,
}

/// Где искать. В тестах подставляется целиком, окружение процесса не трогаем.
#[derive(Debug, Clone, Default)]
pub struct Lookup {
    pub override_value: Option<String>,
    pub path_dirs: Vec<PathBuf>,
    pub home: Option<PathBuf>,
}

/// Подмена бинаря разрешена только в тестах и в харнессе с `--config`: иначе модель через `bash`
/// могла бы подменить бинарь на следующий запуск. Пользователям — только `PATH` и известные места.
static OVERRIDES_ALLOWED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Харнесс с `--config` включает подмену, обычный запуск оставляет выключенной.
pub fn allow_overrides(allowed: bool) {
    OVERRIDES_ALLOWED.store(allowed, std::sync::atomic::Ordering::Relaxed);
}

fn overrides_allowed() -> bool {
    cfg!(test) || OVERRIDES_ALLOWED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Значение переменной подмены, если подмена вообще разрешена; пустое — как отсутствующее.
pub(super) fn gate_override(allowed: bool, value: Option<String>) -> Option<String> {
    value.filter(|v| allowed && !v.trim().is_empty())
}

impl Lookup {
    pub fn from_env(tool: CliTool) -> Self {
        Self {
            override_value: gate_override(
                overrides_allowed(),
                std::env::var(override_var(tool)).ok(),
            ),
            path_dirs: std::env::var_os("PATH")
                .map(|path| std::env::split_paths(&path).collect())
                .unwrap_or_default(),
            home: dirs::home_dir(),
        }
    }
}

pub fn override_var(tool: CliTool) -> String {
    format!(
        "{OVERRIDE_PREFIX}{}",
        binary_name(tool).to_ascii_uppercase()
    )
}

fn binary_name(tool: CliTool) -> &'static str {
    match tool {
        CliTool::Claude => "claude",
        CliTool::Opencode => "opencode",
        CliTool::Codex => "codex",
    }
}

fn exe_name(tool: CliTool) -> String {
    let name = binary_name(tool);
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// Настоящий бинарь OpenCode рядом с шимом npm: `<npm>\node_modules\@opencode\cli\bin\opencode.exe`.
fn opencode_behind_shim(dir: &Path) -> Option<PathBuf> {
    let shim = ["opencode.cmd", "opencode.ps1"]
        .iter()
        .any(|name| dir.join(name).is_file());
    let real = dir
        .join("node_modules")
        .join("@opencode")
        .join("cli")
        .join("bin")
        .join("opencode.exe");
    (shim && real.is_file()).then_some(real)
}

/// `mock:<сценарий>` или JSON-массив `["программа", "аргумент", …]` или просто путь.
fn parse_override(tool: CliTool, value: &str) -> Result<Program, String> {
    let value = value.trim();
    let var = override_var(tool);
    if let Some(scenario) = value.strip_prefix("mock:") {
        let exe = std::env::current_exe()
            .map_err(|error| format!("{var}: cannot locate own binary: {error}"))?;
        let prefix = [
            "mock-cli",
            "--flavor",
            binary_name(tool),
            "--scenario",
            scenario,
            "--",
        ];
        return Ok(Program {
            path: exe,
            prefix: prefix.iter().map(OsString::from).collect(),
            source: Source::Override,
        });
    }
    let parts: Vec<String> = if value.starts_with('[') {
        serde_json::from_str(value)
            .map_err(|_| format!("{var} must be a path or a JSON array of strings"))?
    } else {
        vec![value.to_string()]
    };
    let (path, prefix) = parts
        .split_first()
        .ok_or_else(|| format!("{var} is an empty list"))?;
    if !Path::new(path).is_file() {
        return Err(format!("{var} points to '{path}', which is not a file"));
    }
    Ok(Program {
        path: PathBuf::from(path),
        prefix: prefix.iter().map(OsString::from).collect(),
        source: Source::Override,
    })
}

/// Найти бинарь: подмена, затем `PATH`, затем известная папка.
pub fn find(tool: CliTool, lookup: &Lookup) -> Result<Program, String> {
    if let Some(value) = &lookup.override_value {
        return parse_override(tool, value);
    }
    let name = exe_name(tool);
    for dir in &lookup.path_dirs {
        let direct = dir.join(&name);
        if direct.is_file() {
            return Ok(plain(direct, Source::Path));
        }
        if tool == CliTool::Opencode
            && let Some(real) = opencode_behind_shim(dir)
        {
            return Ok(plain(real, Source::Path));
        }
    }
    if tool == CliTool::Claude
        && let Some(home) = &lookup.home
    {
        let known = home.join(".local").join("bin").join(&name);
        if known.is_file() {
            return Ok(plain(known, Source::KnownDir));
        }
    }
    Err(format!(
        "{} was not found in PATH{}; install it or set {}",
        binary_name(tool),
        if tool == CliTool::Claude {
            " or ~/.local/bin"
        } else {
            ""
        },
        override_var(tool)
    ))
}

fn plain(path: PathBuf, source: Source) -> Program {
    Program {
        path,
        prefix: Vec::new(),
        source,
    }
}

/// Состояние проверки версии бинаря.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionState {
    Ready(Version),
    Failed(String),
    /// Проверка идёт в фоне, результата ещё нет.
    Checking,
}

enum Slot {
    Checking,
    Done(Result<Version, String>, std::time::Instant),
}

type Key = (PathBuf, Vec<OsString>);

/// Ошибку проверки помним недолго: бинарь могли поставить или починить.
const FAILURE_TTL: Duration = Duration::from_secs(30);

fn cache() -> &'static std::sync::Mutex<std::collections::HashMap<Key, Slot>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<Key, Slot>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn key_of(program: &Program) -> Key {
    (program.path.clone(), program.prefix.clone())
}

fn probe_version(program: &Program) -> Result<Version, String> {
    let text = run_version(program)?;
    Version::parse(&text).ok_or_else(|| {
        format!(
            "cannot read a version from the output of {}",
            program.path.display()
        )
    })
}

fn store(key: Key, result: Result<Version, String>) {
    if let Ok(mut slots) = cache().lock() {
        slots.insert(key, Slot::Done(result, std::time::Instant::now()));
    }
}

/// Версия без ожидания: кэш или `Checking` (проверка стартует в фоновом потоке).
/// Годится для пути правки конфига, который идёт рядом с циклом событий.
pub fn version_state(program: &Program) -> VersionState {
    let key = key_of(program);
    {
        let Ok(mut slots) = cache().lock() else {
            return VersionState::Failed("version cache is unavailable".to_string());
        };
        match slots.get(&key) {
            Some(Slot::Done(Ok(version), _)) => return VersionState::Ready(*version),
            Some(Slot::Done(Err(error), at)) if at.elapsed() < FAILURE_TTL => {
                return VersionState::Failed(error.clone());
            }
            Some(Slot::Checking) => return VersionState::Checking,
            _ => {}
        }
        slots.insert(key.clone(), Slot::Checking);
    }
    let program = program.clone();
    let slot_key = key.clone();
    let spawned = std::thread::Builder::new()
        .name("cli-version".into())
        .spawn(move || store(slot_key, probe_version(&program)));
    if let Err(error) = spawned {
        let message = format!("cannot start the version check: {error}");
        store(key, Err(message.clone()));
        return VersionState::Failed(message);
    }
    VersionState::Checking
}

/// Версия с ожиданием (до [`VERSION_TIMEOUT`]) и записью в кэш. Только вне цикла событий:
/// при старте шлюза (`BackendFactory::prepare`) и в тестах.
pub fn warm_version(program: &Program) -> Result<Version, String> {
    let key = key_of(program);
    if let Ok(slots) = cache().lock()
        && let Some(Slot::Done(Ok(version), _)) = slots.get(&key)
    {
        return Ok(*version);
    }
    let result = probe_version(program);
    store(key, result.clone());
    result
}

fn run_version(program: &Program) -> Result<String, String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut command = Command::new(&program.path);
    command
        .args(&program.prefix)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot start {}: {error}", program.path.display()))?;
    let deadline = std::time::Instant::now() + VERSION_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{} --version did not answer in time",
                    program.path.display()
                ));
            }
            Err(error) => {
                return Err(format!(
                    "{} --version failed: {error}",
                    program.path.display()
                ));
            }
        }
    }
    let mut bytes = Vec::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.by_ref().take(4096).read_to_end(&mut bytes);
    }
    Ok(crate::util::decode_process_output(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pooprusteek-discover-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn versions_are_read_from_both_cli_banners() {
        assert_eq!(
            Version::parse("2.1.318 (Claude Code)"),
            Some(Version(2, 1, 318))
        );
        assert_eq!(Version::parse("opencode v2.2.5"), Some(Version(2, 2, 5)));
        assert_eq!(Version::parse("no digits"), None);
        assert!(Version(2, 1, 318) > Version(2, 1, 248) && Version(2, 1, 99) < Version(2, 1, 248));
    }

    #[test]
    fn a_binary_in_path_is_found_and_a_missing_one_is_explained() {
        let dir = temp("path");
        touch(&dir.join(exe_name(CliTool::Claude)));
        let lookup = Lookup {
            path_dirs: vec![dir.clone()],
            ..Lookup::default()
        };
        let found = find(CliTool::Claude, &lookup).unwrap();
        assert_eq!(found.source, Source::Path);
        let missing = find(CliTool::Opencode, &lookup).unwrap_err();
        assert!(missing.contains("opencode") && missing.contains("POOPRUSTEEK_CLI_OPENCODE"));
    }

    #[test]
    fn the_claude_native_install_dir_is_a_fallback() {
        let home = temp("home");
        touch(
            &home
                .join(".local")
                .join("bin")
                .join(exe_name(CliTool::Claude)),
        );
        let lookup = Lookup {
            home: Some(home),
            ..Lookup::default()
        };
        assert_eq!(
            find(CliTool::Claude, &lookup).unwrap().source,
            Source::KnownDir
        );
    }

    #[test]
    fn an_npm_shim_leads_to_the_real_opencode_binary_not_to_the_shim() {
        let dir = temp("shim");
        touch(&dir.join("opencode.cmd"));
        let real = dir.join("node_modules/@opencode/cli/bin/opencode.exe");
        touch(&real);
        let lookup = Lookup {
            path_dirs: vec![dir],
            ..Lookup::default()
        };
        let found = find(CliTool::Opencode, &lookup).unwrap();
        assert_eq!(found.path, real);
        assert!(found.prefix.is_empty());
    }

    #[test]
    fn a_shim_without_the_real_binary_is_not_a_hit() {
        let dir = temp("shim-only");
        touch(&dir.join("opencode.cmd"));
        let lookup = Lookup {
            path_dirs: vec![dir],
            ..Lookup::default()
        };
        assert!(find(CliTool::Opencode, &lookup).is_err());
    }

    #[test]
    fn overrides_take_a_path_a_json_list_or_a_mock_scenario() {
        let dir = temp("override");
        let file = dir.join("fake");
        touch(&file);
        let with = |value: String| Lookup {
            override_value: Some(value),
            ..Lookup::default()
        };
        let plain = find(CliTool::Claude, &with(file.to_string_lossy().into_owned())).unwrap();
        assert_eq!(plain.source, Source::Override);
        let list = serde_json::json!([file.to_string_lossy(), "mock-cli", "--"]).to_string();
        let listed = find(CliTool::Claude, &with(list)).unwrap();
        assert_eq!(listed.prefix, ["mock-cli", "--"].map(OsString::from));
        let mock = find(CliTool::Claude, &with("mock:rate-limit".into())).unwrap();
        assert_eq!(
            mock.prefix[1..5],
            ["--flavor", "claude", "--scenario", "rate-limit"].map(OsString::from)
        );
        let bad = find(
            CliTool::Claude,
            &with(dir.join("absent").to_string_lossy().into_owned()),
        );
        assert!(bad.unwrap_err().contains("not a file"));
    }
}
