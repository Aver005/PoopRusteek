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
        "--model=haiku",
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
            "--model=opencode-go/deepseek-v4.1-flash"
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
    launch_with(
        dialect,
        scratch,
        home,
        "m",
        vec![("PATH".into(), "x".into())],
    )
}

fn launch_with(
    dialect: &dyn Dialect,
    scratch: &Scratch,
    home: Option<&Path>,
    model: &str,
    parent_env: Vec<(OsString, OsString)>,
) -> Launch {
    let system = scratch.write_system("s").unwrap();
    let program = Program {
        path: "tool".into(),
        prefix: Vec::new(),
        source: Source::Override,
    };
    dialect.launch(&Plan {
        program: &program,
        home,
        model,
        system_file: &system,
        prompt: "p".into(),
        scratch,
        parent_env,
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

/// Диалект, который портит запуск после сборки, как мог бы испортить рефакторинг.
struct Mutating {
    inner: Arc<dyn Dialect>,
    mutate: fn(&mut Launch),
}

impl Dialect for Mutating {
    fn tool(&self) -> crate::gateway::types::CliTool {
        self.inner.tool()
    }
    fn capabilities(&self) -> crate::gateway::backend::Capabilities {
        self.inner.capabilities()
    }
    fn limits(&self) -> Limits {
        self.inner.limits()
    }
    fn launch(&self, plan: &Plan<'_>) -> Launch {
        let mut launch = self.inner.launch(plan);
        (self.mutate)(&mut launch);
        launch
    }
    fn launch_side(&self, plan: &Plan<'_>, command: &[&str]) -> Launch {
        let mut launch = self.inner.launch_side(plan, command);
        (self.mutate)(&mut launch);
        launch
    }
    fn verify(&self, launch: &Launch) -> Result<(), String> {
        self.inner.verify(launch)
    }
    fn verify_side(&self, launch: &Launch, command: &[&str]) -> Result<(), String> {
        self.inner.verify_side(launch, command)
    }
    fn parser(&self, account: &AccountId) -> Box<dyn super::dialect::Parser> {
        self.inner.parser(account)
    }
    fn models(&self) -> super::dialect::ModelSource {
        self.inner.models()
    }
    fn auth_check(&self) -> Option<super::dialect::AuthCheck> {
        self.inner.auth_check()
    }
}

fn mutating(inner: impl Dialect + 'static, mutate: fn(&mut Launch)) -> Arc<dyn Dialect> {
    Arc::new(Mutating {
        inner: Arc::new(inner),
        mutate,
    })
}

#[tokio::test]
async fn a_launch_that_lost_the_tool_ban_never_starts_the_process() {
    let rig = Rig::new("careless");
    let backend = backend(
        mutating(ClaudeDialect, |launch| {
            launch.args.retain(|arg| arg != "--restricted")
        }),
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

// ── F1: модель из id не может стать флагом; /serve закрыт для всех cli:* ─────────────────

#[test]
fn a_model_id_that_could_pose_as_a_flag_is_refused_but_real_ones_pass() {
    use super::dialect::check_model;
    for ok in [
        "",
        "haiku",
        "claude-haiku-5-5",
        "opencode-go/deepseek-v4.1-flash",
        "a:b@c#high",
    ] {
        assert_eq!(check_model(ok), Ok(()), "{ok}");
    }
    let long = "m".repeat(201);
    for bad in [
        "--standalone",
        "-m",
        "-",
        "a b",
        "a\tb",
        "a\nb",
        "a\u{0}b",
        " x",
        "x ",
        long.as_str(),
    ] {
        let refusal = check_model(bad).unwrap_err();
        assert!(refusal.contains("refusing to start"), "{bad:?}: {refusal}");
    }
}

#[test]
fn verify_refuses_a_flag_like_or_separate_model_for_both_clis() {
    let (scratch, base) = scratch("model");
    for dialect in [&ClaudeDialect as &dyn Dialect, &OpencodeDialect] {
        for bad in ["--standalone", "-m", "x y"] {
            let launch = launch_with(dialect, &scratch, None, bad, vec![]);
            let refusal = dialect.verify(&launch).unwrap_err();
            assert!(refusal.contains("model id"), "{bad}: {refusal}");
        }
        // Модель отдельным аргументом: следующий за ней флаг мог бы быть съеден как значение.
        let mut split = launch_with(dialect, &scratch, None, "m", vec![]);
        let at = split
            .args
            .iter()
            .position(|a| a.to_string_lossy().starts_with("--model="))
            .unwrap();
        split.args[at] = "--model".into();
        split.args.insert(at + 1, "m".into());
        assert!(dialect.verify(&split).unwrap_err().contains("separate"));
        let mut short = launch_with(dialect, &scratch, None, "m", vec![]);
        short.args.push("-m".into());
        assert!(dialect.verify(&short).is_err());
        let fine = launch_with(dialect, &scratch, None, "m", vec![]);
        assert_eq!(dialect.verify(&fine), Ok(()));
    }
    let _ = std::fs::remove_dir_all(base);
}

#[tokio::test]
async fn a_flag_like_model_never_starts_the_process() {
    for (flavor, dialect) in [
        ("claude", Arc::new(ClaudeDialect) as Arc<dyn Dialect>),
        ("opencode", Arc::new(OpencodeDialect) as Arc<dyn Dialect>),
    ] {
        let rig = Rig::new(&format!("model-{flavor}"));
        let backend = backend(dialect, flavor, "success", &rig, None, limits(30_000));
        let answer = ask(&backend, "--standalone").await;
        let error = answer.error();
        assert_eq!(error.class, FailureClass::BadRequest);
        assert!(error.message.contains("refusing to start") && !error.emitted);
        assert!(
            !rig.record.exists(),
            "{flavor}: the CLI must not have been started"
        );
        assert!(ask(&backend, "fine-model").await.result.is_ok());
    }
}

#[test]
fn an_account_whose_default_model_is_a_flag_is_unavailable() {
    let rig = Rig::new("factory-model");
    let mut account = cli_account(AccountProvider::CliOpencode);
    account.model = Some("--standalone".into());
    let refusal = factory(lookup_json("opencode", "success"), &rig)
        .build_for(&account)
        .err()
        .expect("a flag is not a model");
    assert!(refusal.contains("account model") && refusal.contains("refusing to start"));
    account.model = Some("mock/model".into());
    assert!(
        factory(lookup_json("opencode", "success"), &rig)
            .build_for(&account)
            .is_ok()
    );
}

/// Шлюз с одним CLI-аккаунтом `x`, собранным на мок-процессе.
fn cli_engine(provider: &str, flavor: &str, rig: &Rig) -> Engine {
    let config = crate::gateway::testkit::parse(&format!(
        r#"
        [gateway]
        enabled = true

        [[accounts]]
        id = "x"
        provider = "{provider}"
        model = "mock/model"

        [[routes]]
        id = "main"
        steps = [{{ account = "x" }}]

        [roles]
        main = "main"
        "#
    ));
    Engine::new(
        config,
        EngineDeps {
            data_dir: None,
            clock: Arc::new(crate::gateway::clock::SystemClock),
            resolver: Arc::new(EnvOnlyResolver),
            factories: vec![Arc::new(factory(lookup_json(flavor, "success"), rig))],
            persist: None,
        },
    )
}

fn decide_for(engine: &Engine, source: CallSource) -> crate::gateway::handle::Decision {
    engine.decide(&DecisionQuery {
        role: Role::Main,
        source,
        tool_mode: ToolMode::Prompt,
        input_tokens: None,
        pinned: None,
        sticky: None,
        failed: Vec::new(),
    })
}

#[test]
fn no_cli_subscription_is_served_outside_but_every_one_works_for_the_owner() {
    for (provider, flavor) in [("cli:claude", "claude"), ("cli:opencode", "opencode")] {
        let rig = Rig::new(&format!("serve-{flavor}"));
        let engine = cli_engine(provider, flavor, &rig);
        let served = decide_for(&engine, CallSource::Serve);
        assert!(served.target.is_none(), "{provider}: {}", served.explain());
        assert!(
            served
                .skipped
                .iter()
                .any(|s| s.reason == SkipReason::NotForServe),
            "{provider}"
        );
        assert!(
            decide_for(&engine, CallSource::Chat).target.is_some(),
            "{provider}: the owner's own chat still works"
        );
        // Адрес `account/model` для /serve тоже не существует; для чата он есть.
        assert!(
            engine
                .provider_for_model_id("x/mock/model", CallSource::Serve)
                .is_err(),
            "{provider}"
        );
        assert!(
            engine
                .provider_for_model_id("x/mock/model", CallSource::Chat)
                .is_ok(),
            "{provider}"
        );
    }
}

// ── F3: охранные флаги ровно по одному, лишнее не перекрывает запрет ─────────────────────

fn args_of(launch: &Launch) -> Vec<String> {
    launch
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn push(launch: &mut Launch, extra: &[&str]) {
    launch.args.extend(extra.iter().map(OsString::from));
}

#[test]
fn a_second_copy_or_an_override_of_a_guard_flag_is_refused_for_claude() {
    let (scratch, base) = scratch("claude-dups");
    let good = launch_of(&ClaudeDialect, &scratch, None);
    assert_eq!(ClaudeDialect.verify(&good), Ok(()));
    let cases: [&[&str]; 8] = [
        &["--tools", "Bash"],
        &["--tools=Bash"],
        &["--restricted"],
        &["--strict-mcp-config"],
        &["--disable-slash-commands"],
        &["--permission-mode", "acceptEdits"],
        &["--allowedTools", "Bash"],
        &["--dangerously-skip-permissions"],
    ];
    for extra in cases {
        let mut broken = good.clone();
        push(&mut broken, extra);
        let refusal = ClaudeDialect.verify(&broken).unwrap_err();
        assert!(
            refusal.contains("refusing to start"),
            "{extra:?}: {refusal}"
        );
    }
    // Префикс подмены не считается: охранные флаги ищутся после него.
    let mut prefixed = good.clone();
    prefixed.args.insert(0, "--tools".into());
    prefixed.prefix_len = 1;
    assert_eq!(ClaudeDialect.verify(&prefixed), Ok(()));
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn a_second_copy_or_an_override_of_a_guard_flag_is_refused_for_opencode() {
    let (scratch, base) = scratch("opencode-dups");
    let good = launch_of(&OpencodeDialect, &scratch, None);
    assert_eq!(OpencodeDialect.verify(&good), Ok(()));
    let cases: [&[&str]; 5] = [
        &["--standalone"],
        &["--agent", "build"],
        &["--agent=build"],
        &["--dangerously-skip-permissions"],
        &["run"],
    ];
    for extra in cases {
        let mut broken = good.clone();
        push(&mut broken, extra);
        let refusal = OpencodeDialect.verify(&broken).unwrap_err();
        assert!(
            refusal.contains("refusing to start"),
            "{extra:?}: {refusal}"
        );
    }
    assert!(args_of(&good).contains(&"--model=m".to_string()));
    let _ = std::fs::remove_dir_all(base);
}

// ── F4: побочные команды идут проверенным запуском ──────────────────────────────────────

#[tokio::test]
async fn opencode_models_runs_standalone_in_the_checked_environment() {
    let rig = Rig::new("side-opencode");
    let home = rig.dir.join("oc-home");
    let backend = backend(
        Arc::new(OpencodeDialect),
        "opencode",
        "success",
        &rig,
        Some(home.clone()),
        limits(30_000),
    );
    backend.list_models().await.unwrap();
    let record = rig.record();
    assert_eq!(argv(&record), ["models", "--standalone"]);
    assert_eq!(record["cwd_entries"], 0);
    assert_eq!(
        record["env"]["XDG_DATA_HOME"],
        home.to_string_lossy().as_ref()
    );
    assert!(
        !record["env"]["XDG_CONFIG_HOME"]
            .as_str()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn claude_login_check_is_exactly_auth_status_in_the_account_folder() {
    let rig = Rig::new("side-claude");
    let home = rig.dir.join("cl-home");
    let backend = backend(
        Arc::new(ClaudeDialect),
        "claude",
        "success",
        &rig,
        Some(home.clone()),
        limits(30_000),
    );
    backend.probe().await;
    let record = rig.record();
    assert_eq!(argv(&record), ["auth", "status"]);
    assert_eq!(
        record["env"]["CLAUDE_CONFIG_DIR"],
        home.to_string_lossy().as_ref()
    );
}

#[tokio::test]
async fn a_side_command_that_lost_standalone_is_refused_before_it_starts() {
    let rig = Rig::new("side-careless");
    let sabotaged = mutating(OpencodeDialect, |launch| {
        launch.args.retain(|a| a != "--standalone")
    });
    let backend = backend(sabotaged, "opencode", "success", &rig, None, limits(30_000));
    let error = backend.list_models().await.unwrap_err();
    assert!(
        error.message.contains("refusing to start"),
        "{}",
        error.message
    );
    assert!(!rig.record.exists(), "the CLI must not have been started");
}

// ── F5/F6: окружение ребёнка ────────────────────────────────────────────────────────────

fn pair(name: &str, value: &str) -> (OsString, OsString) {
    (name.into(), value.into())
}

fn env_of(launch: &Launch, name: &str) -> Option<String> {
    launch
        .env_value(name)
        .map(|v| v.to_string_lossy().into_owned())
}

#[test]
fn nothing_from_the_anthropic_or_claude_family_survives_into_claude() {
    let (scratch, base) = scratch("claude-env");
    let parent = vec![
        pair("PATH", "p"),
        pair("HTTPS_PROXY", "http://proxy"),
        pair("ANTHROPIC_API_KEY", "k"),
        pair("ANTHROPIC_AUTH_TOKEN", "t"),
        pair("ANTHROPIC_MODEL", "opus"),
        pair("ANTHROPIC_CUSTOM_HEADERS", "authorization: Bearer x"),
        pair("ANTHROPIC_VERTEX_PROJECT_ID", "v"),
        pair("ANTHROPIC_BEDROCK_BASE_URL", "b"),
        pair("anthropic_lowercase", "l"),
        pair("CLAUDE_CODE_OAUTH_TOKEN", "o"),
        pair("CLAUDE_CODE_USE_VERTEX", "1"),
        pair("CLAUDECODE", "1"),
    ];
    let home = base.join("home");
    let launch = launch_with(&ClaudeDialect, &scratch, Some(&home), "m", parent);
    assert_eq!(ClaudeDialect.verify(&launch), Ok(()));
    let names = launch.env_names();
    assert!(
        !names.iter().any(|n| {
            let up = n.to_ascii_uppercase();
            up.starts_with("ANTHROPIC_") || (up.starts_with("CLAUDE") && up != "CLAUDE_CONFIG_DIR")
        }),
        "{names:?}"
    );
    assert!(names.contains(&"PATH".to_string()) && names.contains(&"HTTPS_PROXY".to_string()));
    assert!(names.contains(&"CLAUDE_CONFIG_DIR".to_string()));
    let mut leaky = launch.clone();
    leaky.env.push(pair("ANTHROPIC_MODEL", "opus"));
    assert!(
        ClaudeDialect
            .verify(&leaky)
            .unwrap_err()
            .contains("ANTHROPIC_MODEL")
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn opencode_with_its_own_home_keeps_data_cache_and_state_inside_it() {
    let (scratch, base) = scratch("oc-env-home");
    let parent = vec![
        pair("PATH", "p"),
        pair("XDG_DATA_HOME", "/their/data"),
        pair("XDG_CACHE_HOME", "/their/cache"),
        pair("XDG_STATE_HOME", "/their/state"),
        pair("XDG_CONFIG_HOME", "/their/config"),
        pair("XDG_DATA_DIRS", "/usr/share"),
        pair("XDG_RUNTIME_DIR", "/run/user/1"),
        pair("OPENCODE_CONFIG", "/their/opencode.json"),
    ];
    let home = base.join("account-home");
    let launch = launch_with(&OpencodeDialect, &scratch, Some(&home), "m", parent);
    assert_eq!(OpencodeDialect.verify(&launch), Ok(()));
    assert_eq!(
        env_of(&launch, "XDG_DATA_HOME"),
        Some(home.to_string_lossy().into_owned())
    );
    for (name, sub) in [
        ("XDG_CACHE_HOME", "xdg-cache"),
        ("XDG_STATE_HOME", "xdg-state"),
    ] {
        let value = PathBuf::from(env_of(&launch, name).unwrap());
        assert!(
            value.starts_with(&home) && value.ends_with(sub),
            "{name}: {value:?}"
        );
    }
    let config = env_of(&launch, "XDG_CONFIG_HOME").unwrap();
    assert!(Path::new(&config).starts_with(&base) && config != "/their/config");
    assert_eq!(env_of(&launch, "XDG_CONFIG_DIRS"), Some(config));
    let names = launch.env_names();
    for gone in ["XDG_DATA_DIRS", "XDG_RUNTIME_DIR", "OPENCODE_CONFIG"] {
        assert!(!names.contains(&gone.to_string()), "{gone} leaked");
    }
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn opencode_on_the_shared_login_passes_the_users_own_dirs_through_and_nothing_else() {
    let (scratch, base) = scratch("oc-env-shared");
    let parent = vec![
        pair("XDG_DATA_HOME", "/their/data"),
        pair("XDG_CACHE_HOME", "/their/cache"),
        pair("XDG_CONFIG_HOME", "/their/config"),
        pair("XDG_CONFIG_DIRS", "/etc/xdg"),
        pair("XDG_DATA_DIRS", "/usr/share"),
    ];
    let launch = launch_with(&OpencodeDialect, &scratch, None, "m", parent);
    assert_eq!(OpencodeDialect.verify(&launch), Ok(()));
    assert_eq!(
        env_of(&launch, "XDG_DATA_HOME").as_deref(),
        Some("/their/data")
    );
    assert_eq!(
        env_of(&launch, "XDG_CACHE_HOME").as_deref(),
        Some("/their/cache")
    );
    assert!(
        env_of(&launch, "XDG_STATE_HOME").is_none(),
        "unset stays unset"
    );
    assert_ne!(
        env_of(&launch, "XDG_CONFIG_HOME").as_deref(),
        Some("/their/config")
    );
    assert_ne!(
        env_of(&launch, "XDG_CONFIG_DIRS").as_deref(),
        Some("/etc/xdg")
    );
    assert!(!launch.env_names().contains(&"XDG_DATA_DIRS".to_string()));
    let mut stray = launch.clone();
    stray.env.push(pair("XDG_DATA_DIRS", "/usr/share"));
    assert!(
        OpencodeDialect
            .verify(&stray)
            .unwrap_err()
            .contains("XDG_DATA_DIRS")
    );
    let mut stray = launch.clone();
    stray
        .env
        .push(pair("OPENCODE_CONFIG", "/their/opencode.json"));
    assert!(
        OpencodeDialect
            .verify(&stray)
            .unwrap_err()
            .contains("OPENCODE_CONFIG")
    );
    let _ = std::fs::remove_dir_all(base);
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
