//! Шлюз, который ещё собирается.
//!
//! Движок читает секреты (хранилище, `cmd:`, файлы) и создаёт адаптеры; это блокирует, и на цикле
//! событий этого делать нельзя. Поэтому приложение стартует с `PendingGateway`, собирает движок на
//! `spawn_blocking` и по готовности вызывает `fulfil`. Пока идёт сборка, ввод и панели живы, а запрос,
//! отправленный в этот момент, ждёт готовности (`PendingProvider`), а не падает.

use super::backend::Backend;
use super::handle::{
    BuildState, CallSource, ConfigEdit, Decision, DecisionQuery, DisabledGateway, EditError,
    Gateway, GatewayEvent, GatewayHandle, GatewaySnapshot, Ledger, ResolveError, Why,
};
use super::state::RouteState;
use super::types::{AccountId, GatewayConfig, Problem, Role, ToolMode};
use crate::error::{AppError, AppResult};
use crate::provider::{
    ChatMessage, CompletionChunk, CompletionRequest, CompletionResponse, LLMProvider,
    RemoteSessionInfo, SessionLiveness,
};
use async_trait::async_trait;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;
use tokio::sync::{broadcast, mpsc::UnboundedSender, watch};

#[derive(Clone)]
enum Phase {
    Building,
    Ready(GatewayHandle),
    Failed(String),
}

pub struct PendingGateway {
    /// Конфиг, как он будет собран: из него берутся подписи и режим инструментов до готовности.
    config: GatewayConfig,
    phase: watch::Sender<Phase>,
    events: broadcast::Sender<GatewayEvent>,
}

impl PendingGateway {
    pub fn new(config: GatewayConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            phase: watch::channel(Phase::Building).0,
            events: broadcast::channel(64).0,
        })
    }

    /// Собранный движок: ждущие запросы продолжаются, события пересылаются подписчикам.
    pub fn fulfil(&self, gateway: GatewayHandle) {
        if gateway.is_enabled()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let mut inner = gateway.subscribe();
            let outer = self.events.clone();
            runtime.spawn(async move {
                loop {
                    match inner.recv().await {
                        Ok(event) => {
                            let _ = outer.send(event);
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }
        self.phase.send_replace(Phase::Ready(gateway));
    }

    /// Сборка не удалась (паника задачи): ждущие запросы получают понятную ошибку.
    pub fn fail(&self, reason: String) {
        self.phase.send_replace(Phase::Failed(reason));
    }

    fn ready(&self) -> Option<GatewayHandle> {
        match &*self.phase.borrow() {
            Phase::Ready(gateway) => Some(Arc::clone(gateway)),
            _ => None,
        }
    }

    pub fn is_building(&self) -> bool {
        matches!(&*self.phase.borrow(), Phase::Building)
    }

    fn build_state(&self) -> BuildState {
        match &*self.phase.borrow() {
            Phase::Ready(_) => BuildState::Ready,
            Phase::Building => BuildState::Starting,
            Phase::Failed(reason) => BuildState::Failed(reason.clone()),
        }
    }

    fn provider(self: &Arc<Self>, role: Role, source: CallSource) -> Option<PendingProvider> {
        let route = self.config.roles.route_for(role)?;
        let native = self
            .config
            .route(route)
            .and_then(|route| route.tool_mode(&self.config.accounts).ok())
            == Some(ToolMode::Native);
        Some(PendingProvider {
            gateway: Arc::clone(self),
            role,
            source,
            label: format!("auto/{route}"),
            native,
            inner: OnceLock::new(),
            seed: Mutex::new(None),
        })
    }
}

impl PendingGateway {
    fn is_enabled(&self) -> bool {
        true
    }

    fn snapshot(&self) -> GatewaySnapshot {
        if let Some(gateway) = self.ready() {
            return gateway.snapshot();
        }
        let message = match &*self.phase.borrow() {
            Phase::Failed(reason) => format!("the gateway failed to start: {reason}"),
            _ => "the gateway is starting: accounts are being connected".to_string(),
        };
        GatewaySnapshot {
            settings: self.config.gateway.clone(),
            accounts: Vec::new(),
            routes: self.config.routes.clone(),
            roles: self.config.roles.clone(),
            recent: Vec::new(),
            problems: vec![Problem {
                severity: super::types::Severity::Warning,
                path: "gateway".to_string(),
                message,
            }],
            generated_at: SystemTime::now(),
        }
    }

    fn provider_for_model_id(
        &self,
        id: &str,
        source: CallSource,
    ) -> Result<Arc<dyn LLMProvider>, ResolveError> {
        match self.ready() {
            Some(gateway) => gateway.provider_for_model_id(id, source),
            None => Err(match self.build_state() {
                BuildState::Failed(reason) => ResolveError::Failed(reason),
                _ => ResolveError::Starting,
            }),
        }
    }

    fn decide(&self, query: &DecisionQuery) -> Decision {
        if let Some(gateway) = self.ready() {
            return gateway.decide(query);
        }
        Decision {
            route: None,
            target: None,
            tool_mode: query.tool_mode,
            why: match self.build_state() {
                BuildState::Failed(reason) => Why::ConfigInvalid {
                    message: format!("the gateway failed to start: {reason}"),
                },
                _ => Why::Starting,
            },
            skipped: Vec::new(),
            snapshot_age: None,
            retry_at: None,
            at: SystemTime::now(),
        }
    }

    fn apply_edit(&self, edit: ConfigEdit) -> Result<(), EditError> {
        match self.ready() {
            Some(gateway) => gateway.apply_edit(edit),
            None => Err(EditError::Storage(
                "the gateway is still starting, try again in a moment".to_string(),
            )),
        }
    }

    fn subscribe(&self) -> broadcast::Receiver<GatewayEvent> {
        self.events.subscribe()
    }

    fn ledger(&self) -> Arc<dyn Ledger> {
        match self.ready() {
            Some(gateway) => gateway.ledger(),
            None => DisabledGateway::new().ledger(),
        }
    }

    fn backend_for(&self, account: &AccountId) -> Option<Arc<dyn Backend>> {
        self.ready()?.backend_for(account)
    }
}

/// Ручка, которой приложение пользуется как `GatewayHandle`: знает свою `Arc`, поэтому умеет
/// отдавать ждущие провайдеры. `PendingGateway` сам по себе провайдеров не раздаёт.
pub struct PendingHandle(pub Arc<PendingGateway>);

impl Gateway for PendingHandle {
    fn is_enabled(&self) -> bool {
        self.0.is_enabled()
    }

    fn build_state(&self) -> BuildState {
        match self.0.ready() {
            Some(gateway) => gateway.build_state(),
            None => self.0.build_state(),
        }
    }

    fn snapshot(&self) -> GatewaySnapshot {
        self.0.snapshot()
    }

    fn provider_for_role(&self, role: Role, source: CallSource) -> Option<Arc<dyn LLMProvider>> {
        if let Some(gateway) = self.0.ready() {
            return gateway.provider_for_role(role, source);
        }
        self.0
            .provider(role, source)
            .map(|provider| Arc::new(provider) as Arc<dyn LLMProvider>)
    }

    fn provider_for_model_id(
        &self,
        id: &str,
        source: CallSource,
    ) -> Result<Arc<dyn LLMProvider>, ResolveError> {
        self.0.provider_for_model_id(id, source)
    }

    fn decide(&self, query: &DecisionQuery) -> Decision {
        self.0.decide(query)
    }

    fn apply_edit(&self, edit: ConfigEdit) -> Result<(), EditError> {
        self.0.apply_edit(edit)
    }

    fn subscribe(&self) -> broadcast::Receiver<GatewayEvent> {
        self.0.subscribe()
    }

    fn ledger(&self) -> Arc<dyn Ledger> {
        self.0.ledger()
    }

    fn backend_for(&self, account: &AccountId) -> Option<Arc<dyn Backend>> {
        self.0.backend_for(account)
    }
}

/// Провайдер роли, пока движок собирается: вызов ждёт готовности и дальше идёт настоящему
/// `RoutedProvider`. Всё синхронное отвечает из конфига (подпись, режим инструментов).
struct PendingProvider {
    gateway: Arc<PendingGateway>,
    role: Role,
    source: CallSource,
    label: String,
    native: bool,
    inner: OnceLock<Arc<dyn LLMProvider>>,
    /// Где беседа была до перезапуска: передаётся настоящему провайдеру, когда он появится.
    seed: Mutex<Option<RouteState>>,
}

impl PendingProvider {
    async fn real(&self) -> AppResult<&Arc<dyn LLMProvider>> {
        if let Some(inner) = self.inner.get() {
            return Ok(inner);
        }
        let mut phase = self.gateway.phase.subscribe();
        loop {
            let current = phase.borrow_and_update().clone();
            match current {
                Phase::Ready(gateway) => {
                    let provider = gateway
                        .provider_for_role(self.role, self.source)
                        .ok_or_else(|| {
                            AppError::Provider(format!(
                                "the gateway has no route for the {:?} role",
                                self.role
                            ))
                        })?;
                    if let Some(seed) = self.seed.lock().ok().and_then(|mut s| s.take()) {
                        provider.seed_route_state(seed);
                    }
                    let _ = self.inner.set(provider);
                    return self
                        .inner
                        .get()
                        .ok_or_else(|| AppError::Provider("gateway provider unavailable".into()));
                }
                Phase::Failed(reason) => {
                    return Err(AppError::Provider(format!(
                        "the gateway failed to start: {reason}"
                    )));
                }
                Phase::Building => {
                    if phase.changed().await.is_err() {
                        return Err(AppError::Provider("the gateway was dropped".into()));
                    }
                }
            }
        }
    }
}

#[async_trait]
impl LLMProvider for PendingProvider {
    async fn complete(&self, request: CompletionRequest) -> AppResult<CompletionResponse> {
        self.real().await?.complete(request).await
    }

    async fn complete_stream(
        &self,
        request: CompletionRequest,
        tx: UnboundedSender<CompletionChunk>,
    ) -> AppResult<()> {
        self.real().await?.complete_stream(request, tx).await
    }

    fn model(&self) -> &str {
        self.inner
            .get()
            .map_or(self.label.as_str(), |inner| inner.model())
    }

    async fn list_models(&self) -> AppResult<Vec<String>> {
        self.real().await?.list_models().await
    }

    async fn context_window(&self) -> Option<u32> {
        match self.real().await {
            Ok(inner) => inner.context_window().await,
            Err(_) => None,
        }
    }

    fn keeps_server_side_history(&self) -> bool {
        self.inner
            .get()
            .is_some_and(|inner| inner.keeps_server_side_history())
    }

    fn native_tools(&self) -> bool {
        self.inner
            .get()
            .map_or(self.native, |inner| inner.native_tools())
    }

    fn accepts_attachment(&self, path: &std::path::Path) -> bool {
        self.inner
            .get()
            .is_some_and(|inner| inner.accepts_attachment(path))
    }

    fn session_tokens(&self) -> Option<u32> {
        self.inner.get()?.session_tokens()
    }

    fn rate_limits(&self) -> Option<crate::provider::failure::RateLimitHeaders> {
        self.inner.get()?.rate_limits()
    }

    fn route_state(&self) -> Option<RouteState> {
        match self.inner.get() {
            Some(inner) => inner.route_state(),
            None => self.seed.lock().ok().and_then(|seed| seed.clone()),
        }
    }

    fn seed_route_state(&self, state: RouteState) {
        match self.inner.get() {
            Some(inner) => inner.seed_route_state(state),
            None => {
                if let Ok(mut seed) = self.seed.lock() {
                    *seed = Some(state);
                }
            }
        }
    }

    fn fork(&self) -> Arc<dyn LLMProvider> {
        let fork = PendingProvider {
            gateway: Arc::clone(&self.gateway),
            role: self.role,
            source: self.source,
            label: self.label.clone(),
            native: self.native,
            inner: OnceLock::new(),
            seed: Mutex::new(None),
        };
        if let Some(inner) = self.inner.get() {
            let _ = fork.inner.set(inner.fork());
        }
        Arc::new(fork)
    }

    async fn reset(&self) -> AppResult<()> {
        match self.inner.get() {
            Some(inner) => inner.reset().await,
            None => Ok(()),
        }
    }

    async fn discard_remote_session(&self) -> AppResult<()> {
        match self.inner.get() {
            Some(inner) => inner.discard_remote_session().await,
            None => Ok(()),
        }
    }

    async fn list_remote_sessions(&self) -> AppResult<Vec<RemoteSessionInfo>> {
        self.real().await?.list_remote_sessions().await
    }

    async fn delete_remote_session_by_id(&self, session_id: &str) -> AppResult<()> {
        self.real()
            .await?
            .delete_remote_session_by_id(session_id)
            .await
    }

    async fn fetch_remote_session_messages(&self, session_id: &str) -> AppResult<Vec<ChatMessage>> {
        self.real()
            .await?
            .fetch_remote_session_messages(session_id)
            .await
    }

    fn session_identity(&self) -> Option<(String, Option<i64>)> {
        self.inner.get()?.session_identity()
    }

    async fn check_session(&self, session_id: &str) -> SessionLiveness {
        match self.real().await {
            Ok(inner) => inner.check_session(session_id).await,
            Err(error) => SessionLiveness::Unknown(error.to_string()),
        }
    }

    async fn adopt_session(
        &self,
        session_id: &str,
        parent_message_id: Option<i64>,
    ) -> AppResult<()> {
        self.real()
            .await?
            .adopt_session(session_id, parent_message_id)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::fake::FakeProvider;
    use std::time::Duration;

    const CONFIG: &str = r#"
        [gateway]
        enabled = true
        [[accounts]]
        id = "a"
        provider = "custom"
        base_url = "http://127.0.0.1:1/v1"
        model = "m"
        tools = "native"
        [[routes]]
        id = "main"
        steps = [{ account = "a" }]
        [roles]
        main = "main"
    "#;

    fn config() -> GatewayConfig {
        toml::from_str(CONFIG).unwrap()
    }

    /// Готовый «движок»: всё, что просят, отдаёт один сценарный провайдер.
    struct Stub(Arc<dyn LLMProvider>);

    impl Gateway for Stub {
        fn is_enabled(&self) -> bool {
            true
        }
        fn snapshot(&self) -> GatewaySnapshot {
            DisabledGateway::new().snapshot()
        }
        fn provider_for_role(&self, _: Role, _: CallSource) -> Option<Arc<dyn LLMProvider>> {
            Some(Arc::clone(&self.0))
        }
        fn provider_for_model_id(
            &self,
            _: &str,
            _: CallSource,
        ) -> Result<Arc<dyn LLMProvider>, ResolveError> {
            Ok(Arc::clone(&self.0))
        }
        fn decide(&self, query: &DecisionQuery) -> Decision {
            DisabledGateway::new().decide(query)
        }
        fn apply_edit(&self, _: ConfigEdit) -> Result<(), EditError> {
            Ok(())
        }
        fn subscribe(&self) -> broadcast::Receiver<GatewayEvent> {
            broadcast::channel(1).0.subscribe()
        }
        fn ledger(&self) -> Arc<dyn Ledger> {
            DisabledGateway::new().ledger()
        }
    }

    /// Что провайдер сказал: потоковый ответ собирается из чанков (у `FakeProvider.complete` тела нет).
    async fn said(provider: &dyn LLMProvider) -> AppResult<String> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        provider.complete_stream(request(), tx).await?;
        let mut text = String::new();
        while let Ok(chunk) = rx.try_recv() {
            text.push_str(&chunk.content);
        }
        Ok(text)
    }

    fn request() -> CompletionRequest {
        CompletionRequest {
            messages: vec![ChatMessage::user("hi")],
            tools: Vec::new(),
            model: "m".to_string(),
            temperature: 0.0,
            max_tokens: 16,
            stream: false,
        }
    }

    fn handle() -> (Arc<PendingGateway>, PendingHandle) {
        let gateway = PendingGateway::new(config());
        (Arc::clone(&gateway), PendingHandle(gateway))
    }

    /// Пока идёт сборка, всё живо и отвечает из конфига, ничего не блокируется и не падает.
    #[test]
    fn while_building_the_panel_and_the_commands_answer_from_the_config() {
        let (_, handle) = handle();
        assert!(handle.is_enabled());
        let snapshot = handle.snapshot();
        assert!(snapshot.settings.enabled);
        assert_eq!(snapshot.routes.len(), 1);
        assert!(snapshot.problems[0].message.contains("starting"));
        let decision = handle.decide(&DecisionQuery {
            role: Role::Main,
            source: CallSource::Chat,
            tool_mode: ToolMode::Prompt,
            input_tokens: None,
            pinned: None,
            sticky: None,
            failed: Vec::new(),
        });
        assert_eq!(decision.why, Why::Starting);
        assert!(decision.explain().contains("starting"));
        assert!(matches!(
            handle.provider_for_model_id("a/m", CallSource::Chat),
            Err(ResolveError::Starting)
        ));
        assert!(
            handle
                .apply_edit(ConfigEdit::SetEnabled {
                    id: AccountId::parse("a").unwrap(),
                    enabled: false,
                })
                .is_err()
        );
        let provider = handle
            .provider_for_role(Role::Main, CallSource::Chat)
            .unwrap();
        assert_eq!(provider.model(), "auto/main");
        assert!(
            provider.native_tools(),
            "the route constant is known before the build"
        );
        assert!(provider.session_identity().is_none());
        assert!(provider.route_state().is_none());
    }

    /// Сообщение, отправленное в момент сборки, ждёт и уходит, когда движок готов.
    #[tokio::test]
    async fn a_message_sent_while_building_waits_and_then_goes_through() {
        let (pending, handle) = handle();
        let provider = handle
            .provider_for_role(Role::Main, CallSource::Chat)
            .unwrap();
        let waiting = tokio::spawn(async move { said(provider.as_ref()).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!waiting.is_finished(), "the call waits for the gateway");
        assert!(pending.is_building());

        pending.fulfil(Arc::new(Stub(Arc::new(FakeProvider::with_response(
            "done",
        )))));
        let text = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("must wake up")
            .unwrap()
            .expect("the message must not be lost");
        assert!(text.contains("done"), "{text}");
    }

    /// Другая сторона: провайдер, взятый после готовности, идёт напрямую к движку.
    #[tokio::test]
    async fn after_the_build_providers_come_straight_from_the_engine() {
        let (pending, handle) = handle();
        pending.fulfil(Arc::new(Stub(Arc::new(FakeProvider::with_response(
            "direct",
        )))));
        assert!(!pending.is_building());
        let provider = handle
            .provider_for_role(Role::Main, CallSource::Chat)
            .unwrap();
        assert!(said(provider.as_ref()).await.unwrap().contains("direct"));
    }

    /// Сборка не удалась: ждущий запрос получает ошибку, а не зависает.
    #[tokio::test]
    async fn a_failed_build_ends_a_waiting_call_with_a_clear_error() {
        let (pending, handle) = handle();
        let provider = handle
            .provider_for_role(Role::Main, CallSource::Chat)
            .unwrap();
        let waiting = tokio::spawn(async move { said(provider.as_ref()).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        pending.fail("boom".to_string());
        let error = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("failed to start"), "{error}");
    }

    /// Форк ждущего провайдера тоже ждёт; seed, поданный до готовности, не теряется.
    #[tokio::test]
    async fn a_fork_waits_too_and_a_route_state_seed_survives() {
        let (pending, handle) = handle();
        let provider = handle
            .provider_for_role(Role::Main, CallSource::Chat)
            .unwrap();
        let fork = provider.fork();
        let waiting = tokio::spawn(async move { said(fork.as_ref()).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiting.is_finished());
        pending.fulfil(Arc::new(Stub(Arc::new(FakeProvider::with_response(
            "forked",
        )))));
        assert!(waiting.await.unwrap().is_ok());
    }

    /// Роли без маршрута шлюз провайдера не даёт (вызывающий остаётся на прежнем пути).
    #[test]
    fn a_role_without_a_route_gets_no_provider() {
        let mut config = config();
        config.roles.0.clear();
        let handle = PendingHandle(PendingGateway::new(config));
        assert!(
            handle
                .provider_for_role(Role::Main, CallSource::Chat)
                .is_none()
        );
    }
}
