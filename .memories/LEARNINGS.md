# LEARNINGS
> Hard-won technical knowledge. Gotchas. Patterns. (Deep detail lives in `reference/`.)
> Last updated: 2026-06-30

## DEEPSEEK API  (full detail → `reference/PROVIDER.md`)

| Topic | Detail |
|-------|--------|
| Auth | DeepSeek **web** API: cookie/token session + spoofed Android client headers. NOT the public API-key product. |
| PoW | Every gated call needs a solved SHA-3 challenge in `x-ds-pow-response`. Solved by bundled WASM via `wasmtime`. `→ src/provider/pow.rs` |
| Prompt shape | Web API takes ONE `prompt` string; history is flattened with `### USER INPUT` / `### TOOL RESULT` / `### LOCAL MEMORY` labels. |
| `### LOCAL MEMORY` | Just a history-section label — does **NOT** load `.memories/` files. Don't conflate. |
| SSE | Mixed named (`ready/update_session/title/close`) + unnamed (`o`/`p`/`v` op-path-value) events; ~9 fallback text-extraction paths. |
| Usage | Token usage never returned by streaming → `usage` is always None; counts are estimated `len/4`. |
| Fragility | Undocumented, reverse-engineered; client version `1.8.0` hardcoded. May break anytime. |

## CONFIG / DEFAULTS  (→ `reference/CONFIG.md`)

- **Correct agent defaults**: `max_steps_per_turn = 256`, `max_tools_per_step = 10`, `max_context_messages = 256`. (Older memory wrongly said 25 / 50.)
- Paths use the `dirs` crate → platform-specific. Config = `{config_dir}/pooprusteek/config.toml`; data (sessions, history, mcp.json) = `{data_dir}/pooprusteek/`.
- Sessions are JSON; ids are time-sortable (`{rfc3339}-{uuid8}`). History capped at 500.

## RUST PATTERNS  (→ `CONVENTIONS.md`)

| Topic | Detail |
|-------|--------|
| Errors | `AppError` (`thiserror`) + `AppResult<T>` everywhere; `color-eyre` at the top level. |
| Async | Tokio; `LLMProvider`/`Tool` are `#[async_trait]`. Blocking PTY work on `spawn_blocking`. |
| Event loop | One `tokio::select!` @120ms over tick / crossterm / internal channel / Ctrl+C. Agent runs in a spawned task. |
| State | Only the main loop mutates `AppState`; async tasks talk via `AppEvent` + `Notify` handshakes. |
| Cross-task req/resp | `Arc<Mutex<Option<T>>> + tokio::Notify` (tool approval, questions). |
| Assets | Resolve via `CARGO_MANIFEST_DIR` → CWD → exe-dir (prompts, PoW WASM). |

## AGENT LOOP  (→ `reference/TOOLS.md`)

- Tool calls parsed from raw LLM text — 3 formats: XML `<tool_use>`, legacy `[TOOL:name]`, JSON. No native function-calling.
- Loop: up to `max_steps` steps, `max_tools_per_step` tools each. 120s idle stream timeout.
- `question` tool is **special-cased** (opens a modal, no approval); all other tools go through approval.
- `summarize_tool_result` truncates at 200 bytes on a **char boundary** (emoji-safe; tested).
- `stream_visible_text` hides partial tool tags during streaming — but over-eagerly cuts at any `<`.

## BACKGROUND / PTY  (→ `reference/TOOLS.md`)

- `tools/background.rs` runs detached (`spawn_background`, piped) or interactive (`spawn_interactive`, real PTY via `portable-pty`).
- Persistent jobs (dev servers) survive turns; idle TTL default 1800s. Non-persistent killed each user turn; all killed on exit.
- `shell_input` maps named keys to terminal escape sequences (up/down/enter/esc/ctrl+c…).
- Windows: foreground processes use `CREATE_NO_WINDOW`/DETACHED to avoid corrupting the TUI; force-kill via `taskkill /F /T`.

## MCP  (→ `reference/MCP.md`)

- Tool names prefixed `mcp__{server}__{tool}`. Protocol version `2024-11-05`.
- Three transports: stdio (spawn+stdin/stdout, 60s), HTTP (30s, `MCP-Session-Id`, SSE-fallback), SSE.
- Config auto-discovered from **8 sources** (own → workspace → global → Claude Desktop → VSCode → Claude CLI → Cursor → Opencode), first-found-wins. (Older memory said 5.)
- Tool lists cached per-server; TTL default 300s, set via `/mcp ttl`.
- Windows stdio retries `.cmd`/`.bat` so `npx` resolves.

## GOAL MODE  (→ `ARCHITECTURE.md`)

- `/goal` arms it. Flow: prompt → define goal → Agent 1 works → Evaluator (non-streaming) judges → SUCCESS or feedback-retry.
- Evaluator uses `goal-evaluator.prompt.md`, structured `**Status:** SUCCESS/FAILURE`.
- Session swap: 3 agent-1 failures → fresh worker session; 5 evaluator failures → fresh evaluator session.
- Evaluator sessions tagged `__goal_system__` (hidden from `/sessions`). ⚠ No hard iteration cap yet.

## META: THE `.memories` SYSTEM

- This folder is the canonical agent onboarding doc. **The app does not read it** — point agents at `.memories/INDEX.md`.
- Keep `→ file:line` references; when memory and code disagree, the code wins.

## BUILD
- `lto="fat"`, `codegen-units=1`, `strip=true` (release). `wasmtime` for PoW, `portable-pty` for shells, `syntect`+`pulldown-cmark` for rendering.
