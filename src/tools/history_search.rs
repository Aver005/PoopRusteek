use super::*;
use crate::semantic::SemanticService;
use std::sync::Arc;

/// Semantic + keyword search over the indexed history of every saved
/// session — the agent-facing sibling of `/search`. Lets the model recall
/// past conversations ("we solved this in another chat") without the user
/// re-pasting anything.
pub struct HistorySearchTool {
    pub semantic: Arc<SemanticService>,
}

const DEFAULT_LIMIT: usize = 5;
const MAX_LIMIT: usize = 20;
/// Per-result text budget in the tool output — enough to be useful,
/// bounded so ten hits can't flood the context.
const RESULT_TEXT_MAX_BYTES: usize = 700;

#[async_trait]
impl Tool for HistorySearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "history_search".to_string(),
            description:
                "Search the user's past conversations (previous saved sessions) by meaning or keywords, any language. Use when the user refers to something discussed before (\"как мы делали в прошлый раз\", \"that error we fixed\") or when prior decisions/solutions may already exist. Returns matching message excerpts with their session ids."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "What to look for — topic, error text, decision, code fragment"
                    },
                    "limit": {
                        "type": "number",
                        "description": "Max results (default 5)"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let Some(query) = args["query"].as_str().filter(|q| !q.trim().is_empty()) else {
            return ToolResult::error("Missing 'query' argument");
        };
        let limit = args["limit"]
            .as_u64()
            .map(|l| (l as usize).clamp(1, MAX_LIMIT))
            .unwrap_or(DEFAULT_LIMIT);

        // ONNX inference is CPU-bound — keep it off the tokio worker.
        let semantic = Arc::clone(&self.semantic);
        let query_owned = query.to_string();
        let matches =
            tokio::task::spawn_blocking(move || semantic.search_history(&query_owned, limit))
                .await
                .unwrap_or_default();

        if matches.is_empty() {
            return ToolResult::success(
                "No matches in past conversations (the index may still be building, or nothing similar was discussed).",
            );
        }
        let body = matches
            .iter()
            .map(|m| {
                let when = m.timestamp.split('T').next().unwrap_or(&m.timestamp);
                format!(
                    "[{}] {} — {} (session {}):\n{}",
                    when,
                    m.title,
                    m.role,
                    m.session_id,
                    crate::util::truncate_with_ellipsis(&m.text, RESULT_TEXT_MAX_BYTES)
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        ToolResult::success(&format!("Matches from past conversations:\n\n{body}"))
    }
}
