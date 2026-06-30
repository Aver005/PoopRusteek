# ARCHITECTURE
> How the pieces fit and how data flows. Read after MAP.md.
> Last updated: 2026-06-30 (post conversation-unification + controllers refactor)

## LAYERS (clean-ish, layered architecture)

```
Presentation   tui/ (render, widgets, theme, markdown)
Application     app/ (App + AppState, event loop, key handling), config/, session.rs
Domain          agent/ (loop, tool parser), provider/ (LLMProvider, DeepSeek), tools/
Infrastructure  mcp/ (clients, transports, discovery), acp/ (server mode), skills/
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

## EVENT LOOP (`App::run_loop`, `app/mod.rs:256`)

A single `tokio::select!` at **120 ms tick** multiplexes:
1. **Tick** → animations, periodic MCP/background stat refresh (MCP every 2s).
2. **Crossterm** key/resize (`EventStream`) → `handle_key` / `handle_event`.
3. **Internal channel** (`event_rx`) → `handle_event` for `AppEvent`s emitted by the agent/tools.
4. **Ctrl+C** → kill foreground child, shutdown background jobs, exit.
After each iteration: refresh MCP view if active, handle terminal-restore flag, `render()`.

This is the "event-driven, no render races" design from the README: the agent runs in a **spawned task** and communicates only via `AppEvent`s + channels, never touching `AppState` directly.

## `AppEvent` (`app/events.rs`)

TUI: `Key, Resize, Tick`. Agent (now **tagged with `ConversationId`** so background turns stream into the right buffer): `AgentStarted(id), AgentChunk(id, String), AgentDone(id, AgentResult), AgentError(id, String), BeginAssistantMessage(id), DiscardEmptyAssistantMessage(id), AddMessage(id, …)`. Tools: `ToolStarted, ToolDone, ToolError, RequestToolApproval, RequestQuestion`. Goal: `GoalEvaluationDone(GoalVerdict), GoalCycleFinished`. Sub-agent: `SpawnSubAgent { parent, label, prompt }`.

`handle_event` first calls `agent_event_target(&event)`; if the target id ≠ focused, it routes to `handle_background_event` (`app/multichat.rs`) which mutates that conversation's parked record instead of the focused chat. Rendering only ever shows the focused conversation.

## PRIMARY DATA FLOW (a normal turn)

```
key 'Enter' → handle_key (app/keys.rs)
  ├ input starts with '/' → CommandRegistry::execute → CommandResult
  └ else → expand @file mentions → focused_mut().messages.push(user) → send_to_agent (app/mod.rs)
              system_prompt::build(prompts, skills, tools, mcp, workspace)
              AgentRuntime::spawn(TurnSpec{ conversation: focused_id, provider, messages, … auto_approve:false })
                run_agent_loop → provider.complete_stream → SSE → CompletionChunk
                  → AppEvent::AgentChunk(id, delta) → handle_event → focused_mut().messages → render
                parse_tool_calls → per tool:
                  RequestToolApproval → modal → user Y/N → tools.execute()/mcp.call_tool()
                  → AppEvent::AddMessage(id, tool result)
                  task tool → fork + run_sub_agent (fg) | SpawnSubAgent event (bg)
                AppEvent::AgentDone(id, _) → record stats → auto_save_session()
```

## TOOL APPROVAL / QUESTION HANDSHAKE

Cross-task request/response uses `Arc<Mutex<Option<T>>> + tokio::Notify`:
- Agent task builds a `ToolApprovalRequest`/`QuestionRequest`, sends it as an `AppEvent`, then `await`s `.wait()`.
- Main loop shows the modal, captures the key, calls `.resolve(value)` which notifies the waiting agent task.
- **Consequence**: while a modal is open the agent task is parked — and the modal also blocks input handling (known limitation in BUGS.md).

## GOAL MODE (state machine) — `app/goal.rs`, `events.rs`

`GoalStage`: `Inactive → WaitForGoal → RunAgent1 → RunEvaluator → Done`.
1. `/goal` arms it; first user message = `goal_prompt`; second = `goal_text` (the success spec).
2. Agent 1 works a normal turn toward the goal.
3. On `AgentDone`, `run_goal_evaluation()` calls a **separate evaluator** (non-streaming `complete()`, lower temp) with `goal-evaluator.prompt.md`.
4. `parse_goal_verdict()` reads `**Status:** SUCCESS/FAILURE` + summary/issues/feedback.
   - SUCCESS → `Done`, evaluator session saved tagged `__goal_system__`.
   - FAILURE → feedback fed back to Agent 1; counters increment.
5. **Session swapping**: after **3** agent-1 failures → fresh agent-1 session; after **5** evaluator failures → fresh evaluator session. Two distinct session ids tracked (`goal_agent1_session_id`, `goal_agent2_session_id`).
6. ⚠ No hard iteration cap → potential infinite loop (see BUGS).

## AGENT-TURN LAUNCH (`AgentRuntime` + `TurnSpec`)

Every agent turn — normal, sidechat, or sub-agent — is described by a `TurnSpec { conversation, provider, messages, system_prompt, model, temperature, max_tokens, max_steps, max_tools_per_step, auto_approve }` and launched by `AgentRuntime::spawn(spec)` (`app/runtime.rs`), which `tokio::spawn`s `run_agent_loop(...)`. `auto_approve=false` for the focused user turn (interactive approval modal); `auto_approve=true` for background turns (sidechats/sub-agents) so they never block on a modal nobody is watching.

## SUB-AGENTS (`agent/sub_agent.rs`, `runner.rs`, `/agent` `/agents`)

Reference: Claude Code. Spawned by **the model** (a `task` tool call, special-cased in `run_agent_loop` like `question`) and by **the user** (`/agent <prompt>`).
- **Foreground (default)**: `run_agent_loop` forks the provider, awaits `run_sub_agent(...)` inline, and returns only its final text as the tool result — isolated context, just the conclusion crosses back.
- **Background (`background:true`)**: emits `AppEvent::SpawnSubAgent { parent, label, prompt }`, returns immediately; `App` spawns it as a `SubAgent` conversation (`spawn_sub_agent`), notifies + delivers the result into the parent on completion.
- Tracked/stoppable via `/agents` (picker) and the conversations store. Sub-agents auto-approve within their toolset and don't recursively spawn (depth-limited).

## `/btw` SIDECHAT + PARALLEL SESSIONS (`app/multichat.rs`, `/btw` `/new` `/chats`)

- **`/btw <q>`**: a one-shot `Sidechat` conversation with a forked provider + small step cap, spawned in the background (`spawn_sidechat`); its answer streams in without disturbing the main turn (events route by id).
- **`/new`**: open a new full `Session` conversation (forked provider, fresh disk session), focus it. **`/chats`**: picker to switch focus; **Tab / Ctrl** cycles (`cycle_focus`). Background conversations keep streaming into their own buffers; switching focus only changes what renders. Status bar shows count + how many are streaming.
- Esc/Ctrl+C abort targets the **focused** conversation only; on shutdown all conversations' tasks are aborted.

## PROVIDER FORKING (`LLMProvider::fork`)

`fork(&self) -> Arc<dyn LLMProvider>` (`provider/mod.rs:211`) returns a fresh-session sibling sharing config/token. `DeepseekProvider::fork` (`deepseek.rs:1712`) rebuilds from stored config with a new `SessionState` (`fork_session` at `:144`); `FakeProvider::fork` (`fake.rs:86`) returns a new instance. Unit-tested for independent session state (`deepseek.rs:1773`). This is the analog of poopseek's `provider.clone()` and the foundation that makes concurrent conversations safe.

## TWO RUN MODES (`main.rs`)

- **TUI** (default): full interactive app.
- **ACP server** (`--acp`): `acp::server::AcpServer` speaks ND-JSON JSON-RPC over stdio (`initialize`, `prompt`, `ping`) — lets an external client (IDE) drive the DeepSeek provider. Non-streaming `complete()` only; no tools.

## KEY INVARIANTS / PATTERNS

- **State is single-threaded**; only the main loop mutates `AppState`. Everything async talks through channels.
- **Async I/O everywhere** via Tokio; blocking PTY reads isolated on `spawn_blocking`.
- **`Arc<dyn Trait>`** for `LLMProvider` and `Tool` — swappable abstractions (though only one impl each today).
- **Errors** bubble via `AppResult<T>` + `?`; user-facing ones land in the status bar.
- **Assets resolution** (prompts, WASM): try `CARGO_MANIFEST_DIR` (dev) → CWD → exe-dir (release). Keep `assets/` next to the binary when distributing.
