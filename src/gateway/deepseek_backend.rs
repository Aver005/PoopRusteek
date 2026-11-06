//! Адаптер встроенного веб-клиента DeepSeek: у него серверная сессия, поэтому форк —
//! новая сессия, а уход с аккаунта удаляет прежнюю (`discard_session`).
//!
//! Расход DeepSeek не сообщает: он всегда оценка и в интерфейсе помечается `~`.

use super::api_backend::drive_provider;
use super::backend::{
    AuthState, Backend, BackendError, BackendKind, CallContext, CallReport, Capabilities,
    EmitTracker, ModelInfo, PROBE_TIMEOUT, ProbeReport, UsageSupport,
};
use super::types::AccountId;
use crate::error::AppResult;
use crate::provider::failure::FailureClass;
use crate::provider::{
    ChatMessage, CompletionRequest, LLMProvider, RemoteSessionInfo, SessionLiveness,
};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::SystemTime;

pub struct DeepseekBackend {
    account: AccountId,
    provider: Arc<dyn LLMProvider>,
}

impl DeepseekBackend {
    pub fn new(account: AccountId, provider: Arc<dyn LLMProvider>) -> Self {
        Self { account, provider }
    }

    fn app_error(&self, error: &crate::error::AppError) -> BackendError {
        BackendError::from_app(&self.account, error, false)
    }
}

#[async_trait]
impl Backend for DeepseekBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::DeepseekWeb
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            native_tools: false,
            pure_model: true,
            streaming: true,
            usage: UsageSupport::Estimated,
            reports_limits: false,
            server_side_session: true,
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
        // Режим DeepSeek выбирается по id модели; пустой id оставляет модель запроса.
        if !context.model.is_empty() {
            request.model = context.model.clone();
        }
        request.tools.clear();
        let driven = drive_provider(&self.provider, request, &tx, &context.account).await?;
        Ok(CallReport {
            usage: Some(driven.usage),
            usage_estimated: true,
            limits: None,
            finish_reason: driven.finish_reason,
            context_window: None,
            ttft: driven.ttft,
            elapsed: driven.elapsed,
            upstream_id: None,
        })
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, BackendError> {
        let ids = self
            .provider
            .list_models()
            .await
            .map_err(|error| self.app_error(&error))?;
        Ok(ids
            .into_iter()
            .map(|id| ModelInfo {
                id,
                context_window: None,
                max_output: None,
                supports_tools: Some(false),
            })
            .collect())
    }

    /// Дешёвая проба: список сессий аккаунта. Просроченный токен виден по классу отказа.
    async fn probe(&self) -> ProbeReport {
        let checked_at = SystemTime::now();
        let listed =
            tokio::time::timeout(PROBE_TIMEOUT, self.provider.list_remote_sessions()).await;
        let (auth, detail) = match listed {
            Ok(Ok(_)) => (AuthState::Ok, None),
            Ok(Err(error)) => match error.failure_class() {
                Some(FailureClass::Auth | FailureClass::AccountBlocked) => {
                    (AuthState::NeedsLogin, Some(error.to_string()))
                }
                _ => (AuthState::Unknown, Some(error.to_string())),
            },
            Err(_) => (AuthState::Unknown, Some("probe timed out".to_string())),
        };
        ProbeReport {
            auth,
            limits: None,
            version: None,
            detail,
            checked_at,
        }
    }

    fn fork(&self) -> Arc<dyn Backend> {
        Arc::new(Self {
            account: self.account.clone(),
            provider: self.provider.fork(),
        })
    }

    fn accepts_attachment(&self, path: &std::path::Path) -> bool {
        self.provider.accepts_attachment(path)
    }

    fn session_identity(&self) -> Option<(String, Option<i64>)> {
        self.provider.session_identity()
    }

    fn session_tokens(&self) -> Option<u32> {
        self.provider.session_tokens()
    }

    async fn discard_session(&self) {
        if let Err(error) = self.provider.discard_remote_session().await {
            tracing::debug!("deepseek session discard failed: {error}");
        }
    }

    async fn list_remote_sessions(&self) -> AppResult<Vec<RemoteSessionInfo>> {
        self.provider.list_remote_sessions().await
    }

    async fn delete_remote_session_by_id(&self, session_id: &str) -> AppResult<()> {
        self.provider.delete_remote_session_by_id(session_id).await
    }

    async fn fetch_remote_session_messages(&self, session_id: &str) -> AppResult<Vec<ChatMessage>> {
        self.provider
            .fetch_remote_session_messages(session_id)
            .await
    }

    async fn check_session(&self, session_id: &str) -> SessionLiveness {
        self.provider.check_session(session_id).await
    }

    async fn adopt_session(&self, session_id: &str, parent: Option<i64>) -> AppResult<()> {
        self.provider.adopt_session(session_id, parent).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::types::ToolMode;
    use crate::provider::fake::FakeProvider;

    #[test]
    fn the_adapter_is_a_prompt_only_pure_model_with_a_server_session_and_estimated_usage() {
        let backend = DeepseekBackend::new(
            AccountId::parse("deepseek").unwrap(),
            Arc::new(FakeProvider::with_responses(Vec::new())),
        );
        let caps = backend.capabilities();
        assert!(caps.server_side_session && caps.pure_model && !caps.native_tools);
        assert_eq!(caps.usage, UsageSupport::Estimated);
        assert!(caps.usable_for_tool_turn(ToolMode::Prompt));
        assert!(!caps.usable_for_tool_turn(ToolMode::Native));
    }
}
