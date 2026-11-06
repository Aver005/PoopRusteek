//! Подставные адаптеры и сборка движка для тестов шлюза (`cfg(test)`); часы ручные, сети нет.

use super::backend::{
    AuthState, Backend, BackendError, BackendKind, CallContext, CallReport, Capabilities,
    EmitTracker, ModelInfo, ProbeReport, UsageSupport,
};
use super::clock::ManualClock;
use super::engine::{BackendFactory, Engine, EngineDeps, EnvOnlyResolver, SecretResolver};
use super::handle::{CallSource, Gateway};
use super::types::{Account, GatewayConfig, Role, ToolMode};
use crate::config::ProviderProtocol;
use crate::error::AppResult;
use crate::provider::failure::FailureClass;
use crate::provider::{ChatMessage, CompletionChunk, CompletionRequest, LLMProvider, Usage};
use async_trait::async_trait;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::sync::Notify;

/// Что адаптер сделает на очередном вызове.
pub enum Act {
    Reply(&'static str),
    /// Ответ, расход которого приходит отдельным чанком после `finish_reason`.
    ReplyWithUsage(&'static str, u32, u32),
    Fail(FailureClass),
    /// Текст ушёл, потом обрыв: fallback уже нельзя.
    FailAfterText(&'static str, FailureClass),
    /// Пустой живой сигнал, потом отказ: наружу ничего смыслового не ушло.
    HeartbeatThenFail(FailureClass),
    /// Ждать `release`, потом ответить.
    Hold,
}

pub struct CallSeen {
    pub model: String,
    pub tool_mode: ToolMode,
    pub messages: Vec<ChatMessage>,
}

pub struct Inner {
    caps: Capabilities,
    accepts_files: bool,
    pub deletes: AtomicUsize,
    pub adopts: AtomicUsize,
    acts: Mutex<VecDeque<Act>>,
    pub calls: Mutex<Vec<CallSeen>>,
    pub forks: AtomicUsize,
    pub discards: AtomicUsize,
    pub started: Notify,
    pub release: Notify,
}

/// Подставной адаптер: делится сценарием и счётчиками со своими форками.
pub struct FakeBackend {
    pub inner: Arc<Inner>,
}

pub fn caps() -> Capabilities {
    Capabilities {
        native_tools: false,
        pure_model: true,
        streaming: true,
        usage: UsageSupport::Reported,
        reports_limits: true,
        server_side_session: false,
        slow_start: false,
        reports_context_window: false,
    }
}

impl FakeBackend {
    pub fn new(acts: Vec<Act>) -> Arc<Self> {
        Self::with_caps(caps(), acts)
    }

    /// Адаптер, умеющий приложить файл (как DeepSeek).
    pub fn accepting_files(acts: Vec<Act>) -> Arc<Self> {
        Self::build(caps(), true, acts)
    }

    pub fn with_caps(caps: Capabilities, acts: Vec<Act>) -> Arc<Self> {
        Self::build(caps, false, acts)
    }

    fn build(caps: Capabilities, accepts_files: bool, acts: Vec<Act>) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(Inner {
                caps,
                accepts_files,
                deletes: AtomicUsize::new(0),
                adopts: AtomicUsize::new(0),
                acts: Mutex::new(acts.into_iter().collect()),
                calls: Mutex::new(Vec::new()),
                forks: AtomicUsize::new(0),
                discards: AtomicUsize::new(0),
                started: Notify::new(),
                release: Notify::new(),
            }),
        })
    }

    pub fn call_count(&self) -> usize {
        self.inner.calls.lock().unwrap().len()
    }

    pub fn forks(&self) -> usize {
        self.inner.forks.load(Ordering::SeqCst)
    }

    pub fn discards(&self) -> usize {
        self.inner.discards.load(Ordering::SeqCst)
    }

    pub fn deletes(&self) -> usize {
        self.inner.deletes.load(Ordering::SeqCst)
    }

    pub fn adopts(&self) -> usize {
        self.inner.adopts.load(Ordering::SeqCst)
    }

    /// Сообщения последнего запроса, как их увидел адаптер.
    pub fn last_messages(&self) -> Vec<ChatMessage> {
        self.inner
            .calls
            .lock()
            .unwrap()
            .last()
            .map(|c| c.messages.clone())
            .unwrap_or_default()
    }
}

fn text_chunk(text: &str) -> CompletionChunk {
    CompletionChunk {
        content: text.to_string(),
        ..CompletionChunk::default()
    }
}

fn finish_chunk() -> CompletionChunk {
    CompletionChunk {
        finish_reason: Some("stop".to_string()),
        ..CompletionChunk::default()
    }
}

fn usage(prompt: u32, completion: u32) -> Usage {
    Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: prompt + completion,
        ..Usage::default()
    }
}

fn error(class: FailureClass, tracker: &EmitTracker) -> BackendError {
    let status = match class {
        FailureClass::RateLimit { .. } => Some(429),
        FailureClass::Auth => Some(401),
        _ => None,
    };
    BackendError {
        class,
        status,
        message: "scripted failure".to_string(),
        emitted: tracker.emitted(),
        limits: None,
    }
}

fn report(prompt: u32, completion: u32) -> CallReport {
    CallReport {
        usage: Some(usage(prompt, completion)),
        usage_estimated: false,
        finish_reason: Some("stop".to_string()),
        ..CallReport::default()
    }
}

#[async_trait]
impl Backend for FakeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Api(ProviderProtocol::Openai)
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.caps.clone()
    }

    async fn call(
        &self,
        request: CompletionRequest,
        context: &CallContext,
        tx: EmitTracker,
    ) -> Result<CallReport, BackendError> {
        self.inner.calls.lock().unwrap().push(CallSeen {
            model: context.model.clone(),
            tool_mode: context.tool_mode,
            messages: request.messages.clone(),
        });
        let act = self
            .inner
            .acts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Act::Reply("ok"));
        match act {
            Act::Reply(text) => {
                tx.send(text_chunk(text));
                tx.send(finish_chunk());
                Ok(report(10, 2))
            }
            Act::ReplyWithUsage(text, prompt, completion) => {
                tx.send(text_chunk(text));
                tx.send(finish_chunk());
                tx.send(CompletionChunk {
                    usage: Some(usage(prompt, completion)),
                    ..CompletionChunk::default()
                });
                Ok(report(prompt, completion))
            }
            Act::Fail(class) => Err(error(class, &tx)),
            Act::FailAfterText(text, class) => {
                tx.send(text_chunk(text));
                Err(error(class, &tx))
            }
            Act::HeartbeatThenFail(class) => {
                tx.send(text_chunk(""));
                Err(error(class, &tx))
            }
            Act::Hold => {
                self.inner.started.notify_one();
                self.inner.release.notified().await;
                tx.send(text_chunk("held"));
                tx.send(finish_chunk());
                Ok(report(1, 1))
            }
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, BackendError> {
        Ok(vec![ModelInfo {
            id: "listed".to_string(),
            context_window: None,
            max_output: None,
            supports_tools: None,
        }])
    }

    async fn probe(&self) -> ProbeReport {
        ProbeReport {
            auth: AuthState::Ok,
            limits: None,
            version: None,
            detail: None,
            checked_at: SystemTime::now(),
        }
    }

    fn fork(&self) -> Arc<dyn Backend> {
        self.inner.forks.fetch_add(1, Ordering::SeqCst);
        Arc::new(Self {
            inner: Arc::clone(&self.inner),
        })
    }

    async fn discard_session(&self) {
        self.inner.discards.fetch_add(1, Ordering::SeqCst);
    }

    fn accepts_attachment(&self, _path: &std::path::Path) -> bool {
        self.inner.accepts_files
    }

    async fn delete_remote_session_by_id(&self, _session_id: &str) -> AppResult<()> {
        self.inner.deletes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn adopt_session(&self, _session_id: &str, _parent: Option<i64>) -> AppResult<()> {
        self.inner.adopts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Отдаёт заранее заготовленные адаптеры по id аккаунта.
pub struct FakeFactory {
    backends: HashMap<String, Arc<FakeBackend>>,
}

impl BackendFactory for FakeFactory {
    fn build(
        &self,
        account: &Account,
        _secrets: &dyn SecretResolver,
    ) -> Option<Result<Arc<dyn Backend>, String>> {
        let fake = self.backends.get(account.id.as_str())?;
        Some(Ok(Arc::new(FakeBackend {
            inner: Arc::clone(&fake.inner),
        })))
    }
}

pub struct Rig {
    pub engine: Engine,
    pub clock: Arc<ManualClock>,
    pub fakes: HashMap<String, Arc<FakeBackend>>,
}

/// Два аккаунта `a`, `b` и маршрут `main` по порядку; роль `main` смотрит на него.
pub const TWO_ACCOUNTS: &str = r#"
    [gateway]
    enabled = true

    [[accounts]]
    id = "a"
    provider = "custom"
    base_url = "http://a.invalid/v1"
    secret = "none"
    model = "model-a"

    [[accounts]]
    id = "b"
    provider = "custom"
    base_url = "http://b.invalid/v1"
    secret = "none"
    model = "model-b"

    [[routes]]
    id = "main"
    steps = [{ account = "a" }, { account = "b" }]

    [roles]
    main = "main"
"#;

pub fn parse(toml_text: &str) -> GatewayConfig {
    toml::from_str(toml_text).expect("test config parses")
}

pub fn rig(toml_text: &str, fakes: Vec<(&str, Arc<FakeBackend>)>) -> Rig {
    rig_in(toml_text, fakes, None, ManualClock::new(1_000_000))
}

pub fn rig_in(
    toml_text: &str,
    fakes: Vec<(&str, Arc<FakeBackend>)>,
    data_dir: Option<PathBuf>,
    clock: Arc<ManualClock>,
) -> Rig {
    let map: HashMap<String, Arc<FakeBackend>> = fakes
        .into_iter()
        .map(|(id, fake)| (id.to_string(), fake))
        .collect();
    let resolver: Arc<dyn SecretResolver> = Arc::new(EnvOnlyResolver);
    let deps = EngineDeps {
        data_dir,
        clock: clock.clone(),
        resolver,
        factories: vec![Arc::new(FakeFactory {
            backends: map.clone(),
        })],
        persist: None,
    };
    Rig {
        engine: Engine::new(parse(toml_text), deps),
        clock,
        fakes: map,
    }
}

impl Rig {
    pub fn main(&self) -> Arc<dyn LLMProvider> {
        self.engine
            .provider_for_role(Role::Main, CallSource::Chat)
            .expect("main route exists")
    }

    /// Отложить аккаунт на `secs` секунд, как ответ `429` с `Retry-After`.
    pub fn cool(&self, account: &str, secs: u64) {
        let account = super::types::AccountId::parse(account).expect("test account id");
        let runtime = self.engine.core().runtime(&account).expect("known account");
        let class = FailureClass::RateLimit {
            retry_after: Some(Duration::from_secs(secs)),
        };
        self.engine
            .core()
            .report_failure(&runtime, &account, &class, "scripted");
    }

    pub fn advance(&self, secs: u64) {
        self.clock.advance(Duration::from_secs(secs));
    }
}

pub fn request(messages: Vec<ChatMessage>) -> CompletionRequest {
    CompletionRequest {
        messages,
        tools: Vec::new(),
        model: "ignored".to_string(),
        temperature: 0.0,
        max_tokens: 16,
        stream: true,
    }
}

/// Один вопрос провайдеру: результат вызова и весь пришедший текст.
pub async fn ask(provider: &Arc<dyn LLMProvider>, text: &str) -> (AppResult<()>, String) {
    ask_with(provider, vec![ChatMessage::user(text)]).await
}

pub async fn ask_with(
    provider: &Arc<dyn LLMProvider>,
    messages: Vec<ChatMessage>,
) -> (AppResult<()>, String) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let result = provider.complete_stream(request(messages), tx).await;
    let mut text = String::new();
    while let Ok(chunk) = rx.try_recv() {
        text.push_str(&chunk.content);
    }
    (result, text)
}
