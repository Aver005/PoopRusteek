# PLANS
> Roadmap, active priorities, and ideas.
> Last updated: 2026-08-26 (context-compaction ladder decided, not built — SHORT-TERM P0 + `.docs/context-compaction.md`). Before: 2026-07-06 (OpenAI-compatible server mode SHIPPED — moved off ACTIVE)

## RECENTLY SHIPPED (`[DONE]` — was on this roadmap)

- ✅ **OpenAI-compatible server mode** (2026-07-06) — `src/server/`: hyper-1 listener behind `--serve`/`--server`/`--api` + `/serve on|off` + `/server <port>` (persisted `[server]` config, default port 7667). Serves ALL providers, not just the active one: `deepseek-chat`/`deepseek-reasoner` + `<entry>/<model>` (caller-chosen sub-models pass through). Decisions taken as planned: stateless fork-per-request + `discard_remote_session` for DeepSeek; optional bearer auth; server requests bypass the tool loop (plain completions, v1). Extension seam: `config::ServerApi` reserves `anthropic`/`gemini` inbound dialects (currently 501; outbound halves already exist in `provider/{anthropic,gemini}_compat`).
- ✅ **Sub-agents** (model `task` tool + `/agent`/`/agents`, fg+bg) — was an `[IDEA]`.
- ✅ **`/btw` sidechat + parallel sessions** (`/new`/`/chats` + Tab) — concurrent streams, no connection loss.
- ✅ **Conversation abstraction + provider `fork()`** — isolated session per chat.
- ✅ **God-object `App` decomposition** — sub-state modules + controllers (`AgentRuntime`, `system_prompt::build`, `BackgroundCounters`, `McpStatus`). `mod.rs` 2.4k → 925.
- ✅ **GOAL mode overhaul** with safety checks; pure `apply_verdict` core.

## ACTIVE (`[WIP]`)

| Priority | What | Why |
|----------|------|-----|
| P2 | Server mode: anthropic/gemini inbound dialects | `[server] api` accepts them but answers 501 — needs inbound wire→internal conversions (outbound halves exist in `provider/{anthropic,gemini}_compat`) + per-dialect routes in `src/server/` (`ApiDialect` dispatch seam is ready in `server/http.rs::route`) |
| P2 | Server mode: agent-loop-backed completions | v1 serves plain completions (no tools); an opt-in "agentic" model id could run `run_agent_loop` instead |
| P0 | Multi-theme support | Only Catppuccin Mocha; `ui.theme` is currently ignored |
| P1 | Error recovery polish | Retry/backoff exists but no jitter, no total-time cap, no `Retry-After` |
| P1 | GOAL hard iteration cap | Prevent infinite agent↔evaluator loops |
| P2 | Tool approval for background conversations | A non-focused chat's pending approval can't surface until focused |
| P2 | Mouse support | Scroll, click-to-select in TUI |
| P2 | Copy/paste | System clipboard integration |

## SHORT-TERM (`[TODO]` — next)

| Priority | What | Why |
|----------|------|-----|
| P0 | **Context compaction — four-rung ladder** (decided 2026-08-26, not implemented) | Closes review findings #3 (`max_context_messages`/`auto_compact` never read) and #4 (`/compact` is a stub). Compaction is a ladder of four rungs, cheapest first: (0) budget tool output at capture, (1) clear old tool-result bodies with the full output spilled to disk, (2) a history boundary — for DeepSeek this means resetting the server-side session — (3) an LLM summary as the last resort. Checked only at turn boundaries, never mid tool-chain. The summary is written by the same model that runs the conversation, not a separate cheap model. Computable facts — file lists, commands run, error text — are filled in by the harness from tool-call history, not asked of the model. Full reasoning and the rejected alternatives (separate summarizer model, 9/18-field summary schemas, mid-chain compaction, reactive-only triggers, post-hoc size rollback): `.docs/context-compaction.md`. |
| P1 | **Auto-load `.memories/` into the agent** | **Partially done 2026-08-29**: `src/instructions.rs` now auto-loads the project's `AGENTS.md`/`CLAUDE.md` chain into the system prompt, so this repo's root `CLAUDE.md` (which bridges to `.memories/`) arrives automatically. What remains is the curated read order itself — `INDEX.md` → `STATE.md` → … — which is far larger than the 16 KiB instruction budget and needs the same `auto` treatment skills got (head + `read_file` on demand), not a verbatim paste. |
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
- Compaction by a **separate cheaper model** — Roo removed the option (PR #10901) and Goose reverted to the main model (PR #11255); a summariser that drops a constraint or a path corrupts every turn after it. Reasoning: `.docs/context-compaction.md` §5.1
- Compaction **mid tool-chain** (Codex does it) — a chain that compacts between a call and its result leaves the model unable to say what it was doing. Accepted cost: a long chain can hit the limit without ever being checked. §5.3
- **Reactive-only** compaction on a provider overflow error — LM Studio answers `finish_reason: "length"` instead of an error, so the refusal is not reliably detectable. §5.3
- Summary schemas of **9 or 18 fields** (Claude Code, OpenHands) — measured worse than prose on token cost at 18 fields, and unusable output length at 9 for small models. Six sections. §5.2
- **Post-hoc rollback** when a summary comes out larger than what it replaced — Roo shipped that guard and later deleted it (PR #10920); check that the summary prompt fits *before* calling instead. §5.5
