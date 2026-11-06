//! Снимки экрана TUI без терминала: кадр рисуется в `TestBackend`, буфер
//! уходит в HTML (цвета и рамки как есть) и в чистый текст.
//!
//! Запуск и превращение HTML в PNG — `scripts/tui-capture.ps1`. Тест
//! `#[ignore]`: обычный `cargo test` его не трогает.
//!
//! Данные детерминированы: время сообщений, сессии и счётчики зафиксированы,
//! папка данных временная, токена и сети нет.

use super::capture_accounts as accounts;
use crate::app::AppState;
use crate::app::conversation::{Conversation, ConversationKind, QueuedMessage, ToolRun};
use crate::app::events::{GoalStage, View};
use crate::app::view_state::{Grant, Modal, PickerItem, PickerKind, PickerMode, PickerState};
use crate::config::{Config, GlyphMode};
use crate::mcp::types::ServerDisplayInfo;
use crate::provider::ChatMessage;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

/// Ячейка в пикселях. Каждый символ ставится в свою ячейку, поэтому метрики
/// шрифта не влияют на сетку.
const CELL_W: u32 = 10;
const CELL_H: u32 = 20;

type Size = (u16, u16);
const SIZES_ALL: &[Size] = &[(80, 24), (120, 40), (200, 60)];
const SIZES_MAIN: &[Size] = &[(120, 40)];
const SIZES_NARROW: &[Size] = &[(60, 20), (80, 24), (120, 40)];

/// Одна сцена: как собрать состояние и в каких размерах/темах снять.
struct Scene {
    name: &'static str,
    build: fn(&mut Config) -> AppState,
    sizes: &'static [Size],
    themes: &'static [&'static str],
    /// Снимается до того, как в папке данных появятся сессии (первый запуск).
    first_run: bool,
}

const DEFAULT_THEME: &[&str] = &["default"];
const THEMES_THREE: &[&str] = &["default", "nord", "paper"];
const SIZES_ACCOUNTS: &[Size] = &[(80, 24), (120, 40)];

fn scenes() -> Vec<Scene> {
    vec![
        Scene {
            name: "landing-empty",
            build: landing,
            sizes: SIZES_ALL,
            themes: DEFAULT_THEME,
            first_run: true,
        },
        Scene {
            name: "landing",
            build: landing,
            sizes: SIZES_ALL,
            themes: DEFAULT_THEME,
            first_run: false,
        },
        Scene {
            name: "landing-typing",
            build: landing_typing,
            sizes: SIZES_MAIN,
            themes: DEFAULT_THEME,
            first_run: false,
        },
        Scene {
            name: "chat-long",
            build: chat_long,
            sizes: SIZES_ALL,
            themes: DEFAULT_THEME,
            first_run: false,
        },
        Scene {
            name: "chat-working",
            build: chat_working,
            sizes: SIZES_NARROW,
            themes: DEFAULT_THEME,
            first_run: false,
        },
        Scene {
            name: "chat-working-wide",
            build: chat_working,
            sizes: &[(200, 60)],
            themes: DEFAULT_THEME,
            first_run: false,
        },
        Scene {
            name: "approval",
            build: approval,
            sizes: &[(80, 24), (120, 40)],
            themes: DEFAULT_THEME,
            first_run: false,
        },
        Scene {
            name: "autocomplete",
            build: autocomplete,
            sizes: SIZES_MAIN,
            themes: DEFAULT_THEME,
            first_run: false,
        },
        Scene {
            name: "models",
            build: models_picker,
            sizes: SIZES_MAIN,
            themes: DEFAULT_THEME,
            first_run: false,
        },
        Scene {
            name: "accounts",
            build: |c| with_base(c, accounts::accounts),
            sizes: SIZES_ACCOUNTS,
            themes: THEMES_THREE,
            first_run: false,
        },
        Scene {
            name: "accounts-second",
            build: |c| with_base(c, accounts::accounts_second_selected),
            sizes: SIZES_ACCOUNTS,
            themes: DEFAULT_THEME,
            first_run: false,
        },
        Scene {
            name: "accounts-details",
            build: |c| with_base(c, accounts::accounts_details),
            sizes: SIZES_ACCOUNTS,
            themes: DEFAULT_THEME,
            first_run: false,
        },
        Scene {
            name: "accounts-routes",
            build: |c| with_base(c, accounts::accounts_routes),
            sizes: SIZES_ACCOUNTS,
            themes: DEFAULT_THEME,
            first_run: false,
        },
        Scene {
            name: "accounts-off",
            build: |c| with_base(c, accounts::accounts_off),
            sizes: SIZES_ACCOUNTS,
            themes: DEFAULT_THEME,
            first_run: false,
        },
        Scene {
            name: "landing-accounts",
            build: |c| with_base(c, accounts::landing_accounts),
            sizes: SIZES_ACCOUNTS,
            themes: THEMES_THREE,
            first_run: false,
        },
        Scene {
            name: "landing-pick",
            build: landing,
            sizes: SIZES_ACCOUNTS,
            themes: DEFAULT_THEME,
            first_run: false,
        },
        Scene {
            name: "wizard-kind",
            build: |c| with_base(c, accounts::wizard_kind),
            sizes: SIZES_ACCOUNTS,
            themes: THEMES_THREE,
            first_run: false,
        },
        Scene {
            name: "wizard-confirm",
            build: |c| with_base(c, accounts::wizard_confirm),
            sizes: SIZES_ACCOUNTS,
            themes: DEFAULT_THEME,
            first_run: false,
        },
        Scene {
            name: "wizard-secret",
            build: |c| with_base(c, accounts::wizard_secret),
            sizes: SIZES_ACCOUNTS,
            themes: DEFAULT_THEME,
            first_run: false,
        },
        Scene {
            name: "wizard-direct",
            build: |c| with_base(c, accounts::wizard_direct),
            sizes: SIZES_ACCOUNTS,
            themes: DEFAULT_THEME,
            first_run: false,
        },
        Scene {
            name: "limits-form",
            build: |c| with_base(c, accounts::limits_form),
            sizes: SIZES_ACCOUNTS,
            themes: DEFAULT_THEME,
            first_run: false,
        },
        Scene {
            name: "secret-prompt",
            build: |c| with_base(c, accounts::secret_prompt),
            sizes: SIZES_ACCOUNTS,
            themes: THEMES_THREE,
            first_run: false,
        },
        Scene {
            name: "trust",
            build: |c| with_base(c, accounts::trust),
            sizes: SIZES_ACCOUNTS,
            themes: THEMES_THREE,
            first_run: false,
        },
        Scene {
            name: "usage",
            build: |c| with_base(c, accounts::usage),
            sizes: SIZES_ACCOUNTS,
            themes: DEFAULT_THEME,
            first_run: false,
        },
        Scene {
            name: "seam",
            build: |c| with_base(c, accounts::seam),
            sizes: SIZES_ACCOUNTS,
            themes: THEMES_THREE,
            first_run: false,
        },
        Scene {
            name: "mcp",
            build: mcp_view,
            sizes: SIZES_MAIN,
            themes: DEFAULT_THEME,
            first_run: false,
        },
        Scene {
            name: "themes-gallery",
            build: themes_gallery,
            sizes: SIZES_MAIN,
            themes: DEFAULT_THEME,
            first_run: false,
        },
        Scene {
            name: "onboarding",
            build: onboarding,
            sizes: &[(80, 24)],
            themes: DEFAULT_THEME,
            first_run: true,
        },
        Scene {
            name: "search",
            build: search,
            sizes: &[(80, 24)],
            themes: DEFAULT_THEME,
            first_run: true,
        },
        // Панель работы: очередь сообщений, много суб-агентов, ASCII-знаки.
        Scene {
            name: "dock-agents",
            build: dock_agents,
            sizes: SIZES_NARROW,
            themes: DEFAULT_THEME,
            first_run: false,
        },
        Scene {
            name: "chat-working-ascii",
            build: chat_working_ascii,
            sizes: &[(80, 24), (120, 40)],
            themes: DEFAULT_THEME,
            first_run: false,
        },
        Scene {
            name: "chat-typing",
            build: chat_typing,
            sizes: &[(80, 24), (120, 40)],
            themes: DEFAULT_THEME,
            first_run: false,
        },
        // Одна и та же рабочая сцена и главная в трёх других палитрах.
        Scene {
            name: "chat-working",
            build: chat_working,
            sizes: SIZES_MAIN,
            themes: &["paper", "dracula", "matrix", "nord", "gruvbox"],
            first_run: false,
        },
        Scene {
            name: "landing",
            build: landing,
            sizes: SIZES_MAIN,
            themes: &["paper", "gruvbox"],
            first_run: false,
        },
    ]
}

// ─── Фикстуры ──────────────────────────────────────────────────────────────

const SESSION_ID: &str = "2026-11-06T14-02-11-a1b2c3";
const STARTED: &str = "2026-11-06T14:02:11+00:00";

fn base(config: &mut Config, theme: &str) -> AppState {
    config.ui.theme = theme.to_string();
    let mut main = Conversation::fresh_main(None);
    main.session_id = SESSION_ID.to_string();
    main.session_started_at = STARTED.to_string();
    let mut state = AppState::new(main);
    state.workspace_path = r"D:\Projects\demo-api".to_string();
    state.status_message = "Ready".to_string();
    // Окно известно — в подвале видна шкала контекста.
    state.provider_context_window = 42_000;
    state
}

fn at(mut message: ChatMessage, time: &str, tokens: u32, model: &str) -> ChatMessage {
    message.created_at = format!("2026-11-06T{time}+00:00");
    message.total_tokens = Some(tokens);
    message.model = model.to_string();
    message
}

fn tool(name: &str, id: &str, body: &str, time: &str, failed: bool) -> ChatMessage {
    let mut message = ChatMessage::tool_with_display(id, name, body, body, failed);
    message.created_at = format!("2026-11-06T{time}+00:00");
    message
}

fn note(text: &str, time: &str) -> ChatMessage {
    let mut message = ChatMessage::ui_system(text);
    message.created_at = format!("2026-11-06T{time}+00:00");
    message
}

const MODEL: &str = "deepseek-chat";

const LONG_ANSWER: &str = "\
## Почему тест падает

Падает `parse_range`: на пустой строке он вызывает `unwrap()` на `None`.
Это **не** гонка — воспроизводится каждый раз.

1. Пустой ввод доходит до парсера, потому что валидация стоит *после* него.
2. В ответе `/v1/items` приходит `500` вместо `400`.
3. Лог пишет `thread 'main' panicked`, а клиент видит пустое тело.

```rust
fn parse_range(input: &str) -> Result<Range, ParseError> {
    let (from, to) = input.split_once('-').ok_or(ParseError::Empty)?;
    Ok(Range { from: from.parse()?, to: to.parse()? })
}
```

| Слой        | Было          | Стало            |
|-------------|---------------|------------------|
| парсер      | `unwrap()`    | `Result`         |
| обработчик  | 500           | 400 + текст      |
| тесты       | 41 / 42       | 43 / 43          |

> Заметка: старый тест `empty_range_panics` проверял именно панику — его нужно переписать.

- [x] причина найдена
- [ ] обновить документацию API
";

fn chat_messages() -> Vec<ChatMessage> {
    vec![
        at(
            ChatMessage::user("Тест `items::range` падает на CI, посмотри что там и почини."),
            "14:02:11",
            24,
            "",
        ),
        at(
            ChatMessage::assistant(
                "Сначала запущу тесты и посмотрю на падение, потом найду место в коде.",
            ),
            "14:02:19",
            71,
            MODEL,
        ),
        tool(
            "bash",
            "c1",
            "$ cargo test items::range\nrunning 3 tests\ntest items::range::empty ... FAILED\ntest items::range::single ... ok\ntest items::range::pair ... ok\n\nfailures:\n    items::range::empty\n\ntest result: FAILED. 2 passed; 1 failed
[exit code 101]",
            "14:02:34",
            false,
        ),
        tool(
            "read_file",
            "c2",
            "src/items/range.rs (84 lines)\n  12  pub fn parse_range(input: &str) -> Range {\n  13      let (from, to) = input.split_once('-').unwrap();",
            "14:02:41",
            false,
        ),
        tool(
            "grep",
            "c3",
            "src/items/handler.rs:41: parse_range(&query.range)\nsrc/items/mod.rs:9: pub mod range;",
            "14:02:44",
            false,
        ),
        note("Context 38% (16.2k / 42k)", "14:02:45"),
        at(
            {
                let mut answer = ChatMessage::assistant(LONG_ANSWER);
                answer.think_elapsed_secs = 12.3;
                answer
            },
            "14:03:02",
            612,
            MODEL,
        ),
        tool(
            "edit_file",
            "c4",
            "error: old_string not found in src/items/range.rs (file changed since read)",
            "14:03:20",
            true,
        ),
        at(
            ChatMessage::user("А можно сразу и тест поправить? И покажи diff кратко."),
            "14:04:01",
            19,
            "",
        ),
        at(
            ChatMessage::assistant(
                "Да. Переписал `parse_range` на `Result`, обработчик отвечает `400`, \
                 старый тест заменён на `empty_range_is_bad_request`. Все 43 теста зелёные.",
            ),
            "14:04:40",
            148,
            MODEL,
        ),
    ]
}

fn mcp_servers(state: &mut AppState) {
    let server = |name: &str, tools: usize, status: &str, auth: bool| ServerDisplayInfo {
        name: name.to_string(),
        transport: "stdio".to_string(),
        status: status.to_string(),
        tool_count: tools,
        resource_count: 0,
        enabled: true,
        needs_auth: auth,
    };
    state.mcp_status.view.servers = vec![
        server("context7", 2, "connected", false),
        server("playwright", 21, "connected", false),
        server("linear", 14, "auth required", true),
    ];
    state.mcp_status.server_count = 3;
    state.mcp_status.connected_count = 2;
}

/// Основа сцены и доработка из `capture_accounts`.
fn with_base(config: &mut Config, build: fn(AppState, &mut Config) -> AppState) -> AppState {
    let theme = config.ui.theme.clone();
    let state = base(config, &theme);
    build(state, config)
}

fn landing(config: &mut Config) -> AppState {
    let theme = config.ui.theme.clone();
    base(config, &theme)
}

fn landing_typing(config: &mut Config) -> AppState {
    let mut state = landing(config);
    state.input.buffer = "добавь в README раздел про запуск через docker compose".to_string();
    state.input.cursor = state.input.buffer.chars().count();
    state
}

fn chat_long(config: &mut Config) -> AppState {
    let theme = config.ui.theme.clone();
    let mut state = base(config, &theme);
    mcp_servers(&mut state);
    state.focused_mut().messages = chat_messages();
    state.focused_mut().generation.last_tokens = 148;
    state.focused_mut().generation.last_duration_secs = 8.4;
    state.focused_mut().generation.last_model = MODEL.to_string();
    state.focused_mut().context_used = 16_200;
    state
}

/// Агент работает: идёт ход, запущены инструмент, два суб-агента, фоновые
/// процессы, GOAL — всё, что в реальном сеансе выглядит как «идёт работа».
fn chat_working(config: &mut Config) -> AppState {
    let theme = config.ui.theme.clone();
    let mut state = base(config, &theme);
    mcp_servers(&mut state);
    let mut messages = chat_messages();
    messages.truncate(6);
    let mut streaming = at(
        ChatMessage::assistant(
            "Нашёл причину: `parse_range` вызывает `unwrap()` на пустой строке. \
             Сейчас перепишу его на `Result` и прогоню тесты ещё раз, потом",
        ),
        "14:03:02",
        88,
        MODEL,
    );
    streaming.status = Some("WIP".to_string());
    messages.push(streaming);
    state.focused_mut().messages = messages;

    let generation = &mut state.focused_mut().generation;
    generation.active = true;
    generation.start_time = Instant::now().checked_sub(Duration::from_secs(47));
    generation.animation_tick = 7;
    generation.last_tokens = 1840;
    generation.last_model = MODEL.to_string();
    state.focused_mut().context_used = 31_500;
    state.status_message = "Running bash...".to_string();

    state.focused_mut().current_tool = Some(ToolRun {
        name: "bash".to_string(),
        summary: "cargo test items::range".to_string(),
        started: ago(47),
    });
    for (title, tool, summary, secs) in [
        (
            "refactor tests",
            Some("edit_file"),
            "src/items/range.rs",
            31,
        ),
        ("update docs", None, "", 12),
    ] {
        add_agent(&mut state, title, tool, summary, secs);
    }
    state.focused_mut().queued.push_back(QueuedMessage {
        text: "и ещё добавь тест на пустую строку".to_string(),
        chips: Vec::new(),
        named: Vec::new(),
    });
    state.background.total = 2;
    state.background.interactive = 1;
    state.background.persistent = 1;
    state.timers_pending = 1;
    state.goal.mode = true;
    state.goal.stage = GoalStage::RunAgent1;
    state.goal.iteration = 2;
    state.error_count = 1;
    state.last_error = Some("mcp linear: authorization required (HTTP 401)".to_string());
    state.input.buffer = "а ещё проверь README".to_string();
    state.input.cursor = state.input.buffer.chars().count();
    state
}

/// Момент `secs` секунд назад: так на снимке у работы есть возраст.
fn ago(secs: u64) -> Instant {
    Instant::now()
        .checked_sub(Duration::from_secs(secs))
        .unwrap_or_else(Instant::now)
}

/// Суб-агент в работе: свой чат, инструмент и время.
fn add_agent(state: &mut AppState, title: &str, tool: Option<&str>, summary: &str, secs: u64) {
    let mut agent = Conversation::fresh_main(None);
    agent.kind = ConversationKind::SubAgent;
    agent.parent = Some(state.conversations.focused_id());
    agent.title = title.to_string();
    agent.generation.active = true;
    agent.generation.start_time = Instant::now().checked_sub(Duration::from_secs(secs));
    agent.current_tool = tool.map(|name| ToolRun {
        name: name.to_string(),
        summary: summary.to_string(),
        started: ago(secs),
    });
    state.conversations.add_background(agent);
}

/// Много суб-агентов сразу: панель обязана уложиться в четыре строки.
fn dock_agents(config: &mut Config) -> AppState {
    let mut state = chat_working(config);
    for index in 0..4 {
        add_agent(
            &mut state,
            &format!("worker {index}"),
            Some("grep"),
            "parse_range",
            5 + index,
        );
    }
    state
}

fn chat_working_ascii(config: &mut Config) -> AppState {
    config.ui.glyphs = GlyphMode::Ascii;
    chat_working(config)
}

/// Простой чат, человек печатает длинное сообщение: ввод растёт вверх.
fn chat_typing(config: &mut Config) -> AppState {
    let mut state = chat_long(config);
    state.input.buffer = "Добавь тест на пустую строку,\nпроверь граничные случаи (0, 1, максимум)\nи обнови документацию API.".to_string();
    state.input.cursor = state.input.buffer.chars().count();
    state
}

fn approval(config: &mut Config) -> AppState {
    let mut state = chat_working(config);
    state.status_message = "Approve tool bash?".to_string();
    state.modal = Some(Modal::ToolApproval {
        tool_name: "bash".to_string(),
        arguments: concat!(
            "{\n",
            "  \"command\": \"cargo test --bin pooprusteek items::range -- --nocapture",
            " && git diff --stat && cargo clippy --bin pooprusteek -- -D warnings",
            " && echo the-whole-command-is-visible\",\n",
            "  \"timeout\": 120\n",
            "}"
        )
        .to_string(),
        origin: Some("sub-agent: refactor tests".to_string()),
        scroll_offset: 0,
        grant: Grant::Once,
        scope: None,
    });
    state
}

fn autocomplete(config: &mut Config) -> AppState {
    let mut state = chat_long(config);
    state.input.buffer = "/".to_string();
    state.input.cursor = 1;
    let suggestion = |name: &str, description: &str| crate::commands::CommandSuggestion {
        name: name.to_string(),
        description: description.to_string(),
        usage: format!("/{name}"),
    };
    state.autocomplete.visible = true;
    state.autocomplete.items = vec![
        suggestion("help", "Show commands and shortcuts"),
        suggestion("models", "Switch the model of the active provider"),
        suggestion("providers", "Manage extra LLM providers"),
        suggestion("mcp", "MCP servers: status, add, auth"),
        suggestion("sessions", "Load or manage saved sessions"),
        suggestion("compact", "Compress the conversation context"),
    ];
    state
}

fn models_picker(config: &mut Config) -> AppState {
    let mut state = chat_long(config);
    let items = [
        "deepseek-chat",
        "deepseek-reasoner",
        "deepseek-v4.1-flash",
        "deepseek-v4.1-pro",
    ]
    .iter()
    .map(|model| PickerItem::new(*model, *model))
    .collect();
    state.modal = Some(Modal::Picker(PickerState::new_with_kind(
        "Models · DeepSeek",
        items,
        PickerMode::Single,
        PickerKind::Models,
    )));
    state
}

fn onboarding(config: &mut Config) -> AppState {
    let theme = config.ui.theme.clone();
    let mut state = base(config, &theme);
    state.view = View::Onboarding;
    state
}

fn search(config: &mut Config) -> AppState {
    let theme = config.ui.theme.clone();
    let mut state = base(config, &theme);
    state.view = View::Search;
    state
}

fn mcp_view(config: &mut Config) -> AppState {
    let theme = config.ui.theme.clone();
    let mut state = base(config, &theme);
    mcp_servers(&mut state);
    state.mcp_status.view.active = true;
    state.view = View::Mcp;
    state
}

fn themes_gallery(config: &mut Config) -> AppState {
    let theme = config.ui.theme.clone();
    let mut state = base(config, &theme);
    state.view = View::Themes;
    state.themes.selected = 2;
    state
}

/// Сессии для таблицы «Recent sessions» на главной.
fn plant_sessions(config: &Config) {
    let rows = [
        (
            "2026-11-05T09-12-40-11aa22",
            "Починить падающий тест items::range",
            "deepseek-chat",
        ),
        (
            "2026-11-04T18-40-02-33bb44",
            "Перенести конфиг CI на reusable workflow",
            "deepseek-reasoner",
        ),
        (
            "2026-11-03T12-05-55-55cc66",
            "Разобрать утечку памяти в воркере очереди",
            "lm-studio",
        ),
        (
            "2026-11-01T21-30-10-77dd88",
            "Написать README и пример запуска",
            "deepseek-chat",
        ),
    ];
    for (id, title, model) in rows {
        let stamp = id.get(..19).unwrap_or_default();
        let iso = format!(
            "{}:{}:{}+00:00",
            &stamp[..13],
            &stamp[14..16],
            &stamp[17..19]
        );
        let session = crate::session::Session {
            version: crate::session::SESSION_VERSION,
            id: id.to_string(),
            created_at: iso.clone(),
            updated_at: iso,
            workspace_root: r"D:\Projects\demo-api".to_string(),
            model_type: model.to_string(),
            messages: vec![ChatMessage::assistant("Готово."), ChatMessage::user(title)],
            tag: None,
            title: None,
            provider_session_id: None,
            provider_parent_message_id: None,
            broken: false,
            route_state: None,
        };
        let _ = crate::session::save_local(&session, config);
    }
}

// ─── Буфер → HTML / текст ──────────────────────────────────────────────────

const DEFAULT_FG: &str = "#cccccc";
const DEFAULT_BG: &str = "#0c0c0c";

fn indexed(n: u8) -> String {
    const BASE: [&str; 16] = [
        "#0c0c0c", "#c50f1f", "#13a10e", "#c19c00", "#0037da", "#881798", "#3a96dd", "#cccccc",
        "#767676", "#e74856", "#16c60c", "#f9f1a5", "#3b78ff", "#b4009e", "#61d6d6", "#f2f2f2",
    ];
    match n {
        0..=15 => BASE[n as usize].to_string(),
        16..=231 => {
            let n = n - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            format!(
                "#{:02x}{:02x}{:02x}",
                level(n / 36),
                level(n / 6 % 6),
                level(n % 6)
            )
        }
        _ => {
            let v = 8 + (n - 232) * 10;
            format!("#{v:02x}{v:02x}{v:02x}")
        }
    }
}

fn hex(color: Color, fallback: &str) -> String {
    match color {
        Color::Reset => fallback.to_string(),
        Color::Black => indexed(0),
        Color::Red => indexed(1),
        Color::Green => indexed(2),
        Color::Yellow => indexed(3),
        Color::Blue => indexed(4),
        Color::Magenta => indexed(5),
        Color::Cyan => indexed(6),
        Color::Gray => indexed(7),
        Color::DarkGray => indexed(8),
        Color::LightRed => indexed(9),
        Color::LightGreen => indexed(10),
        Color::LightYellow => indexed(11),
        Color::LightBlue => indexed(12),
        Color::LightMagenta => indexed(13),
        Color::LightCyan => indexed(14),
        Color::White => indexed(15),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Indexed(n) => indexed(n),
    }
}

/// Цвет и начертание одной ячейки после применения REVERSED.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Look {
    fg: String,
    bg: String,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
}

fn look(cell: &ratatui::buffer::Cell) -> Look {
    let modifier = cell.modifier;
    let (mut fg, mut bg) = (hex(cell.fg, DEFAULT_FG), hex(cell.bg, DEFAULT_BG));
    if modifier.contains(Modifier::REVERSED) {
        std::mem::swap(&mut fg, &mut bg);
    }
    Look {
        fg,
        bg,
        bold: modifier.contains(Modifier::BOLD),
        italic: modifier.contains(Modifier::ITALIC),
        underline: modifier.contains(Modifier::UNDERLINED),
        strike: modifier.contains(Modifier::CROSSED_OUT),
    }
}

/// Ячейки строки без «хвостов» широких символов: (текст, ширина в колонках, вид).
fn row_cells(buffer: &Buffer, y: u16) -> Vec<(String, u16, Look)> {
    let width = buffer.area.width;
    let mut cells = Vec::new();
    let mut x = 0;
    while x < width {
        let cell = &buffer[(x, y)];
        let symbol = cell.symbol();
        let cols = (UnicodeWidthStr::width(symbol) as u16).max(1);
        cells.push((symbol.to_string(), cols.min(width - x), look(cell)));
        x += cols;
    }
    cells
}

fn plain_text(buffer: &Buffer) -> String {
    let mut out = String::new();
    for y in 0..buffer.area.height {
        let line: String = row_cells(buffer, y)
            .iter()
            .map(|(s, ..)| s.as_str())
            .collect();
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn to_html(buffer: &Buffer, cursor: Option<(u16, u16)>, title: &str) -> String {
    let (cols, rows) = (buffer.area.width as u32, buffer.area.height as u32);
    let mut classes: Vec<Look> = Vec::new();
    let mut body = String::new();
    for y in 0..buffer.area.height {
        body.push_str("<div class=r>");
        let cells = row_cells(buffer, y);
        let mut i = 0;
        while i < cells.len() {
            let (text, width, view) = &cells[i];
            let id = classes.iter().position(|c| c == view).unwrap_or_else(|| {
                classes.push(view.clone());
                classes.len() - 1
            });
            // Подряд идущие пробелы одного вида — один span.
            if text == " " {
                let mut run = 1u32;
                while i + (run as usize) < cells.len()
                    && cells[i + run as usize].0 == " "
                    && cells[i + run as usize].2 == *view
                {
                    run += 1;
                }
                let _ = write!(
                    body,
                    "<i class=c{id} style=\"width:{}px\"> </i>",
                    run * CELL_W
                );
                i += run as usize;
                continue;
            }
            let _ = write!(
                body,
                "<i class=c{id} style=\"width:{}px\">{}</i>",
                *width as u32 * CELL_W,
                escape(text)
            );
            i += 1;
        }
        body.push_str("</div>\n");
    }
    let mut css = String::new();
    for (id, view) in classes.iter().enumerate() {
        let _ = write!(css, ".c{id}{{color:{};background:{}", view.fg, view.bg);
        if view.bold {
            css.push_str(";font-weight:700");
        }
        if view.italic {
            css.push_str(";font-style:italic");
        }
        match (view.underline, view.strike) {
            (true, true) => css.push_str(";text-decoration:underline line-through"),
            (true, false) => css.push_str(";text-decoration:underline"),
            (false, true) => css.push_str(";text-decoration:line-through"),
            _ => {}
        }
        css.push_str("}\n");
    }
    let cursor_html = cursor
        .map(|(x, y)| {
            format!(
                "<b class=cur style=\"left:{}px;top:{}px\"></b>",
                x as u32 * CELL_W,
                y as u32 * CELL_H
            )
        })
        .unwrap_or_default();
    format!(
        "<!doctype html><html><head><meta charset=utf-8><title>{}</title><style>\
html,body{{margin:0;padding:0;background:{DEFAULT_BG};overflow:hidden}}\
#t{{position:relative;width:{}px;height:{}px;font:17px 'Cascadia Mono',Consolas,'Segoe UI Symbol',monospace}}\
.r{{display:flex;height:{CELL_H}px}}\
i{{flex:none;display:block;box-sizing:border-box;text-align:center;white-space:pre;overflow:visible;font-style:normal;line-height:{CELL_H}px;height:{CELL_H}px}}\
.cur{{position:absolute;width:{CELL_W}px;height:{CELL_H}px;box-sizing:border-box;border:1px solid #fff;mix-blend-mode:difference}}\
{css}</style></head><body><div id=t>\n{body}{cursor_html}</div></body></html>\n",
        escape(title),
        cols * CELL_W,
        rows * CELL_H,
    )
}

// ─── Прогон ────────────────────────────────────────────────────────────────

struct Shot {
    file: String,
    width_px: u32,
    height_px: u32,
}

fn capture(
    scene: &Scene,
    theme: &str,
    (width, height): Size,
    out: &Path,
    config_template: &Config,
) -> Shot {
    let mut config = config_template.clone();
    config.ui.theme = theme.to_string();
    crate::tui::render::fix_clock("14:02");
    let mut state = (scene.build)(&mut config);
    state.terminal_rows = height;
    state.terminal_cols = width;
    state
        .landing
        .apply(crate::session::list_sessions().unwrap_or_default());
    if scene.name == "landing-pick" {
        state.landing.step_down();
        state.landing.step_down();
    }

    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    let mut cursor = None;
    terminal
        .draw(|frame| cursor = crate::tui::render::draw_frame(frame, &state, &config))
        .expect("draw frame");
    // Курсор показывается только там, где живой рендер его не прячет.
    let cursor_visible = state.modal.is_none()
        && !state.focused().generation.active
        && !matches!(
            state.view,
            View::Mcp | View::Accounts | View::Search | View::Themes
        );
    let cursor = cursor.filter(|_| cursor_visible);

    let buffer = terminal.backend().buffer();
    let file = format!("{}__{}__{}x{}", scene.name, theme, width, height);
    let title = format!("{} · {theme} · {width}x{height}", scene.name);
    std::fs::write(
        out.join(format!("{file}.html")),
        to_html(buffer, cursor, &title),
    )
    .expect("write html");
    std::fs::write(out.join(format!("{file}.txt")), plain_text(buffer)).expect("write txt");
    Shot {
        file,
        width_px: width as u32 * CELL_W,
        height_px: height as u32 * CELL_H,
    }
}

/// `TUI_CAPTURE_OUT` — куда класть файлы (по умолчанию `target/tui-capture`),
/// `TUI_CAPTURE_ONLY` — подстрока имени сцены, чтобы снять часть.
#[test]
#[ignore = "инструмент снимков: см. scripts/tui-capture.ps1"]
fn capture_screens() {
    let out = PathBuf::from(
        std::env::var("TUI_CAPTURE_OUT").unwrap_or_else(|_| "target/tui-capture".to_string()),
    );
    let only = std::env::var("TUI_CAPTURE_ONLY").ok();
    std::fs::create_dir_all(&out).expect("output dir");

    let data = std::env::temp_dir().join("pooprusteek-capture-data");
    let _ = std::fs::remove_dir_all(&data);
    crate::config::set_data_dir(&data).expect("data dir");

    let mut config = Config::default();
    // Снимки в HTML не могут показать «квадраты» консоли, поэтому Unicode по умолчанию.
    config.ui.glyphs = GlyphMode::Unicode;
    let selected = |scene: &Scene| only.as_deref().is_none_or(|part| scene.name.contains(part));
    let mut manifest = String::new();
    let mut record = |shot: Shot| {
        let _ = writeln!(
            manifest,
            "{}\t{}\t{}",
            shot.file, shot.width_px, shot.height_px
        );
    };

    // Первый запуск: сессий нет, главная без таблицы.
    let all = scenes();
    for scene in all.iter().filter(|s| selected(s) && s.first_run) {
        for theme in scene.themes {
            for size in scene.sizes {
                record(capture(scene, theme, *size, &out, &config));
            }
        }
    }

    plant_sessions(&config);
    for scene in all.iter().filter(|s| selected(s) && !s.first_run) {
        for theme in scene.themes {
            for size in scene.sizes {
                record(capture(scene, theme, *size, &out, &config));
            }
        }
    }
    std::fs::write(out.join("manifest.tsv"), manifest).expect("manifest");
    let _ = std::fs::remove_dir_all(&data);
}

/// ASCII-режим не рисует ни одного знака, который старая консоль покажет
/// квадратом: проходим по всем сценам, включая онбординг, поиск и темы.
#[test]
fn ascii_mode_draws_only_console_safe_characters() {
    let template = Config::default();
    let mut offenders = Vec::new();
    for scene in scenes() {
        for theme in scene.themes.iter().take(1) {
            let mut config = template.clone();
            config.ui.theme = (*theme).to_string();
            let mut state = (scene.build)(&mut config);
            config.ui.glyphs = GlyphMode::Ascii;
            let (width, height) = (100, 30);
            state.terminal_rows = height;
            state.terminal_cols = width;
            let mut terminal =
                Terminal::new(TestBackend::new(width, height)).expect("test terminal");
            terminal
                .draw(|frame| {
                    crate::tui::render::draw_frame(frame, &state, &config);
                })
                .expect("draw frame");
            let buffer = terminal.backend().buffer();
            for y in 0..height {
                for x in 0..width {
                    let symbol = buffer[(x, y)].symbol();
                    if let Some(bad) = symbol
                        .chars()
                        .find(|ch| !crate::tui::glyphs::is_console_safe(*ch))
                    {
                        offenders.push(format!("{} ({x},{y}): {bad:?}", scene.name));
                    }
                }
            }
        }
    }
    assert!(offenders.is_empty(), "{offenders:#?}");
}
