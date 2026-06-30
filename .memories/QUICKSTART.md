# QUICKSTART
> Agent: read this first. ~15s.

## WHAT

**Pooprusteek** (Пупра́стик) — a Rust TUI coding agent; a DeepSeek-powered alternative to Claude Code.
Rust rewrite of the TypeScript **Poopseek**. ~15k LOC, edition 2024, MSRV 1.85.

- TUI: `ratatui` + `crossterm` (Catppuccin Mocha theme)
- LLM: DeepSeek **web** API (reverse-engineered; cookie/token auth + SHA-3 PoW via WASM)
- Async: Tokio, event-driven (single `select!` loop, agent runs in a spawned task)
- Build: `cargo build` · Run: `cargo run` · Help: `cargo run -- --help` · ACP server: `cargo run -- --acp`

## WHERE (hot files)

| Path | Role | Size |
|------|------|------|
| `src/main.rs` | Entry, CLI flags (`--acp`, `--debug_log`) | 85 |
| `src/app/mod.rs` | **God-file**: App+AppState, event loop, key handling, GOAL mode | ~2382 |
| `src/app/events.rs` | `AppEvent`, `Modal`, `GoalStage`, `QuestionState` | 474 |
| `src/provider/deepseek.rs` | DeepSeek web API client (biggest after app) | ~1819 |
| `src/provider/pow.rs` | SHA-3 PoW solver via `wasmtime` | 245 |
| `src/agent/runner.rs` | Agent loop (LLM ↔ tools, streaming) | 272 |
| `src/agent/tool_parser.rs` | Parse tool calls (XML / `[TOOL:]` / JSON) | 195 |
| `src/tools/background.rs` | Background + interactive PTY processes | 744 |
| `src/tui/render.rs` | All views (landing/chat/MCP/modals/status) | ~1336 |
| `src/mcp/` | MCP clients, transports, discovery | — |
| `src/commands/defs/` | 25 slash commands (one file each) | — |
| `src/config/mod.rs` | Config schema + storage paths | 134 |
| `assets/prompts/` | System prompts + built-in skills | — |

## KEY ARCHITECTURE

```
main ─ App (single tokio::select! loop @120ms)
        ├─ run_agent_loop (spawned task; LLM ↔ tools via channels)
        ├─ ToolRegistry  (bash, powershell, question, shell_*, skill, mcp__*)
        ├─ MCPManager    (external tool servers; stdio/http/sse)
        ├─ DeepseekProvider (PoW + SSE streaming)
        └─ TUI render (reads &AppState; never mutates)
```

## CRITICAL NOTES

- DeepSeek requires **PoW** (SHA-3 via WASM blob `assets/sha3_wasm_bg.7b9ca65ddd.wasm`) — see `reference/PROVIDER.md`.
- **No native function-calling** — tool calls are parsed from raw LLM text (3 formats). See `reference/TOOLS.md`.
- **`.memories/` is NOT read by the app.** `### LOCAL MEMORY` in the prompt is just a history label, unrelated.
- Config: `{config_dir}/pooprusteek/config.toml`; sessions/history: `{data_dir}/pooprusteek/` (platform-specific via `dirs`). Details: `reference/CONFIG.md`.
- Agent defaults: `max_steps_per_turn=256`, `max_tools_per_step=10` (NOT 25/50 — old memory was wrong).
- Two run modes: TUI (default) and ACP server (`--acp`, JSON-RPC over stdio).
- Verification baseline: `cargo build`. Tests are minimal; `cargo clippy` not yet clean.
