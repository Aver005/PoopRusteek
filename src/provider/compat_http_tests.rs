//! Настоящий HTTP-обмен с локальным сервером: тип отказа, заголовки лимитов,
//! расход в потоке и безопасный путь для сервера без `stream_options`.

use super::compat_client::CompatClient;
use super::failure::{FailureClass, ProviderFailure};
use super::openai_client::{OpenAiCompatProvider, OpenAiProtocol};
use super::{ChatMessage, CompletionChunk, CompletionRequest, LLMProvider};
use crate::config::ProviderEntry;
use crate::error::AppError;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Готовый ответ сервера: статус, заголовки, тело.
struct Reply {
    status: &'static str,
    headers: Vec<(&'static str, &'static str)>,
    body: String,
}

impl Reply {
    fn json(status: &'static str, headers: Vec<(&'static str, &'static str)>, body: &str) -> Self {
        let mut headers = headers;
        headers.push(("content-type", "application/json"));
        Self {
            status,
            headers,
            body: body.to_string(),
        }
    }

    fn sse(body: &str) -> Self {
        Self {
            status: "200 OK",
            headers: vec![("content-type", "text/event-stream")],
            body: body.to_string(),
        }
    }
}

/// Отвечает по очереди; возвращает базовый адрес и тела полученных запросов.
async fn serve(replies: Vec<Reply>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        for reply in replies {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = socket.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buffer).to_string();
                if let Some(split) = text.find("\r\n\r\n") {
                    let length = text[..split]
                        .to_ascii_lowercase()
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("content-length:")
                                .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if buffer.len() >= split + 4 + length {
                        log.lock().unwrap().push(text[split + 4..].to_string());
                        break;
                    }
                }
            }
            let mut head = format!(
                "HTTP/1.1 {}\r\ncontent-length: {}\r\nconnection: close\r\n",
                reply.status,
                reply.body.len()
            );
            for (name, value) in &reply.headers {
                head.push_str(&format!("{name}: {value}\r\n"));
            }
            head.push_str("\r\n");
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(reply.body.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    (base, seen)
}

fn provider(base_url: &str) -> CompatClient<OpenAiProtocol> {
    OpenAiCompatProvider::new(&ProviderEntry {
        name: "test".to_string(),
        base_url: base_url.to_string(),
        api_key: None,
        model: "m".to_string(),
        protocol: crate::config::ProviderProtocol::Openai,
        tools: crate::config::ToolProtocol::default(),
    })
    .unwrap()
}

fn request() -> CompletionRequest {
    CompletionRequest {
        messages: vec![ChatMessage::user("hi")],
        tools: Vec::new(),
        model: "m".to_string(),
        temperature: 0.0,
        max_tokens: 16,
        stream: false,
    }
}

fn failure_of(error: AppError) -> Box<ProviderFailure> {
    match error {
        AppError::Upstream(failure) => failure,
        other => panic!("expected a typed upstream failure, got {other}"),
    }
}

async fn collect(provider: &dyn LLMProvider) -> Vec<CompletionChunk> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    provider.complete_stream(request(), tx).await.unwrap();
    let mut chunks = Vec::new();
    while let Ok(chunk) = rx.try_recv() {
        chunks.push(chunk);
    }
    chunks
}

#[tokio::test]
async fn a_429_with_retry_after_is_a_typed_rate_limit_and_keeps_the_headers() {
    let (base, _) = serve(vec![Reply::json(
        "429 Too Many Requests",
        vec![
            ("retry-after", "30"),
            ("x-ratelimit-remaining-requests", "0"),
            ("x-ratelimit-reset-requests", "30s"),
        ],
        r#"{"error":{"message":"slow down","type":"rate_limit_error"}}"#,
    )])
    .await;
    let provider = provider(&base);
    let failure = failure_of(provider.complete(request()).await.unwrap_err());
    assert_eq!(
        failure.class,
        FailureClass::RateLimit {
            retry_after: Some(Duration::from_secs(30))
        }
    );
    assert_eq!(failure.message, "slow down");
    let limits = provider.rate_limits().expect("headers survive the error");
    assert_eq!(limits.windows[0].remaining, Some(0));
    // Лимиты принадлежат аккаунту, а не копии клиента.
    assert!(provider.fork().rate_limits().is_some());
}

#[tokio::test]
async fn a_429_without_retry_after_is_a_rate_limit_without_a_delay() {
    let (base, _) = serve(vec![Reply::json(
        "429 Too Many Requests",
        vec![],
        r#"{"error":{"message":"busy","type":"rate_limit_error"}}"#,
    )])
    .await;
    let provider = provider(&base);
    let error = provider.complete(request()).await.unwrap_err();
    assert_eq!(
        error.failure_class(),
        Some(FailureClass::RateLimit { retry_after: None })
    );
    assert_eq!(error.retry_after(), None);
    assert!(
        provider.rate_limits().is_none(),
        "no headers, nothing to remember"
    );
}

#[tokio::test]
async fn an_exhausted_quota_is_not_a_rate_limit() {
    let (base, _) = serve(vec![Reply::json(
        "429 Too Many Requests",
        vec![],
        r#"{"error":{"message":"You exceeded your current quota","type":"insufficient_quota","code":"insufficient_quota"}}"#,
    )])
    .await;
    let error = provider(&base).complete(request()).await.unwrap_err();
    assert!(matches!(
        error.failure_class(),
        Some(FailureClass::QuotaExhausted { .. })
    ));
}

#[tokio::test]
async fn a_200_without_usage_still_parses_and_reports_no_usage() {
    let (base, _) = serve(vec![Reply::json(
        "200 OK",
        vec![],
        r#"{"id":"x","object":"chat.completion","created":0,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}]}"#,
    )])
    .await;
    let response = provider(&base).complete(request()).await.unwrap();
    assert_eq!(response.content, "hello");
    assert!(
        response.usage.is_none(),
        "no numbers from the server, none invented"
    );
}

const USAGE_STREAM: &str = concat!(
    r#"data: {"id":"1","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#,
    "\n\n",
    r#"data: {"id":"1","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    "\n\n",
    r#"data: {"id":"1","object":"chat.completion.chunk","created":0,"model":"m","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12,"prompt_tokens_details":{"cached_tokens":4}}}"#,
    "\n\n",
    "data: [DONE]\n\n",
);

const PLAIN_STREAM: &str = concat!(
    r#"data: {"id":"1","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]}"#,
    "\n\n",
    "data: [DONE]\n\n",
);

#[tokio::test]
async fn the_stream_asks_for_usage_and_surfaces_it_after_the_finish_chunk() {
    let (base, seen) = serve(vec![Reply::sse(USAGE_STREAM)]).await;
    let chunks = collect(&provider(&base)).await;
    assert!(seen.lock().unwrap()[0].contains("include_usage"));
    let usage = chunks
        .iter()
        .find_map(|chunk| chunk.usage.clone())
        .expect("usage chunk");
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (10, 2));
    assert_eq!(usage.cache_read_tokens, Some(4));
    let finish = chunks
        .iter()
        .position(|chunk| chunk.finish_reason.is_some())
        .unwrap();
    let carrier = chunks
        .iter()
        .position(|chunk| chunk.usage.is_some())
        .unwrap();
    assert!(
        carrier > finish,
        "OpenAI sends usage after the finish chunk"
    );
}

#[tokio::test]
async fn a_stream_without_a_usage_chunk_just_has_no_usage() {
    let (base, _) = serve(vec![Reply::sse(PLAIN_STREAM)]).await;
    let chunks = collect(&provider(&base)).await;
    assert!(chunks.iter().all(|chunk| chunk.usage.is_none()));
    assert!(chunks.iter().any(|chunk| chunk.finish_reason.is_some()));
}

#[tokio::test]
async fn a_server_that_rejects_stream_options_gets_one_retry_and_is_remembered() {
    let (base, seen) = serve(vec![
        Reply::json(
            "400 Bad Request",
            vec![],
            r#"{"error":{"message":"Unrecognized request argument supplied: stream_options"}}"#,
        ),
        Reply::sse(PLAIN_STREAM),
        Reply::sse(PLAIN_STREAM),
    ])
    .await;
    let provider = provider(&base);
    let chunks = collect(&provider).await;
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| chunk.content.as_str())
            .collect::<String>(),
        "ok"
    );
    // Форк делит память об отказе с оригиналом: параметр больше не шлётся.
    collect(provider.fork().as_ref()).await;
    let bodies = seen.lock().unwrap().clone();
    assert_eq!(bodies.len(), 3);
    assert!(bodies[0].contains("stream_options"));
    assert!(!bodies[1].contains("stream_options"));
    assert!(!bodies[2].contains("stream_options"));
}

/// Шлюз отвергает параметр пустым телом: по тексту это не распознать.
#[tokio::test]
async fn a_gateway_rejecting_with_an_empty_400_still_streams_after_a_retry() {
    let (base, seen) = serve(vec![
        Reply::json("400 Bad Request", vec![], ""),
        Reply::sse(PLAIN_STREAM),
        Reply::sse(PLAIN_STREAM),
    ])
    .await;
    let provider = provider(&base);
    assert!(
        collect(&provider)
            .await
            .iter()
            .any(|chunk| chunk.content == "ok")
    );
    collect(&provider).await;
    let bodies = seen.lock().unwrap().clone();
    assert_eq!(bodies.len(), 3);
    assert!(!bodies[2].contains("stream_options"));
}

/// Настоящая ошибка запроса не должна отключать расход для этого сервера.
#[tokio::test]
async fn a_real_400_stays_a_bad_request_and_usage_is_still_asked_next_time() {
    let bad = || {
        Reply::json(
            "400 Bad Request",
            vec![],
            r#"{"error":{"message":"messages: field required"}}"#,
        )
    };
    let (base, seen) = serve(vec![bad(), bad(), Reply::sse(PLAIN_STREAM)]).await;
    let provider = provider(&base);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let error = provider.complete_stream(request(), tx).await.unwrap_err();
    assert_eq!(error.failure_class(), Some(FailureClass::BadRequest));
    collect(&provider).await;
    let bodies = seen.lock().unwrap().clone();
    assert_eq!(bodies.len(), 3);
    assert!(bodies[2].contains("stream_options"));
}
