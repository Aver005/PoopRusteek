# STATE
> Live project snapshot. Update on every meaningful change.
> Last updated: 2026-07-02 (post stability/perf overhaul — all 7 audit criticals fixed, clippy 0)

## PHASE COMPLETION

| Phase | Status | What |
|-------|--------|------|
| 1 Core | `[DONE]` | TUI, provider trait, DeepSeek client, agent loop, tools, MCP types, PoW, streaming |
| 2 Features | `[DONE]` | Onboarding, sessions, 25 slash commands, markdown+syntect, compaction, @file, tool approval, input history |
| 3 Integration | `[DONE]` | MCP stdio/HTTP/SSE, 8-source auto-discovery, manager+caching, JSON-RPC, ACP server mode |
| 3.5 Agentic | `[DONE]` | GOAL mode (2-agent iterative loop), background/interactive PTY jobs, `/jobs` `/ps`, skills system |
| 3.6 Multi-chat | `[DONE]` | `Conversation`/`Conversations` model, provider `fork()`, event tagging by `ConversationId`, parallel sessions (`/new` `/chats` + Tab), `/btw` sidechat, sub-agents (model `task` tool + `/agent`/`/agents`, fg+bg) |
| 3.7 Architecture | `[DONE]` | God-object `App` decomposed (mod.rs 2.4k→925): sub-state modules + controllers (`AgentRuntime`, `system_prompt::build`, `BackgroundCounters`, `McpStatus`). Provider split (prompt/sse/fake). |
| 3.8 Stability & perf overhaul | `[DONE]` | 2026-07-02: all 7 audit criticals fixed (live streaming, MCP mutex freeze, GOAL evaluator off-loop, `/goal` registration, `--acp` panic, drain+dirty render with markdown/syntect cache, MCP stdio stderr+id-correlation) + ~30 majors (interaction queue, ui_only message split, atomic_write everywhere, CI, shell timeout/cap, background process-group kill, etc.) + dead-code sweep; clippy ~220→0, tests 84→189. See `reference/AUDIT-2026-07-02.md` + `JOURNAL/2026-07-02.md`. |
| 4 Polish | `[WIP]` | Multi-theme, mouse, copy/paste, error recovery, rate limiting (retry/backoff exists), schema validation |
| 5 Distribution | `[WIP]` | CI added (`.github/workflows/ci.yml`, build+test win+linux, clippy advisory); release builds, cross-compile, installers, man page still `[TODO]` |

## BUILD STATUS

| Check | Status |
|-------|--------|
| `cargo build` | Passes |
| `cargo clippy` | 0 warnings (was ~220 before the 2026-07-02 session) |
| Tests | **189 passing** (`cargo test --bin pooprusteek`), was 84 before the 2026-07-02 session |
| CI | `.github/workflows/ci.yml` — build+test on Windows and Linux; clippy runs advisory (`continue-on-error`) |

## CURRENT FOCUS

1. Stability/perf overhaul (2026-07-02) is done — all 7 audit criticals + ~30 majors fixed, dead code swept, clippy clean. See `reference/AUDIT-2026-07-02.md`.
2. Next candidates (in rough priority order):
   - `keys.rs` decomposition: key→intent + intent→effect split (testable without a live `App`). Not started.
   - `deepseek.rs` split (endpoints/http/session/stream) + dedupe the ~26 unused REST wrappers currently parked in a `#[allow(dead_code)]` impl block.
   - Dependency major-version bumps: `reqwest` 0.13, `ratatui` 0.30.2 + `crossterm` 0.29, `pulldown-cmark` 0.13, `toml` 1.x.
   - `wasmtime` 27 → native SHA-3 PoW reimplementation (drops the wasm dependency entirely).
3. Phase-4 polish (multi-theme on hold; mouse, copy/paste, error recovery).
4. MCP tool-arg schema validation (still absent).

## KNOWN GAPS

- **`.memories/` is not auto-loaded** by the agent (verified: nothing in `src/` reads it). The "Integrate memories" commit only created the docs; `CLAUDE.md` at the repo root now bridges into it for Claude Code specifically.
- Single provider (DeepSeek only); `openai`/`custom` kinds declared but unimplemented. (`FakeProvider` exists for tests only.)
- No schema validation on MCP tool arguments. `→ src/mcp/client.rs`
- DeepSeek streaming never reports token usage (`usage` always None).
- Theme hardcoded (Catppuccin Mocha); `ui.theme` ignored. `→ src/tui/theme.rs`
- No persistent RAG / codebase search.
- `wasmtime` 27 WASM PoW solver — native SHA-3 reimplementation planned to drop the dependency.
- `deepseek.rs` still a large multi-responsibility file; split (endpoints/http/session/stream) planned, along with REST-wrapper dedupe.
- `keys.rs` decomposition (key→intent/intent→effect) planned, not started.
- DeepSeek remote-session leak: `delete_remote_session` has zero call sites — sessions accumulate on the DeepSeek account unbounded.
- Foreground child PID tracking is a single global slot, not per-conversation (also an upward `tools`→`app` dependency).
- PoW challenge solved once per request, not re-solved per retry attempt; solve runs on the async task rather than `spawn_blocking`.
- `"model"` field is hardcoded `"deepseek-chat"` in the request body, ignoring user config.
- `tool_parser` truncates visible streamed text at the first bare `<`; legacy `[TOOL:]` regex can't parse nested-brace JSON; fenced code-block tool-call syntax can be executed as real calls during auto-approve turns.
- `mcp__` name separator (`__`) can collide with a server/tool name containing `__`.
- History dedup only catches consecutive duplicates.
- CI clippy is advisory, not blocking, pending further confidence.

## RECENTLY CLOSED (was a gap/bug, now fixed)

- ✅ **2026-07-02 stability/perf overhaul** — all 7 audit criticals (live streaming via spawned `complete_stream`, MCP mutex freeze via lock-free `client_for` handles, GOAL evaluator moved off the event loop, `/goal` registration leading-slash bug, `--acp` nested-runtime panic, render pipeline drain+dirty+per-message markdown/syntect cache, MCP stdio stderr-drain+JSON-RPC id correlation) plus ~30 majors (interaction queue replacing the single-slot approval waiter, `ChatMessage.ui_only` model/display split, unified conversation reducer methods, `util::atomic_write` wired into all persistence, shell timeout+cap+tree-kill, background process-group kill, CI added, dead-code sweep). Full detail: `reference/AUDIT-2026-07-02.md`, `JOURNAL/2026-07-02.md`.
- ✅ **`parent_message_id` "evaporating messages" bug** — interrupted streams desynced the session tree onto an invisible branch. Fixed by incremental persist + flush-on-error in `deepseek.rs` (commit `183712e`) and structurally by per-conversation `fork()` isolation.
- ✅ **God-object `App`** — decomposed into sub-state modules + controllers; `mod.rs` 2.4k → 925 lines.
- ✅ **Single-turn limitation** — replaced by the `Conversation`/`Conversations` model; parallel sessions + sidechats + sub-agents stream concurrently without stream/connection loss.
- ✅ **GOAL mode wedges** — overhauled with safety checks (commit `c0d4280`); pure `apply_verdict` core.
- ✅ **GOAL cycle had no max-iteration cap** — `MAX_GOAL_ITERATIONS = 10` in `src/app/goal.rs`.
- ✅ **Tool-approval modal blocked the event loop / orphaned a second waiter** — replaced with a `PendingInteraction` queue; Esc/Ctrl+C now cancels wedged turns.

## RECENT MILESTONES

| Date | Event |
|------|-------|
| 2026-06-24 | Project inception — core, features, integration built |
| 2026-06-28 | `.memories` system created (`32adf27`); GOAL mode + `/jobs` + `/ps` + PTY jobs (`e801dbe`) |
| 2026-06-30 | `.memories` deeply enriched: added ARCHITECTURE/GLOSSARY/CONVENTIONS + `reference/`; corrected drift (commands 22→25, MCP sources 5→8, agent defaults 25/50→256/10) |
| 2026-06-30 | **Big refactor + features wave**: provider split (prompt/sse/fake `42f6164`/`f783a87`); god-object decomposition (input/mcp_status/generation/goal/shell-unify/view-model/background-split `b252567`…`5205be4`); GOAL overhaul `c0d4280`; `parent_message_id` fix `183712e`; provider `fork()` + conversations `20c90ca`; `/btw` `438e60d`; `/chats` `6c04774`; sub-agents `38ce06f`; goal+multichat extract `92163cb`; **controllers** — conversation mgmt `4efe8cb`, AgentRuntime `c24c7a8`, system_prompt `24e6b00`, background_stats `391c6e4` |
| 2026-07-02 | **Full-codebase audit** (`reference/AUDIT-2026-07-02.md`) → same-day **3-wave refactor**: streaming/MCP/GOAL/ACP/render/stdio criticals + ~30 majors + dead-code sweep, committed as `bad8011`. 54 files, +4056/−1291. Tests 84→189, clippy ~220→0. |

## FACTS CORRECTED THIS PASS (were wrong in older memory)

- Agent defaults: `max_steps=256`, `max_tools_per_step=10` (was "25 / 50").
- Command count: **25** (+2 aliases) (was "22–23").
- MCP config discovery: **8 sources** (was "5").
- Built-in tools: **7 default** + `skill` (background/interactive PTY family is substantial).
