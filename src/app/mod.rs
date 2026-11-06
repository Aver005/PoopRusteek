pub mod attachments;
pub mod background_stats;
pub mod conversation;
pub mod dropped_paths;
pub mod events;
mod file_scan;
pub mod gateway_view;
pub mod generation;
mod goal;
pub mod input;
mod interaction;
mod keys;
pub mod landing;
pub mod list;
pub mod mcp_add;
pub mod mcp_status;
mod multichat;
pub mod paste_burst;
mod persist;
mod pickers;
pub mod providers;
mod queue;
pub mod reduce;
pub(crate) mod runtime;
pub mod search;
mod serve;
mod sessions;
pub mod system_prompt;
pub mod themes;
mod timers;
pub mod titles;
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::mpsc;

static TERMINAL_RESTORE_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn request_terminal_restore() {
    TERMINAL_RESTORE_REQUESTED.store(true, Ordering::SeqCst);
}

fn consume_terminal_restore_request() -> bool {
    TERMINAL_RESTORE_REQUESTED.swap(false, Ordering::SeqCst)
}

/// Строка состояния для события — или `None`, если сказать нечего.
/// Чистая: разница фокуса и фона лишь в том, зовут её или нет.
fn status_for(event: &AgentEvent) -> Option<String> {
    Some(match event {
        AgentEvent::Started => "Thinking...".to_string(),
        AgentEvent::Done(_) => "Ready".to_string(),
        AgentEvent::Failed(error) => error.clone(),
        AgentEvent::ToolStarted { name, .. } => format!("Running {name}..."),
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
        | AgentEvent::EndAssistantMessage { .. }
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
    // История остаётся, поэтому новый провайдер узнаёт, откуда она: чужие вызовы в ней он развернёт.
    if let (Some(old), Some(new)) = (&state.focused().provider, &provider)
        && let Some(origin) = old.route_state()
    {
        new.seed_route_state(origin);
    }
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
    /// Шлюз аккаунтов и маршрутов. Выключенный не вмешивается в сборку провайдера.
    gateway: crate::gateway::handle::GatewayHandle,
    /// Пока движок собирается на `spawn_blocking`, `gateway` — ждущая ручка, а здесь её хозяин.
    pending_gateway: Option<Arc<crate::gateway::pending::PendingGateway>>,
}

/// Строка статуса при старте, если конфиг включённого шлюза содержит ошибки.
fn gateway_startup_note(gateway: &crate::gateway::handle::GatewayHandle) -> Option<String> {
    use crate::gateway::types::Severity;
    if !gateway.is_enabled() {
        return None;
    }
    let errors: Vec<_> = gateway
        .snapshot()
        .problems
        .into_iter()
        .filter(|problem| problem.severity == Severity::Error)
        .collect();
    let first = errors.first()?;
    Some(format!(
        "Gateway config has {} error(s), the accounts and routes they touch are off. First: {}: {}",
        errors.len(),
        first.path,
        first.message
    ))
}

/// Шлюз для запуска. Выключенный готов сразу и ничего не читает; включённый собирается
/// на `spawn_blocking` (секреты: хранилище, `cmd:`, файлы), результат приходит `GatewayBuilt`.
/// Пока он не готов, ввод и панели живы, а запрос ждёт готовности.
pub(crate) fn start_gateway(
    config: &Config,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
) -> (
    crate::gateway::handle::GatewayHandle,
    Option<Arc<crate::gateway::pending::PendingGateway>>,
) {
    let settings = crate::gateway::engine::gateway_config(config);
    if !settings.gateway.enabled {
        let deps = gateway_view::engine_deps(config, event_tx);
        return (crate::gateway::engine::build_gateway(settings, deps), None);
    }
    let pending = crate::gateway::pending::PendingGateway::new(settings.clone());
    let config = config.clone();
    let tx = event_tx.clone();
    let persist_tx = event_tx.clone();
    tokio::spawn(async move {
        let built = tokio::task::spawn_blocking(move || {
            let deps = gateway_view::engine_deps(&config, &persist_tx);
            crate::gateway::engine::build_gateway(settings, deps)
        })
        .await
        .map_err(|error| error.to_string());
        let _ = tx.send(AppEvent::GatewayBuilt(events::BuiltGateway(built)));
    });
    let handle: crate::gateway::handle::GatewayHandle =
        Arc::new(crate::gateway::pending::PendingHandle(Arc::clone(&pending)));
    (handle, Some(pending))
}

/// Нужна ли пересборка шлюза после перечитывания конфига.
fn gateway_is_stale(gateway: &crate::gateway::handle::GatewayHandle, config: &Config) -> bool {
    use crate::gateway::handle::{AccountStatus, BuildState};
    let wanted = crate::gateway::engine::gateway_config(config)
        .gateway
        .enabled;
    if wanted != gateway.is_enabled() {
        return true;
    }
    if !wanted {
        return false;
    }
    match gateway.build_state() {
        BuildState::Failed(_) => true,
        BuildState::Starting => false,
        BuildState::Ready => gateway
            .snapshot()
            .accounts
            .iter()
            .any(|a| matches!(a.status, AccountStatus::Unavailable { .. })),
    }
}

/// Передать события шлюза в цикл приложения. Задача кончается вместе со шлюзом.
fn spawn_gateway_events(
    gateway: &crate::gateway::handle::GatewayHandle,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) {
    if !gateway.is_enabled() {
        return;
    }
    let mut events = gateway.subscribe();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    if event_tx.send(AppEvent::Gateway(event)).is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// Run one self-update pass off the event loop and report through
/// `AppEvent::UpdateStatus`. Shared by `/update` (dispatch) and the startup
/// auto-check (`App::new`); `quiet_when_current` keeps the routine startup
/// "already up to date" out of the chat (status line only).
pub(crate) fn spawn_update_task(
    event_tx: mpsc::UnboundedSender<AppEvent>,
    in_flight: Arc<AtomicBool>,
    channel: crate::config::UpdateChannel,
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
        let current = crate::update::CURRENT_VERSION;
        let name = channel.as_str();
        let (message, notable) = match crate::update::run(channel).await {
            Ok(crate::update::UpdateOutcome::UpToDate { channel_build }) => (
                format!(
                    "Already up to date — running {current}, {name} channel has {channel_build}."
                ),
                !quiet_when_current,
            ),
            Ok(crate::update::UpdateOutcome::Updated { build }) => (
                format!("⬇ Updated {current} → {build} ({name} channel) — restart to apply."),
                true,
            ),
            Ok(crate::update::UpdateOutcome::PendingRestart { build }) => (
                format!("{build} is already installed — restart to apply."),
                true,
            ),
            Ok(crate::update::UpdateOutcome::NoRelease) => (
                format!("The {name} channel has no release yet — nothing to install."),
                !quiet_when_current,
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
    /// Высота области переписки в прошлом кадре: от неё считается, на сколько
    /// сдвинуть прокрутку, когда панель работы или поле ввода меняют высоту.
    pub last_transcript_rows: u16,
    /// Высота терминала, обновляется на `Resize`. Клавиши приходят раньше
    /// отрисовки, а шаг `PageUp`/`PageDown` должен совпадать с экраном.
    pub terminal_rows: u16,
    /// Ширина терминала, обновляется на `Resize`: окно подтверждения по ней
    /// считает перенос строк, а клавиши прокрутки обязаны считать так же.
    pub terminal_cols: u16,
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
    /// Сканы папок для `@путь`: живёт отдельно, чтобы сброс меню не терял идущий скан.
    pub(crate) file_scans: file_scan::ScanTracker,
    /// Недавние сессии для главного экрана; читает фоновая задача.
    pub landing: landing::LandingSessions,
    pub view: View,
    pub onboarding: OnboardingState,
    pub mcp_status: mcp_status::McpStatus,
    pub gateway_view: gateway_view::GatewayView,
    pub search: search::SearchViewState,
    pub themes: themes::ThemesViewState,
    pub workspace_path: String,
    /// Секция системного промпта из `AGENTS.md` рабочей папки и глобальных
    /// правил. Кэш: собирается при старте, на `/cwd` и по `/instructions
    /// reload`, а не на каждый ход (инвариант 1).
    pub instructions_section: String,
    /// Боковая панель: `None` — по ширине окна (с `SIDEBAR_AUTO_COLS` колонок
    /// включена), `Some` — решение человека (`Ctrl+P`).
    pub show_stats_panel: Option<bool>,
    pub attached_files: Vec<crate::provider::AttachedFile>,

    // Goal mode state
    pub goal: goal::GoalState,

    pub needs_terminal_restore: bool,
    /// Перерисовать всё без выхода из альтернативного экрана (`Ctrl+L`).
    pub needs_full_redraw: bool,
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
    let loaded = crate::instructions::load(
        &state.workspace_path,
        config.instructions.max_bytes,
        crate::instructions::Scope::UserAndAncestors,
    );
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
    /// Показывается ли боковая панель в окне шириной `cols`.
    pub fn stats_panel_visible(&self, cols: u16) -> bool {
        self.show_stats_panel
            .unwrap_or(cols >= crate::tui::render::SIDEBAR_AUTO_COLS)
    }

    /// Почему боковая панель сейчас не видна, хотя её включили: на приветствии
    /// её нет, а узкому окну не хватает места. `None` — ничто не мешает.
    pub fn stats_panel_blocker(&self) -> Option<String> {
        if self.focused().messages.is_empty() && !self.focused().generation.active {
            return Some("The side panel appears once the chat has messages".to_string());
        }
        let need = crate::tui::render::SIDEBAR_MIN_COLS;
        (self.terminal_cols < need)
            .then(|| format!("The side panel needs a window at least {need} columns wide"))
    }

    /// Рабочая папка путём: от неё считаются `@file`, `/attach` и автодополнение.
    pub fn workspace(&self) -> std::path::PathBuf {
        std::path::PathBuf::from(&self.workspace_path)
    }

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
            last_transcript_rows: 0,
            terminal_rows: 0,
            terminal_cols: 0,
            modal: None,
            approved_tools: crate::whitelist::Whitelist::default(),
            pending_tool_approval: None,
            pending_question: None,
            pending_interactions: std::collections::VecDeque::new(),
            autocomplete: AutocompleteState::default(),
            file_scans: file_scan::ScanTracker::default(),
            landing: landing::LandingSessions::default(),
            view: View::Chat,
            onboarding: OnboardingState::default(),
            mcp_status: mcp_status::McpStatus::default(),
            gateway_view: gateway_view::GatewayView::default(),
            search: search::SearchViewState::default(),
            themes: themes::ThemesViewState::default(),
            workspace_path: String::new(),
            instructions_section: String::new(),
            show_stats_panel: None,
            attached_files: Vec::new(),
            goal: goal::GoalState::default(),
            timers_pending: 0,
            needs_terminal_restore: false,
            needs_full_redraw: false,
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
        // Очередь относится к стёртой истории; её текст не пропадает молча.
        self.restore_queued_to_input();
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
        // Сохранение конфига ждёт файловый замок и пишет на диск: оно идёт в фоне, а
        // ошибка приходит событием. Порядок правок очередь сохраняет.
        let save_tx = event_tx.clone();
        crate::config::enable_background_saves(move |message| {
            let _ = save_tx.send(AppEvent::UpdateStatus {
                message,
                notable: true,
            });
        });

        let (gateway, pending_gateway) = start_gateway(&config, &event_tx);
        spawn_gateway_events(&gateway, event_tx.clone());
        let provider: Option<Arc<dyn LLMProvider>> =
            crate::gateway::handle::main_provider(&config, Some(&gateway));

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
        } else if let Some(problem) = config.startup_problem() {
            // Файл или хранилище не прочитались: это не «первый запуск». Онбординг поверх
            // заставил бы вводить токен заново и затёр бы то, что лежит на диске.
            state.status_message = problem.clone();
            state.push_ui_system(&problem);
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

        // Ошибки конфига шлюза: приложение запускается, а отключённые аккаунты названы.
        if let Some(note) = gateway_startup_note(&gateway) {
            state.status_message = note;
        } else if pending_gateway.is_some() {
            state.status_message = "Starting the gateway…".to_string();
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
        provider_models.set_gateway(gateway.clone());
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
        // then — only when opted in via /autoupdate — check the configured
        // channel in the background. Quiet when already current.
        crate::update::cleanup_stale_backup();
        let update_in_flight = Arc::new(AtomicBool::new(false));
        if config.update.auto {
            spawn_update_task(
                event_tx.clone(),
                Arc::clone(&update_in_flight),
                config.update.channel,
                true,
            );
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
            gateway,
            pending_gateway,
        })
        .map(|mut app| {
            // Проектный слой ждёт решения: спрашиваем сразу, не пряча молчаливый отказ в заметке.
            app.refresh_gateway_view();
            app.open_trust_dialog_if_needed();
            app
        })
    }

    pub async fn run(&mut self) -> AppResult<()> {
        let mut terminal = crate::tui::init()?;
        // До первого Resize размер знает только терминал.
        if let Ok((cols, rows)) = crossterm::terminal::size() {
            self.state.terminal_rows = rows;
            self.state.terminal_cols = cols;
        }
        let result = self.run_loop(&mut terminal).await;
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
        // Сохранения конфига дойдут до диска: на занятом диске запись занимает секунды.
        // Пустая очередь не ждёт, а срыв срока видно в журнале, а не потеря молчком.
        let flushed = tokio::task::spawn_blocking(|| {
            crate::config::flush_saves(std::time::Duration::from_secs(30))
        })
        .await
        .unwrap_or(false);
        if !flushed {
            tracing::error!("config saves did not reach the disk before exit");
        }
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
        let mut burst = paste_burst::Burst::default();
        // Один на весь цикл: созданный заново на каждой итерации, он терял
        // сигнал, пришедший между итерациями.
        let shutdown = crate::util::shutdown_signal();
        tokio::pin!(shutdown);

        loop {
            let mut dirty = false;
            tokio::select! {
                _ = tick_interval.tick() => {
                    dirty |= self.tick_is_visual();
                    self.handle_event(AppEvent::Tick).await?;
                }
                Some(Ok(event)) = event_stream.next() => {
                    for event in gather_paste_burst(event, &mut event_stream, &mut burst).await {
                        let (visual, quit) = self.handle_terminal_event(event).await?;
                        dirty |= visual;
                        if quit {
                            return Ok(());
                        }
                    }
                }
                Some(event) = self.event_rx.recv() => {
                    dirty = true;
                    if self.handle_event(event).await? {
                        return Ok(());
                    }
                }
                () = &mut shutdown => {
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

        // Единственный выход очереди: что бы ни освободило чат — ход,
        // `/compact`, отмена, — следующее сообщение уходит отсюда.
        dirty |= self.drain_idle_queue().await;

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

        self.keep_scroll_when_layout_changes();

        if std::mem::take(&mut self.state.needs_full_redraw) {
            // Только сброс буфера: выхода из альтернативного экрана не нужно.
            terminal.clear()?;
            dirty = true;
        }

        if dirty {
            self.render(terminal)?;
        }
        Ok(false)
    }

    /// Человек читает историю с прокруткой вверх, а панель работы выросла или
    /// ввод стал выше: область переписки сузилась, и без поправки текст уехал
    /// бы на эти строки. Верхний край остаётся на месте.
    fn keep_scroll_when_layout_changes(&mut self) {
        let Some(rows) = crate::tui::render::transcript_rows(&self.state) else {
            return;
        };
        self.state.scroll_offset = crate::tui::render::scroll_after_height_change(
            self.state.scroll_offset,
            self.state.last_transcript_rows,
            rows,
        );
        self.state.last_transcript_rows = rows;
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
            AppEvent::Resize { cols, rows } => {
                self.state.terminal_rows = rows;
                self.state.terminal_cols = cols;
            }
            AppEvent::Paste(text) => self.handle_paste(text),
            AppEvent::OnboardingActionDone { action, ok } => {
                self.state.onboarding.report(action, ok);
            }
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
                if self.is_fenced(conversation) {
                    crate::debug_log::log("turn.fenced_event", format!("{event:?}"));
                } else {
                    self.on_agent_event(conversation, event).await;
                }
            }
            AppEvent::TurnFenced(conversation) => {
                if let Some(conv) = self.state.conversations.get_mut(conversation) {
                    conv.fenced = false;
                }
            }
            AppEvent::RequestToolApproval(request) => {
                // Запрос оборванного хода: его некому ждать.
                if self.is_fenced(request.conversation) {
                    request.resolve(false).await;
                } else {
                    self.on_tool_approval_requested(request).await;
                }
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
                liveness,
            } => {
                self.apply_session_availability(
                    conversation,
                    session,
                    remote_id,
                    parent_message_id,
                    liveness,
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
            AppEvent::Gateway(event) => self.on_gateway_event(event),
            AppEvent::GatewayBuilt(built) => self.on_gateway_built(built),
            AppEvent::ChatTitle {
                conversation,
                title,
            } => self.on_chat_title(conversation, title),
            AppEvent::GatewayPersist(next) => self.on_gateway_persist(*next),
            AppEvent::AccountStored(plan) => self.on_account_stored(*plan),
            AppEvent::ConfigReloaded(loaded) => self.on_config_reloaded(*loaded),
            AppEvent::ProjectConfigStamp(stamp) => self.on_project_stamp(stamp),
            AppEvent::HistorySearchDone { query, matches } => {
                self.state.search.apply_results(&query, matches);
            }
            AppEvent::LandingSessionsLoaded(sessions) => self.state.landing.apply(sessions),
            AppEvent::AutocompleteReady { generation, items } => {
                self.on_autocomplete_ready(generation, items);
            }
        }
        Ok(false)
    }

    /// Событие шлюза. Экран аккаунтов читает снимок сам; здесь только строка статуса.
    /// Движок собран: ждущие запросы продолжаются, дальше все берут провайдеров прямо у него.
    fn on_gateway_built(&mut self, built: events::BuiltGateway) {
        let Some(pending) = self.pending_gateway.take() else {
            return;
        };
        match built.0 {
            Ok(gateway) => {
                pending.fulfil(Arc::clone(&gateway));
                self.gateway = gateway;
                self.provider_models.set_gateway(self.gateway.clone());
                self.sync_server_gateway();
                // Адаптеры собраны: каталог моделей аккаунтов можно наполнять.
                providers::spawn_models_refresh(
                    Arc::clone(&self.provider_models),
                    self.config.providers.clone(),
                    self.config.provider_models.cache_ms,
                    false,
                    self.event_tx.clone(),
                );
                self.state.status_message =
                    gateway_startup_note(&self.gateway).unwrap_or_else(|| "Gateway ready".into());
                self.refresh_gateway_view();
                self.adopt_provider_after_edit();
            }
            Err(reason) => {
                pending.fail(reason.clone());
                let message = format!("The gateway failed to start: {reason}");
                self.state.push_ui_system(&message);
                self.state.status_message = message;
            }
        }
    }

    fn on_gateway_event(&mut self, event: crate::gateway::handle::GatewayEvent) {
        use crate::gateway::handle::GatewayEvent;
        if let GatewayEvent::FallbackTaken {
            from, to, class, ..
        } = &event
        {
            self.state.status_message =
                format!("{from} failed ({}), switched to {to}", class.code());
        }
        self.on_gateway_event_view(&event);
    }

    /// Такт анимации и периодическая дозагрузка моделей.
    fn on_tick(&mut self) {
        if self.tick_is_visual() {
            let generation = &mut self.state.focused_mut().generation;
            generation.animation_tick = generation.animation_tick.wrapping_add(1);
        }
        self.maybe_refetch_provider_models();
        self.tick_gateway_view();
        // Главный экран показывает сессии: держим список свежим, не трогая диск в кадре.
        if self.state.view == View::Chat && self.state.focused().messages.is_empty() {
            self.state.landing.refresh_if_stale(&self.event_tx);
        }
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

        let goal_end = end
            .clone()
            .filter(|_| self.state.goal.conversation == Some(target));
        let finished = matches!(end, Some(reduce::TurnEnd::Done));
        match reduce::turn_tail(end, reduce::TurnOwner::classify(focused, background)) {
            reduce::TurnTail::None => {}
            reduce::TurnTail::Background(error) => self.finish_background(target, error),
            reduce::TurnTail::Focused(_) => {
                self.record_gen_stats();
                self.auto_save_session();
            }
        }
        // Ответ пришёл: заголовок просит служебная модель, в фоне и один раз на чат.
        if finished {
            self.maybe_request_chat_title(target);
        }
        // GOAL идёт по своему чату, а не по фокусу: смена чата его не клинит.
        match goal_end {
            Some(reduce::TurnEnd::Done) => self.maybe_advance_goal_cycle().await,
            // Ошибка посреди цикла оставила бы GOAL в «выполняется»,
            // когда ничего уже не выполняется.
            Some(reduce::TurnEnd::Failed(error)) if self.state.goal.is_running() => {
                self.cancel_goal_cycle(&format!(
                    "⚠ Goal cycle stopped after an error: {error}. Use /goal to retry."
                ));
            }
            _ => {}
        }
    }

    /// Оборвать задачу хода чата и выставить забор: события, что она успела
    /// отправить, выбрасываются, пока не придёт `TurnFenced` после её конца.
    pub(crate) fn abort_turn_of(&mut self, conversation: conversation::ConversationId) {
        let Some(conv) = self.state.conversations.get_mut(conversation) else {
            return;
        };
        let Some(handle) = conv.agent_task.take() else {
            return;
        };
        handle.abort();
        conv.fenced = true;
        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            // Ждём саму задачу: её отправки встают в очередь раньше забора.
            let _ = handle.await;
            let _ = event_tx.send(AppEvent::TurnFenced(conversation));
        });
    }

    fn is_fenced(&self, conversation: conversation::ConversationId) -> bool {
        self.state
            .conversations
            .get(conversation)
            .is_some_and(|conv| conv.fenced)
    }

    /// Cancel the focused conversation's in-flight turn: kill its foreground
    /// child, abort the agent task, deny its pending approvals, and reset the
    /// visible state. Shared by Esc and Ctrl+C.
    pub(crate) async fn cancel_focused_turn(&mut self) {
        // Падение задачи хода гасит и дерево его команды (`ProcessTree`).
        let conversation = self.state.conversations.focused_id();
        self.abort_turn_of(conversation);
        self.purge_interactions_for(conversation).await;
        // Только временные процессы этого чата: постоянные серверы и чужие
        // чаты Esc больше не гасит.
        let killed = self.state.background.cleanup_turn_of(conversation).await;
        self.state.focused_mut().generation.active = false;
        self.state.focused_mut().current_tool = None;
        let queued = self.state.restore_queued_to_input();
        // The aborted task may have been a `/compact`: drop its claim here, or
        // the command stays refused for this chat forever.
        self.state.focused_mut().compacting = None;
        self.state.status_message = match (killed, queued) {
            (0, 0) => "Cancelled".to_string(),
            (0, n) => format!("Cancelled; {n} queued message(s) returned to the input"),
            (k, 0) => format!("Cancelled; killed {k} background process(es)"),
            (k, n) => format!(
                "Cancelled; killed {k} background process(es), {n} queued message(s) returned to the input"
            ),
        };
        self.state.needs_terminal_restore = true;
        {
            let focused = self.state.focused_mut();
            focused.discard_empty_assistant();
            // Esc на модалке подтверждения обрывает ход ровно между вызовом
            // и его результатом: без этого история осталась бы с вызовом,
            // на который никто не ответил.
            focused.settle_unanswered_tool_calls();
            // Заметки — после результатов: между вызовом и его ответом строгие
            // эндпоинты (OpenAI, Anthropic) не принимают ничего.
            focused.flush_deferred();
        }
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
        let system_prompt = system_prompt::build(self.prompt_inputs(provider.native_tools())).await;

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
            unattended: false,
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
        let ids: Vec<_> = self.state.conversations.iter().map(|c| c.id).collect();
        for id in ids {
            self.abort_turn_of(id);
        }
        for conv in self.state.conversations.iter_mut() {
            conv.generation.active = false;
            conv.flush_deferred();
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
        if let Some(reason) = crate::config::wipe_blocked_reason(crate::config::is_isolated()) {
            self.state.status_message = reason.to_string();
            self.state.push_ui_system(reason);
            return;
        }
        self.cancel_all_turns().await;
        // Drain queued writes first — a still-in-flight session save could
        // otherwise re-create files inside the directories deleted below.
        self.persister
            .flush(std::time::Duration::from_secs(3))
            .await;
        // Поставленное в очередь сохранение не должно воскресить файл после удаления.
        let _ = tokio::task::spawn_blocking(|| {
            crate::config::flush_saves(std::time::Duration::from_secs(5))
        })
        .await;
        let roots = wipe_roots();
        let mut errors: Vec<String> = Vec::new();
        // Ключи в системном хранилище, токены MCP и файлы конфига: папки их не содержат.
        let config_snapshot = self.config.clone();
        let leftovers = tokio::task::spawn_blocking(move || {
            let servers: Vec<String> = crate::mcp::config::load_mcp_config().into_keys().collect();
            crate::config::wipe_secrets_and_files(&config_snapshot, &servers)
        })
        .await
        .unwrap_or_default();
        errors.extend(leftovers);
        for root in &roots {
            match std::fs::remove_dir_all(root) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => errors.push(format!("{}: {e}", root.display())),
            }
        }
        self.config = crate::config::Config::default();
        // Живой шлюз держит аккаунты и разрешённые секреты из стёртого конфига: пересобираем.
        self.gateway = crate::gateway::engine::build_gateway(
            crate::gateway::engine::gateway_config(&self.config),
            gateway_view::engine_deps(&self.config, &self.event_tx),
        );
        // Сборка, которая шла в этот момент, больше не нужна: конфиг стёрт.
        self.pending_gateway = None;
        self.provider_models.set_gateway(self.gateway.clone());
        self.sync_server_gateway();
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
    fn prompt_inputs(&self, native_tools: bool) -> system_prompt::PromptInputs<'_> {
        system_prompt::PromptInputs {
            prompts: &self.prompts,
            skills: &self.skills,
            skills_injection: self.config.skills.injection,
            tools: &self.tools,
            mcp: &self.mcp,
            mcp_schema_mode: self.config.effective_mcp_schema_mode(),
            workspace: &self.state.workspace_path,
            project_instructions: &self.state.instructions_section,
            native_tools,
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
    /// Шлюз собирается заново, когда конфиг перечитан, а прежняя сборка упала, включена или
    /// выключена правкой, либо оставила аккаунты недоступными (секрет, бинарь): правку видно сразу.
    fn rebuild_gateway_if_stale(&mut self) {
        if !gateway_is_stale(&self.gateway, &self.config) {
            return;
        }
        let (gateway, pending) = start_gateway(&self.config, &self.event_tx);
        self.gateway = gateway;
        self.pending_gateway = pending;
        self.provider_models.set_gateway(self.gateway.clone());
        self.sync_server_gateway();
    }

    pub(crate) fn rebuild_provider(&mut self) {
        self.rebuild_gateway_if_stale();
        switch_provider(
            &mut self.state,
            crate::gateway::handle::main_provider(&self.config, Some(&self.gateway)),
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
        // Мимо заметок в хвосте (провал хода кладёт свою последней), но не
        // дальше: ответ прошлого хода чужой.
        if let Some(last) = conv
            .messages
            .iter_mut()
            .rev()
            .find(|message| message.role != Role::System)
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

    /// Одно событие терминала → `AppEvent`. Возвращает (перерисовать ли, выйти ли).
    async fn handle_terminal_event(
        &mut self,
        event: crossterm::event::Event,
    ) -> AppResult<(bool, bool)> {
        use crossterm::event::{Event, KeyEventKind};
        let app_event = match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                AppEvent::Key(key)
            }
            Event::Mouse(mouse) => AppEvent::Mouse(mouse),
            Event::Paste(text) => AppEvent::Paste(text),
            Event::Resize(cols, rows) => AppEvent::Resize { cols, rows },
            _ => return Ok((false, false)),
        };
        let quit = self.handle_event(app_event).await?;
        Ok((true, quit))
    }

    fn render(&self, terminal: &mut crate::tui::TuiTerminal) -> AppResult<()> {
        crate::tui::render::render(terminal, &self.state, &self.config)
    }

    fn expand_file_mentions(&self, input: &str) -> String {
        crate::cli::file_mentions::expand(input, &self.state.workspace())
    }
}

/// На Windows вставка и перетащенный файл приходят потоком нажатий
/// ([`paste_burst`]): серию, лежащую в очереди разом, склеиваем во вставку.
async fn gather_paste_burst(
    first: crossterm::event::Event,
    stream: &mut crossterm::event::EventStream,
    burst: &mut paste_burst::Burst,
) -> Vec<crossterm::event::Event> {
    use futures::StreamExt;
    if !cfg!(windows) || !paste_burst::is_text_press(&first) {
        return vec![first];
    }
    let started = std::time::Instant::now();
    let mut last_text = started;
    let mut batch = vec![first];
    // Окно продлевают только текстовые нажатия: мышью его не растянуть.
    while started.elapsed() < paste_burst::MAX_GATHER {
        let wait = paste_burst::GAP.saturating_sub(last_text.elapsed());
        match tokio::time::timeout(wait, stream.next()).await {
            Ok(Some(Ok(event))) => {
                if paste_burst::is_text_press(&event) {
                    last_text = std::time::Instant::now();
                }
                batch.push(event);
            }
            _ => break,
        }
    }
    burst.coalesce(batch, started, std::time::Instant::now())
}

/// Папки, чьё содержимое принадлежит приложению: данные и старые места конфига и данных.
/// `~/.pooprusteek` сюда не входит — там лежат правила пользователя (`AGENTS.md`);
/// файлы конфига удаляет `config::wipe_secrets_and_files`.
fn wipe_roots() -> Vec<std::path::PathBuf> {
    let mut roots = vec![crate::config::Config::data_dir()];
    roots.extend(crate::config::legacy_data_dir());
    roots.extend(
        crate::config::legacy_config_file()
            .and_then(|path| path.parent().map(std::path::Path::to_path_buf)),
    );
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

    fn history_with_calls() -> Vec<ChatMessage> {
        let mut assistant = ChatMessage::assistant("looking");
        assistant.tool_calls = vec![crate::provider::ToolCall {
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

    /// Переключение провайдера оставляет историю, поэтому новый узнаёт, откуда она.
    #[tokio::test]
    async fn switching_provider_hands_the_route_origin_to_the_new_one() {
        use crate::gateway::testkit::{FakeBackend, TWO_ACCOUNTS, ask, ask_with, rig};
        let rig = rig(
            TWO_ACCOUNTS,
            vec![
                ("a", FakeBackend::new(vec![])),
                ("b", FakeBackend::new(vec![])),
            ],
        );
        let (tx, _rx) = mpsc::unbounded_channel();
        let epoch = Arc::new(AtomicU64::new(0));
        let mut state = test_state();

        // Беседа шла на a, потом a остыл: новый провайдер уйдёт на b и не должен получить подписи a.
        let old = rig.main();
        ask(&old, "hi").await.0.unwrap();
        state.focused_mut().provider = Some(old);
        rig.cool("a", 300);
        let fresh = rig.main();
        switch_provider(&mut state, Some(Arc::clone(&fresh)), &epoch, &tx);
        ask_with(&fresh, history_with_calls()).await.0.unwrap();
        let seen = rig.fakes["b"].last_messages();
        assert!(seen[1].tool_calls.is_empty(), "b never sees a's ids");
        assert!(seen[1].content.contains("<tool_use>"));

        // Обратная сторона: цель не сменилась — вызовы остаются как были.
        let rig = rig_pair();
        let old = rig.main();
        ask(&old, "hi").await.0.unwrap();
        state.focused_mut().provider = Some(old);
        let same = rig.main();
        switch_provider(&mut state, Some(Arc::clone(&same)), &epoch, &tx);
        ask_with(&same, history_with_calls()).await.0.unwrap();
        assert_eq!(rig.fakes["a"].last_messages()[1].tool_calls.len(), 1);
    }

    fn rig_pair() -> crate::gateway::testkit::Rig {
        use crate::gateway::testkit::{FakeBackend, TWO_ACCOUNTS, rig};
        rig(
            TWO_ACCOUNTS,
            vec![
                ("a", FakeBackend::new(vec![])),
                ("b", FakeBackend::new(vec![])),
            ],
        )
    }

    #[test]
    fn a_gateway_with_config_errors_is_named_at_startup_and_a_healthy_one_is_silent() {
        use crate::gateway::handle::GatewayHandle;
        use crate::gateway::testkit::TWO_ACCOUNTS;
        let bad = TWO_ACCOUNTS.replace(
            "model = \"model-a\"",
            "model = \"model-a\"\n[accounts.limits]\nmax_concurrency = 0",
        );
        let broken: GatewayHandle = Arc::new(crate::gateway::testkit::rig(&bad, Vec::new()).engine);
        let note = gateway_startup_note(&broken).expect("the errors are named");
        assert!(
            note.contains("1 error") && note.contains("max_concurrency"),
            "{note}"
        );
        let healthy: GatewayHandle = Arc::new(rig_pair().engine);
        assert_eq!(gateway_startup_note(&healthy), None);
        let off: GatewayHandle = Arc::new(crate::gateway::handle::DisabledGateway::new());
        assert_eq!(gateway_startup_note(&off), None);
    }
}

#[cfg(test)]
mod gateway_start_tests {
    use super::*;

    /// Секрет читается медленно (`cmd:` ждёт), а запуск не ждёт: ручка отдаётся сразу, ввод и
    /// панели живы, движок приходит событием.
    #[tokio::test]
    async fn a_slow_secret_never_stalls_startup_and_the_engine_arrives_as_an_event() {
        let slow = if cfg!(windows) {
            r#"cmd:["cmd","/C","ping -n 3 127.0.0.1 > nul && echo slow-key"]"#
        } else {
            r#"cmd:["sh","-c","sleep 2; echo slow-key"]"#
        };
        let config = crate::config::parse(&format!(
            "[gateway]\nenabled = true\n[[accounts]]\nid = \"a\"\nprovider = \"custom\"\nbase_url = \"http://127.0.0.1:1/v1\"\nsecret = '{slow}'\nmodel = \"m\"\n[[routes]]\nid = \"main\"\nsteps = [{{ account = \"a\" }}]\n[roles]\nmain = \"main\"\n"
        ))
        .unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let started = std::time::Instant::now();
        let (handle, pending) = start_gateway(&config, &tx);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(1500),
            "startup must not wait for the secret, took {:?}",
            started.elapsed()
        );
        assert!(pending.is_some() && handle.is_enabled());
        assert!(handle.snapshot().problems[0].message.contains("starting"));

        let event = tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
            .await
            .expect("the engine must arrive")
            .expect("channel open");
        let AppEvent::GatewayBuilt(built) = event else {
            panic!("expected GatewayBuilt, got {event:?}");
        };
        let engine = built.0.expect("built");
        assert_eq!(
            engine.snapshot().accounts.len(),
            1,
            "the secret was resolved off the loop"
        );
    }

    /// Упавшая сборка, правка включённости и недоступный аккаунт требуют пересборки; здоровый — нет.
    #[tokio::test]
    async fn a_reloaded_config_rebuilds_a_broken_gateway_and_leaves_a_healthy_one() {
        let text = |enabled: bool, secret: &str| {
            format!(
                "[gateway]
enabled = {enabled}
[[accounts]]
id = \"a\"
provider = \"custom\"
base_url = \"http://127.0.0.1:1/v1\"
secret = \"{secret}\"
model = \"m\"
"
            )
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let good = crate::config::parse(&text(true, "none")).unwrap();
        let off = crate::config::parse(&text(false, "none")).unwrap();
        let missing =
            crate::config::parse(&text(true, "env:POOPRUSTEEK_STALE_TEST_MISSING")).unwrap();

        let (disabled, _) = start_gateway(&off, &tx);
        assert!(!gateway_is_stale(&disabled, &off), "off and wanted off");
        assert!(
            gateway_is_stale(&disabled, &good),
            "the file now enables it"
        );

        let engine = crate::gateway::engine::build_gateway(
            crate::gateway::engine::gateway_config(&good),
            crate::gateway::engine::EngineDeps {
                data_dir: None,
                ..crate::gateway::engine::EngineDeps::for_config(&good)
            },
        );
        assert!(
            !gateway_is_stale(&engine, &good),
            "a healthy gateway is left alone"
        );
        assert!(gateway_is_stale(&engine, &off), "the file now disables it");

        let broken = crate::gateway::engine::build_gateway(
            crate::gateway::engine::gateway_config(&missing),
            crate::gateway::engine::EngineDeps {
                data_dir: None,
                ..crate::gateway::engine::EngineDeps::for_config(&missing)
            },
        );
        assert!(
            gateway_is_stale(&broken, &missing),
            "an unavailable account is retried on reload"
        );

        let failed = crate::gateway::pending::PendingGateway::new(
            crate::gateway::engine::gateway_config(&good),
        );
        failed.fail("task panicked".into());
        let handle: crate::gateway::handle::GatewayHandle =
            Arc::new(crate::gateway::pending::PendingHandle(failed));
        assert!(
            gateway_is_stale(&handle, &good),
            "a failed build is retried"
        );
    }

    /// Выключенный шлюз готов сразу и ничего не читает: прежний путь.
    #[tokio::test]
    async fn a_disabled_gateway_is_ready_at_once_and_reads_nothing() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (handle, pending) = start_gateway(&Config::default(), &tx);
        assert!(pending.is_none() && !handle.is_enabled());
        assert!(
            rx.try_recv().is_err(),
            "no build event for a disabled gateway"
        );
    }
}
