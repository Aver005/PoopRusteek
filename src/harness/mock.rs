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
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

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
    /// `Retry-After` in seconds on a scripted error reply.
    #[serde(default)]
    retry_after: Option<u64>,
    /// Serve this reply only on the `/<account>/v1/…` prefix. Without it the
    /// reply belongs to the bare `/v1/…` endpoint and to any account that has
    /// no script of its own.
    #[serde(default)]
    account: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Script {
    #[serde(default, rename = "reply")]
    replies: Vec<Reply>,
}

struct MockState {
    replies: Vec<Reply>,
    /// Round-robin cursor over positional replies, one per account prefix
    /// (`""` is the bare `/v1` endpoint).
    cursors: Mutex<HashMap<String, usize>>,
}

impl MockState {
    fn new(replies: Vec<Reply>) -> Self {
        Self {
            replies,
            cursors: Mutex::new(HashMap::new()),
        }
    }

    /// The replies an account may see: its own, else the shared ones. The bare
    /// endpoint sees only the shared ones, so older single-script files behave
    /// exactly as before.
    fn visible(&self, account: &str) -> Vec<&Reply> {
        let own: Vec<&Reply> = self
            .replies
            .iter()
            .filter(|reply| !account.is_empty() && reply.account.as_deref() == Some(account))
            .collect();
        if own.is_empty() {
            self.replies
                .iter()
                .filter(|reply| reply.account.is_none())
                .collect()
        } else {
            own
        }
    }

    /// Pick the reply for this request: the first still-unserved one whose
    /// `when` matches, else the next positional one, else a fixed
    /// acknowledgement so an unscripted run still completes.
    fn next(&self, account: &str, last_user_message: &str, fresh_turn: bool) -> Reply {
        let pool = self.visible(account);
        if pool.is_empty() {
            return Reply {
                when: None,
                content: "Mock provider: no script loaded.".to_string(),
                delay_ms: 0,
                status: None,
                retry_after: None,
                account: None,
            };
        }
        let mut cursors = self.cursors.lock().unwrap_or_else(|e| e.into_inner());
        let cursor = cursors.entry(account.to_string()).or_insert(0);
        // The service outlives a single turn, so without this a scenario run
        // with `--repeat 2` would find the cursor already at the end of the
        // script and the second repeat would skip straight to the last reply —
        // silently testing something else than the first repeat did.
        if fresh_turn {
            *cursor = 0;
        }
        let needle = last_user_message.to_lowercase();
        if let Some(reply) = pool.iter().find(|reply| {
            reply
                .when
                .as_ref()
                .is_some_and(|when| needle.contains(&when.to_lowercase()))
        }) {
            return (*reply).clone();
        }
        let index = *cursor;
        *cursor += 1;
        let positional: Vec<&Reply> = pool
            .iter()
            .copied()
            .filter(|reply| reply.when.is_none())
            .collect();
        if positional.is_empty() {
            return pool[index % pool.len()].clone();
        }
        // The last positional reply repeats, so a longer-than-scripted turn
        // ends instead of 404-ing mid-loop.
        (*positional[index.min(positional.len() - 1)]).clone()
    }
}

pub async fn run(args: MockArgs) -> AppResult<i32> {
    let script = match &args.script {
        Some(path) => load_script(path)?,
        None => Script::default(),
    };
    let state = Arc::new(MockState::new(script.replies));

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
    let (account, trimmed) = split_account(&path);

    let response = match (&method, trimmed.as_str()) {
        (&Method::GET, "/models") => json(StatusCode::OK, models_payload()),
        (&Method::POST, "/chat/completions") => completions(request, &state, &account).await,
        _ => json(
            StatusCode::NOT_FOUND,
            serde_json::json!({ "error": { "message": format!("no mock route for {path}") } }),
        ),
    };
    Ok(response)
}

/// `/<account>/v1/chat/completions` → (`<account>`, `/chat/completions`). A path
/// with no `/v1` segment, or one that starts with it, belongs to no account.
fn split_account(path: &str) -> (String, String) {
    let at = path
        .find("/v1/")
        .or_else(|| path.ends_with("/v1").then(|| path.len() - 3));
    match at {
        Some(at) => (
            path[..at].trim_matches('/').to_string(),
            path[at + 3..].to_string(),
        ),
        None => (String::new(), path.to_string()),
    }
}

/// `context_length` is the field `CompatClient::context_window` reads (the
/// OpenRouter extension to OpenAI's schema). Serving it is what lets a mock
/// run exercise the provider-reported window at all — a scenario that pins
/// `[context] window` still overrides it, as config always does.
const MOCK_CONTEXT_LENGTH: u32 = 32_768;

fn models_payload() -> serde_json::Value {
    serde_json::json!({
        "object": "list",
        "data": [{
            "id": "mock",
            "object": "model",
            "owned_by": "harness",
            "context_length": MOCK_CONTEXT_LENGTH,
        }],
    })
}

async fn completions(
    request: Request<Incoming>,
    state: &Arc<MockState>,
    account: &str,
) -> Response<MockBody> {
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
    let reply = state.next(
        account,
        &last_user_message(&payload),
        is_fresh_turn(&payload),
    );

    if reply.delay_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(reply.delay_ms)).await;
    }
    if let Some(status) = reply.status {
        return failure_response(status, reply.retry_after);
    }

    if streaming {
        stream_reply(&reply.content)
    } else {
        json(StatusCode::OK, completion_payload(&reply.content))
    }
}

/// A scripted error reply, with `Retry-After` when the script gives one.
fn failure_response(status: u16, retry_after: Option<u64>) -> Response<MockBody> {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = serde_json::json!({ "error": { "message": "scripted failure", "type": "mock" } });
    let mut response = json(code, body);
    if let Some(seconds) = retry_after
        && let Ok(value) = header::HeaderValue::from_str(&seconds.to_string())
    {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
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
        MockState::new(replies)
    }

    fn reply(content: &str, when: Option<&str>) -> Reply {
        Reply {
            when: when.map(str::to_string),
            content: content.to_string(),
            delay_ms: 0,
            status: None,
            retry_after: None,
            account: None,
        }
    }

    fn account_reply(account: &str, content: &str, status: Option<u16>) -> Reply {
        Reply {
            account: Some(account.to_string()),
            status,
            retry_after: status.map(|_| 30),
            ..reply(content, None)
        }
    }

    #[test]
    fn positional_replies_advance_then_repeat_the_last() {
        let state = state(vec![reply("first", None), reply("second", None)]);
        assert_eq!(state.next("", "hi", false).content, "first");
        assert_eq!(state.next("", "hi", false).content, "second");
        assert_eq!(state.next("", "hi", false).content, "second");
    }

    #[test]
    fn when_matching_wins_over_position_and_ignores_case() {
        let state = state(vec![
            reply("positional", None),
            reply("matched", Some("LIST files")),
        ]);
        assert_eq!(
            state.next("", "please list FILES here", false).content,
            "matched"
        );
        assert_eq!(state.next("", "anything else", false).content, "positional");
    }

    #[test]
    fn an_empty_script_still_answers() {
        assert!(
            state(Vec::new())
                .next("", "hi", false)
                .content
                .contains("no script")
        );
    }

    #[test]
    fn a_fresh_turn_rewinds_the_script() {
        let state = state(vec![reply("first", None), reply("second", None)]);
        assert_eq!(state.next("", "go", true).content, "first");
        assert_eq!(state.next("", "go", false).content, "second");
        // A second repeat of the same scenario must see the script from the
        // top, not the tail the previous repeat left it at.
        assert_eq!(state.next("", "go", true).content, "first");
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

    /// Without this field the mock cannot report a window, and the driver's
    /// `ContextWindowLearned` path has nothing to carry.
    #[test]
    fn the_model_listing_reports_a_context_window() {
        let entry = &models_payload()["data"][0];
        assert_eq!(entry["id"], "mock");
        assert_eq!(entry["context_length"], MOCK_CONTEXT_LENGTH);
    }

    /// Скрипты на аккаунт: у каждого свой курсор, чужие ответы не видны.
    #[test]
    fn each_account_has_its_own_script_and_cursor() {
        let state = state(vec![
            account_reply("a", "limited", Some(429)),
            account_reply("b", "from b", None),
            reply("shared", None),
        ]);
        assert_eq!(state.next("a", "hi", false).status, Some(429));
        assert_eq!(state.next("b", "hi", false).content, "from b");
        assert_eq!(
            state.next("c", "hi", false).content,
            "shared",
            "an account without a script falls back to the shared one"
        );
        assert_eq!(
            state.next("", "hi", false).content,
            "shared",
            "the bare endpoint never sees account scripts"
        );
        // Курсор a не сдвинут чужими запросами.
        assert_eq!(state.next("a", "hi", false).status, Some(429));
    }

    #[test]
    fn a_path_names_its_account_by_the_segment_before_v1() {
        assert_eq!(
            split_account("/a/v1/chat/completions"),
            ("a".to_string(), "/chat/completions".to_string())
        );
        assert_eq!(
            split_account("/v1/models"),
            (String::new(), "/models".to_string())
        );
        assert_eq!(
            split_account("/team/b/v1"),
            ("team/b".to_string(), String::new())
        );
        assert_eq!(
            split_account("/chat/completions"),
            (String::new(), "/chat/completions".to_string())
        );
    }

    #[test]
    fn a_scripted_failure_can_carry_retry_after() {
        let with = failure_response(429, Some(30));
        assert_eq!(with.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(with.headers()[header::RETRY_AFTER], "30");
        assert!(
            failure_response(500, None)
                .headers()
                .get(header::RETRY_AFTER)
                .is_none()
        );
    }

    #[test]
    fn unknown_script_field_is_rejected() {
        assert!(toml::from_str::<Script>("[[reply]]\ncontent=\"x\"\nnope=1\n").is_err());
    }
}
