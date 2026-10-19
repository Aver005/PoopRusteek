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
    /// За ходом не следит человек: вопросы, таймеры, вложенные `task` и вызовы
    /// в чужой разметке отклоняются.
    pub unattended: bool,
    /// Chars kept per tool result before it enters the history (rung 0 of
    /// the compaction ladder). `0` = uncapped.
    pub tool_output_limit: usize,
    /// Rung 1: window settings and spill directory for clearing old tool
    /// bodies. A default `ContextSpec` keeps the ladder off.
    pub context: crate::context::ContextSpec,
}

impl TurnSpec {
    /// Фоновый ход без человека (`/btw`, фоновый `task`) — один рецепт для TUI
    /// и харнесса: шагов не больше потолка, в промпте — рассказ о положении.
    pub fn background(
        conversation: ConversationId,
        provider: Arc<dyn LLMProvider>,
        prompt: &str,
        system_prompt: &str,
        config: &crate::config::Config,
        context: crate::context::ContextSpec,
    ) -> Self {
        let max_steps = config
            .agent
            .max_steps_per_turn
            .clamp(1, crate::agent::SUB_AGENT_MAX_STEPS);
        Self {
            conversation,
            provider,
            messages: vec![ChatMessage::user(prompt)],
            system_prompt: format!(
                "{system_prompt}\n\n{}",
                crate::agent::sub_agent::unattended_note(max_steps)
            ),
            model: config.provider.model.clone(),
            temperature: config.provider.temperature,
            max_tokens: config.provider.max_tokens,
            max_steps,
            max_tools_per_step: config.agent.max_tools_per_step.max(1),
            unattended: true,
            tool_output_limit: config.context.tool_output_limit as usize,
            context: context.with_output_cap(config.provider.max_tokens),
        }
    }
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

    /// Запустить ход в своей задаче; хэндл нужен для отмены.
    pub fn spawn(&self, spec: TurnSpec) -> JoinHandle<()> {
        tokio::spawn(run_guarded(
            spec,
            Arc::clone(&self.tools),
            Arc::clone(&self.mcp),
            Arc::clone(&self.semantic),
            self.event_tx.clone(),
        ))
    }
}

/// Единственная обёртка над `run_agent_loop`: и для `spawn`, и для суб-агента
/// `task`, который идёт внутри future родителя. Паника хода становится `Failed`.
pub(crate) async fn run_guarded(
    spec: TurnSpec,
    tools: Arc<ToolRegistry>,
    mcp: Arc<tokio::sync::Mutex<MCPManager>>,
    semantic: Arc<crate::semantic::SemanticService>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) {
    let conversation = spec.conversation;
    let turn = crate::agent::runner::run_agent_loop(spec, tools, mcp, semantic, event_tx.clone());
    // Фоновые процессы хода помечены его чатом — так их гасят точечно.
    let turn = crate::tools::background::with_owner(conversation.0, turn);
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
            unattended: false,
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
