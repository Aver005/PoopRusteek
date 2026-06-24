use super::*;
use crate::config::ProviderConfig;
use crate::error::AppResult;
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;

const DEEPSEEK_CHAT_URL: &str = "https://chat.deepseek.com";
const DEEPSEEK_API_URL: &str = "https://chat.deepseek.com/api/v0/chat";
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

pub struct DeepseekProvider {
    client: Client,
    token: String,
    model: String,
    temperature: f32,
    max_tokens: u32,
}

impl DeepseekProvider {
    pub fn new(config: &ProviderConfig) -> AppResult<Self> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .build()?;

        Ok(Self {
            client,
            token: config.token.clone(),
            model: config.model.clone(),
            temperature: config.temperature,
            max_tokens: config.max_tokens,
        })
    }

    fn headers(&self) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("authorization", format!("Bearer {}", self.token).parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("origin", DEEPSEEK_CHAT_URL.parse().unwrap());
        headers.insert("referer", format!("{DEEPSEEK_CHAT_URL}/").parse().unwrap());
        headers
    }

    async fn create_pow_challenge(&self) -> AppResult<String> {
        let resp = self.client
            .post(format!("{DEEPSEEK_API_URL}/create_pow_challenge"))
            .headers(self.headers())
            .body("{}")
            .send()
            .await?;

        let _challenge: serde_json::Value = resp.json().await?;
        // TODO: Implement actual PoW solution
        // For now, return empty string
        Ok(String::new())
    }
}

#[async_trait]
impl LLMProvider for DeepseekProvider {
    async fn complete(&self, request: CompletionRequest) -> AppResult<CompletionResponse> {
        let messages: Vec<serde_json::Value> = request.messages
            .iter()
            .map(|m| {
                let mut obj = serde_json::json!({
                    "role": match m.role {
                        Role::System => "system",
                        Role::User => "user",
                        Role::Assistant => "assistant",
                        Role::Tool => "tool",
                    },
                    "content": m.content,
                });
                if let Some(ref name) = m.name {
                    obj["name"] = serde_json::json!(name);
                }
                if let Some(ref id) = m.tool_call_id {
                    obj["tool_call_id"] = serde_json::json!(id);
                }
                obj
            })
            .collect();

        let body = serde_json::json!({
            "messages": messages,
            "model": self.model,
            "temperature": self.temperature,
            "max_tokens": self.max_tokens,
            "stream": false,
        });

        let resp = self.client
            .post(format!("{DEEPSEEK_API_URL}/completion"))
            .headers(self.headers())
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(crate::error::AppError::Provider(
                format!("HTTP {status}: {text}")
            ));
        }

        let raw: serde_json::Value = resp.json().await?;
        let content = raw["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();

        Ok(CompletionResponse {
            content,
            finish_reason: raw["choices"][0]["finish_reason"]
                .as_str()
                .map(|s| s.to_string()),
            usage: raw["usage"].as_object().map(|u| Usage {
                prompt_tokens: u["prompt_tokens"].as_u64().unwrap_or(0) as u32,
                completion_tokens: u["completion_tokens"].as_u64().unwrap_or(0) as u32,
                total_tokens: u["total_tokens"].as_u64().unwrap_or(0) as u32,
            }),
        })
    }

    async fn complete_stream(
        &self,
        request: CompletionRequest,
        tx: tokio::sync::mpsc::UnboundedSender<CompletionChunk>,
    ) -> AppResult<()> {
        let messages: Vec<serde_json::Value> = request.messages
            .iter()
            .map(|m| {
                serde_json::json!({
                    "role": match m.role {
                        Role::System => "system",
                        Role::User => "user",
                        Role::Assistant => "assistant",
                        Role::Tool => "tool",
                    },
                    "content": m.content,
                })
            })
            .collect();

        let body = serde_json::json!({
            "messages": messages,
            "model": self.model,
            "temperature": self.temperature,
            "max_tokens": self.max_tokens,
            "stream": true,
        });

        let resp = self.client
            .post(format!("{DEEPSEEK_API_URL}/completion"))
            .headers(self.headers())
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(crate::error::AppError::Provider(
                format!("HTTP {status}: {text}")
            ));
        }

        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(line_end) = buffer.find('\n') {
                let line = buffer[..line_end].trim().to_string();
                buffer = buffer[line_end + 1..].to_string();

                if line.is_empty() || !line.starts_with("data:") {
                    continue;
                }

                let data = line[5..].trim();
                if data == "[DONE]" {
                    let _ = tx.send(CompletionChunk {
                        content: String::new(),
                        finish_reason: Some("stop".to_string()),
                    });
                    return Ok(());
                }

                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(data) {
                    let content = parsed["choices"][0]["delta"]["content"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();

                    let finish_reason = parsed["choices"][0]["finish_reason"]
                        .as_str()
                        .map(|s| s.to_string());

                    if !content.is_empty() || finish_reason.is_some() {
                        let _ = tx.send(CompletionChunk {
                            content,
                            finish_reason,
                        });
                    }
                }
            }
        }

        Ok(())
    }

    fn name(&self) -> &str {
        "deepseek"
    }

    fn model(&self) -> &str {
        &self.model
    }
}
