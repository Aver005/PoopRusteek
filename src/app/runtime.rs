//! The agent-execution controller.
//!
//! `AgentRuntime` owns the shared dependencies needed to run an agent turn —
//! the tool registry, the MCP manager, and the event channel — and is the one
//! place that launches the agent loop. Callers (a normal turn, a sidechat, a
//! sub-agent) just describe the turn with a [`TurnSpec`]; they no longer reach
//! into the runner with a dozen positional arguments.

use crate::app::conversation::ConversationId;
use crate::app::events::AppEvent;
use crate::mcp::MCPManager;
use crate::provider::{ChatMessage, LLMProvider};
use crate::tools::registry::ToolRegistry;
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

    /// Launch a turn on its own task; the returned handle drives cancellation.
    pub fn spawn(&self, spec: TurnSpec) -> JoinHandle<()> {
        tokio::spawn(crate::agent::runner::run_agent_loop(
            spec,
            Arc::clone(&self.tools),
            Arc::clone(&self.mcp),
            Arc::clone(&self.semantic),
            self.event_tx.clone(),
        ))
    }
}
