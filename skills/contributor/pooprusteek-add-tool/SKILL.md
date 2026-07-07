---
name: Add a PoopRusteek Agent Tool
description: Recipe for adding a new agent-facing Tool (bash/skill/tool_search-style) to PoopRusteek — the Tool trait, the single registration line, and the hard tools-never-touch-app boundary. Use when adding an agent tool; not for slash commands (see pooprusteek-add-slash-command).
---

# Add a PoopRusteek Agent Tool

Source of truth: `src/tools/mod.rs`, `src/tools/registry.rs`. Reference: `.memories/reference/TOOLS.md`.

## Recipe

**A new tool = implement the `Tool` trait + ONE registration line in `tools/registry.rs`.**

## The `Tool` trait (`tools/mod.rs`)

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;      // name + description + JSON-schema parameters
    async fn execute(&self, args: Value) -> ToolResult;
}
```

- `ToolDefinition { name, description, parameters: serde_json::Value }` — `parameters` is a JSON
  Schema object; it's what the model sees, so make `description` and required fields precise.
- `ToolResult { content: String, is_error: bool }` — build with `ToolResult::success(&str)` /
  `ToolResult::error(&str)`. Return `error` (don't panic) on bad/missing args.

## Register it

In `ToolRegistry::register_default_tools()` (`src/tools/registry.rs`):

```rust
self.register(Arc::new(mymod::MyTool));
```

Platform-gate if needed (PowerShell is Windows-only; see the `cfg!(windows)` branch there).
Semantic-backed tools (`tool_search`, `history_search`) are registered separately via
`register_semantic_tools`, and `skill` via `update_skills` — mirror the plain path unless yours
also needs the shared `SemanticService` handle.

## Hard invariants

- **The tools layer must NEVER reach into the app layer.** `tools/` and `app/` communicate
  exclusively through `AppEvent`s — no direct calls, no shared state reaching upward. A tool that
  needs to affect the UI/app emits an event; it does not import or mutate `App`/`AppState`.
- **No native function-calling.** The DeepSeek web API has none — tool calls are parsed from raw
  LLM text in 3 formats (XML `<tool_use>`, XML+JSON, legacy `[TOOL:name] {json}`) by
  `agent/tool_parser.rs`. Your tool's `name` must be a clean identifier the parser and dispatch
  (`agent/runner.rs`) can match; MCP tools use the `mcp__{server}__{tool}` prefix.
- **CPU-heavy work goes on `tokio::task::spawn_blocking`** — never block the async worker or the
  event loop. `question` and `task` are special-cased in the agent loop *before* registry dispatch
  (they're declared as tools so the model sees them, but never run through `ToolRegistry::execute`);
  dispatch compares against `QUESTION_TOOL_NAME` / `TASK_TOOL_NAME` constants, not literals.

## Minimal example (shape mirrors `tools/skill.rs`)

Copy-paste skeleton: `assets/tool-template.rs` (compile-shaped, with `// AUTHOR:` edit spots).

```rust
use super::*;
use serde_json::{Value, json};

pub struct MyTool;

#[async_trait]
impl Tool for MyTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "my_tool".to_string(),
            description: "One-line, model-facing: what it does and when to use it.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "…" }
                },
                "required": ["target"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let Some(target) = args["target"].as_str() else {
            return ToolResult::error("Missing 'target' argument");
        };
        // …work; heavy/blocking work → tokio::task::spawn_blocking…
        ToolResult::success(&format!("did it for {target}"))
    }
}
```

Then add `pub mod mymod;` in `tools/mod.rs` and the one `self.register(...)` line. Registry tests
in `registry.rs` confirm expected tools resolve — add one if the platform gating is non-obvious.
