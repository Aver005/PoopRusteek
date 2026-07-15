# AUDIT — 2026-07-15 (quality / structure pass)

> Scope: bad practices, dead code, excessive branching, duplication, overloaded
> modules. Focus on code added since the 2026-07-04 cleanup pass (`server/`,
> `semantic/`, the three compat providers + clients, themes, `model_cache`,
> `update`, paste/mouse/search) plus regrowth of previously-split files.
> Not a defect hunt (that's `AUDIT-2026-07-02.md`), but several real defects
> surfaced and are cross-filed in `BUGS.md`.
> Method: 7 parallel subsystem sweeps + lead verification of all top findings +
> mechanical metrics (clippy `cognitive_complexity`/`too_many_lines`, LOC,
> repo-wide greps for invariant violations).
> Status legend: `[VERIFIED]` = lead read the cited code; `[REPORTED]` =
> subsystem sweep finding, spot-check before acting.
> Anchors are function/struct names; line numbers are as-of-today and drift.
> Baseline at audit time: clippy 0 warnings, 434 tests, fmt clean, 42.2k LOC
> total (incl. inline tests) across 166 files.

## METRICS SNAPSHOT (mechanical, clippy-confirmed)

- **36 functions >100 lines**; worst: `run_agent_loop` 513 (runner.rs),
  `handle_event` 338 (app/mod.rs), `ShellTool::execute` ~294 (grown past
  clippy's own 253 count — measured non-blank body), `apply_command_result` 235
  (keys/dispatch.rs), `render_question` ~218 (render/modals.rs).
- **6 functions cognitive complexity >25**: `run_agent_loop` 42/25,
  `handle_event` 30, `handle_modal_key` 28, `apply_command_result` 26,
  `run_loop` 26, `handle_key` (keys/dispatch) 26.
- `.lock().unwrap()` ≈ 50 non-test sites (semantic/mod.rs 13, tools/background
  ~15, mcp/transport 5) — see poison policy below. `let _ =` 158 sites.
- `TODO`/`FIXME`/`HACK`: **0**. `#[allow(dead_code)]`: 4, all known/documented
  except `widgets/input.rs::cursor_pos` (see DEAD CODE).

## INVARIANT VIOLATIONS (CLAUDE.md №1–11)

1. `[FIXED 2026-07-15]` **#1+#2, the one real event-loop freeze**: `keys/mcp.rs`
   `handle_mcp_key` `'d'` arm runs `self.mcp.lock().await` +
   `remove_server(&name).await` (process close + config write) **inline in the
   key handler**, while siblings toggle/reconnect/add in the same file
   correctly `tokio::spawn` + report via `McpOperationDone`. Root cause (per
   mcp sweep): `MCPManager`'s mutating methods are `&mut self` async fns with
   I/O inside — the caller decides whether the antipattern happens. Fix the
   'd' arm now; longer-term give mutations a snapshot→unlock→I/O→relock shape.
2. `[VERIFIED]` **#2 by the letter (not on the event loop)**:
   `app/mod.rs` `McpOAuthResult` arm — `mcp.lock().await.reconnect_server(...)
   .await` inside a spawned task holds the manager lock for the whole
   reconnect handshake (other lockers queue); `run()` exit path
   `self.mcp.lock().await.shutdown_all().await` is unbounded while its
   neighboring cleanup steps are deliberately time-bounded (persister flush
   3s). Wrap shutdown in a timeout; move reconnect I/O outside the lock.
3. `[FIXED 2026-07-15 — poison-tolerant `lock_inner()`; the embed-under-lock hold stays, by design]` **#11 (semantic must degrade, never crash) — poison gap**:
   `semantic/mod.rs` embeds **under** the `inner` lock in `rebuild_mcp_corpus`
   / `backfill_history` / `index_session` (while `spawn_init` correctly embeds
   *outside* the lock), and 9+ methods use bare `.lock().unwrap()` while
   `match_prompt`/`status` deliberately treat `Poisoned` as "busy, degrade"
   (`lock_inner_bounded`). One ONNX panic under the lock → every later call
   panics the caller instead of degrading. Same class as the
   `enforce_rate_limit` poison fix from 2026-07-04 — apply the same policy
   (poison-tolerant helper) to all sites.
4. `[FIXED 2026-07-15]` **#3**: `commands/defs/export.rs` writes the export file via
   `std::fs::write` — the only non-test `atomic_write` bypass in the repo.
5. `[REPORTED]` **#1 (blocking on the event loop), three smaller cases**:
   `execute_wipe` runs `std::fs::remove_dir_all` synchronously (user-initiated,
   rare); `keys/autocomplete.rs::refresh_autocomplete` runs `read_dir` + one
   `metadata()` per entry on **every keystroke** while composing `@path` (no
   debounce, no spawn_blocking); `app/sessions.rs` `handle_load_session`/
   `finalize_broken_session` do sync `load_local`/`save_local` while
   `auto_save_session` in the same file routes through the persist worker.
6. `[VERIFIED]` **#6 (tools→app upward reaches), full extent mapped**: the
   known `FOREGROUND_CHILD_PID` slot (shell.rs), plus `request_terminal_restore()`
   (shell.rs, same category — cross-layer atomic signal), plus
   `shell_control.rs` calling `crate::app::format_duration_secs` — a **pure
   function** that merely lives in app/mod.rs. Moving it (and `format_size`)
   to `util.rs` removes one of three reaches for free.
7. `[REPORTED]` **#9-adjacent**: `tools/background/registry.rs::write_input`
   does a synchronous PTY stdin write inside async (no spawn_blocking) — the
   read side of the same PTY is correctly offloaded (spawn.rs). A stuffed pipe
   blocks a tokio worker.
8. `[VERIFIED]` **#9/#11 positive**: all 4 external semantic entry points
   (`runner.rs`, `app/search.rs`, `tool_search`, `history_search`) correctly
   wrap embedding in `spawn_blocking`; PoW solve confirmed still on
   `spawn_blocking`; deepseek http rate-limit lock still poison-safe.

## RELIABILITY DEFECTS FOUND WHILE SWEEPING (cross-filed in BUGS.md)

- `[FIXED 2026-07-15]` **MCP stdio children are not in the Windows Job Object.**
  `transport.rs` relies on `kill_on_drop(true)` alone; `win_job` (added
  2026-07-07 exactly because kill_on_drop doesn't fire on force-close) is
  wired only into `tools/background/spawn.rs`. Task-Manager kill / crash
  orphans every `npx …server-*` child. Fix: export `win_job::assign_*` and
  call it when spawning stdio transports (and consider shell.rs foreground).
- `[FIXED 2026-07-15]` **Tick/redraw divergence**: `tick_is_visual()` counts *any*
  conversation streaming, but the `Tick` arm advances `animation_tick` only
  for the *focused* one — a background sidechat forces a full redraw every
  120 ms for its whole lifetime while the focused frame never changes,
  defeating the documented idle-skip. Unify the two predicates (`app/mod.rs`).
- `[FIXED 2026-07-15]` **Sub-agents get no malformed-tool-call recovery**:
  `sub_agent.rs` calls `parse_tool_calls` (diagnostics discarded) instead of
  `parse_tool_calls_with_errors`, so a malformed `<tool_use>` silently returns
  preceding text (possibly empty) — the 2026-07-07 "frozen agent" fix never
  reached this path.
- `[FIXED 2026-07-15]` **Server stats skew**: `server/http.rs::route` returns 401
  before `stats.requests.fetch_add`, but `handle_request` still increments
  `errors` for it → `errors > requests` possible under a misconfigured client.
- `[REPORTED]` **ACP mode is a parallel mini-implementation**: no tools, no
  `system_prompt::build` (hardcoded one-liner), `PromptRequest.images` parsed
  then silently dropped; stdout write failures `.ok()`-swallowed → on a
  half-closed pipe the loop runs completions and discards responses forever.
  Decide: wire through `AgentRuntime` or document as deliberately minimal.
- `[REPORTED]` **`RagLimit` deserializer contract split**: `visit_u64(0)`
  silently floors to `Fixed(1)` while `visit_i64(0)` hard-errors — behavior
  depends on which visitor the TOML backend picks (`config/mod.rs`).
- `[REPORTED]` **`Conversations::remove` latent panic**: removing the focused,
  last conversation leaves `self.focused` dangling → next `focused()`
  `.expect` panics. Guarded only by today's two call sites (multichat).
- `[FIXED 2026-07-15]` **Server SSE bridges have no idle-timeout backstop**
  (`bridge_stream`/`bridge_legacy_stream`) — they rely on every provider
  having configured `read_timeout(120s)`, an unenforced convention;
  `collect_stream`'s explicit idle-timeout pattern is right there to reuse.
- `[REPORTED]` `session.rs::write_history` swallows serialize+write errors
  with no log (siblings `whitelist`/`semantic history` both log).

## DUPLICATION (ranked by payoff)

### Provider client trio — the largest single dedup (~150–200 lines)
- `[DONE 2026-07-15]` `openai_client.rs` / `anthropic_client.rs` / `gemini_client.rs`
  are near-identical end to end: same 4-field struct, byte-identical `new()`
  (same 10s/120s timeouts ×3), structurally identical
  `send`/`complete`/`complete_stream` (SSE pump)/`list_models`/`fork`. Only
  URL shape, auth header, body model-field, error JSON path, and per-event SSE
  dispatch differ. Extract a `CompatTransport<P: CompatProtocol>` with ~5
  protocol hooks; each protocol becomes ~40–60 lines.
- `[DONE 2026-07-15]` Satellites of the same cleanup: error-envelope parser
  byte-identical in anthropic/gemini clients; `data:`-prefix strip repeated
  4×; `merge_alternating_turns` copy-pasted between `anthropic_compat` and
  `gemini_compat` (only the assistant-role literal differs);
  `fetch_remote_session_messages` override in openai_client duplicates the
  trait default's behavior (delete it).

### Agent runner pair (residual after 2026-07-04's `collect_stream`)
- `[DONE 2026-07-15]` The MCP-dispatch block ("resolve client under short lock, call,
  fallback to registry") is copy-pasted `runner.rs` ↔ `sub_agent.rs`
  (sub_agent's comment admits "Same lock discipline as the main loop"); the
  per-step skip message is byte-identical in both; the step-request build
  (system prompt + `messages.clone()`) is duplicated. Extract
  `dispatch_generic_tool` + `tool_skip_message` + `build_step_request`.
- `[DONE 2026-07-15]` `deepseek/mod.rs` `complete` vs `complete_stream` repeat the
  same ~85-line SSE-consume loop (session persistence, parent-id capture,
  finished handling) — only text delivery differs. Same "shared core + chunk
  hook" fix as the runner pair got.

### Server gateway
- `[DONE 2026-07-15]` `server/openai.rs`: SSE bridge scaffolding (channel + spawn +
  `poll_fn` StreamBody + identical Response builder + identical 3-arm
  post-mortem match) duplicated between `stream_completion` and
  `legacy_stream` (~90 lines); blocking skeleton (complete → discard session →
  match into json_response) duplicated between `blocking_completion` and
  `legacy_blocking`; a bare `is_deepseek: bool` threaded through instead of
  `&ResolvedModel`.

### App layer
- `[DONE 2026-07-15]` "status_message + identical ui_system chat push" announce shape
  ×7 in `handle_event` → one `announce()` helper.
- `[DONE 2026-07-15 — shared `spawn_mcp_semantic_refresh`; the startup copy stays, it also signals `McpInitialized`]` mcp+semantic corpus-refresh spawn block ×3 (app/mod.rs ×2,
  keys/dispatch.rs RAG reload).
- `[REPORTED]` config save-or-rollback ("mutate → save → on Err roll back +
  format!(\"Failed to save config: {e}\")") ×10 across app/providers.rs,
  keys/themes.rs, keys/providers.rs, keys/dispatch.rs — the 2026-07-04
  `save_config_then` helper covered only `commands/defs/`.
- `[REPORTED]` `toggle_skill` (pickers.rs) and the Picker Skills arm
  (keys/modal.rs) both re-`config::load()` from disk, mutate `.skills.enabled`
  on the fresh copy and save — an extra read plus a latent lost-update of any
  in-memory config change. Mutate `self.config` like every other site.
- `[REPORTED]` Wizard Esc-steps-back state machine ×3 (mcp/providers/themes
  key handlers); clamped up/down navigation hand-rolled at 8+ sites across 4
  keys files while `events::handle_picker_key` already has the canonical
  version; scroll-follows-cursor windowing ×3 (`handle_picker_key`,
  `handle_delete_sessions_key`, `QuestionState::update_scroll`) with
  inconsistent hardcoded VISIBLE (12/12/10) + the same magic `12` again in
  keys/modal.rs and keys/mouse.rs (mouse.rs's comment even says "must mirror
  the key handler's clamp"). One `visible_rows` const + one `clamp_scroll`.
- `[VERIFIED]` char-boundary insert/remove reimplemented ×5 inside
  `app/events.rs` (OnboardingState, QuestionState) while the canonical
  `char_to_byte_pos` already exists in `app/input.rs` (and is imported by
  widgets/input.rs).

### TUI
- `[VERIFIED]` `render_question` custom-input mode hand-rolls single-line
  cursor rendering (and doesn't split `'\n'` — a paste renders glued) while
  `popup::push_text_box_lines` is already imported and used in the same file.
- `[REPORTED]` "selectable option list" loop ×4 (modals ×2, themes, providers)
  → `popup::push_option_list`; `GoalStage`→label match ×3 (status.rs ×2,
  landing.rs) → one helper next to the enum; selected-row background drawn
  from 3 different theme roles across 7 list renderers (drift, not tiers).
- `[VERIFIED]` `render/util.rs::truncate` measures `.chars().count()` while
  its neighbor `fit_col` correctly measures display columns — emoji/CJK
  under-truncate and misalign fixed-width tables (Cyrillic is width-1, safe).
  Repo-wide there are now **4 truncate/fit helpers with 3 measurement units**
  (chars / bytes / display cells) and 2 ellipsis styles: `render/util.rs`
  `truncate`+`fit_col`, `util.rs` `truncate_at_char_boundary`+
  `truncate_with_ellipsis`. Consolidate on width-aware.
- `[REPORTED]` `widgets/input.rs::render_input` computes the full wrap/scroll
  pass, then calls `cursor_pos_inner` which recomputes it from scratch — the
  per-frame work runs twice.

### Platform
- `[REPORTED]` `semantic/history.rs::search` vs `semantic/index.rs::query`
  hand-duplicate the same dense+sparse+RRF ranking recipe → free `rank()` fn.
- `[REPORTED]` `goal-evaluator.prompt.md` embedded twice: fixed evaluator
  prompt (prompts.rs, used by goal.rs) AND a user-toggleable builtin skill
  (discovery.rs) — the file's own exclusion rationale for base/tools applies.
- `[REPORTED]` `session.rs::save_session` re-implements `save_local`'s body;
  import.rs flush block ×2; tool_search/history_search identical
  limit-clamp consts; background kill-dispatch ×4 + capture-initial-output ×2
  (spawn.rs); shell.rs `capped_pipe_reader` vs spawn.rs `pipe_reader_loop` —
  two different byte-cap strategies for the same problem (document or merge).
- `[REPORTED]` `mcp/oauth.rs::http_client` (flat 15s) still diverges from
  `transport.rs::build_http_client` (10s/60s/60s + cookies) — this exact item
  was `[REPORTED]` on 2026-07-04 and was **not** done (unlike the other MCP
  items, which stuck). `load_own_config_file` parses `mcp.json` 3× per
  `reload_all` pass (enabled-map / overrides / own-config each re-read).

## OVERLOADED MODULES / EXCESSIVE BRANCHING

### The big four
1. `[DONE 2026-07-15]` **`agent/runner.rs::run_agent_loop` — 513 lines, CC 42, and a
   14-positional-arg signature** under `#[allow(clippy::too_many_arguments)]`
   even though `TurnSpec` exists precisely for this: `AgentRuntime::spawn`
   unpacks the spec back into positional order (runtime.rs doc comment
   contradicts its own body), and 3 test call sites repeat all 14 positions.
   Sweep mapped 8 sequential phases; cleanest extractions first:
   `inject_semantic_hint`, `stream_step`, and `execute_tool_call(call, ctx:
   &ToolExecContext)` for the ~152-line 5-deep tool dispatch (which also
   becomes the shared home for sub_agent's copy). Signature fix: pass
   `TurnSpec` + the runtime deps directly.
2. `[DONE 2026-07-15 — `app/view_state.rs`, glob-re-exported]` **`app/events.rs` (1440) is two files**: the `AppEvent`
   contract (~200 lines, imported by server/semantic/agent/logging as the
   cross-layer vocabulary) buried under ~900 lines of UI state machines
   (View, Onboarding, Picker*, Confirm*, Modal, DeleteSessions*, Question*).
   Split: events.rs keeps the event vocabulary; per-modal state moves to
   `app/view_state/` (or per-feature modules). Everything imports this file —
   the split shrinks incremental-build blast radius too.
3. `[DONE 2026-07-15 — arm delegation + announce(); 1309→1190, CC ok]` **`app/mod.rs` regrew 990 → 1309**: the regrowth is precisely
   the new features bolted onto the dispatcher — server/update/model-cache
   wiring in `new()` + Server*/UpdateStatus/ProviderModelsRefreshed/
   SessionsDeleted arms in `handle_event` (338 lines, CC 30). Fix is
   delegation, not a new file: each domain's arms → a method in that domain's
   module (serve.rs, mcp_status.rs, goal.rs — the GOAL stage transition inside
   `AgentDone` belongs next to `apply_verdict`), leaving handle_event as pure
   dispatch. `run_loop`'s post-select "settle the frame" phase (drain, MCP
   refresh, terminal-restore, conditional render) is its own extraction.
4. `[PARTLY DONE 2026-07-15 — the GOAL machine moved to goal.rs; dispatch/modal arm extraction remains]` **keys layer**: `submit_input` (147) inlines a 4-stage GOAL
   state machine + paste-chip expansion + history + attachments — extract the
   GOAL block to goal.rs; `apply_command_result` (235) and `handle_modal_key`
   (163) both already contain the fix pattern (two arms delegate to
   `apply_rag_action`/`handle_mcp_add_key`) — apply it to the remaining
   multi-line arms; `handle_mcp_add_key` Wizard arm nests 5–6 deep → per-step
   validate-and-advance fns; `handle_question_key` (121) multiplexes 3 modes
   (mirror of `render_question`'s 3 modes — same feature, both sides split
   the same way).

### Also worth decomposing (function-level, files are fine)
- `[REPORTED]` `ShellTool::execute` ~294 lines: foreground path inline while
  interactive/background are extracted — extract `run_foreground` to match.
- `[REPORTED]` `system_prompt::build` (105): two structurally identical
  3-way (empty/deferred/full) MCP sections → section builders.
- `[REPORTED]` TUI >150-line renderers: `render_question` 218 (3 modes),
  `render_stats_panel` 210 (6 ready-made sections), `render_onboarding` 212,
  `render_mcp` 190; `render/mod.rs` chat branch inlines ~85 lines of layout
  while every sibling view is a one-line delegate (contradicts its own doc).
- `[REPORTED]` `insert_handle` 13 params / `spawn_interactive` 10 params with
  adjacent same-typed bools/u16s under `#[allow(too_many_arguments)]` →
  params struct.

### Explicitly NOT split-worthy (verdicts, per CONVENTIONS file-size rule)
`config/mod.rs` 867 (homogeneous schema), `mcp/config.rs` 751 (8 homogeneous
loaders), `mcp/transport.rs` 696 (one trait, 4 impls), `server/openai.rs` 973
(one dialect's endpoint catalog — dedup internally instead), `theme.rs`
(data catalog), `render/modals.rs` (accepted per-modal catalog),
`deepseek/endpoints.rs` (documented parity catalog). `mcp/manager.rs` 917 —
split "yes but low urgency" (lifecycle/connect/persist impl-block split).
Mild splits suggested: `openai_compat.rs` → extract the self-contained
~175-line reasoning-split state machine to `provider/reasoning.rs`;
`deepseek/stream.rs` → move the free-function SSE-shape-parsing cluster to
`sse_parse.rs`; `skills/discovery.rs` → move prompt-injection formatting out
of the filesystem scanner; `semantic/mod.rs` → extract mutex-free
`render_hint`+`PromptMatches`+`RagStatus`; `app/pickers.rs` → picker builders
vs status-text display builders are unrelated under a misleading name.

## DEAD CODE

- `[VERIFIED]` `UiConfig::show_status_bar` / `show_line_numbers` /
  `max_message_length`: defined, defaulted, serialized, round-trip-tested —
  **zero consumers**. Wire or delete.
- `[REPORTED]` `ServerCapabilities` deserialized from every `initialize` then
  discarded by the sole caller; `list_resources()` then runs unconditionally
  even for servers that never advertised resources (guaranteed doomed
  round-trip + error log). `JsonRpc*::_meta` always None, never read.
- `[REPORTED]` `Delta.reasoning_content`/`ChatCompletionMessage.reasoning_content`
  populated on the outbound/server direction but never read inbound — an
  upstream reasoning model's `reasoning_content` is silently dropped when
  pooprusteek is the client (not covered by the file's "deliberately not
  translated" note, which only names tool-calling).
- `[VERIFIED]` `widgets/input.rs::cursor_pos` — `#[allow(dead_code)]`, zero
  callers, its private twin is the live one. Delete.
- `[REPORTED]` ACP `PromptRequest.images` parsed, never read (see RELIABILITY).
- Known/stable (owner decisions, unchanged): `provider/types.rs` parity
  structs, `endpoints.rs` wrapper collection, `ProviderKind::Openai/Custom`,
  `MCPServerStatus::Connecting`, events.rs `#[expect]` payloads.

## TEST GAPS (worst-first)

- `[REPORTED]` `mcp/transport.rs` (696), `tools/background/spawn.rs` (356),
  `tools/background/registry.rs` (272): **zero tests** on the hardest
  concurrency logic in the repo (chunk-boundary SSE framing, stdio
  id-correlation under interleaved notifications, spawn/kill-tree, prune
  loops) while the easy pure files around them are well tested.
- `[REPORTED]` `app/keys/`: 11 of 12 files have no tests (only modal.rs's two
  pure decoders); `app/sessions.rs` (456) and `app/pickers.rs` (286) have
  none. Riskiest untested pure logic: autocomplete's `@path` buffer splice
  (byte-index math — currently correct, verified, but one edit from a panic),
  GOAL stage transitions in `submit_input`, delete-scope resolution.

## INEFFICIENCY (small, hot-path)

- `[REPORTED]` `run_agent_loop`/`run_sub_agent` clone the entire message
  history every step (O(steps × history)) to prepend the system prompt —
  same anti-pattern the 2026-07-04 audit flagged for `split_system_prompt`.
- `[REPORTED]` `MCPClient` locks the whole transport for a request round-trip,
  serializing concurrent tool calls per server; only stdio actually needs
  exclusivity (HTTP/SSE are naturally concurrent, session_id already has its
  own Mutex). Trait-split or `&self` path for HTTP/SSE.
- `[VERIFIED]` Tick/redraw divergence (filed under RELIABILITY) is also the
  biggest idle-CPU item. `render_input` double wrap pass (TUI section).

## VERIFIED CLEAN (don't re-litigate)

- Layering: **zero** `tools/→app/` imports beyond the 3 mapped reaches; no
  invariant-#6 import violations anywhere else; `AppEvent` is the contract.
- `server/openai.rs` **reuses** `provider::openai_compat` wire types — the
  suspected server↔provider struct duplication does not exist.
- 2026-07-04 fixes held: endpoints wrappers stay on `post_biz`/`post_void`/
  `get_biz`; command helpers (`with_args`/`save_config_then`/`push_system`)
  adopted by every post-04 command; `apply_connect_outcome` hoist, config
  `load_from_path`/`merge_parsed` intact; per-keystroke Modal clone still
  gone; rate-limit poison fix intact. Exception: oauth http-client share —
  never done (see DUPLICATION).
- `session.rs` → `app/persist.rs` → `app/sessions.rs` is a clean three-way
  responsibility split (load/IO primitives → write-ordering worker → App
  orchestration) — a sweep hypothesis to the contrary was checked and dropped.
- `util.rs` has no dead helpers; `error.rs` variants all constructed;
  `main.rs` pre-TUI prints exempt and correct; fastembed progress bar
  suppression in place; `model_cache.rs` uses `atomic_write`; `catalog.rs`
  and `render/search.rs` cited as exemplary single-responsibility modules.

## DISCARDED SWEEP CLAIMS (checked by lead, not real)

- "`println!` in tui/widgets/chat.rs violates #10" — it's inside the
  `#[ignore]`d perf probe / test fixture. Test-only, fine.
- "`accept_autocomplete` byte-slicing can panic on non-ASCII" — indices come
  from `rfind('@')`/`split_whitespace` on the same string; boundaries are
  sound today. Kept as *fragility* under TEST GAPS, not a bug.
- "app/mod.rs reconnect freezes the event loop" — it's inside `tokio::spawn`;
  downgraded to lock-held-across-I/O (letter of #2), see INVARIANTS item 2.
- "server/openai.rs duplicates provider wire structs" — refuted, it imports
  them (only Legacy* types are new and have no provider equivalent).

## SUGGESTED EXECUTION ORDER

1. `[DONE 2026-07-15]` **Defect batch (small, high value)**: win_job for MCP stdio children;
   spawn the `'d'` remove_server arm; unify `tick_is_visual`; sub_agent →
   `parse_tool_calls_with_errors`; export.rs → atomic_write; 401 stats fix;
   semantic poison-tolerant lock helper (+ embed-outside-lock ×3).
2. `[DONE 2026-07-15]` **Dedup batch A (provider)**: client-trio `CompatTransport` extraction +
   satellites (error parser, `data:` strip, merge_alternating_turns);
   deepseek complete/complete_stream shared core.
3. `[DONE 2026-07-15]` **Dedup batch B (agent/server)**: `dispatch_generic_tool` +
   `build_step_request` + `tool_skip_message`; server SSE-bridge +
   blocking-skeleton helpers (+ idle-timeout backstop while there).
4. `[DONE 2026-07-15]` **Structure batch**: `run_agent_loop` phase extraction + `TurnSpec`
   signature; events.rs contract/state split; handle_event arm delegation;
   keys extractions (GOAL block out of submit_input first).
5. **Sweepables (mechanical, anytime)**: announce() helper, save-or-rollback
   helper, nav/scroll helpers + visible-rows const, `format_duration_secs` →
   util.rs, option-list/text-box popup helpers, truncate consolidation,
   UiConfig ghost fields, `cursor_pos` deletion, magic-number consts.
6. **Owner decisions**: ACP mode fate (wire tools or document as minimal);
   ServerCapabilities gating vs deletion; reasoning_content inbound handling;
   goal-evaluator double embedding; pipe-cap strategy convergence.
