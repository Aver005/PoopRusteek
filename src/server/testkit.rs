//! Стенд тестов сервера (`cfg(test)`): локальный «апстрим» на сыром TCP и сервер поверх
//! живого шлюза. Всё остаётся на loopback, настоящих ключей и папок данных нет.

use super::{GatewayCell, ServerHandle, ServerSettings, spawn};
use crate::app::events::AppEvent;
use crate::config::ServerApi;
use crate::gateway::engine::{BackendFactory, BuiltinFactory, EngineDeps, build_gateway};
use crate::gateway::handle::GatewayHandle;
use crate::provider::openai_compat::RequestDefaults;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Что апстрим делает с очередным запросом.
#[derive(Clone)]
pub(crate) enum Script {
    /// `429` с `Retry-After: 30`.
    RateLimited,
    /// `401`, в теле которого апстрим эхом повторяет ключ из запроса.
    RejectsKey,
    /// Обычный поток: куски текста, конец, расход (отдельным чанком после конца) и заголовки.
    Stream {
        chunks: Vec<&'static str>,
        usage: Option<(u32, u32)>,
        headers: Vec<(&'static str, &'static str)>,
    },
    /// Первый кусок и тишина, пока клиент не закроет соединение.
    Hang(&'static str),
    /// Первый кусок и обрыв соединения без конца ответа.
    Break(&'static str),
}

/// Что апстрим увидел в запросе.
#[derive(Clone, Debug)]
pub(crate) struct Seen {
    pub path: String,
    pub auth: String,
    pub body: String,
}

pub(crate) struct Upstream {
    pub port: u16,
    pub seen: Arc<Mutex<Vec<Seen>>>,
    /// Сколько соединений с «зависшим» ответом клиент закрыл сам.
    pub closed: Arc<AtomicUsize>,
}

impl Upstream {
    pub fn hits(&self, prefix: &str) -> usize {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.path.starts_with(prefix))
            .count()
    }

    pub fn last(&self, prefix: &str) -> Option<Seen> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|s| s.path.starts_with(prefix))
            .cloned()
    }
}

pub(crate) fn sse_chunk(content: &str, finish: Option<&str>) -> String {
    let finish = finish.map_or("null".to_string(), |f| format!("\"{f}\""));
    let delta = if content.is_empty() {
        "{}".to_string()
    } else {
        format!("{{\"content\":\"{content}\"}}")
    };
    format!(
        "data: {{\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{delta},\"finish_reason\":{finish}}}]}}\n\n"
    )
}

fn usage_chunk(prompt: u32, completion: u32) -> String {
    format!(
        "data: {{\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"m\",\"choices\":[],\"usage\":{{\"prompt_tokens\":{prompt},\"completion_tokens\":{completion},\"total_tokens\":{}}}}}\n\n",
        prompt + completion
    )
}

fn stream_head(extra: &[(&str, &str)]) -> String {
    let mut head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n".to_string();
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("connection: close\r\n\r\n");
    head
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<Seen> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buffer).to_string();
        let Some(split) = text.find("\r\n\r\n") else {
            continue;
        };
        let length = text[..split]
            .to_ascii_lowercase()
            .lines()
            .find_map(|l| {
                l.strip_prefix("content-length:")
                    .map(|v| v.trim().parse().unwrap_or(0))
            })
            .unwrap_or(0usize);
        if buffer.len() >= split + 4 + length {
            let path = text
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("")
                .to_string();
            let auth = text
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .starts_with("authorization:")
                        .then(|| l.split_once(':').map_or("", |(_, v)| v.trim()).to_string())
                })
                .unwrap_or_default();
            return Some(Seen {
                path,
                auth,
                body: text[split + 4..].to_string(),
            });
        }
    }
}

async fn answer(
    mut socket: tokio::net::TcpStream,
    script: Script,
    seen: Seen,
    closed: Arc<AtomicUsize>,
) {
    match script {
        Script::RateLimited => {
            let body = r#"{"error":{"message":"slow down","type":"rate_limit_error"}}"#;
            let reply = format!(
                "HTTP/1.1 429 Too Many Requests\r\nretry-after: 30\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(reply.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
        Script::RejectsKey => {
            let body = format!(
                "{{\"error\":{{\"message\":\"bad key {}\",\"type\":\"invalid_api_key\"}}}}",
                seen.auth
            );
            let reply = format!(
                "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(reply.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
        Script::Stream {
            chunks,
            usage,
            headers,
        } => {
            let mut out = stream_head(&headers);
            for piece in &chunks {
                out.push_str(&sse_chunk(piece, None));
            }
            out.push_str(&sse_chunk("", Some("stop")));
            if let Some((prompt, completion)) = usage {
                out.push_str(&usage_chunk(prompt, completion));
            }
            out.push_str("data: [DONE]\n\n");
            let _ = socket.write_all(out.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
        Script::Hang(first) => {
            let out = format!("{}{}", stream_head(&[]), sse_chunk(first, None));
            let _ = socket.write_all(out.as_bytes()).await;
            let mut sink = [0u8; 256];
            loop {
                match socket.read(&mut sink).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            closed.fetch_add(1, Ordering::SeqCst);
        }
        Script::Break(first) => {
            let out = format!("{}{}", stream_head(&[]), sse_chunk(first, None));
            let _ = socket.write_all(out.as_bytes()).await;
            // Обрыв посреди ответа: без `shutdown` и без конца потока.
            drop(socket);
        }
    }
}

/// Апстрим на loopback. Скрипт выбирается по началу пути; n-й запрос к префиксу получает n-й
/// скрипт, а после последнего повторяется последний.
pub(crate) async fn upstream(scripts: Vec<(&'static str, Vec<Script>)>) -> Upstream {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
    let closed = Arc::new(AtomicUsize::new(0));
    let counters: Arc<Mutex<HashMap<&'static str, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    let (log, hung) = (Arc::clone(&seen), Arc::clone(&closed));
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let (log, hung, counters) =
                (Arc::clone(&log), Arc::clone(&hung), Arc::clone(&counters));
            let scripts = scripts.clone();
            tokio::spawn(async move {
                let Some(request) = read_request(&mut socket).await else {
                    return;
                };
                log.lock().unwrap().push(request.clone());
                let Some((prefix, list)) = scripts
                    .iter()
                    .find(|(prefix, _)| request.path.starts_with(prefix))
                else {
                    return;
                };
                let index = {
                    let mut counters = counters.lock().unwrap();
                    let slot = counters.entry(prefix).or_insert(0);
                    let current = (*slot).min(list.len() - 1);
                    *slot += 1;
                    current
                };
                answer(socket, list[index].clone(), request, hung).await;
            });
        }
    });
    Upstream { port, seen, closed }
}

/// Движок из TOML-текста: данные на диск не пишет, секреты — настоящим резолвером.
pub(crate) fn engine(toml_text: &str, extra: Vec<Arc<dyn BackendFactory>>) -> GatewayHandle {
    let config = crate::config::parse(toml_text).expect("the test config parses");
    let gateway = crate::gateway::engine::gateway_config(&config);
    let problems: Vec<_> = gateway
        .validate()
        .into_iter()
        .filter(|p| p.severity == crate::gateway::types::Severity::Error)
        .collect();
    assert!(problems.is_empty(), "config problems: {problems:?}");
    let mut deps = EngineDeps::for_config(&config);
    deps.data_dir = None;
    deps.factories.truncate(0);
    deps.factories.extend(extra);
    deps.factories.push(Arc::new(BuiltinFactory::new(None)));
    build_gateway(gateway, deps)
}

/// Запущенный сервер и всё, чем его трогают из теста.
pub(crate) struct Stand {
    pub base: String,
    pub cell: GatewayCell,
    pub handle: ServerHandle,
    _events: tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
}

pub(crate) fn settings(
    gateway: GatewayHandle,
    api_key: Option<&str>,
    host: &str,
) -> ServerSettings {
    ServerSettings {
        host: host.to_string(),
        port: 0,
        api: ServerApi::Openai,
        api_key: api_key.map(str::to_string),
        defaults: RequestDefaults {
            temperature: 0.0,
            max_tokens: 64,
        },
        deepseek: None,
        entries: Vec::new(),
        request_log: false,
        gateway: GatewayCell::new(gateway),
    }
}

pub(crate) async fn start_with(settings: ServerSettings) -> Stand {
    let (event_tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    let cell = settings.gateway.clone();
    let models = crate::provider::model_cache::ProviderModelCache::empty_for_tests();
    let handle = spawn(settings, models, 1, event_tx);
    let addr = loop {
        match events.recv().await {
            Some(AppEvent::ServerStarted { addr, .. }) => break addr,
            Some(_) => continue,
            None => panic!("the server task ended before it started"),
        }
    };
    Stand {
        base: format!("http://127.0.0.1:{}", addr.port()),
        cell,
        handle,
        _events: events,
    }
}

pub(crate) async fn start(gateway: GatewayHandle, api_key: Option<&str>) -> Stand {
    start_with(settings(gateway, api_key, "127.0.0.1")).await
}

/// Ждёт условия до пяти секунд: фоновые задачи сервера и учёта не мгновенны.
pub(crate) async fn eventually(mut check: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    check()
}
