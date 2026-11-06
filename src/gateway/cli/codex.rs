//! Codex: в режиме модели не поддерживается, в ходы с нашими инструментами не берётся.
//! Пользовательскими средствами его инструменты не выключить (`06-cli-spike.md` §3).

use crate::gateway::backend::{
    AuthState, Backend, BackendError, BackendKind, CallContext, CallReport, Capabilities,
    EmitTracker, ModelInfo, ProbeReport, UsageSupport,
};
use crate::gateway::types::{CliTool, CliTransport};
use crate::provider::CompletionRequest;
use crate::provider::failure::FailureClass;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::SystemTime;

/// Причина, которую видят `doctor` и ошибка вызова.
pub const REASON: &str = "Codex cannot run as a plain model (its own tools cannot be switched off); \
    it is for agent mode, planned for a later release";

pub struct CodexBackend;

#[async_trait]
impl Backend for CodexBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Cli {
            tool: CliTool::Codex,
            transport: CliTransport::OneShot,
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            native_tools: false,
            pure_model: false,
            streaming: false,
            usage: UsageSupport::None,
            reports_limits: false,
            server_side_session: false,
            slow_start: true,
            reports_context_window: false,
        }
    }

    async fn call(
        &self,
        _request: CompletionRequest,
        _context: &CallContext,
        _tx: EmitTracker,
    ) -> Result<CallReport, BackendError> {
        // Сюда движок не доходит (`NotPureModel`); защита от прямого вызова.
        Err(BackendError {
            class: FailureClass::BadRequest,
            status: None,
            message: REASON.to_string(),
            emitted: false,
            limits: None,
        })
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, BackendError> {
        Ok(Vec::new())
    }

    async fn probe(&self) -> ProbeReport {
        ProbeReport {
            auth: AuthState::Unknown,
            limits: None,
            version: None,
            detail: Some(REASON.to_string()),
            checked_at: SystemTime::now(),
        }
    }

    fn fork(&self) -> Arc<dyn Backend> {
        Arc::new(CodexBackend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::types::ToolMode;

    #[test]
    fn codex_never_serves_a_tool_turn_and_says_why() {
        let caps = CodexBackend.capabilities();
        assert!(!caps.pure_model);
        assert!(!caps.usable_for_tool_turn(ToolMode::Prompt));
        assert!(REASON.contains("agent mode"));
    }
}
