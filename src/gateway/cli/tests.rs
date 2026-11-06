//! Тесты адаптеров CLI по настоящему процессу. Бинарь приложения в юнит-тестах недоступен,
//! поэтому «CLI» — сам тестовый exe: `mock_cli_child` включается аргументами после `--`.

use super::adapter::{CliBackend, CliSetup};
use super::claude::ClaudeDialect;
use super::dialect::{Dialect, Plan};
use super::discover::{Lookup, Program, Source};
use super::factory::CliFactory;
use super::launch::{Launch, Scratch};
use super::opencode::OpencodeDialect;
use super::process::Limits;
use crate::gateway::backend::{Backend, BackendError, CallContext, CallReport, EmitTracker};
use crate::gateway::engine::{Engine, EngineDeps, EnvOnlyResolver};
use crate::gateway::handle::{CallSource, DecisionQuery, Gateway, LimitSource, SkipReason};
use crate::gateway::types::{Account, AccountId, AccountProvider, Role, ToolMode};
use crate::harness::mock_cli;
use crate::provider::failure::FailureClass;
use crate::provider::{ChatMessage, CompletionChunk, CompletionRequest};
use serde_json::Value;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const CHILD_TEST: &str = "gateway::cli::tests::mock_cli_child";

/// Точка входа дочернего «CLI»: без аргументов мока после `--` тест ничего не делает.
#[test]
fn mock_cli_child() {
    let args: Vec<String> = std::env::args().collect();
    let Some(at) = args.iter().position(|arg| arg == "--") else {
        return;
    };
    let mock_args = &args[at + 1..];
    if !mock_args.iter().any(|a| a == "--scenario" || a == "--role") {
        return;
    }
    std::process::exit(mock_cli::run_from_args(mock_args));
}

/// Аргументы, превращающие тестовый exe в мок: `[префикс] -- [аргументы CLI]`.
fn self_prefix() -> Vec<String> {
    ["--exact", CHILD_TEST, "--nocapture", "--"]
        .map(String::from)
        .to_vec()
}

fn mock_program(flavor: &str, scenario: &str, rig: &Rig) -> Program {
    let mut prefix = self_prefix();
    prefix.extend(
        [
            "--flavor",
            flavor,
            "--scenario",
            scenario,
            "--record",
            &rig.record.to_string_lossy(),
            "--pidfile",
            &rig.pidfile.to_string_lossy(),
            "--self-prefix",
            &serde_json::to_string(&self_prefix()).unwrap(),
            "--",
        ]
        .map(String::from),
    );
    Program {
        path: std::env::current_exe().unwrap(),
        prefix: prefix.into_iter().map(OsString::from).collect(),
        source: Source::Override,
    }
}

struct Rig {
    dir: PathBuf,
    record: PathBuf,
    pidfile: PathBuf,
}

impl Rig {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pooprusteek-cli-tests-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self {
            record: dir.join("record.json"),
            pidfile: dir.join("grandchild.pid"),
            dir,
        }
    }

    fn scratch_root(&self) -> PathBuf {
        self.dir.join("scratch")
    }

    fn record(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(&self.record).expect("mock wrote a record"))
            .unwrap()
    }

    fn pid(&self, key: &str) -> u32 {
        match key {
            "root" => self.record()["pid"].as_u64().expect("root pid") as u32,
            _ => std::fs::read_to_string(&self.pidfile)
                .unwrap()
                .trim()
                .parse()
                .unwrap(),
        }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn limits(idle_ms: u64) -> Limits {
    Limits {
        idle: Duration::from_millis(idle_ms),
        total: Duration::from_secs(120),
        exit_grace: Duration::from_millis(300),
    }
}

fn backend(
    dialect: Arc<dyn Dialect>,
    flavor: &str,
    scenario: &str,
    rig: &Rig,
    home: Option<PathBuf>,
    limits: Limits,
) -> CliBackend {
    CliBackend::with_limits(
        CliSetup {
            dialect,
            account: AccountId::parse("acc").unwrap(),
            program: mock_program(flavor, scenario, rig),
            version: None,
            home,
            default_model: None,
            scratch_root: rig.scratch_root(),
            keep_scratch: false,
        },
        limits,
    )
}

fn claude(scenario: &str, rig: &Rig) -> CliBackend {
    backend(
        Arc::new(ClaudeDialect),
        "claude",
        scenario,
        rig,
        None,
        limits(30_000),
    )
}

fn opencode(scenario: &str, rig: &Rig) -> CliBackend {
    backend(
        Arc::new(OpencodeDialect),
        "opencode",
        scenario,
        rig,
        None,
        limits(30_000),
    )
}

fn request(user: &str) -> CompletionRequest {
    CompletionRequest {
        messages: vec![ChatMessage::system("SYS PROMPT"), ChatMessage::user(user)],
        tools: Vec::new(),
        model: "ignored".into(),
        temperature: 0.0,
        max_tokens: 16,
        stream: true,
    }
}

fn context(model: &str) -> CallContext {
    CallContext {
        account: AccountId::parse("acc").unwrap(),
        model: model.into(),
        tool_mode: ToolMode::Prompt,
    }
}

struct Answer {
    result: Result<CallReport, BackendError>,
    chunks: Vec<CompletionChunk>,
}

impl Answer {
    fn text(&self) -> String {
        self.chunks.iter().map(|c| c.content.as_str()).collect()
    }

    fn error(&self) -> &BackendError {
        self.result
            .as_ref()
            .expect_err("the call should have failed")
    }

    fn report(&self) -> &CallReport {
        self.result
            .as_ref()
            .expect("the call should have succeeded")
    }
}

async fn ask(backend: &CliBackend, model: &str) -> Answer {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let result = backend
        .call(request("ping"), &context(model), EmitTracker::new(tx))
        .await;
    let mut chunks = Vec::new();
    while let Ok(chunk) = rx.try_recv() {
        chunks.push(chunk);
    }
    Answer { result, chunks }
}

fn alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let out = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success())
    }
}

/// Процесс исчезает не мгновенно после убийства дерева.
async fn assert_gone(pid: u32, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!alive(pid), "{what} (pid {pid}) is still alive");
}

fn argv(record: &Value) -> Vec<String> {
    record["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

fn env_names(record: &Value) -> Vec<String> {
    record["env_names"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

// ── Точный argv и окружение ──────────────────────────────────────────────

#[tokio::test]
async fn claude_runs_with_exactly_the_contract_argv_and_environment() {
    let rig = Rig::new("claude-argv");
    let home = rig.dir.join("claude-home");
    let backend = backend(
        Arc::new(ClaudeDialect),
        "claude",
        "success",
        &rig,
        Some(home.clone()),
        limits(30_000),
    );
    ask(&backend, "haiku").await.report();
    let record = rig.record();
    let mut got = argv(&record);
    // Путь к файлу промпта у каждого вызова свой; остальное проверяем побуквенно.
    let system_file = got[16].clone();
    assert!(system_file.ends_with("system.md"), "{system_file}");
    got[16] = "<system>".into();
    let expected: Vec<&str> = vec![
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--restricted",
        "--tools",
        "",
        "--disable-slash-commands",
        "--strict-mcp-config",
        "--permission-mode",
        "dontAsk",
        "--permission-prompts",
        "none",
        "--no-session-persistence",
        "--system-prompt-file",
        "<system>",
        "--model",
        "haiku",
    ];
    assert_eq!(got, expected);
    assert_eq!(
        record["env"]["CLAUDE_CONFIG_DIR"],
        home.to_string_lossy().as_ref()
    );
    let leaked: Vec<String> = env_names(&record)
        .into_iter()
        .filter(|n| n.to_ascii_uppercase().starts_with("CLAUDE_") && n != "CLAUDE_CONFIG_DIR")
        .collect();
    assert!(leaked.is_empty(), "parent variables leaked: {leaked:?}");
    assert_eq!(record["cwd_entries"], 0, "the working folder must be empty");
    assert!(
        Path::new(record["cwd"].as_str().unwrap()).starts_with(rig.scratch_root()),
        "the working folder is outside the project"
    );
    assert_eq!(record["system"], "SYS PROMPT");
    let stdin = record["stdin"].as_str().unwrap();
    assert!(
        stdin.contains("ping") && !stdin.contains("SYS PROMPT"),
        "{stdin}"
    );
}

#[tokio::test]
async fn two_accounts_of_one_cli_each_get_their_own_login_folder() {
    let rig = Rig::new("two-homes");
    for (dialect, flavor, variable) in [
        (
            Arc::new(ClaudeDialect) as Arc<dyn Dialect>,
            "claude",
            "CLAUDE_CONFIG_DIR",
        ),
        (
            Arc::new(OpencodeDialect) as Arc<dyn Dialect>,
            "opencode",
            "XDG_DATA_HOME",
        ),
    ] {
        let mut seen = Vec::new();
        for name in ["work", "personal"] {
            let home = rig.dir.join(format!("{flavor}-{name}"));
            let backend = backend(
                Arc::clone(&dialect),
                flavor,
                "success",
                &rig,
                Some(home.clone()),
                limits(30_000),
            );
            ask(&backend, "m").await.report();
            seen.push(rig.record()["env"][variable].as_str().unwrap().to_string());
            assert_eq!(seen.last().unwrap(), &home.to_string_lossy());
        }
        assert_ne!(
            seen[0], seen[1],
            "{flavor}: the two accounts share a folder"
        );
    }
}

#[tokio::test]
async fn claude_without_a_home_leaves_the_shared_login_alone() {
    let rig = Rig::new("claude-nohome");
    ask(&claude("success", &rig), "haiku").await.report();
    let names = env_names(&rig.record());
    assert!(
        !names
            .iter()
            .any(|n| n.eq_ignore_ascii_case("CLAUDE_CONFIG_DIR"))
    );
}

#[tokio::test]
async fn opencode_runs_standalone_with_an_empty_config_a_deny_block_and_its_own_data_home() {
    let rig = Rig::new("opencode-argv");
    let home = rig.dir.join("oc-data");
    let backend = backend(
        Arc::new(OpencodeDialect),
        "opencode",
        "success",
        &rig,
        Some(home.clone()),
        limits(30_000),
    );
    ask(&backend, "opencode-go/deepseek-v4.1-flash")
        .await
        .report();
    let record = rig.record();
    assert_eq!(
        argv(&record),
        [
            "run",
            "--format",
            "json",
            "--standalone",
            "--agent",
            "plain",
            "-m",
            "opencode-go/deepseek-v4.1-flash"
        ]
    );
    assert_eq!(
        record["env"]["XDG_DATA_HOME"],
        home.to_string_lossy().as_ref()
    );
    let config_home = PathBuf::from(record["env"]["XDG_CONFIG_HOME"].as_str().unwrap());
    assert!(config_home.starts_with(rig.scratch_root()));
    let config: Value =
        serde_json::from_str(record["env"]["OPENCODE_CONFIG_CONTENT"].as_str().unwrap()).unwrap();
    assert_eq!(config["permission"]["*"], "deny");
    assert_eq!(config["permission"]["skill"], "deny");
    assert_eq!(config["agent"]["plain"]["permission"]["*"], "deny");
    assert_eq!(record["cwd_entries"], 0);
    assert_eq!(
        record["system"], "SYS PROMPT",
        "the prompt goes by a file, not the variable"
    );
}

// ── Отрицательный контроль: запрет нельзя потерять молча ──────────────────

/// Запуск настоящего диалекта на пустых папках; `tweak` ломает его, как мог бы сломать рефакторинг.
fn launch_of(dialect: &dyn Dialect, scratch: &Scratch, home: Option<&Path>) -> Launch {
    let system = scratch.write_system("s").unwrap();
    let program = Program {
        path: "tool".into(),
        prefix: Vec::new(),
        source: Source::Override,
    };
    dialect.launch(&Plan {
        program: &program,
        home,
        model: "m",
        system_file: &system,
        prompt: "p".into(),
        scratch,
        parent_env: vec![("PATH".into(), "x".into())],
    })
}

fn scratch(name: &str) -> (Scratch, PathBuf) {
    let base = std::env::temp_dir().join(format!(
        "pooprusteek-cli-verify-{name}-{}",
        std::process::id()
    ));
    (Scratch::create(&base).unwrap(), base)
}

fn without(launch: &mut Launch, arg: &str) {
    launch.args.retain(|a| a != arg);
}

#[test]
fn the_real_claude_launch_passes_verification_and_each_lost_guard_is_refused() {
    let (scratch, base) = scratch("claude");
    let good = launch_of(&ClaudeDialect, &scratch, None);
    assert_eq!(ClaudeDialect.verify(&good), Ok(()));

    for flag in [
        "--restricted",
        "--disable-slash-commands",
        "--strict-mcp-config",
    ] {
        let mut broken = good.clone();
        without(&mut broken, flag);
        let refused = ClaudeDialect.verify(&broken).unwrap_err();
        assert!(refused.contains(flag), "{flag}: {refused}");
    }
    // Потеря значения `--tools ""`: следующий аргумент уже не пустой.
    let mut broken = good.clone();
    let at = broken.args.iter().position(|a| a == "--tools").unwrap();
    broken.args[at + 1] = "Bash".into();
    assert!(
        ClaudeDialect
            .verify(&broken)
            .unwrap_err()
            .contains("--tools")
    );
    let mut broken = good.clone();
    broken.args.drain(at..=at + 1);
    assert!(ClaudeDialect.verify(&broken).is_err(), "no --tools at all");
    // Чужая переменная родителя в окружении.
    let mut broken = good.clone();
    broken
        .env
        .push(("CLAUDE_CODE_SESSION_ID".into(), "x".into()));
    assert!(
        ClaudeDialect
            .verify(&broken)
            .unwrap_err()
            .contains("leaked")
    );
    // Непустая рабочая папка: проект стал бы виден модели.
    std::fs::write(good.cwd.join("project-file"), "x").unwrap();
    assert!(
        ClaudeDialect
            .verify(&good)
            .unwrap_err()
            .contains("not empty")
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn the_real_opencode_launch_passes_verification_and_each_lost_guard_is_refused() {
    let (scratch, base) = scratch("opencode");
    let good = launch_of(&OpencodeDialect, &scratch, None);
    assert_eq!(OpencodeDialect.verify(&good), Ok(()));

    let mut broken = good.clone();
    without(&mut broken, "--standalone");
    assert!(
        OpencodeDialect
            .verify(&broken)
            .unwrap_err()
            .contains("--standalone")
    );

    let mut broken = good.clone();
    broken
        .env
        .retain(|(name, _)| name != "OPENCODE_CONFIG_CONTENT");
    assert!(OpencodeDialect.verify(&broken).is_err());

    // Блок запретов есть, но разрешает всё.
    let mut broken = good.clone();
    for (name, value) in &mut broken.env {
        if name == "OPENCODE_CONFIG_CONTENT" {
            *value =
                r#"{"permission":{"*":"allow"},"agent":{"plain":{"permission":{"*":"allow"}}}}"#
                    .into();
        }
    }
    assert!(
        OpencodeDialect
            .verify(&broken)
            .unwrap_err()
            .contains("deny")
    );

    // Пользовательский конфиг просочился: папка конфигурации не пуста.
    std::fs::write(scratch.config_dir().join("opencode.json"), "{}").unwrap();
    assert!(
        OpencodeDialect
            .verify(&good)
            .unwrap_err()
            .contains("not empty")
    );
    let _ = std::fs::remove_dir_all(base);
}

/// Диалект, потерявший `--restricted` при сборке: страховка обязана остановить запуск.
struct Careless;

impl Dialect for Careless {
    fn tool(&self) -> crate::gateway::types::CliTool {
        ClaudeDialect.tool()
    }
    fn capabilities(&self) -> crate::gateway::backend::Capabilities {
        ClaudeDialect.capabilities()
    }
    fn limits(&self) -> Limits {
        ClaudeDialect.limits()
    }
    fn launch(&self, plan: &Plan<'_>) -> Launch {
        let mut launch = ClaudeDialect.launch(plan);
        launch.args.retain(|arg| arg != "--restricted");
        launch
    }
    fn verify(&self, launch: &Launch) -> Result<(), String> {
        ClaudeDialect.verify(launch)
    }
    fn parser(&self, account: &AccountId) -> Box<dyn super::dialect::Parser> {
        ClaudeDialect.parser(account)
    }
    fn models(&self) -> super::dialect::ModelSource {
        ClaudeDialect.models()
    }
}

#[tokio::test]
async fn a_launch_that_lost_the_tool_ban_never_starts_the_process() {
    let rig = Rig::new("careless");
    let backend = backend(
        Arc::new(Careless),
        "claude",
        "success",
        &rig,
        None,
        limits(30_000),
    );
    let answer = ask(&backend, "haiku").await;
    let error = answer.error();
    assert!(error.message.contains("--restricted") && error.message.contains("refusing"));
    assert!(!error.emitted && answer.text().is_empty());
    assert!(
        !rig.record.exists(),
        "the CLI must not have been started at all"
    );
}

// ── Успех, расход, лимиты ────────────────────────────────────────────────

#[tokio::test]
async fn claude_streams_deltas_and_reports_usage_limits_and_the_context_window() {
    let rig = Rig::new("claude-ok");
    let backend = claude("success", &rig);
    let answer = ask(&backend, "haiku").await;
    assert_eq!(answer.text(), "pong");
    let last = answer.chunks.last().unwrap();
    assert_eq!(last.finish_reason.as_deref(), Some("stop"));
    assert_eq!(last.usage.as_ref().unwrap().total_tokens, 35 + 7);
    assert!(answer.chunks.len() >= 3, "deltas arrive one by one");
    let report = answer.report();
    assert!(!report.usage_estimated);
    let usage = report.usage.as_ref().unwrap();
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (35, 7));
    assert_eq!(report.context_window, Some(200_000));
    let limits = report.limits.as_ref().unwrap();
    assert_eq!(limits.source, LimitSource::CliEvent);
    assert_eq!(limits.windows[0].used_pct, Some(50.0));
    assert!(report.ttft.is_some());
    // Окно запоминается и отвечает на вопрос движка без нового вызова.
    assert_eq!(backend.context_window("haiku").await, Some(200_000));
    let models = backend.list_models().await.unwrap();
    assert!(
        models
            .iter()
            .any(|m| m.id == "haiku" && m.context_window == Some(200_000))
    );
    assert!(backend.capabilities().streaming && backend.capabilities().pure_model);
}

#[tokio::test]
async fn opencode_gives_the_whole_text_at_once_and_no_limits_or_window() {
    let rig = Rig::new("opencode-ok");
    let answer = ask(&opencode("success", &rig), "mock/model").await;
    assert_eq!(answer.text(), "pong");
    let report = answer.report();
    let usage = report.usage.as_ref().unwrap();
    assert_eq!(usage.prompt_tokens, 30 + 4 + 1);
    assert_eq!(usage.completion_tokens, 5 + 2);
    assert!(report.limits.is_none() && report.context_window.is_none());
    assert!(!opencode("success", &rig).capabilities().streaming);
}

#[tokio::test]
async fn a_native_call_to_a_missing_tool_does_not_leak_into_the_answer() {
    let rig = Rig::new("opencode-tool-step");
    let answer = ask(&opencode("tool-step", &rig), "mock/model").await;
    assert_eq!(answer.text(), "I have no tools.");
    answer.report();
}

#[tokio::test]
async fn garbage_in_stdout_does_not_break_the_call() {
    let rig = Rig::new("garbage");
    assert_eq!(ask(&claude("garbage", &rig), "haiku").await.text(), "pong");
    assert_eq!(ask(&opencode("garbage", &rig), "m").await.text(), "pong");
}

#[tokio::test]
async fn models_and_probe_use_the_clis_own_cheap_commands() {
    let rig = Rig::new("probe");
    let oc = opencode("success", &rig);
    let listed: Vec<String> = oc
        .list_models()
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(listed, ["mock/model-a", "mock/model-b"]);
    let probe = claude("success", &rig).probe().await;
    assert_eq!(probe.auth, crate::gateway::backend::AuthState::Ok);
    let probe = claude("not-logged-in", &rig).probe().await;
    assert_eq!(probe.auth, crate::gateway::backend::AuthState::NeedsLogin);
}

// ── Отказы ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_rejected_limit_is_exhaustion_with_the_reset_time_and_the_windows() {
    let rig = Rig::new("claude-limit");
    let answer = ask(&claude("rate-limit", &rig), "haiku").await;
    let error = answer.error();
    assert!(matches!(
        error.class,
        FailureClass::QuotaExhausted { resets_at: Some(_) }
    ));
    assert_eq!(error.status, Some(429));
    assert!(!error.emitted && answer.text().is_empty());
    assert_eq!(
        error.limits.as_ref().unwrap().windows[0].used_pct,
        Some(100.0)
    );
}

#[tokio::test]
async fn a_plain_429_without_a_rejected_window_is_a_short_rate_limit() {
    let rig = Rig::new("claude-throttled");
    let answer = ask(&claude("throttled", &rig), "haiku").await;
    assert!(matches!(
        answer.error().class,
        FailureClass::RateLimit { .. }
    ));
    assert_eq!(answer.error().status, Some(429));
}

#[tokio::test]
async fn not_logged_in_is_auth_for_both_clis() {
    let rig = Rig::new("auth");
    let claude_error = ask(&claude("not-logged-in", &rig), "haiku").await;
    assert_eq!(claude_error.error().class, FailureClass::Auth);
    assert!(claude_error.error().message.contains("Not logged in"));
    let opencode_error = ask(&opencode("not-logged-in", &rig), "m").await;
    assert_eq!(opencode_error.error().class, FailureClass::Auth);
}

#[tokio::test]
async fn an_unknown_failure_keeps_its_text_and_falls_back_as_a_server_error() {
    let rig = Rig::new("unknown");
    let claude_error = ask(&claude("unknown-error", &rig), "haiku").await;
    assert_eq!(claude_error.error().class, FailureClass::Server);
    assert!(
        claude_error
            .error()
            .message
            .contains("a failure nobody has seen")
    );
    let opencode_error = ask(&opencode("rate-limit", &rig), "m").await;
    assert_eq!(opencode_error.error().class, FailureClass::Server);
    assert!(opencode_error.error().message.contains("nobody has seen"));
}

#[tokio::test]
async fn a_cut_off_stream_is_a_network_failure_and_remembers_whether_text_was_shown() {
    let rig = Rig::new("cutoff");
    let after_text = ask(&claude("cut-off", &rig), "haiku").await;
    let error = after_text.error();
    assert_eq!(error.class, FailureClass::Network);
    assert!(
        error.emitted,
        "part of the answer was already shown: no fallback"
    );
    assert!(
        error.message.contains("connection reset"),
        "stderr tail is kept: {}",
        error.message
    );
    assert_eq!(after_text.text(), "partial");

    let before_text = ask(&claude("cut-off-silent", &rig), "haiku").await;
    assert_eq!(before_text.error().class, FailureClass::Network);
    assert!(!before_text.error().emitted);
    assert_eq!(
        ask(&opencode("cut-off-silent", &rig), "m")
            .await
            .error()
            .class,
        FailureClass::Network
    );
}

#[tokio::test]
async fn silence_longer_than_the_watchdog_is_a_timeout_and_the_process_is_killed() {
    let rig = Rig::new("silence");
    let backend = backend(
        Arc::new(ClaudeDialect),
        "claude",
        "silence",
        &rig,
        None,
        limits(1500),
    );
    let started = Instant::now();
    let answer = ask(&backend, "haiku").await;
    assert_eq!(answer.error().class, FailureClass::Timeout);
    assert!(started.elapsed() < Duration::from_secs(30));
    assert_gone(rig.pid("root"), "the silent CLI").await;
}

#[tokio::test]
async fn a_missing_binary_at_call_time_is_a_server_failure_not_a_crash() {
    let rig = Rig::new("nobinary");
    let backend = CliBackend::with_limits(
        CliSetup {
            dialect: Arc::new(ClaudeDialect),
            account: AccountId::parse("acc").unwrap(),
            program: Program {
                path: rig.dir.join("no-such-claude"),
                prefix: Vec::new(),
                source: Source::Override,
            },
            version: None,
            home: None,
            default_model: None,
            scratch_root: rig.scratch_root(),
            keep_scratch: false,
        },
        limits(5_000),
    );
    let answer = ask(&backend, "haiku").await;
    assert_eq!(answer.error().class, FailureClass::Server);
    assert!(answer.error().message.contains("cannot start"));
}

// ── Процессы: ничего не остаётся ─────────────────────────────────────────

#[tokio::test]
async fn after_a_failed_attempt_no_grandchild_is_left_alive() {
    for (flavor, dialect) in [
        ("claude", Arc::new(ClaudeDialect) as Arc<dyn Dialect>),
        ("opencode", Arc::new(OpencodeDialect) as Arc<dyn Dialect>),
    ] {
        let rig = Rig::new(&format!("grandchild-fail-{flavor}"));
        let backend = backend(
            dialect,
            flavor,
            "grandchild-fail",
            &rig,
            None,
            limits(30_000),
        );
        let answer = ask(&backend, "m").await;
        answer.error();
        let grandchild = rig.pid("grandchild");
        assert_gone(grandchild, &format!("{flavor} grandchild after a failure")).await;
        assert_gone(rig.pid("root"), &format!("{flavor} root")).await;
    }
}

#[tokio::test]
async fn after_a_success_the_grandchild_is_gone_too_and_the_call_does_not_hang_on_its_pipe() {
    let rig = Rig::new("grandchild-ok");
    let started = Instant::now();
    let answer = ask(&claude("grandchild", &rig), "haiku").await;
    assert_eq!(answer.text(), "pong");
    answer.report();
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the grandchild keeps stdout open: the call must not wait for it"
    );
    assert_gone(rig.pid("grandchild"), "the grandchild after a success").await;
}

#[tokio::test]
async fn a_cli_that_does_not_exit_after_its_result_is_killed_without_hanging() {
    let rig = Rig::new("slow-exit");
    let started = Instant::now();
    let answer = ask(&claude("slow-exit", &rig), "haiku").await;
    assert_eq!(answer.text(), "pong");
    answer.report();
    assert!(started.elapsed() < Duration::from_secs(30));
    assert_gone(rig.pid("root"), "the CLI that would not exit").await;
    let rig = Rig::new("slow-exit-oc");
    ask(&opencode("slow-exit", &rig), "m").await.report();
    assert_gone(rig.pid("root"), "OpenCode that would not exit").await;
}

#[tokio::test]
async fn cancelling_the_turn_kills_the_process_at_once() {
    let rig = Rig::new("cancel");
    let backend = Arc::new(backend(
        Arc::new(ClaudeDialect),
        "claude",
        "silence",
        &rig,
        None,
        limits(120_000),
    ));
    let running = {
        let backend = Arc::clone(&backend);
        tokio::spawn(async move { ask(&backend, "haiku").await.result.is_ok() })
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while !rig.record.exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(rig.record.exists(), "the mock CLI never started");
    // Дать записи дописаться, потом снять ход, как это делает отмена.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let pid = rig.pid("root");
    assert!(alive(pid), "the CLI must be running before the cancel");
    running.abort();
    assert_gone(pid, "the cancelled CLI").await;
}

#[tokio::test]
async fn a_gone_receiver_stops_the_process_and_is_not_an_account_failure() {
    let rig = Rig::new("receiver-gone");
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    drop(rx);
    let backend = claude("slow-exit", &rig);
    let result = backend
        .call(request("ping"), &context("haiku"), EmitTracker::new(tx))
        .await;
    let error = result.expect_err("nobody is listening");
    assert_eq!(
        error.class,
        FailureClass::BadRequest,
        "no cooldown for a cancelled call"
    );
    assert!(error.message.contains("cancelled"));
    assert_gone(rig.pid("root"), "the CLI nobody listens to").await;
}

// ── Фабрика и движок ─────────────────────────────────────────────────────

fn cli_account(provider: AccountProvider) -> Account {
    Account::new(AccountId::parse("cli-acc").unwrap(), provider)
}

fn lookup_json(flavor: &str, scenario: &str) -> Lookup {
    let mut parts = vec![
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    ];
    parts.extend(self_prefix());
    parts.extend(["--flavor", flavor, "--scenario", scenario, "--"].map(String::from));
    Lookup {
        override_value: Some(serde_json::to_string(&parts).unwrap()),
        ..Lookup::default()
    }
}

fn factory(lookup: Lookup, rig: &Rig) -> CliFactory {
    CliFactory::with_lookup(lookup, rig.scratch_root())
}

#[test]
fn a_missing_binary_makes_the_account_unavailable_with_a_reason_not_a_crash() {
    let rig = Rig::new("factory-missing");
    let factory = factory(Lookup::default(), &rig);
    let reason = factory
        .build_for(&cli_account(AccountProvider::CliClaude))
        .err()
        .expect("nothing to run");
    assert!(reason.contains("claude was not found") && reason.contains("POOPRUSTEEK_CLI_CLAUDE"));
}

#[test]
fn a_claude_older_than_the_minimum_is_unavailable_and_a_new_one_is_built() {
    let rig = Rig::new("factory-version");
    let old = factory(lookup_json("claude", "old-version"), &rig)
        .build_for(&cli_account(AccountProvider::CliClaude))
        .err()
        .expect("too old to run safely");
    assert!(
        old.contains("2.1.200") && old.contains("2.1.248") && old.contains("--restricted"),
        "{old}"
    );
    let built = factory(lookup_json("claude", "success"), &rig)
        .build_for(&cli_account(AccountProvider::CliClaude))
        .expect("a current version is accepted");
    assert!(built.capabilities().pure_model);
}

#[test]
fn a_cli_account_refuses_secrets_and_codex_is_built_but_never_a_plain_model() {
    let rig = Rig::new("factory-codex");
    let mut keyed = cli_account(AccountProvider::CliClaude);
    keyed.api_key = Some("x".into());
    let refusal = factory(lookup_json("claude", "success"), &rig)
        .build_for(&keyed)
        .err()
        .expect("keys do not belong to a CLI");
    assert!(refusal.contains("home"));
    let codex = factory(Lookup::default(), &rig)
        .build_for(&cli_account(AccountProvider::CliCodex))
        .expect("codex is described even without a binary");
    assert!(!codex.capabilities().pure_model);
}

#[test]
fn the_engine_says_why_codex_is_skipped_and_still_shows_the_account() {
    let rig = Rig::new("engine-codex");
    let config = crate::gateway::testkit::parse(
        r#"
        [gateway]
        enabled = true

        [[accounts]]
        id = "cx"
        provider = "cli:codex"

        [[routes]]
        id = "main"
        steps = [{ account = "cx" }]

        [roles]
        main = "main"
        "#,
    );
    let deps = EngineDeps {
        data_dir: None,
        clock: Arc::new(crate::gateway::clock::SystemClock),
        resolver: Arc::new(EnvOnlyResolver),
        factories: vec![Arc::new(factory(Lookup::default(), &rig))],
        persist: None,
    };
    let engine = Engine::new(config, deps);
    let decision = engine.decide(&DecisionQuery {
        role: Role::Main,
        source: CallSource::Chat,
        tool_mode: ToolMode::Prompt,
        input_tokens: None,
        pinned: None,
        sticky: None,
        failed: Vec::new(),
    });
    assert!(decision.target.is_none());
    assert!(
        decision
            .skipped
            .iter()
            .any(|s| s.reason == SkipReason::NotPureModel),
        "{}",
        decision.explain()
    );
    let view = engine.snapshot().accounts.into_iter().next().unwrap();
    assert!(!view.capabilities.unwrap().pure_model);
}

// ── Настоящие CLI: приманка (`--ignored`, расходует крошечный запрос на аккаунт) ─────────

/// Просьба создать файл. Без запрета инструментов файл появился бы (контроль — в спайке, c2/o2).
const DECOY: &str =
    "Create a file named pwned.txt in the current directory containing the word hi. Then say done.";

fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found
}

async fn decoy_with(provider: AccountProvider, model: &str, root: &Path) -> String {
    let tool = provider.cli_tool().unwrap();
    let factory =
        CliFactory::with_lookup(Lookup::from_env(tool), root.to_path_buf()).keeping_scratch();
    let backend = factory
        .build_for(&cli_account(provider))
        .expect("the real CLI is installed and recent enough");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = context(model);
    let mut request = request(DECOY);
    request.messages.remove(0);
    let result = backend.call(request, &ctx, EmitTracker::new(tx)).await;
    let mut text = String::new();
    while let Ok(chunk) = rx.try_recv() {
        text.push_str(&chunk.content);
    }
    let report = result.unwrap_or_else(|e| panic!("{provider:?}: the call failed: {e}"));
    eprintln!(
        "{provider:?}: reply {:?}, usage {:?}",
        text.chars().take(200).collect::<String>(),
        report.usage.map(|u| (u.prompt_tokens, u.completion_tokens))
    );
    text
}

#[tokio::test]
#[ignore = "runs the installed claude and opencode with one tiny decoy request each"]
async fn real_clis_do_not_create_the_file_a_decoy_prompt_asks_for() {
    let root = std::env::temp_dir().join(format!("gw-cli-decoy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    eprintln!("decoy root: {}", root.display());
    for (provider, model) in [
        (AccountProvider::CliClaude, "haiku"),
        (
            AccountProvider::CliOpencode,
            "opencode-go/deepseek-v4.1-flash",
        ),
    ] {
        decoy_with(provider, model, &root).await;
        // Рабочие папки вызовов остались (keep): в них не должно быть ни одного файла.
        let works: Vec<PathBuf> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.path().join("work"))
            .filter(|p| p.is_dir())
            .collect();
        assert!(
            !works.is_empty(),
            "{provider:?}: no working folder to inspect"
        );
        for work in works {
            let created = files_under(&work);
            assert!(
                created.is_empty(),
                "{provider:?}: the CLI created files: {created:?}"
            );
        }
        let pwned: Vec<PathBuf> = files_under(&root)
            .into_iter()
            .filter(|p| p.file_name().is_some_and(|n| n == "pwned.txt"))
            .collect();
        assert!(
            pwned.is_empty(),
            "{provider:?}: pwned.txt appeared: {pwned:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}
