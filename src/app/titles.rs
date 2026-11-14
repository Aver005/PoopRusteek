//! Заголовки чатов служебной моделью (`[roles] title`).
//!
//! Один запрос на чат — после первого ответа ассистента, всегда в фоне
//! (инвариант 1: цикл событий не ждёт сеть). Запрос идёт на одноразовом форке
//! и удаляет свою серверную сессию, поэтому чатов в аккаунте DeepSeek не
//! остаётся, а в историю беседы он не попадает. Не вышло — остаётся запасной
//! `session::derive_title`, на экране ничего не меняется.
//!
//! Текст модели — недоверенный: одна строка, без разметки, кавычек и
//! управления. Инструменты в служебный запрос не передаются никогда.

use super::conversation::ConversationId;
use super::events::AppEvent;
use crate::gateway::handle::{CallSource, GatewayHandle};
use crate::gateway::types::Role as ServiceRole;
use crate::provider::{ChatMessage, CompletionRequest, LLMProvider, Role};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

/// Сколько ждём служебную модель. Не ответила — заголовок остаётся запасным.
pub(crate) const TITLE_TIMEOUT: Duration = Duration::from_secs(10);

/// Предел длины заголовка в символах.
pub(crate) const TITLE_MAX_CHARS: usize = 60;

/// Сколько текста каждой реплики показываем модели.
const TITLE_SOURCE_BYTES: usize = 800;

/// Сколько токенов разрешаем ответу: заголовок короче.
const TITLE_MAX_TOKENS: u32 = 48;

/// Что просим у служебной модели.
const TITLE_SYSTEM: &str = "You name chat threads. Reply with the title only: at most 6 words, at most 60 characters, in the language of the conversation. No quotes, no markdown, no ending punctuation.";

/// Заголовок из первой просьбы и первого ответа. Нет просьбы — нечего называть.
pub(crate) fn title_prompt(messages: &[ChatMessage]) -> Option<String> {
    let first = |role: Role| {
        messages
            .iter()
            .find(|message| {
                message.role == role
                    && !message.ui_only
                    && !message.visible_content().trim().is_empty()
            })
            .map(|message| snippet(message.visible_content()))
    };
    let question = first(Role::User)?;
    let mut prompt = format!("First user message:\n{question}");
    if let Some(answer) = first(Role::Assistant) {
        prompt.push_str(&format!("\n\nFirst assistant answer:\n{answer}"));
    }
    Some(prompt)
}

/// Кусок реплики: модель видит начало, не весь ход.
fn snippet(text: &str) -> String {
    // Запрос может уйти на другой аккаунт: секрет из реплики туда не попадает.
    let safe = crate::secrets::redact::redact(text.trim());
    crate::util::truncate_with_ellipsis(&safe, TITLE_SOURCE_BYTES)
}

/// Текст модели недоверенный: одна строка без разметки, кавычек и управления.
/// Пустой результат не принимается.
pub(crate) fn clean_title(raw: &str) -> Option<String> {
    let line = raw.lines().find(|line| !line.trim().is_empty())?.trim();
    let body: String = line
        .chars()
        // Управление — пробелом: перевод строки не должен склеивать слова.
        .map(|c| if c.is_control() { ' ' } else { c })
        .filter(|c| !matches!(c, '#' | '*' | '`' | '_' | '~' | '|' | '<' | '>'))
        .collect();
    let body = strip_list_marker(body.trim());
    let body = strip_label(body);
    let body = strip_quotes(body.trim());

    let mut text = String::new();
    let mut last_space = false;
    for c in body.chars() {
        if c.is_whitespace() {
            if !last_space && !text.is_empty() {
                text.push(' ');
            }
            last_space = true;
        } else {
            text.push(c);
            last_space = false;
        }
    }
    // Инвариант 4: режем по символам, а не по байтам.
    let text: String = text
        .trim()
        .trim_end_matches(['.', ',', ';', ':'])
        .trim()
        .to_string();
    let text: String = text.chars().take(TITLE_MAX_CHARS).collect();
    let text = text.trim().trim_end_matches(['.', ',', ';', ':']).trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// `- `, `* `, `• ` и `1. `/`1) ` в начале строки — не часть названия.
fn strip_list_marker(line: &str) -> &str {
    let rest = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("• "))
        .or_else(|| line.strip_prefix("– "));
    if let Some(rest) = rest {
        return rest.trim();
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits > 0
        && let Some(rest) = line[digits..]
            .strip_prefix(". ")
            .or_else(|| line[digits..].strip_prefix(") "))
    {
        return rest.trim();
    }
    line
}

/// Модель часто повторяет подпись «Title:» или «Заголовок:» — она не нужна.
fn strip_label(line: &str) -> &str {
    for label in ["title:", "chat title:", "заголовок:"] {
        let Some(head) = line.get(..label.len()) else {
            continue;
        };
        if head.to_lowercase() == label {
            return line[label.len()..].trim();
        }
    }
    line
}

/// Кавычки вокруг названия — не часть названия.
fn strip_quotes(line: &str) -> &str {
    let quotes = ['"', '\'', '«', '»', '“', '”', '‘', '’', '„', '`'];
    line.trim_matches(|c| quotes.contains(&c)).trim()
}

/// Одноразовый форк на роль `title`, ответ — событием `ChatTitle`.
pub(crate) fn spawn_title_request(
    gateway: GatewayHandle,
    conversation: ConversationId,
    prompt: String,
    event_tx: UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        // Роль не настроена или шлюз выключен — заголовок остаётся запасным.
        let Some(provider) =
            gateway.provider_for_role(ServiceRole::Title, CallSource::Role(ServiceRole::Title))
        else {
            return;
        };
        // Форк обязателен и одноразов: чужая сессия беседы не должна меняться.
        let provider = provider.fork();
        if let Some(title) = fetch_title(&provider, prompt).await {
            let _ = event_tx.send(AppEvent::ChatTitle {
                conversation,
                title,
            });
        }
    });
}

/// Спросить заголовок и убрать за собой серверную сессию форка. `None` — любой
/// отказ: ошибка, пустой ответ, таймаут.
async fn fetch_title(provider: &Arc<dyn LLMProvider>, prompt: String) -> Option<String> {
    fetch_title_within(provider, prompt, TITLE_TIMEOUT).await
}

/// То же с явным сроком: тесту не нужно ждать настоящие десять секунд.
async fn fetch_title_within(
    provider: &Arc<dyn LLMProvider>,
    prompt: String,
    timeout: Duration,
) -> Option<String> {
    let discard = crate::provider::SessionDiscard::new(Arc::clone(provider));
    let request = CompletionRequest {
        messages: vec![
            ChatMessage::system(TITLE_SYSTEM),
            ChatMessage::user(&prompt),
        ],
        // Инструменты в служебный запрос не передаются никогда.
        tools: Vec::new(),
        // Цель задаёт маршрут роли; поле остаётся устаревшим подсказчиком.
        model: String::new(),
        temperature: 0.2,
        max_tokens: TITLE_MAX_TOKENS,
        stream: false,
    };
    let answer = tokio::time::timeout(timeout, provider.complete(request)).await;
    discard.now().await;
    match answer {
        Ok(Ok(response)) => clean_title(&response.content),
        _ => None,
    }
}

impl super::App {
    /// После первого законченного хода: попросить заголовок один раз на чат.
    /// Сеть не ждём — задача уходит в фон и вернётся событием.
    pub(crate) fn maybe_request_chat_title(&mut self, target: ConversationId) {
        let Some(conversation) = self.state.conversations.get_mut(target) else {
            return;
        };
        // Побочные чаты в списке сессий не показываются и на диск не пишутся.
        if conversation.title_requested
            || !conversation.title.trim().is_empty()
            || conversation.is_background_kind()
        {
            return;
        }
        let Some(prompt) = title_prompt(&conversation.messages) else {
            return;
        };
        // Пометка до запроса: отказ роли и таймаут второго шанса не получают.
        conversation.title_requested = true;
        spawn_title_request(
            Arc::clone(&self.gateway),
            target,
            prompt,
            self.event_tx.clone(),
        );
    }

    /// `AppEvent::ChatTitle`: заголовок модели в чат и в файл сессии.
    pub(crate) fn on_chat_title(&mut self, target: ConversationId, title: String) {
        let Some(conversation) = self.state.conversations.get_mut(target) else {
            return;
        };
        // Пришедший позже ответ не перебивает уже известное название.
        if !conversation.title.trim().is_empty() {
            return;
        }
        conversation.title = title;
        let focused = self.state.conversations.focused_id() == target;
        let streaming = self.state.focused().is_streaming();
        // Файл сессии пишет тот же воркер, что и автосейв, — без гонки за файл.
        if focused && !streaming {
            self.auto_save_session();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_known_secret_in_the_chat_does_not_reach_the_title_account() {
        let secret = crate::secrets::Secret::new("title-test-secret-value-9f3a71c2");
        crate::secrets::redact::register(&secret);
        let messages = [ChatMessage::new(
            Role::User,
            "use the key title-test-secret-value-9f3a71c2 please",
        )];
        let prompt = title_prompt(&messages).expect("prompt");
        assert!(
            !prompt.contains("title-test-secret-value-9f3a71c2"),
            "{prompt}"
        );
        assert!(prompt.contains("use the key"), "{prompt}");
    }

    use crate::gateway::handle::{DisabledGateway, Gateway};
    use crate::gateway::testkit::{Act, FakeBackend, rig};
    use crate::provider::fake::FakeProvider;
    use crate::provider::{CompletionResponse, LLMProvider};

    const TITLE_RIG: &str = r#"
        [gateway]
        enabled = true

        [[accounts]]
        id = "cheap"
        provider = "custom"
        base_url = "http://cheap.invalid/v1"
        secret = "none"
        model = "cheap-model"

        [[routes]]
        id = "main"
        steps = [{ account = "cheap" }]

        [[routes]]
        id = "cheap"
        steps = [{ account = "cheap" }]

        [roles]
        main = "main"
        title = "cheap"
        summary = "cheap"
        evaluator = "cheap"
    "#;

    #[test]
    fn the_service_title_is_one_clean_line() {
        assert_eq!(
            clean_title("**Сборка проекта**\nвторая строка").as_deref(),
            Some("Сборка проекта")
        );
        assert_eq!(clean_title("«Заголовок»").as_deref(), Some("Заголовок"));
        assert_eq!(
            clean_title("Title: My chat.").as_deref(),
            Some("My chat"),
            "an echoed label and the ending period are not part of the title"
        );
        assert_eq!(clean_title("- покупки").as_deref(), Some("покупки"));
        assert_eq!(
            clean_title("1. разбор логов").as_deref(),
            Some("разбор логов")
        );
        assert_eq!(
            clean_title("`code` and **bold**").as_deref(),
            Some("code and bold")
        );
        assert_eq!(
            clean_title("строка\u{7} с управляющим").as_deref(),
            Some("строка с управляющим")
        );
        assert_eq!(
            clean_title("  много   пробелов \t и\tтабов ").as_deref(),
            Some("много пробелов и табов")
        );
    }

    #[test]
    fn an_empty_title_is_not_accepted() {
        for raw in ["", "   ", "\n\n", "#", "``", "\u{7}", "\n# **\n"] {
            assert_eq!(clean_title(raw), None, "«{raw}» must not become a title");
        }
    }

    /// 60 символов, а не 60 байт: кириллица режется по границе символа.
    #[test]
    fn a_long_title_is_cut_on_a_character_boundary() {
        let long = "ж".repeat(200);
        let title = clean_title(&long).expect("a long title is truncated, not dropped");
        assert_eq!(title.chars().count(), TITLE_MAX_CHARS);
        assert_eq!(title.len(), TITLE_MAX_CHARS * 2, "no broken UTF-8");
    }

    #[test]
    fn the_prompt_carries_the_first_question_and_the_first_answer() {
        let messages = vec![
            ChatMessage::user("как собрать проект?"),
            ChatMessage::assistant("Вот шаги."),
            ChatMessage::user("а тесты?"),
            ChatMessage::assistant("Тоже."),
        ];
        let prompt = title_prompt(&messages).expect("there is something to name");
        assert!(prompt.contains("как собрать проект?"), "{prompt}");
        assert!(prompt.contains("Вот шаги."), "{prompt}");
        assert!(
            !prompt.contains("а тесты?"),
            "only the opening turn: {prompt}"
        );
        // Служебная заметка интерфейса не считается вопросом.
        let mut chrome = ChatMessage::user("◈ btw started");
        chrome.ui_only = true;
        assert!(title_prompt(&[chrome]).is_none());
        assert!(title_prompt(&[]).is_none());
    }

    #[tokio::test]
    async fn a_clean_model_answer_becomes_the_title() {
        let fake = Arc::new(FakeProvider::with_response(
            "**Сборка проекта**\nлишняя строка",
        ));
        let provider: Arc<dyn LLMProvider> = fake.clone();
        let title = fetch_title(&provider, "первый вопрос".to_string()).await;
        assert_eq!(title.as_deref(), Some("Сборка проекта"));

        let sent = fake.request(0).expect("the service model was asked");
        assert_eq!(sent.len(), 2, "system prompt + the one question");
        assert_eq!(
            sent[0].role,
            Role::System,
            "the service rule rides as a system prompt"
        );
        assert_eq!(
            fake.declared_tools(0),
            Some(Vec::new()),
            "tools never reach the service call"
        );
    }

    #[tokio::test]
    async fn an_empty_or_failed_answer_leaves_the_fallback() {
        let empty: Arc<dyn LLMProvider> = Arc::new(FakeProvider::with_response("  \n "));
        assert_eq!(fetch_title(&empty, "вопрос".to_string()).await, None);

        struct Broken;
        #[async_trait::async_trait]
        impl LLMProvider for Broken {
            async fn complete(
                &self,
                _request: CompletionRequest,
            ) -> crate::error::AppResult<CompletionResponse> {
                Err(crate::error::AppError::Custom("upstream is down".into()))
            }
            async fn complete_stream(
                &self,
                _request: CompletionRequest,
                _tx: tokio::sync::mpsc::UnboundedSender<crate::provider::CompletionChunk>,
            ) -> crate::error::AppResult<()> {
                unreachable!("the title call is not streamed")
            }
            fn model(&self) -> &str {
                "broken"
            }
            fn fork(&self) -> Arc<dyn LLMProvider> {
                Arc::new(Broken)
            }
        }
        let broken: Arc<dyn LLMProvider> = Arc::new(Broken);
        assert_eq!(fetch_title(&broken, "вопрос".to_string()).await, None);
    }

    /// Молчащая модель не держит чат вечно: срок закрывает запрос и остаётся запасной.
    #[tokio::test]
    async fn a_silent_service_model_times_out() {
        struct Silent;
        #[async_trait::async_trait]
        impl LLMProvider for Silent {
            async fn complete(
                &self,
                _request: CompletionRequest,
            ) -> crate::error::AppResult<CompletionResponse> {
                std::future::pending().await
            }
            async fn complete_stream(
                &self,
                _request: CompletionRequest,
                _tx: tokio::sync::mpsc::UnboundedSender<crate::provider::CompletionChunk>,
            ) -> crate::error::AppResult<()> {
                unreachable!()
            }
            fn model(&self) -> &str {
                "silent"
            }
            fn fork(&self) -> Arc<dyn LLMProvider> {
                Arc::new(Silent)
            }
        }
        let provider: Arc<dyn LLMProvider> = Arc::new(Silent);
        let started = std::time::Instant::now();
        let title =
            fetch_title_within(&provider, "вопрос".to_string(), Duration::from_millis(50)).await;
        assert_eq!(title, None);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the request is cut by the deadline, not left hanging"
        );
        assert_eq!(
            TITLE_TIMEOUT,
            Duration::from_secs(10),
            "the deadline the user sees is ten seconds"
        );
    }

    /// Роль ведёт вызов, а серверная сессия форка после него удаляется.
    #[tokio::test]
    async fn the_title_role_takes_the_call_and_cleans_up_its_session() {
        let rig = rig(
            TITLE_RIG,
            vec![(
                "cheap",
                FakeBackend::new(vec![Act::Reply("Сборка проекта")]),
            )],
        );
        let provider = rig
            .engine
            .provider_for_role(ServiceRole::Title, CallSource::Role(ServiceRole::Title))
            .expect("the title role has a route");
        let title = fetch_title(&provider.fork(), "как собрать проект?".to_string()).await;
        assert_eq!(title.as_deref(), Some("Сборка проекта"));
        assert_eq!(
            rig.fakes["cheap"].discards(),
            1,
            "the one-off chat must be deleted in the account"
        );
    }

    async fn test_app() -> crate::app::App {
        let mut config = crate::config::Config::default();
        config.semantic.enabled = false;
        config.update.auto = false;
        crate::app::App::new(config).await.expect("the app starts")
    }

    async fn wait_for_title_event(app: &mut crate::app::App) -> (ConversationId, String) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                while let Ok(event) = app.event_rx.try_recv() {
                    if let AppEvent::ChatTitle {
                        conversation,
                        title,
                    } = event
                    {
                        return (conversation, title);
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the title arrives as an event")
    }

    /// Без роли запроса нет; с ролью — ровно один на чат, повтор не уходит.
    #[tokio::test]
    async fn a_chat_asks_for_a_title_once_and_only_when_the_role_is_set() {
        let mut app = test_app().await;
        let target = app.state.conversations.focused_id();
        app.state.focused_mut().messages = vec![
            ChatMessage::user("как собрать проект?"),
            ChatMessage::assistant("Вот шаги."),
        ];

        // Шлюз выключен: роли нет, заголовок остаётся запасным.
        app.maybe_request_chat_title(target);
        assert!(app.state.focused().title_requested);
        assert!(app.state.focused().title.is_empty());

        // Роль настроена: один вызов и ни одного повтора.
        let rig = rig(
            TITLE_RIG,
            vec![(
                "cheap",
                FakeBackend::new(vec![Act::Reply("Сборка проекта")]),
            )],
        );
        app.gateway = Arc::new(rig.engine);
        let target = app.state.conversations.focused_id();
        app.state.focused_mut().title_requested = false;
        app.maybe_request_chat_title(target);
        app.maybe_request_chat_title(target);

        let (conversation, title) = wait_for_title_event(&mut app).await;
        assert_eq!(conversation, target);
        assert_eq!(title, "Сборка проекта");
        assert_eq!(
            rig.fakes["cheap"].call_count(),
            1,
            "a chat is named once, not once per turn"
        );

        app.on_chat_title(target, title);
        assert_eq!(app.state.focused().title, "Сборка проекта");
        assert_eq!(
            crate::app::multichat::conversation_title(app.state.focused()),
            "Сборка проекта",
            "the chats list shows the service title"
        );
        app.persister.flush(Duration::from_secs(2)).await;
        let _ = std::fs::remove_file(
            crate::config::Config::sessions_dir()
                .join(format!("{}.json", app.state.focused().session_id)),
        );
    }

    #[test]
    fn a_disabled_gateway_answers_no_title_role() {
        let gateway: GatewayHandle = Arc::new(DisabledGateway::new());
        assert!(
            gateway
                .provider_for_role(ServiceRole::Title, CallSource::Role(ServiceRole::Title))
                .is_none()
        );
    }
}
