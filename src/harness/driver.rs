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
//! - `auto_approve` stays **false**. Approvals are answered from
//!   [`ApprovePolicy`] instead of being bypassed, so the real approval path
//!   is exercised, denial is testable, and the `task` tool still works
//!   (`runner` refuses sub-agents when `auto_approve` is on).
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
use crate::provider::{ChatMessage, LLMProvider};
use crate::semantic::SemanticService;
use crate::skills::discovery::discover_all_skills;
use crate::tools::registry::ToolRegistry;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Marks where one user turn ends and the next begins in the trace.
const TURN_STARTED: &str = "harness.turn.started";

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
    fn decide(&self, tool: &str, whitelist: &HashSet<String>) -> bool {
        match self {
            Self::All => true,
            Self::None => false,
            Self::Whitelist => whitelist.contains(tool),
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
    /// Extra instructions appended to the assembled system prompt. Prompt
    /// wording is the cheapest variable to change and one of the most
    /// influential, so it is a first-class knob here.
    pub system_append: Option<PathBuf>,
    /// Compaction settings layered over the config, so a scenario can put the
    /// ladder in reach without anyone hand-editing a config file.
    pub context: ContextOverrides,
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
}

fn one_turn() -> usize {
    1
}

impl RunOutcome {
    fn setup_failed(trace_path: PathBuf, error: impl Into<String>) -> Self {
        Self {
            status: RunStatus::SetupFailed,
            trace_path,
            duration_ms: 0,
            turns: 0,
            final_text: String::new(),
            tools: Vec::new(),
            sub_agents: 0,
            questions: Vec::new(),
            error: Some(error.into()),
            session_id: None,
            semantic_ready: false,
        }
    }
}

/// Deps a turn runs against, assembled the way `App::new` assembles them.
struct Harness {
    runtime: AgentRuntime,
    provider: Arc<dyn LLMProvider>,
    system_prompt: String,
    semantic_ready: bool,
}

/// Run one turn to completion. Errors are returned as a `SetupFailed`
/// outcome rather than an `Err` whenever the run itself produced a verdict —
/// `Err` is reserved for "could not even open the trace".
pub async fn exec(mut config: Config, mut options: ExecOptions) -> AppResult<RunOutcome> {
    // Absolutize the trace path *before* the chdir below. A relative path
    // resolved afterwards would land inside the workspace under test — which
    // both hides the trace and contaminates the very directory the turn is
    // measured against (the agent sees the harness's own scratch files).
    options.trace_path = absolutize(&options.trace_path)?;
    if let Some(parent) = options.trace_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| AppError::Custom(format!("{}: {e}", parent.display())))?;
    }

    if let Some(workspace) = &options.workspace {
        std::env::set_current_dir(workspace)
            .map_err(|e| AppError::Custom(format!("workspace {}: {e}", workspace.display())))?;
    }

    debug_log::configure(options.trace_path.clone(), debug_log::Format::Jsonl);
    debug_log::init(true)?;

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

    let workspace = std::env::current_dir()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_default();

    debug_log::log_json(
        action::RUN_STARTED,
        &serde_json::json!({
            // `prompt` stays the first turn so existing trace readers keep working.
            "prompt": options.prompts.first().cloned().unwrap_or_default(),
            "prompts": options.prompts,
            "turns": options.prompts.len(),
            "workspace": workspace,
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

    let started = Instant::now();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let harness = match assemble(&config, &options, &workspace, event_tx.clone()).await {
        Ok(harness) => harness,
        Err(error) => {
            let outcome = RunOutcome::setup_failed(options.trace_path.clone(), error.to_string());
            finish(&outcome);
            return Ok(outcome);
        }
    };

    let mut outcome = drive(&config, &options, &harness, event_tx, event_rx, started).await;
    outcome.semantic_ready = harness.semantic_ready;
    finish(&outcome);
    Ok(outcome)
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

fn finish(outcome: &RunOutcome) {
    debug_log::log_json(action::RUN_FINISHED, outcome);
}

/// Build provider, tools, skills, semantic service and system prompt.
async fn assemble(
    config: &Config,
    options: &ExecOptions,
    workspace: &str,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) -> AppResult<Harness> {
    let provider = crate::provider::build_provider(config).ok_or_else(|| {
        AppError::Custom(
            "no provider configured: set [provider].token or pick a /providers entry".to_string(),
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
        crate::instructions::load(workspace, config.instructions.max_bytes).section
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
        runtime: AgentRuntime::new(tools, mcp, semantic, event_tx),
        provider,
        system_prompt,
        semantic_ready,
    })
}

/// Poll readiness rather than subscribe: `SemanticStatus` events are
/// human-readable strings, and `is_ready` is the actual gate the turn path
/// consults.
async fn await_semantic(semantic: &Arc<SemanticService>, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if semantic.is_ready() {
            return true;
        }
        if !semantic.is_enabled() {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    semantic.is_ready()
}

/// Drive every turn in order, servicing each one's events until its whole
/// conversation tree is done. One history, one provider session, one budget:
/// the turns are a conversation, not a batch of independent runs.
async fn drive(
    config: &Config,
    options: &ExecOptions,
    harness: &Harness,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    mut event_rx: mpsc::UnboundedReceiver<AppEvent>,
    started: Instant,
) -> RunOutcome {
    let whitelist = crate::whitelist::load();
    let root = ConversationId::next();
    // History is accumulated through the app's own reducer, so turn two sees
    // exactly what turn two in the TUI would see.
    let mut history = Conversation::fresh_main(None);
    // Namespaces this run's spill directory, the way a session id does in the app.
    let run_id = crate::session::create_session_id();
    // Filled in by `ContextWindowLearned` if the provider answers. Read when
    // each turn's `ContextSpec` is built, exactly as `App` reads its own
    // `provider_context_window`: a window learned during turn 1 governs turn 2.
    let mut provider_window = 0u32;

    let mut tools: Vec<ToolInvocation> = Vec::new();
    let mut questions = Vec::new();
    let mut sub_agents = 0usize;
    let mut final_text = String::new();
    let mut error = None;
    let mut status = RunStatus::Completed;
    let mut turns = 0usize;

    // One budget for the whole run. Arming it per turn would let a three-turn
    // scenario quietly spend three times its declared timeout.
    let deadline = tokio::time::sleep(options.timeout);
    tokio::pin!(deadline);

    for prompt in &options.prompts {
        turns += 1;
        history.messages.push(ChatMessage::user(prompt));
        debug_log::log_json(
            TURN_STARTED,
            &serde_json::json!({
                "turn": turns,
                "of": options.prompts.len(),
                "prompt": prompt,
                "history_messages": history.messages.len(),
            }),
        );

        let root_handle = harness.runtime.spawn(TurnSpec {
            conversation: root,
            provider: Arc::clone(&harness.provider),
            messages: history.messages.clone(),
            system_prompt: harness.system_prompt.clone(),
            model: config.provider.model.clone(),
            temperature: config.provider.temperature,
            max_tokens: config.provider.max_tokens,
            max_steps: config.agent.max_steps_per_turn.max(1),
            max_tools_per_step: config.agent.max_tools_per_step.max(1),
            auto_approve: false,
            tool_output_limit: config.context.tool_output_limit as usize,
            context: crate::context::ContextSpec::new(&config.context, provider_window, &run_id)
                .with_output_cap(config.provider.max_tokens),
        });

        let mut pending: HashSet<ConversationId> = HashSet::from([root]);
        let mut handles = vec![root_handle];
        let mut stopped = false;

        while !pending.is_empty() {
            tokio::select! {
                () = &mut deadline => {
                    status = RunStatus::TimedOut;
                    error = Some(format!(
                        "run exceeded {}s during turn {}/{} with {} conversation(s) still active",
                        options.timeout.as_secs(),
                        turns,
                        options.prompts.len(),
                        pending.len()
                    ));
                    stopped = true;
                    break;
                }
                event = event_rx.recv() => {
                    let Some(event) = event else {
                        stopped = true;
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
                            if let AgentEvent::ToolError { .. } = &agent_event
                                && let Some(last) = tools.last_mut()
                            {
                                last.ok = false;
                            }
                            match end {
                                Some(reduce::TurnEnd::Done) => {
                                    if conversation == root
                                        && let AgentEvent::Done(result) = agent_event
                                    {
                                        final_text = result.text;
                                    }
                                    pending.remove(&conversation);
                                }
                                Some(reduce::TurnEnd::Failed(message)) => {
                                    if error.is_none() {
                                        error = Some(message);
                                        status = RunStatus::Failed;
                                    }
                                    pending.remove(&conversation);
                                }
                                None => {}
                            }
                        }
                        AppEvent::RequestToolApproval(request) => {
                            let approved = options.approve.decide(&request.tool_name, &whitelist);
                            record_approval(&request, approved);
                            tools.push(ToolInvocation {
                                name: request.tool_name.clone(),
                                approved,
                                ok: approved,
                            });
                            tokio::spawn(async move { request.resolve(approved).await });
                        }
                        AppEvent::RequestQuestion(request, state) => {
                            let answer = answer_for(options, &state);
                            questions.push(state.question.clone());
                            debug_log::log_json(
                                action::QUESTION,
                                &serde_json::json!({
                                    "question": state.question,
                                    "options": state.options,
                                    "answered": answer,
                                }),
                            );
                            record_question(&request, answer);
                        }
                        AppEvent::SpawnSubAgent { parent, label, prompt } => {
                            sub_agents += 1;
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
                            handles.push(harness.runtime.spawn(TurnSpec {
                                conversation: id,
                                provider: harness.provider.fork(),
                                messages: vec![ChatMessage::user(&prompt)],
                                system_prompt: harness.system_prompt.clone(),
                                model: config.provider.model.clone(),
                                temperature: config.provider.temperature,
                                max_tokens: config.provider.max_tokens,
                                // Same clamp the TUI applies to background turns.
                                max_steps: config.agent.max_steps_per_turn.clamp(1, 8),
                                max_tools_per_step: config.agent.max_tools_per_step.max(1),
                                auto_approve: true,
                                tool_output_limit: config.context.tool_output_limit as usize,
                                context: crate::context::ContextSpec::new(
                                    &config.context,
                                    provider_window,
                                    &run_id,
                                )
                                .with_output_cap(config.provider.max_tokens),
                            }));
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
        // A turn that timed out, errored, or lost its event channel leaves a
        // half-written history; running the next prompt on top of it would
        // measure nothing useful.
        if stopped || status != RunStatus::Completed {
            break;
        }
    }

    drop(event_tx);

    let session_id = if options.save_session {
        persist(config, &history.messages)
    } else {
        None
    };

    RunOutcome {
        status,
        trace_path: options.trace_path.clone(),
        duration_ms: started.elapsed().as_millis() as u64,
        turns,
        final_text,
        tools,
        sub_agents,
        questions,
        error,
        session_id,
        semantic_ready: false,
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
            &serde_json::json!({ "ok": false, "error": error }),
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

fn answer_for(options: &ExecOptions, state: &QuestionState) -> String {
    match &options.answer {
        Some(answer) => answer.clone(),
        None => state.options.first().cloned().unwrap_or_default(),
    }
}

fn persist(config: &Config, transcript: &[ChatMessage]) -> Option<String> {
    let id = crate::session::create_session_id();
    let workspace = std::env::current_dir()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_default();
    let meta = crate::session::SessionMeta::default();
    match crate::session::save_session(
        &id,
        &crate::session::timestamp_now(),
        transcript,
        config,
        &workspace,
        &meta,
    ) {
        Ok(()) => Some(id),
        Err(error) => {
            tracing::warn!("harness: could not save session: {error}");
            None
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
        let whitelist = HashSet::from(["bash".to_string()]);
        assert!(ApprovePolicy::All.decide("write", &whitelist));
        assert!(!ApprovePolicy::None.decide("bash", &whitelist));
        assert!(ApprovePolicy::Whitelist.decide("bash", &whitelist));
        assert!(!ApprovePolicy::Whitelist.decide("write", &whitelist));
        let except = ApprovePolicy::Except(vec!["bash".to_string()]);
        assert!(!except.decide("bash", &whitelist));
        assert!(except.decide("powershell", &whitelist));
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
        assert_eq!(answer_for(&options, &state), "a");
        options.answer = Some("custom".into());
        assert_eq!(answer_for(&options, &state), "custom");
        let empty = QuestionState::new("q".into(), Vec::new(), false);
        options.answer = None;
        assert_eq!(answer_for(&options, &empty), "");
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
            system_append: None,
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
            &harness,
            event_tx,
            event_rx,
            Instant::now(),
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
            &harness,
            event_tx,
            event_rx,
            Instant::now(),
        )
        .await;

        assert_eq!(outcome.turns, 1);
        assert_eq!(outcome.final_text, "done");
        assert!(fake.request(1).is_none(), "a second turn was sent");
    }

    /// Инвариант 11: корневой ход идёт с `auto_approve: false`. При `true`
    /// инструмент `task` отвечает отказом, и суб-агенты нечем проверить.
    /// Фоновый вызов — потому что только он идёт через `SpawnSubAgent`.
    #[tokio::test]
    async fn the_root_turn_can_still_spawn_a_sub_agent() {
        let call = r#"<tool_use><name>task</name><arguments>{"description":"probe","prompt":"go","background":true}</arguments></tool_use>"#;
        let fake = Arc::new(crate::provider::fake::FakeProvider::with_responses(vec![
            call.to_string(),
            "done".to_string(),
        ]));
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
            &harness,
            event_tx,
            event_rx,
            Instant::now(),
        )
        .await;

        assert_eq!(
            outcome.sub_agents, 1,
            "the root turn must keep auto_approve false, or `task` is refused"
        );
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
            runtime: AgentRuntime::new(tools, mcp, semantic, event_tx),
            provider,
            system_prompt: "You are a test.".to_string(),
            semantic_ready: false,
        }
    }
}
