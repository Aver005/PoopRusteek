# STATE
> Live project snapshot. Update on every meaningful change.
> Last updated: 2026-07-06, session 2 continued (**startup input-lag fix + first-token latency**: ONNX intra-op threads capped to `(cores/2).clamp(1,4)` in `semantic/embedder.rs` — the post-launch indexing burst no longer saturates every core and starves the TUI (user-reported hard input lag; render exonerated at ~1.4ms/frame by the new `render_chat_perf_probe`); `match_prompt` now bounded-waits 150ms on the semantic index lock and skips the hint during indexing holds (`lock_inner_bounded`), `SemanticService::status()` returns `Option` via try_lock so `/rag` can't freeze the event loop mid-rebuild — tests 365, clippy 0.) Earlier: (**BLAZE perf pass** — 4 stages, zero behavior change: (1) MCP init backgrounded via lock-phase-safe `manager::startup_initialize` + concurrent client builds — first frame no longer waits up to ~60s on a dead server, new quiet `AppEvent::McpInitialized`, `MCPManager::initialize` deleted; (2) streaming de-O(n²)'d — `StreamTextTracker` (incremental, byte-identical to the old pipeline, prefix-equivalence-tested) replaces per-chunk full-text regex in `run_agent_loop`, plus per-code-block syntect memoization (`HIGHLIGHT_CACHE`) in `tui/markdown.rs`; (3) `render_chat` two-pass: cached per-message row counts (`ROWS_CACHE`, `CachedMsg.meta_rows`) + viewport culling — typing/scroll latency flat on long transcripts, u16 scroll overflow >65k rows incidentally fixed; (4) persistence off the event loop — `app/persist.rs` FIFO worker for session autosave + history writes, `flush(3s)` on exit and before `/wipe`, `session::append_history` split into pure `push_history_entry` + `write_history`. Tests 358→364, clippy 0. See `JOURNAL/2026-07-06.md` session 2.) Earlier today: (**API server mode shipped** — `src/server/` {mod,catalog,http,openai}: hyper-1 HTTP listener behind `--serve`/`--server`/`--api` flags + `/serve [on|off|api <d>]` + `/server <port>` (persisted `[server]` config: host/port/api/api_key, default port 7667); OpenAI Chat Completions dialect over EVERY configured provider — built-in DeepSeek (fork-per-request + `discard_remote_session`, no junk chats) and all `/providers` entries via `<entry>/<model>` ids (sub-model override supported); SSE streaming re-framed under one completion id + `data: [DONE]`; optional bearer auth, CORS, `/health`; `ServerApi` enum reserves anthropic/gemini dialect seams (501 until inbound conversions land); lifecycle via generation-tagged `AppEvent::ServerStarted/Failed/Stopped`; E2E socket tests incl. a mock-upstream gateway round-trip — tests 358, clippy 0). Before: 2026-07-05 (multi-theme system: `/themes` gallery with whole-frame live preview + step-by-step custom-theme wizard, 10 presets in `tui/theme.rs::PRESETS`, `[[ui.custom_themes]]` config — tests 345, clippy 0. Earlier same day: semantic matching `src/semantic/`, stages 1–3 — local e5-small + stemmed TF-IDF + RRF over skills, MCP tools AND persistent message history; deferred MCP schemas + `tool_search`/`history_search` builtins; `/rag` control + `/search`; tests 320, clippy 0). Before that: 2026-07-04 (cleanup audit `reference/AUDIT-2026-07-04-CLEANUP.md` + god-file split: `tui/render.rs` 2160→8-module `tui/render/` with shared popup kit; `app/mod.rs` 1659→~990 via new `app/sessions.rs`+`app/pickers.rs`; shared `agent/stream.rs::collect_stream` replacing the runner/sub_agent copy-paste + meta-tool name constants — tests 262, clippy 0. Earlier same day: `/mcp add`, MCP OAuth, remote session resume)

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
| 4 Polish | `[WIP]` | Multi-theme `[DONE]` (2026-07-05: `/themes` — 10 presets, live preview, custom-theme wizard, `[[ui.custom_themes]]`), mouse, copy/paste, error recovery, rate limiting `[DONE]` (ms-interval + per-minute cap, both via `/rate`; retry/backoff exists), schema validation |
| 5 Distribution | `[WIP]` | CI added (`.github/workflows/ci.yml`, build+test win+linux, clippy advisory); release builds, cross-compile, installers, man page still `[TODO]` |

## BUILD STATUS

| Check | Status |
|-------|--------|
| `cargo build` | Passes |
| `cargo clippy` | 0 warnings (was ~220 before the 2026-07-02 session) |
| Tests | **358 passing** + 3 `#[ignore]`d (semantic evals, need the ~120 MB model) (`cargo test --bin pooprusteek`) |
| CI | `.github/workflows/ci.yml` — build+test on Windows and Linux; clippy runs advisory (`continue-on-error`) |

## CURRENT FOCUS

0. Cleanup audit 2026-07-04 (`reference/AUDIT-2026-07-04-CLEANUP.md`): god-file split DONE (`tui/render/` layered modules + `app/sessions.rs`/`app/pickers.rs`); runner/sub_agent shared stream helper DONE (`agent/stream.rs::collect_stream` + `QUESTION_TOOL_NAME`/`TASK_TOOL_NAME`/`MCP_TOOL_PREFIX` constants); reliability trio DONE (poison-safe rate-limit lock, PoW on `spawn_blocking`, 8 MiB stream cap w/ tests); mechanical dedup DONE (`with_args`/`save_config_then`/`clear_chat_view`/`push_system` adoption in commands, `post_void`+`pow_auth_headers` endpoint migration, MCP `load_from_path`/`merge_parsed`, `apply_connect_outcome` hoist, `list_sessions()` dead param dropped). Remaining: owner decisions on `#[expect(dead_code)]` scaffolding (events.rs payloads, types.rs parity structs, `ProviderKind::Openai/Custom`).
1. Stability/perf overhaul (2026-07-02) is done — all 7 audit criticals + ~30 majors fixed, dead code swept, clippy clean. See `reference/AUDIT-2026-07-02.md`.
2. Next candidates (in rough priority order):
   - ~~keys.rs decomposition~~ DONE 2026-07-05: `app/keys/` — mod (dispatch) / chat / modal / dispatch (CommandResult interpreter) / mcp / onboarding / autocomplete; pure `approval_key`/`confirm_key` decoders w/ tests; per-keystroke `Modal` clone removed.
   - ~~deepseek.rs split~~ DONE 2026-07-03: `provider/deepseek/{mod,http,session,stream,endpoints}.rs`, 8/26 wrappers deduped onto `post_biz`/`get_biz`.
   - ~~Dependency bumps~~ DONE 2026-07-03: reqwest 0.13 (`rustls-tls`→`rustls` feature), ratatui 0.30 + crossterm 0.29, pulldown-cmark 0.13, toml 1.x — zero source changes forced.
   - ~~PoW wasm embed~~ DONE 2026-07-03: `include_bytes!` with a disk-file override for dev drop-ins. Native SHA-3 reimpl remains REJECTED by owner; stretch: fetch the server-referenced wasm at runtime (real rotation-resilience).
3. Phase-4 polish (~~multi-theme~~ DONE 2026-07-05 — `/themes`; mouse, copy/paste, error recovery remain).
4. MCP tool-arg schema validation (still absent).

## KNOWN GAPS

- **`.memories/` is not auto-loaded** by the agent (verified: nothing in `src/` reads it). The "Integrate memories" commit only created the docs; `CLAUDE.md` at the repo root now bridges into it for Claude Code specifically.
- ~~Single provider~~ FIXED 2026-07-05: `/providers` manages OpenAI-compatible endpoints (LM Studio, Ollama `/v1`, vLLM, …) — `OpenAiCompatProvider` (`provider/openai_client.rs`) + `provider::build_provider` factory + `Config.providers`/`active_provider`. The old `ProviderKind::Openai/Custom` enum variants are now purely decorative (selection goes through `/providers` entries, not `provider.kind`) — candidates for removal.
- No schema validation on MCP tool arguments. `→ src/mcp/client.rs`
- DeepSeek streaming never reports token usage (`usage` always None).
- `/models` switching: model id is validated against `LLMProvider::list_models()` (GET /models for compat providers, fixed pair for DeepSeek).
- ~~Theme hardcoded; `ui.theme` ignored~~ FIXED 2026-07-05: `Theme::resolve(&config.ui)` in `render()` honors `ui.theme` — 10 presets (`tui/theme.rs::PRESETS`) + `[[ui.custom_themes]]` (base preset + per-role hex overrides), managed via `/themes` (gallery w/ live preview) and its create/edit wizard.
- ~~No persistent RAG~~ Stages 1–3 landed 2026-07-05: `src/semantic/` matches prompts against the *skill catalog* and *MCP tool catalog* locally (e5-small ONNX + stemmed TF-IDF + RRF), defers MCP schemas + `tool_search`, and now indexes *message history* persistently (`data_dir/semantic/history.json`, backfilled from session files) behind `/search` + the `history_search` tool. Still open: **codebase search** (files aren't indexed) and the far-goal **RAG context refill** (retrieve past messages instead of truncating when the window overflows) — see `JOURNAL/2026-07-05.md`.
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
| 2026-07-05 | **Semantic matching, stages 1+2** (`src/semantic/`): local hybrid RAG-lite — fastembed `MultilingualE5Small` (one-time ~120 MB download into `data_dir/models`, offline after) + Snowball-stemmed TF-IDF + RRF (`HybridIndex`) over TWO corpora: skills and MCP tools. Every turn gets an ephemeral advisory hint (skills → `skill` tool; MCP tools inline full defs in deferred mode). **Deferred MCP schemas**: `[semantic] mcp_schemas = auto\|full\|deferred` (auto defers >12 tools) turns the system-prompt MCP section into name+one-liner rows; full definitions come from per-turn hints or the new **`tool_search`** builtin (semantic search with lexical fallback — never bricks). MCP corpus re-embeds on server changes via `McpOperationDone`. Evals: skills MRR 0.927, MCP 0.836. |
| 2026-07-04 | **`/mcp add`**: paste-JSON, step-by-step wizard, and quick inline (`<name> <command> [args...]` or inline JSON, falling back to the same choice modal on parse failure) — all converge on new `MCPManager::add_new_server`. New `app/mcp_add.rs` (pure state machine + parsers, no crossterm dep), new `Modal::McpAdd`, `app::input::InputState` gained `Clone` (reused for every text-entry step). Tests 242→262, clippy 0. See `reference/MCP.md` ADDING SERVERS. |

## FACTS CORRECTED THIS PASS (were wrong in older memory)

- Agent defaults: `max_steps=256`, `max_tools_per_step=10` (was "25 / 50").
- Command count: **25** (+2 aliases) (was "22–23").
- MCP config discovery: **8 sources** (was "5").
- Built-in tools: **7 default** + `skill` (background/interactive PTY family is substantial).
