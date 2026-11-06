//! Runs real agent turns without a terminal.
//!
//! One prompt is one turn; several prompts are one conversation, driven in
//! order against one accumulating history and one provider session — which is
//! the only way to reach what happens *between* turns (the compaction ladder).
//!
//! This is the same turn the TUI runs — same provider, same `ToolRegistry`,
//! same system prompt, same `AgentRuntime::spawn` — with the human replaced
//! by a policy. Two details make it faithful rather than a shortcut:
//!
//! - Подтверждения отвечает [`ApprovePolicy`], а не обход: у каждого хода,
//!   фонового и суб-агента тоже, проверяется настоящий путь и отказ.
//! - Sub-agents requested via `AppEvent::SpawnSubAgent` are actually
//!   spawned and waited for, so a turn's full tree is in the trace.
//!
//! Deep telemetry (raw model output, parsed tool calls, parse errors) comes
//! from the runner's own `debug_log` call sites; this module points that log
//! at the run's trace file and adds only what it alone knows — the policy
//! decisions it made and the run's verdict.

use crate::app::conversation::{Conversation, ConversationId};
use crate::app::events::{
    AgentEvent, AppEvent, QuestionRequest, QuestionState, ToolApprovalRequest,
};
use crate::app::reduce;
use crate::app::runtime::{AgentRuntime, TurnSpec};
use crate::config::Config;
use crate::debug_log;
use crate::error::{AppError, AppResult};
use crate::harness::trace::action;
use crate::mcp::MCPManager;
use crate::provider::{ChatMessage, LLMProvider, SessionLiveness};
use crate::semantic::SemanticService;
use crate::skills::discovery::discover_all_skills;
use crate::tools::registry::ToolRegistry;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Marks where one user turn ends and the next begins in the trace.
pub(crate) const TURN_STARTED: &str = "harness.turn.started";

/// How the driver answers `RequestToolApproval`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovePolicy {
    /// Approve everything. The usual choice inside the sandbox.
    All,
    /// Deny everything — checks that the agent reports refusal sensibly
    /// instead of looping or claiming success.
    None,
    /// Approve only what the user's persisted whitelist already covers.
    Whitelist,
    /// Approve everything except the named tools.
    Except(Vec<String>),
}

impl FromStr for ApprovePolicy {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        match value {
            "all" => Ok(Self::All),
            "none" => Ok(Self::None),
            "whitelist" => Ok(Self::Whitelist),
            _ => match value.strip_prefix("except:") {
                Some(list) => {
                    let names: Vec<String> = list
                        .split(',')
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .map(str::to_string)
                        .collect();
                    if names.is_empty() {
                        Err("except: needs at least one tool name".to_string())
                    } else {
                        Ok(Self::Except(names))
                    }
                }
                None => Err(format!(
                    "unknown approve policy '{value}' (all | none | whitelist | except:a,b)"
                )),
            },
        }
    }
}

impl ApprovePolicy {
    fn decide(
        &self,
        tool: &str,
        scope: Option<&crate::whitelist::Scope>,
        whitelist: &crate::whitelist::Whitelist,
        always_ask: bool,
    ) -> bool {
        match self {
            Self::All => true,
            Self::None => false,
            // Как в TUI: вызов в чужой разметке или с поправленным именем
            // белый список не одобряет — только человек, которого здесь нет.
            Self::Whitelist => !always_ask && whitelist.allows(tool, scope),
            Self::Except(denied) => !denied.iter().any(|name| name == tool),
        }
    }
}

/// What to do about the semantic (RAG) layer, whose init is slow and
/// asynchronous. Racing it makes RAG-dependent runs flaky, so the harness
/// makes the choice explicit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticMode {
    /// Disabled outright — fastest, and the right baseline for runs that
    /// should not see hints at all.
    Off,
    /// Started, not waited for. Mirrors a real cold start.
    Background,
    /// Started and waited for, up to the given budget.
    Ready(Duration),
}

/// Compaction settings a run overrides on top of the config it loaded.
///
/// The ladder only runs against a *known* window (invariant 12), and neither
/// the sandbox config nor a scenario file could name one — so rungs 1-3 were
/// unreachable from a scenario. Every field is optional: absent leaves
/// `[context]` exactly as the config had it.
///
/// Deserialised straight from a scenario's `[context]` table, so the flags and
/// the TOML keys cannot drift apart.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextOverrides {
    /// `[context] context_window`. The one setting the ladder cannot run
    /// without, and the only one that makes a run deterministic — a provider's
    /// own answer arrives asynchronously and may not arrive at all.
    pub window: Option<u32>,
    pub reserved_tokens: Option<u32>,
    pub preserve_recent_tokens: Option<u32>,
    /// Rung 0's cap, in characters.
    pub tool_output_limit: Option<u32>,
    /// Master switch for the whole ladder.
    pub auto_compact: Option<bool>,
}

impl ContextOverrides {
    fn apply(&self, config: &mut crate::config::ContextConfig) {
        if let Some(window) = self.window {
            config.context_window = window;
        }
        if let Some(reserved) = self.reserved_tokens {
            config.reserved_tokens = reserved;
        }
        if let Some(preserve) = self.preserve_recent_tokens {
            config.preserve_recent_tokens = preserve;
        }
        if let Some(limit) = self.tool_output_limit {
            config.tool_output_limit = limit;
        }
        if let Some(auto) = self.auto_compact {
            config.auto_compact = auto;
        }
    }
}

/// Everything one headless run needs.
#[derive(Debug, Clone)]
pub struct ExecOptions {
    /// User turns, in order. Several are driven against one accumulating
    /// history — the only way to reach what happens *between* turns.
    pub prompts: Vec<String>,
    /// Working directory tools run in, and the workspace the system prompt
    /// describes. `None` keeps the process's own cwd.
    pub workspace: Option<PathBuf>,
    pub trace_path: PathBuf,
    pub approve: ApprovePolicy,
    /// Canned reply for the `question` tool. `None` picks the first offered
    /// option, or an empty answer when there are none (which the runner
    /// reports as a cancelled question).
    pub answer: Option<String>,
    pub max_steps: Option<usize>,
    pub timeout: Duration,
    pub semantic: SemanticMode,
    /// Connect configured MCP servers before the turn. Off by default: it
    /// costs up to a minute and makes the tool surface depend on servers
    /// outside the sandbox.
    pub mcp: bool,
    /// `/providers` entry name to run against, overriding the config's
    /// active provider. `"deepseek"` selects the built-in web client.
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Persist the turn as a session file, so the run joins the corpus
    /// `harness mine` and the history index read from.
    pub save_session: bool,
    /// Id of a saved session this run continues. Its history seeds the
    /// conversation and its provider-side session is adopted, so a chain of
    /// separate `exec` calls is one conversation — the only way to test an
    /// *adaptive* exchange, where the next prompt depends on what the last
    /// run produced.
    pub resume: Option<String>,
    /// Extra instructions appended to the assembled system prompt. Prompt
    /// wording is the cheapest variable to change and one of the most
    /// influential, so it is a first-class knob here.
    pub system_append: Option<PathBuf>,
    /// Файлы к первой реплике — тем же путём, что `/attach` в TUI.
    pub attach: Vec<PathBuf>,
    /// Compaction settings layered over the config, so a scenario can put the
    /// ladder in reach without anyone hand-editing a config file.
    pub context: ContextOverrides,
}

impl ExecOptions {
    /// Резюме подразумевает сохранение: иначе цепочка обрывается на первом
    /// же звене — следующий запуск продолжал бы историю без последнего хода.
    fn saves_session(&self) -> bool {
        self.save_session || self.resume.is_some()
    }
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// The turn finished on its own.
    Completed,
    /// The agent loop reported an error (provider failure, stream timeout).
    Failed,
    /// The wall-clock budget ran out; the turn was aborted.
    TimedOut,
    /// Never started — no provider, bad workspace.
    SetupFailed,
}

impl RunStatus {
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Completed => 0,
            Self::Failed => 1,
            Self::TimedOut => 2,
            Self::SetupFailed => 3,
        }
    }
}

/// One recorded tool invocation, as the driver saw it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInvocation {
    pub name: String,
    pub approved: bool,
    /// `false` for a tool whose result came back flagged as an error.
    pub ok: bool,
    /// Чей это вызов: корневой ход, фоновый суб-агент. У суб-агента `task`
    /// имя вида `label › tool`, а чат — родительский.
    #[serde(default)]
    pub conversation: u64,
    /// Выжимка ошибки или причина отказа.
    #[serde(default)]
    pub error: Option<String>,
}

/// Ответ на `question`, выбранный за вызывающего.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnsweredQuestion {
    pub question: String,
    pub options: Vec<String>,
    pub answer: String,
    /// `flag` — из `--answer`, `default` — первый вариант, `none` — пустой ответ.
    pub source: String,
}

/// Чем кончился фоновый суб-агент (`task` с `background=true`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentResult {
    pub label: String,
    /// `completed` | `failed` | `stopped`.
    pub status: String,
    pub text: String,
}

/// Machine-readable result of one run. Printed by `exec` and consumed by
/// the scenario runner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunOutcome {
    pub status: RunStatus,
    pub trace_path: PathBuf,
    pub duration_ms: u64,
    /// User turns actually driven. Older reports predate the field and read
    /// back as the single turn they were.
    #[serde(default = "one_turn")]
    pub turns: usize,
    /// Assistant text of the final step, i.e. the last turn's answer.
    pub final_text: String,
    pub tools: Vec<ToolInvocation>,
    pub sub_agents: usize,
    pub questions: Vec<String>,
    pub error: Option<String>,
    pub session_id: Option<String>,
    /// `true` when the semantic layer was ready by the time the turn ran.
    pub semantic_ready: bool,
    /// Что вызывающему стоит знать при любом статусе: ответ обрезан лимитом,
    /// сессия не сохранилась, фоновые суб-агенты остановлены по сроку.
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub answered: Vec<AnsweredQuestion>,
    #[serde(default)]
    pub sub_agent_results: Vec<SubAgentResult>,
}

fn one_turn() -> usize {
    1
}

impl RunOutcome {
    /// Пустой вердикт с данным статусом — основа для всех остальных.
    pub fn bare(status: RunStatus, trace_path: PathBuf) -> Self {
        Self {
            status,
            trace_path,
            duration_ms: 0,
            turns: 0,
            final_text: String::new(),
            tools: Vec::new(),
            sub_agents: 0,
            questions: Vec::new(),
            error: None,
            session_id: None,
            semantic_ready: false,
            warnings: Vec::new(),
            answered: Vec::new(),
            sub_agent_results: Vec::new(),
        }
    }

    pub fn setup_failed(trace_path: PathBuf, error: impl Into<String>) -> Self {
        Self {
            error: Some(error.into()),
            ..Self::bare(RunStatus::SetupFailed, trace_path)
        }
    }
}

/// Deps a turn runs against, assembled the way `App::new` assembles them.
struct Harness {
    runtime: AgentRuntime,
    /// Чтобы погасить серверы в конце прогона: их процессы иначе переживали `exec`.
    mcp: Arc<tokio::sync::Mutex<MCPManager>>,
    provider: Arc<dyn LLMProvider>,
    system_prompt: String,
    semantic_ready: bool,
    /// `--attach`, разрешённые один раз: пропавший файл — отказ настройки.
    attachments: Vec<crate::provider::AttachedFile>,
}

/// Run every turn to completion and return the verdict. Never an `Err`:
/// whatever went wrong — a bad workspace, an unwritable trace, a setup that
/// outlived the budget — is a `SetupFailed` / `TimedOut` verdict on stdout.
pub async fn exec(mut config: Config, mut options: ExecOptions) -> RunOutcome {
    let started = Instant::now();
    // Один срок на весь прогон, от старта: подготовка (MCP, индекс, проверка
    // сессии) раньше шла вне его и могла длиться дольше `--timeout`.
    // `checked_add`: огромный `--timeout` раньше просто ждал вечно, а не падал.
    let now = tokio::time::Instant::now();
    let deadline = now
        .checked_add(options.timeout)
        .unwrap_or_else(|| now + Duration::from_secs(u64::from(u32::MAX)));
    if let Err(error) = prepare_trace(&mut options) {
        return finished(
            RunOutcome::setup_failed(options.trace_path.clone(), error),
            started,
        );
    }
    if let Err(error) = check_provider(&config, &options) {
        return finished(
            RunOutcome::setup_failed(options.trace_path.clone(), error),
            started,
        );
    }
    apply_overrides(&mut config, &mut options);
    log_run_started(&config, &options);

    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let setup = async {
        // Сессию поднимаем раньше провайдера: неизвестный id — отказ до первого
        // хода, а не тихий прогон с нуля, который судить нечем.
        let resumed = load_resumed(&config, &options)?;
        let harness = assemble(&config, &options, &workspace(), event_tx.clone()).await?;
        if let Some(session) = &resumed {
            adopt_remote_session(&harness.provider, session).await;
        }
        Ok::<_, AppError>((resumed, harness))
    };
    let (resumed, harness) = match tokio::time::timeout_at(deadline, setup).await {
        Ok(Ok(ready)) => ready,
        Ok(Err(error)) => {
            return finished(
                RunOutcome::setup_failed(options.trace_path.clone(), error.to_string()),
                started,
            );
        }
        Err(_) => {
            let outcome = RunOutcome {
                error: Some(format!(
                    "run exceeded {}s before the first turn (connecting MCP servers, waiting for the semantic index, or checking the resumed session)",
                    options.timeout.as_secs()
                )),
                ..RunOutcome::bare(RunStatus::TimedOut, options.trace_path.clone())
            };
            return finished(outcome, started);
        }
    };

    let mut outcome = drive(
        &config,
        &options,
        resumed.as_ref(),
        &harness,
        (event_tx, event_rx),
        deadline,
    )
    .await;
    outcome.semantic_ready = harness.semantic_ready;
    shut_down(&harness, &options).await;
    finished(outcome, started)
}

/// Записать вердикт в трассу, проставив длительность.
fn finished(mut outcome: RunOutcome, started: Instant) -> RunOutcome {
    outcome.duration_ms = started.elapsed().as_millis() as u64;
    debug_log::log_json(action::RUN_FINISHED, &outcome);
    outcome
}

/// Трасса и рабочая папка. Путь трассы — абсолютный до смены папки: иначе он
/// лёг бы внутрь проверяемой папки и агент увидел бы файлы харнесса.
fn prepare_trace(options: &mut ExecOptions) -> Result<(), String> {
    options.trace_path = absolutize(&options.trace_path).map_err(|e| e.to_string())?;
    if let Some(parent) = options.trace_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    debug_log::configure(options.trace_path.clone(), debug_log::Format::Jsonl);
    debug_log::init(true).map_err(|e| format!("cannot open the trace: {e}"))?;
    if let Some(workspace) = &options.workspace {
        std::env::set_current_dir(workspace)
            .map_err(|e| format!("workspace {}: {e}", workspace.display()))?;
    }
    Ok(())
}

/// `--provider` с опечаткой раньше молча шёл на встроенный DeepSeek.
fn check_provider(config: &Config, options: &ExecOptions) -> Result<(), String> {
    let Some(name) = &options.provider else {
        return Ok(());
    };
    if name == "deepseek" || config.providers.iter().any(|entry| &entry.name == name) {
        return Ok(());
    }
    let known: Vec<&str> = std::iter::once("deepseek")
        .chain(config.providers.iter().map(|entry| entry.name.as_str()))
        .collect();
    Err(format!(
        "unknown provider '{name}' (configured: {})",
        known.join(", ")
    ))
}

/// Флаги прогона поверх конфига.
fn apply_overrides(config: &mut Config, options: &mut ExecOptions) {
    if let Some(name) = &options.provider {
        config.active_provider = Some(name.clone());
    }
    if let Some(model) = &options.model {
        config.provider.model = model.clone();
        for entry in &mut config.providers {
            if Some(&entry.name) == config.active_provider.as_ref() {
                entry.model = model.clone();
            }
        }
    }
    if let Some(max_steps) = options.max_steps {
        config.agent.max_steps_per_turn = max_steps.max(1);
    }
    if options.semantic == SemanticMode::Off {
        config.semantic.enabled = false;
    }
    options.context.apply(&mut config.context);
}

fn workspace() -> String {
    std::env::current_dir()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn log_run_started(config: &Config, options: &ExecOptions) {
    debug_log::log_json(
        action::RUN_STARTED,
        &serde_json::json!({
            // `prompt` stays the first turn so existing trace readers keep working.
            "prompt": options.prompts.first().cloned().unwrap_or_default(),
            "prompts": options.prompts,
            "turns": options.prompts.len(),
            "workspace": workspace(),
            "model": config.active_model(),
            "provider": config.active_provider.clone().unwrap_or_else(|| "deepseek".into()),
            "approve": format!("{:?}", options.approve),
            "semantic": format!("{:?}", options.semantic),
            "mcp": options.mcp,
            "max_steps": config.agent.max_steps_per_turn,
            "timeout_ms": options.timeout.as_millis(),
            // Recorded because the ladder's whole behaviour hangs off these,
            // and a trace read later has no other way to know what they were.
            "context": {
                "auto_compact": config.context.auto_compact,
                "context_window": config.context.context_window,
                "reserved_tokens": config.context.reserved_tokens,
                "preserve_recent_tokens": config.context.preserve_recent_tokens,
                "tool_output_limit": config.context.tool_output_limit,
                "max_tokens": config.provider.max_tokens,
            },
        }),
    );
}

/// Погасить фоновые задачи и MCP-серверы прогона — со сроком: выход не должен
/// зависеть от того, как быстро умирает чужой процесс.
async fn shut_down(harness: &Harness, options: &ExecOptions) {
    let grace = Duration::from_secs(5);
    let _ = tokio::time::timeout(grace, crate::tools::background::shutdown_all()).await;
    if options.mcp {
        let mcp = Arc::clone(&harness.mcp);
        let _ = tokio::time::timeout(grace, async move {
            mcp.lock().await.shutdown_all().await;
        })
        .await;
    }
}

/// Join a relative path onto the current directory without touching the
/// filesystem: `canonicalize` would fail on a trace file that does not exist
/// yet, and on Windows it also rewrites the path into `\?\` form.
fn absolutize(path: &std::path::Path) -> AppResult<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let cwd = std::env::current_dir()
        .map_err(|e| AppError::Custom(format!("cannot read current directory: {e}")))?;
    Ok(cwd.join(path))
}

/// Read the session `--resume` names. A missing or unreadable id is a hard
/// setup failure: starting from scratch instead would hand back a run that
/// looks successful while it answered without the history it was given.
fn load_resumed(
    config: &Config,
    options: &ExecOptions,
) -> AppResult<Option<crate::session::Session>> {
    let Some(id) = &options.resume else {
        return Ok(None);
    };
    let session = crate::session::load_local(id, config)
        .map_err(|error| AppError::Custom(format!("cannot resume session {id}: {error}")))?;
    Ok(Some(session))
}

/// Continue the provider's own server-side session, or fall back to replaying
/// local history into a fresh one — the same fork `App::handle_load_session`
/// makes, minus the TUI's asynchrony (no turn has run yet, so the liveness
/// check can simply be awaited).
///
/// A dead or absent remote link is **not** a refusal: the local transcript is
/// the source of truth and is replayed as context, exactly as the TUI does.
/// The trace says which of the two happened, so a run is never silently
/// judged on a conversation it did not actually continue.
async fn adopt_remote_session(provider: &Arc<dyn LLMProvider>, session: &crate::session::Session) {
    if let Some(route) = &session.route_state {
        provider.seed_route_state(route.clone());
    }
    let remote = match (session.broken, session.provider_session_id.as_deref()) {
        (false, Some(id)) => id,
        (true, _) => return replay_locally(session, "session_marked_broken", None, provider).await,
        (false, None) => return replay_locally(session, "no_remote_session", None, provider).await,
    };
    let liveness = provider.check_session(remote).await;
    if let Some((label, detail)) = liveness_label(&liveness) {
        return replay_locally(session, label, Some(detail), provider).await;
    }
    match provider
        .adopt_session(remote, session.provider_parent_message_id)
        .await
    {
        Ok(()) => log_resume(session, "adopted", None, None),
        Err(error) => {
            replay_locally(session, "adopt_failed", Some(&error.to_string()), provider).await
        }
    }
}

/// Ярлык для трассы по исходу проверки; `None` — сессия жива.
///
/// Отдельной функцией, чтобы «сессии нет» и «проверить не удалось» нельзя
/// было снова слить в одно слово незаметно. Раньше их сливали, и трасса
/// говорила `remote_session_gone` про ответ, которого никто не понял —
/// на деле снесённый эндпоинт отдавал HTML.
fn liveness_label(liveness: &SessionLiveness) -> Option<(&'static str, &str)> {
    match liveness {
        SessionLiveness::Alive => None,
        // Провайдер ответил про эту сессию и отказал.
        SessionLiveness::Gone(reason) => Some(("remote_session_gone", reason)),
        // Проверка не состоялась: сеть, токен, форма ответа. Про сессию
        // это не утверждает ничего.
        SessionLiveness::Unknown(reason) => Some(("remote_session_unverified", reason)),
    }
}

/// Drop whatever thread the provider was on, so the next turn opens a fresh
/// one and the local transcript travels with it as context.
async fn replay_locally(
    session: &crate::session::Session,
    reason: &str,
    detail: Option<&str>,
    provider: &Arc<dyn LLMProvider>,
) {
    let _ = provider.reset().await;
    log_resume(session, "replayed", Some(reason), detail);
}

fn log_resume(
    session: &crate::session::Session,
    remote: &str,
    reason: Option<&str>,
    detail: Option<&str>,
) {
    debug_log::log_json(
        action::RESUME,
        &serde_json::json!({
            "session_id": session.id,
            "messages": session.messages.len(),
            "broken": session.broken,
            // "adopted" — тот же серверный тред; "replayed" — история уедет
            // в свежий одним промптом.
            "remote": remote,
            // Ярлык (что решили) и подробность (что сказал провайдер) —
            // порознь: по одному ярлыку причину не восстановить.
            "reason": reason,
            "detail": detail,
        }),
    );
}

/// Шлюз прогона: `[gateway]`, `[[accounts]]`, `[[routes]]`, `[roles]` самого конфига (`--config`).
/// Сборка читает секреты (`env:`, файлы, `cmd:`), поэтому идёт на `spawn_blocking`.
async fn harness_gateway(config: &Config) -> AppResult<crate::gateway::handle::GatewayHandle> {
    let settings = crate::gateway::engine::gateway_config(config);
    check_gateway_config(&settings)?;
    let config = config.clone();
    tokio::task::spawn_blocking(move || {
        crate::gateway::engine::build_gateway(
            settings,
            crate::gateway::engine::EngineDeps::for_config(&config),
        )
    })
    .await
    .map_err(|e| AppError::Custom(format!("gateway build: {e}")))
}

/// Прогон на сломанном конфиге ничего не доказывает: отказ до первого хода.
fn check_gateway_config(settings: &crate::gateway::types::GatewayConfig) -> AppResult<()> {
    if !settings.gateway.enabled {
        return Ok(());
    }
    let errors: Vec<String> = settings
        .validate()
        .into_iter()
        .filter(|p| p.severity == crate::gateway::types::Severity::Error)
        .map(|p| format!("{}: {}", p.path, p.message))
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(AppError::Custom(format!(
            "invalid gateway config: {}",
            errors.join("; ")
        )))
    }
}

/// Build provider, tools, skills, semantic service and system prompt.
async fn assemble(
    config: &Config,
    options: &ExecOptions,
    workspace: &str,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) -> AppResult<Harness> {
    // Пропавший `--attach` — отказ до сборки провайдера и ожидания индекса.
    let attachments =
        attached_files(options, std::path::Path::new(workspace)).map_err(AppError::Custom)?;
    let gateway = harness_gateway(config).await?;
    let provider =
        crate::gateway::handle::main_provider(config, Some(&gateway)).ok_or_else(|| {
            AppError::Custom(
                "no provider configured: set [provider].token or pick a /providers entry"
                    .to_string(),
            )
        })?;

    // Same shape as `App::new`: ask once, in the background, and let the
    // answer arrive as an event. A provider that cannot say never sends one,
    // and `[context] context_window` outranks it either way — which is why a
    // scenario that needs a guaranteed window pins it rather than hoping.
    {
        let handle = Arc::clone(&provider);
        let event_tx = event_tx.clone();
        tokio::spawn(async move {
            if let Some(window) = handle.context_window().await {
                let _ = event_tx.send(AppEvent::ContextWindowLearned(window));
            }
        });
    }

    let mcp = Arc::new(tokio::sync::Mutex::new(MCPManager::new()));
    let tools = Arc::new(ToolRegistry::new());

    let mut skills = discover_all_skills(&config.skills.paths);
    for skill in &mut skills {
        if config.skills.enabled.contains(&skill.slug)
            || config.skills.enabled.contains(&skill.name)
        {
            skill.enabled = true;
        }
    }
    tools.update_skills(skills.clone());

    // `Off` already cleared `config.semantic.enabled`, and `start` spawns no
    // init in that case — one construction path for all three modes.
    let semantic = SemanticService::start(config, skills.clone(), event_tx.clone());
    tools.register_semantic_tools(Arc::clone(&semantic));

    if options.mcp {
        crate::mcp::manager::startup_initialize(&mcp).await;
        let mcp_tools = mcp.lock().await.get_all_tools();
        semantic.update_mcp_tools(mcp_tools);
    }

    let semantic_ready = match options.semantic {
        SemanticMode::Ready(budget) => await_semantic(&semantic, budget).await,
        _ => semantic.is_ready(),
    };
    debug_log::log_json(
        action::SEMANTIC,
        &serde_json::json!({ "ready": semantic_ready, "enabled": semantic.is_enabled() }),
    );

    let instructions = if config.instructions.enabled {
        // Только сама рабочая папка: копия шаблона живёт под `--out`, и
        // цепочка вверх утаскивала в промпт `AGENTS.md` репозитория, внутри
        // которого этот каталог оказался. Прогон обязан зависеть от своей
        // фикстуры, а не от места запуска.
        crate::instructions::load(
            workspace,
            config.instructions.max_bytes,
            crate::instructions::Scope::WorkspaceOnly,
        )
        .section
    } else {
        String::new()
    };
    let mut system_prompt =
        crate::app::system_prompt::build(crate::app::system_prompt::PromptInputs {
            prompts: &crate::prompts::load_prompt_files(),
            skills: &skills,
            skills_injection: config.skills.injection,
            tools: &tools,
            mcp: &mcp,
            mcp_schema_mode: config.effective_mcp_schema_mode(),
            workspace,
            // Сценарий гоняется в чужой рабочей папке, и её AGENTS.md — часть
            // условий задачи ровно так же, как в TUI.
            project_instructions: &instructions,
            native_tools: provider.native_tools(),
        })
        .await;

    // A prompt variant is appended rather than substituted: the comparison
    // worth making is "does this instruction change behaviour", and replacing
    // the whole prompt would change the tool contract along with it.
    if let Some(path) = &options.system_append {
        let extra = std::fs::read_to_string(path)
            .map_err(|e| AppError::Custom(format!("{}: {e}", path.display())))?;
        debug_log::log_json(
            "system_prompt.appended",
            &serde_json::json!({
                "file": path.display().to_string(),
                "bytes": extra.len(),
                "text": extra,
            }),
        );
        system_prompt.push_str(
            "

",
        );
        system_prompt.push_str(extra.trim());
    }

    Ok(Harness {
        runtime: AgentRuntime::new(tools, Arc::clone(&mcp), semantic, event_tx),
        mcp,
        provider,
        system_prompt,
        semantic_ready,
        attachments,
    })
}

/// Poll readiness rather than subscribe: `SemanticStatus` events are
/// human-readable strings, and `is_ready` is the actual gate the turn path
/// consults.
async fn await_semantic(semantic: &Arc<SemanticService>, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    // Готов и не занят: пока идёт индексация истории, подсказка первого хода
    // не дождётся замка индекса и уйдёт пустой.
    while Instant::now() < deadline {
        if semantic.is_ready() && !semantic.is_initializing() {
            return true;
        }
        if !semantic.is_enabled() {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    semantic.is_ready() && !semantic.is_initializing()
}

/// `--attach` пути; относительные — от рабочей папки прогона, как в TUI.
fn attached_files(
    options: &ExecOptions,
    workspace: &std::path::Path,
) -> Result<Vec<crate::provider::AttachedFile>, String> {
    options
        .attach
        .iter()
        .map(|path| crate::app::attachments::resolve(&path.to_string_lossy(), workspace))
        .collect()
}

/// Что прогон узнаёт по ходу событий; из этого собирается вердикт.
struct Verdict {
    status: RunStatus,
    error: Option<String>,
    final_text: String,
    tools: Vec<ToolInvocation>,
    /// Инструменты, запущенные в чате и ещё не вернувшие результат: стек, потому
    /// что вызов суб-агента идёт внутри вызова `task` того же чата.
    in_flight: HashMap<ConversationId, Vec<usize>>,
    questions: Vec<String>,
    answered: Vec<AnsweredQuestion>,
    sub_agents: usize,
    /// Фоновые суб-агенты: чат → имя.
    background: HashMap<ConversationId, String>,
    /// Их форки: серверные сессии удаляются в конце прогона, как в TUI.
    forks: Vec<Arc<dyn LLMProvider>>,
    /// Последняя непустая реплика каждого фонового агента — для итога при провале.
    last_texts: HashMap<ConversationId, String>,
    sub_agent_results: Vec<SubAgentResult>,
    warnings: Vec<String>,
}

impl Verdict {
    fn new() -> Self {
        Self {
            status: RunStatus::Completed,
            error: None,
            final_text: String::new(),
            tools: Vec::new(),
            in_flight: HashMap::new(),
            questions: Vec::new(),
            answered: Vec::new(),
            sub_agents: 0,
            background: HashMap::new(),
            forks: Vec::new(),
            last_texts: HashMap::new(),
            sub_agent_results: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// Учёт инструментов — по их собственным событиям, а не по запросам
    /// подтверждения: иначе `todo`, `task` и ход суб-агента не попадали в список.
    fn on_tool_event(&mut self, conversation: ConversationId, event: &AgentEvent) {
        match event {
            AgentEvent::ToolStarted { name, .. } => {
                self.tools.push(ToolInvocation {
                    name: name.clone(),
                    approved: true,
                    ok: true,
                    conversation: conversation.0,
                    error: None,
                });
                let index = self.tools.len() - 1;
                self.in_flight.entry(conversation).or_default().push(index);
            }
            AgentEvent::ToolDone { .. } => {
                self.finish_tool(conversation);
            }
            // Ошибка без запущенного инструмента — это отказ (его записал
            // `on_approval`) или неисполнимый вызов: приписывать её некому.
            AgentEvent::ToolError { error } => {
                if let Some(index) = self.finish_tool(conversation) {
                    self.tools[index].ok = false;
                    self.tools[index].error = Some(error.clone());
                }
            }
            _ => {}
        }
    }

    fn finish_tool(&mut self, conversation: ConversationId) -> Option<usize> {
        self.in_flight.get_mut(&conversation)?.pop()
    }

    /// Ход чата кончился, а его инструменты так и не вернулись — это не `ok`.
    fn close_in_flight(&mut self, conversation: ConversationId, why: &str) {
        for index in self.in_flight.remove(&conversation).unwrap_or_default() {
            self.tools[index].ok = false;
            self.tools[index].error = Some(why.to_string());
        }
    }

    fn remember_text(&mut self, conversation: ConversationId, event: &AgentEvent) {
        if let AgentEvent::EndAssistantMessage { text, .. } = event
            && !text.trim().is_empty()
            && self.background.contains_key(&conversation)
        {
            self.last_texts.insert(conversation, text.clone());
        }
    }

    fn on_approval(&mut self, request: &ToolApprovalRequest, approved: bool) {
        if !approved {
            self.tools.push(ToolInvocation {
                name: request.tool_name.clone(),
                approved: false,
                ok: false,
                conversation: request.conversation.0,
                error: Some("denied by the --approve policy".to_string()),
            });
        }
    }

    /// Фоновый суб-агент закончил. Его провал — не провал прогона: корень
    /// мог справиться и без него.
    /// Возвращает заметку для истории корня — ту же, что кладёт TUI.
    fn on_background_end(
        &mut self,
        conversation: ConversationId,
        event: &AgentEvent,
    ) -> Option<String> {
        let label = self.background.remove(&conversation)?;
        let (status, end) = match event {
            AgentEvent::Done(result) if result.text.trim().is_empty() => ("completed", None),
            AgentEvent::Done(result) => ("completed", Some(Ok(result.text.as_str()))),
            AgentEvent::Failed(reason) => ("failed", Some(Err(reason.as_str()))),
            _ => return None,
        };
        self.close_in_flight(
            conversation,
            "the agent's turn ended before this tool finished",
        );
        let last_text = self.last_texts.remove(&conversation).unwrap_or_default();
        let ran: Vec<String> = self
            .tools
            .iter()
            .filter(|tool| tool.conversation == conversation.0 && tool.ok)
            .map(|tool| tool.name.clone())
            .collect();
        let report = crate::agent::sub_agent::report(&label, end, &last_text, &ran);
        self.sub_agent_results.push(SubAgentResult {
            label: label.clone(),
            status: status.to_string(),
            text: report.clone(),
        });
        Some(crate::agent::sub_agent::background_block(
            "🤖 sub-agent",
            &label,
            &report,
        ))
    }

    /// Срок вышел. Если корень уже закончил, ждали только фоновых — их итог
    /// не делает прогон «тайм-аутом».
    fn on_deadline(
        &mut self,
        root_active: bool,
        still_active: usize,
        budget: Duration,
        turn: String,
    ) {
        if root_active {
            let message = format!("run exceeded {}s during turn {turn}", budget.as_secs());
            self.error = Some(match self.error.take() {
                Some(earlier) => format!("{message}; before that: {earlier}"),
                None => message,
            });
            self.status = RunStatus::TimedOut;
        } else {
            self.warnings.push(format!(
                "{still_active} background sub-agent(s) were still running at the {}s deadline and were stopped",
                budget.as_secs()
            ));
        }
        let open: Vec<ConversationId> = self.in_flight.keys().copied().collect();
        for conversation in open {
            self.close_in_flight(conversation, "stopped by the run's deadline");
        }
        for label in self.background.drain().map(|(_, label)| label) {
            self.sub_agent_results.push(SubAgentResult {
                label,
                status: "stopped".to_string(),
                text: String::new(),
            });
        }
    }

    fn into_outcome(
        self,
        options: &ExecOptions,
        turns: usize,
        session_id: Option<String>,
    ) -> RunOutcome {
        RunOutcome {
            turns,
            final_text: self.final_text,
            tools: self.tools,
            sub_agents: self.sub_agents,
            questions: self.questions,
            error: self.error,
            session_id,
            warnings: self.warnings,
            answered: self.answered,
            sub_agent_results: self.sub_agent_results,
            ..RunOutcome::bare(self.status, options.trace_path.clone())
        }
    }
}

/// Drive every turn in order, servicing each one's events until its whole
/// conversation tree is done. One history, one provider session, one budget:
/// the turns are a conversation, not a batch of independent runs.
async fn drive(
    config: &Config,
    options: &ExecOptions,
    resumed: Option<&crate::session::Session>,
    harness: &Harness,
    (event_tx, mut event_rx): (
        mpsc::UnboundedSender<AppEvent>,
        mpsc::UnboundedReceiver<AppEvent>,
    ),
    deadline: tokio::time::Instant,
) -> RunOutcome {
    let whitelist = crate::whitelist::load().whitelist;
    let root = ConversationId::next();
    // History is accumulated through the app's own reducer, so turn two sees
    // exactly what turn two in the TUI would see.
    let mut history = Conversation::fresh_main(None);
    if let Some(session) = resumed {
        // Те же поля, что переносит `App::handle_load_session`. Идентичность
        // сессии тоже: прогон дописывается в тот же файл, иначе цепочка из
        // пяти ходов рассыпалась бы на пять сессий.
        history.messages = session.messages.clone();
        history.session_id = session.id.clone();
        history.session_started_at = session.created_at.clone();
        history.tag = session.tag.clone();
        history.broken = session.broken;
    }
    // Namespaces this run's spill directory, the way a session id does in the app.
    let run_id = crate::session::create_session_id();
    // Filled in by `ContextWindowLearned` if the provider answers. Read when
    // each turn's `ContextSpec` is built, exactly as `App` reads its own
    // `provider_context_window`: a window learned during turn 1 governs turn 2.
    let mut provider_window = 0u32;
    let mut verdict = Verdict::new();
    let mut turns = 0usize;
    // Срок выстрелил: ни одного хода больше не начинать.
    let mut budget_spent = false;
    // Хоть один ход корня оборван извне: серверной ветке верить нельзя.
    let mut was_interrupted = false;

    let deadline = tokio::time::sleep_until(deadline);
    tokio::pin!(deadline);
    // Один на весь прогон: заново на каждой итерации он терял сигнал.
    let shutdown = crate::util::shutdown_signal();
    tokio::pin!(shutdown);

    let accepts = |path: &std::path::Path| harness.provider.accepts_attachment(path);
    for prompt in &options.prompts {
        turns += 1;
        // Ответ прошлого хода — не ответ этого: при провале вернули бы его.
        verdict.final_text.clear();
        let first = if turns == 1 {
            &harness.attachments[..]
        } else {
            &[]
        };
        history.messages.push(crate::app::attachments::user_message(
            prompt, prompt, first, first, &accepts,
        ));
        let turn_start = history.messages.len();
        debug_log::log_json(
            TURN_STARTED,
            &serde_json::json!({
                "turn": turns,
                "of": options.prompts.len(),
                "prompt": prompt,
                "history_messages": history.messages.len(),
                // По нему метрики отделяют корень от суб-агентов.
                "conversation": root.0,
            }),
        );

        // Как `App::send_turn`: ход начат — заметки фоновых агентов ждут его конца.
        reduce::apply(&mut history, &AgentEvent::Started);
        let root_handle = harness.runtime.spawn(TurnSpec {
            conversation: root,
            provider: Arc::clone(&harness.provider),
            // Как в TUI: заметки только для глаз человека модели не уходят.
            messages: history
                .messages
                .iter()
                .filter(|message| !message.ui_only)
                .cloned()
                .collect(),
            system_prompt: harness.system_prompt.clone(),
            model: config.provider.model.clone(),
            temperature: config.provider.temperature,
            max_tokens: config.provider.max_tokens,
            max_steps: config.agent.max_steps_per_turn.max(1),
            max_tools_per_step: config.agent.max_tools_per_step.max(1),
            unattended: false,
            tool_output_limit: config.context.tool_output_limit as usize,
            context: crate::context::ContextSpec::new(&config.context, provider_window, &run_id)
                .with_output_cap(config.provider.max_tokens),
        });

        let mut pending: HashSet<ConversationId> = HashSet::from([root]);
        let mut handles = vec![root_handle];
        // Почему ход корня остановлен извне — сигнал, срок, закрытый канал.
        let mut interrupted: Option<String> = None;

        while !pending.is_empty() {
            tokio::select! {
                // Без этого Ctrl+C убивал бы процесс, а дерево команды хода
                // (своя группа на Unix) оставалось бы жить.
                () = &mut shutdown => {
                    let reason = format!("interrupted by a signal during turn {turns}");
                    verdict.error.get_or_insert_with(|| reason.clone());
                    verdict.status = RunStatus::Failed;
                    // Корень уже ответил — его ход не «оборван».
                    if pending.contains(&root) {
                        interrupted = Some(reason);
                    }
                    break;
                }
                () = &mut deadline => {
                    let root_active = pending.contains(&root);
                    verdict.on_deadline(
                        root_active,
                        pending.len(),
                        options.timeout,
                        format!("{turns}/{}", options.prompts.len()),
                    );
                    if root_active {
                        interrupted = Some(format!("the run's {}s budget ran out", options.timeout.as_secs()));
                    }
                    // Бюджет кончился: следующий ход не начинаем.
                    budget_spent = true;
                    break;
                }
                event = event_rx.recv() => {
                    let Some(event) = event else {
                        let reason = "the event channel closed".to_string();
                        verdict.error.get_or_insert_with(|| reason.clone());
                        verdict.status = RunStatus::Failed;
                        interrupted = Some(reason);
                        break;
                    };
                    match event {
                        // История меняется тем же редьюсером, что и в TUI, —
                        // иначе харнесс мерил бы не то поведение, что видит
                        // человек.
                        AppEvent::Agent { conversation, event: agent_event } => {
                            let end = if conversation == root {
                                reduce::apply(&mut history, &agent_event)
                            } else {
                                reduce::turn_end(&agent_event)
                            };
                            trace_agent_event(conversation, &agent_event);
                            verdict.on_tool_event(conversation, &agent_event);
                            verdict.remember_text(conversation, &agent_event);
                            if end.is_none() {
                                continue;
                            }
                            pending.remove(&conversation);
                            if conversation != root {
                                // Итог — в историю корня, как в TUI: иначе следующий
                                // ход и `--resume` расходились бы с ним.
                                if let Some(note) = verdict.on_background_end(conversation, &agent_event) {
                                    history.deliver_note(ChatMessage::system(&note));
                                }
                                continue;
                            }
                            match agent_event {
                                AgentEvent::Done(result) => {
                                    if let Some(note) = result.truncated {
                                        verdict.warnings.push(format!("the final answer is incomplete: {note}"));
                                    }
                                    verdict.final_text = result.text;
                                }
                                AgentEvent::Failed(message) => {
                                    verdict.close_in_flight(root, "the turn failed before this tool finished");
                                    verdict.error.get_or_insert(message);
                                    verdict.status = RunStatus::Failed;
                                }
                                _ => {}
                            }
                        }
                        AppEvent::RequestToolApproval(request) => {
                            let approved = options.approve.decide(
                                &request.tool_name,
                                request.scope.as_ref(),
                                &whitelist,
                                request.always_ask,
                            );
                            record_approval(&request, approved);
                            verdict.on_approval(&request, approved);
                            tokio::spawn(async move { request.resolve(approved).await });
                        }
                        AppEvent::RequestQuestion(request, state) => {
                            let answered = answer_for(options, &state);
                            debug_log::log_json(
                                action::QUESTION,
                                &serde_json::json!({
                                    "question": state.question,
                                    "options": state.options,
                                    "answered": answered.answer,
                                    "source": answered.source,
                                }),
                            );
                            verdict.questions.push(state.question.clone());
                            record_question(&request, answered.answer.clone());
                            verdict.answered.push(answered);
                        }
                        AppEvent::SpawnSubAgent { parent, label, prompt } => {
                            verdict.sub_agents += 1;
                            let id = ConversationId::next();
                            debug_log::log_json(
                                action::SUB_AGENT,
                                &serde_json::json!({
                                    "parent": parent.0,
                                    "conversation": id.0,
                                    "label": label,
                                    "prompt": prompt,
                                }),
                            );
                            pending.insert(id);
                            verdict.background.insert(id, label);
                            let fork = harness.provider.fork();
                            verdict.forks.push(Arc::clone(&fork));
                            // Тот же рецепт, что у TUI: иначе харнесс мерил бы другой промпт.
                            handles.push(harness.runtime.spawn(TurnSpec::background(
                                id,
                                fork,
                                &prompt,
                                &harness.system_prompt,
                                config,
                                crate::context::ContextSpec::new(&config.context, provider_window, &run_id),
                            )));
                        }
                        AppEvent::ContextWindowLearned(window) => {
                            provider_window = window;
                            debug_log::log_json(
                                action::CONTEXT_WINDOW,
                                &serde_json::json!({ "provider_window": window }),
                            );
                        }
                        // Everything else is TUI chrome (status lines, server
                        // and update events) with no bearing on the run.
                        _ => {}
                    }
                }
            }
        }

        for handle in handles {
            handle.abort();
        }
        if let Some(reason) = interrupted {
            verdict.close_in_flight(root, &reason);
            close_unfinished_turn(&mut history, &mut verdict, turn_start, &reason);
            was_interrupted = true;
        }
        // A turn that timed out, errored, or lost its event channel leaves a
        // half-written history; running the next prompt on top of it would
        // measure nothing useful.
        if verdict.status != RunStatus::Completed || budget_spent {
            break;
        }
    }

    drop(event_tx);
    // Разом и с одним сроком: по очереди десяток зависших форков съел бы
    // запас сценария на завершение ребёнка.
    let forks = std::mem::take(&mut verdict.forks);
    let discards =
        futures::future::join_all(forks.iter().map(|fork| fork.discard_remote_session()));
    let _ = tokio::time::timeout(Duration::from_secs(5), discards).await;

    let session_id = if options.saves_session() {
        match persist(config, &history, &harness.provider, was_interrupted) {
            Ok(id) => Some(id),
            Err(error) => {
                verdict.warnings.push(format!(
                    "the session could not be saved, so --resume will not work: {error}"
                ));
                None
            }
        }
    } else {
        None
    };
    verdict.into_outcome(options, turns, session_id)
}

/// Ход корня оборван извне. Закрыть его, как TUI закрывает проваленный: без
/// этого сессия сохранялась с вызовами без результатов, и `--resume` ломался.
fn close_unfinished_turn(
    history: &mut Conversation,
    verdict: &mut Verdict,
    turn_start: usize,
    reason: &str,
) {
    reduce::apply(history, &AgentEvent::Failed(reason.to_string()));
    // Серверная ветка могла уйти вперёд локальной копии — продолжать её нельзя.
    history.broken = true;
    if let Some(partial) = history.messages[turn_start.min(history.messages.len())..]
        .iter()
        .rev()
        .find(|message| {
            message.role == crate::provider::Role::Assistant && !message.content.trim().is_empty()
        })
    {
        verdict.final_text = partial.content.clone();
        verdict
            .warnings
            .push("final_text is the partial reply of a turn that did not finish".to_string());
    }
}

/// Всё, что трасса знает об агентском событии. Отдельная функция, потому
/// что редьюсер историю уже применил и возвращаться к разбору поздно.
fn trace_agent_event(conversation: ConversationId, event: &AgentEvent) {
    match event {
        AgentEvent::Message(message) => debug_log::log_json(
            action::MESSAGE,
            &serde_json::json!({
                "conversation": conversation.0,
                "role": &message.role,
                "content": message.content,
                "tool_error": message.tool_error,
            }),
        ),
        AgentEvent::Failed(message) => debug_log::log_json(
            action::MESSAGE,
            &serde_json::json!({
                "conversation": conversation.0,
                "role": "error",
                "content": message,
            }),
        ),
        AgentEvent::ToolError { error } => debug_log::log_json(
            action::TOOL_RESULT,
            &serde_json::json!({ "conversation": conversation.0, "ok": false, "error": error }),
        ),
        AgentEvent::ToolOutputCleared {
            cleared,
            freed_tokens,
        } => debug_log::log_json(
            action::TOOL_OUTPUT_CLEARED,
            &serde_json::json!({
                "conversation": conversation.0,
                "cleared": cleared.len(),
                "freed_tokens": freed_tokens,
            }),
        ),
        _ => {}
    }
}

fn record_approval(request: &ToolApprovalRequest, approved: bool) {
    debug_log::log_json(
        action::APPROVAL,
        &serde_json::json!({
            "tool": request.tool_name,
            "arguments": request.arguments,
            "approved": approved,
        }),
    );
}

fn record_question(request: &QuestionRequest, answer: String) {
    let request = request.clone();
    tokio::spawn(async move { request.resolve(answer).await });
}

fn answer_for(options: &ExecOptions, state: &QuestionState) -> AnsweredQuestion {
    let (answer, source) = match (&options.answer, state.options.first()) {
        (Some(answer), _) => (answer.clone(), "flag"),
        (None, Some(first)) => (first.clone(), "default"),
        (None, None) => (String::new(), "none"),
    };
    AnsweredQuestion {
        question: state.question.clone(),
        options: state.options.clone(),
        answer,
        source: source.to_string(),
    }
}

/// Write the conversation to its session file — the same id it was resumed
/// from, or the fresh one `Conversation::fresh_main` minted.
///
/// The metadata is sampled exactly as `App::auto_save_session` samples it: a
/// provider that reports an identity has just proved the remote link works, so
/// it is recorded (this is what lets the *next* `--resume` continue the same
/// server-side thread) and clears a stale `broken` flag. A provider that
/// reports none says only that *this* run established nothing — a run that
/// died on an expired token still must not erase the link it resumed
/// (`RemoteLink::Unknown`).
fn persist(
    config: &Config,
    conversation: &Conversation,
    provider: &Arc<dyn LLMProvider>,
    interrupted: bool,
) -> Result<String, String> {
    let workspace = std::env::current_dir()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_default();
    let remote = crate::session::RemoteLink::from_identity(provider.session_identity());
    let meta = crate::session::SessionMeta {
        tag: conversation.tag.clone(),
        // Оборванный ход: живая связь не значит, что ветка сервера совпадает с
        // локальной историей, — следующий `--resume` перескажет её заново.
        broken: conversation.broken && (interrupted || !remote.is_established()),
        remote,
        route: provider.route_state(),
    };
    match crate::session::save_session(
        &conversation.session_id,
        &conversation.session_started_at,
        &conversation.messages,
        config,
        &workspace,
        &meta,
    ) {
        Ok(()) => Ok(conversation.session_id.clone()),
        Err(error) => {
            tracing::warn!("harness: could not save session: {error}");
            Err(error.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approve_policy_parses_every_form() {
        assert_eq!("all".parse::<ApprovePolicy>().unwrap(), ApprovePolicy::All);
        assert_eq!(
            "none".parse::<ApprovePolicy>().unwrap(),
            ApprovePolicy::None
        );
        assert_eq!(
            "whitelist".parse::<ApprovePolicy>().unwrap(),
            ApprovePolicy::Whitelist
        );
        assert_eq!(
            "except:bash, powershell".parse::<ApprovePolicy>().unwrap(),
            ApprovePolicy::Except(vec!["bash".into(), "powershell".into()])
        );
        assert!("except:".parse::<ApprovePolicy>().is_err());
        assert!("maybe".parse::<ApprovePolicy>().is_err());
    }

    #[test]
    fn policies_decide_per_tool() {
        use crate::whitelist::{Rule, Whitelist};
        let whitelist = Whitelist::from_rules(vec![Rule::tool("bash")]);
        assert!(ApprovePolicy::All.decide("write", None, &whitelist, false));
        assert!(!ApprovePolicy::None.decide("bash", None, &whitelist, false));
        assert!(ApprovePolicy::Whitelist.decide("bash", None, &whitelist, false));
        assert!(!ApprovePolicy::Whitelist.decide("write", None, &whitelist, false));
        let except = ApprovePolicy::Except(vec!["bash".to_string()]);
        assert!(!except.decide("bash", None, &whitelist, false));
        assert!(except.decide("powershell", None, &whitelist, false));
        // Чужая разметка или поправленное имя: белый список не решает — как в TUI.
        assert!(!ApprovePolicy::Whitelist.decide("bash", None, &whitelist, true));
    }

    /// Политика `whitelist` в сценариях должна уважать область правила, иначе
    /// харнесс проверял бы не ту модель разрешений, что живёт в TUI.
    #[test]
    fn the_whitelist_policy_respects_a_rule_scope() {
        use crate::whitelist::{Rule, Whitelist};
        let whitelist = Whitelist::from_rules(vec![Rule::scoped(
            "bash",
            crate::whitelist::Scope::Command(vec!["cargo".into(), "test".into()]),
        )]);
        let allowed = crate::whitelist::Scope::Command(vec!["cargo".into(), "test".into()]);
        assert!(ApprovePolicy::Whitelist.decide("bash", Some(&allowed), &whitelist, false));
        let other = crate::whitelist::Scope::Command(vec!["rm".into(), "-rf".into()]);
        assert!(!ApprovePolicy::Whitelist.decide("bash", Some(&other), &whitelist, false));
    }

    #[test]
    fn status_maps_to_distinct_exit_codes() {
        let codes = [
            RunStatus::Completed,
            RunStatus::Failed,
            RunStatus::TimedOut,
            RunStatus::SetupFailed,
        ]
        .map(RunStatus::exit_code);
        assert_eq!(codes, [0, 1, 2, 3]);
    }

    #[test]
    fn canned_answer_wins_over_first_option() {
        let state = QuestionState::new("q".into(), vec!["a".into(), "b".into()], false);
        let mut options = probe_options();
        let picked = answer_for(&options, &state);
        assert_eq!(
            (picked.answer.as_str(), picked.source.as_str()),
            ("a", "default")
        );
        options.answer = Some("custom".into());
        let picked = answer_for(&options, &state);
        assert_eq!(
            (picked.answer.as_str(), picked.source.as_str()),
            ("custom", "flag")
        );
        let empty = QuestionState::new("q".into(), Vec::new(), false);
        options.answer = None;
        let picked = answer_for(&options, &empty);
        assert_eq!(
            (picked.answer.as_str(), picked.source.as_str()),
            ("", "none")
        );
    }

    #[test]
    fn context_overrides_rewrite_only_what_they_name() {
        let mut config = crate::config::ContextConfig::default();
        let before = config.clone();
        ContextOverrides {
            window: Some(12_000),
            preserve_recent_tokens: Some(500),
            ..ContextOverrides::default()
        }
        .apply(&mut config);
        assert_eq!(config.context_window, 12_000);
        assert_eq!(config.preserve_recent_tokens, 500);
        // Everything unset keeps whatever the config file said.
        assert_eq!(config.reserved_tokens, before.reserved_tokens);
        assert_eq!(config.tool_output_limit, before.tool_output_limit);
        assert_eq!(config.auto_compact, before.auto_compact);
    }

    /// A scenario window has to reach the budget the ladder measures against,
    /// or the plumbing is decorative.
    #[test]
    fn a_scenario_window_produces_a_usable_budget() {
        let mut config = Config::default();
        ContextOverrides {
            window: Some(12_000),
            reserved_tokens: Some(1_000),
            ..ContextOverrides::default()
        }
        .apply(&mut config.context);
        let spec = crate::context::ContextSpec::new(&config.context, 0, "run")
            .with_output_cap(config.provider.max_tokens);
        assert_eq!(spec.budget().usable(), Some(11_000));
    }

    /// Вызов суб-агента идёт внутри вызова `task` того же чата: его конец не
    /// должен закрывать сам `task`.
    #[test]
    fn a_nested_sub_agent_call_does_not_close_the_task_call() {
        let mut verdict = Verdict::new();
        let root = ConversationId::next();
        let started = |name: &str| AgentEvent::ToolStarted {
            name: name.to_string(),
            summary: String::new(),
        };
        verdict.on_tool_event(root, &started("task"));
        verdict.on_tool_event(root, &started("look › bash"));
        verdict.on_tool_event(
            root,
            &AgentEvent::ToolError {
                error: "exit 1".to_string(),
            },
        );
        verdict.on_tool_event(
            root,
            &AgentEvent::ToolDone {
                result: "ok".to_string(),
            },
        );
        assert!(verdict.tools[0].ok, "task itself succeeded");
        assert!(!verdict.tools[1].ok, "the nested call failed");
        assert_eq!(verdict.tools[1].error.as_deref(), Some("exit 1"));
    }

    /// Корень закончил, ждали только фоновых — это не «тайм-аут прогона».
    #[test]
    fn a_deadline_after_the_root_finished_is_a_warning() {
        let mut verdict = Verdict::new();
        verdict.on_deadline(false, 1, Duration::from_secs(10), "1/1".to_string());
        assert_eq!(verdict.status, RunStatus::Completed);
        assert_eq!(verdict.warnings.len(), 1);
        let mut verdict = Verdict::new();
        verdict.error = Some("provider said no".to_string());
        verdict.on_deadline(true, 1, Duration::from_secs(10), "1/1".to_string());
        assert_eq!(verdict.status, RunStatus::TimedOut);
        assert!(verdict.error.unwrap().contains("provider said no"));
    }

    /// Опечатка в `--provider` раньше молча шла на DeepSeek.
    #[test]
    fn an_unknown_provider_is_a_setup_failure() {
        let options = ExecOptions {
            provider: Some("deepsek".to_string()),
            ..probe_options()
        };
        let refused = check_provider(&Config::default(), &options).unwrap_err();
        assert!(refused.contains("unknown provider 'deepsek'"), "{refused}");
        let fine = ExecOptions {
            provider: Some("deepseek".to_string()),
            ..probe_options()
        };
        assert!(check_provider(&Config::default(), &fine).is_ok());
    }

    fn probe_options() -> ExecOptions {
        ExecOptions {
            prompts: Vec::new(),
            workspace: None,
            trace_path: PathBuf::from("trace.jsonl"),
            approve: ApprovePolicy::All,
            answer: None,
            max_steps: None,
            timeout: Duration::from_secs(1),
            semantic: SemanticMode::Off,
            mcp: false,
            provider: None,
            model: None,
            save_session: false,
            resume: None,
            system_append: None,
            attach: Vec::new(),
            context: ContextOverrides::default(),
        }
    }

    /// The whole point of multi-turn: the second request must carry the first
    /// turn's user message *and* the answer to it, with the new prompt last.
    #[tokio::test]
    async fn a_second_turn_is_sent_the_first_turns_history() {
        let fake = Arc::new(crate::provider::fake::FakeProvider::with_responses(vec![
            "first answer".to_string(),
            "second answer".to_string(),
        ]));
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let harness = probe_harness(Arc::clone(&fake) as Arc<dyn LLMProvider>, event_tx.clone());
        let options = ExecOptions {
            prompts: vec!["one".to_string(), "two".to_string()],
            timeout: Duration::from_secs(30),
            ..probe_options()
        };

        let outcome = drive(
            &probe_config(),
            &options,
            None,
            &harness,
            (event_tx, event_rx),
            tokio::time::Instant::now() + options.timeout,
        )
        .await;

        assert_eq!(outcome.status, RunStatus::Completed);
        assert_eq!(outcome.turns, 2);
        assert_eq!(outcome.final_text, "second answer");

        let first = fake.request(0).expect("first turn was sent");
        assert!(
            !first.iter().any(|m| m.content.contains("two")),
            "the first turn must not see the second prompt: {first:?}"
        );
        let second = fake.request(1).expect("second turn was sent");
        let contents: Vec<&str> = second.iter().map(|m| m.content.as_str()).collect();
        assert!(contents.contains(&"one"), "{contents:?}");
        assert!(
            contents.iter().any(|c| c.contains("first answer")),
            "{contents:?}"
        );
        assert_eq!(contents.last(), Some(&"two"), "{contents:?}");
    }

    /// A single prompt must still be a single turn, sent with nothing in front
    /// of it — the shape every existing scenario relies on.
    #[tokio::test]
    async fn a_single_prompt_is_still_one_turn() {
        let fake = Arc::new(crate::provider::fake::FakeProvider::with_response("done"));
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let harness = probe_harness(Arc::clone(&fake) as Arc<dyn LLMProvider>, event_tx.clone());
        let options = ExecOptions {
            prompts: vec!["only".to_string()],
            timeout: Duration::from_secs(30),
            ..probe_options()
        };

        let outcome = drive(
            &probe_config(),
            &options,
            None,
            &harness,
            (event_tx, event_rx),
            tokio::time::Instant::now() + options.timeout,
        )
        .await;

        assert_eq!(outcome.turns, 1);
        assert_eq!(outcome.final_text, "done");
        assert!(fake.request(1).is_none(), "a second turn was sent");
    }

    /// Корневой ход прогона не `unattended`: иначе `task` отвечал бы отказом,
    /// и суб-агенты нечем было бы проверить.
    /// Фоновый вызов — потому что только он идёт через `SpawnSubAgent`.
    #[tokio::test]
    async fn the_root_turn_can_still_spawn_a_sub_agent() {
        let call = r#"<tool_use><name>task</name><arguments>{"description":"probe","prompt":"go","background":true}</arguments></tool_use>"#;
        let fake = Arc::new(
            crate::provider::fake::FakeProvider::with_responses(vec![
                call.to_string(),
                "done".to_string(),
            ])
            .forks_with(vec!["probe finished".to_string()]),
        );
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let harness = probe_harness(Arc::clone(&fake) as Arc<dyn LLMProvider>, event_tx.clone());
        let options = ExecOptions {
            prompts: vec!["spawn one".to_string()],
            timeout: Duration::from_secs(30),
            ..probe_options()
        };

        let outcome = drive(
            &probe_config(),
            &options,
            None,
            &harness,
            (event_tx, event_rx),
            tokio::time::Instant::now() + options.timeout,
        )
        .await;

        assert_eq!(
            outcome.sub_agents, 1,
            "the root turn must not be unattended, or `task` is refused"
        );
        // Итог фонового агента — в вердикте, а не только в трассе.
        let result = outcome
            .sub_agent_results
            .first()
            .expect("the background result");
        assert_eq!(result.status, "completed");
        assert!(result.text.contains("probe finished"), "{}", result.text);
    }

    fn probe_config() -> Config {
        let mut config = Config::default();
        config.semantic.enabled = false;
        config.agent.max_steps_per_turn = 2;
        config
    }

    fn probe_harness(
        provider: Arc<dyn LLMProvider>,
        event_tx: mpsc::UnboundedSender<AppEvent>,
    ) -> Harness {
        let config = probe_config();
        let tools = Arc::new(ToolRegistry::new());
        let mcp = Arc::new(tokio::sync::Mutex::new(MCPManager::new()));
        let semantic = SemanticService::start(&config, Vec::new(), event_tx.clone());
        Harness {
            runtime: AgentRuntime::new(tools, Arc::clone(&mcp), semantic, event_tx),
            mcp,
            provider,
            system_prompt: "You are a test.".to_string(),
            semantic_ready: false,
            attachments: Vec::new(),
        }
    }

    /// Продолжение без сохранения — цепочка, которая рвётся на первом звене:
    /// следующий запуск не увидел бы последнего хода.
    #[test]
    fn resuming_implies_saving_the_session() {
        let mut options = probe_options();
        assert!(!options.saves_session());
        options.resume = Some("some-id".to_string());
        assert!(options.saves_session());
    }

    /// Неизвестный id обязан быть отказом до первого хода: прогон с нуля
    /// выглядел бы успешным, отвечая без истории, ради которой его запустили.
    #[test]
    fn an_unknown_session_id_is_a_setup_failure() {
        let options = ExecOptions {
            resume: Some("pooprusteek_test_no_such_session".to_string()),
            ..probe_options()
        };
        let error = load_resumed(&Config::default(), &options)
            .expect_err("an unknown id must not resume silently")
            .to_string();
        assert!(
            error.contains("pooprusteek_test_no_such_session"),
            "{error}"
        );

        // Без флага — ни загрузки, ни отказа.
        assert!(
            load_resumed(&Config::default(), &probe_options())
                .expect("no --resume is not an error")
                .is_none()
        );
    }

    /// Ради чего всё: следующий запуск и видит прошлый разговор, и дописывает
    /// себя в тот же файл — иначе цепочка из пяти ходов станет пятью сессиями.
    #[tokio::test]
    async fn a_resumed_run_sees_the_history_and_saves_back_to_the_same_id() {
        let id = "pooprusteek_test_harness_resume_roundtrip";
        let config = probe_config();
        let seeded = vec![
            ChatMessage::user("the first prompt"),
            ChatMessage::assistant("the first answer"),
        ];
        crate::session::save_session(
            id,
            "2026-01-01T00:00:00Z",
            &seeded,
            &config,
            "/tmp",
            &crate::session::SessionMeta::default(),
        )
        .expect("the session under test must be written");
        let path = Config::sessions_dir().join(format!("{id}.json"));

        let fake = Arc::new(crate::provider::fake::FakeProvider::with_response(
            "the second answer",
        ));
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let harness = probe_harness(Arc::clone(&fake) as Arc<dyn LLMProvider>, event_tx.clone());
        let options = ExecOptions {
            prompts: vec!["the second prompt".to_string()],
            resume: Some(id.to_string()),
            timeout: Duration::from_secs(30),
            ..probe_options()
        };
        let resumed = load_resumed(&config, &options)
            .expect("the planted session must load")
            .expect("--resume was set");

        let outcome = drive(
            &config,
            &options,
            Some(&resumed),
            &harness,
            (event_tx, event_rx),
            tokio::time::Instant::now() + options.timeout,
        )
        .await;

        let saved = crate::session::load_local(id, &config);
        let _ = std::fs::remove_file(&path);

        assert_eq!(outcome.status, RunStatus::Completed);
        assert_eq!(
            outcome.session_id.as_deref(),
            Some(id),
            "a resumed run must save back into the same session"
        );
        let sent = fake.request(0).expect("the turn was sent");
        let contents: Vec<&str> = sent.iter().map(|m| m.content.as_str()).collect();
        assert!(contents.contains(&"the first prompt"), "{contents:?}");
        assert!(contents.contains(&"the first answer"), "{contents:?}");
        assert_eq!(contents.last(), Some(&"the second prompt"), "{contents:?}");

        let saved = saved.expect("the session file must still be loadable");
        assert_eq!(
            saved.created_at, "2026-01-01T00:00:00Z",
            "created_at drifted"
        );
        assert_eq!(
            saved.messages.len(),
            4,
            "the run must append to the transcript it resumed: {:?}",
            saved.messages
        );
    }

    /// «Сессии нет» и «проверить не удалось» обязаны звучать в трассе по-
    /// разному: на одном слове для обоих исходов уже потерян день.
    #[test]
    fn a_failed_check_is_not_reported_as_a_missing_session() {
        assert_eq!(liveness_label(&SessionLiveness::Alive), None);
        assert_eq!(
            liveness_label(&SessionLiveness::Gone("invalid chat session id".into())),
            Some(("remote_session_gone", "invalid chat session id"))
        );
        assert_eq!(
            liveness_label(&SessionLiveness::Unknown("answered text/html".into())),
            Some(("remote_session_unverified", "answered text/html"))
        );
    }

    // ── Шлюз в прогоне ──────────────────────────────────────────────────

    #[test]
    fn a_broken_gateway_config_is_refused_before_the_first_turn() {
        let broken: crate::gateway::types::GatewayConfig = toml::from_str(
            "[gateway]
enabled = true
[[accounts]]
id = \"a\"
provider = \"custom\"
base_url = \"http://h/v1\"
secret = \"none\"
[accounts.limits]
max_concurrency = 0
",
        )
        .unwrap();
        let error = check_gateway_config(&broken).unwrap_err().to_string();
        assert!(error.contains("max_concurrency"), "{error}");

        let good: crate::gateway::types::GatewayConfig = toml::from_str(
            "[gateway]
enabled = true
[[accounts]]
id = \"a\"
provider = \"custom\"
base_url = \"http://h/v1\"
secret = \"none\"
model = \"m\"
",
        )
        .unwrap();
        assert!(check_gateway_config(&good).is_ok());
        // Выключенный шлюз не проверяется: прежнее поведение.
        let mut off = broken;
        off.gateway.enabled = false;
        assert!(check_gateway_config(&off).is_ok());
    }

    #[tokio::test]
    async fn the_gateway_of_a_run_comes_from_the_config_itself() {
        let off = harness_gateway(&Config::default()).await.unwrap();
        assert!(!off.is_enabled());
        let mut config = Config::default();
        config.gateway.enabled = true;
        config.accounts.push(
            toml::from_str(
                "id = \"a\"
provider = \"custom\"
base_url = \"http://127.0.0.1:1/v1\"
secret = \"none\"
model = \"m\"
",
            )
            .unwrap(),
        );
        let on = harness_gateway(&config).await.unwrap();
        assert!(on.is_enabled());
        assert_eq!(on.snapshot().accounts.len(), 1);
    }

    fn history_with_native_calls() -> Vec<crate::provider::ChatMessage> {
        use crate::provider::{ChatMessage, ToolCall};
        let mut assistant = ChatMessage::assistant("looking");
        assistant.tool_calls = vec![ToolCall {
            id: "call_1".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "a.rs"}),
            provider_state: Some(serde_json::json!({"thoughtSignature": "sig"})),
        }];
        vec![
            ChatMessage::user("read it"),
            assistant,
            ChatMessage::tool("call_1", "contents"),
            ChatMessage::user("and now?"),
        ]
    }

    fn saved_with_origin(account: &str) -> crate::session::Session {
        crate::session::Session {
            version: crate::session::SESSION_VERSION,
            id: "resumed".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            workspace_root: "/".to_string(),
            model_type: "m".to_string(),
            messages: history_with_native_calls(),
            tag: None,
            provider_session_id: None,
            provider_parent_message_id: None,
            broken: false,
            route_state: Some(crate::gateway::state::RouteState {
                route: None,
                account: crate::gateway::types::AccountId::parse(account).unwrap(),
                model: format!("model-{account}"),
            }),
        }
    }

    async fn ask_history(
        provider: &Arc<dyn LLMProvider>,
        messages: Vec<crate::provider::ChatMessage>,
    ) {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        provider
            .complete_stream(crate::gateway::testkit::request(messages), tx)
            .await
            .unwrap();
    }

    /// Настоящий путь `--resume`: подсказка о цели, затем `reset()` внутри `replay_locally`.
    /// На старом коде `reset` стирал подсказку, и чужие подписи уходили второму аккаунту.
    #[tokio::test]
    async fn a_resumed_session_keeps_its_origin_through_the_reset_and_foreign_calls_are_flattened()
    {
        use crate::gateway::testkit::{FakeBackend, TWO_ACCOUNTS, rig};
        let rig = rig(
            TWO_ACCOUNTS,
            vec![
                ("a", FakeBackend::new(vec![])),
                ("b", FakeBackend::new(vec![])),
            ],
        );
        // История пришла с b, а ход идёт на a.
        let provider = rig.main();
        adopt_remote_session(&provider, &saved_with_origin("b")).await;
        ask_history(&provider, history_with_native_calls()).await;
        let seen = rig.fakes["a"].last_messages();
        assert!(seen[1].tool_calls.is_empty(), "no foreign ids reach a");
        assert!(seen[1].content.contains("<tool_use><name>read_file</name>"));
        assert_eq!(
            seen[2].content, "contents",
            "the result keeps its call in the text"
        );

        // Обратная сторона: история пришла с самого a — вызовы остаются родными.
        let own = rig.main();
        adopt_remote_session(&own, &saved_with_origin("a")).await;
        ask_history(&own, history_with_native_calls()).await;
        assert_eq!(rig.fakes["a"].last_messages()[1].tool_calls.len(), 1);
    }
}
