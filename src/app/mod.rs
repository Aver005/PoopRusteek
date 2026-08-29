pub mod background_stats;
pub mod conversation;
pub mod events;
pub mod generation;
mod goal;
pub mod input;
mod interaction;
mod keys;
pub mod list;
pub mod mcp_add;
pub mod mcp_status;
mod multichat;
mod persist;
mod pickers;
pub mod providers;
pub mod reduce;
pub(crate) mod runtime;
pub mod search;
mod serve;
mod sessions;
pub mod system_prompt;
pub mod themes;
mod timers;
pub mod view_state;

use crate::commands::CommandRegistry;
use crate::commands::CommandSuggestion;
use crate::config::Config;
use crate::error::AppResult;
use crate::mcp::MCPManager;
use crate::prompts::{self, PromptFiles};
use crate::provider::estimate_tokens;
use crate::provider::{ChatMessage, LLMProvider, Role};
use crate::skills::{SkillDefinition, discovery::discover_all_skills};
use crate::tools::registry::ToolRegistry;
use events::{
    AgentEvent, AppEvent, ConfirmAction, Modal, OnboardingState, PendingInteraction,
    QuestionRequest, ToolApprovalRequest, View,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use tokio::sync::mpsc;

/// PID of the foreground child process (if any), for killing on abort.
pub static FOREGROUND_CHILD_PID: AtomicU32 = AtomicU32::new(0);
static TERMINAL_RESTORE_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn request_terminal_restore() {
    TERMINAL_RESTORE_REQUESTED.store(true, Ordering::SeqCst);
}

fn consume_terminal_restore_request() -> bool {
    TERMINAL_RESTORE_REQUESTED.swap(false, Ordering::SeqCst)
}

pub fn kill_foreground_child() {
    let pid = FOREGROUND_CHILD_PID.swap(0, Ordering::SeqCst);
    if pid != 0 {
        #[cfg(windows)]
        {
            // /T kills the entire process tree (child + grandchild processes).
            let _ = std::process::Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .spawn();
        }
        #[cfg(not(windows))]
        {
            let _ = std::process::Command::new("kill")
                .arg("-9")
                .arg(pid.to_string())
                .spawn();
        }
    }
}

/// Строка состояния для события — или `None`, если сказать нечего.
/// Чистая: разница фокуса и фона лишь в том, зовут её или нет.
fn status_for(event: &AgentEvent) -> Option<String> {
    Some(match event {
        AgentEvent::Started => "Thinking...".to_string(),
        AgentEvent::Done(_) => "Ready".to_string(),
        AgentEvent::Failed(error) => error.clone(),
        AgentEvent::ToolStarted { name } => format!("Running {name}..."),
        AgentEvent::ToolDone { .. } => "Tool finished".to_string(),
        AgentEvent::ToolError { error } => format!("Tool error: {error}"),
        AgentEvent::ToolOutputCleared {
            cleared,
            freed_tokens,
        } => format!(
            "Cleared {} old tool output(s), ~{freed_tokens} tokens freed",
            cleared.len()
        ),
        AgentEvent::SessionReset {
            before_tokens,
            after_tokens,
        } => format!(
            "Started a fresh provider session, re-seeding ~{after_tokens} tokens instead of ~{before_tokens}"
        ),
        AgentEvent::BeginAssistantMessage
        | AgentEvent::Chunk(_)
        | AgentEvent::Message(_)
        | AgentEvent::DiscardEmptyAssistant
        | AgentEvent::ContextUsage(_) => return None,
    })
}

/// Ask the active provider for its context window off the event loop
/// (invariant 1). The answer is dropped when the provider or model changed
/// again while the request was in flight.
fn spawn_window_poll(
    provider: Option<Arc<dyn LLMProvider>>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    epoch_cell: Arc<AtomicU64>,
    epoch: u64,
) {
    let Some(handle) = provider else {
        return;
    };
    tokio::spawn(async move {
        let Some(window) = handle.context_window().await else {
            return;
        };
        if epoch_cell.load(Ordering::SeqCst) == epoch {
            let _ = event_tx.send(AppEvent::ContextWindowLearned(window));
        }
    });
}

/// A window belongs to the provider/model that reported it, so a switch puts it
/// back to unknown — that switches the ladder off, which is the safe direction.
fn invalidate_context_window(state: &mut AppState, epoch_cell: &AtomicU64) -> u64 {
    state.provider_context_window = 0;
    epoch_cell.fetch_add(1, Ordering::SeqCst) + 1
}

/// Point the focused conversation at `provider`, start a fresh session id and
/// re-poll the window. Every provider/model switch goes through here.
fn switch_provider(
    state: &mut AppState,
    provider: Option<Arc<dyn LLMProvider>>,
    epoch_cell: &Arc<AtomicU64>,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
) {
    state.focused_mut().provider = provider;
    state.focused_mut().session_id = crate::session::create_session_id();
    let epoch = invalidate_context_window(state, epoch_cell);
    spawn_window_poll(
        state.focused().provider.clone(),
        event_tx.clone(),
        Arc::clone(epoch_cell),
        epoch,
    );
}

pub struct App {
    pub config: Config,
    pub state: AppState,
    pub event_tx: mpsc::UnboundedSender<AppEvent>,
    event_rx: mpsc::UnboundedReceiver<AppEvent>,
    commands: CommandRegistry,
    pub mcp: Arc<tokio::sync::Mutex<MCPManager>>,
    tools: Arc<ToolRegistry>,
    prompts: PromptFiles,
    skills: Vec<SkillDefinition>,
    /// Local semantic matcher over skills + MCP tools (also backs the
    /// `tool_search` builtin). The app refreshes its MCP corpus whenever
    /// the server set changes.
    semantic: Arc<crate::semantic::SemanticService>,
    /// Launches agent turns (owns the tool registry / MCP / event channel for
    /// execution). The single place the agent loop is spawned.
    runtime: runtime::AgentRuntime,
    /// Serialized off-loop writer for session/history files — see
    /// `app::persist` for why ordering matters.
    persister: persist::Persister,
    /// The running API server (`/serve on`, `--serve`), if any.
    server: Option<crate::server::ServerHandle>,
    /// Monotonic server-launch counter — see `AppEvent::ServerStarted`.
    server_generation: u64,
    /// Fetched `/providers` model lists (persisted, TTL'd) — feeds the API
    /// server catalog and `/serve` status. See `provider::model_cache`.
    provider_models: std::sync::Arc<crate::provider::model_cache::ProviderModelCache>,
    /// When the periodic model refetch last ran (`[provider_models] refetch_ms`).
    last_models_refetch: std::time::Instant,
    /// True while a self-update pass runs (startup auto-check or `/update`) —
    /// two concurrent passes would race on the binary swap.
    update_in_flight: Arc<AtomicBool>,
    /// Bumped on every provider/model switch. A window poll answers with the
    /// epoch it started under, so a slow one can never overwrite a newer.
    provider_window_epoch: Arc<AtomicU64>,
}

/// Run one self-update pass off the event loop and report through
/// `AppEvent::UpdateStatus`. Shared by `/update` (dispatch) and the startup
/// auto-check (`App::new`); `quiet_when_current` keeps the routine startup
/// "already up to date" out of the chat (status line only).
pub(crate) fn spawn_update_task(
    event_tx: mpsc::UnboundedSender<AppEvent>,
    in_flight: Arc<AtomicBool>,
    quiet_when_current: bool,
) {
    if in_flight.swap(true, Ordering::SeqCst) {
        let _ = event_tx.send(AppEvent::UpdateStatus {
            message: "An update is already in progress.".to_string(),
            notable: true,
        });
        return;
    }
    tokio::spawn(async move {
        let (message, notable) = match crate::update::run().await {
            Ok(crate::update::UpdateOutcome::UpToDate) => (
                "Already up to date — the binary matches the `latest` release.".to_string(),
                !quiet_when_current,
            ),
            Ok(crate::update::UpdateOutcome::Updated { new_hash }) => (
                format!(
                    "⬇ Updated to the latest dev build (sha256 {}…) — restart to apply.",
                    crate::util::truncate_at_char_boundary(&new_hash, 12)
                ),
                true,
            ),
            Err(e) => (format!("Update failed: {e}"), true),
        };
        in_flight.store(false, Ordering::SeqCst);
        let _ = event_tx.send(AppEvent::UpdateStatus { message, notable });
    });
}

pub struct AppState {
    /// All open conversations with one focused. The focused conversation's
    /// messages / provider / generation / session live inside it — there is no
    /// separate "live" copy on `App`/`AppState`.
    pub conversations: conversation::Conversations,
    pub input: input::InputState,
    pub status_message: String,
    /// Count of ERROR-level logs since the last message was sent, and the
    /// most recent one's text. Drives the red error marker (panel / status
    /// bar). Reset on submit — sending a message acknowledges them. Full
    /// details always live in `errors.log`.
    pub error_count: usize,
    pub last_error: Option<String>,
    pub scroll_offset: u32,
    /// Высота терминала, обновляется на `Resize`. Клавиши приходят раньше
    /// отрисовки, а шаг `PageUp`/`PageDown` должен совпадать с экраном.
    pub terminal_rows: u16,
    pub modal: Option<Modal>,
    /// Правила авто-одобрения. С областью, а не только с именем
    /// инструмента: см. `crate::whitelist`.
    pub approved_tools: crate::whitelist::Whitelist,
    /// The interaction currently on screen (tool approval / question)…
    pub pending_tool_approval: Option<ToolApprovalRequest>,
    pub pending_question: Option<QuestionRequest>,
    /// …and the ones parked behind it, FIFO. See `present_next_interaction`.
    pub pending_interactions: std::collections::VecDeque<PendingInteraction>,
    pub autocomplete: AutocompleteState,
    pub view: View,
    pub onboarding: OnboardingState,
    pub mcp_status: mcp_status::McpStatus,
    pub providers_view: providers::ProvidersViewState,
    pub search: search::SearchViewState,
    pub themes: themes::ThemesViewState,
    pub workspace_path: String,
    /// Секция системного промпта из `AGENTS.md` рабочей папки и глобальных
    /// правил. Кэш: собирается при старте, на `/cwd` и по `/instructions
    /// reload`, а не на каждый ход (инвариант 1).
    pub instructions_section: String,
    pub show_stats_panel: bool,
    pub attached_files: Vec<crate::provider::AttachedFile>,

    // Goal mode state
    pub goal: goal::GoalState,

    pub needs_terminal_restore: bool,
    pub background: background_stats::BackgroundCounters,
    /// Сколько таймеров взведено — для строки состояния. Обновляется тем же
    /// тиком, что их и будит (`app::timers`).
    pub timers_pending: usize,
    /// Context window the provider reported, 0 while unknown. Config wins over
    /// it — see `context::ContextBudget::learn_provider_window`.
    pub provider_context_window: u32,
}

/// Перечитать правила проекта в кэш состояния и вернуть строку для человека.
/// Одна точка на три вызова (старт, `/cwd`, `/instructions reload`) — раньше
/// блок был скопирован дословно и читал файлы дважды.
pub fn reload_instructions(state: &mut AppState, config: &Config) -> Option<String> {
    if !config.instructions.enabled {
        state.instructions_section.clear();
        return None;
    }
    let loaded = crate::instructions::load(&state.workspace_path, config.instructions.max_bytes);
    state.instructions_section = loaded.section;
    let names: Vec<String> = loaded
        .sources
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    if names.is_empty() {
        return None;
    }
    Some(format!(
        "Project instructions loaded from {}{}",
        names.join(", "),
        if loaded.truncated { " (truncated)" } else { "" }
    ))
}

impl AppState {
    /// Состояние с одним разговором и значениями по умолчанию. Поля, которые
    /// зависят от диска и от наличия провайдера, выставляет вызывающий.
    pub fn new(main: conversation::Conversation) -> Self {
        Self {
            conversations: conversation::Conversations::new(main),
            input: input::InputState::default(),
            status_message: String::new(),
            error_count: 0,
            last_error: None,
            scroll_offset: 0,
            terminal_rows: 0,
            modal: None,
            approved_tools: crate::whitelist::Whitelist::default(),
            pending_tool_approval: None,
            pending_question: None,
            pending_interactions: std::collections::VecDeque::new(),
            autocomplete: AutocompleteState::default(),
            view: View::Chat,
            onboarding: OnboardingState::default(),
            mcp_status: mcp_status::McpStatus::default(),
            providers_view: providers::ProvidersViewState::default(),
            search: search::SearchViewState::default(),
            themes: themes::ThemesViewState::default(),
            workspace_path: String::new(),
            instructions_section: String::new(),
            show_stats_panel: true,
            attached_files: Vec::new(),
            goal: goal::GoalState::default(),
            timers_pending: 0,
            needs_terminal_restore: false,
            background: background_stats::BackgroundCounters::default(),
            provider_context_window: 0,
        }
    }

    /// The focused conversation (whose messages/generation/session are shown).
    pub fn focused(&self) -> &conversation::Conversation {
        self.conversations.focused()
    }

    pub fn focused_mut(&mut self) -> &mut conversation::Conversation {
        self.conversations.focused_mut()
    }

    /// Append a message to the focused conversation. Convenience that avoids
    /// borrow conflicts when the content is derived from `self`.
    pub fn push_message(&mut self, message: ChatMessage) {
        self.focused_mut().messages.push(message);
    }

    /// Append a system message to the focused conversation.
    pub fn push_system(&mut self, content: &str) {
        self.focused_mut()
            .messages
            .push(ChatMessage::system(content));
    }

    /// Сообщение только для человека. Не путать с `push_system`: тот уезжает
    /// модели в каждом последующем запросе, а такие заметки ей не нужны.
    pub fn push_ui_system(&mut self, content: &str) {
        self.focused_mut()
            .messages
            .push(ChatMessage::ui_system(content));
    }

    /// Reset the focused conversation's chat view: wipe its messages and
    /// clear the scroll / autocomplete state. Shared by `/clear`, `/home`,
    /// and `/reset`.
    pub fn clear_chat_view(&mut self) {
        self.focused_mut().messages.clear();
        self.scroll_offset = 0;
        self.autocomplete = AutocompleteState::default();
    }
}

#[derive(Debug, Clone, Default)]
pub struct AutocompleteState {
    pub visible: bool,
    pub items: Vec<CommandSuggestion>,
    pub cursor: list::ListCursor,
    pub file_mode: bool,
}

const AUTOCOMPLETE_VISIBLE: usize = 8;

impl App {
    pub async fn new(config: Config) -> AppResult<Self> {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        // Bridge ERROR-level logs to the in-UI red marker now that a channel
        // exists (see logging::setup). Idempotent across App re-creation.
        crate::logging::set_error_sink(event_tx.clone());

        let provider: Option<Arc<dyn LLMProvider>> = crate::provider::build_provider(&config);

        let mcp = Arc::new(tokio::sync::Mutex::new(MCPManager::new()));
        let tools = Arc::new(ToolRegistry::new());
        let prompts = prompts::load_prompt_files();

        let mut skills = discover_all_skills(&config.skills.paths);
        for skill in &mut skills {
            if config.skills.enabled.contains(&skill.slug)
                || config.skills.enabled.contains(&skill.name)
            {
                skill.enabled = true;
            }
        }
        tools.update_skills(skills.clone());

        let has_provider = provider.is_some();
        let provider_window_epoch = Arc::new(AtomicU64::new(0));
        spawn_window_poll(
            provider.clone(),
            event_tx.clone(),
            Arc::clone(&provider_window_epoch),
            0,
        );
        let main_conversation = conversation::Conversation::fresh_main(provider);

        // Всё остальное — значения по умолчанию из `AppState::new`; здесь
        // только то, что читается с диска и что решает наличие провайдера.
        let mut state = AppState::new(main_conversation);
        state.input.history = crate::session::load_history();
        let whitelist = crate::whitelist::load();
        state.approved_tools = whitelist.whitelist;
        // Перенос старого формата и битый файл — не молча: в первом случае
        // разрешение стало шире, чем человек выбирал, во втором пропало.
        let whitelist_notice = whitelist.notice;
        state.workspace_path = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        // В статус-строку, а не в чат: сообщение в буфере оставило бы его
        // непустым и подменило домашний экран на каждом запуске.
        let instructions_notice = reload_instructions(&mut state, &config);
        if has_provider {
            // Правила проекта и перенос whitelist важнее «Ready»: и то, и
            // другое произошло молча, а знать об этом человеку надо.
            state.status_message = whitelist_notice
                .or(instructions_notice)
                .unwrap_or_else(|| "Ready".to_string());
        } else {
            state.status_message = "No token configured".to_string();
            state.view = View::Onboarding;
        }

        // A setting that changed section was applied from its old place — say
        // so, so the change is never silent (`config::apply_migrations`). Goes
        // to the status line, not the transcript: a message here would leave
        // the chat non-empty and replace the home screen on every launch.
        if !config.migration_notices.is_empty() {
            state.status_message = config.migration_notices.join(" · ");
        }

        // Semantic matcher: background init (first run downloads the
        // embedding model); turns get skill/MCP-tool hints once it's ready.
        // `tool_search` is registered unconditionally — before readiness it
        // degrades to lexical matching over the raw tool list.
        let semantic =
            crate::semantic::SemanticService::start(&config, skills.clone(), event_tx.clone());
        tools.register_semantic_tools(Arc::clone(&semantic));

        // MCP discovery + connect runs in the background so the first frame
        // never waits on a slow or unreachable server (a synchronous
        // `initialize().await` here used to hold a blank terminal for up to
        // ~60s). `startup_initialize` takes the manager lock only for short
        // await-free merges, so event-loop paths that `lock().await` the
        // manager (sending a turn, the /mcp view) stay responsive while
        // servers connect. Counts and the semantic MCP corpus catch up via
        // the 2s stats poll and `McpInitialized`.
        {
            let mcp = Arc::clone(&mcp);
            let semantic = Arc::clone(&semantic);
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                crate::mcp::manager::startup_initialize(&mcp).await;
                let tools = mcp.lock().await.get_all_tools();
                semantic.update_mcp_tools(tools);
                let _ = event_tx.send(AppEvent::McpInitialized);
            });
        }

        let runtime = runtime::AgentRuntime::new(
            Arc::clone(&tools),
            Arc::clone(&mcp),
            Arc::clone(&semantic),
            event_tx.clone(),
        );

        let persister = persist::Persister::start(Arc::clone(&semantic));

        // Provider model lists: load the persisted cache and refresh stale
        // entries in the background (`cache_ms` decides what "stale" means;
        // a fresh cache makes this a no-op network-wise).
        let provider_models = crate::provider::model_cache::ProviderModelCache::load();
        if !config.providers.is_empty() {
            providers::spawn_models_refresh(
                Arc::clone(&provider_models),
                config.providers.clone(),
                config.provider_models.cache_ms,
                false,
                event_tx.clone(),
            );
        }

        // Self-updater: clear the `.old` backup a previous update may have
        // left (Windows can't delete it while that binary's process runs),
        // then — only when opted in via /autoupdate — check the `latest`
        // release in the background. Quiet when already current.
        crate::update::cleanup_stale_backup();
        let update_in_flight = Arc::new(AtomicBool::new(false));
        if config.update.auto {
            spawn_update_task(event_tx.clone(), Arc::clone(&update_in_flight), true);
        }

        Ok(Self {
            config,
            state,
            event_tx,
            event_rx,
            commands: CommandRegistry::new(),
            mcp,
            tools,
            prompts,
            skills,
            semantic,
            runtime,
            persister,
            server: None,
            server_generation: 0,
            provider_models,
            last_models_refetch: std::time::Instant::now(),
            update_in_flight,
            provider_window_epoch,
        })
    }

    pub async fn run(&mut self) -> AppResult<()> {
        let mut terminal = crate::tui::init()?;
        // До первого Resize размер знает только терминал.
        if let Ok((_, rows)) = crossterm::terminal::size() {
            self.state.terminal_rows = rows;
        }
        let result = self.run_loop(&mut terminal).await;
        // Kill any running foreground child before restoring terminal.
        kill_foreground_child();
        crate::tui::restore(&mut terminal)?;
        // Abort every conversation's agent task so none keeps the runtime alive.
        for conv in self.state.conversations.iter_mut() {
            if let Some(handle) = conv.agent_task.take() {
                handle.abort();
            }
        }
        // The API server dies with the app — nobody is left to receive its
        // lifecycle events, so a hard abort beats a graceful shutdown here.
        if let Some(server) = self.server.take() {
            server.abort();
        }
        // Ephemeral conversations' remote sessions die with them. Bounded —
        // exiting must not hang on a dead network.
        let discards: Vec<_> = self
            .state
            .conversations
            .iter()
            .filter(|c| c.is_background_kind())
            .filter_map(|c| c.provider.clone())
            .map(|provider| async move {
                let _ = provider.discard_remote_session().await;
            })
            .collect();
        if !discards.is_empty() {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                futures::future::join_all(discards),
            )
            .await;
        }
        // Queued session/history writes must land before exit — per-turn
        // saves run on the persist worker now, and quitting right after a
        // turn completes must not lose that turn's save. Bounded like the
        // remote discards above: exiting must not hang on a wedged disk.
        self.persister
            .flush(std::time::Duration::from_secs(3))
            .await;
        // Kill all background/PTY processes so spawn_blocking waiters unblock
        // and the tokio runtime can shut down cleanly.
        let _ = crate::tools::background::shutdown_all().await;
        // Terminate MCP server children — dropping them without close() used
        // to orphan one subprocess per stdio server on every exit. Bounded
        // like every other cleanup step: the manager lock can be busy (a
        // startup merge, an admin operation) and a wedged child can stall
        // `close()` — exit must not wait either out. On timeout the children
        // still die with the process on Windows (the kill-on-close Job
        // Object) and via `kill_on_drop` wherever the transport is dropped.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            self.mcp.lock().await.shutdown_all().await;
        })
        .await;
        result
    }

    async fn run_loop(&mut self, terminal: &mut crate::tui::TuiTerminal) -> AppResult<()> {
        use crossterm::event::EventStream;
        use futures::StreamExt;

        let tick_rate = std::time::Duration::from_millis(120);
        let mut tick_interval = tokio::time::interval(tick_rate);
        let mut event_stream = EventStream::new();

        loop {
            let mut dirty = false;
            tokio::select! {
                _ = tick_interval.tick() => {
                    dirty |= self.tick_is_visual();
                    self.handle_event(AppEvent::Tick).await?;
                }
                Some(Ok(event)) = event_stream.next() => {
                    match event {
                        crossterm::event::Event::Key(key)
                            if matches!(
                                key.kind,
                                crossterm::event::KeyEventKind::Press
                                    | crossterm::event::KeyEventKind::Repeat
                            ) =>
                        {
                            dirty = true;
                            if self.handle_event(AppEvent::Key(key)).await? {
                                return Ok(());
                            }
                        }
                        crossterm::event::Event::Mouse(mouse) => {
                            dirty = true;
                            self.handle_event(AppEvent::Mouse(mouse)).await?;
                        }
                        crossterm::event::Event::Paste(text) => {
                            dirty = true;
                            self.handle_event(AppEvent::Paste(text)).await?;
                        }
                        crossterm::event::Event::Resize(_, rows) => {
                            dirty = true;
                            self.handle_event(AppEvent::Resize { rows }).await?;
                        }
                        _ => {}
                    }
                }
                Some(event) = self.event_rx.recv() => {
                    dirty = true;
                    if self.handle_event(event).await? {
                        return Ok(());
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    // Kill any running foreground child before restoring terminal.
                    kill_foreground_child();
                    let _ = self.state.background.shutdown_all().await;
                    return Ok(());
                }
            }

            if self.settle_frame(terminal, dirty).await? {
                return Ok(());
            }
        }
    }

    /// Post-`select!` settle phase: drain whatever queued up while handling
    /// (a streaming burst is one event per token — rendering per token would
    /// let the unbounded queue outgrow render time), refresh MCP/background
    /// stats, honor terminal-restore requests, then render ONCE behind the
    /// dirty flag. Idle ticks with nothing animating skip the draw entirely —
    /// this is what turns the former ~8 renders/sec at rest into zero.
    /// Returns `true` when a drained event asked the app to quit.
    async fn settle_frame(
        &mut self,
        terminal: &mut crate::tui::TuiTerminal,
        mut dirty: bool,
    ) -> AppResult<bool> {
        let mut drained = 0usize;
        while drained < 256 {
            match self.event_rx.try_recv() {
                Ok(event) => {
                    drained += 1;
                    dirty = true;
                    if self.handle_event(event).await? {
                        return Ok(true);
                    }
                }
                Err(_) => break,
            }
        }

        if self.state.view == View::Mcp {
            dirty |= self.state.mcp_status.refresh_view(&self.mcp).await;
        }
        dirty |= self.state.mcp_status.update_stats(&self.mcp).await;
        dirty |= self.state.background.refresh().await;

        if consume_terminal_restore_request() {
            self.state.needs_terminal_restore = true;
        }

        if self.state.needs_terminal_restore {
            self.state.needs_terminal_restore = false;
            let _ = crossterm::terminal::disable_raw_mode();
            let _ = crossterm::execute!(
                terminal.backend_mut(),
                crossterm::terminal::LeaveAlternateScreen,
            );
            let _ = crossterm::terminal::enable_raw_mode();
            let _ = crossterm::execute!(
                terminal.backend_mut(),
                crossterm::terminal::EnterAlternateScreen,
                crossterm::cursor::DisableBlinking,
                crossterm::cursor::Show,
            );
            terminal.clear()?;
            dirty = true;
        }

        if dirty {
            self.render(terminal)?;
        }
        Ok(false)
    }

    /// Does a tick actually change pixels? The spinner and elapsed-time stats
    /// animate while the **focused** conversation is streaming — background
    /// streams repaint via their own chunk events, and every renderer reads
    /// only the focused conversation's `animation_tick`; the landing logo
    /// animates while the chat is empty and no modal covers it. The `Tick`
    /// handler advances `animation_tick` under this exact predicate — keep
    /// them in lockstep, or idle ticks either redraw an unchanged frame or
    /// freeze the spinner.
    fn tick_is_visual(&self) -> bool {
        self.state.view == View::Onboarding
            || self.state.focused().is_streaming()
            || (self.state.focused().messages.is_empty() && self.state.modal.is_none())
    }

    /// Set the status line and mirror the same text into the focused chat as
    /// a UI-only system line — the standard shape for reporting a background
    /// operation's outcome (was copy-pasted across seven event arms).
    pub(crate) fn announce(&mut self, message: impl Into<String>) {
        let message = message.into();
        self.state
            .focused_mut()
            .messages
            .push(ChatMessage::ui_system(&message));
        self.state.status_message = message;
    }

    async fn handle_event(&mut self, event: AppEvent) -> AppResult<bool> {
        match event {
            AppEvent::Key(key) => return self.handle_key(key).await,
            AppEvent::Mouse(mouse) => self.handle_mouse(mouse),
            AppEvent::Resize { rows } => self.state.terminal_rows = rows,
            AppEvent::Paste(text) => self.handle_paste(text),
            AppEvent::CompactFinished {
                conversation,
                messages,
                status,
            } => self.on_compact_finished(conversation, messages, status),
            AppEvent::ContextWindowLearned(window) => {
                self.state.provider_context_window = window;
            }
            AppEvent::Agent {
                conversation,
                event,
            } => {
                self.on_agent_event(conversation, event).await;
            }
            AppEvent::RequestToolApproval(request) => {
                self.on_tool_approval_requested(request).await;
            }
            AppEvent::RequestQuestion(request, state) => {
                self.on_question_requested(request, state);
            }
            AppEvent::Tick => {
                self.on_tick();
                self.fire_due_timers().await?;
            }
            AppEvent::GoalEvaluationDone(outcome) => {
                self.handle_goal_evaluation_done(outcome).await;
            }
            AppEvent::SpawnSubAgent {
                parent,
                label,
                prompt,
            } => {
                self.spawn_sub_agent(parent, label, prompt).await?;
            }
            AppEvent::SessionFetched {
                conversation,
                session_id,
                result,
            } => {
                self.apply_fetched_session(conversation, &session_id, result)
                    .await;
            }
            AppEvent::SessionAvailabilityChecked {
                conversation,
                session,
                remote_id,
                parent_message_id,
                alive,
            } => {
                self.apply_session_availability(
                    conversation,
                    session,
                    remote_id,
                    parent_message_id,
                    alive,
                )
                .await;
            }
            AppEvent::RemoteSessionsListed { result } => {
                if let Some(Modal::DeleteSessions(st)) = self.state.modal.as_mut() {
                    match result {
                        Ok(sessions) => st.merge_remote(sessions),
                        Err(e) => st.remote_status = events::RemoteListStatus::Failed(e),
                    }
                }
            }
            AppEvent::ModelsListed { result, switch_to } => {
                self.handle_models_listed(result, switch_to);
            }
            AppEvent::SessionsDeleted { deleted, failed } => {
                self.on_sessions_deleted(deleted, failed);
            }
            AppEvent::McpOperationDone { message } => {
                self.on_mcp_operation_done(message);
            }
            AppEvent::McpInitialized => {
                self.on_mcp_initialized();
            }
            AppEvent::McpOAuthResult { server, result } => {
                self.on_mcp_oauth_result(server, result);
            }
            AppEvent::SemanticStatus(message) => {
                self.state.status_message = message;
            }
            AppEvent::UndoFinished(outcome) => match outcome {
                Ok(report) => {
                    self.state.push_ui_system(&report);
                    self.state.status_message = "Undone".to_string();
                }
                Err(error) => {
                    self.state.push_ui_system(&error);
                    self.state.status_message = "Undo failed".to_string();
                }
            },
            AppEvent::UpdateStatus { message, notable } => {
                if notable {
                    self.announce(message);
                } else {
                    self.state.status_message = message;
                }
            }
            AppEvent::ErrorLogged { message } => {
                self.state.error_count += 1;
                self.state.last_error = Some(message);
            }
            AppEvent::ServerStarted { generation, addr } => {
                self.on_server_started(generation, addr);
            }
            AppEvent::ServerFailed { generation, error } => {
                self.on_server_failed(generation, error);
            }
            AppEvent::ServerStopped { generation } => {
                self.on_server_stopped(generation);
            }
            AppEvent::ServerRequestLog { .. } => {
                // Proxy-mode-only event (request_log is off for TUI-owned
                // servers); nothing to show here.
            }
            AppEvent::ProviderModelsRefreshed { summary, failed } => {
                self.on_provider_models_refreshed(summary, failed);
            }
            AppEvent::HistorySearchDone { query, matches } => {
                self.state.search.apply_results(&query, matches);
            }
        }
        Ok(false)
    }

    /// Такт анимации и периодическая дозагрузка моделей.
    fn on_tick(&mut self) {
        if self.tick_is_visual() {
            let generation = &mut self.state.focused_mut().generation;
            generation.animation_tick = generation.animation_tick.wrapping_add(1);
        }
        self.maybe_refetch_provider_models();
    }

    /// Единственный вход агентских событий: историю меняет `reduce`, здесь
    /// остаётся хвост — фон и фокус расходятся ровно в нём.
    async fn on_agent_event(&mut self, target: conversation::ConversationId, event: AgentEvent) {
        let focused = self.state.conversations.focused_id() == target;
        let Some(conversation) = self.state.conversations.get_mut(target) else {
            return;
        };
        let background = conversation.is_background_kind();
        let end = reduce::apply(conversation, &event);

        // Фоновый ход не перебивает строку, которую читают для чата на экране.
        if focused
            && !background
            && let Some(message) = status_for(&event)
        {
            self.state.status_message = message;
        }

        match reduce::turn_tail(end, reduce::TurnOwner::classify(focused, background)) {
            reduce::TurnTail::None => {}
            reduce::TurnTail::Background(error) => self.finish_background(target, error),
            reduce::TurnTail::Focused(end) => {
                self.record_gen_stats();
                self.auto_save_session();
                match end {
                    reduce::TurnEnd::Done => self.maybe_advance_goal_cycle().await,
                    // Ошибка посреди цикла оставила бы GOAL в «выполняется»,
                    // когда ничего уже не выполняется.
                    reduce::TurnEnd::Failed(error) if self.state.goal.is_running() => {
                        self.cancel_goal_cycle(&format!(
                            "⚠ Goal cycle stopped after an error: {error}. Use /goal to retry."
                        ));
                    }
                    reduce::TurnEnd::Failed(_) => {}
                }
            }
        }
    }

    /// Cancel the focused conversation's in-flight turn: kill its foreground
    /// child, abort the agent task, deny its pending approvals, and reset the
    /// visible state. Shared by Esc and Ctrl+C.
    pub(crate) async fn cancel_focused_turn(&mut self) {
        // Kill the running foreground child process first.
        kill_foreground_child();
        if let Some(handle) = self.state.focused_mut().agent_task.take() {
            handle.abort();
        }
        let conversation = self.state.conversations.focused_id();
        self.purge_interactions_for(conversation).await;
        let killed = self.state.background.shutdown_all().await;
        self.state.focused_mut().generation.active = false;
        // The aborted task may have been a `/compact`: drop its claim here, or
        // the command stays refused for this chat forever.
        self.state.focused_mut().compacting = None;
        self.state.status_message = if killed > 0 {
            format!("Cancelled; killed {killed} background process(es)")
        } else {
            "Cancelled".to_string()
        };
        self.state.needs_terminal_restore = true;
        self.state.focused_mut().discard_empty_assistant();
        if self.state.goal.is_running() {
            self.cancel_goal_cycle("⏹ Goal cycle cancelled. Use /goal to start a new one.");
        }
    }

    /// Start an agent turn on the focused conversation. `user_message`, when
    /// given, is appended to the chat first and therefore reaches the model —
    /// this replaces an older `send_to_agent(input)` whose argument was
    /// silently discarded. The provider sees every non-`ui_only` message.
    async fn send_focused_turn(&mut self, user_message: Option<ChatMessage>) -> AppResult<()> {
        self.send_turn(self.state.conversations.focused_id(), user_message)
            .await
    }

    /// Тот же ход, но в названной беседе: её может не быть на экране —
    /// так сработавший таймер будит свой чат, а не тот, куда ушёл человек.
    pub(crate) async fn send_turn(
        &mut self,
        conversation: conversation::ConversationId,
        user_message: Option<ChatMessage>,
    ) -> AppResult<()> {
        let Some(target) = self.state.conversations.get(conversation) else {
            return Ok(());
        };
        let provider = match &target.provider {
            Some(p) => Arc::clone(p),
            None => {
                self.state
                    .focused_mut()
                    .messages
                    .push(ChatMessage::assistant(
                        "No provider configured. Set your DeepSeek token in config.",
                    ));
                return Ok(());
            }
        };

        if let Some(message) = user_message
            && let Some(target) = self.state.conversations.get_mut(conversation)
        {
            target.messages.push(message);
        }

        let Some(target) = self.state.conversations.get(conversation) else {
            return Ok(());
        };
        let session_id = target.session_id.clone();
        let messages: Vec<ChatMessage> = target
            .messages
            .iter()
            .filter(|m| !m.ui_only)
            .cloned()
            .collect();
        let system_prompt = system_prompt::build(self.prompt_inputs()).await;

        let spec = runtime::TurnSpec {
            conversation,
            provider,
            messages,
            system_prompt,
            model: self.config.provider.model.clone(),
            temperature: self.config.provider.temperature,
            max_tokens: self.config.provider.max_tokens,
            max_steps: self.config.agent.max_steps_per_turn.max(1),
            max_tools_per_step: self.config.agent.max_tools_per_step.max(1),
            auto_approve: false, // turn with a human behind it: interactive approval
            tool_output_limit: self.config.context.tool_output_limit as usize,
            context: crate::context::ContextSpec::new(
                &self.config.context,
                self.state.provider_context_window,
                &session_id,
            )
            .with_output_cap(self.config.provider.max_tokens),
        };

        let _ = self.event_tx.send(AppEvent::Agent {
            conversation,
            event: AgentEvent::Started,
        });
        let handle = self.runtime.spawn(spec);
        if let Some(target) = self.state.conversations.get_mut(conversation) {
            target.agent_task = Some(handle);
        }

        Ok(())
    }

    /// Откат вне цикла событий: копия может быть в мегабайты, а
    /// `atomic_write` внутри делает `sync_all` (инвариант 1).
    pub(crate) fn spawn_undo(&mut self) {
        let event_tx = self.event_tx.clone();
        self.state.status_message = "Undoing the last file change...".to_string();
        tokio::task::spawn_blocking(move || {
            let outcome = crate::checkpoints::Store::shared().undo_last();
            let _ = event_tx.send(AppEvent::UndoFinished(outcome));
        });
    }

    /// Open the generic confirm modal for `/logout` or `/wipe`.
    pub(crate) fn open_confirm(&mut self, action: ConfirmAction) {
        use events::ConfirmState;
        let cs = match action {
            ConfirmAction::Logout => ConfirmState::logout(),
            ConfirmAction::Wipe => ConfirmState::wipe(),
            ConfirmAction::Update => ConfirmState::update_dev(),
            // Цель отката знает только журнал, поэтому этот экран открывает
            // диспетчер команды, а не общий путь.
            ConfirmAction::Undo => return,
        };
        self.state.modal = Some(Modal::Confirm(cs));
    }

    /// Cancel every conversation's in-flight turn (focused + all background).
    async fn cancel_all_turns(&mut self) {
        kill_foreground_child();
        for conv in self.state.conversations.iter_mut() {
            if let Some(handle) = conv.agent_task.take() {
                handle.abort();
            }
            conv.generation.active = false;
        }
        if let Some(req) = self.state.pending_tool_approval.take() {
            req.resolve(false).await;
        }
        if let Some(req) = self.state.pending_question.take() {
            req.resolve(String::new()).await;
        }
        self.state.pending_interactions.clear();
    }

    /// Execute `/logout`: clear token from config+disk, return to onboarding.
    pub(crate) async fn execute_logout(&mut self) {
        self.cancel_all_turns().await;
        self.config.provider.token = String::new();
        if let Err(e) = crate::config::save(&self.config) {
            tracing::warn!("Logout: failed to save config: {e}");
        }
        self.reset_to_onboarding("Logged out".to_string());
    }

    /// Execute `/wipe`: delete all app-owned data dirs, factory-reset in memory.
    pub(crate) async fn execute_wipe(&mut self) {
        self.cancel_all_turns().await;
        // Drain queued writes first — a still-in-flight session save could
        // otherwise re-create files inside the directories deleted below.
        self.persister
            .flush(std::time::Duration::from_secs(3))
            .await;
        let roots = wipe_roots();
        let mut errors: Vec<String> = Vec::new();
        for root in &roots {
            match std::fs::remove_dir_all(root) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => errors.push(format!("{}: {e}", root.display())),
            }
        }
        self.config = crate::config::Config::default();
        self.state.approved_tools = crate::whitelist::Whitelist::default();
        self.state.input.history.clear();
        if errors.is_empty() {
            self.reset_to_onboarding("All local data wiped".to_string());
        } else {
            // Chat messages are invisible on the onboarding view — surface via status.
            tracing::warn!("Wipe errors: {}", errors.join("; "));
            self.reset_to_onboarding(format!(
                "Wiped with errors — {} path(s) failed, see log",
                errors.len()
            ));
        }
    }

    /// Drop provider, swap to a single fresh empty conversation, land on onboarding.
    /// Входы сборки промпта из текущего состояния. Три места собирали этот
    /// литерал побайтово одинаково.
    fn prompt_inputs(&self) -> system_prompt::PromptInputs<'_> {
        system_prompt::PromptInputs {
            prompts: &self.prompts,
            skills: &self.skills,
            skills_injection: self.config.skills.injection,
            tools: &self.tools,
            mcp: &self.mcp,
            mcp_schema_mode: self.config.effective_mcp_schema_mode(),
            workspace: &self.state.workspace_path,
            project_instructions: &self.state.instructions_section,
        }
    }

    fn reset_to_onboarding(&mut self, status: String) {
        self.state.conversations =
            conversation::Conversations::new(conversation::Conversation::fresh_main(None));
        self.state.onboarding = OnboardingState::default();
        self.state.modal = None;
        self.state.view = View::Onboarding;
        self.state.status_message = status;
        // `/wipe` сносит и папку глобальных правил, так что кэш секции после
        // него описывает уже несуществующие файлы.
        self.state.instructions_section.clear();
        self.state.scroll_offset = 0;
        self.state.input.buffer.clear();
        self.state.input.cursor = 0;
        self.state.input.selection_anchor = None;
        self.state.autocomplete = AutocompleteState::default();
    }

    /// Rebuild the focused conversation's provider from the current config
    /// (used after config edits and `/providers` switches) and start a
    /// fresh session id so the next turn can't thread onto state that
    /// belonged to the previous provider.
    pub(crate) fn rebuild_provider(&mut self) {
        switch_provider(
            &mut self.state,
            crate::provider::build_provider(&self.config),
            &self.provider_window_epoch,
            &self.event_tx,
        );
    }

    /// Drop the learned window without re-polling — for a model switch that
    /// changed the config but left the provider handle on the old model.
    pub(crate) fn invalidate_provider_context_window(&mut self) {
        invalidate_context_window(&mut self.state, &self.provider_window_epoch);
    }

    fn record_gen_stats(&mut self) {
        let conv = self.state.focused_mut();
        let Some(elapsed) = conv.generation.take_elapsed() else {
            return;
        };
        let last_model = conv.generation.last_model.clone();
        let last_status = conv.generation.last_status.clone();
        if let Some(last) = conv.messages.iter_mut().last()
            && last.role == Role::Assistant
            && !last.content.is_empty()
        {
            let tokens = estimate_tokens(&last.content);
            last.total_tokens = Some(tokens);
            last.model = last_model;
            last.status = last_status;
            last.think_elapsed_secs = 0.0;
            last.references_count = 0;
            conv.generation.last_tokens = tokens;
            conv.generation.last_duration_secs = elapsed;
        }
    }

    fn render(&self, terminal: &mut crate::tui::TuiTerminal) -> AppResult<()> {
        crate::tui::render::render(terminal, &self.state, &self.config)
    }

    fn expand_file_mentions(&self, input: &str) -> String {
        let workspace = std::path::Path::new(&self.state.workspace_path);
        let mentions = crate::cli::file_mentions::extract_mentions(input, workspace);

        if mentions.is_empty() {
            return input.to_string();
        }

        let mut result = input.to_string();
        for mention in &mentions {
            let tag = format!(
                "@{}",
                mention
                    .path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("file")
            );
            let replacement = crate::cli::file_mentions::format_mention(mention);
            result = result.replace(&tag, &replacement);
        }

        result
    }
}

/// Returns the deduplicated list of directories that own all app-owned data.
/// On Windows config and data typically live in the same parent; on Linux they
/// may differ (XDG_CONFIG_HOME vs XDG_DATA_HOME).
fn wipe_roots() -> Vec<std::path::PathBuf> {
    let config_dir = crate::config::Config::path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(crate::config::Config::path);
    let mut roots = vec![config_dir, crate::config::Config::data_dir()];
    roots.sort();
    roots.dedup();
    roots
}

#[cfg(test)]
mod wipe_tests {
    use super::wipe_roots;

    #[test]
    fn wipe_roots_no_duplicates() {
        let roots = wipe_roots();
        let mut deduped = roots.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(
            roots.len(),
            deduped.len(),
            "wipe_roots must return deduped paths"
        );
        assert!(!roots.is_empty());
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;
    use crate::provider::{CompletionChunk, CompletionRequest, CompletionResponse};
    use async_trait::async_trait;
    use tokio::time::{Duration, timeout};

    /// Reports a fixed window after an optional delay — enough to model both
    /// "the catalogue answers" and "the answer is slow".
    struct WindowProvider {
        window: Option<u32>,
        delay_ms: u64,
    }

    impl WindowProvider {
        fn new(window: Option<u32>, delay_ms: u64) -> Self {
            Self { window, delay_ms }
        }
    }

    #[async_trait]
    impl LLMProvider for WindowProvider {
        async fn complete(&self, _request: CompletionRequest) -> AppResult<CompletionResponse> {
            unreachable!("the window poll never completes a turn")
        }

        async fn complete_stream(
            &self,
            _request: CompletionRequest,
            _tx: mpsc::UnboundedSender<CompletionChunk>,
        ) -> AppResult<()> {
            unreachable!("the window poll never completes a turn")
        }

        fn model(&self) -> &str {
            "window-test"
        }

        async fn context_window(&self) -> Option<u32> {
            if self.delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            }
            self.window
        }

        fn fork(&self) -> Arc<dyn LLMProvider> {
            Arc::new(Self::new(self.window, self.delay_ms))
        }
    }

    fn test_state() -> AppState {
        AppState {
            provider_context_window: 1_000_000,
            ..AppState::new(conversation::Conversation::fresh_main(None))
        }
    }

    async fn next_window(rx: &mut mpsc::UnboundedReceiver<AppEvent>) -> u32 {
        match timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Some(AppEvent::ContextWindowLearned(window))) => window,
            other => panic!("expected a re-polled window, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn switching_provider_forgets_the_old_window_and_re_polls() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = test_state();
        let epoch = Arc::new(AtomicU64::new(0));

        switch_provider(
            &mut state,
            Some(Arc::new(WindowProvider::new(Some(32_000), 0))),
            &epoch,
            &tx,
        );

        assert_eq!(
            state.provider_context_window, 0,
            "the previous provider's window must go unknown the moment we switch"
        );
        assert_eq!(next_window(&mut rx).await, 32_000);
    }

    #[tokio::test]
    async fn a_provider_that_cannot_say_leaves_the_window_unknown() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = test_state();
        let epoch = Arc::new(AtomicU64::new(0));

        switch_provider(
            &mut state,
            Some(Arc::new(WindowProvider::new(None, 0))),
            &epoch,
            &tx,
        );

        assert_eq!(state.provider_context_window, 0);
        assert!(
            timeout(Duration::from_millis(300), rx.recv())
                .await
                .is_err(),
            "a provider with no catalogue window must not report one"
        );
    }

    #[tokio::test]
    async fn a_slow_answer_from_the_previous_provider_is_dropped() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = test_state();
        let epoch = Arc::new(AtomicU64::new(0));

        switch_provider(
            &mut state,
            Some(Arc::new(WindowProvider::new(Some(1_000_000), 150))),
            &epoch,
            &tx,
        );
        switch_provider(
            &mut state,
            Some(Arc::new(WindowProvider::new(Some(32_000), 0))),
            &epoch,
            &tx,
        );

        assert_eq!(next_window(&mut rx).await, 32_000);
        assert!(
            timeout(Duration::from_millis(500), rx.recv())
                .await
                .is_err(),
            "the provider we switched away from must not overwrite the new window"
        );
    }
}
