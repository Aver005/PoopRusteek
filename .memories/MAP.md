# MAP
> Codebase map — file → purpose. Navigation aid. (~15k LOC)
> Last updated: 2026-07-02 (in-TUI onboarding, /logout, /wipe — sizes approximate, anchor to names not lines)

## ENTRY / ROOT
| File | Purpose | Lines |
|------|---------|-------|
| `src/main.rs` | CLI (`--acp`, `--debug_log`), init order, launch TUI or ACP | 85 |
| `src/error.rs` | `AppError` enum, `AppResult<T>` | — |
| `src/prompts.rs` | `PromptFiles`, asset resolution for prompts | 67 |
| `src/session.rs` | `Session`, save/load/list, tags, history file | 191 |
| `src/debug_log.rs` | Optional `.dev/debug.log` logger | 68 |

## APP LAYER (Application) — decomposed from the old ~2.4k god-file into cohesive modules
| File | Purpose | Lines |
|------|---------|-------|
| `src/app/mod.rs` | Coordinator: `App` + `AppState`, event loop (`run_loop` — drains ≤256 events then renders once behind a dirty flag), `handle_event`, `send_focused_turn`, `FOREGROUND_CHILD_PID`, `PendingInteraction` queue, onboarding/logout/wipe, `format_size`/`format_duration_secs`/`format_tool_definition`. Re-slimmed 2026-07-04 (1659→~990) by extracting `sessions.rs` + `pickers.rs`. | ~990 |
| `src/app/sessions.rs` | Session lifecycle controller (extracted 2026-07-04): `handle_load_session`, `apply_session_availability`, `finalize_broken_session`, `apply_fetched_session`, `open_delete_sessions`, `execute_delete_sessions`, `auto_save_session` — network work spawned, results via `AppEvent` | ~450 |
| `src/app/pickers.rs` | Registry-inspection UI glue (extracted 2026-07-04): whitelist/skills pickers, `toggle_skill`, `/tools` + `/ps` markdown display builders | ~240 |
| `src/app/conversation.rs` | `ConversationId`, `ConversationKind` (Main/Session/Sidechat/SubAgent), `Conversation` (owns messages/provider/generation/agent_task) with unified reducer methods (`begin_assistant_message`/`append_chunk`/`discard_empty_assistant`/`finish_turn`), `Conversations` store (`focused()/focused_mut()/open()/add_background()/remove()/iter()`…), `Conversation::fresh_main` (shared constructor used by `App::new` and `reset_to_onboarding`) | ~230 |
| `src/app/runtime.rs` | `AgentRuntime` controller — owns `tools`/`mcp`/`event_tx`; `spawn(TurnSpec)` is the one place agent turns launch | ~65 |
| `src/app/system_prompt.rs` | `build(prompts, skills, tools, mcp, workspace)` — system-prompt assembly with explicit narrow deps (was god-method on `&self`) | ~80 |
| `src/app/background_stats.rs` | `BackgroundCounters` (total/interactive/persistent) + refresh/shutdown/kill/prune methods (data clump lifted off `AppState`) | ~100 |
| `src/app/mcp_status.rs` | `McpStatus` (UI view + cached counts) + `update_stats(&mcp)`/`refresh_view(&mcp)` poll methods, now `try_lock`-based with changed-flags instead of blocking the mutex | ~90 |
| `src/app/generation.rs` | `GenerationState` — per-turn streaming/animation/stats status | ~60 |
| `src/app/input.rs` | `InputState` + autocomplete state and input-editing logic | ~350 |
| `src/app/goal.rs` | `GoalState`, `GoalOutcome`, `MAX_GOAL_ITERATIONS`, pure `apply_verdict`, `parse_goal_verdict`, `spawn_goal_evaluation`/`handle_goal_evaluation_done` (event-driven evaluator) + goal `impl App` methods | ~700 (grew: event-driven evaluator replaced the inline blocking call) |
| `src/app/keys/` | Key handling split by input surface (2026-07-05, was one 1318-line file): `mod.rs` (~80, `handle_key` dispatcher + `apply_text_key`), `chat.rs` (~300, chat-view keys + `submit_input`), `dispatch.rs` (~160, `apply_command_result` — the CommandResult→effects interpreter), `modal.rs` (~300, modal keys; pure `approval_key`/`confirm_key` decoders + tests; `Modal` taken/moved, not cloned per keystroke), `mcp.rs` (~365, MCP view/auth/add), `onboarding.rs` (~70), `autocomplete.rs` (~220). Everything except `handle_key` is private to the module. | ~1500 total |
| `src/app/multichat.rs` | `spawn_background_agent`/`spawn_sidechat`/`spawn_sub_agent`/`stop_background`/`handle_background_event`/`new_conversation`/`switch_to`/`cycle_focus`/pickers — kind-based routing so a focused sidechat still finalizes into its parent | ~400 |
| `src/app/events.rs` | `AppEvent` (agent variants tagged with `ConversationId`; `SpawnSubAgent`; `GoalEvaluationDone(GoalEvalOutcome)`; `McpOperationDone`), `Modal` (incl. `Modal::Confirm(ConfirmState)`), `PickerState`, `QuestionState`, `GoalStage`, `PendingInteraction`, `OnboardingState`, `ConfirmAction {Logout, Wipe}`, `ConfirmLine`/`ConfirmLineKind` | ~500 |
| `src/config/mod.rs` | `Config` schema (provider/ui/agent/mcp/skills), paths, load/save (now via `util::atomic_write`) | ~140 |

## PROVIDER (Domain)
| File | Purpose | Lines |
|------|---------|-------|
| `src/provider/mod.rs` | `LLMProvider` trait (incl. `fork() -> Arc<dyn LLMProvider>`), `ChatMessage` (now has `ui_only: bool` + `user_with_display()`), `Role`, request/response types | ~250 |
| `src/provider/deepseek.rs` | DeepSeek web client: auth, ~30 endpoints (most unused ones now parked in a separate `#[allow(dead_code)]` impl block pending a planned split), session state, prompt build, SSE, retry; `fork_session()` (fresh session) + `fork()` impl + incremental `parent_message_id` persist/flush-on-error; reqwest client has `connect_timeout(10s)`/`read_timeout(120s)`, no stray `gzip` header, saturating retry backoff. Dead `parse_sse_event`/`ParsedSSEEvent` typed-SSE path removed. Still a large multi-responsibility file — split (endpoints/http/session/stream) still planned. | ~1600 |
| `src/provider/fake.rs` | `FakeProvider` test double (impls `fork()`) — `#[cfg(test)]` | ~80 |
| `src/provider/prompt.rs` | Prompt/history assembly for the web API (extracted from deepseek.rs) | — |
| `src/provider/sse.rs` | `SseLineBuffer` — now byte-based (not string-slicing, was O(n²)) with a 4MiB cap | — |
| `src/provider/pow.rs` | SHA-3 PoW solver via `wasmtime` (native reimpl planned to drop the wasm dep) | ~245 |
| `src/provider/types.rs` | API/SSE response types (dead `ParsedSSEEvent` family + ~12 other unused structs/consts removed this session) | ~500 |

## AGENT (Domain)
| File | Purpose | Lines |
|------|---------|-------|
| `src/agent/runner.rs` | `run_agent_loop` — multi-step LLM↔tool loop; streaming via shared `stream::collect_stream` (visible deltas through its progress callback), approval, summarize; all events tagged with `ConversationId`; `task` tool special-cased (fg: fork+`run_sub_agent`; bg: emit `SpawnSubAgent`); meta-tool dispatch uses `QUESTION_TOOL_NAME`/`TASK_TOOL_NAME`/`MCP_TOOL_PREFIX` constants, not string literals; `max_tools_per_step` overflow returns explicit "Skipped:" `tool_result`s | ~700 |
| `src/agent/stream.rs` | `collect_stream(provider, request, on_progress) -> StreamOutcome{text, got_stop, end: StreamEnd}` — the once-duplicated spawn-stream + 120s-idle-guard + post-mortem block shared by runner and sub_agent (extracted 2026-07-04). Reports what happened; callers interpret (runner treats Completed-without-stop as error w/ salvage, sub_agent doesn't) | ~90 |
| `src/agent/sub_agent.rs` | `run_sub_agent` — headless isolated agent run (own forked provider, auto-approval), streams via `stream::collect_stream` with a no-op progress callback, returns final text | ~155 |
| `src/agent/tool_parser.rs` | Parse/strip tool calls (XML / `[TOOL:]` / JSON), stream-visible text. Still truncates visible text at the first bare `<` and the legacy regex still can't parse nested-brace JSON (both open, see BUGS.md) | ~175 |

## TOOLS (Domain)
| File | Purpose | Lines |
|------|---------|-------|
| `src/tools/mod.rs` | `Tool` trait, `ToolDefinition`, `ToolResult`, interactive/persistent heuristics | ~85 |
| `src/tools/registry.rs` | `ToolRegistry` — register/resolve/execute, skill injection; tool registration now platform-gated (PowerShell only on Windows) | — |
| `src/tools/shell.rs` | Unified shell tool (replaces the old separate `bash.rs`/`powershell.rs`) — fg/bg/interactive, foreground path now has a 300s timeout + 1MiB output cap + kill_on_drop + tree-kill; owns `FOREGROUND_CHILD_PID` writes | ~570 |
| `src/tools/task.rs` | `task` tool definition — model-invoked sub-agent spawn (fg/bg) | — |
| `src/tools/question.rs` | question tool (special-cased in agent loop) | — |
| `src/tools/background/` | Background + PTY process registry, now a directory: `mod.rs` (re-exports), `registry.rs` (`shutdown_all` and friends), `spawn.rs` (output reader loop — UTF-8-lossy-safe, cp866 prefix fix), `types.rs` (`BackgroundHandle`, async `force_kill_pid` — Unix process-group kill, one-shot overflow marker) | ~900 total |
| `src/tools/shell_control.rs` | `shell_output/kill/list/input` tools; key→escape mapping | ~280 |
| `src/tools/skill.rs` | `skill` tool (list/load) | ~90 |

## MCP (Infrastructure)
| File | Purpose | Lines |
|------|---------|-------|
| `src/mcp/client.rs` | `MCPClient`, JSON-RPC, content flattening. Still no schema validation on tool args (open, see BUGS.md) | ~260 |
| `src/mcp/transport.rs` | Stdio + HTTP + SSE + Dummy transports; stdio now drains stderr continuously and correlates responses by JSON-RPC `id`; `Transport::close()` now has real call sites | ~600 |
| `src/mcp/config.rs` | 8-source config discovery (precedence); `persist_config` now saves only pooprusteek-owned servers (foreign servers get enable/disable overrides, no secret copying) | ~700 |
| `src/mcp/manager.rs` | Server lifecycle, tool caching/TTL, execution; `connect_all` now concurrent (was serial); lock-free `client_for` handles avoid holding the manager mutex across network `.await`s; `shutdown_all` now exists and is called on app exit | ~800 |
| `src/mcp/jsonrpc.rs` | JSON-RPC 2.0 wire types | — |
| `src/mcp/types.rs` | `MCPTool/Resource/ServerConfig`, states | ~110 |

## TUI (Presentation)
| File | Purpose | Lines |
|------|---------|-------|
| `src/tui/mod.rs` | Terminal init/restore | — |
| `src/tui/render/` | Split from the old 2160-line `render.rs` (2026-07-04) into layers: `mod.rs` (~130, `render()` dispatch only), `util.rs` (~215, pure helpers: truncate/format_date/centered_h/status_bar_gap/highlight_json + tests), `popup.rs` (~105, shared modal skeleton: `center_popup`/`modal_block`/`separator_line`/`fill_panel_space`/`push_text_box_lines` — deduped from 4–7 copies each), `status.rs` (~205, mini status bar/separators/attach bar), `landing.rs` (~210, landing + TTL'd session cache), `onboarding.rs` (~220, onboarding + `pulsing_title`), `mcp.rs` (~350, MCP view + `/mcp add` modal), `modals.rs` (~755, tool-approval/confirm/picker/question/delete-sessions/autocomplete) | ~2200 total |
| `src/tui/theme.rs` | Catppuccin Mocha palette (`Theme`) — unused color fields removed | — |
| `src/tui/markdown.rs` | Markdown + syntect highlight renderer; bold/italic/strikethrough now actually apply their style (were previously no-ops) | ~300 |
| `src/tui/widgets/input.rs` | Multi-line input, cursor, selection, wrapping | ~270 |
| `src/tui/widgets/chat.rs` | Chat history widget; now has a per-message thread-local markdown/syntect render cache (fingerprint-keyed, 4096-entry eviction) plus cached token estimates — the core fix for the render-perf critical | ~500 |
| `src/tui/widgets/panel.rs` | Right stats panel; `mcp_row_layout` extracted and made testable, fixing the gap-underflow crash | ~330 |
| `src/tui/widgets/status.rs` | Status bar; display-width gap fixed | ~110 |

## COMMANDS
| File | Purpose |
|------|---------|
| `src/commands/mod.rs` | `Command` trait, `CommandRegistry`, `CommandResult` (`NeedsAgent` dead variant removed; `OpenConfirm` variant added for confirm-modal routing) |
| `src/commands/defs/*.rs` | 30 commands (one per file) — incl. `/btw`, `/new`+`/chats`, `/agent`+`/agents`, `/goal` (leading-slash registration bug fixed), `/logout`, `/wipe` — see `reference/COMMANDS.md`. `/help` is now generated from the live registry instead of a hand-maintained list. |
| `src/commands/defs/logout.rs` | `/logout` — confirm → `cancel_all_turns`, clear `provider.token`, `config::save`, `reset_to_onboarding` ("Logged out") |
| `src/commands/defs/wipe.rs` | `/wipe` — confirm → `cancel_all_turns`, `remove_dir_all` over `wipe_roots()` (deduped config-file parent + data dir; on Linux these differ, on Windows coincide), in-memory `Config::default` + cleared whitelist/history, lands on onboarding; errors go to tracing + status line |

## OTHER
| File | Purpose |
|------|---------|
| `src/acp/server.rs` | ACP JSON-RPC-over-stdio server (`--acp`) — nested-runtime panic fixed with `block_in_place` + `Handle::current()` | ~185 |
| `src/acp/types.rs` | ACP request/response/content types |
| `src/skills/mod.rs` | `SkillDefinition`, `SkillSource`, frontmatter parse — now keeps repeated keys instead of only the first occurrence |
| `src/skills/discovery.rs` | Skill discovery (many dirs), formats; tilde expansion fixed via `util::expand_tilde` | ~280 |
| `src/util.rs` | `atomic_write` (now actually used everywhere), `expand_tilde` (single shared impl), `truncate_at_char_boundary` |
| `src/cli/onboarding.rs` | **DELETED** — replaced by `View::Onboarding` in-TUI flow | — |
| `src/cli/file_mentions.rs` | `@file:line` expansion — line-range clamp fixed (was an out-of-bounds slice panic) | ~120 |
| `assets/prompts/` | base/tools/compact/goal-evaluator + persona & figma prompts |
| `assets/sha3_wasm_bg.*.wasm` | DeepSeek PoW solver blob |
| `.github/workflows/ci.yml` | CI: build+test on Windows and Linux, clippy advisory |
| `CLAUDE.md` | Repo-root bridge that points Claude Code at `.memories/INDEX.md` |
| `.docs/` | Human docs (partly aspirational — trust code/`.memories` over it) |

> Sizes above are **approximate** (rounded, several files grew/shrank this session) — anchor to function/struct names, not line counts, when citing something specific.

## CROSS-REFERENCES
- Provider ↔ Agent: `runner.rs` → `LLMProvider::complete_stream()`, spawned as its own task so the idle guard races live network I/O
- Agent ↔ Tools: `runner.rs` → `ToolRegistry::execute()` / `MCPManager::call_tool()` via lock-free `client_for(name)` handles — the agent/runner never holds the `MCPManager` mutex across the network `.await`
- App ↔ MCP: `app/mod.rs` owns `Arc<Mutex<MCPManager>>`; the lock is held only for short synchronous operations (status polling uses `try_lock`), never across a tool call
- App ↔ TUI: `app/mod.rs` → `render.rs`, but only when `run_loop`'s dirty flag is set (event batching, not every tick)
- App ↔ Agent: every turn launches via `AgentRuntime::spawn(TurnSpec)` (`app/runtime.rs`); spawned task ↔ main loop via `AppEvent` channels (tagged with `ConversationId`) + `Notify` handshakes
- Conversations: `App.state.conversations: Conversations` (`app/conversation.rs`) — one focused, others background; `agent_event_target()` routes non-focused agent events to `handle_background_event` (`app/multichat.rs`), which shares the same unified reducer methods as the focused path (kind-based, not focus-based, so sidechats finalize correctly)
- Each `Conversation` owns its **own forked provider** (`LLMProvider::fork()`) → isolated DeepSeek session, no `parent_message_id` cross-talk
