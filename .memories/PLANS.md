# PLANS
> Roadmap, active priorities, and ideas.
> Last updated: 2026-06-30 (sub-agents/multichat shipped; god-object refactor mostly done)

## RECENTLY SHIPPED (`[DONE]` — was on this roadmap)

- ✅ **Sub-agents** (model `task` tool + `/agent`/`/agents`, fg+bg) — was an `[IDEA]`.
- ✅ **`/btw` sidechat + parallel sessions** (`/new`/`/chats` + Tab) — concurrent streams, no connection loss.
- ✅ **Conversation abstraction + provider `fork()`** — isolated session per chat.
- ✅ **God-object `App` decomposition** — sub-state modules + controllers (`AgentRuntime`, `system_prompt::build`, `BackgroundCounters`, `McpStatus`). `mod.rs` 2.4k → 925.
- ✅ **GOAL mode overhaul** with safety checks; pure `apply_verdict` core.

## ACTIVE (`[WIP]`)

| Priority | What | Why |
|----------|------|-----|
| P1 | Decompose `handle_key` (`keys.rs`, 878 lines) into key→intent + intent→effect | Next clean-refactor step; makes key handling testable without a live `App`. Not started. |
| P0 | Multi-theme support | Only Catppuccin Mocha; `ui.theme` is currently ignored |
| P1 | Error recovery polish | Retry/backoff exists but no jitter, no total-time cap, no `Retry-After` |
| P1 | GOAL hard iteration cap | Prevent infinite agent↔evaluator loops |
| P2 | Tool approval for background conversations | A non-focused chat's pending approval can't surface until focused |
| P2 | Mouse support | Scroll, click-to-select in TUI |
| P2 | Copy/paste | System clipboard integration |

## SHORT-TERM (`[TODO]` — next)

| Priority | What | Why |
|----------|------|-----|
| P0 | **Auto-load `.memories/` into the agent** | The "Integrate memories" commit only created the docs; `build_system_prompt` still doesn't read them. Goal: agents understand the project cold. (`app/mod.rs:1480`) |
| P1 | MCP tool-arg schema validation | Args passed unchecked to `tools/call` |
| P1 | `cargo clippy` clean pass | Lint debt |
| P1 | RAG / codebase search | Semantic search across project files |
| P2 | Test infrastructure | Smoke tests + parser/provider unit tests |
| P2 | Token usage tracking | DeepSeek streaming returns none; currently estimated `len/4` |

## LONG-TERM

| What | Why |
|------|-----|
| Multi-provider (OpenAI, Anthropic, local) | `ProviderKind` already has the slots; vendor independence |
| Plugin system | Third-party tool extensions |
| Remote session sharing | Multi-device workflow |
| Windows MSI installer + GitHub Actions CI/CD | Distribution (Phase 5) |

## IDEAS (`[IDEA]`)

- `[IDEA]` Inline image rendering in TUI (Sixel / Kitty protocol)
- `[IDEA]` Voice input via whisper.cpp
- `[IDEA]` Richer VSCode integration over ACP
- `[IDEA]` Built-in git integration (auto-commit suggestions, diff view)
- `[IDEA]` Structured (JSON) GOAL verdict instead of markdown `**Status:**` parsing
- `[IDEA]` Split-screen / multi-viewport rendering (today: single focused viewport + `/chats` picker)

## DECIDED AGAINST

- `[IDEA]` Electron/GUI wrapper — TUI is the identity, keep it terminal-native
- `[IDEA]` Database-backed sessions — JSON files are simpler and debuggable
