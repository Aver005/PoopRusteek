//! `mock-cli`: подставной `claude`/`opencode` для проверки адаптеров шлюза без настоящих CLI.
//! Сценарии: успех, отказ по лимиту, обрыв, «не вошёл», молчание, внук-процесс, зависший выход.

use clap::Args;
use serde_json::{Value, json};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

/// Версии, которые мок называет в `--version`.
const CLAUDE_VERSION: &str = "2.1.999 (Claude Code) [mock]";
const OPENCODE_VERSION: &str = "opencode v2.2.5 [mock]";
/// Мок-Claude старше минимальной версии адаптера.
const OLD_CLAUDE_VERSION: &str = "2.1.200 (Claude Code) [mock]";
/// Сколько живёт внук и зависший процесс: заведомо дольше любого теста.
const HANG: Duration = Duration::from_secs(600);

#[derive(Args, Debug, Clone)]
pub struct MockCliArgs {
    /// Чьи события изображаем: `claude` или `opencode`.
    #[arg(long, default_value = "claude")]
    pub flavor: String,

    /// success | rate-limit | throttled | not-logged-in | cut-off | cut-off-silent | silence | grandchild |
    /// grandchild-fail | unknown-error | garbage | slow-exit | tool-step | old-version.
    #[arg(long, default_value = "success")]
    pub scenario: String,

    /// Куда записать argv, окружение (имена и безопасные значения), ввод и системный промпт.
    #[arg(long)]
    pub record: Option<PathBuf>,

    /// Куда внук пишет свой pid.
    #[arg(long)]
    pub pidfile: Option<PathBuf>,

    /// `sleeper` — роль внука: записать pid и ждать.
    #[arg(long)]
    pub role: Option<String>,

    /// JSON-массив аргументов, которыми мок запускает самого себя (внука).
    #[arg(long)]
    pub self_prefix: Option<String>,

    /// Настоящие аргументы, с которыми адаптер запустил «CLI» (после `--`).
    #[arg(last = true, allow_hyphen_values = true)]
    pub argv: Vec<String>,
}

/// Чем кончается сценарий.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tail {
    Exit(i32),
    /// Не выходить вовсе (молчание, зависший выход): убивает сторож или адаптер.
    Hang,
}

/// Сценарий целиком: строки stdout, строка stderr, внук, финал.
#[derive(Debug, Clone)]
pub struct Script {
    pub lines: Vec<String>,
    pub stderr: Option<String>,
    pub grandchild: bool,
    pub tail: Tail,
}

impl Script {
    fn new(lines: Vec<String>, tail: Tail) -> Self {
        Self {
            lines,
            stderr: None,
            grandchild: false,
            tail,
        }
    }

    fn with_grandchild(mut self) -> Self {
        self.grandchild = true;
        self
    }

    fn with_stderr(mut self, text: &str) -> Self {
        self.stderr = Some(text.to_string());
        self
    }
}

// ── Строки потока Claude ────────────────────────────────────────────────

fn claude_init() -> String {
    json!({"type":"system","subtype":"init","model":"claude-mock-1","tools":[],"mcp_servers":[],
        "apiKeySource":"none","claude_code_version":"2.1.999"})
    .to_string()
}

fn claude_delta(text: &str) -> String {
    json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,
        "delta":{"type":"text_delta","text":text}},"parent_tool_use_id":null})
    .to_string()
}

fn claude_limits(five_hour: f64, status: &str) -> String {
    json!({"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1_793_990_400u64,
        "rateLimitType":"five_hour","isUsingOverage":false,"unifiedWindows":{
            "five_hour":{"utilization":five_hour,"resetsAt":1_793_990_400u64,"status":status},
            "seven_day":{"utilization":0.2,"resetsAt":1_794_211_200u64,"status":"allowed"}}}})
    .to_string()
}

fn claude_result_ok(text: &str) -> String {
    json!({"type":"result","subtype":"success","is_error":false,"result":text,"stop_reason":"end_turn",
        "api_error_status":null,"session_id":"mock-session",
        "usage":{"input_tokens":10,"cache_creation_input_tokens":20,"cache_read_input_tokens":5,
            "output_tokens":7,"output_tokens_details":{"thinking_tokens":0}},
        "modelUsage":{"claude-mock-1":{"contextWindow":200_000,"maxOutputTokens":8_000}}})
    .to_string()
}

fn claude_result_err(text: &str, status: Option<u16>) -> String {
    json!({"type":"result","subtype":"success","is_error":true,"result":text,
        "terminal_reason":"api_error","api_error_status":status,"usage":{}})
    .to_string()
}

fn claude_success() -> Vec<String> {
    vec![
        claude_init(),
        claude_delta("po"),
        claude_delta("ng"),
        claude_limits(0.5, "allowed"),
        claude_result_ok("pong"),
    ]
}

fn claude_script(scenario: &str) -> Option<Script> {
    Some(match scenario {
        "success" | "garbage" => {
            let mut lines = claude_success();
            if scenario == "garbage" {
                lines.insert(1, "this is not json".to_string());
                lines.insert(1, String::new());
            }
            Script::new(lines, Tail::Exit(0))
        }
        "rate-limit" => Script::new(
            vec![
                claude_init(),
                claude_limits(1.0, "rejected"),
                claude_result_err("mock: usage limit reached", Some(429)),
            ],
            Tail::Exit(1),
        ),
        // Временный отказ без отклонённого окна: класс `RateLimit`, пауза живёт только в памяти.
        "throttled" => Script::new(
            vec![
                claude_init(),
                claude_limits(0.9, "allowed"),
                claude_result_err("mock: too many requests", Some(429)),
            ],
            Tail::Exit(1),
        ),
        "not-logged-in" => Script::new(
            vec![
                claude_init(),
                claude_result_err("Not logged in · Please run /login", None),
            ],
            Tail::Exit(1),
        ),
        "unknown-error" => Script::new(
            vec![
                claude_init(),
                claude_result_err("mock: a failure nobody has seen", None),
            ],
            Tail::Exit(1),
        ),
        "cut-off" => Script::new(
            vec![claude_init(), claude_delta("par"), claude_delta("tial")],
            Tail::Exit(1),
        )
        .with_stderr("mock: connection reset"),
        "cut-off-silent" => Script::new(vec![claude_init()], Tail::Exit(1)),
        "silence" => Script::new(vec![claude_init()], Tail::Hang),
        "slow-exit" => Script::new(claude_success(), Tail::Hang),
        "grandchild" => Script::new(claude_success(), Tail::Exit(0)).with_grandchild(),
        "grandchild-fail" => Script::new(
            vec![
                claude_init(),
                claude_result_err("mock: usage limit reached", Some(429)),
            ],
            Tail::Exit(1),
        )
        .with_grandchild(),
        _ => return None,
    })
}

// ── Строки потока OpenCode ──────────────────────────────────────────────

fn opencode_step_start() -> String {
    json!({"type":"step_start","sessionID":"mock-session","part":{"type":"step-start"}}).to_string()
}

fn opencode_text(text: &str) -> String {
    json!({"type":"text","sessionID":"mock-session","part":{"type":"text","text":text}}).to_string()
}

fn opencode_step_finish(reason: &str) -> String {
    json!({"type":"step_finish","sessionID":"mock-session","part":{"type":"step-finish","reason":reason,
        "cost":0,"tokens":{"input":30,"output":5,"reasoning":2,"cache":{"read":4,"write":1}}}})
    .to_string()
}

fn opencode_error(kind: &str, message: &str) -> String {
    json!({"type":"error","error":{"type":kind,"message":message}}).to_string()
}

fn opencode_success() -> Vec<String> {
    vec![
        opencode_step_start(),
        opencode_text("pong"),
        opencode_step_finish("stop"),
    ]
}

fn opencode_script(scenario: &str) -> Option<Script> {
    Some(match scenario {
        "success" | "garbage" => {
            let mut lines = opencode_success();
            if scenario == "garbage" {
                lines.insert(1, "this is not json".to_string());
            }
            Script::new(lines, Tail::Exit(0))
        }
        "rate-limit" | "throttled" | "unknown-error" => Script::new(
            vec![
                opencode_step_start(),
                opencode_error("mock.limit", "mock: a failure nobody has seen"),
            ],
            Tail::Exit(1),
        ),
        "not-logged-in" => Script::new(
            vec![opencode_error(
                "provider.no-route",
                "Model unavailable: mock",
            )],
            Tail::Exit(1),
        ),
        "cut-off" => Script::new(
            vec![opencode_step_start(), opencode_text("par")],
            Tail::Exit(1),
        )
        .with_stderr("mock: connection reset"),
        "cut-off-silent" => Script::new(vec![opencode_step_start()], Tail::Exit(1)),
        "silence" => Script::new(vec![opencode_step_start()], Tail::Hang),
        "slow-exit" => Script::new(opencode_success(), Tail::Hang),
        "tool-step" => Script::new(
            vec![
                opencode_step_start(),
                opencode_text("I will write the file"),
                json!({"type":"tool_use","part":{"tool":"write_file","state":{"status":"error",
                    "error":"No tool named \"write_file\" is currently available"}}})
                .to_string(),
                opencode_step_finish("tool-calls"),
                opencode_step_start(),
                opencode_text("I have no tools."),
                opencode_step_finish("stop"),
            ],
            Tail::Exit(0),
        ),
        "grandchild" => Script::new(opencode_success(), Tail::Exit(0)).with_grandchild(),
        "grandchild-fail" => Script::new(
            vec![
                opencode_step_start(),
                opencode_error("mock.limit", "mock: a failure nobody has seen"),
            ],
            Tail::Exit(1),
        )
        .with_grandchild(),
        _ => return None,
    })
}

/// Сценарий по типу CLI и имени.
pub fn script(flavor: &str, scenario: &str) -> Option<Script> {
    match flavor {
        "claude" => claude_script(scenario),
        "opencode" => opencode_script(scenario),
        _ => None,
    }
}

// ── Запись запуска и исполнение ─────────────────────────────────────────

/// Значения безопасных переменных идут в запись целиком, остальные — только именем.
const SHOWN_ENV: [&str; 4] = [
    "CLAUDE_CONFIG_DIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "OPENCODE_CONFIG_CONTENT",
];

/// Системный промпт, который адаптер передал файлом: флаг Claude или `{file:…}` OpenCode.
fn system_prompt_seen(argv: &[String]) -> Option<String> {
    let path = argv
        .iter()
        .position(|a| a == "--system-prompt-file")
        .and_then(|at| argv.get(at + 1).cloned())
        .or_else(|| {
            let raw = std::env::var("OPENCODE_CONFIG_CONTENT").ok()?;
            let config: Value = serde_json::from_str(&raw).ok()?;
            let prompt = config.pointer("/agent/plain/prompt")?.as_str()?;
            Some(
                prompt
                    .strip_prefix("{file:")?
                    .strip_suffix('}')?
                    .to_string(),
            )
        })?;
    std::fs::read_to_string(path).ok()
}

fn write_record(path: &PathBuf, argv: &[String], stdin: &str) {
    let env: serde_json::Map<String, Value> = std::env::vars()
        .filter(|(name, _)| SHOWN_ENV.contains(&name.as_str()))
        .map(|(name, value)| (name, Value::String(value)))
        .collect();
    let names: Vec<String> = std::env::vars_os()
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .collect();
    let cwd = std::env::current_dir().ok();
    let cwd_entries = cwd
        .as_ref()
        .and_then(|dir| std::fs::read_dir(dir).ok())
        .map_or(0, Iterator::count);
    let record = json!({
        "pid": std::process::id(),
        "argv": argv,
        "cwd": cwd.map(|dir| dir.to_string_lossy().into_owned()),
        "cwd_entries": cwd_entries,
        "env": env,
        "env_names": names,
        "stdin": stdin,
        "system": system_prompt_seen(argv),
    });
    // Запись нужна тесту сразу и целиком; параллельных писателей у мока нет.
    let _ = std::fs::write(path, record.to_string());
}

fn emit(out: &mut impl Write, line: &str) {
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn hang() -> ! {
    loop {
        std::thread::sleep(HANG);
    }
}

/// Внук: записать pid и жить. Наследует stdout/stderr — как настоящий дочерний процесс CLI.
fn run_sleeper(args: &MockCliArgs) -> i32 {
    if let Some(path) = &args.pidfile {
        let _ = std::fs::write(path, std::process::id().to_string());
    }
    hang()
}

fn spawn_grandchild(args: &MockCliArgs) {
    let Some(pidfile) = &args.pidfile else {
        return;
    };
    let _ = std::fs::remove_file(pidfile);
    let prefix: Vec<String> = args
        .self_prefix
        .as_deref()
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or_else(|| vec!["mock-cli".to_string()]);
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let spawned = std::process::Command::new(exe)
        .args(prefix)
        .args(["--role", "sleeper", "--pidfile"])
        .arg(pidfile)
        .spawn();
    // Ждём, пока внук назовёт pid: тест после этого может на него опереться.
    if spawned.is_ok() {
        for _ in 0..200 {
            if pidfile.is_file()
                && std::fs::read_to_string(pidfile).is_ok_and(|t| !t.trim().is_empty())
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

fn version_of(flavor: &str, scenario: &str) -> &'static str {
    match (flavor, scenario) {
        ("opencode", _) => OPENCODE_VERSION,
        (_, "old-version") => OLD_CLAUDE_VERSION,
        _ => CLAUDE_VERSION,
    }
}

/// Побочные вызовы адаптера: версия, проверка входа, каталог моделей.
fn answer_side_call(args: &MockCliArgs, out: &mut impl Write) -> Option<i32> {
    let argv = &args.argv;
    let first = argv.first().map(String::as_str);
    if argv.iter().any(|a| a == "--version") {
        emit(out, version_of(&args.flavor, &args.scenario));
        return Some(0);
    }
    match (args.flavor.as_str(), first) {
        ("claude", Some("auth")) => {
            let logged_in = args.scenario != "not-logged-in";
            emit(out, &json!({"loggedIn": logged_in}).to_string());
            Some(0)
        }
        ("opencode", Some("models")) => {
            emit(out, "mock/model-a");
            emit(out, "mock/model-b");
            Some(0)
        }
        _ => None,
    }
}

/// Исполнить мок по разобранным аргументам. Возвращает код выхода.
pub fn play(args: &MockCliArgs) -> i32 {
    if args.role.as_deref() == Some("sleeper") {
        return run_sleeper(args);
    }
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if let Some(code) = answer_side_call(args, &mut out) {
        return code;
    }
    let stdin = std::io::read_to_string(std::io::stdin()).unwrap_or_default();
    if let Some(path) = &args.record {
        write_record(path, &args.argv, &stdin);
    }
    let Some(script) = script(&args.flavor, &args.scenario) else {
        let _ = writeln!(
            std::io::stderr(),
            "mock-cli: unknown scenario '{}' for {}",
            args.scenario,
            args.flavor
        );
        return 2;
    };
    if script.grandchild {
        spawn_grandchild(args);
    }
    if let Some(text) = &script.stderr {
        let _ = writeln!(std::io::stderr(), "{text}");
    }
    for line in &script.lines {
        emit(&mut out, line);
    }
    match script.tail {
        Tail::Exit(code) => code,
        Tail::Hang => hang(),
    }
}

/// Разобрать `[опции мока] -- [аргументы CLI]` и исполнить. Вход для тестового дочернего процесса.
#[cfg(test)]
pub fn run_from_args(args: &[String]) -> i32 {
    use clap::Parser;
    #[derive(Parser)]
    struct Wrapper {
        #[command(flatten)]
        args: MockCliArgs,
    }
    match Wrapper::try_parse_from(
        std::iter::once("mock-cli".to_string()).chain(args.iter().cloned()),
    ) {
        Ok(wrapper) => play(&wrapper.args),
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "{error}");
            2
        }
    }
}

/// Подкоманда `pooprusteek mock-cli`.
pub fn run(args: MockCliArgs) -> crate::error::AppResult<i32> {
    Ok(play(&args))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_scenario_exists_for_both_flavors_where_it_makes_sense() {
        for flavor in ["claude", "opencode"] {
            for scenario in [
                "success",
                "rate-limit",
                "throttled",
                "not-logged-in",
                "cut-off",
                "cut-off-silent",
                "silence",
                "grandchild",
                "grandchild-fail",
                "unknown-error",
                "garbage",
                "slow-exit",
            ] {
                assert!(script(flavor, scenario).is_some(), "{flavor}/{scenario}");
            }
        }
        assert!(script("opencode", "tool-step").is_some());
        assert!(script("claude", "nope").is_none() && script("other", "success").is_none());
    }

    #[test]
    fn the_scripts_that_must_never_finish_hang_and_the_failing_ones_exit_nonzero() {
        assert_eq!(script("claude", "silence").unwrap().tail, Tail::Hang);
        assert_eq!(script("claude", "slow-exit").unwrap().tail, Tail::Hang);
        assert_eq!(script("claude", "rate-limit").unwrap().tail, Tail::Exit(1));
        assert_eq!(script("opencode", "success").unwrap().tail, Tail::Exit(0));
        assert!(script("claude", "grandchild").unwrap().grandchild);
    }

    #[test]
    fn run_from_args_rejects_nonsense_instead_of_panicking() {
        assert_eq!(run_from_args(&["--no-such-flag".to_string()]), 2);
    }
}
