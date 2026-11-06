# QUICKSTART
> Agent: read this first. ~15s.

## WHAT

**Pooprusteek** (Пупра́стик) — a Rust TUI coding agent; a DeepSeek-powered alternative to Claude Code.
Rust rewrite of the TypeScript **Poopseek**. ~15k LOC, edition 2024, MSRV 1.91.

- TUI: `ratatui` + `crossterm` (Catppuccin Mocha theme)
- LLM: DeepSeek **web** API (reverse-engineered; cookie/token auth + SHA-3 PoW via WASM),
  plus API accounts and CLI subscriptions through the gateway (`src/gateway/`)
- Async: Tokio, event-driven (single `select!` loop, agent runs in a spawned task)
- Build: `cargo build` · Run: `cargo run` · Help: `cargo run -- --help` · ACP server: `cargo run -- --acp`

## WHERE (hot files)

| Path | Role |
|------|------|
| `src/main.rs` | Entry; CLI flags and subcommands (`--acp`, `--config`, `--data-dir`, `secrets`, `doctor`, `trust`, harness) |
| `src/app/mod.rs` | Coordinator: App+AppState, event loop, `handle_event` |
| `src/app/conversation.rs` | `Conversation` + `Conversations` store (multi-chat core) |
| `src/app/gateway_view.rs`, `src/app/gateway_view/` | Accounts/routes/roles screen state, row building, edits, wizard, forms |
| `src/app/keys/accounts.rs` | Screen, wizard, masked secret input and trust dialog keys |
| `src/app/titles.rs` | Service role `title`: one background request per chat |
| `src/gateway/handle.rs` | `Gateway` facade, `Decision`, limits, ledger view |
| `src/gateway/engine.rs` | Selection, cooldown, edits, factories, build |
| `src/gateway/routed.rs` | `RoutedProvider`: fallback, fork, route state |
| `src/gateway/cli/` | Claude Code / OpenCode / Codex adapters |
| `src/secrets/`, `src/config/` | Keyring, references, layered config, migration, save queue |
| `src/provider/deepseek/` | DeepSeek web API client; `fork_session()`/`fork()` |
| `src/provider/failure.rs` | `FailureClass`, `ProviderFailure`, `Retry-After`, limit headers |
| `src/agent/runner.rs` | Agent loop (LLM ↔ tools, streaming, `task` tool) |
| `src/tui/render/` | Layered renderers; `layout.rs`/`dock.rs` — the one skeleton |
| `src/harness/` | `exec`/`scenario`/`suite`/`mine`/`mock-provider`/`mock-cli` |
| `assets/prompts/` | System prompts + built-in skills |

## KEY ARCHITECTURE

```
main ─ App (single tokio::select! loop @120ms; thin coordinator)
        ├─ AppState
        │    └─ Conversations (focused + background; each owns msgs + forked provider + task)
        ├─ AgentRuntime.spawn(TurnSpec) ─ run_agent_loop (spawned task; events tagged by ConversationId)
        │    └─ task tool ⇒ run_sub_agent (fg) | SpawnSubAgent (bg)
        ├─ ToolRegistry  (bash, powershell, question, shell_*, skill, mcp__*)
        ├─ MCPManager    (external tool servers; stdio/http/sse)
        ├─ Gateway       (accounts · routes · roles · limits · /serve) → RoutedProvider
        │    └─ Backends: DeepSeek web · API clients · Claude/OpenCode CLI
        ├─ SecretsStore  (keyring + references) ← layered config (~/.pooprusteek + project)
        └─ TUI render (reads &AppState, focused conversation only; never mutates)
```

## CRITICAL NOTES

- DeepSeek requires **PoW** (SHA-3 via WASM blob `assets/sha3_wasm_bg.7b9ca65ddd.wasm`) — see `reference/PROVIDER.md`.
- Tool calls are parsed from raw LLM text for DeepSeek and prompt-mode accounts; native tool calling exists for `/providers` entries with `tools = "native"`. See `reference/TOOLS.md`.
- **`.memories/` is NOT read by the app.** `### LOCAL MEMORY` in the prompt is just a history label, unrelated.
- Config: **`~/.pooprusteek/config.toml`** (shared) + optional `<repo>/.pooprusteek/config.toml`. Data: `dirs::data_local_dir()/pooprusteek`. Secrets: OS keyring. Details: `reference/CONFIG.md`.
- Agent defaults: `max_steps_per_turn=256`, `max_tools_per_step=10` (NOT 25/50 — old memory was wrong).
- Two run modes: TUI (default) and ACP server (`--acp`, JSON-RPC over stdio).
- Verification baseline: `cargo build` + `cargo test --bin pooprusteek` + `cargo clippy --bin pooprusteek`.
- Before accepting a branch: `git ls-files --others --ignored --exclude-standard -- src` — `.gitignore` once hid `src/cli/secrets.rs` and a whole track built only because the files were still on disk.
