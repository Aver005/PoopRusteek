//! `Backend` над процессом CLI: один вызов — один процесс в пустой папке, дерево гасится всегда.
//! Отмена хода (уроненный вызов) убивает дерево сразу и не считается отказом аккаунта.

use super::classify::cap_message;
use super::dialect::{Dialect, ModelSource, Plan, check_model};
use super::discover::{Program, Version};
use super::launch::Scratch;
use super::process::{Captured, End, Flow, Limits, run_captured, run_streaming};
use crate::gateway::backend::{
    AuthState, Backend, BackendError, BackendKind, CallContext, CallReport, Capabilities,
    EmitTracker, ModelInfo, PROBE_TIMEOUT, ProbeReport,
};
use crate::gateway::handle::LimitSnapshot;
use crate::gateway::types::{AccountId, CliTransport};
use crate::provider::failure::FailureClass;
use crate::provider::prompt::{SystemDelivery, build_prompt, split_system_prompt};
use crate::provider::{CompletionChunk, CompletionRequest};
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Если системного промпта нет, CLI подставил бы свой, с повадками агента.
const FALLBACK_SYSTEM: &str = "You are a helpful assistant.";
/// Срок команд каталога и проверки входа (меньше `PROBE_TIMEOUT`).
const SIDE_COMMAND_TIMEOUT: Duration = Duration::from_secs(8);
/// Сколько stderr кладём в сообщение об обрыве.
const STDERR_IN_MESSAGE: usize = 300;
/// Сколько текста неопознанного отказа уходит в журнал.
const LOG_TEXT_CAP: usize = 8000;

/// Что адаптер узнал за вызовы и показывает дальше (окна контекста, лимиты).
#[derive(Default)]
struct Learned {
    windows: HashMap<String, u32>,
    limits: Option<LimitSnapshot>,
    seen_models: Vec<String>,
}

pub struct CliSetup {
    pub dialect: Arc<dyn Dialect>,
    pub account: AccountId,
    pub program: Program,
    pub version: Option<Version>,
    /// Папка входа аккаунта; `None` — общий вход пользователя.
    pub home: Option<PathBuf>,
    pub default_model: Option<String>,
    pub scratch_root: PathBuf,
    /// Оставлять папки вызова после него (только проверка приманки).
    pub keep_scratch: bool,
}

struct Inner {
    setup: CliSetup,
    limits: Limits,
    learned: Mutex<Learned>,
}

/// Адаптер CLI. `fork` общий: процессы без состояния, место на аккаунте держит движок.
#[derive(Clone)]
pub struct CliBackend {
    inner: Arc<Inner>,
}

impl CliBackend {
    pub fn new(setup: CliSetup) -> Self {
        let limits = setup.dialect.limits();
        Self::with_limits(setup, limits)
    }

    /// Свои сроки: тесты ставят короткие.
    pub fn with_limits(setup: CliSetup, limits: Limits) -> Self {
        Self {
            inner: Arc::new(Inner {
                setup,
                limits,
                learned: Mutex::new(Learned::default()),
            }),
        }
    }

    fn setup(&self) -> &CliSetup {
        &self.inner.setup
    }

    fn model_for(&self, context: &CallContext) -> String {
        if !context.model.is_empty() {
            context.model.clone()
        } else {
            self.setup().default_model.clone().unwrap_or_default()
        }
    }

    fn learn(&self, model: &str, window: Option<u32>, limits: Option<&LimitSnapshot>) {
        let Ok(mut learned) = self.inner.learned.lock() else {
            return;
        };
        if let Some(window) = window {
            learned.windows.insert(model.to_string(), window);
        }
        if let Some(limits) = limits {
            learned.limits = Some(limits.clone());
        }
        if !model.is_empty() && !learned.seen_models.iter().any(|m| m == model) {
            learned.seen_models.push(model.to_string());
        }
    }

    fn error(
        &self,
        class: FailureClass,
        message: impl Into<String>,
        emitted: bool,
    ) -> BackendError {
        BackendError {
            class,
            status: None,
            message: message.into(),
            emitted,
            limits: None,
        }
    }

    /// Побочная команда в окружении вызова: тот же проверенный запуск, другие аргументы, пустой ввод.
    async fn side_command(&self, command: &[&str]) -> Result<Captured, String> {
        let setup = self.setup();
        let scratch = Scratch::create(&setup.scratch_root).map_err(|e| e.to_string())?;
        let system = scratch
            .write_system(FALLBACK_SYSTEM)
            .map_err(|e| e.to_string())?;
        let plan = Plan {
            program: &setup.program,
            home: setup.home.as_deref(),
            model: "",
            system_file: &system,
            prompt: String::new(),
            scratch: &scratch,
            parent_env: std::env::vars_os().collect(),
        };
        let launch = setup.dialect.launch_side(&plan, command);
        setup.dialect.verify_side(&launch, command)?;
        // Асинхронный процесс под сроком: цикл событий не блокируется.
        Ok(run_captured(&launch, SIDE_COMMAND_TIMEOUT).await)
    }
}

/// Оценка токенов по длине текста, как у остальных адаптеров (4 символа на токен).
fn estimate(chars: usize) -> u32 {
    u32::try_from((chars / 4).max(1)).unwrap_or(u32::MAX)
}

/// Отказ по концу процесса без терминального события.
fn end_failure(end: &End, stderr: &str, emitted: bool) -> (FailureClass, String) {
    let tail = {
        let trimmed = stderr.trim();
        let start = trimmed.len().saturating_sub(STDERR_IN_MESSAGE);
        let start = trimmed.ceil_char_boundary(start);
        trimmed[start..].trim().to_string()
    };
    let with_tail = |text: String| {
        if tail.is_empty() {
            text
        } else {
            format!("{text}: {tail}")
        }
    };
    match end {
        End::Spawn(error) => (
            FailureClass::Server,
            format!("cannot start the CLI: {error}"),
        ),
        End::Idle(after) => (
            FailureClass::Timeout,
            format!(
                "the CLI was silent for {}s and was stopped",
                after.as_secs()
            ),
        ),
        End::Total(after) => (
            FailureClass::Timeout,
            format!("the call exceeded {}s and was stopped", after.as_secs()),
        ),
        End::Exited(Some(0)) | End::Done(_) => (
            FailureClass::Protocol,
            "the CLI finished without a final event".to_string(),
        ),
        End::Exited(code) => (
            FailureClass::Network,
            with_tail(match code {
                Some(code) => format!(
                    "the CLI stopped before finishing (exit code {code}{})",
                    if emitted {
                        ", after part of the answer"
                    } else {
                        ""
                    }
                ),
                None => "the CLI was cut off before finishing".to_string(),
            }),
        ),
        End::Stopped => (
            FailureClass::BadRequest,
            "the call was cancelled".to_string(),
        ),
    }
}

#[async_trait]
impl Backend for CliBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Cli {
            tool: self.setup().dialect.tool(),
            transport: CliTransport::OneShot,
        }
    }

    fn capabilities(&self) -> Capabilities {
        self.setup().dialect.capabilities()
    }

    async fn call(
        &self,
        request: CompletionRequest,
        context: &CallContext,
        tx: EmitTracker,
    ) -> Result<CallReport, BackendError> {
        let started = Instant::now();
        let setup = self.setup();
        let model = self.model_for(context);
        if let Err(reason) = check_model(&model) {
            crate::debug_log::log("gateway.cli.refused", &reason);
            return Err(self.error(FailureClass::BadRequest, reason, false));
        }
        let (system, rest) = split_system_prompt(&request.messages);
        let system = if system.trim().is_empty() {
            FALLBACK_SYSTEM.to_string()
        } else {
            system
        };
        let prompt = build_prompt(&rest, "", SystemDelivery::Fresh);
        let prompt_chars = prompt.chars().count() + system.chars().count();

        let io_error = |what: &str, error: std::io::Error| {
            self.error(FailureClass::Server, format!("{what}: {error}"), false)
        };
        let mut scratch = Scratch::create(&setup.scratch_root)
            .map_err(|e| io_error("cannot prepare the working folder", e))?;
        if setup.keep_scratch {
            scratch.keep();
        }
        let system_file = scratch
            .write_system(&system)
            .map_err(|e| io_error("cannot write the system prompt", e))?;
        let plan = Plan {
            program: &setup.program,
            home: setup.home.as_deref(),
            model: &model,
            system_file: &system_file,
            prompt,
            scratch: &scratch,
            parent_env: std::env::vars_os().collect(),
        };
        let launch = setup.dialect.launch(&plan);
        // Потерянный флаг изоляции не должен пройти молча: лучше отказ, чем агент с инструментами.
        if let Err(reason) = setup.dialect.verify(&launch) {
            crate::debug_log::log("gateway.cli.refused", &reason);
            return Err(self.error(FailureClass::Server, reason, false));
        }

        let mut parser = setup.dialect.parser(&setup.account);
        let mut first_text: Option<Instant> = None;
        let mut reply_chars = 0usize;
        let ran = run_streaming(&launch, &self.inner.limits, |line| {
            let feed = parser.feed(line);
            if !feed.text.is_empty() {
                first_text.get_or_insert_with(Instant::now);
                reply_chars += feed.text.chars().count();
                let chunk = CompletionChunk {
                    content: feed.text,
                    ..CompletionChunk::default()
                };
                if !tx.send(chunk) {
                    return Flow::Stop;
                }
            }
            if feed.done {
                Flow::Done
            } else {
                Flow::Continue
            }
        })
        .await;
        drop(scratch);
        let elapsed = started.elapsed();
        let limits = parser.limits();
        if limits.is_some() {
            self.learn(&model, None, limits.as_ref());
        }

        let outcome = match parser.terminal() {
            Some(terminal) => terminal.map_err(|failure| {
                if failure.unrecognized {
                    crate::debug_log::log_json(
                        "gateway.cli.unrecognized_failure",
                        &serde_json::json!({
                            "account": setup.account.as_str(),
                            "status": failure.status,
                            "text": crate::util::truncate_at_char_boundary(&failure.message, LOG_TEXT_CAP),
                        }),
                    );
                }
                BackendError {
                    class: failure.class,
                    status: failure.status,
                    message: cap_message(&failure.message),
                    emitted: tx.emitted(),
                    limits: failure.limits,
                }
            }),
            None => {
                let (class, message) = end_failure(&ran.end, &ran.stderr, tx.emitted());
                Err(BackendError {
                    class,
                    status: None,
                    message: cap_message(&message),
                    emitted: tx.emitted(),
                    limits,
                })
            }
        };
        crate::debug_log::log_json(
            "gateway.cli.finished",
            &serde_json::json!({
                "account": setup.account.as_str(),
                "tool": format!("{:?}", setup.dialect.tool()),
                "end": format!("{:?}", ran.end),
                "lines": ran.lines,
                "ms": elapsed.as_millis() as u64,
                "ok": outcome.is_ok(),
                "class": outcome.as_ref().err().map(|e| e.class.code()),
                "stderr": crate::util::truncate_at_char_boundary(ran.stderr.trim(), 1000),
            }),
        );

        let success = outcome?;
        if let Some(note) = &success.note {
            crate::debug_log::log("gateway.cli.note", note);
        }
        let (usage, usage_estimated) = if success.usage.total_tokens == 0 {
            // CLI ничего не сообщил: честная оценка с пометкой, а не нули под видом данных.
            let prompt_tokens = estimate(prompt_chars);
            let completion_tokens = estimate(reply_chars);
            (
                crate::provider::Usage {
                    prompt_tokens,
                    completion_tokens,
                    total_tokens: prompt_tokens + completion_tokens,
                    ..crate::provider::Usage::default()
                },
                true,
            )
        } else {
            (success.usage.clone(), false)
        };
        // Конец ответа и расход — одним терминальным чанком, как у Anthropic.
        tx.send(CompletionChunk {
            finish_reason: Some(success.finish_reason.clone()),
            usage: Some(usage.clone()),
            ..CompletionChunk::default()
        });
        self.learn(&model, success.context_window, success.limits.as_ref());
        Ok(CallReport {
            usage: Some(usage),
            usage_estimated,
            limits: success.limits,
            finish_reason: Some(success.finish_reason),
            context_window: success.context_window,
            ttft: first_text.map(|at| at.duration_since(started)),
            elapsed,
            upstream_id: success.upstream_id,
        })
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, BackendError> {
        let info = |id: &str, window: Option<u32>| ModelInfo {
            id: id.to_string(),
            context_window: window,
            max_output: None,
            supports_tools: None,
        };
        let windows = self
            .inner
            .learned
            .lock()
            .map(|learned| (learned.windows.clone(), learned.seen_models.clone()))
            .unwrap_or_default();
        match self.setup().dialect.models() {
            ModelSource::Aliases(names) => {
                let mut ids: Vec<String> = names.iter().map(|n| n.to_string()).collect();
                let extra: Vec<String> = windows
                    .1
                    .into_iter()
                    .filter(|seen| !ids.contains(seen))
                    .collect();
                ids.extend(extra);
                Ok(ids
                    .iter()
                    .map(|id| info(id, windows.0.get(id).copied()))
                    .collect())
            }
            ModelSource::Command(args) => {
                let captured = self
                    .side_command(args)
                    .await
                    .map_err(|m| self.error(FailureClass::Server, m, false))?;
                if let Some(failure) = captured.failure {
                    return Err(self.error(FailureClass::Server, failure, false));
                }
                if captured.exit != Some(0) {
                    let detail = cap_message(&captured.stderr);
                    return Err(self.error(
                        FailureClass::Server,
                        format!("model list failed (exit {:?}): {detail}", captured.exit),
                        false,
                    ));
                }
                Ok(captured
                    .stdout
                    .lines()
                    .map(str::trim)
                    // Идентификатор модели — `провайдер/модель`; баннеры и предупреждения не берём.
                    .filter(|line| line.contains('/') && !line.contains(char::is_whitespace))
                    .map(|id| info(id, windows.0.get(id).copied()))
                    .collect())
            }
        }
    }

    async fn probe(&self) -> ProbeReport {
        let setup = self.setup();
        let (auth, detail) = match setup.dialect.auth_check() {
            Some(check) => match self.side_command(check.args).await {
                Ok(captured) if captured.failure.is_none() => {
                    ((check.parse)(&captured.stdout), None)
                }
                Ok(captured) => (AuthState::Unknown, captured.failure),
                Err(message) => (AuthState::Unknown, Some(message)),
            },
            None => (
                AuthState::Unknown,
                Some("this CLI has no login check".to_string()),
            ),
        };
        let limits = self
            .inner
            .learned
            .lock()
            .ok()
            .and_then(|l| l.limits.clone());
        debug_assert!(SIDE_COMMAND_TIMEOUT < PROBE_TIMEOUT);
        ProbeReport {
            auth,
            limits,
            version: setup.version.map(|v| v.to_string()),
            detail,
            checked_at: SystemTime::now(),
        }
    }

    fn fork(&self) -> Arc<dyn Backend> {
        Arc::new(self.clone())
    }

    async fn context_window(&self, model: &str) -> Option<u32> {
        let model = if model.is_empty() {
            self.setup().default_model.clone().unwrap_or_default()
        } else {
            model.to_string()
        };
        self.inner.learned.lock().ok()?.windows.get(&model).copied()
    }
}
