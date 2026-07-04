# AUDIT — 2026-07-04 (cleanup / simplification pass)

> Scope: duplication, dead code, inefficiency, bad practices, implicit coupling.
> Not a bug hunt — see `AUDIT-2026-07-02.md` for the defect audit. Functional
> behavior must not change; goal is a smaller, simpler, easier-to-use codebase.
> Method: 6 parallel subsystem sweeps + lead verification of top findings.
> Status legend: `[VERIFIED]` = confirmed by reading the code, `[REPORTED]` =
> subsystem-sweep finding, spot-check before acting, `[DECISION]` = deliberate
> `#[expect(dead_code)]`-style trade-off the owner should re-confirm or delete.
> Anchors are function/struct names; line numbers drift.

## RELIABILITY (fix while cleaning — small, high blast radius)

- `[VERIFIED]` `enforce_rate_limit` uses `.lock().unwrap()` on `request_history`
  while every other lock in the provider uses the poison-safe `.lock().map(...)`
  pattern — one inconsistent panic vector. `→ src/provider/deepseek/http.rs`
- `[VERIFIED]` PoW solve still runs synchronously on the async runtime (no
  `spawn_blocking`) and a retry reuses the once-solved (possibly stale)
  challenge — both already tracked in STATE.md KNOWN GAPS; note: PoW is NOT
  re-solved per retry (a sweep claimed so — false; headers are built once
  before the retry loop). `→ src/provider/pow.rs`, `deepseek/stream.rs`
- `[REPORTED]` Streaming accumulators `full_response`/`full` are unbounded —
  no size cap analogous to the shell tool's 1MiB cap. `→ src/agent/runner.rs`,
  `src/agent/sub_agent.rs`

## DUPLICATION

### TUI (`src/tui/render.rs`, 2160 lines — regrown god-file)
- `[REPORTED]` Popup-centering block repeated in ~8 modal renderers → extract
  `center_popup(area, w, h) -> Rect`.
- `[REPORTED]` Modal `Block` header boilerplate (borders/title/style) repeated
  ~7× → extract `modal_block(title, color, theme)`.
- `[REPORTED]` Fill-remaining-space loop (~5×) and separator-line build (~4×)
  → tiny helpers.
- `[REPORTED]` Proposed split of `render.rs`: `modals.rs` / `landing.rs` /
  `mcp_view.rs` / `status_bar.rs` / small `util` — main `render.rs` keeps only
  dispatch. Long functions to break up while moving: `render_delete_sessions`
  (~210 lines, confirm+list mixed), `render_onboarding` (~185),
  `render_question` (~180, three modes in one), `render_mcp_add` (~140).

### Agent (`runner.rs` vs `sub_agent.rs`)
- `[VERIFIED]` The spawn-stream + idle-timeout + recv-loop + `stream_task.await`
  post-mortem block is structurally duplicated between `run_agent_loop` and
  `run_sub_agent` (the comment in sub_agent even says "Same shape as the main
  agent loop"). Extract a shared `stream_completion(provider, request,
  on_chunk) -> Result<(String, bool)>` where only chunk handling differs.
- `[VERIFIED]` Tool names `"question"`/`"task"`/`"mcp__"` are string-literal
  special cases in both runners → shared constants or an `is_meta_tool()` /
  dispatch enum in `tools/mod.rs`.

### Provider (`deepseek/endpoints.rs`)
- `[REPORTED]` ~18 of 26 REST wrappers still hand-roll headers +
  `send_json_request` + `is_success` + `read_error_response` instead of the
  `post_biz`/`get_biz` helpers (8 already migrated). Add a void-return variant
  (`post_void`) and migrate the rest.
- `[REPORTED]` PoW+auth header assembly repeated inline in `edit_message`,
  `regenerate_message`, `upload_file` while `get_chat_headers` already exists
  → one `headers_with_pow()` helper.

### MCP
- `[VERIFIED]` `apply_connect_outcome` repeats the identical `tool_name_map`
  insertion loop in both branches (cached vs fresh-connect) → hoist the loop
  above the branch. `→ src/mcp/manager.rs`
- `[REPORTED]` The 7 per-source config loaders in `config.rs` share the same
  resolve-path / read / parse / drain skeleton → one `load_from_path(path,
  parser, &mut configs)` helper.
- `[REPORTED]` `oauth.rs` builds its own `reqwest::Client` (15s timeout only)
  while `transport.rs::build_http_client` sets connect/read timeouts + cookies
  → share one builder.

### Commands (`src/commands/defs/`, ~30 files)
- `[REPORTED]` Empty-args + usage-error prologue repeated in ~14 commands →
  `require_args(args, usage)` helper in `commands/mod.rs`.
- `[REPORTED]` `clear.rs`/`home.rs`/`reset.rs` share ~70% of their
  state-clearing bodies → `AppState::clear_ui()` / `reset_session()` methods.
- `[REPORTED]` Config load→mutate→save→`ResetProvider` block repeated in
  `mcp.rs`/`rate.rs`/`retry.rs` → `apply_and_save_config` helper.
- `[REPORTED]` `push_system` helper exists but most commands still push
  `ChatMessage::system(...)` manually (14 direct vs 2 via helper) — adopt it
  uniformly; also unify the error channel (some commands report via
  `CommandResult::Error`, some via system message, some via status line).

### App
- `[REPORTED]` Tool-approval y/n resolution duplicated between the keys.rs
  modal handler and the auto-approve path in `handle_event`
  (`RequestToolApproval`) → one `resolve_tool_approval(approve: bool)`.
- `[REPORTED]` `retry_agent1` / `retry_agent1_with_feedback` in goal.rs are
  ~75% identical → single parameterized method.

## INEFFICIENCY

- `[VERIFIED]` `handle_key` clones the whole `Modal` (can hold a large
  `arguments` string) on every keystroke while a modal is open; scroll keys
  then rebuild the variant field-by-field → mutate in place / `take()`.
  `→ src/app/keys.rs`
- `[REPORTED]` `render_mini_status` builds ~12 fresh `String`s per draw; draws
  are dirty-flag gated, but during streaming/animation that's every frame →
  reduce allocations or cache until inputs change. `→ src/tui/render.rs`
- `[REPORTED]` `load_mcp_config()` re-reads all 8 config sources from disk on
  every reload; manager could cache and re-read only on explicit mutation.
  `→ src/mcp/manager.rs` (`initialize`, `reload_all`)
- `[REPORTED]` Tool/resource lists cloned on every cache snapshot / read —
  `Arc<Vec<MCPTool>>` would make reads free. `→ src/mcp/manager.rs`
- `[REPORTED]` SSE event normalization allocates ~9 strings per event and can
  triple-parse a payload (`normalize_event_payload` + fallbacks) → `Cow`/slices
  and 1–2 documented parse attempts. `→ src/provider/deepseek/stream.rs`
- `[REPORTED]` `split_system_prompt` clones the full message history each turn
  → borrow instead. `→ src/provider/prompt.rs`
- `[REPORTED]` `ToolRegistry::definitions()` rebuilds all static JSON schemas
  per call (once per turn — cheap, LOW) → cache, invalidate on
  register/update_skills. `→ src/tools/registry.rs`

## DEAD CODE / SCAFFOLDING (mostly `[DECISION]` — delete or wire up)

- `[DECISION]` `AgentResult`/`ToolCallInfo` + `ToolDone.result` — all
  `#[expect(dead_code)]`, payloads built at send sites but discarded by every
  receiver. Either surface tool-result previews in the UI or delete (~40 lines
  + the per-call `tool_result.clone()` that feeds them). `→ src/app/events.rs`
- `[DECISION]` `provider/types.rs` is file-level `#![allow(dead_code)]` with
  ~15 unused request/response structs (kept for API parity); several SSE event
  structs (`CompletionReadyEvent` etc.) are deserialized then never read.
  Prune now that the endpoints split settled, or gate behind a feature.
- `[DECISION]` `ProviderKind::Openai`/`Custom` declared + rendered in the
  onboarding model selector, but no implementation exists (known STATE.md gap)
  — misleading UI; hide the variants until a second provider is real.
- `[DECISION]` `MCPServerStatus::Connecting` — `#[expect(dead_code)]`, never
  constructed. `→ src/mcp/types.rs`
- `[VERIFIED-ish]` Small strays: empty `impl JsonRpcNotification {}`
  (`mcp/jsonrpc.rs`), `_count` in `render_sessions_table`, unused
  `list_sessions(_config)` param (`session.rs`), `urlencoding()` used only by
  an unused endpoint (`endpoints.rs`), unreachable `execute()` stubs on
  `QuestionTool`/`TaskTool` (always intercepted in the runner — keep the stub
  error but note it, or route the runner through them).

## COUPLING / STRUCTURE

- `[REPORTED]` `app/mod.rs` regrew 925→1659 lines: session load/delete
  (~250 lines), display builders, picker logic all live in the coordinator
  again → extract `app/sessions.rs` (+ optionally `app/display.rs`).
- `[REPORTED]` Modal handling is spread across keys.rs (input) + mod.rs
  (effects) + render.rs (view) — every new modal touches 3 files with a copy
  of the same match skeleton. The planned key→intent/intent→effect split
  (STATE.md) is the real fix; helpers above are the cheap interim.
- `[REPORTED]` `pending_tool_approval` and `Modal::ToolApproval` must be set
  together but nothing enforces it (comment-enforced invariant).
- `[REPORTED]` Modal renderers take full `&AppState` instead of their own
  state slice → pass `&PickerState`/`&QuestionState`/etc.
- `[REPORTED]` `ToolRegistry::definitions()` returns HashMap-iteration order —
  system-prompt tool order is nondeterministic across runs → sort by name or
  keep registration order.
- `[STALE-FIXED]` The 2026-07-02 claim "`delete_remote_session` has zero call
  sites" is now stale — `discard_remote_session` calls it from runner /
  multichat / app since the remote-session work (c4416d2, 43e0f09). BUGS/STATE
  should drop that entry.

## DISCARDED SWEEP CLAIMS (checked, not real)

- "PoW re-solved on every retry" — false, solved once before the retry loop
  (the *stale challenge on retry* issue is the real one, already tracked).
- "UUID v4 as OAuth `state` is weak" — 122 bits of randomness is fine.
- "Bearer token logged in plaintext" — the header is built, not logged; only
  worth a redacting `Debug` impl if `TokenSet` ever derives Debug.
- "Skills re-discovered per call" — discovery runs once in `App::new`.

## SUGGESTED EXECUTION ORDER

1. **Reliability trio** (http.rs unwrap, PoW `spawn_blocking`, stream cap) —
   small diffs, test-covered area, do first and alone.
2. **Mechanical dedup helpers** (TUI modal helpers, command helpers,
   `post_void` migration, MCP config-loader helper, `apply_connect_outcome`
   hoist, strays deletion) — low-risk, parallelizable, big LOC win.
3. **Structural splits** (`render.rs` → 5 modules; `app/mod.rs` →
   `sessions.rs`; runner/sub_agent stream extraction; meta-tool constants) —
   behavior-preserving moves, one subsystem per PR, tests after each.
4. **Decisions needed from owner**: events.rs dead payloads, types.rs parity
   structs, Openai/Custom scaffolding, `Connecting` variant.
