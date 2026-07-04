# STATE
> Live project snapshot. Update on every meaningful change.
> Last updated: 2026-07-04 (cleanup audit `reference/AUDIT-2026-07-04-CLEANUP.md` + god-file split: `tui/render.rs` 2160→8-module `tui/render/` with shared popup kit; `app/mod.rs` 1659→~990 via new `app/sessions.rs`+`app/pickers.rs`; shared `agent/stream.rs::collect_stream` replacing the runner/sub_agent copy-paste + meta-tool name constants — tests 262, clippy 0. Earlier same day: `/mcp add`, MCP OAuth, remote session resume)

## PHASE COMPLETION

| Phase | Status | What |
|-------|--------|------|
| 1 Core | `[DONE]` | TUI, provider trait, DeepSeek client, agent loop, tools, MCP types, PoW, streaming |
| 2 Features | `[DONE]` | In-TUI onboarding (`View::Onboarding`), sessions, 27 slash commands, markdown+syntect, compaction, @file, tool approval, input history |
| 3 Integration | `[DONE]` | MCP stdio/HTTP/SSE, 8-source auto-discovery, manager+caching, JSON-RPC, ACP server mode, OAuth authorization (`/mcp auth`/`/mcp oauth`, OS-keyring token storage), `/mcp add` (paste-JSON/wizard/quick-inline server add — 2026-07-04) |
| 3.5 Agentic | `[DONE]` | GOAL mode (2-agent iterative loop), background/interactive PTY jobs, `/jobs` `/ps`, skills system |
| 3.6 Multi-chat | `[DONE]` | `Conversation`/`Conversations` model, provider `fork()`, event tagging by `ConversationId`, parallel sessions (`/new` `/chats` + Tab), `/btw` sidechat, sub-agents (model `task` tool + `/agent`/`/agents`, fg+bg) |
| 3.7 Architecture | `[DONE]` | God-object `App` decomposed (mod.rs 2.4k→925): sub-state modules + controllers (`AgentRuntime`, `system_prompt::build`, `BackgroundCounters`, `McpStatus`). Provider split (prompt/sse/fake). |
| 3.8 Stability & perf overhaul | `[DONE]` | 2026-07-02: all 7 audit criticals fixed (live streaming, MCP mutex freeze, GOAL evaluator off-loop, `/goal` registration, `--acp` panic, drain+dirty render with markdown/syntect cache, MCP stdio stderr+id-correlation) + ~30 majors (interaction queue, ui_only message split, atomic_write everywhere, CI, shell timeout/cap, background process-group kill, etc.) + dead-code sweep; clippy ~220→0, tests 84→189. See `reference/AUDIT-2026-07-02.md` + `JOURNAL/2026-07-02.md`. |
| 4 Polish | `[WIP]` | Multi-theme, mouse, copy/paste, error recovery, rate limiting `[DONE]` (ms-interval + per-minute cap, both via `/rate`; retry/backoff exists), schema validation |
| 5 Distribution | `[WIP]` | CI added (`.github/workflows/ci.yml`, build+test win+linux, clippy advisory); release builds, cross-compile, installers, man page still `[TODO]` |

## BUILD STATUS

| Check | Status |
|-------|--------|
| `cargo build` | Passes |
| `cargo clippy` | 0 warnings (was ~220 before the 2026-07-02 session) |
| Tests | **287 passing** (`cargo test --bin pooprusteek`) |
| CI | `.github/workflows/ci.yml` — build+test on Windows and Linux; clippy runs advisory (`continue-on-error`) |

## CURRENT FOCUS

0. Cleanup audit 2026-07-04 (`reference/AUDIT-2026-07-04-CLEANUP.md`): god-file split DONE (`tui/render/` layered modules + `app/sessions.rs`/`app/pickers.rs`); runner/sub_agent shared stream helper DONE (`agent/stream.rs::collect_stream` + `QUESTION_TOOL_NAME`/`TASK_TOOL_NAME`/`MCP_TOOL_PREFIX` constants); reliability trio DONE (poison-safe rate-limit lock, PoW on `spawn_blocking`, 8 MiB stream cap w/ tests); mechanical dedup DONE (`with_args`/`save_config_then`/`clear_chat_view`/`push_system` adoption in commands, `post_void`+`pow_auth_headers` endpoint migration, MCP `load_from_path`/`merge_parsed`, `apply_connect_outcome` hoist, `list_sessions()` dead param dropped). Remaining: owner decisions on `#[expect(dead_code)]` scaffolding (events.rs payloads, types.rs parity structs, `ProviderKind::Openai/Custom`).
1. Stability/perf overhaul (2026-07-02) is done — all 7 audit criticals + ~30 majors fixed, dead code swept, clippy clean. See `reference/AUDIT-2026-07-02.md`.
2. Next candidates (in rough priority order):
   - ~~keys.rs decomposition~~ DONE 2026-07-05: `app/keys/` — mod (dispatch) / chat / modal / dispatch (CommandResult interpreter) / mcp / onboarding / autocomplete; pure `approval_key`/`confirm_key` decoders w/ tests; per-keystroke `Modal` clone removed.
   - ~~deepseek.rs split~~ DONE 2026-07-03: `provider/deepseek/{mod,http,session,stream,endpoints}.rs`, 8/26 wrappers deduped onto `post_biz`/`get_biz`.
   - ~~Dependency bumps~~ DONE 2026-07-03: reqwest 0.13 (`rustls-tls`→`rustls` feature), ratatui 0.30 + crossterm 0.29, pulldown-cmark 0.13, toml 1.x — zero source changes forced.
   - ~~PoW wasm embed~~ DONE 2026-07-03: `include_bytes!` with a disk-file override for dev drop-ins. Native SHA-3 reimpl remains REJECTED by owner; stretch: fetch the server-referenced wasm at runtime (real rotation-resilience).
3. Phase-4 polish (multi-theme on hold; mouse, copy/paste, error recovery).
4. MCP tool-arg schema validation (still absent).

## KNOWN GAPS

- **`.memories/` is not auto-loaded** by the agent (verified: nothing in `src/` reads it). The "Integrate memories" commit only created the docs; `CLAUDE.md` at the repo root now bridges into it for Claude Code specifically.
- ~~Single provider~~ FIXED 2026-07-05: `/providers` manages OpenAI-compatible endpoints (LM Studio, Ollama `/v1`, vLLM, …) — `OpenAiCompatProvider` (`provider/openai_client.rs`) + `provider::build_provider` factory + `Config.providers`/`active_provider`. The old `ProviderKind::Openai/Custom` enum variants are now purely decorative (selection goes through `/providers` entries, not `provider.kind`) — candidates for removal.
- No schema validation on MCP tool arguments. `→ src/mcp/client.rs`
- DeepSeek streaming never reports token usage (`usage` always None).
- Theme hardcoded (Catppuccin Mocha); `ui.theme` ignored. `→ src/tui/theme.rs`
- No persistent RAG / codebase search.
- Foreground child PID tracking is a single global slot, not per-conversation (also an upward `tools`→`app` dependency).
- PoW challenge solved once per request, not re-solved per retry attempt (deliberately left as-is 2026-07-04 — changing it changes the request pattern against DeepSeek). The solve itself now runs on `spawn_blocking` (fixed 2026-07-04).
- `"model"` field is hardcoded `"deepseek-chat"` in the request body, ignoring user config.
- `tool_parser` truncates visible streamed text at the first bare `<`; legacy `[TOOL:]` regex can't parse nested-brace JSON; fenced code-block tool-call syntax can be executed as real calls during auto-approve turns.
- `mcp__` name separator (`__`) can collide with a server/tool name containing `__`.
- MCP OAuth (`/mcp auth`) only supports authorization servers with RFC 7591 dynamic client registration; `keyring` needs a working OS credential backend (fails silently to "never authorized" on headless Linux with no Secret Service). `→ reference/MCP.md` AUTHORIZATION GOTCHAS.
- DeepSeek token in `config.toml` is still plaintext (chmod 0600 Unix-only, unprotected on Windows) — MCP OAuth tokens are the first encrypted-at-rest secrets in the repo, but this pass didn't retrofit the DeepSeek token onto the same keyring-backed storage.
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
| 2026-07-02 | **In-TUI onboarding + /logout + /wipe**: `cli/onboarding.rs` deleted; `View::Onboarding` full-screen rework (`OnboardingState`, `handle_onboarding_key`, `render_onboarding`, `Conversation::fresh_main`); generic `Modal::Confirm(ConfirmState)` + `ConfirmAction`; `/logout` (confirm → cancel turns → clear token → `reset_to_onboarding`) and `/wipe` (confirm → cancel turns → `remove_dir_all` over `wipe_roots()` → factory reset → onboarding). Tests 189→209, clippy 0. |
| 2026-07-04 | **MCP OAuth authorization**: `/mcp auth`/`/mcp oauth` list+authorize servers in `AuthRequired` status; full RFC 9728/8414/7591 + PKCE flow (`mcp/oauth.rs`), OS-keyring encrypted token storage (`mcp/oauth_store.rs`, `keyring` crate) — first encrypted-at-rest secrets in the repo. `build_client` unified across `add_server`/`toggle_server`/`reconnect_server`; HTTP/SSE 401 detection deduped into shared transport helpers. Tests 227→242, clippy 0. See `reference/MCP.md` AUTHORIZATION. |
| 2026-07-04 | **`/mcp add`**: paste-JSON, step-by-step wizard, and quick inline (`<name> <command> [args...]` or inline JSON, falling back to the same choice modal on parse failure) — all converge on new `MCPManager::add_new_server`. New `app/mcp_add.rs` (pure state machine + parsers, no crossterm dep), new `Modal::McpAdd`, `app::input::InputState` gained `Clone` (reused for every text-entry step). Tests 242→262, clippy 0. See `reference/MCP.md` ADDING SERVERS. |

## FACTS CORRECTED THIS PASS (were wrong in older memory)

- Agent defaults: `max_steps=256`, `max_tools_per_step=10` (was "25 / 50").
- Command count: **25** (+2 aliases) (was "22–23").
- MCP config discovery: **8 sources** (was "5").
- Built-in tools: **7 default** + `skill` (background/interactive PTY family is substantial).
