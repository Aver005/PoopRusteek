//! A scripted OpenAI-compatible endpoint, so agent-loop behaviour can be
//! tested without a live model.
//!
//! Point a `/providers` entry at it and the app talks to it exactly as it
//! would to LM Studio or Ollama: the same `openai_compat` client, the same
//! streaming path, the same tool parsing. What changes is that the replies
//! are fixed, which makes a run a *regression test* rather than a sample.
//!
//! The JSON here is hand-rolled rather than reusing the app's own
//! serializers on purpose: a test double that shares wire code with the
//! thing under test cannot catch a wire-format bug.

use crate::error::{AppError, AppResult};
use crate::harness::MockArgs;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, header};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

type MockBody = BoxBody<Bytes, std::convert::Infallible>;

/// One canned reply.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    /// Serve this reply only when the last user message contains this
    /// substring (case-insensitive). Without it, position alone decides.
    #[serde(default)]
    when: Option<String>,
    /// Verbatim assistant text, tool-call syntax included.
    content: String,
    /// Milliseconds to stall before responding, for timeout tests.
    #[serde(default)]
    delay_ms: u64,
    /// Reply with this HTTP status instead of a completion, for error-path
    /// tests (429, 500, …).
    #[serde(default)]
    status: Option<u16>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Script {
    #[serde(default, rename = "reply")]
    replies: Vec<Reply>,
}

struct MockState {
    replies: Vec<Reply>,
    /// Round-robin cursor over positional replies.
    cursor: AtomicUsize,
}

impl MockState {
    /// Pick the reply for this request: the first still-unserved one whose
    /// `when` matches, else the next positional one, else a fixed
    /// acknowledgement so an unscripted run still completes.
    fn next(&self, last_user_message: &str, fresh_turn: bool) -> Reply {
        if self.replies.is_empty() {
            return Reply {
                when: None,
                content: "Mock provider: no script loaded.".to_string(),
                delay_ms: 0,
                status: None,
            };
        }
        // The service outlives a single turn, so without this a scenario run
        // with `--repeat 2` would find the cursor already at the end of the
        // script and the second repeat would skip straight to the last reply —
        // silently testing something else than the first repeat did.
        if fresh_turn {
            self.cursor.store(0, Ordering::Relaxed);
        }
        let needle = last_user_message.to_lowercase();
        if let Some(reply) = self.replies.iter().find(|reply| {
            reply
                .when
                .as_ref()
                .is_some_and(|when| needle.contains(&when.to_lowercase()))
        }) {
            return reply.clone();
        }
        let index = self.cursor.fetch_add(1, Ordering::Relaxed);
        let positional: Vec<&Reply> = self
            .replies
            .iter()
            .filter(|reply| reply.when.is_none())
            .collect();
        if positional.is_empty() {
            return self.replies[index % self.replies.len()].clone();
        }
        // The last positional reply repeats, so a longer-than-scripted turn
        // ends instead of 404-ing mid-loop.
        positional[index.min(positional.len() - 1)].clone()
    }
}

pub async fn run(args: MockArgs) -> AppResult<i32> {
    let script = match &args.script {
        Some(path) => load_script(path)?,
        None => Script::default(),
    };
    let state = Arc::new(MockState {
        replies: script.replies,
        cursor: AtomicUsize::new(0),
    });

    let address = format!("{}:{}", args.host, args.port);
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .map_err(|e| AppError::Custom(format!("cannot bind {address}: {e}")))?;
    println!(
        "mock provider listening on http://{address}/v1 ({} scripted repl{})",
        state.replies.len(),
        if state.replies.len() == 1 { "y" } else { "ies" }
    );

    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _peer)) => {
                    let state = Arc::clone(&state);
                    connections.spawn(async move {
                        let service = service_fn(move |request| {
                            route(request, Arc::clone(&state))
                        });
                        if let Err(error) = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await
                        {
                            tracing::debug!("mock: connection ended: {error}");
                        }
                    });
                }
                Err(error) => tracing::warn!("mock: accept failed: {error}"),
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    connections.abort_all();
    Ok(0)
}

fn load_script(path: &Path) -> AppResult<Script> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| AppError::Custom(format!("{}: {e}", path.display())))?;
    toml::from_str(&text).map_err(|e| AppError::Custom(format!("{}: {e}", path.display())))
}

async fn route(
    request: Request<Incoming>,
    state: Arc<MockState>,
) -> Result<Response<MockBody>, std::convert::Infallible> {
    let path = request.uri().path().to_string();
    let method = request.method().clone();
    let trimmed = path.strip_prefix("/v1").unwrap_or(&path).to_string();

    let response = match (&method, trimmed.as_str()) {
        (&Method::GET, "/models") => json(StatusCode::OK, models_payload()),
        (&Method::POST, "/chat/completions") => completions(request, &state).await,
        _ => json(
            StatusCode::NOT_FOUND,
            serde_json::json!({ "error": { "message": format!("no mock route for {path}") } }),
        ),
    };
    Ok(response)
}

fn models_payload() -> serde_json::Value {
    serde_json::json!({
        "object": "list",
        "data": [{ "id": "mock", "object": "model", "owned_by": "harness" }],
    })
}

async fn completions(request: Request<Incoming>, state: &Arc<MockState>) -> Response<MockBody> {
    let body = match request.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(error) => {
            return json(
                StatusCode::BAD_REQUEST,
                serde_json::json!({ "error": { "message": format!("unreadable body: {error}") } }),
            );
        }
    };
    let payload: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    let streaming = payload
        .get("stream")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let reply = state.next(&last_user_message(&payload), is_fresh_turn(&payload));

    if reply.delay_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(reply.delay_ms)).await;
    }
    if let Some(status) = reply.status {
        let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        return json(
            code,
            serde_json::json!({ "error": { "message": "scripted failure", "type": "mock" } }),
        );
    }

    if streaming {
        stream_reply(&reply.content)
    } else {
        json(StatusCode::OK, completion_payload(&reply.content))
    }
}

/// A turn's first request carries exactly one non-system message: the user's
/// prompt. Every later step of the same turn adds assistant and tool
/// messages, so this is what tells one turn from the next.
fn is_fresh_turn(payload: &serde_json::Value) -> bool {
    let non_system = payload
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|message| message.get("role").and_then(serde_json::Value::as_str) != Some("system"))
        .count();
    non_system <= 1
}

fn last_user_message(payload: &serde_json::Value) -> String {
    payload
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|message| message.get("role").and_then(serde_json::Value::as_str) == Some("user"))
        .filter_map(|message| message.get("content"))
        .filter_map(content_text)
        .next_back()
        .unwrap_or_default()
}

/// `content` is either a string or the multi-part array form.
fn content_text(value: &serde_json::Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    let parts: Vec<String> = value
        .as_array()?
        .iter()
        .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect();
    Some(parts.join(" "))
}

fn completion_payload(content: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "created": 0,
        "model": "mock",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": "stop",
        }],
        "usage": { "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 },
    })
}

/// One content delta plus the terminal chunk. Splitting the text further
/// would test the client's chunk reassembly, which the live providers
/// already exercise.
fn stream_reply(content: &str) -> Response<MockBody> {
    let delta = serde_json::json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "mock",
        "choices": [{ "index": 0, "delta": { "role": "assistant", "content": content }, "finish_reason": null }],
    });
    let final_chunk = serde_json::json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "mock",
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
    });
    let lines = vec![
        format!("data: {delta}\n\n"),
        format!("data: {final_chunk}\n\n"),
        "data: [DONE]\n\n".to_string(),
    ];
    let stream =
        futures::stream::iter(lines.into_iter().map(|line| {
            Ok::<Frame<Bytes>, std::convert::Infallible>(Frame::data(Bytes::from(line)))
        }));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(BoxBody::new(StreamBody::new(stream)))
        .expect("static response builds")
}

fn json(status: StatusCode, value: serde_json::Value) -> Response<MockBody> {
    let body = serde_json::to_vec(&value).unwrap_or_default();
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(BoxBody::new(Full::new(Bytes::from(body))))
        .expect("static response builds")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(replies: Vec<Reply>) -> MockState {
        MockState {
            replies,
            cursor: AtomicUsize::new(0),
        }
    }

    fn reply(content: &str, when: Option<&str>) -> Reply {
        Reply {
            when: when.map(str::to_string),
            content: content.to_string(),
            delay_ms: 0,
            status: None,
        }
    }

    #[test]
    fn positional_replies_advance_then_repeat_the_last() {
        let state = state(vec![reply("first", None), reply("second", None)]);
        assert_eq!(state.next("hi", false).content, "first");
        assert_eq!(state.next("hi", false).content, "second");
        assert_eq!(state.next("hi", false).content, "second");
    }

    #[test]
    fn when_matching_wins_over_position_and_ignores_case() {
        let state = state(vec![
            reply("positional", None),
            reply("matched", Some("LIST files")),
        ]);
        assert_eq!(
            state.next("please list FILES here", false).content,
            "matched"
        );
        assert_eq!(state.next("anything else", false).content, "positional");
    }

    #[test]
    fn an_empty_script_still_answers() {
        assert!(
            state(Vec::new())
                .next("hi", false)
                .content
                .contains("no script")
        );
    }

    #[test]
    fn a_fresh_turn_rewinds_the_script() {
        let state = state(vec![reply("first", None), reply("second", None)]);
        assert_eq!(state.next("go", true).content, "first");
        assert_eq!(state.next("go", false).content, "second");
        // A second repeat of the same scenario must see the script from the
        // top, not the tail the previous repeat left it at.
        assert_eq!(state.next("go", true).content, "first");
    }

    #[test]
    fn fresh_turn_is_detected_from_the_message_list() {
        let start = serde_json::json!({
            "messages": [
                { "role": "system", "content": "prompt" },
                { "role": "user", "content": "do it" },
            ]
        });
        assert!(is_fresh_turn(&start));
        let mid_turn = serde_json::json!({
            "messages": [
                { "role": "system", "content": "prompt" },
                { "role": "user", "content": "do it" },
                { "role": "assistant", "content": "calling a tool" },
                { "role": "tool", "content": "result" },
            ]
        });
        assert!(!is_fresh_turn(&mid_turn));
    }

    #[test]
    fn last_user_message_wins_and_handles_multipart_content() {
        let payload = serde_json::json!({
            "messages": [
                { "role": "system", "content": "ignored" },
                { "role": "user", "content": "first" },
                { "role": "assistant", "content": "reply" },
                { "role": "user", "content": [{ "text": "second" }, { "text": "part" }] },
            ]
        });
        assert_eq!(last_user_message(&payload), "second part");
        assert_eq!(last_user_message(&serde_json::json!({})), "");
    }

    #[test]
    fn unknown_script_field_is_rejected() {
        assert!(toml::from_str::<Script>("[[reply]]\ncontent=\"x\"\nnope=1\n").is_err());
    }
}
