// Template for a new PoopRusteek agent tool.
// Copy into `src/tools/<mymod>.rs`, rename `ExampleTool` / `example_tool`, and
// fill in the // AUTHOR: spots. Mirrors the real `Tool` trait in
// `src/tools/mod.rs` exactly:
//
//     #[async_trait]
//     pub trait Tool: Send + Sync {
//         fn definition(&self) -> ToolDefinition;
//         async fn execute(&self, args: Value) -> ToolResult;
//     }

use super::*; // brings in Tool, ToolDefinition, ToolResult, async_trait, Value
use serde_json::{Value, json};

// AUTHOR: name your struct. Hold config/handles here (e.g. an
// `Arc<SemanticService>`) if the tool needs them; a unit struct is fine
// otherwise. Registered as `Arc<dyn Tool>`, so keep it `Send + Sync`.
pub struct ExampleTool;

#[async_trait]
impl Tool for ExampleTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            // AUTHOR: clean identifier the tool_parser + dispatch can match
            // (no `mcp__` prefix — that namespace is reserved for MCP tools).
            name: "example_tool".to_string(),
            // AUTHOR: model-facing — say what it does and when to use it.
            description: "One-line description of what this tool does and when to use it."
                .to_string(),
            // AUTHOR: JSON Schema for the arguments the model must supply.
            parameters: json!({
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "What to act on"
                    }
                },
                "required": ["target"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> ToolResult {
        // AUTHOR: validate args — return ToolResult::error, never panic.
        let Some(target) = args["target"].as_str().filter(|s| !s.trim().is_empty()) else {
            return ToolResult::error("Missing 'target' argument");
        };

        // AUTHOR: do the work here.
        // - CPU-heavy work (parsing, hashing, ONNX, etc.) must run on
        //   tokio::task::spawn_blocking so it never blocks the async worker:
        //       let owned = target.to_string();
        //       let out = tokio::task::spawn_blocking(move || heavy(&owned))
        //           .await
        //           .unwrap_or_default();
        // - NEVER reach into the app layer. tools/ and app/ talk only through
        //   AppEvents — do not import or mutate App/AppState here.

        ToolResult::success(&format!("did it for {target}"))
    }
}

// ── Wire it up ───────────────────────────────────────────────────────────
// 1. In `src/tools/mod.rs` add:  pub mod mymod;
// 2. In `src/tools/registry.rs`, inside `register_default_tools()`, add the
//    ONE registration line (platform-gate with `cfg!(windows)` if needed):
//
//        self.register(Arc::new(mymod::ExampleTool));
//
//    Tools needing the shared SemanticService go through
//    `register_semantic_tools` instead (see `tool_search` / `history_search`).
