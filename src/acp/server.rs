use super::types::*;
use crate::error::AppResult;
use crate::provider::{ChatMessage, CompletionRequest, LLMProvider};
use std::io::{BufRead, Write};
use std::sync::Arc;

/// Коды ошибок JSON-RPC 2.0.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
/// Сбой провайдера — своя ошибка сервера, а не успешный `result` с «Error:».
const PROVIDER_ERROR: i64 = -32000;

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
                    tracing::error!("ACP: failed to read stdin: {e}");
                    break;
                }
            }

            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            if let Some(response) = self.handle_line(line) {
                match serde_json::to_string(&response) {
                    Ok(json) => {
                        writeln!(writer, "{json}").ok();
                        writer.flush().ok();
                    }
                    Err(e) => {
                        tracing::error!("ACP: failed to serialize a response: {e}");
                    }
                }
            }
        }

        Ok(())
    }

    /// Одна строка входа → ответ. Битый JSON и неверный запрос тоже получают
    /// ответ: без него клиент ждал вечно.
    fn handle_line(&mut self, line: &str) -> Option<AcpResponse> {
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(e) => {
                return Some(Self::error(None, PARSE_ERROR, format!("Parse error: {e}")));
            }
        };
        match serde_json::from_value::<AcpRequest>(value.clone()) {
            Ok(mut request) => {
                // `"id": null` — тоже запрос: уведомление — это когда поля нет.
                if request.id.is_none() && value.get("id").is_some() {
                    request.id = Some(serde_json::Value::Null);
                }
                self.handle_request(request)
            }
            Err(e) => Some(Self::error(
                value.get("id").cloned(),
                INVALID_REQUEST,
                format!("Invalid request: {e}"),
            )),
        }
    }

    /// Успешный ответ на запрос `id`, либо внутренняя ошибка, если результат
    /// не сериализуется.
    fn reply(id: Option<serde_json::Value>, result: impl serde::Serialize) -> AcpResponse {
        match serde_json::to_value(result) {
            Ok(value) => AcpResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(value),
                error: None,
            },
            Err(e) => Self::error(id, INTERNAL_ERROR, format!("Internal error: {e}")),
        }
    }

    fn error(id: Option<serde_json::Value>, code: i64, message: String) -> AcpResponse {
        AcpResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(AcpError { code, message }),
        }
    }

    fn handle_request(&mut self, request: AcpRequest) -> Option<AcpResponse> {
        // Уведомление (без id) выполняется, но ответа по JSON-RPC не получает.
        let notification = request.id.is_none();
        let id = request.id.unwrap_or(serde_json::Value::Null);
        let response = match request.method.as_str() {
            "initialize" => Self::reply(
                Some(id),
                InitializeResult {
                    server_info: ServerInfo {
                        name: "pooprusteek".to_string(),
                        version: env!("CARGO_PKG_VERSION").to_string(),
                    },
                    capabilities: serde_json::json!({
                        "prompt": true,
                    }),
                },
            ),
            "prompt" => {
                let params = request.params.unwrap_or(serde_json::Value::Null);
                let params: PromptRequest = match serde_json::from_value(params) {
                    Ok(params) => params,
                    // Плохие параметры — ошибка, а не пустой промпт модели.
                    Err(e) => {
                        let error =
                            Self::error(Some(id), INVALID_PARAMS, format!("Invalid params: {e}"));
                        return (!notification).then_some(error);
                    }
                };
                // We're already inside the runtime — creating a nested Runtime
                // and block_on'ing it panics. block_in_place + Handle::current()
                // is the sanctioned way to run async work from this stdio loop.
                let result = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(self.handle_prompt(&params))
                });
                match result {
                    Ok(result) => Self::reply(Some(id), result),
                    Err(message) => Self::error(Some(id), PROVIDER_ERROR, message),
                }
            }
            "ping" => Self::reply(Some(id), serde_json::json!({})),
            _ => Self::error(
                Some(id),
                METHOD_NOT_FOUND,
                format!("Unknown method: {}", request.method),
            ),
        };
        (!notification).then_some(response)
    }

    async fn handle_prompt(&mut self, request: &PromptRequest) -> Result<PromptResult, String> {
        self.messages.push(ChatMessage::user(&request.prompt));

        let system_prompt = "You are Pooprusteek, a helpful AI coding assistant.";

        let mut messages = vec![ChatMessage::system(system_prompt)];
        messages.extend(self.messages.clone());

        let request = CompletionRequest {
            messages,
            // ACP — голый ретранслятор промпта, цикл агента здесь не крутится.
            tools: Vec::new(),
            model: self.provider.model().to_string(),
            temperature: 0.7,
            max_tokens: 4096,
            stream: false,
        };

        match self.provider.complete(request).await {
            Ok(response) => {
                self.messages
                    .push(ChatMessage::assistant(&response.content));
                Ok(PromptResult {
                    content: vec![ContentBlock::Text {
                        text: response.content,
                    }],
                })
            }
            // Провалившийся промпт из истории убираем: иначе следующий запрос
            // шёл бы с двумя репликами пользователя подряд.
            Err(e) => {
                self.messages.pop();
                Err(format!("Provider error: {e}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::fake::FakeProvider;

    fn server() -> AcpServer {
        AcpServer::new(Arc::new(FakeProvider::with_response("hi")))
    }

    /// Битый JSON и строковый id раньше оставались без ответа — клиент ждал вечно.
    #[test]
    fn every_request_with_an_id_gets_an_answer() {
        let mut server = server();
        let parse = server
            .handle_line("{not json")
            .expect("parse error answered");
        assert_eq!(parse.error.map(|e| e.code), Some(PARSE_ERROR));

        let ping = server
            .handle_line(r#"{"jsonrpc":"2.0","id":"abc","method":"ping"}"#)
            .expect("string id answered");
        assert_eq!(ping.id, Some(serde_json::json!("abc")));
        assert!(ping.error.is_none());

        let bad = server
            .handle_line(r#"{"jsonrpc":"2.0","id":7,"method":"prompt","params":{"text":1}}"#)
            .expect("bad params answered");
        assert_eq!(bad.error.map(|e| e.code), Some(INVALID_PARAMS));
    }

    #[test]
    fn a_notification_gets_no_answer() {
        let mut server = server();
        assert!(
            server
                .handle_line(r#"{"jsonrpc":"2.0","method":"initialized"}"#)
                .is_none()
        );
    }

    /// `"id": null` — запрос, а не уведомление: без ответа клиент повис бы.
    #[test]
    fn a_null_id_still_gets_an_answer() {
        let mut server = server();
        let ping = server
            .handle_line(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#)
            .expect("null id answered");
        assert!(ping.error.is_none());
    }
}
