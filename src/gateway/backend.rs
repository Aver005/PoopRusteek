//! Трейт `Backend`: один аккаунт как источник ответов модели.
//!
//! Контракт: адаптер не знает про маршруты и fallback, он делает один вызов и честно
//! сообщает итог. API-адаптер оборачивает `CompatClient`, DeepSeek — `DeepseekProvider`,
//! CLI запускает официальный бинарь в «режиме модели» (`06-cli-spike.md`). Чужие файлы
//! авторизации CLI адаптер не читает. Владелец трейта: GW-0; реализации: GW-ENGINE (API,
//! DeepSeek) и GW-CLI (CLI).

use super::handle::LimitSnapshot;
use super::types::{AccountId, CliTool, CliTransport, ToolMode};
use crate::config::ProviderProtocol;
use crate::error::AppError;
use crate::provider::failure::FailureClass;
use crate::provider::{CompletionChunk, CompletionRequest, Usage};
use async_trait::async_trait;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc::UnboundedSender;

/// Жёсткий предел пробы. Адаптер обязан уложиться, вызывающий дополнительно обрывает по нему.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Что за адаптер.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackendKind {
    Api(ProviderProtocol),
    DeepseekWeb,
    Cli {
        tool: CliTool,
        transport: CliTransport,
    },
}

/// Откуда у адаптера числа расхода.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageSupport {
    /// Не сообщает.
    None,
    /// Считается по длине текста; в интерфейсе помечается `~`.
    Estimated,
    /// Сообщено сервером или CLI.
    Reported,
}

/// Что адаптер умеет. Фильтр выбора, а не подсказка.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// Принимает `tools` и отдаёт структурные вызовы.
    pub native_tools: bool,
    /// Работает как чистая модель: своих инструментов не запускает (инвариант 11).
    pub pure_model: bool,
    /// Отдаёт текст кусками по мере генерации.
    pub streaming: bool,
    pub usage: UsageSupport,
    /// Сообщает окна лимитов (заголовки или событие CLI).
    pub reports_limits: bool,
    /// Держит историю на своей стороне (веб DeepSeek).
    pub server_side_session: bool,
    /// Холодный старт занимает секунды (процесс CLI): не для служебных ролей.
    pub slow_start: bool,
    /// Называет размер окна контекста модели.
    pub reports_context_window: bool,
}

impl Capabilities {
    /// Годится ли адаптер для хода с таким режимом инструментов.
    pub fn supports_tool_mode(&self, mode: ToolMode) -> bool {
        match mode {
            ToolMode::Native => self.native_tools,
            ToolMode::Prompt => true,
        }
    }

    /// Можно ли ставить его в ход с нашими инструментами и подтверждениями.
    pub fn usable_for_tool_turn(&self, mode: ToolMode) -> bool {
        self.pure_model && self.supports_tool_mode(mode)
    }
}

/// Параметры одного вызова.
#[derive(Debug, Clone, PartialEq)]
pub struct CallContext {
    pub account: AccountId,
    /// Модель этого вызова. `request.model` адаптеры игнорируют.
    pub model: String,
    /// Режим инструментов хода. Адаптер подчиняется ему, а не своей настройке.
    pub tool_mode: ToolMode,
}

/// Итог удачного вызова.
#[derive(Debug, Clone, Default)]
pub struct CallReport {
    pub usage: Option<Usage>,
    /// Расход посчитан по тексту, а не сообщён.
    pub usage_estimated: bool,
    pub limits: Option<LimitSnapshot>,
    pub finish_reason: Option<String>,
    pub context_window: Option<u32>,
    /// Время до первого смыслового чанка.
    pub ttft: Option<Duration>,
    pub elapsed: Duration,
    /// Идентификатор запроса на стороне сервера, для разбора жалоб.
    pub upstream_id: Option<String>,
}

/// Итог неудачного вызова.
#[derive(Debug, Clone)]
pub struct BackendError {
    pub class: FailureClass,
    pub status: Option<u16>,
    pub message: String,
    /// Ушёл ли наружу смысловой чанк. После этого fallback запрещён.
    pub emitted: bool,
    pub limits: Option<LimitSnapshot>,
}

impl BackendError {
    /// Из ошибки провайдера. Без известного класса считаем сбоем сервера.
    pub fn from_app(account: &AccountId, error: &AppError, emitted: bool) -> Self {
        let (status, limits) = match error {
            AppError::Upstream(failure) => (
                failure.status.map(|s| s.as_u16()),
                failure
                    .limits
                    .as_ref()
                    .map(|l| LimitSnapshot::from_headers(account.clone(), l)),
            ),
            _ => (None, None),
        };
        Self {
            class: error.failure_class().unwrap_or(FailureClass::Server),
            status,
            message: error.to_string(),
            emitted,
            limits,
        }
    }
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.class.code())
    }
}

/// Кусок, который клиент уже мог увидеть: текст, рассуждение или вызов инструмента.
/// Живые сигналы, комментарии SSE и чанк с одним расходом не в счёт.
pub fn is_meaningful(chunk: &CompletionChunk) -> bool {
    !chunk.content.is_empty() || !chunk.tool_calls.is_empty()
}

/// Обёртка над каналом чанков: помнит, ушло ли наружу что-то смысловое.
#[derive(Clone)]
pub struct EmitTracker {
    tx: UnboundedSender<CompletionChunk>,
    emitted: Arc<AtomicBool>,
}

impl EmitTracker {
    pub fn new(tx: UnboundedSender<CompletionChunk>) -> Self {
        Self {
            tx,
            emitted: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Отправить чанк. `false` — получатель ушёл, вызов можно прекращать.
    pub fn send(&self, chunk: CompletionChunk) -> bool {
        let meaningful = is_meaningful(&chunk);
        let delivered = self.tx.send(chunk).is_ok();
        if delivered && meaningful {
            self.emitted.store(true, Ordering::Relaxed);
        }
        delivered
    }

    pub fn emitted(&self) -> bool {
        self.emitted.load(Ordering::Relaxed)
    }
}

/// Модель аккаунта из его каталога.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    pub context_window: Option<u32>,
    pub max_output: Option<u32>,
    /// `None` — неизвестно, не «нет».
    pub supports_tools: Option<bool>,
}

/// Состояние входа по пробе.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthState {
    Ok,
    /// Не вошёл или вход просрочен.
    NeedsLogin,
    Unknown,
}

/// Итог пробы: дёшево и без расхода токенов, если источник это позволяет.
#[derive(Debug, Clone)]
pub struct ProbeReport {
    pub auth: AuthState,
    pub limits: Option<LimitSnapshot>,
    /// Версия CLI или сервера.
    pub version: Option<String>,
    pub detail: Option<String>,
    pub checked_at: SystemTime,
}

// async-trait вешает `#[must_use]` на метод, чей `Pin<Box<dyn Future>>` и так must_use.
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait Backend: Send + Sync {
    fn kind(&self) -> BackendKind;
    fn capabilities(&self) -> Capabilities;

    /// Один вызов модели. Чанки идут в `tx` по мере прихода, итог типизирован.
    /// Расход настоящий, если сервер его назвал; иначе оценка с `usage_estimated`.
    async fn call(
        &self,
        request: CompletionRequest,
        context: &CallContext,
        tx: EmitTracker,
    ) -> Result<CallReport, BackendError>;

    async fn list_models(&self) -> Result<Vec<ModelInfo>, BackendError>;

    /// Не дольше [`PROBE_TIMEOUT`]. Не должна тратить токены без нужды.
    async fn probe(&self) -> ProbeReport;

    /// Новый сеанс того же аккаунта: общие соединения и лимиты, своя серверная история.
    fn fork(&self) -> Arc<dyn Backend>;

    /// Живая серверная сессия, если адаптер её ведёт (как `LLMProvider::session_identity`).
    fn session_identity(&self) -> Option<(String, Option<i64>)> {
        None
    }

    /// Токены текущей сессии на стороне сервера, если известны.
    fn session_tokens(&self) -> Option<u32> {
        None
    }

    /// Удалить серверную сессию одноразового форка.
    async fn discard_session(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(content: &str) -> CompletionChunk {
        CompletionChunk {
            content: content.to_string(),
            ..CompletionChunk::default()
        }
    }

    /// Критик A9: пустой живой сигнал и чанк расхода не выключают fallback.
    #[test]
    fn only_text_or_calls_count_as_emitted() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tracker = EmitTracker::new(tx);
        assert!(tracker.send(text("")));
        assert!(tracker.send(CompletionChunk {
            usage: Some(Usage::default()),
            ..CompletionChunk::default()
        }));
        assert!(
            !tracker.emitted(),
            "heartbeats and usage are invisible to the client"
        );
        assert!(tracker.send(text("hi")));
        assert!(tracker.emitted());
    }

    #[test]
    fn a_tool_call_alone_counts_as_emitted() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tracker = EmitTracker::new(tx);
        tracker.send(CompletionChunk {
            tool_calls: vec![crate::provider::ToolCall::default()],
            ..CompletionChunk::default()
        });
        assert!(tracker.emitted());
    }

    #[test]
    fn a_dropped_receiver_is_reported_and_nothing_is_marked() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        let tracker = EmitTracker::new(tx);
        assert!(!tracker.send(text("lost")));
        assert!(!tracker.emitted());
    }

    #[test]
    fn a_provider_error_keeps_its_class_status_and_limits() {
        use crate::provider::failure::{HeaderWindow, ProviderFailure, RateLimitHeaders};
        let error = AppError::Upstream(Box::new(ProviderFailure {
            class: FailureClass::RateLimit {
                retry_after: Some(Duration::from_secs(5)),
            },
            status: Some(reqwest::StatusCode::TOO_MANY_REQUESTS),
            message: "slow".into(),
            limits: Some(RateLimitHeaders {
                retry_after: Some(Duration::from_secs(5)),
                windows: vec![HeaderWindow {
                    name: "requests".into(),
                    limit: Some(10),
                    remaining: Some(0),
                    resets_at: None,
                }],
                observed_at: SystemTime::now(),
            }),
        }));
        let account = AccountId::parse("a").unwrap();
        let mapped = BackendError::from_app(&account, &error, false);
        assert_eq!(mapped.status, Some(429));
        assert!(matches!(mapped.class, FailureClass::RateLimit { .. }));
        assert_eq!(mapped.limits.unwrap().windows[0].used_pct, Some(100.0));
        let unknown = BackendError::from_app(&account, &AppError::Provider("x".into()), true);
        assert_eq!(unknown.class, FailureClass::Server);
        assert!(unknown.emitted);
    }

    #[test]
    fn a_cli_that_is_not_a_pure_model_never_serves_a_tool_turn() {
        let caps = Capabilities {
            native_tools: false,
            pure_model: false,
            streaming: true,
            usage: UsageSupport::Reported,
            reports_limits: false,
            server_side_session: false,
            slow_start: true,
            reports_context_window: false,
        };
        assert!(!caps.usable_for_tool_turn(ToolMode::Prompt));
        let pure = Capabilities {
            pure_model: true,
            ..caps.clone()
        };
        assert!(pure.usable_for_tool_turn(ToolMode::Prompt));
        assert!(
            !pure.usable_for_tool_turn(ToolMode::Native),
            "no native tools, no native turn"
        );
    }
}
