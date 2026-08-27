# REFERENCE: Tool System & Background Processes
> How the agent acts on the world. Source: `src/tools/`, `src/agent/`.
> Last updated: 2026-06-30

## Tool TRAIT & REGISTRY

- **`Tool` trait** (`tools/mod.rs:78`): `fn definition() -> ToolDefinition` + `async fn execute(args: Value) -> ToolResult`.
- **`ToolDefinition`** (`tools/mod.rs:12`): `name, description, parameters(JSON schema)`.
- **`ToolResult`** (`tools/mod.rs:19`): `content: String, is_error: bool`. Helpers `::success()`, `::error()`.
- **`ToolRegistry`** (`tools/registry.rs:6`): `Mutex<HashMap<String, Arc<dyn Tool>>>` + optional skill tool. API: `register`, `get`, `definitions()`, `execute(name,args)` (returns "Unknown tool" error if missing), `update_skills()`.

### Built-in tools (7 default + `skill` dynamic) — `registry.rs:21`
`bash` · `powershell` · `question` · `shell_output` · `shell_kill` · `shell_list` · `shell_input` (+ `skill` registered via `update_skills`).

| Tool | Args | Returns | Notes |
|------|------|---------|-------|
| `bash` | `command`(req), `background`, `interactive`, `wait_seconds`(0–10, def 2), `persistent`, `ttl_seconds`(def 1800; 0=∞) | foreground: stdout/stderr; bg/interactive: `Job #{id}` + initial output | runs `bash -c`; Windows uses `CREATE_NO_WINDOW`/DETACHED (0x08) to protect TUI |
| `powershell` | same as bash | same | runs `powershell -NoProfile -Command` |
| `question` | `question`(req), `type`(yes_no\|multiple_choice), `options`, `allow_custom` | special-cased in agent loop (not via registry) | no approval prompt; opens a modal, waits for user |
| `shell_output` | `id`(req) | `Job #{id} · {status}\n{output}` | **destructive drain** — reads only new bytes since last call; removes job if finished |
| `shell_kill` | `id`(req) | `Stopped job #{id}…\nFinal output:…` | force-kill + remove |
| `shell_list` | — | formatted job table | prunes finished first; shows pid, kind, persist, age, idle, ttl |
| `shell_input` | `id`(req), `text`, `keys[]` | confirmation | interactive jobs only; `keys` → escape seqs (up/down/enter/esc/tab/ctrl+c…) |
| `skill` | `action`(list\|load), `name` | list or `# Skill: {name}\n{content}` | backed by `Arc<RwLock<Vec<SkillDefinition>>>` |

**Auto-detection heuristics** (`tools/mod.rs:41`):
- `looks_interactive_command()` → forces `interactive=true` for `bun/npm create`, `npm init`, `gh auth`.
- `looks_persistent_background_command()` → defaults `persistent=true` for dev servers (vite, next dev, cargo watch…).

## BACKGROUND PTY SYSTEM (`tools/background.rs`, ~744 lines)

The most intricate subsystem. Powers background + interactive shells.

- **`ProcessStatus`** (:14): `Running | Finished(Option<i32>)`.
- **`BackgroundHandle`** (:48): `id, pid?, command, shell, started_at, last_activity_at, buffer(Arc<Mutex<Vec<u8>>>), overflow(AtomicBool), status, cmd_tx, writer?(interactive), interactive, persistent, ttl_secs?`.
- **`BackgroundRegistry`** (:139): global `OnceLock<Mutex<…>>` with auto-incrementing `next_id` + `HashMap<u64, Arc<BackgroundHandle>>`.
- **Buffer cap**: `MAX_BUFFER_BYTES = 256 KiB` (:94); overflow sets a flag and appends a warning; dropped data is unrecoverable.
- **Output sanitizing** (`drain_output`): strips ANSI escapes (regex), normalizes `\r\n`/`\r` → `\n`.

### Spawn paths
- **`spawn_background`** (:224): pipe-based, stdin=null, stdout/stderr piped, `kill_on_drop`. Two reader tasks + a waiter task (`select!` on `child.wait()` vs `BgCmd::Kill`). Sleeps `capture_secs` then drains initial output.
- **`spawn_interactive`** (:386): **`portable_pty`** pseudo-terminal. Sets `TERM=xterm-256color`, `COLORTERM=truecolor`. Blocking reader (4 KiB chunks) + blocking waiter + async kill handler on `spawn_blocking`. Exposes a `StdinWriter` for `shell_input`.

### Lifecycle fns
`read_output` (:542) · `kill_process` (:600) · `write_input` (:578) · `list_processes` (:608) · `process_snapshots` (:619) · `prune_finished_processes` (:552) · `expire_persistent_idle_processes` (:660, idle > ttl) · `prune_jobs` (:693) · `shutdown_nonpersistent` (:699, on each user turn) · `shutdown_all` (:729, on app exit) · `force_kill_pid` (:110, Windows `taskkill /F /T`, POSIX `kill -9`).

**Foreground** bash/powershell store their PID in a global so `Esc`/`Ctrl+C` can kill the child (`app/mod.rs:33` `kill_foreground_child`).

## AGENT LOOP (`agent/runner.rs:9` `run_agent_loop`)

```
for step in 0..max_steps:                       # default max_steps_per_turn = 256
  BeginAssistantMessage
  request = system_prompt + messages, stream=true
  provider.complete_stream(request, tx)
  loop over chunks (idle timeout 120s, runner.rs:47):
      full_response += chunk
      stream_visible_text(full_response) → emit AgentChunk deltas (hides partial tool tags)
      break on finish_reason == "stop"
  tool_calls = parse_tool_calls(full_response)
  visible   = strip_tool_calls(full_response)
  if no tool_calls: push assistant msg, AgentEvent::Done, return
  push assistant(visible)
  for call in tool_calls.take(max_tools_per_step):   # default 10
      if name == "question": RequestQuestion → wait()      # no approval, opens modal
      elif name == "task":   fork provider + run_sub_agent (fg) | emit SpawnSubAgent (bg)   # special-cased like question
      else: RequestToolApproval → wait()      # auto-approved when TurnSpec.auto_approve (background turns)
          if approved:
              mcp__* → mcp.call_tool()
              else   → tools.execute()
          else: "Execution denied by user." (is_error)
      summarize_tool_result() (≤200 bytes, char-boundary-safe) → tool msg + AgentEvent::Message + AgentEvent::ToolDone/ToolError
# loop exhausted → AgentEvent::Failed("Reached max agent steps…")
```

- Launched via `AgentRuntime::spawn(TurnSpec)` (`app/runtime.rs`); the handle lives on the owning `Conversation` (`state.focused().agent_task`). `Esc` aborts the focused conversation's task.
- All emitted `AppEvent`s are tagged with the turn's `ConversationId` so background turns stream into the right buffer.
- The `task` tool (sub-agents) is special-cased here, not a `Tool` impl; the headless runner is `agent/sub_agent.rs::run_sub_agent`.
- `summarize_tool_result` (:219) truncates at `floor_char_boundary(200)` — UTF-8/emoji safe (tested).

## TOOL-CALL PARSING (`agent/tool_parser.rs`)

Three formats parsed from raw LLM text (DeepSeek web API has NO native function-calling):
1. **XML** (primary): `<tool_use><name>…</name><arguments>{json}</arguments></tool_use>`.
2. **XML+JSON**: `<tool_use>{"tool":…,"args":…}</tool_use>`.
3. **Legacy**: `[TOOL:name] {json}`.

- `strip_tool_calls()` removes `<tool_use>`, `<thinking>`, `[TOOL:…]` blocks.
- `stream_visible_text()` also cuts at the first bare `<` or partial marker — **gotcha**: truncates legit text containing `<` (e.g. C++ templates, `a < b`).
- Regexes are `LazyLock`-compiled. Has unit tests for all three formats.

## SKILLS as tools

`skill` tool can `list`/`load` skills at runtime. Skills discovered from many dirs (see `MCP.md`/`CONFIG.md` siblings and `skills/discovery.rs`); enabled ones are injected into the system prompt by `app::system_prompt::build(...)` (`app/system_prompt.rs`).

## SAFETY MODEL

- `bash`/`powershell` run **arbitrary** commands — no sandbox, no command allow/deny list. Trust boundary = the **tool-approval modal** + the `/whitelist` of auto-approved tools (`approved_tools` set).
- Approval currently **blocks the event loop** until the user answers (known limitation).
