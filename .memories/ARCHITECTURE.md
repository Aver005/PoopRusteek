# ARCHITECTURE
> How the pieces fit and how data flows. Read after MAP.md.
> Last updated: 2026-06-30

## LAYERS (clean-ish, layered architecture)

```
Presentation   tui/ (render, widgets, theme, markdown)
Application     app/ (App + AppState, event loop, key handling), config/, session.rs
Domain          agent/ (loop, tool parser), provider/ (LLMProvider, DeepSeek), tools/
Infrastructure  mcp/ (clients, transports, discovery), acp/ (server mode), skills/
```

`.docs/architecture.md` describes the *intended* design and is partly **aspirational** (it names `AgentLoop`/`ContextManager`/`StreamingResponse` structs that don't exist — the real thing is the free function `run_agent_loop`). Trust the code + this folder over `.docs/`.

## THE CENTRAL OBJECT: `App` (`app/mod.rs:53`)

`App` owns everything: `config`, `state: AppState`, the event channel (`event_tx/rx`), `provider`, `commands`, `mcp` (`Arc<Mutex<MCPManager>>`), `tools` (`Arc<ToolRegistry>`), `prompts`, `skills`, and `agent_task`. `app/mod.rs` is the ~2.4k-line god-file — most behavior lives here.

## EVENT LOOP (`App::run_loop`, `app/mod.rs:256`)

A single `tokio::select!` at **120 ms tick** multiplexes:
1. **Tick** → animations, periodic MCP/background stat refresh (MCP every 2s).
2. **Crossterm** key/resize (`EventStream`) → `handle_key` / `handle_event`.
3. **Internal channel** (`event_rx`) → `handle_event` for `AppEvent`s emitted by the agent/tools.
4. **Ctrl+C** → kill foreground child, shutdown background jobs, exit.
After each iteration: refresh MCP view if active, handle terminal-restore flag, `render()`.

This is the "event-driven, no render races" design from the README: the agent runs in a **spawned task** and communicates only via `AppEvent`s + channels, never touching `AppState` directly.

## `AppEvent` (`app/events.rs:86`)

TUI: `Key, Resize, Tick`. Agent: `AgentStarted, AgentChunk(String), AgentDone(AgentResult), AgentError(String), BeginAssistantMessage, DiscardEmptyAssistantMessage, AddMessage`. Tools: `ToolStarted, ToolDone, ToolError, RequestToolApproval, RequestQuestion`. Goal: `GoalEvaluationDone(GoalVerdict), GoalCycleFinished`.

## PRIMARY DATA FLOW (a normal turn)

```
key 'Enter' → handle_key (app/mod.rs:~849)
  ├ input starts with '/' → CommandRegistry::execute → CommandResult
  └ else → expand @file mentions → push user msg → send_to_agent (app/mod.rs:1552)
              spawn run_agent_loop(provider, tools, mcp, messages, system_prompt, …)
                provider.complete_stream → SSE → CompletionChunk
                  → AppEvent::AgentChunk(delta) → handle_event → AppState.messages → render
                parse_tool_calls → per tool:
                  RequestToolApproval → modal → user Y/N → tools.execute()/mcp.call_tool()
                  → AppEvent::AddMessage(tool result)
                AppEvent::AgentDone → auto_save_session()
```

## TOOL APPROVAL / QUESTION HANDSHAKE

Cross-task request/response uses `Arc<Mutex<Option<T>>> + tokio::Notify`:
- Agent task builds a `ToolApprovalRequest`/`QuestionRequest`, sends it as an `AppEvent`, then `await`s `.wait()`.
- Main loop shows the modal, captures the key, calls `.resolve(value)` which notifies the waiting agent task.
- **Consequence**: while a modal is open the agent task is parked — and the modal also blocks input handling (known limitation in BUGS.md).

## GOAL MODE (state machine) — `app/mod.rs:1596`+, `events.rs:69`

`GoalStage`: `Inactive → WaitForGoal → RunAgent1 → RunEvaluator → Done`.
1. `/goal` arms it; first user message = `goal_prompt`; second = `goal_text` (the success spec).
2. Agent 1 works a normal turn toward the goal.
3. On `AgentDone`, `run_goal_evaluation()` calls a **separate evaluator** (non-streaming `complete()`, lower temp) with `goal-evaluator.prompt.md`.
4. `parse_goal_verdict()` reads `**Status:** SUCCESS/FAILURE` + summary/issues/feedback.
   - SUCCESS → `Done`, evaluator session saved tagged `__goal_system__`.
   - FAILURE → feedback fed back to Agent 1; counters increment.
5. **Session swapping**: after **3** agent-1 failures → fresh agent-1 session; after **5** evaluator failures → fresh evaluator session. Two distinct session ids tracked (`goal_agent1_session_id`, `goal_agent2_session_id`).
6. ⚠ No hard iteration cap → potential infinite loop (see BUGS).

## TWO RUN MODES (`main.rs`)

- **TUI** (default): full interactive app.
- **ACP server** (`--acp`): `acp::server::AcpServer` speaks ND-JSON JSON-RPC over stdio (`initialize`, `prompt`, `ping`) — lets an external client (IDE) drive the DeepSeek provider. Non-streaming `complete()` only; no tools.

## KEY INVARIANTS / PATTERNS

- **State is single-threaded**; only the main loop mutates `AppState`. Everything async talks through channels.
- **Async I/O everywhere** via Tokio; blocking PTY reads isolated on `spawn_blocking`.
- **`Arc<dyn Trait>`** for `LLMProvider` and `Tool` — swappable abstractions (though only one impl each today).
- **Errors** bubble via `AppResult<T>` + `?`; user-facing ones land in the status bar.
- **Assets resolution** (prompts, WASM): try `CARGO_MANIFEST_DIR` (dev) → CWD → exe-dir (release). Keep `assets/` next to the binary when distributing.
