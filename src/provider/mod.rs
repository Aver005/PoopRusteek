pub mod anthropic_client;
pub mod anthropic_compat;
pub(crate) mod compat_client;
#[cfg(test)]
mod compat_http_tests;
pub mod deepseek;
pub mod failure;

/// Сколько провайдер ждёт следующего байта ответа. Строго короче сторожа
/// простоя хода (`agent::stream::STREAM_IDLE_TIMEOUT`), чтобы причину назвал он.
pub const PROVIDER_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);
#[cfg(test)]
pub mod fake;
pub mod gemini_client;
pub mod gemini_compat;
pub mod model_cache;
pub mod openai_client;
pub mod openai_compat;
pub mod pow;
pub mod prompt;
pub mod sse;
pub mod types;

/// Build the provider selected by the config: the active `/providers`
/// entry (an OpenAI-compatible endpoint) when one is set, otherwise the
/// built-in DeepSeek web client — or `None` when neither is usable (no
/// entry active and no DeepSeek token). The single construction point
/// behind `App::new`, provider resets, and onboarding.
pub fn build_provider(config: &crate::config::Config) -> Option<std::sync::Arc<dyn LLMProvider>> {
    if let Some(entry) = config.active_provider_entry() {
        return match build_entry_provider(entry) {
            Ok(provider) => Some(provider),
            Err(error) => {
                tracing::warn!("Failed to initialize provider '{}': {error}", entry.name);
                None
            }
        };
    }
    if config.provider.token.is_empty() {
        return None;
    }
    match deepseek::DeepseekProvider::new(
        &config.provider,
        config.agent.rate_limit_ms,
        config.agent.rate_limit_per_minute,
        config.agent.max_retries,
    ) {
        Ok(provider) => Some(std::sync::Arc::new(provider)),
        Err(error) => {
            tracing::warn!("Failed to initialize DeepSeek provider: {error}");
            None
        }
    }
}

/// Construct the client for one `/providers` entry, dispatching on its wire
/// protocol. Shared by [`build_provider`] (the TUI's active provider) and
/// the API server, which builds a per-request instance so a caller-chosen
/// sub-model (`entry/model` ids) can override the entry's configured one.
pub fn build_entry_provider(
    entry: &crate::config::ProviderEntry,
) -> crate::error::AppResult<std::sync::Arc<dyn LLMProvider>> {
    match entry.protocol {
        crate::config::ProviderProtocol::Openai => openai_client::OpenAiCompatProvider::new(entry)
            .map(|provider| std::sync::Arc::new(provider) as std::sync::Arc<dyn LLMProvider>),
        crate::config::ProviderProtocol::Anthropic => {
            anthropic_client::AnthropicCompatProvider::new(entry)
                .map(|provider| std::sync::Arc::new(provider) as std::sync::Arc<dyn LLMProvider>)
        }
        crate::config::ProviderProtocol::Gemini => gemini_client::GeminiProvider::new(entry)
            .map(|provider| std::sync::Arc::new(provider) as std::sync::Arc<dyn LLMProvider>),
    }
}

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Один вызов инструмента так, как его назвал провайдер.
///
/// `id` — идентификатор провайдера (`call_…` у OpenAI, `toolu_…` у
/// Anthropic): результат обязан сослаться на него дословно, иначе строгий
/// эндпоинт отвечает 400. У Gemini своих идентификаторов нет, там он
/// синтезируется локально и на провод не уходит.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    /// Что провайдер обязан получить обратно вместе с вызовом в истории
    /// (у Gemini — свой id и `thoughtSignature`). Для остальных пусто.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_state: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    /// Вызовы инструментов, объявленные этим сообщением ассистента.
    /// Пусто на промптовом пути: там вызовы приходят текстом. Сохраняется в
    /// файл сессии — иначе восстановленная история отдала бы результаты без
    /// объявивших их вызовов.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Полные пути двоичных файлов (PDF, картинки), которые провайдер
    /// прикладывает сам: текст из них извлекает сервер ([`LLMProvider::accepts_attachment`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip)]
    pub display_content: Option<String>,
    #[serde(skip)]
    pub tool_error: bool,
    /// UI-only notice (status lines, goal-cycle chrome). Rendered in the chat
    /// but filtered out of what is sent to the provider, so decorating the UI
    /// can never silently change what the model sees.
    #[serde(default, skip_serializing_if = "is_false")]
    pub ui_only: bool,
    #[serde(default)]
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub total_tokens: Option<u32>,
    #[serde(skip)]
    pub model: String,
    #[serde(skip)]
    pub status: Option<String>,
    #[serde(skip)]
    pub think_elapsed_secs: f64,
    #[serde(skip)]
    pub references_count: u32,
    #[serde(skip)]
    pub search_triggered: bool,
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn is_false(value: &bool) -> bool {
    !value
}

impl ChatMessage {
    /// Основа для всех конструкторов: роль и текст, остальное — по умолчанию.
    /// Поля, которые ставит только один вызывающий, добавляются через `..`.
    pub fn new(role: Role, content: &str) -> Self {
        Self {
            role,
            content: content.to_string(),
            tool_calls: Vec::new(),
            attachments: Vec::new(),
            name: None,
            tool_call_id: None,
            display_content: None,
            tool_error: false,
            ui_only: false,
            created_at: now_rfc3339(),
            total_tokens: None,
            model: String::new(),
            status: None,
            think_elapsed_secs: 0.0,
            references_count: 0,
            search_triggered: false,
        }
    }

    pub fn system(content: &str) -> Self {
        Self::new(Role::System, content)
    }

    pub fn user(content: &str) -> Self {
        Self::new(Role::User, content)
    }

    pub fn assistant(content: &str) -> Self {
        Self::new(Role::Assistant, content)
    }

    pub fn tool(tool_call_id: &str, content: &str) -> Self {
        Self {
            tool_call_id: Some(tool_call_id.to_string()),
            ..Self::new(Role::Tool, content)
        }
    }

    pub fn tool_with_display(
        tool_call_id: &str,
        tool_name: &str,
        content: &str,
        display: &str,
        is_error: bool,
    ) -> Self {
        Self {
            name: Some(tool_name.to_string()),
            tool_call_id: Some(tool_call_id.to_string()),
            display_content: Some(display.to_string()),
            tool_error: is_error,
            ..Self::new(Role::Tool, content)
        }
    }

    /// A system-styled notice shown in the chat but never sent to the model.
    /// Системная заметка из середины истории (провал хода, подсказка, таймер)
    /// текстом пользователя: поднятая в system prompt, она теряла место и копилась.
    pub fn inline_note(&self) -> String {
        format!("[Note]\n{}", self.content)
    }

    /// Результат инструмента с пометкой провала — там, где у протокола нет флага.
    pub fn labelled_tool_output(&self) -> String {
        if self.tool_error {
            format!("[tool failed]\n{}", self.content)
        } else {
            self.content.clone()
        }
    }

    pub fn ui_system(content: &str) -> Self {
        Self {
            ui_only: true,
            ..Self::system(content)
        }
    }

    /// A real user message whose chat rendering is a short label instead of
    /// the full content (e.g. goal-retry prompts).
    pub fn user_with_display(content: &str, display: &str) -> Self {
        Self {
            display_content: Some(display.to_string()),
            ..Self::user(content)
        }
    }

    pub fn visible_content(&self) -> &str {
        self.display_content.as_deref().unwrap_or(&self.content)
    }
}

/// A server-side chat session as shown in the `/delete` picker.
#[derive(Debug, Clone)]
pub struct RemoteSessionInfo {
    pub id: String,
    pub title: String,
    /// RFC 3339, when the provider reports it.
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AttachedFile {
    pub display_name: String,
    pub path: String,
    pub size: u64,
    pub is_image: bool,
}

pub fn estimate_tokens(text: &str) -> u32 {
    (text.chars().count() / 4).max(1) as u32
}

#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub messages: Vec<ChatMessage>,
    /// Инструменты, объявляемые провайдеру нативно. Пусто на промптовом
    /// пути — там они описаны текстом в системном промпте.
    pub tools: Vec<crate::tools::ToolDefinition>,
    pub model: String,
    pub temperature: f32,
    pub max_tokens: u32,
    // Providers pick streaming vs. non-streaming by which trait method the
    // caller invokes (`complete`/`complete_stream`); this flag only rides
    // into the OpenAI-compatible wire format (`request_to_openai`).
    pub stream: bool,
}

#[derive(Debug, Clone, Default)]
pub struct CompletionChunk {
    pub content: String,
    /// Вызовы, собранные протоколом из фрагментов стрима. Приходят только
    /// целиком: частичный JSON наверх не поднимается.
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    /// Настоящий расход, когда сервер его сообщил. Anthropic и Gemini кладут его
    /// в терминальный чанк, OpenAI — в отдельный чанк после него: кто читает
    /// только до `finish_reason`, его не увидит.
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone)]
pub struct CompletionResponse {
    pub content: String,
    // The TUI callers of `complete()` (src/acp/server.rs, app/goal.rs) only
    // read `.content`; `finish_reason`/`usage` ride through the
    // OpenAI-compatible boundary (`openai_compat::response_to_openai`).
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
}

/// Расход токенов. `prompt_tokens` — весь вход, включая кэш; разбивка ниже.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    /// Часть входа, прочитанная из кэша.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u32>,
    /// Часть входа, записанная в кэш.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u32>,
    /// Часть выхода, ушедшая на рассуждения.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
}

/// What a liveness check actually established about a server-side session.
///
/// Three states, not a `bool`, because the two failure kinds must lead to
/// different decisions: a session the API **refused by name** may have its
/// stored link erased, while a check that could not be made (network, an
/// expired token, a changed wire format) may not — treating "couldn't tell"
/// as "gone" is what silently branded healthy sessions broken and wiped their
/// provider ids off disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionLiveness {
    /// The provider answered about this session and it is usable.
    Alive,
    /// The provider answered about this session and refused it.
    Gone(String),
    /// The check itself did not happen. Carries why.
    Unknown(String),
}

// async-trait вешает `#[must_use]` на метод, чей `Pin<Box<dyn Future>>` и так
// must_use; clippy 2026-09 считает это ошибкой. Код наш тут ни при чём.
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait LLMProvider: Send + Sync {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> crate::error::AppResult<CompletionResponse>;
    async fn complete_stream(
        &self,
        request: CompletionRequest,
        tx: tokio::sync::mpsc::UnboundedSender<CompletionChunk>,
    ) -> crate::error::AppResult<()>;
    fn model(&self) -> &str;

    /// The model ids this provider can serve (for `/models`). OpenAI-style
    /// providers answer from `GET /models`; DeepSeek's web API has a fixed
    /// pair. Default: unsupported.
    async fn list_models(&self) -> crate::error::AppResult<Vec<String>> {
        Err(crate::error::AppError::Custom(
            "Model listing not supported by this provider".to_string(),
        ))
    }

    /// The active model's context window in tokens, when the provider's own
    /// catalogue reports one. `None` means "don't know" — never a guess, since
    /// compaction stays off rather than act on a wrong number (invariant 12).
    async fn context_window(&self) -> Option<u32> {
        None
    }

    /// True when the provider keeps conversation history on its own side and
    /// the client sends only the newest turn (DeepSeek's web API). Rewriting
    /// old local messages then changes nothing that will be sent again.
    fn keeps_server_side_history(&self) -> bool {
        false
    }

    /// Инструменты уходят провайдеру полем `tools`, а вызовы приходят
    /// структурой, а не текстом (`[[providers]] tools = "native"`).
    fn native_tools(&self) -> bool {
        false
    }

    /// Приложит ли провайдер этот файл из [`ChatMessage::attachments`] сам, извлекая
    /// из него текст (DeepSeek: PDF, картинки, docx). Иначе модель получает путь.
    fn accepts_attachment(&self, _path: &std::path::Path) -> bool {
        false
    }

    /// Budget tokens this provider has accumulated in its *current* session,
    /// when it keeps history server-side. `None` means "no idea, count the
    /// local history instead".
    fn session_tokens(&self) -> Option<u32> {
        None
    }

    /// Заголовки лимитов последнего ответа аккаунта (общие для всех форков).
    /// `None`: протокол их не отдаёт или ответа ещё не было.
    fn rate_limits(&self) -> Option<failure::RateLimitHeaders> {
        None
    }

    /// Где беседа остановилась в шлюзе (маршрут, аккаунт, модель). Без шлюза — `None`.
    fn route_state(&self) -> Option<crate::gateway::state::RouteState> {
        None
    }

    /// Подсказать шлюзу, где беседа была до перезапуска: чужую историю он развернёт в текст.
    fn seed_route_state(&self, _state: crate::gateway::state::RouteState) {}

    /// Create a sibling instance that shares this provider's configuration
    /// (and connection) but starts a **fresh session** — no shared
    /// `parent_message_id`/`session_id`. Each parallel conversation and
    /// sub-agent gets its own fork so concurrent turns never collide on
    /// session state.
    fn fork(&self) -> std::sync::Arc<dyn LLMProvider>;
    async fn reset(&self) -> crate::error::AppResult<()> {
        Ok(())
    }

    /// Delete this instance's server-side session, if one was created.
    /// Called when an ephemeral conversation (sidechat / sub-agent fork) is
    /// discarded, so one-shot runs don't pile up junk chats on the user's
    /// account. Main chats are never discarded — the user may want to
    /// continue them from the web UI. Default: nothing to clean.
    async fn discard_remote_session(&self) -> crate::error::AppResult<()> {
        Ok(())
    }

    /// List the account's server-side chat sessions (for `/delete`).
    /// Default: unsupported.
    async fn list_remote_sessions(&self) -> crate::error::AppResult<Vec<RemoteSessionInfo>> {
        Err(crate::error::AppError::Custom(
            "Remote session listing not supported by this provider".to_string(),
        ))
    }

    /// Delete a server-side session by its id (for `/delete`).
    /// Default: unsupported.
    async fn delete_remote_session_by_id(&self, _session_id: &str) -> crate::error::AppResult<()> {
        Err(crate::error::AppError::Custom(
            "Remote session deletion not supported by this provider".to_string(),
        ))
    }

    async fn fetch_remote_session_messages(
        &self,
        _session_id: &str,
    ) -> crate::error::AppResult<Vec<ChatMessage>> {
        Err(crate::error::AppError::Custom(
            "Remote session fetching not supported by this provider".to_string(),
        ))
    }

    /// This provider's live server-side session identity (session id + last
    /// known parent message id), if a turn has established one since the
    /// last `reset()`. Read synchronously (in-memory only, no network) so
    /// it can be sampled on every auto-save without blocking. `None` before
    /// the first turn of a session, or right after a reset.
    fn session_identity(&self) -> Option<(String, Option<i64>)> {
        None
    }

    /// What could be learned about a previously-established server-side
    /// session. Default: nothing — a provider with no server-side sessions
    /// has nothing to check, and saying [`SessionLiveness::Gone`] would
    /// invite callers to destroy a link it knows nothing about.
    async fn check_session(&self, _session_id: &str) -> SessionLiveness {
        SessionLiveness::Unknown("this provider does not track server-side sessions".to_string())
    }

    /// Adopt a previously-known server-side session id + parent message id
    /// as this provider's active thread, so the next turn continues it
    /// instead of creating a new remote session. Callers should confirm the
    /// session is still alive (`session_is_alive`) first.
    async fn adopt_session(
        &self,
        _session_id: &str,
        _parent_message_id: Option<i64>,
    ) -> crate::error::AppResult<()> {
        Ok(())
    }
}

/// Удалить серверную сессию одноразового форка, чем бы ни кончилась работа:
/// на обычном пути — дождавшись (со сроком), при отмене — в фоне из `Drop`.
pub struct SessionDiscard(Option<std::sync::Arc<dyn LLMProvider>>);

impl SessionDiscard {
    /// Сколько ждать удаления на обычном пути.
    const GRACE: std::time::Duration = std::time::Duration::from_secs(5);

    pub fn new(provider: std::sync::Arc<dyn LLMProvider>) -> Self {
        Self(Some(provider))
    }

    pub async fn now(mut self) {
        if let Some(provider) = self.0.take() {
            match tokio::time::timeout(Self::GRACE, discard_logged(provider)).await {
                Ok(()) => {}
                Err(_) => tracing::debug!("remote session discard took over {:?}", Self::GRACE),
            }
        }
    }
}

impl Drop for SessionDiscard {
    fn drop(&mut self) {
        let Some(provider) = self.0.take() else {
            return;
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(discard_logged(provider));
        }
    }
}

/// Сбой удаления не мешает работе, но след в журнале нужен.
async fn discard_logged(provider: std::sync::Arc<dyn LLMProvider>) {
    if let Err(error) = provider.discard_remote_session().await {
        tracing::debug!("remote session discard failed: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Файл сессии, записанный до появления `tool_calls`, обязан читаться:
    /// поле необязательное. Иначе обновление обнулило бы всю историю.
    #[test]
    fn a_session_message_without_tool_calls_still_loads() {
        let old = r#"{"role":"assistant","content":"hi","created_at":"2026-01-01T00:00:00Z"}"#;
        let message: ChatMessage = serde_json::from_str(old).expect("old shape must load");
        assert!(message.tool_calls.is_empty());
    }

    /// …и пустой список не пишется в файл, иначе каждая старая сессия
    /// распухла бы на ровном месте при первом же пересохранении.
    #[test]
    fn an_empty_tool_call_list_is_not_serialized() {
        let json = serde_json::to_string(&ChatMessage::assistant("hi")).unwrap();
        assert!(!json.contains("tool_calls"), "{json}");
    }

    /// А непустой — пишется и читается обратно дословно: результат обязан
    /// сослаться на тот же `id`, что объявил вызов.
    #[test]
    fn tool_calls_round_trip_through_a_session_file() {
        let mut message = ChatMessage::assistant("");
        message.tool_calls = vec![ToolCall {
            id: "call_1".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "Cargo.toml"}),
            provider_state: None,
        }];
        let json = serde_json::to_string(&message).unwrap();
        let back: ChatMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tool_calls, message.tool_calls);
    }
}
