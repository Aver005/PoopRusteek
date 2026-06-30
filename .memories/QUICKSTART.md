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
| `src/app/mod.rs` | Coordinator: App+AppState, event loop, `handle_event`, `send_to_agent` (no longer a god-file) | 925 |
| `src/app/conversation.rs` | `Conversation` + `Conversations` store (multi-chat core) | 154 |
| `src/app/multichat.rs` | sub-agents / sidechat / parallel-session spawning + focus | 364 |
| `src/app/runtime.rs` | `AgentRuntime` — the one place agent turns launch (`spawn(TurnSpec)`) | 63 |
| `src/app/keys.rs` | key handling | 878 |
| `src/app/goal.rs` | GOAL state machine + pure `apply_verdict` | 512 |
| `src/app/events.rs` | `AppEvent` (id-tagged), `Modal`, `GoalStage`, `QuestionState` | 452 |
| `src/provider/deepseek.rs` | DeepSeek web API client; `fork_session()`/`fork()` | ~1819 |
| `src/provider/pow.rs` | SHA-3 PoW solver via `wasmtime` | 245 |
| `src/agent/runner.rs` | Agent loop (LLM ↔ tools, streaming, `task` tool) | 370 |
| `src/agent/sub_agent.rs` | Headless isolated sub-agent runner | 114 |
| `src/tools/background.rs` | Background + interactive PTY processes | 744 |
| `src/tui/render.rs` | All views (landing/chat/MCP/modals/status) | ~1336 |
| `src/mcp/` | MCP clients, transports, discovery | — |
| `src/commands/defs/` | 28 slash commands (one file each) | — |
| `src/config/mod.rs` | Config schema + storage paths | 134 |
| `assets/prompts/` | System prompts + built-in skills | — |

## KEY ARCHITECTURE

```
main ─ App (single tokio::select! loop @120ms; thin coordinator)
        ├─ AppState
        │    └─ Conversations (focused + background; each owns msgs + forked provider + task)
        ├─ AgentRuntime.spawn(TurnSpec) ─ run_agent_loop (spawned task; events tagged by ConversationId)
        │    └─ task tool ⇒ run_sub_agent (fg) | SpawnSubAgent (bg)
        ├─ ToolRegistry  (bash, powershell, question, shell_*, skill, mcp__*)
        ├─ MCPManager    (external tool servers; stdio/http/sse)
        ├─ DeepseekProvider (PoW + SSE streaming; fork() → isolated session per conversation)
        └─ TUI render (reads &AppState, focused conversation only; never mutates)
```

## CRITICAL NOTES

- DeepSeek requires **PoW** (SHA-3 via WASM blob `assets/sha3_wasm_bg.7b9ca65ddd.wasm`) — see `reference/PROVIDER.md`.
- **No native function-calling** — tool calls are parsed from raw LLM text (3 formats). See `reference/TOOLS.md`.
- **`.memories/` is NOT read by the app.** `### LOCAL MEMORY` in the prompt is just a history label, unrelated.
- Config: `{config_dir}/pooprusteek/config.toml`; sessions/history: `{data_dir}/pooprusteek/` (platform-specific via `dirs`). Details: `reference/CONFIG.md`.
- Agent defaults: `max_steps_per_turn=256`, `max_tools_per_step=10` (NOT 25/50 — old memory was wrong).
- Two run modes: TUI (default) and ACP server (`--acp`, JSON-RPC over stdio).
- Verification baseline: `cargo build`. Tests are minimal; `cargo clippy` not yet clean.
