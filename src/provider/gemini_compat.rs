//! Google Generative Language API (Gemini) wire format ⇄ internal provider
//! types — the third compat pair after [`super::openai_compat`] and
//! [`super::anthropic_compat`].
//!
//! Format differences that need real conversion:
//! - history is `contents[].parts[].text` with roles `user`/`model` (not
//!   `assistant`), and multi-turn requests must alternate — consecutive
//!   same-role turns are merged here;
//! - the system prompt is a top-level `systemInstruction`, never a turn;
//! - sampling knobs live under `generationConfig`
//!   (`temperature`/`maxOutputTokens`);
//! - the model id is part of the URL (`models/{model}:generateContent`),
//!   not the body — the transport owns that part;
//! - streaming (`:streamGenerateContent?alt=sse`) sends the SAME response
//!   shape per SSE `data:` line (no typed events, no `[DONE]`) — the final
//!   chunk carries `finishReason`.
//!
//! Pure data mapping — no I/O. Transport lives in [`super::gemini_client`].

use super::compat_client::merge_alternating_turns;
use crate::provider::{CompletionRequest, CompletionResponse, Usage};
use serde::Deserialize;
use serde_json::{Value, json};

/// Build the `generateContent` request body. `ui_only` messages are
/// filtered for the same reason every other compat layer filters them.
pub fn request_to_gemini(request: &CompletionRequest) -> Value {
    // Shared with anthropic_compat — same merge/opener algorithm, Gemini
    // just calls the model side "model".
    let (system_parts, turns) = merge_alternating_turns(request, "model");

    let contents: Vec<Value> = turns
        .into_iter()
        .map(|(role, text)| json!({ "role": role, "parts": [{ "text": text }] }))
        .collect();

    let mut body = json!({
        "contents": contents,
        "generationConfig": {
            "temperature": request.temperature,
            "maxOutputTokens": request.max_tokens,
        },
    });
    if !system_parts.is_empty() {
        body["systemInstruction"] = json!({ "parts": [{ "text": system_parts.join("\n\n") }] });
    }
    body
}

// ── Response (shared by non-streaming AND each streamed SSE chunk) ──────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentResponse {
    #[serde(default)]
    pub candidates: Vec<Candidate>,
    #[serde(default)]
    pub usage_metadata: Option<UsageMetadata>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    #[serde(default)]
    pub content: Option<Content>,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Content {
    #[serde(default)]
    pub parts: Vec<Part>,
}

#[derive(Debug, Deserialize)]
pub struct Part {
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageMetadata {
    #[serde(default)]
    pub prompt_token_count: u32,
    #[serde(default)]
    pub candidates_token_count: u32,
    #[serde(default)]
    pub total_token_count: u32,
}

/// Map Gemini finish reasons onto the OpenAI-style vocabulary the rest of
/// the app speaks; unknown reasons pass through lowercased.
fn map_finish_reason(reason: String) -> String {
    match reason.as_str() {
        "STOP" => "stop".to_string(),
        "MAX_TOKENS" => "length".to_string(),
        other => other.to_lowercase(),
    }
}

/// First-candidate text + mapped finish reason — the shared extractor for
/// the non-streaming response and every streamed chunk.
pub fn extract_piece(response: &GenerateContentResponse) -> (String, Option<String>) {
    let Some(candidate) = response.candidates.first() else {
        return (String::new(), None);
    };
    let text: String = candidate
        .content
        .as_ref()
        .map(|content| {
            content
                .parts
                .iter()
                .filter_map(|part| part.text.as_deref())
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let finish = candidate.finish_reason.clone().map(map_finish_reason);
    (text, finish)
}

pub fn response_from_gemini(response: GenerateContentResponse) -> CompletionResponse {
    let (content, finish_reason) = extract_piece(&response);
    CompletionResponse {
        content,
        finish_reason,
        usage: response.usage_metadata.map(|usage| Usage {
            prompt_tokens: usage.prompt_token_count,
            completion_tokens: usage.candidates_token_count,
            total_tokens: usage.total_token_count,
        }),
    }
}

// ── Models listing ──────────────────────────────────────────────────────

/// `GET {base}/models` page. Names come back as `models/<id>` — the
/// transport strips the prefix so `/models` shows bare ids.
#[derive(Debug, Deserialize)]
pub struct ModelsPage {
    #[serde(default)]
    pub models: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
pub struct ModelEntry {
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ChatMessage;

    #[test]
    fn system_instruction_and_alternating_contents() {
        let request = CompletionRequest {
            messages: vec![
                ChatMessage::system("be terse"),
                ChatMessage::user("hi"),
                ChatMessage::tool("call_1", "42"),
                ChatMessage::assistant("ok"),
            ],
            tools: Vec::new(),
            model: "gemini-2.5-flash".to_string(),
            temperature: 0.4,
            max_tokens: 512,
            stream: false,
        };
        let body = request_to_gemini(&request);

        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be terse");
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 512);
        let contents = body["contents"].as_array().unwrap();
        // user + tool merged; assistant → "model".
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0]["role"], "user");
        assert!(
            contents[0]["parts"][0]["text"]
                .as_str()
                .unwrap()
                .contains("[tool result]")
        );
        assert_eq!(contents[1]["role"], "model");
    }

    #[test]
    fn model_first_history_gets_user_opener() {
        let request = CompletionRequest {
            messages: vec![ChatMessage::assistant("earlier answer")],
            tools: Vec::new(),
            model: "m".to_string(),
            temperature: 0.0,
            max_tokens: 16,
            stream: true,
        };
        let body = request_to_gemini(&request);
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[1]["role"], "model");
    }

    #[test]
    fn response_extracts_text_finish_and_usage() {
        let raw = r#"{
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "Hel"}, {"text": "lo"}]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 3, "totalTokenCount": 10}
        }"#;
        let parsed: GenerateContentResponse = serde_json::from_str(raw).unwrap();
        let internal = response_from_gemini(parsed);
        assert_eq!(internal.content, "Hello");
        assert_eq!(internal.finish_reason.as_deref(), Some("stop"));
        assert_eq!(internal.usage.unwrap().total_tokens, 10);
    }

    #[test]
    fn finish_reasons_map() {
        let parsed: GenerateContentResponse =
            serde_json::from_str(r#"{"candidates": [{"finishReason": "MAX_TOKENS"}]}"#).unwrap();
        assert_eq!(extract_piece(&parsed).1.as_deref(), Some("length"));

        let parsed: GenerateContentResponse =
            serde_json::from_str(r#"{"candidates": [{"finishReason": "SAFETY"}]}"#).unwrap();
        assert_eq!(extract_piece(&parsed).1.as_deref(), Some("safety"));
    }

    #[test]
    fn stream_chunk_without_finish_is_plain_text() {
        // A mid-stream chunk is the same shape, just no finishReason.
        let parsed: GenerateContentResponse =
            serde_json::from_str(r#"{"candidates": [{"content": {"parts": [{"text": "tok"}]}}]}"#)
                .unwrap();
        let (text, finish) = extract_piece(&parsed);
        assert_eq!(text, "tok");
        assert!(finish.is_none());
    }
}
