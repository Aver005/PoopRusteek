//! Адаптер аккаунта с ключом: оборачивает существующие HTTP-клиенты (OpenAI, Anthropic,
//! Gemini) и общий разбор потока `drive_provider`, которым пользуется и DeepSeek.
//!
//! Модель берётся из `CallContext`, а не из запроса: клиент хранит модель при создании,
//! поэтому на каждую модель аккаунта заводится свой клиент (кэш в `Shared::clients`).

use super::backend::{
    AuthState, Backend, BackendError, BackendKind, CallContext, CallReport, Capabilities,
    EmitTracker, ModelInfo, PROBE_TIMEOUT, ProbeReport, UsageSupport, is_meaningful,
};
use super::handle::LimitSnapshot;
use super::types::{Account, AccountId, AccountProvider, ToolMode};
use crate::config::{ProviderEntry, ProviderProtocol, ToolProtocol};
use crate::provider::failure::FailureClass;
use crate::provider::{
    CompletionChunk, CompletionRequest, LLMProvider, Usage, build_entry_provider, estimate_tokens,
};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com/v1";
const GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

// ── Общий разбор потока ─────────────────────────────────────────────────

/// Что осталось от удачного прогона потока.
pub(super) struct Driven {
    pub usage: Usage,
    pub usage_estimated: bool,
    pub finish_reason: Option<String>,
    pub ttft: Option<Duration>,
    pub elapsed: Duration,
}

/// Грубая оценка входа, когда сервер расход не назвал (помечается `~`).
pub(super) fn estimate_prompt_tokens(request: &CompletionRequest) -> u32 {
    request
        .messages
        .iter()
        .map(|message| estimate_tokens(&message.content))
        .sum()
}

/// Прогнать `complete_stream` провайдера до конца, переслав чанки в `tracker`.
/// Поток читается целиком, даже если получатель ушёл: чанк расхода OpenAI идёт после
/// `finish_reason`, и без дочитывания его не увидеть.
#[allow(
    clippy::result_large_err,
    reason = "`BackendError` отдаётся трейтом `Backend` по значению (контракт GW-0)"
)]
pub(super) async fn drive_provider(
    provider: &Arc<dyn LLMProvider>,
    request: CompletionRequest,
    tracker: &EmitTracker,
    account: &AccountId,
) -> Result<Driven, BackendError> {
    let started = Instant::now();
    let prompt_estimate = estimate_prompt_tokens(&request);
    let (inner_tx, mut inner_rx) = tokio::sync::mpsc::unbounded_channel::<CompletionChunk>();
    let drive = provider.complete_stream(request, inner_tx);
    let forward = async {
        let mut usage: Option<Usage> = None;
        let mut finish_reason: Option<String> = None;
        let mut ttft: Option<Duration> = None;
        let mut reply_chars = 0usize;
        let mut receiver_alive = true;
        while let Some(chunk) = inner_rx.recv().await {
            if chunk.usage.is_some() {
                // Последнее слово сервера: расход приходит один раз, но не суммируется.
                usage = chunk.usage.clone();
            }
            if chunk.finish_reason.is_some() {
                finish_reason = chunk.finish_reason.clone();
            }
            if ttft.is_none() && is_meaningful(&chunk) {
                ttft = Some(started.elapsed());
            }
            reply_chars += chunk.content.chars().count();
            if receiver_alive {
                receiver_alive = tracker.send(chunk);
            }
        }
        (usage, finish_reason, ttft, reply_chars)
    };
    let (result, (usage, finish_reason, ttft, reply_chars)) = tokio::join!(drive, forward);

    if let Err(error) = result {
        return Err(BackendError::from_app(account, &error, tracker.emitted()));
    }
    if !tracker.emitted() && finish_reason.is_none() {
        // Пустой поток без конца: на другом аккаунте может быть лучше.
        return Err(BackendError {
            class: FailureClass::Server,
            status: None,
            message: "the stream ended without a reply".to_string(),
            emitted: false,
            limits: None,
        });
    }
    let (usage, usage_estimated) = match usage {
        Some(usage) => (usage, false),
        None => {
            let completion = (reply_chars / 4) as u32;
            (
                Usage {
                    prompt_tokens: prompt_estimate,
                    completion_tokens: completion,
                    total_tokens: prompt_estimate + completion,
                    ..Usage::default()
                },
                true,
            )
        }
    };
    Ok(Driven {
        usage,
        usage_estimated,
        finish_reason,
        ttft,
        elapsed: started.elapsed(),
    })
}

// ── API-аккаунт ─────────────────────────────────────────────────────────

struct Shared {
    account: AccountId,
    entry: ProviderEntry,
    native_tools: bool,
    clients: Mutex<HashMap<String, Arc<dyn LLMProvider>>>,
    limits: Mutex<Option<LimitSnapshot>>,
}

pub struct ApiBackend {
    shared: Arc<Shared>,
}

impl ApiBackend {
    /// `api_key` уже разрешён из секрета снаружи; адаптер сам секреты не читает.
    pub fn new(account: &Account, api_key: Option<String>) -> Result<Self, String> {
        let protocol = account
            .provider
            .protocol()
            .ok_or_else(|| format!("account '{}' is not an API account", account.id))?;
        let base_url = account
            .base_url
            .clone()
            .or_else(|| default_base_url(account.provider).map(str::to_string))
            .ok_or_else(|| format!("account '{}' needs a base_url", account.id))?;
        let entry = ProviderEntry {
            name: account.id.to_string(),
            base_url,
            api_key,
            model: account.model.clone().unwrap_or_default(),
            protocol,
            tools: account.tool_protocol(),
        };
        Ok(Self {
            shared: Arc::new(Shared {
                account: account.id.clone(),
                native_tools: entry.tools == ToolProtocol::Native,
                entry,
                clients: Mutex::new(HashMap::new()),
                limits: Mutex::new(None),
            }),
        })
    }

    /// Клиент под модель; пустая модель — модель по умолчанию из аккаунта.
    #[allow(
        clippy::result_large_err,
        reason = "`BackendError` отдаётся трейтом `Backend` по значению (контракт GW-0)"
    )]
    fn client_for(&self, model: &str) -> Result<Arc<dyn LLMProvider>, BackendError> {
        let mut clients = self
            .shared
            .clients
            .lock()
            .map_err(|_| internal("client cache lock poisoned"))?;
        if let Some(client) = clients.get(model) {
            return Ok(Arc::clone(client));
        }
        let mut entry = self.shared.entry.clone();
        if !model.is_empty() {
            entry.model = model.to_string();
        }
        let client = build_entry_provider(&entry)
            .map_err(|error| BackendError::from_app(&self.shared.account, &error, false))?;
        clients.insert(model.to_string(), Arc::clone(&client));
        Ok(client)
    }

    fn remember_limits(&self, client: &Arc<dyn LLMProvider>) -> Option<LimitSnapshot> {
        let headers = client.rate_limits()?;
        let snapshot = LimitSnapshot::from_headers(self.shared.account.clone(), &headers);
        if let Ok(mut slot) = self.shared.limits.lock() {
            *slot = Some(snapshot.clone());
        }
        Some(snapshot)
    }
}

fn internal(message: &str) -> BackendError {
    BackendError {
        class: FailureClass::Server,
        status: None,
        message: message.to_string(),
        emitted: false,
        limits: None,
    }
}

fn default_base_url(provider: AccountProvider) -> Option<&'static str> {
    match provider {
        AccountProvider::Openai => Some(OPENAI_BASE_URL),
        AccountProvider::Anthropic => Some(ANTHROPIC_BASE_URL),
        AccountProvider::Gemini => Some(GEMINI_BASE_URL),
        _ => None,
    }
}

#[async_trait]
impl Backend for ApiBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Api(self.shared.entry.protocol)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            native_tools: self.shared.native_tools,
            pure_model: true,
            streaming: true,
            usage: UsageSupport::Reported,
            reports_limits: matches!(
                self.shared.entry.protocol,
                ProviderProtocol::Openai | ProviderProtocol::Anthropic
            ),
            server_side_session: false,
            slow_start: false,
            reports_context_window: false,
        }
    }

    async fn call(
        &self,
        mut request: CompletionRequest,
        context: &CallContext,
        tx: EmitTracker,
    ) -> Result<CallReport, BackendError> {
        let client = self.client_for(&context.model)?;
        request.model = context.model.clone();
        if context.tool_mode == ToolMode::Prompt {
            // В промптовом режиме описание инструментов уже в системном промпте.
            request.tools.clear();
        }
        let driven = drive_provider(&client, request, &tx, &context.account).await;
        let limits = self.remember_limits(&client);
        match driven {
            Ok(driven) => Ok(CallReport {
                usage: Some(driven.usage),
                usage_estimated: driven.usage_estimated,
                limits,
                finish_reason: driven.finish_reason,
                context_window: None,
                ttft: driven.ttft,
                elapsed: driven.elapsed,
                upstream_id: None,
            }),
            Err(mut error) => {
                error.limits = error.limits.or(limits);
                Err(error)
            }
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, BackendError> {
        let client = self.client_for("")?;
        let ids = client
            .list_models()
            .await
            .map_err(|error| BackendError::from_app(&self.shared.account, &error, false))?;
        Ok(ids
            .into_iter()
            .map(|id| ModelInfo {
                id,
                context_window: None,
                max_output: None,
                supports_tools: None,
            })
            .collect())
    }

    async fn probe(&self) -> ProbeReport {
        let checked_at = SystemTime::now();
        let listed = tokio::time::timeout(PROBE_TIMEOUT, self.list_models()).await;
        let (auth, detail) = match listed {
            Ok(Ok(_)) => (AuthState::Ok, None),
            Ok(Err(error)) => match error.class {
                FailureClass::Auth | FailureClass::AccountBlocked => {
                    (AuthState::NeedsLogin, Some(error.message))
                }
                _ => (AuthState::Unknown, Some(error.message)),
            },
            Err(_) => (AuthState::Unknown, Some("probe timed out".to_string())),
        };
        let limits = self.shared.limits.lock().ok().and_then(|slot| slot.clone());
        ProbeReport {
            auth,
            limits,
            version: None,
            detail,
            checked_at,
        }
    }

    /// Без состояния на сервере: форк делит клиенты и лимиты аккаунта.
    fn fork(&self) -> Arc<dyn Backend> {
        Arc::new(Self {
            shared: Arc::clone(&self.shared),
        })
    }

    async fn context_window(&self, model: &str) -> Option<u32> {
        self.client_for(model).ok()?.context_window().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ChatMessage;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Готовый ответ сервера: статус, заголовки, тело.
    pub(crate) struct Reply {
        pub status: &'static str,
        pub headers: Vec<(&'static str, &'static str)>,
        pub body: String,
    }

    /// Отвечает по очереди; возвращает базовый адрес и тела полученных запросов.
    pub(crate) async fn serve(replies: Vec<Reply>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        tokio::spawn(async move {
            for reply in replies {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buffer.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buffer).to_string();
                    if let Some(split) = text.find("\r\n\r\n") {
                        let length = text[..split]
                            .to_ascii_lowercase()
                            .lines()
                            .find_map(|line| {
                                line.strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if buffer.len() >= split + 4 + length {
                            log.lock().unwrap().push(text[split + 4..].to_string());
                            break;
                        }
                    }
                }
                let mut head = format!(
                    "HTTP/1.1 {}\r\ncontent-length: {}\r\nconnection: close\r\n",
                    reply.status,
                    reply.body.len()
                );
                for (name, value) in &reply.headers {
                    head.push_str(&format!("{name}: {value}\r\n"));
                }
                head.push_str("\r\n");
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(reply.body.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        (base, seen)
    }

    fn account(base_url: &str) -> Account {
        let mut account = Account::new(AccountId::parse("api").unwrap(), AccountProvider::Openai);
        account.base_url = Some(base_url.to_string());
        account.model = Some("default-model".to_string());
        account
    }

    fn request() -> CompletionRequest {
        CompletionRequest {
            messages: vec![ChatMessage::user("hi")],
            tools: Vec::new(),
            model: "ignored".to_string(),
            temperature: 0.0,
            max_tokens: 16,
            stream: true,
        }
    }

    fn context(model: &str) -> CallContext {
        CallContext {
            account: AccountId::parse("api").unwrap(),
            model: model.to_string(),
            tool_mode: ToolMode::Prompt,
        }
    }

    fn tracker() -> (
        EmitTracker,
        tokio::sync::mpsc::UnboundedReceiver<CompletionChunk>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (EmitTracker::new(tx), rx)
    }

    const HEAD: &str = r#"{"id":"1","object":"chat.completion.chunk","created":0,"model":"m","#;
    const USAGE_CHUNK: &str = r#"data: {"id":"1","object":"chat.completion.chunk","created":0,"model":"m","choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15}}

"#;

    fn stream(with_usage: bool) -> String {
        format!(
            "data: {HEAD}\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"hello\"}},\"finish_reason\":null}}]}}

             data: {HEAD}\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}

             {}data: [DONE]

",
            if with_usage { USAGE_CHUNK } else { "" }
        )
    }

    fn sse(body: &str) -> Reply {
        Reply {
            status: "200 OK",
            headers: vec![("content-type", "text/event-stream")],
            body: body.to_string(),
        }
    }

    /// Расход приходит отдельным чанком после `finish_reason`, и он считается один раз.
    #[tokio::test]
    async fn a_call_reads_past_the_finish_to_the_reported_usage_and_uses_the_context_model() {
        let (base, seen) = serve(vec![sse(&stream(true))]).await;
        let backend = ApiBackend::new(&account(&base), None).unwrap();
        let (tracker, mut rx) = tracker();
        let report = backend
            .call(request(), &context("chosen-model"), tracker.clone())
            .await
            .expect("call succeeds");
        let usage = report.usage.unwrap();
        assert_eq!((usage.prompt_tokens, usage.completion_tokens), (12, 3));
        assert!(!report.usage_estimated, "the server named the numbers");
        assert_eq!(report.finish_reason.as_deref(), Some("stop"));
        assert!(tracker.emitted());
        let mut text = String::new();
        while let Ok(chunk) = rx.try_recv() {
            text.push_str(&chunk.content);
        }
        assert_eq!(text, "hello");
        let body = seen.lock().unwrap()[0].clone();
        assert!(
            body.contains("\"model\":\"chosen-model\""),
            "the context model, not request.model: {body}"
        );
    }

    #[tokio::test]
    async fn a_missing_usage_is_estimated_and_marked() {
        let (base, _) = serve(vec![sse(&stream(false))]).await;
        let backend = ApiBackend::new(&account(&base), None).unwrap();
        let (tracker, _rx) = tracker();
        let report = backend
            .call(request(), &context(""), tracker)
            .await
            .unwrap();
        assert!(report.usage_estimated);
        assert!(report.usage.unwrap().total_tokens > 0);
    }

    /// Отрицательная сторона: 429 с `Retry-After` приходит типизированным и до вывода.
    #[tokio::test]
    async fn a_429_arrives_typed_with_its_wait_and_nothing_emitted() {
        let (base, _) = serve(vec![Reply {
            status: "429 Too Many Requests",
            headers: vec![("content-type", "application/json"), ("retry-after", "42")],
            body: r#"{"error":{"message":"slow down","type":"rate_limit"}}"#.to_string(),
        }])
        .await;
        let backend = ApiBackend::new(&account(&base), None).unwrap();
        let (tracker, _rx) = tracker();
        let error = backend
            .call(request(), &context(""), tracker)
            .await
            .expect_err("rate limited");
        assert_eq!(error.status, Some(429));
        assert_eq!(
            error.class,
            FailureClass::RateLimit {
                retry_after: Some(Duration::from_secs(42))
            }
        );
        assert!(!error.emitted, "nothing was shown, a fallback is allowed");
    }

    #[tokio::test]
    async fn an_empty_stream_without_an_end_is_a_server_failure_not_a_reply() {
        let (base, _) = serve(vec![sse("data: [DONE]\n\n")]).await;
        let backend = ApiBackend::new(&account(&base), None).unwrap();
        let (tracker, _rx) = tracker();
        let error = backend
            .call(request(), &context(""), tracker)
            .await
            .expect_err("nothing came");
        assert_eq!(error.class, FailureClass::Server);
        assert!(!error.emitted);
    }

    #[test]
    fn the_adapter_reflects_the_tool_protocol_and_refuses_non_api_accounts() {
        let mut native = account("http://x/v1");
        native.tools = ToolProtocol::Native;
        let backend = ApiBackend::new(&native, None).unwrap();
        assert!(backend.capabilities().native_tools);
        assert!(
            backend
                .capabilities()
                .usable_for_tool_turn(ToolMode::Native)
        );
        let prompt = ApiBackend::new(&account("http://x/v1"), None).unwrap();
        assert!(!prompt.capabilities().native_tools);
        assert!(
            !prompt.capabilities().usable_for_tool_turn(ToolMode::Native),
            "a prompt-only account cannot serve a native turn"
        );
        let cli = Account::new(AccountId::parse("c").unwrap(), AccountProvider::CliClaude);
        assert!(ApiBackend::new(&cli, None).is_err());
        let custom = Account::new(AccountId::parse("k").unwrap(), AccountProvider::Custom);
        assert!(
            ApiBackend::new(&custom, None).is_err(),
            "a custom server must say where it lives"
        );
        let openai = Account::new(AccountId::parse("o").unwrap(), AccountProvider::Openai);
        assert!(
            ApiBackend::new(&openai, None).is_ok(),
            "the hosted default is known"
        );
    }

    #[tokio::test]
    async fn forks_share_clients_and_the_model_cache_has_one_client_per_model() {
        let backend = ApiBackend::new(&account("http://127.0.0.1:9/v1"), None).unwrap();
        let fork = backend.fork();
        drop(fork);
        let first = backend.client_for("m1").unwrap();
        let again = backend.client_for("m1").unwrap();
        assert!(Arc::ptr_eq(&first, &again), "one client per model");
        let other = backend.client_for("m2").unwrap();
        assert!(!Arc::ptr_eq(&first, &other));
    }
}
