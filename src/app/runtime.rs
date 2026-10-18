//! The agent-execution controller.
//!
//! `AgentRuntime` owns the shared dependencies needed to run an agent turn —
//! the tool registry, the MCP manager, and the event channel — and is the one
//! place that launches the agent loop. Callers (a normal turn, a sidechat, a
//! sub-agent) just describe the turn with a [`TurnSpec`]; they no longer reach
//! into the runner with a dozen positional arguments.

use crate::app::conversation::ConversationId;
use crate::app::events::{AgentEvent, AppEvent};
use crate::mcp::MCPManager;
use crate::provider::{ChatMessage, LLMProvider};
use crate::tools::registry::ToolRegistry;
use futures::FutureExt;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Everything needed to launch one agent turn.
pub struct TurnSpec {
    pub conversation: ConversationId,
    pub provider: Arc<dyn LLMProvider>,
    pub messages: Vec<ChatMessage>,
    pub system_prompt: String,
    pub model: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub max_steps: usize,
    pub max_tools_per_step: usize,
    /// Background turns (sidechats / sub-agents) auto-approve tools so they
    /// never block on an approval modal.
    pub auto_approve: bool,
    /// Chars kept per tool result before it enters the history (rung 0 of
    /// the compaction ladder). `0` = uncapped.
    pub tool_output_limit: usize,
    /// Rung 1: window settings and spill directory for clearing old tool
    /// bodies. A default `ContextSpec` keeps the ladder off.
    pub context: crate::context::ContextSpec,
}

/// Owns the dependencies for running agent turns and centralizes spawning.
pub struct AgentRuntime {
    tools: Arc<ToolRegistry>,
    mcp: Arc<tokio::sync::Mutex<MCPManager>>,
    semantic: Arc<crate::semantic::SemanticService>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
}

impl AgentRuntime {
    pub fn new(
        tools: Arc<ToolRegistry>,
        mcp: Arc<tokio::sync::Mutex<MCPManager>>,
        semantic: Arc<crate::semantic::SemanticService>,
        event_tx: mpsc::UnboundedSender<AppEvent>,
    ) -> Self {
        Self {
            tools,
            mcp,
            semantic,
            event_tx,
        }
    }

    /// Запустить ход в своей задаче; хэндл нужен для отмены. Паника хода
    /// становится `Failed`: без него TUI крутил «Thinking» вечно.
    pub fn spawn(&self, spec: TurnSpec) -> JoinHandle<()> {
        let conversation = spec.conversation;
        let event_tx = self.event_tx.clone();
        let turn = crate::agent::runner::run_agent_loop(
            spec,
            Arc::clone(&self.tools),
            Arc::clone(&self.mcp),
            Arc::clone(&self.semantic),
            self.event_tx.clone(),
        );
        tokio::spawn(async move {
            if let Err(panic) = AssertUnwindSafe(turn).catch_unwind().await {
                let reason = panic_text(panic.as_ref());
                tracing::error!("agent task panicked: {reason}");
                crate::debug_log::log("agent.panic", &reason);
                // Текст паники — только в журналы: в истории он ушёл бы модели.
                let _ = event_tx.send(AppEvent::Agent {
                    conversation,
                    event: AgentEvent::Failed(
                        "Internal error: the agent task crashed (details are in the log). Tool calls made before the crash were kept."
                            .to_string(),
                    ),
                });
            }
        })
    }
}

/// Текст паники: `panic!` кладёт `&str` или `String`, остальное безымянно.
fn panic_text(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|text| (*text).to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::fake::FakeProvider;

    struct Boom;

    #[async_trait::async_trait]
    impl crate::tools::Tool for Boom {
        fn definition(&self) -> crate::tools::ToolDefinition {
            crate::tools::ToolDefinition {
                name: "boom".to_string(),
                description: "panics".to_string(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }
        }

        fn requires_approval(&self) -> bool {
            false
        }

        async fn execute(&self, _args: serde_json::Value) -> crate::tools::ToolResult {
            panic!("secret-token-123 in a panic message");
        }
    }

    /// Паника посреди хода заканчивает его `Failed`, а текст паники в историю
    /// (и дальше — модели) не уходит.
    #[tokio::test]
    async fn a_panicking_turn_ends_failed_without_leaking_the_panic_text() {
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(Boom));
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let runtime = AgentRuntime::new(
            tools,
            Arc::new(tokio::sync::Mutex::new(MCPManager::new())),
            crate::semantic::SemanticService::disabled(),
            event_tx,
        );
        let conversation = ConversationId::next();
        let call = "<tool_use><name>boom</name><arguments>{}</arguments></tool_use>";
        runtime.spawn(TurnSpec {
            conversation,
            provider: Arc::new(FakeProvider::with_response(call)),
            messages: vec![ChatMessage::user("go")],
            system_prompt: "system".to_string(),
            model: "fake".to_string(),
            temperature: 0.0,
            max_tokens: 128,
            max_steps: 4,
            max_tools_per_step: 4,
            auto_approve: false,
            tool_output_limit: 0,
            context: crate::context::ContextSpec::default(),
        });

        let failed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(event) = event_rx.recv().await {
                if let AppEvent::Agent {
                    conversation: id,
                    event: AgentEvent::Failed(reason),
                } = event
                    && id == conversation
                {
                    return reason;
                }
            }
            panic!("channel closed without a terminal event");
        })
        .await
        .expect("a panicking turn must still end");
        assert!(failed.contains("crashed"), "{failed}");
        assert!(!failed.contains("secret-token-123"), "{failed}");
    }
}
