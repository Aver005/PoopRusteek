//! Runs one real agent turn without a terminal.
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

use crate::app::conversation::ConversationId;
use crate::app::events::{AppEvent, QuestionRequest, QuestionState, ToolApprovalRequest};
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

/// Everything one headless turn needs.
#[derive(Debug, Clone)]
pub struct ExecOptions {
    pub prompt: String,
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
    /// Assistant text of the final step, i.e. the turn's answer.
    pub final_text: String,
    pub tools: Vec<ToolInvocation>,
    pub sub_agents: usize,
    pub questions: Vec<String>,
    pub error: Option<String>,
    pub session_id: Option<String>,
    /// `true` when the semantic layer was ready by the time the turn ran.
    pub semantic_ready: bool,
}

impl RunOutcome {
    fn setup_failed(trace_path: PathBuf, error: impl Into<String>) -> Self {
        Self {
            status: RunStatus::SetupFailed,
            trace_path,
            duration_ms: 0,
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

    let workspace = std::env::current_dir()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_default();

    debug_log::log_json(
        action::RUN_STARTED,
        &serde_json::json!({
            "prompt": options.prompt,
            "workspace": workspace,
            "model": config.active_model(),
            "provider": config.active_provider.clone().unwrap_or_else(|| "deepseek".into()),
            "approve": format!("{:?}", options.approve),
            "semantic": format!("{:?}", options.semantic),
            "mcp": options.mcp,
            "max_steps": config.agent.max_steps_per_turn,
            "timeout_ms": options.timeout.as_millis(),
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

    let mut system_prompt = crate::app::system_prompt::build(
        &crate::prompts::load_prompt_files(),
        &skills,
        config.skills.injection,
        &tools,
        &mcp,
        config.effective_mcp_schema_mode(),
        workspace,
    )
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

/// Spawn the turn and service its events until every conversation in the
/// tree is done or the budget runs out.
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
    let user_message = ChatMessage::user(&options.prompt);
    let mut transcript = vec![user_message.clone()];

    let root_handle = harness.runtime.spawn(TurnSpec {
        conversation: root,
        provider: Arc::clone(&harness.provider),
        messages: vec![user_message],
        system_prompt: harness.system_prompt.clone(),
        model: config.provider.model.clone(),
        temperature: config.provider.temperature,
        max_tokens: config.provider.max_tokens,
        max_steps: config.agent.max_steps_per_turn.max(1),
        max_tools_per_step: config.agent.max_tools_per_step.max(1),
        auto_approve: false,
        tool_output_limit: config.context.tool_output_limit as usize,
    });

    let mut pending: HashSet<ConversationId> = HashSet::from([root]);
    let mut handles = vec![root_handle];
    let mut tools: Vec<ToolInvocation> = Vec::new();
    let mut questions = Vec::new();
    let mut sub_agents = 0usize;
    let mut final_text = String::new();
    let mut error = None;
    let mut status = RunStatus::Completed;

    let deadline = tokio::time::sleep(options.timeout);
    tokio::pin!(deadline);

    while !pending.is_empty() {
        tokio::select! {
            () = &mut deadline => {
                status = RunStatus::TimedOut;
                error = Some(format!(
                    "run exceeded {}s with {} conversation(s) still active",
                    options.timeout.as_secs(),
                    pending.len()
                ));
                break;
            }
            event = event_rx.recv() => {
                let Some(event) = event else { break };
                match event {
                    AppEvent::AgentDone(conversation, result) => {
                        if conversation == root {
                            final_text = result.text.clone();
                        }
                        pending.remove(&conversation);
                    }
                    AppEvent::AgentError(conversation, message) => {
                        if error.is_none() {
                            error = Some(message.clone());
                            status = RunStatus::Failed;
                        }
                        debug_log::log_json(
                            action::MESSAGE,
                            &serde_json::json!({
                                "conversation": conversation.0,
                                "role": "error",
                                "content": message,
                            }),
                        );
                        pending.remove(&conversation);
                    }
                    AppEvent::AddMessage(conversation, message) => {
                        if conversation == root {
                            transcript.push(message.clone());
                        }
                        debug_log::log_json(
                            action::MESSAGE,
                            &serde_json::json!({
                                "conversation": conversation.0,
                                "role": &message.role,
                                "content": message.content,
                                "tool_error": message.tool_error,
                            }),
                        );
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
                    AppEvent::ToolError { conversation: _, error: message } => {
                        if let Some(last) = tools.last_mut() {
                            last.ok = false;
                        }
                        debug_log::log_json(
                            action::TOOL_RESULT,
                            &serde_json::json!({ "ok": false, "error": message }),
                        );
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
                        }));
                    }
                    // Everything else is TUI chrome (chunks, status lines,
                    // server and update events) with no bearing on the run.
                    _ => {}
                }
            }
        }
    }

    for handle in handles {
        handle.abort();
    }
    drop(event_tx);

    let session_id = if options.save_session {
        persist(config, &transcript)
    } else {
        None
    };

    RunOutcome {
        status,
        trace_path: options.trace_path.clone(),
        duration_ms: started.elapsed().as_millis() as u64,
        final_text,
        tools,
        sub_agents,
        questions,
        error,
        session_id,
        semantic_ready: false,
    }
}

/// Approvals and questions are resolved through an async `Notify`; the
/// waiting agent task is parked, so resolving must not block this loop.
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

    fn probe_options() -> ExecOptions {
        ExecOptions {
            prompt: String::new(),
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
        }
    }
}
