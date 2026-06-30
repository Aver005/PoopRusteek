# STATE
> Live project snapshot. Update on every meaningful change.
> Last updated: 2026-06-30 (post conversation-unification, sub-agents, sidechat, controllers)

## PHASE COMPLETION

| Phase | Status | What |
|-------|--------|------|
| 1 Core | `[DONE]` | TUI, provider trait, DeepSeek client, agent loop, tools, MCP types, PoW, streaming |
| 2 Features | `[DONE]` | Onboarding, sessions, 25 slash commands, markdown+syntect, compaction, @file, tool approval, input history |
| 3 Integration | `[DONE]` | MCP stdio/HTTP/SSE, 8-source auto-discovery, manager+caching, JSON-RPC, ACP server mode |
| 3.5 Agentic | `[DONE]` | GOAL mode (2-agent iterative loop), background/interactive PTY jobs, `/jobs` `/ps`, skills system |
| 3.6 Multi-chat | `[DONE]` | `Conversation`/`Conversations` model, provider `fork()`, event tagging by `ConversationId`, parallel sessions (`/new` `/chats` + Tab), `/btw` sidechat, sub-agents (model `task` tool + `/agent`/`/agents`, fg+bg) |
| 3.7 Architecture | `[DONE]` | God-object `App` decomposed (mod.rs 2.4k→925): sub-state modules + controllers (`AgentRuntime`, `system_prompt::build`, `BackgroundCounters`, `McpStatus`). Provider split (prompt/sse/fake). |
| 4 Polish | `[WIP]` | Multi-theme, mouse, copy/paste, error recovery, rate limiting (retry/backoff exists), schema validation |
| 5 Distribution | `[TODO]` | Release builds, cross-compile, installers, CI/CD, man page |

## BUILD STATUS

| Check | Status |
|-------|--------|
| `cargo build` | ✅ Passes (126 warnings, pre-existing, mostly dead-code on unused fields) |
| `cargo clippy` | ⚠️ Not verified / not clean |
| Tests | **84 passing** (`cargo test --bin pooprusteek`) — grew with provider `fork`, goal `apply_verdict` (pure core), conversation, tool-parser, runner |

## CURRENT FOCUS

1. Clean-refactor sequence (done so far): conversation unification → controllers (AgentRuntime ✅, system_prompt ✅, background/MCP ✅).
2. Next candidate: decompose `handle_key` (878-line `keys.rs`) into key→intent + intent→effect (testable without a live `App`). Not started.
3. Phase-4 polish (multi-theme on hold; mouse, copy/paste, error recovery).
4. Hardening GOAL mode (add a hard iteration cap); MCP tool-arg schema validation.

## KNOWN GAPS

- **`.memories/` is not auto-loaded** by the agent (verified: nothing in `src/` reads it). The "Integrate memories" commit only created the docs.
- Single provider (DeepSeek only); `openai`/`custom` kinds declared but unimplemented. (`FakeProvider` exists for tests only.)
- Tool-approval modal blocks the event loop while open; a non-focused conversation with a pending approval can't surface its modal until focused.
- No max-iteration cap in GOAL cycle → infinite-loop risk.
- No schema validation on MCP tool arguments.
- DeepSeek streaming never reports token usage (`usage` always None).
- Retry loop with `max_retries=-1` can hang forever (no total-time cap, no jitter, no `Retry-After`).
- Theme hardcoded (Catppuccin Mocha); `ui.theme` ignored.
- No persistent RAG / codebase search.

## RECENTLY CLOSED (was a gap/bug, now fixed)

- ✅ **`parent_message_id` "evaporating messages" bug** — interrupted streams desynced the session tree onto an invisible branch. Fixed by incremental persist + flush-on-error in `deepseek.rs` (commit `183712e`) and structurally by per-conversation `fork()` isolation.
- ✅ **God-object `App`** — decomposed into sub-state modules + controllers; `mod.rs` 2.4k → 925 lines.
- ✅ **Single-turn limitation** — replaced by the `Conversation`/`Conversations` model; parallel sessions + sidechats + sub-agents stream concurrently without stream/connection loss.
- ✅ **GOAL mode wedges** — overhauled with safety checks (commit `c0d4280`); pure `apply_verdict` core.

## RECENT MILESTONES

| Date | Event |
|------|-------|
| 2026-06-24 | Project inception — core, features, integration built |
| 2026-06-28 | `.memories` system created (`32adf27`); GOAL mode + `/jobs` + `/ps` + PTY jobs (`e801dbe`) |
| 2026-06-30 | `.memories` deeply enriched: added ARCHITECTURE/GLOSSARY/CONVENTIONS + `reference/`; corrected drift (commands 22→25, MCP sources 5→8, agent defaults 25/50→256/10) |
| 2026-06-30 | **Big refactor + features wave**: provider split (prompt/sse/fake `42f6164`/`f783a87`); god-object decomposition (input/mcp_status/generation/goal/shell-unify/view-model/background-split `b252567`…`5205be4`); GOAL overhaul `c0d4280`; `parent_message_id` fix `183712e`; provider `fork()` + conversations `20c90ca`; `/btw` `438e60d`; `/chats` `6c04774`; sub-agents `38ce06f`; goal+multichat extract `92163cb`; **controllers** — conversation mgmt `4efe8cb`, AgentRuntime `c24c7a8`, system_prompt `24e6b00`, background_stats `391c6e4` |

## FACTS CORRECTED THIS PASS (were wrong in older memory)

- Agent defaults: `max_steps=256`, `max_tools_per_step=10` (was "25 / 50").
- Command count: **25** (+2 aliases) (was "22–23").
- MCP config discovery: **8 sources** (was "5").
- Built-in tools: **7 default** + `skill` (background/interactive PTY family is substantial).
