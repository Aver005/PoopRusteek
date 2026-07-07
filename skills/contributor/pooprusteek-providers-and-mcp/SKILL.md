---
name: PoopRusteek Providers & MCP
description: How PoopRusteek's LLM-provider and MCP subsystems are structured, for anyone editing src/provider/ or src/mcp/ — the LLMProvider trait + fork(), the four protocols, MCP transports/discovery/OAuth, and the lock-across-await invariant. Use before touching provider or MCP code.
---

# PoopRusteek Providers & MCP

Sources: `src/provider/`, `src/mcp/`. References: `.memories/reference/PROVIDER.md`, `.memories/reference/MCP.md`.

## Providers (`src/provider/`)

The `LLMProvider` trait (`provider/mod.rs`) is the abstraction every backend implements:

- `complete` (non-streaming) · `complete_stream(request, tx)` (SSE over an mpsc sender) ·
  `model()` · `reset()`.
- **`fork() -> Arc<dyn LLMProvider>`** is the isolation primitive: each `Conversation` gets its own
  forked provider so per-conversation sessions never cross-talk. DeepSeek rebuilds a fresh
  `SessionState` (`fork_session`); the stateless HTTP providers just return a new instance.
- Session-resume methods `session_identity` / `session_is_alive` / `adopt_session` let a saved
  DeepSeek session be re-adopted across restarts (default no-op for other providers).

Four implementations:

- **Built-in DeepSeek web client** (`deepseek.rs`, split into `deepseek/{mod,http,session,stream,endpoints}.rs`).
  Reverse-engineered `chat.deepseek.com/api/v0`, **cookie/token auth (not an API key)**. Every
  PoW-gated call needs a solved challenge in the `x-ds-pow-response` header — SHA-3 solved via a
  **WASM blob** (`assets/sha3_wasm_bg.*.wasm`) run through `wasmtime` (`provider/pow.rs`). The web
  API takes a single flattened `prompt` string (history is assembled in `provider/prompt.rs`), and
  responses stream as nested-JSON SSE (`provider/sse.rs` `SseLineBuffer`, byte-based). PoW solving
  is CPU-heavy → `spawn_blocking`.
- **OpenAI Chat Completions** (`openai_client.rs` + `openai_compat.rs` wire types) — one POST per
  turn, `data: [DONE]` terminator, bearer auth, stateless `fork()`.
- **Anthropic Messages** (`anthropic_client.rs` + `anthropic_compat.rs`) — `x-api-key` +
  `anthropic-version`, top-level `system`, strict role alternation, typed SSE.
- **Gemini** (`gemini_client.rs` + `gemini_compat.rs`) — model-in-URL
  (`:generateContent` / `:streamGenerateContent?alt=sse`), `x-goog-api-key`.

The three extra providers are managed by `/providers` (state in `app/providers.rs`) and built from
`config::ProviderEntry` by the single construction point `provider::build_provider`. The `_compat`
files are **pure** wire-format conversions (no I/O) — keep them testable and side-effect-free.

For per-protocol wire details (endpoint, auth, request/streaming/response mapping, `LLMProvider`
surface + `fork()`), anchored to `file::function` names, see
[`references/protocol-mapping.md`](references/protocol-mapping.md).

## MCP (`src/mcp/`)

External servers expose tools namespaced `mcp__{server}__{tool}`; the agent loop dispatches any
call whose name starts with `mcp__` to `MCPManager::call_tool` (`agent/runner.rs`).

- **Transports** (`transport.rs`): **stdio** (subprocess, JSON+`\n`, 60s/req; Windows auto-retries
  `.cmd`/`.bat` so `npx` resolves), **HTTP** (reqwest + cookie store, tracks `MCP-Session-Id`,
  SSE-fallback), **SSE** (`text/event-stream`, id-matched), **Dummy** (disabled servers).
  Protocol version `2024-11-05`; handshake `initialize` → `notifications/initialized` → `tools/list`.
- **8-source config discovery** (`config.rs`, first-found-wins): own `mcp.json` → workspace
  (`./mcp.config.json`, `.vscode/mcp.json`) → global → Claude Desktop → VSCode → Claude CLI →
  Cursor → Opencode. `persist_config` writes only pooprusteek-owned servers (foreign servers get
  enable/disable overrides, never secret copying).
- **Manager** (`manager.rs`): server lifecycle, per-server tool cache (default TTL 300s, override
  `/mcp ttl`), `connect_all` concurrent, lock-free `client_for(name)` handles, `shutdown_all` on exit.
- **OAuth** (`oauth.rs` / `oauth_store.rs`, `/mcp auth`): a 401 during connect surfaces as
  `AuthRequired`; the flow does RFC 9728 protected-resource discovery → RFC 8414 / OIDC AS metadata
  → RFC 7591 **dynamic client registration** (no DCR support = hard error) → PKCE S256 → local
  loopback redirect → token exchange. Tokens live in the **OS keyring** (service `pooprusteek-mcp`),
  never in `mcp.json`; keyring calls run on `spawn_blocking`. `/mcp add` (JSON paste / wizard /
  one-liner shorthand) converges on `add_new_server`.

## The load-bearing invariant

**Never hold the `MCPManager` lock (or any shared `Mutex`) across an I/O `.await`.** A slow tool
call under the lock freezes every other consumer, including the UI. The manager exposes lock-free
`client_for(name)` handles precisely so the network `.await` happens outside the lock; status
polling uses `try_lock`. Same rule as the app-wide invariant #2 — the MCP subsystem is where it
bites hardest.
