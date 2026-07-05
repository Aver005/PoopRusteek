use super::*;
use crate::semantic::SemanticService;
use std::sync::Arc;

/// Search the MCP tool catalog by capability and return full definitions.
/// This is the escape hatch that makes deferred MCP schemas safe: the
/// system prompt lists tool names without parameters, and the model calls
/// this to pull the schemas it actually needs. Backed by the semantic
/// index when embeddings are ready, degrading to lexical substring
/// matching otherwise — it never hard-fails into "no way to get a schema".
pub struct ToolSearchTool {
    pub semantic: Arc<SemanticService>,
}

const DEFAULT_LIMIT: usize = 5;
const MAX_LIMIT: usize = 20;

#[async_trait]
impl Tool for ToolSearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tool_search".to_string(),
            description:
                "Search the connected MCP tools by capability and get their full definitions (descriptions + parameter schemas). Use this before calling an MCP tool whose parameters are not shown in the system prompt. Query by what you want to do (e.g. \"click a button in the browser\", \"query database\"), not by exact tool name."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "What you need the tool to do — a short capability description; any language works"
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
            tokio::task::spawn_blocking(move || semantic.search_tools(&query_owned, limit))
                .await
                .unwrap_or_default();

        if matches.is_empty() {
            return ToolResult::success(
                "No matching MCP tools found. Either no MCP servers are connected or nothing matches the query — try different wording.",
            );
        }
        let body = matches
            .iter()
            .map(|m| crate::util::format_tool_definition(&m.full_name, &m.description, &m.schema))
            .collect::<Vec<_>>()
            .join("\n\n");
        ToolResult::success(&format!("Matching MCP tools:\n\n{body}"))
    }
}
