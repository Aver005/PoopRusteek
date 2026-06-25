use super::types::*;
use crate::error::AppResult;
use crate::provider::{ChatMessage, CompletionRequest, LLMProvider};
use std::io::{BufRead, Write};
use std::sync::Arc;

pub struct AcpServer {
    provider: Arc<dyn LLMProvider>,
    messages: Vec<ChatMessage>,
}

impl AcpServer {
    pub fn new(provider: Arc<dyn LLMProvider>) -> Self {
        Self {
            provider,
            messages: Vec::new(),
        }
    }

    pub fn run(&mut self) -> AppResult<()> {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let mut reader = stdin.lock();
        let mut writer = stdout.lock();

        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => {
                    eprintln!("Error reading input: {e}");
                    break;
                }
            }

            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let request: AcpRequest = match serde_json::from_str(line) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("Invalid JSON: {e}");
                    continue;
                }
            };

            let response = self.handle_request(request);

            if let Some(resp) = response {
                let json = serde_json::to_string(&resp).unwrap_or_default();
                writeln!(writer, "{json}").ok();
                writer.flush().ok();
            }
        }

        Ok(())
    }

    fn handle_request(&mut self, request: AcpRequest) -> Option<AcpResponse> {
        match request.method.as_str() {
            "initialize" => {
                let result = InitializeResult {
                    serverInfo: ServerInfo {
                        name: "pooprusteek".to_string(),
                        version: env!("CARGO_PKG_VERSION").to_string(),
                    },
                    capabilities: serde_json::json!({
                        "prompt": true,
                    }),
                };
                let result_value = match serde_json::to_value(result) {
                    Ok(v) => v,
                    Err(e) => return Some(AcpResponse {
                        jsonrpc: "2.0".to_string(),
                        id: request.id,
                        result: None,
                        error: Some(AcpError {
                            code: -32603,
                            message: format!("Internal error: {e}"),
                        }),
                    }),
                };
                Some(AcpResponse {
                    jsonrpc: "2.0".to_string(),
                    id: request.id,
                    result: Some(result_value),
                    error: None,
                })
            }
            "initialized" => None,
            "prompt" => {
                let params: PromptRequest = serde_json::from_value(
                    request.params.unwrap_or(serde_json::json!({}))
                ).unwrap_or(PromptRequest {
                    prompt: String::new(),
                    images: Vec::new(),
                });

                let rt = tokio::runtime::Runtime::new().ok()?;
                let result = rt.block_on(self.handle_prompt(&params));

                let result_value = match serde_json::to_value(result) {
                    Ok(v) => v,
                    Err(e) => return Some(AcpResponse {
                        jsonrpc: "2.0".to_string(),
                        id: request.id,
                        result: None,
                        error: Some(AcpError {
                            code: -32603,
                            message: format!("Internal error: {e}"),
                        }),
                    }),
                };
                Some(AcpResponse {
                    jsonrpc: "2.0".to_string(),
                    id: request.id,
                    result: Some(result_value),
                    error: None,
                })
            }
            "ping" => Some(AcpResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: Some(serde_json::json!({})),
                error: None,
            }),
            _ => Some(AcpResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: None,
                error: Some(AcpError {
                    code: -32601,
                    message: format!("Unknown method: {}", request.method),
                }),
            }),
        }
    }

    async fn handle_prompt(&mut self, request: &PromptRequest) -> PromptResult {
        self.messages.push(ChatMessage::user(&request.prompt));

        let system_prompt = "You are Pooprusteek, a helpful AI coding assistant.";

        let mut messages = vec![ChatMessage::system(system_prompt)];
        messages.extend(self.messages.clone());

        let request = CompletionRequest {
            messages,
            model: self.provider.model().to_string(),
            temperature: 0.7,
            max_tokens: 4096,
            stream: false,
        };

        match self.provider.complete(request).await {
            Ok(response) => {
                self.messages.push(ChatMessage::assistant(&response.content));
                PromptResult {
                    content: vec![ContentBlock::Text { text: response.content }],
                }
            }
            Err(e) => PromptResult {
                content: vec![ContentBlock::Text {
                    text: format!("Error: {e}"),
                }],
            },
        }
    }
}
