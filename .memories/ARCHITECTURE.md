# ARCHITECTURE
> How the pieces fit and how data flows. Read after MAP.md.
> Last updated: 2026-11-07 (шлюз аккаунтов и маршрутов, секреты, новые слои
> конфига). Before: 2026-07-02 (post stability/perf overhaul — event-driven
> GOAL evaluator, drain+dirty render, interaction queue)

## LAYERS (clean-ish, layered architecture)

```
Presentation   tui/ (render, widgets, theme, markdown)
Application     app/ (App + AppState, event loop, key handling), config/, session.rs
Domain          agent/ (loop, tool parser), provider/ (LLMProvider, failures),
                gateway/ (accounts, routes, roles, backend adapters), tools/
Infrastructure  mcp/ (clients, transports, discovery), secrets/ (keyring, references),
                acp/ (server mode), skills/, harness/
```

`.docs/architecture.md` describes the *intended* design and is partly **aspirational** (it names `AgentLoop`/`ContextManager`/`StreamingResponse` structs that don't exist — the real thing is the free function `run_agent_loop`). Trust the code + this folder over `.docs/`.

## THE CENTRAL OBJECT: `App` (`app/mod.rs`)

`App` is now a **thin coordinator**, not a god-file (`mod.rs` ~925 lines, down from ~2.4k). It owns: `config`, `state: AppState`, the event channel (`event_tx/rx`), `commands`, `mcp` (`Arc<Mutex<MCPManager>>`), `tools` (`Arc<ToolRegistry>`), `prompts`, `skills`, and `runtime: AgentRuntime`. The old god-object was decomposed two ways:

1. **Cohesive sub-state structs** — each a module owning related fields: `conversation` (the big one), `generation`, `input`, `mcp_status`, `goal`, `background_stats`. `AppState` holds these instead of dozens of loose fields.
2. **Controllers** — types that own *dependencies* and expose narrow APIs, so behavior no longer reaches into all of `&self`:
   - `AgentRuntime` (`app/runtime.rs`) owns `tools`/`mcp`/`event_tx`; `spawn(TurnSpec)` is the single launch point for every agent turn (normal, sidechat, sub-agent).
   - `system_prompt::build(prompts, skills, tools, mcp, workspace)` (`app/system_prompt.rs`) — explicit narrow deps.
   - `BackgroundCounters` (`app/background_stats.rs`) + `McpStatus` poll methods (`app/mcp_status.rs`) — own their state + the registry/manager calls.

Behavior still split across `impl App` files (`keys.rs`, `multichat.rs`, `goal.rs`) but the central struct is small and its collaborators are explicit.

## CONVERSATIONS — multi-chat core (`app/conversation.rs`)

The single biggest architectural change. Previously the app was strictly single-turn: one `messages` Vec, one `provider`, one `agent_task`, one `current_session_id` — all global on `AppState`. Now:

- A **`Conversation`** owns its full live state: `id`, `kind` (`Main`/`Session`/`Sidechat`/`SubAgent`), `parent`, `title`, `session_id`, `messages`, **its own forked `provider`**, `generation`, `agent_task`.
- **`Conversations`** is the store: a `Vec<Conversation>` + a `focused` id. API: `focused()/focused_mut()`, `set_focus()`, `open()` (focus it), `add_background()` (don't), `remove()`, `get_mut(id)`, `iter()/iter_mut()`, `ordered_ids()`, `focused_id()`. **There is no live/parked duality** — every conversation, including the focused one, is a full record; switching focus is just changing an id.
- `AppState::focused()/focused_mut()` delegate to the store; `push_message`/`push_system` helpers avoid borrow conflicts.
- **Isolation via `fork()`**: each conversation gets its own `LLMProvider` instance with a fresh `SessionState`, so concurrent turns never collide on `session_state` — this is also what structurally prevents the old `parent_message_id` fork-bug.

## EVENT LOOP (`App::run_loop`, `app/mod.rs`)

A single `tokio::select!` at **120 ms tick** multiplexes:
1. **Tick** → animations, periodic MCP/background stat refresh (MCP every 2s). `tick_is_visual` distinguishes ticks that must repaint (animation frames, streaming) from ones that don't, so a fully idle session draws zero frames.
2. **Crossterm** key/resize (`EventStream`) → `handle_key` / `handle_event`.
3. **Internal channel** (`event_rx`) → `handle_event` for `AppEvent`s emitted by the agent/tools.
4. **Ctrl+C** → kill foreground child, shutdown background jobs (`tools::background::shutdown_all`) and MCP servers (`MCPManager::shutdown_all`), exit.

**Render batching**: instead of rendering on every single event, `run_loop` drains up to **256** queued events per iteration before repainting once, behind a dirty flag. Combined with a per-message thread-local markdown/syntect cache (`tui/widgets/chat.rs`, keyed by a content fingerprint, evicted past 4096 entries) and cached token-count estimates, this replaced the old "re-parse the entire transcript on every streamed token" behavior that caused a streaming death-spiral on long sessions.

This is the "event-driven, no render races" design from the README: the agent runs in a **spawned task** and communicates only via `AppEvent`s + channels, never touching `AppState` directly. Streaming itself is also spawned as its own task (`complete_stream` inside `run_agent_loop`/`run_sub_agent`) so the 120s idle-stream guard races the *live* network call instead of watching an already-fully-drained channel.

## `AppEvent` (`app/events.rs`)

TUI: `Key, Resize { rows }, Tick`. Agent — **один вариант** `Agent { conversation, event: AgentEvent }` (2026-08-27; раньше здесь было 13 плоских вариантов). `AgentEvent`: `Started, BeginAssistantMessage, Chunk(String), Message(ChatMessage), DiscardEmptyAssistant, Done(AgentResult), Failed(String), ToolStarted, ToolDone, ToolError, ToolOutputCleared, SessionReset, ContextUsage(u32)`. Взаимодействие с человеком: `RequestToolApproval, RequestQuestion`. Goal: `GoalEvaluationDone(GoalEvalOutcome)` (the old unused `GoalCycleFinished` variant was removed). MCP: `McpOperationDone` (reload/toggle/reconnect results delivered off the event loop). Sub-agent: `SpawnSubAgent { parent, label, prompt }`.

`handle_event`'s `AppEvent::Agent` arm вызывает `on_agent_event` (`app/mod.rs`), и дальше путь один на всех:

1. `reduce::apply(conversation, &event)` (`app/reduce.rs`) — **единственное место**, где агентское событие меняет историю беседы. Его же зовёт безголовый харнесс (`harness/driver.rs`), поэтому разойтись они не могут. До 2026-08-27 таких применений было три, и они успели разойтись.
2. `status_for(&event)` — строка состояния, только для фокусного не-фонового чата.
3. `reduce::turn_tail(end, focused, background)` — чей хвост у завершившегося хода: `Background` (свернуть в родителя), `Focused` (статистика, автосохранение, шаг GOAL) или `None` (параллельный чат не на экране — историю обновил, и всё).

Маршрутизация по-прежнему по **виду** беседы, а не по фокусу, поэтому побочный чат сворачивается в родителя, даже если на него переключились. Rendering only ever shows the focused conversation.

## PRIMARY DATA FLOW (a normal turn)

```
key 'Enter' → handle_key (app/keys.rs)
  ├ input starts with '/' → CommandRegistry::execute → CommandResult
  └ else → expand @file mentions → focused_mut().messages.push(user) → send_to_agent (app/mod.rs)
              system_prompt::build(prompts, skills, tools, mcp, workspace)
              AgentRuntime::spawn(TurnSpec{ conversation: focused_id, provider, messages, … unattended:false }) → run_guarded
                run_agent_loop → provider.complete_stream → SSE → CompletionChunk
                  → AppEvent::Agent{id, AgentEvent::Chunk(delta)} → on_agent_event → reduce::apply → render
                parse_tool_calls → per tool:
                  RequestToolApproval → modal → user Y/N → tools.execute()/mcp.call_tool()
                  → AppEvent::Agent{id, AgentEvent::Message(tool result)}
                  task tool → fork + run_sub_agent (fg) | SpawnSubAgent event (bg)
                AppEvent::Agent{id, AgentEvent::Done(_)} → reduce::apply(finish_turn)
                  → turn_tail == Focused → record stats → auto_save_session() → GOAL
```

## TOOL APPROVAL / QUESTION HANDSHAKE

Cross-task request/response uses `Arc<Mutex<Option<T>>> + tokio::Notify`:
- Agent task builds a `ToolApprovalRequest`/`QuestionRequest`, sends it as an `AppEvent`, then `await`s `.wait()`.
- Main loop shows the modal, captures the key, calls `.resolve(value)` which notifies the waiting agent task.
- **Interaction queue**: pending approval/question requests are held in a `PendingInteraction` `VecDeque` rather than a single overwritable slot, so a second request while one is displayed no longer orphans the first waiter forever. Esc/Ctrl+C now cancels a wedged turn (checked via `agent_task` presence) and purges its queued interactions (`purge_interactions_for`) instead of silently clearing the chat.
- While a modal is open the agent task for that conversation is still parked — this remains a deliberate, understood trade-off, not a bug.

## GOAL MODE (state machine) — `app/goal.rs`, `events.rs`

`GoalStage`: `Inactive → WaitForGoal → RunAgent1 → RunEvaluator → Done`.
1. `/goal` arms it; first user message = `goal_prompt`; second = `goal_text` (the success spec).
2. Agent 1 works a normal turn toward the goal.
3. On `AgentDone`, `spawn_goal_evaluation()` **spawns a task** that calls a **separate evaluator** (non-streaming `complete()`, lower temp) with `goal-evaluator.prompt.md`, off the `select!` loop — the evaluation call used to run inline inside `handle_event`, freezing the whole UI for its duration; it no longer does. The task reports back via `AppEvent::GoalEvaluationDone(GoalEvalOutcome)`, handled by a single `handle_goal_evaluation_done` path (the old duplicate/drifted inline-vs-event handling was removed, along with the dead unused `GoalCycleFinished` variant). A stale-verdict guard drops an evaluation result that arrives after the goal cycle has already moved on (e.g. cancelled mid-flight).
4. `parse_goal_verdict()` reads `**Status:** SUCCESS/FAILURE` + summary/issues/feedback.
   - SUCCESS → `Done`, evaluator session saved tagged `__goal_system__`.
   - FAILURE → feedback fed back to Agent 1 as a real user-visible retry prompt via `user_with_display` (previously this only showed as a UI notice and never actually reached the model); counters increment.
5. **Session swapping**: after **3** agent-1 failures → fresh agent-1 session; after **5** evaluator failures → fresh evaluator session. Two distinct session ids tracked (`goal_agent1_session_id`, `goal_agent2_session_id`).
6. **Hard iteration cap**: `MAX_GOAL_ITERATIONS = 10` (`app/goal.rs`) — `apply_verdict` gives up at the cap instead of looping forever (tested: `gives_up_at_iteration_cap`).

## AGENT-TURN LAUNCH (`AgentRuntime` + `TurnSpec`)

Every agent turn — normal, sidechat, or sub-agent — is described by a `TurnSpec { conversation, provider, messages, system_prompt, model, temperature, max_tokens, max_steps, max_tools_per_step, context, unattended }` and launched by `AgentRuntime::spawn(spec)` (`app/runtime.rs`), which `tokio::spawn`s `run_agent_loop(...)`. `unattended` only removes what needs a watching human (questions, timers, nested `task`); tools still ask, and the harness answers those requests from an `ApprovePolicy`. There is no approval bypass.

## SUB-AGENTS (`agent/sub_agent.rs`, `runner.rs`, `/agent` `/agents`)

Reference: Claude Code. Spawned by **the model** (a `task` tool call, special-cased in `agent/tools_step.rs::execute_tool_call` like `question`) and by **the user** (`/agent <prompt>`).
- **Foreground (default)**: `tools_step::spawn_task` forks the provider, awaits `run_sub_agent(...)` inline, and returns only its final text as the tool result — isolated context, just the conclusion crosses back.
- **Background (`background:true`)**: emits `AppEvent::SpawnSubAgent { parent, label, prompt }`, returns immediately; `App` spawns it as a `SubAgent` conversation (`spawn_sub_agent`), notifies + delivers the result into the parent on completion.
- Tracked/stoppable via `/agents` (picker) and the conversations store. Sub-agents auto-approve within their toolset and don't recursively spawn (depth-limited).

## `/btw` SIDECHAT + PARALLEL SESSIONS (`app/multichat.rs`, `/btw` `/new` `/chats`)

- **`/btw <q>`**: a one-shot `Sidechat` conversation with a forked provider + small step cap, spawned in the background (`spawn_sidechat`); its answer streams in without disturbing the main turn (events route by id).
- **`/new`**: open a new full `Session` conversation (forked provider, fresh disk session), focus it. **`/chats`**: picker to switch focus; **Tab / Ctrl** cycles (`cycle_focus`). Background conversations keep streaming into their own buffers; switching focus only changes what renders. Status bar shows count + how many are streaming.
- Esc/Ctrl+C abort targets the **focused** conversation only; on shutdown all conversations' tasks are aborted.

## PROVIDER FORKING (`LLMProvider::fork`)

`fork(&self) -> Arc<dyn LLMProvider>` (`provider/mod.rs`) returns a fresh-session sibling sharing config/token. `DeepseekProvider::fork` rebuilds from stored config with a new `SessionState` (`fork_session`); `FakeProvider::fork` returns a new instance. Unit-tested for independent session state. This is the analog of poopseek's `provider.clone()` and the foundation that makes concurrent conversations safe.

**With the gateway on, `fork()` goes through `RoutedProvider`:** the fork keeps the engine, the account semaphores and the route state, but gets a fresh backend (`Backend::fork()`) and a fresh provider session. A turn may change target mid-conversation; `history::flatten_for_switch` then rewrites foreign tool calls into text before the new backend sees the history.

## GATEWAY — ACCOUNTS, ROUTES, ROLES (`src/gateway/`)

The gateway is a peer of `provider/`, not a replacement: it owns *which*
provider answers, while the provider owns the wire protocol.

```
Config { gateway, accounts, routes, roles }
        │
        ├─ Engine (Gateway trait) ── Core::select ── AccountState + LimitSnapshot
        │        │                         └─ Decision { why, skipped[], retry_at }
        │        ├─ BackendFactory: BuiltinFactory (DeepSeek + API) · CliFactory (claude/opencode/codex)
        │        └─ ConfigPersist → AppEvent::GatewayPersist → config::save
        │
        └─ RoutedProvider (Arc<dyn LLMProvider>) ── the only provider the rest sees
                 ├─ EmitTracker: fallback only before the first visible chunk
                 ├─ semaphore per account, cooldown, Retry-After
                 ├─ ledger (data_dir/gateway/usage.json)
                 └─ served_by() for /serve headers
```

- **Build off the loop.** `main_provider()` sits behind `gateway::pending`:
  `PendingGateway` starts `build_gateway` on `spawn_blocking` (secrets are
  blocking), the app gets `PendingHandle`, and `AppEvent::GatewayBuilt` swaps it
  in. A request that arrives during the build waits; a disabled gateway is ready
  immediately and reads nothing.
- **One selection function.** `Core::select` is the only place that decides;
  the screen, `/serve` and roles read the same `Decision` and the same
  `snapshot()`.
- **UI is read-only.** `app/gateway_view*` renders `Gateway::snapshot()` and
  writes only `Gateway::apply_edit()`. `View::Accounts` (commands `/accounts`,
  `/routes`, `/roles`) is one screen with three focuses; the wizard and the
  masked secret prompt are `Modal::{AccountWizard, SecretPrompt, AccountLimits,
  Trust}`.
- **Roles are separate calls, not separate chats.** `provider_for_role` returns
  an `Arc<dyn LLMProvider>` for `title`, `summary`, `evaluator`; callers fork it,
  so a service call never touches the conversation or its server session.
- **`/serve`** holds a `GatewayCell` (`server/routing.rs`) and asks
  `provider_for_model_id(..., CallSource::Serve)`. `CallSource::Serve` never
  selects `cli:*`; the catalog is `auto/<route>` and `<account>/<model>` only.
- **Accounting is append-and-merge.** `EngineLedger` accumulates in memory,
  flushes every ~30 s via `atomic_write`, dedups by `(request_id, attempt,
  account)` and merges another process's file instead of overwriting it.

Deeper: `reference/GATEWAY.md`.

## SECRETS AND LAYERED CONFIG (`src/secrets/`, `src/config/`)

Config is no longer one file at a fixed path:

```
~/.pooprusteek/config.toml          shared layer (written by default)
<repo>/.pooprusteek/config.toml     optional project layer (untrusted)
--config <file>                     replaces both for one run (harness)
dirs::data_local_dir()/pooprusteek  data, cache, model
```

- **Secrets are references**, resolved by `StoreResolver` at adapter build
  time: `keyring:`, `env:`, `cmd:`, `file:`, `none`. `Secret` has no `Display`,
  `Debug` prints `<secret>`, and its buffer is zeroed on drop.
- **Project layer is fail-closed.** It may not contain keys, `[server]`,
  `[update]`, `[instructions]`, `[[providers]]`, `[secrets]`, `[trust]`, CLI
  `home`, or `cmd:`/`file:` references. Anything that changes where requests go
  (`[gateway]`, `[[accounts]]`, `[[routes]]`, `[roles]`) waits for
  `pooprusteek trust` — a SHA-256 of the file recorded in the shared layer.
  Editing the file asks again. Confirming the file does not hand over a secret:
  `pooprusteek secrets bind <ref> <host>` trusts a reference for a host.
- **Saving preserves the file.** `config::save` diffs against what was loaded
  and applies the difference through `toml_edit`, under `config.toml.lock`,
  on one worker thread (the event loop never waits for the disk). A file that
  fails to parse is never overwritten.
- **One wildcard.** The model cannot write `.pooprusteek/`, but `bash` can. The
  trust hash is the safety net: the next check sees a different file and asks
  again. This is stated in the README, not hidden.

Deeper: `reference/CONFIG.md`, `.docs/secrets-and-config.md`.

## TWO RUN MODES (`main.rs`)

- **TUI** (default): full interactive app.
- **ACP server** (`--acp`): `acp::server::AcpServer` speaks ND-JSON JSON-RPC over stdio (`initialize`, `prompt`, `ping`) — lets an external client (IDE) drive the DeepSeek provider. Non-streaming `complete()` only; no tools.

## KEY INVARIANTS / PATTERNS

- **State is single-threaded**; only the main loop mutates `AppState`. Everything async talks through channels.
- **Async I/O everywhere** via Tokio; blocking PTY reads isolated on `spawn_blocking`.
- **`Arc<dyn Trait>`** for `LLMProvider` and `Tool` — swappable abstractions (though only one impl each today).
- **Errors** bubble via `AppResult<T>` + `?`; user-facing ones land in the status bar.
- **Assets resolution** (prompts, WASM): try `CARGO_MANIFEST_DIR` (dev) → CWD → exe-dir (release). Keep `assets/` next to the binary when distributing.
