use crate::app::events::{AgentResult, AppEvent};
use crate::agent::context::ContextManager;
use crate::error::AppResult;
use crate::provider::{CompletionRequest, LLMProvider, ChatMessage};
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct AgentLoop {
    provider: Arc<dyn LLMProvider>,
    context: ContextManager,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    max_steps: usize,
    max_tools_per_step: usize,
}

impl AgentLoop {
    pub fn new(
        provider: Arc<dyn LLMProvider>,
        context: ContextManager,
        event_tx: mpsc::UnboundedSender<AppEvent>,
        max_steps: usize,
        max_tools_per_step: usize,
    ) -> Self {
        Self {
            provider,
            context,
            event_tx,
            max_steps,
            max_tools_per_step,
        }
    }

    pub async fn run(&mut self, user_input: &str) -> AppResult<AgentResult> {
        self.context.add_message(ChatMessage::user(user_input));

        let _ = self.event_tx.send(AppEvent::AgentStarted);

        let messages = self.context.build_messages();
        let request = CompletionRequest {
            messages,
            model: self.provider.model().to_string(),
            temperature: 0.7,
            max_tokens: 4096,
            stream: true,
        };

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let provider = Arc::clone(&self.provider);
        let stream_handle = tokio::spawn(async move {
            provider.complete_stream(request, tx).await
        });

        let mut full_response = String::new();

        while let Some(chunk) = rx.recv().await {
            if !chunk.content.is_empty() {
                full_response.push_str(&chunk.content);
                let _ = self.event_tx.send(AppEvent::AgentChunk(chunk.content));
            }
            if chunk.finish_reason.is_some() {
                break;
            }
        }

        stream_handle.await??;

        self.context.add_message(ChatMessage::assistant(&full_response));

        let _ = self.event_tx.send(AppEvent::AgentDone(AgentResult {
            text: full_response.clone(),
            tool_calls: Vec::new(),
        }));

        Ok(AgentResult {
            text: full_response,
            tool_calls: Vec::new(),
        })
    }

    pub fn context_mut(&mut self) -> &mut ContextManager {
        &mut self.context
    }

    pub fn context(&self) -> &ContextManager {
        &self.context
    }
}
