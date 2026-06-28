# LEARNINGS
> Hard-won technical knowledge. Gotchas. Patterns.
> Last updated: 2026-06-28T17:12

## DEEPSEEK API

| Topic | Detail |
|-------|--------|
| Auth | DeepSeek web API uses cookie-based session, not API key. Session ID + token from web login. |
| PoW | Every API call requires SHA-3 proof-of-work. WASM blob solves this. `→ src/provider/pow.rs` |
| Streaming | SSE events: `ready`, `update_session`, `title`, `close`, `fragment`, `content`, `field`, `batch`, `token` |
| Endpoints | ~15 reverse-engineered endpoints: chat, sessions, files, search, sharing, user, feedback |
| Fragility | No documented API — may break without notice. All endpoints reverse-engineered from webapp. |

## RUST PATTERNS

| Topic | Detail |
|-------|--------|
| Error handling | `AppError` enum with `thiserror`. `AppResult<T>` alias throughout. `→ src/error.rs` |
| Async | tokio runtime. `LLMProvider` is `#[async_trait]`. |
| TUI | ratatui with crossterm backend. `App` owns all state, `render()` is called each frame. |
| Events | `tokio::select!` multiplexes: stdin key events, agent channel, tool channel, tick. |
| Config | `serde` + `toml` deserialize. Path resolved via `dirs` crate. |
| MCP | Generic JSON-RPC 2.0 client. No schema validation on tool args (TODO). |

## MCP GOTCHAS

- Tool names in MCP are prefixed: `mcp__{server_name}__{tool_name}`
- Stdio transport: spawn child process, write JSON-RPC to stdin, read from stdout
- HTTP transport: POST to SSE endpoint for init, separate POST for calls
- Auto-discovery checks 5 config sources; workspace config wins over global
- Cache TTL for tool lists configured in `config.toml` (default: 60s)

## AGENT LOOP

- Tool calls parsed from raw LLM output (3 formats: XML `<tool_use>`, legacy `[TOOL:name]`, JSON)
- Agent runs up to `max_steps` (default 25) or `max_tools` (default 50) per conversation
- Context compaction triggered at ~32K tokens — uses summary prompt
- Tool approval dialog sends `AppEvent::RequestToolApproval`, blocks until user responds
- Background PTY processes persist across agent steps (use `shell_kill` to clean up)

## BUILD

- `lto = "fat"`, `codegen-units = 1`, `strip = true` in release profile
- WASM runtime: `wasmtime` crate for PoW solver
- `portable-pty` for background shell sessions
- `syntect` with `base16-ocean-dark` theme for syntax highlighting
- `pulldown-cmark` for markdown parsing
