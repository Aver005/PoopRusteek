# BUGS
> Known defects, sorted by impact. Update on discovery/fix.
> Last updated: 2026-07-04 (fixed silent global-session-per-restart bug; see RESOLVED)
> Full audit digest: `reference/AUDIT-2026-07-02.md` (2026-07-02)

## CRITICAL
None currently known.

## HIGH
None currently known. (The infinite-retry-hang entry that used to live here was fixed this session — see RESOLVED.)

## MEDIUM

- `[BUG]` MCP tool arguments are passed to `tools/call` with **no schema validation**. `→ src/mcp/client.rs`
- `[BUG]` `stream_visible_text` truncates at the first bare `<` → legitimate text containing `<` (C++ templates, `a < b`, HTML) is hidden mid-stream. `→ src/agent/tool_parser.rs`
- `[BUG]` PoW challenge is solved once before the retry loop (not re-solved per attempt) and the solve runs on the async task instead of `spawn_blocking`, competing with the executor. `→ src/provider/pow.rs` + `src/provider/deepseek.rs`
- `[BUG]` Legacy `[TOOL:name] {json}` regex uses a non-nesting brace pattern and can't parse nested JSON objects. `→ src/agent/tool_parser.rs`
- `[BUG]` Fenced code-block examples containing tool-call syntax can be parsed and executed as real tool calls during auto-approve (background) turns. `→ src/agent/tool_parser.rs`

## LOW

- `[BUG]` `/import` overwrites the session tag with `"Imported"`, losing any original tag/metadata. `→ src/commands/defs/import.rs`
- `[BUG]` Autocomplete file paths resolve against `current_dir()`, which can drift from `workspace_path` after a subprocess `cd`. `→ src/app/keys.rs` (autocomplete)
- `[BUG]` Theme is hardcoded Catppuccin Mocha; `ui.theme` config is ignored. `→ src/tui/theme.rs`
- `[BUG]` MCP image content is dropped to `[Image: {mime}]` text; non-text/image/resource content types ignored. `→ src/mcp/client.rs`
- `[BUG]` `mcp__` uses `__` as separator → name collision possible if a server/tool name contains `__`. `→ src/mcp/manager.rs`
- `[BUG]` History deduplication only catches **consecutive** duplicates. `→ src/session.rs`
- `[BUG]` `"model"` field in the outgoing request body is a hardcoded literal `"deepseek-chat"`, ignoring the user's configured model. `→ src/provider/deepseek.rs`
- `[BUG]` Foreground child PID tracking is a single global slot (`AtomicU32`), not per-conversation — Esc/Ctrl+C in one conversation can kill a different conversation's foreground process; also an upward `tools`→`app` dependency. `→ src/app/mod.rs` (`FOREGROUND_CHILD_PID`) + `src/tools/shell.rs`

## WONTFIX / ACCEPTED

- `[?]` PoW runs DeepSeek's own wasm blob via `wasmtime` (heavy dep, repo blob). Native SHA-3 reimpl REJECTED by owner (2026-07): the workaround must execute upstream's solver as-is. The wasm is now embedded via `include_bytes!` (2026-07-03; a file in `assets/` still overrides for dev drop-ins). Remaining stretch: fetch the server-referenced wasm at runtime.
- `[?]` DeepSeek web API is reverse-engineered; may break on any server update. No SLA.
- `[?]` `bash`/`powershell` run arbitrary commands with no sandbox — by design; trust = tool-approval + `/whitelist`.

## RESOLVED / MOOT

- ✅ `~~Loading an existing local session (/sessions → /load) and sending a message silently created a brand-new DeepSeek remote session, even though the old one was still live~~` — root cause: the DeepSeek-side `SessionState` (`session_id`/`parent_message_id`) was never persisted anywhere, and `handle_load_session` unconditionally called `provider.reset()` on every local-session load, discarding any in-memory identity too. So *any* app restart, or any `/load`, always started a fresh remote `chat_session` with no way to tell. Fixed 2026-07-04: `Session` now persists `provider_session_id`/`provider_parent_message_id`/`broken`; `LLMProvider` gained `session_identity`/`session_is_alive`/`adopt_session`; `/load` checks aliveness off-loop and either resumes the same remote thread (`adopt_session`) or marks the session `broken` (yellow ⚠ in `/sessions`, cleared again once a fresh remote link is confirmed) and replays full local history as one prompt on the next message. Side-fix: `auto_save_session` previously wiped a session's `tag` back to `None` on every turn after a `/load`/`/import` (only ever wrote `tag: None` regardless of the file's actual tag) — now mirrored on `Conversation` and round-tripped correctly. Full flow in `reference/CONFIG.md` → "Remote session resume". `→ src/session.rs`, `src/provider/mod.rs`, `src/provider/deepseek/mod.rs`, `src/app/mod.rs`
- ✅ `~~PowerShell commands with genuinely empty output (e.g. a filter matching nothing) were reported to the agent as "[powershell exited successfully but returned no stdout/stderr; output capture likely failed]" — a ToolResult::error~~` — the 2026-07-03 evening pass added this as a speculative capture-failure guard, but exit-0 + empty stdout/stderr is a normal, common outcome (no matches, silent success like `Set-Content`), not evidence of a bug. The agent read the `is_error` flag and treated a healthy zero-result command as broken. Fixed 2026-07-04: same branch now returns `ToolResult::success("(command completed successfully with no output)")`; the `shell.foreground.empty_success` debug-log event is unchanged for diagnostics if a real capture bug ever surfaces. `→ src/tools/shell.rs`
- ✅ `~~Every turn ends with "stream ended early…" warning; final answers abort as AgentError~~` — the strict-stop pass wrongly assumed DeepSeek ends its SSE with `data: [DONE]`; the live protocol just closes the connection (0 `[DONE]` in a full session log). Clean EOF is now a normal stop in both `complete`/`complete_stream`; explicit finish signals (`[DONE]` ±space, `FINISHED` status patch, terminal BATCH) recognized in `process_stream_line`; unrecognized SSE lines now logged (`completion.*.skipped`). Fixed 2026-07-03 evening — see `JOURNAL/2026-07-03.md`. Watch item: one zero-content stream (metadata only, instant close) remains unexplained; skipped-line logging will identify it if it recurs.

- ✅ **2026-07-03 follow-ups** — ephemeral remote-session leak fixed: `LLMProvider::discard_remote_session` wired into sidechat/sub-agent finalize, stop, foreground `task` forks, and (bounded, 3s) app exit; user-facing `/delete [id]` + `/delete-local [id]` added (shared multi-select picker, All/Local/Remote filter = deletion scope, confirm step, background remote fetch/delete via `RemoteSessionsListed`/`SessionsDeleted` events); `deepseek.rs` split into `deepseek/{mod,http,session,stream,endpoints}.rs`; deps bumped (reqwest 0.13, ratatui 0.30, crossterm 0.29, pulldown-cmark 0.13, toml 1.x); PoW wasm embedded via `include_bytes!`.

- ✅ **2026-07-02 refactor session** — see `reference/AUDIT-2026-07-02.md` for full detail; `JOURNAL/2026-07-02.md` for the session narrative. One line per subsystem:
  - **Streaming**: `complete_stream` now spawned as a task so the 120s idle guard races the live network call; reqwest client got `connect_timeout(10s)`/`read_timeout(120s)`; stray `gzip` header removed; `SseLineBuffer` rewritten byte-based with a 4MiB cap (was unbounded + O(n²)).
  - **MCP**: lock-free `client_for` handles replace holding `MCPManager`'s mutex across network `.await`s; stdio stderr now drained; JSON-RPC `id` correlation replaces order-assumed matching; `Transport::close()` now has real call sites (toggle/remove/reload/app-exit `shutdown_all`); `connect_all` now concurrent; honest connection-failure status; `persist_config` no longer copies foreign servers' secrets.
  - **GOAL**: evaluator moved off the event loop via `spawn_goal_evaluation` → `GoalEvaluationDone`; dead duplicate inline path and unused `GoalCycleFinished` variant removed; stale-verdict guard added.
  - **Commands/ACP**: `/goal` leading-slash registration bug fixed (+ round-trip test); `--acp` nested-runtime panic fixed with `block_in_place`.
  - **Render**: `run_loop` drains ≤256 events then renders once behind a dirty flag (idle = zero draws); per-message markdown/syntect cache in `tui/widgets/chat.rs`; token-estimate caching.
  - **Interaction/approval**: single-slot approval waiter replaced by a `PendingInteraction` queue; Esc/Ctrl+C now cancels wedged turns; `whitelist::persist_approval` makes "always allow" survive restarts.
  - **Conversation model**: `ChatMessage.ui_only` splits model-visible history from UI chrome; unified reducer methods (`begin_assistant_message`/`append_chunk`/`discard_empty_assistant`/`finish_turn`) replace the diverged focused/background duplicate logic; `send_to_agent` → `send_focused_turn(Option<ChatMessage>)` whose argument now actually reaches the model.
  - **Shell/background**: foreground shell got a 300s timeout + 1MiB cap + kill_on_drop + tree-kill; PowerShell UTF-8 prefix fixes cp866 mojibake; background readers survive non-UTF8; Unix process-group kill; async `force_kill_pid`; overflow flag no longer sticky.
  - **Persistence**: `util::atomic_write` wired into config/session/whitelist/mcp-config saves (was unused, zero call sites); session version checked on load; config file 0600 on Unix.
  - **Misc**: `/help` generated from the command registry; `@file` mention range clamp; skills frontmatter keeps repeated keys; markdown bold/italic/strikethrough now actually styled; MCP panel gap underflow fixed; scroll row-count uses ratatui's real line-count; stats-panel/landing-session disk reads cached; retry backoff made `saturating`.
  - **Dead code removed**: `parse_sse_event`/`ParsedSSEEvent` family, ~12 dead structs/consts in `provider/types.rs`, `AppState.error`, `GoalCycleFinished`, `CommandResult::NeedsAgent`, unused theme fields, `BackgroundHandle.id`, `SpawnOutcome.status/interactive`, `API_BASE`.
  - **Result**: clippy ~220→0 warnings, tests 84→189, CI added (`.github/workflows/ci.yml`).
- ✅ `~~Messages "evaporate" — appear in the TUI but never reach DeepSeek / the web view~~` — `parent_message_id` desync forked the session onto an invisible branch (triggered by interrupted/errored streams). Fixed by incremental persist + flush-on-error (`deepseek.rs`, commit `183712e`); structurally prevented by per-conversation `fork()` isolation.
- ✅ `~~GOAL pipeline wedges on empty prompt/goal, interrupt+new message~~` — overhaul `c0d4280`.
- ✅ `~~GOAL cycle has no max-iteration cap~~` — was fixed by the goal overhaul; BUGS.md had gone stale. `MAX_GOAL_ITERATIONS = 10` (`src/app/goal.rs`); `apply_verdict` gives up at the cap (test `gives_up_at_iteration_cap`).
- `~~No .gitignore entry for .memories/JOURNAL/~~` — JOURNAL is intentionally tracked in git.
