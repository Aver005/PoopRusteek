use crate::provider::{CompletionChunk, LLMProvider, CompletionRequest, ChatMessage};
use crate::error::AppResult;
use tokio::sync::mpsc;

pub struct StreamingResponse {
    receiver: mpsc::UnboundedReceiver<CompletionChunk>,
    buffer: String,
}

impl StreamingResponse {
    pub async fn new(
        provider: std::sync::Arc<dyn LLMProvider>,
        messages: Vec<ChatMessage>,
        model: String,
        temperature: f32,
        max_tokens: u32,
    ) -> AppResult<Self> {
        let (tx, rx) = mpsc::unbounded_channel();

        let request = CompletionRequest {
            messages,
            model,
            temperature,
            max_tokens,
            stream: true,
        };

        let provider_name = provider.name().to_string();
        tokio::spawn(async move {
            if let Err(e) = provider.complete_stream(request, tx.clone()).await {
                tracing::error!("Stream error from {provider_name}: {e}");
            }
        });

        Ok(Self {
            receiver: rx,
            buffer: String::new(),
        })
    }

    pub async fn next_chunk(&mut self) -> Option<String> {
        while let Some(chunk) = self.receiver.recv().await {
            if !chunk.content.is_empty() {
                self.buffer.push_str(&chunk.content);
                return Some(chunk.content);
            }
            if chunk.finish_reason.is_some() {
                return None;
            }
        }
        None
    }

    pub fn buffer(&self) -> &str {
        &self.buffer
    }
}
