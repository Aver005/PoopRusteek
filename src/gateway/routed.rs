//! `RoutedProvider`: шлюз за обычным `LLMProvider`; на вызов выбирает цель, место и адаптер.
//! Отказ до первого ушедшего текста или вызова переводит на следующий шаг, после — идёт вверх.

use super::api_backend::estimate_prompt_tokens;
use super::backend::{Backend, BackendError, CallContext, Capabilities, EmitTracker};
use super::engine::{Binding, Core, Runtime};
use super::handle::{
    CallSource, Decision, DecisionQuery, GatewayEvent, Outcome, SkipReason, Target, UsageRecord,
};
use super::history::{carries_foreign_state, flatten_for_switch};
use super::state::RouteState;
use super::types::{AccountId, Role, ToolMode};
use crate::debug_log;
use crate::error::{AppError, AppResult};
use crate::provider::failure::{FailureClass, ProviderFailure};
use crate::provider::{
    ChatMessage, CompletionChunk, CompletionRequest, CompletionResponse, LLMProvider,
    RemoteSessionInfo, SessionLiveness,
};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

/// Сколько ждать освобождения единственного годного аккаунта, прежде чем сдаться.
const QUEUE_WAIT: Duration = Duration::from_secs(60);

struct Bound {
    account: AccountId,
    model: String,
    backend: Arc<dyn Backend>,
}

#[derive(Default)]
struct Affinity {
    bound: Option<Bound>,
    /// Где беседа была до перезапуска (из `Session.route_state`).
    seed: Option<RouteState>,
    /// Сколько начальных сообщений уже развёрнуто в текст при смене цели.
    flatten_prefix: usize,
}

pub struct RoutedProvider {
    core: Arc<Core>,
    binding: Binding,
    source: CallSource,
    /// Константа маршрута: цикл агента читает её один раз за ход.
    tool_mode: ToolMode,
    label: String,
    affinity: Mutex<Affinity>,
}

/// Фактический исход одной попытки, нужный для учёта.
struct Attempt<'a> {
    target: &'a Target,
    number: u8,
    request_id: &'a str,
    fallback_from: Option<AccountId>,
    started: Instant,
}

impl RoutedProvider {
    pub(super) fn new(
        core: Arc<Core>,
        binding: Binding,
        source: CallSource,
        tool_mode: ToolMode,
    ) -> Self {
        let label = match &binding {
            Binding::Role(role) => core
                .config()
                .roles
                .route_for(*role)
                .map_or_else(|| format!("{role:?}"), |route| format!("auto/{route}")),
            Binding::Route(route) => format!("auto/{route}"),
            Binding::Fixed(target) => format!("{}/{}", target.account, target.model),
        };
        Self {
            core,
            binding,
            source,
            tool_mode,
            label,
            affinity: Mutex::new(Affinity::default()),
        }
    }

    fn role(&self) -> Role {
        match &self.binding {
            Binding::Role(role) => *role,
            _ => Role::Main,
        }
    }

    fn affinity(&self) -> std::sync::MutexGuard<'_, Affinity> {
        // Отравленный замок: состояние привязки простое, берём как есть.
        self.affinity.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn sticky(&self) -> Option<AccountId> {
        self.affinity().bound.as_ref().map(|b| b.account.clone())
    }

    fn bound_backend(&self) -> Option<Arc<dyn Backend>> {
        self.affinity()
            .bound
            .as_ref()
            .map(|b| Arc::clone(&b.backend))
    }

    /// Куда ушёл бы вызов прямо сейчас (для ответов, которых ждут до первого вызова).
    fn probable_target(&self) -> Option<Target> {
        if let Some(bound) = &self.affinity().bound {
            return Some(Target {
                account: bound.account.clone(),
                model: bound.model.clone(),
            });
        }
        let plan = self.core.plan(&self.binding)?;
        let query = self.query(&plan.tool_mode, None, Vec::new());
        self.core.select(&plan, &query, None).decision.target
    }

    fn probable_caps(&self) -> Option<Capabilities> {
        let target = self.probable_target()?;
        self.core.runtime(&target.account)?.caps.clone()
    }

    fn query(
        &self,
        tool_mode: &ToolMode,
        input_tokens: Option<u32>,
        failed: Vec<(AccountId, String)>,
    ) -> DecisionQuery {
        DecisionQuery {
            role: self.role(),
            source: self.source,
            tool_mode: *tool_mode,
            input_tokens,
            pinned: None,
            sticky: self.sticky(),
            failed,
        }
    }

    // ── Привязка к цели ─────────────────────────────────────────────────

    /// Адаптер под вызов и признак «это новая серверная сессия».
    #[allow(
        clippy::result_large_err,
        reason = "`BackendError` отдаётся трейтом `Backend` по значению (контракт GW-0)"
    )]
    fn backend_for(
        &self,
        target: &Target,
        runtime: &Runtime,
    ) -> Result<(Arc<dyn Backend>, bool), BackendError> {
        if let Some(bound) = self
            .affinity()
            .bound
            .as_ref()
            .filter(|b| b.account == target.account)
        {
            return Ok((Arc::clone(&bound.backend), false));
        }
        match &runtime.backend {
            Ok(backend) => Ok((backend.fork(), true)),
            Err(reason) => Err(BackendError {
                class: FailureClass::Server,
                status: None,
                message: reason.clone(),
                emitted: false,
                limits: None,
            }),
        }
    }

    /// Сколько начальных сообщений развернуть в текст: всё, если цель сменилась.
    /// Без привязки и подсказки откуда история неизвестно, а чужие вызовы в ней разворачиваются.
    fn flatten_len(&self, target: &Target, messages: &[ChatMessage]) -> usize {
        let aff = self.affinity();
        let switching = match (&aff.bound, &aff.seed) {
            (Some(bound), _) => bound.account != target.account || bound.model != target.model,
            (None, Some(seed)) => seed.account != target.account || seed.model != target.model,
            (None, None) => carries_foreign_state(messages),
        };
        if switching {
            messages.len()
        } else {
            aff.flatten_prefix.min(messages.len())
        }
    }

    /// Прежний адаптер просто отпускается: серверную сессию, которую видит пользователь, смена цели не удаляет.
    fn commit(&self, target: &Target, backend: Arc<dyn Backend>, flatten_len: usize) {
        let mut aff = self.affinity();
        aff.bound = Some(Bound {
            account: target.account.clone(),
            model: target.model.clone(),
            backend,
        });
        aff.seed = None;
        aff.flatten_prefix = flatten_len;
    }

    // ── Вызов ───────────────────────────────────────────────────────────

    fn record(
        &self,
        attempt: &Attempt<'_>,
        route: Option<super::types::RouteId>,
        outcome: Outcome,
        report: Option<&super::backend::CallReport>,
    ) {
        self.core.ledger_record(UsageRecord {
            at: self.core.now(),
            source: self.source,
            account: attempt.target.account.clone(),
            model: attempt.target.model.clone(),
            route,
            usage: report.and_then(|r| r.usage.clone()),
            estimated: report.is_some_and(|r| r.usage_estimated),
            cost_usd: None,
            latency_ms: attempt
                .started
                .elapsed()
                .as_millis()
                .min(u128::from(u32::MAX)) as u32,
            ttft_ms: report
                .and_then(|r| r.ttft)
                .map(|t| t.as_millis().min(u128::from(u32::MAX)) as u32),
            outcome,
            fallback_from: attempt.fallback_from.clone(),
            attempt: attempt.number,
            request_id: Some(attempt.request_id.to_string()),
        });
    }

    async fn take_place(
        &self,
        runtime: &Runtime,
        queue: bool,
    ) -> Result<Option<tokio::sync::OwnedSemaphorePermit>, AppError> {
        let sem = Arc::clone(&runtime.sem);
        if !queue {
            // Проиграли гонку за место: пусть выбор повторится и увидит `Busy`.
            return Ok(sem.try_acquire_owned().ok());
        }
        match tokio::time::timeout(QUEUE_WAIT, sem.acquire_owned()).await {
            Ok(Ok(permit)) => Ok(Some(permit)),
            _ => Err(failure(
                FailureClass::Overloaded,
                "every account is busy and none freed up in time".to_string(),
            )),
        }
    }

    async fn run(
        &self,
        request: CompletionRequest,
        tx: UnboundedSender<CompletionChunk>,
    ) -> AppResult<()> {
        let request_id = uuid::Uuid::new_v4().simple().to_string();
        let input_tokens = estimate_prompt_tokens(&request);
        let attachments = attached_paths(&request);
        let mut failed: Vec<(AccountId, String)> = Vec::new();
        let mut last_error: Option<BackendError> = None;
        let mut previous: Option<(AccountId, FailureClass)> = None;
        let mut attempts: u8 = 0;
        // Каждый круг либо тратит аккаунт (`failed`), либо упирается в гонку за место.
        let max_rounds = self
            .core
            .plan(&self.binding)
            .map_or(1, |plan| plan.steps.len() * 2 + 4);

        for _ in 0..max_rounds {
            let Some(plan) = self.core.plan(&self.binding) else {
                return Err(failure(
                    FailureClass::BadRequest,
                    format!("route '{}' no longer exists", self.label),
                ));
            };
            let route_id = plan.route.as_ref().map(|route| route.id.clone());
            let query = self.query(&self.tool_mode, Some(input_tokens), failed.clone());
            let selection = self
                .core
                .select_with(&plan, &query, route_id.clone(), &attachments);
            self.core.remember_decision(&selection.decision);
            debug_log::log_json(
                "gateway.decision",
                &serde_json::json!({
                    "route": self.label,
                    "explain": selection.decision.explain(),
                    "target": selection.decision.target.as_ref().map(|t| format!("{}/{}", t.account, t.model)),
                }),
            );
            let Some(target) = selection.decision.target.clone() else {
                if let Some(error) = self.attachment_refusal(&selection.decision, &attachments) {
                    return Err(error);
                }
                return Err(self
                    .core
                    .exhausted_error(&selection.decision, last_error.as_ref()));
            };
            let Some(runtime) = self.core.runtime(&target.account) else {
                failed.push((target.account.clone(), "unavailable".to_string()));
                continue;
            };
            let Some(permit) = self.take_place(&runtime, selection.queue).await? else {
                continue;
            };
            // О переходе сообщаем, когда он состоялся, а не когда выбран: гонка за место его не съедает.
            if let Some((from, class)) = previous.take() {
                debug_log::log_json(
                    "gateway.fallback",
                    &serde_json::json!({"from": from, "to": target.account, "class": class.code()}),
                );
                self.core.emit(GatewayEvent::FallbackTaken {
                    from,
                    to: target.account.clone(),
                    class,
                    source: self.source,
                });
            }

            let (backend, fresh_fork) = match self.backend_for(&target, &runtime) {
                Ok(pair) => pair,
                Err(error) => {
                    failed.push((target.account.clone(), error.class.code().to_string()));
                    last_error = Some(error);
                    continue;
                }
            };
            let mut attempt_request = request.clone();
            let flatten_len = self.flatten_len(&target, &attempt_request.messages);
            if flatten_len > 0 && carries_foreign_state(&attempt_request.messages[..flatten_len]) {
                let flat = flatten_for_switch(&attempt_request.messages[..flatten_len]);
                attempt_request.messages.splice(..flatten_len, flat);
            }

            attempts = attempts.saturating_add(1);
            let attempt = Attempt {
                target: &target,
                number: attempts,
                request_id: &request_id,
                fallback_from: failed.last().map(|(account, _)| account.clone()),
                started: Instant::now(),
            };
            let context = CallContext {
                account: target.account.clone(),
                model: target.model.clone(),
                tool_mode: self.tool_mode,
            };
            let tracker = EmitTracker::new(tx.clone());
            let result = backend.call(attempt_request, &context, tracker).await;
            drop(permit);

            match result {
                Ok(report) => {
                    self.core.report_success(&runtime, &target.account);
                    if let Some(limits) = &report.limits {
                        self.core
                            .note_limits(&runtime, &target.account, limits.clone());
                    }
                    if let Some(window) = report.context_window {
                        self.core
                            .learn_window(&target.account, &target.model, window);
                    }
                    self.record(&attempt, route_id, Outcome::Ok, Some(&report));
                    self.commit(&target, backend, flatten_len);
                    return Ok(());
                }
                Err(error) => {
                    if fresh_fork && !error.emitted {
                        discard_in_background(backend);
                    }
                    self.core.report_failure(
                        &runtime,
                        &target.account,
                        &error.class,
                        &error.message,
                    );
                    if let Some(limits) = &error.limits {
                        self.core
                            .note_limits(&runtime, &target.account, limits.clone());
                    }
                    self.record(
                        &attempt,
                        route_id,
                        Outcome::Failed(error.class.code().to_string()),
                        None,
                    );
                    if error.emitted || !self.core.fallback_allowed(&plan, &error.class) {
                        return Err(upstream_error(&error));
                    }
                    failed.push((target.account.clone(), error.class.code().to_string()));
                    previous = Some((target.account.clone(), error.class.clone()));
                    last_error = Some(error);
                }
            }
        }
        Err(failure(
            FailureClass::Overloaded,
            format!(
                "gateway could not settle on an account for '{}'",
                self.label
            ),
        ))
    }

    /// Файл не должен тихо пропасть: если принять его некому, ход кончается понятной ошибкой.
    fn attachment_refusal(&self, decision: &Decision, attachments: &[PathBuf]) -> Option<AppError> {
        let blocked = decision
            .skipped
            .iter()
            .any(|skip| skip.reason == SkipReason::NoAttachmentSupport);
        if !blocked {
            return None;
        }
        let names: Vec<String> = attachments
            .iter()
            .filter_map(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        let class = match decision.retry_at {
            Some(at) => FailureClass::RateLimit {
                retry_after: Some(at.duration_since(self.core.now()).unwrap_or_default()),
            },
            None => FailureClass::BadRequest,
        };
        Some(failure(
            class,
            format!(
                "the attached file ({}) was not sent: only accounts that accept attachments can take this turn and none is available now ({})",
                names.join(", "),
                decision.explain()
            ),
        ))
    }

    /// Адаптер, который ведёт серверные сессии: текущий или первый такой в маршруте.
    fn session_backend(&self) -> Option<Arc<dyn Backend>> {
        if let Some(backend) = self.bound_backend()
            && backend.capabilities().server_side_session
        {
            return Some(backend);
        }
        let plan = self.core.plan(&self.binding)?;
        plan.steps.iter().find_map(|step| {
            let runtime = self.core.runtime(&step.account)?;
            let backend = runtime.backend.as_ref().ok()?;
            backend
                .capabilities()
                .server_side_session
                .then(|| Arc::clone(backend))
        })
    }

    /// Привязать беседу к аккаунту с серверной сессией (нужно для `adopt_session`).
    fn bind_session_backend(&self) -> Option<Arc<dyn Backend>> {
        if let Some(backend) = self.bound_backend()
            && backend.capabilities().server_side_session
        {
            return Some(backend);
        }
        let plan = self.core.plan(&self.binding)?;
        let (step, runtime) = plan.steps.iter().find_map(|step| {
            let runtime = self.core.runtime(&step.account)?;
            let ok = runtime
                .backend
                .as_ref()
                .ok()?
                .capabilities()
                .server_side_session;
            ok.then_some((step, runtime))
        })?;
        let backend = runtime.backend.as_ref().ok()?.fork();
        let model = step
            .model
            .clone()
            .or_else(|| self.core.config().account(&step.account)?.model.clone())
            .unwrap_or_default();
        let mut aff = self.affinity();
        aff.bound = Some(Bound {
            account: step.account.clone(),
            model,
            backend: Arc::clone(&backend),
        });
        Some(backend)
    }
}

/// Файлы хвоста после последнего ответа модели: доставляются сейчас, а не в прошлых ходах.
fn attached_paths(request: &CompletionRequest) -> Vec<PathBuf> {
    request
        .messages
        .iter()
        .rev()
        .take_while(|message| message.role != crate::provider::Role::Assistant)
        .flat_map(|message| message.attachments.iter().map(PathBuf::from))
        .collect()
}

/// Удалить сессию, которую создала неудачная попытка и которой не видел пользователь.
fn discard_in_background(backend: Arc<dyn Backend>) {
    if !backend.capabilities().server_side_session {
        return;
    }
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(async move { backend.discard_session().await });
    }
}

/// Ошибка шлюза в том же типе, что у апстрима: класс не теряется на пути наверх.
pub(super) fn failure(class: FailureClass, message: String) -> AppError {
    AppError::Upstream(Box::new(ProviderFailure {
        class,
        status: None,
        message,
        limits: None,
    }))
}

/// `BackendError` обратно в `AppError::Upstream`: класс и статус те же, текст без дублей.
pub(super) fn upstream_error(error: &BackendError) -> AppError {
    let status = error
        .status
        .and_then(|code| reqwest::StatusCode::from_u16(code).ok());
    let mut message = error
        .message
        .strip_prefix("Provider error: ")
        .unwrap_or(&error.message);
    if let Some(status) = status
        && let Some(rest) = message.strip_prefix(&format!("{status}: "))
    {
        message = rest;
    }
    AppError::Upstream(Box::new(ProviderFailure {
        class: error.class.clone(),
        status,
        message: message.to_string(),
        limits: None,
    }))
}

#[async_trait]
impl LLMProvider for RoutedProvider {
    async fn complete(&self, request: CompletionRequest) -> AppResult<CompletionResponse> {
        let (tx, mut rx) = unbounded_channel();
        self.run(request, tx).await?;
        let mut content = String::new();
        let mut finish_reason = None;
        let mut usage = None;
        while let Ok(chunk) = rx.try_recv() {
            content.push_str(&chunk.content);
            finish_reason = chunk.finish_reason.or(finish_reason);
            usage = chunk.usage.or(usage);
        }
        Ok(CompletionResponse {
            content,
            finish_reason,
            usage,
        })
    }

    async fn complete_stream(
        &self,
        request: CompletionRequest,
        tx: UnboundedSender<CompletionChunk>,
    ) -> AppResult<()> {
        self.run(request, tx).await
    }

    /// Имя маршрута: модель меняется от вызова к вызову, а `&str` обязан быть стабильным.
    fn model(&self) -> &str {
        &self.label
    }

    async fn list_models(&self) -> AppResult<Vec<String>> {
        let plan = self
            .core
            .plan(&self.binding)
            .ok_or_else(|| AppError::Custom(format!("route '{}' no longer exists", self.label)))?;
        let mut models = Vec::new();
        let mut last_error = None;
        for step in &plan.steps {
            let Some(backend) = self
                .core
                .runtime(&step.account)
                .and_then(|runtime| runtime.backend.as_ref().ok().cloned())
            else {
                continue;
            };
            match backend.list_models().await {
                Ok(listed) => models.extend(
                    listed
                        .into_iter()
                        .map(|model| format!("{}/{}", step.account, model.id)),
                ),
                Err(error) => last_error = Some(error),
            }
        }
        match (models.is_empty(), last_error) {
            (true, Some(error)) => Err(upstream_error(&error)),
            _ => Ok(models),
        }
    }

    async fn context_window(&self) -> Option<u32> {
        let target = self.probable_target()?;
        if let Some(known) = self.core.window_of(&target.account, &target.model) {
            return Some(known);
        }
        let backend = self
            .core
            .runtime(&target.account)?
            .backend
            .as_ref()
            .ok()?
            .clone();
        let window = backend.context_window(&target.model).await?;
        self.core
            .learn_window(&target.account, &target.model, window);
        Some(window)
    }

    fn keeps_server_side_history(&self) -> bool {
        self.probable_caps()
            .is_some_and(|caps| caps.server_side_session)
    }

    fn native_tools(&self) -> bool {
        self.tool_mode == ToolMode::Native
    }

    /// Отвечает по цели, куда файл реально уйдёт: выбор ограничен аккаунтами, которые его приложат.
    fn accepts_attachment(&self, path: &Path) -> bool {
        let Some(plan) = self.core.plan(&self.binding) else {
            return false;
        };
        let query = self.query(&plan.tool_mode, None, Vec::new());
        let attachments = [path.to_path_buf()];
        self.core
            .select_with(&plan, &query, None, &attachments)
            .decision
            .target
            .is_some()
    }

    fn session_tokens(&self) -> Option<u32> {
        self.bound_backend()?.session_tokens()
    }

    fn session_identity(&self) -> Option<(String, Option<i64>)> {
        self.bound_backend()?.session_identity()
    }

    fn route_state(&self) -> Option<RouteState> {
        let (account, model) = {
            let aff = self.affinity();
            match (&aff.bound, &aff.seed) {
                (Some(bound), _) => (bound.account.clone(), bound.model.clone()),
                (None, Some(seed)) => (seed.account.clone(), seed.model.clone()),
                (None, None) => return None,
            }
        };
        let route = match &self.binding {
            Binding::Role(role) => self.core.config().roles.route_for(*role).cloned(),
            Binding::Route(route) => Some(route.clone()),
            Binding::Fixed(_) => None,
        };
        Some(RouteState {
            route,
            account,
            model,
        })
    }

    fn seed_route_state(&self, state: RouteState) {
        let mut aff = self.affinity();
        if aff.bound.is_none() {
            aff.seed = Some(state);
        }
    }

    fn fork(&self) -> Arc<dyn LLMProvider> {
        Arc::new(Self {
            core: Arc::clone(&self.core),
            binding: self.binding.clone(),
            source: self.source,
            tool_mode: self.tool_mode,
            label: self.label.clone(),
            affinity: Mutex::new(Affinity::default()),
        })
    }

    /// Сессия сбрасывается, но откуда пришла история, помнится: чужие вызовы в ней всё ещё чужие.
    async fn reset(&self) -> AppResult<()> {
        let route = self.route_state();
        let mut aff = self.affinity();
        aff.bound = None;
        aff.seed = route;
        Ok(())
    }

    async fn discard_remote_session(&self) -> AppResult<()> {
        let bound = self.affinity().bound.take();
        if let Some(bound) = bound {
            bound.backend.discard_session().await;
        }
        Ok(())
    }

    async fn list_remote_sessions(&self) -> AppResult<Vec<RemoteSessionInfo>> {
        match self.session_backend() {
            Some(backend) => backend.list_remote_sessions().await,
            None => Err(AppError::Custom(
                "Remote session listing not supported by this route".to_string(),
            )),
        }
    }

    async fn delete_remote_session_by_id(&self, session_id: &str) -> AppResult<()> {
        match self.session_backend() {
            Some(backend) => backend.delete_remote_session_by_id(session_id).await,
            None => Err(AppError::Custom(
                "Remote session deletion not supported by this route".to_string(),
            )),
        }
    }

    async fn fetch_remote_session_messages(&self, session_id: &str) -> AppResult<Vec<ChatMessage>> {
        match self.session_backend() {
            Some(backend) => backend.fetch_remote_session_messages(session_id).await,
            None => Err(AppError::Custom(
                "Remote session fetching not supported by this route".to_string(),
            )),
        }
    }

    async fn check_session(&self, session_id: &str) -> SessionLiveness {
        match self.session_backend() {
            Some(backend) => backend.check_session(session_id).await,
            None => SessionLiveness::Unknown("no account in this route keeps sessions".into()),
        }
    }

    async fn adopt_session(&self, session_id: &str, parent: Option<i64>) -> AppResult<()> {
        match self.bind_session_backend() {
            Some(backend) => backend.adopt_session(session_id, parent).await,
            None => Ok(()),
        }
    }
}
