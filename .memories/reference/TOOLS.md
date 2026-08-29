# REFERENCE: Tool System & Background Processes
> How the agent acts on the world. Source: `src/tools/`, `src/agent/`.
> Last updated: 2026-06-30

## Tool TRAIT & REGISTRY

- **`Tool` trait** (`tools/mod.rs:78`): `fn definition() -> ToolDefinition` + `async fn execute(args: Value) -> ToolResult`.
- **`ToolDefinition`** (`tools/mod.rs:12`): `name, description, parameters(JSON schema)`.
- **`ToolResult`** (`tools/mod.rs:19`): `content: String, is_error: bool`. Helpers `::success()`, `::error()`.
- **`ToolRegistry`** (`tools/registry.rs:6`): `Mutex<HashMap<String, Arc<dyn Tool>>>` + optional skill tool. API: `register`, `get`, `definitions()`, `execute(name,args)` (returns "Unknown tool" error if missing), `update_skills()`.

### Built-in tools (12 default + `skill` dynamic) — `registry.rs:register_default_tools`
`bash` · `powershell` · `question` · `task` · `timer` · `shell_output` · `shell_kill` · `shell_list` · `shell_input` · `read_file` · `edit` · `write` (+ `skill` via `update_skills`, `tool_search`/`history_search` via `register_semantic_tools`).

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
| `timer` | `action`(set\|list\|cancel, def `set`), `after`("20m") **or** `at`("18:30"), `note`(req for set), `wake`, `id`(cancel) | `Timer set — #3 — 2026-08-29 18:30 (in 3h 12m), wake: …` | special-cased in the agent loop (needs the conversation id); no approval prompt; refused when `auto_approve`. See DEFERRED TASKS below |
| `read_file` | `path`(req), `offset`(1-based line, def 1), `limit`(def 400) | `{path} (lines a-b of N)
{slice}` | expands `~`; escape hatch for the compaction ladder's file-path markers |
| `edit` | `path`(req), `old_string`(req), `new_string`(req), `replace_all` | `Edited {path} (N replacements)` + a `-`/`+` diff of the changed region | anchor is a **literal substring**, must be unique unless `replace_all`; strict UTF-8 (binary refused); follows symlinks and preserves permissions; aborts if the file changed since it was read; `replace_all` with >1 hit omits the diff body so a short anchor cannot dump the file into context |
| `write` | `path`(req), `content`(req) | `Created`/`Overwrote {path} (…lines, …bytes)` | creates parent dirs; cannot append; refuses this agent's own config dir and any MCP config file |

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

## DEFERRED TASKS — `timer` (`tools/timer.rs`, `app/timers.rs`)

Отложенная задача = запись в `TimerStore`, которым владеет `ToolRegistry`
(`registry.timers()`); тот же хэндл читает `App`. Взвод — четвёртая ветка
`tools_step::execute_tool_call` → `manage_timer` (инструменту нужна беседа,
через реестр её не получить). Срабатывание — `App::fire_due_timers` на тике
(120 мс): `take_due(now)` изымает созревшие, чистая
`app::timers::route(timer, owner_alive, owner_busy)` решает исход.

| Исход | Когда | Что делает |
|---|---|---|
| `Orphan` | беседы-владельца нет | `ui_system` в фокусный чат, беседа не воскрешается |
| `Notify` | `wake=false`, либо бюджет побудок исчерпан, либо чат занят > 5 мин | `ui_system` в беседу-владельца (+ короткая строка в фокусный, если человек смотрит в другую) — до модели **не** доходит |
| `Defer` | `wake=true`, в беседе идёт ход | таймер возвращается на +5 с, до 60 раз |
| `Wake` | `wake=true`, беседа свободна | `user_with_display` («⏰ Timer #N fired — automatic, not a message from the user») + `App::send_turn(owner, …)` |

Роль побудки — `User`, не `System`: `provider/prompt.rs:format_tail_message`
рендерит system-хвост как `### NOTE`, что для «сделай сейчас» слабо.

**Предохранители:** фоновый ход (`auto_approve`) таймеры ставить не может;
10 s ≤ задержка ≤ 24 h; ≤ 8 таймеров на беседу; заметка ≤ 400 байт;
≤ 3 побудок подряд без реплики человека (`take_wake_slot` / `reset_wakes`,
сброс — в `keys/chat.rs` на отправке сообщения). Таймеры снимаются в
`stop_background` / `finish_background`.

**Границы:** персистентности нет — перезапуск стирает всё (сознательно, см.
`JOURNAL/2026-08-29-timers.md`). Тик есть только у TUI, поэтому в
`pooprusteek exec` таймер взводится, но никогда не стреляет; `--acp` и
`/serve` цикл агента не гоняют вовсе. Человеку — `/timers`, `/timers cancel <id>`
и счётчик `⏰:N` в статус-баре.

## AGENT LOOP (`agent/runner.rs` `run_agent_loop` — шаги; `agent/tools_step.rs` — вызовы инструментов)

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
- The `task` tool (sub-agents) is special-cased in `agent/tools_step.rs::spawn_task`, not a `Tool` impl; the headless runner is `agent/sub_agent.rs::run_sub_agent`.
- `summarize_tool_result` (`agent/tools_step.rs`) truncates at `floor_char_boundary(200)` — UTF-8/emoji safe (tested).

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
- **The whitelist keys on the tool NAME only** — no arguments, no paths, no expiry, and `whitelist::persist_approval` writes it to disk, so it survives restarts. Whitelisting `edit`/`write` therefore grants unlimited, permanent writes to any path the process can reach; it removes the only screen on which the human ever sees `path`. Granular, pattern-based permissions are an open gap (see the competitor comparison in `PLANS.md`).
- **`edit`/`write` get no approval at all under `auto_approve`** — background sub-agents (`multichat.rs`) and `sub_agent.rs` (which calls `dispatch_generic_tool` directly). The policy is inherited from `bash` and is not new, but the blast radius grew: `write` is far easier for a model to reach for than a quoted heredoc. Deliberately left as-is; restricting it would break legitimate sub-agent refactors.
- `edit`/`write` **do** hard-refuse two path classes regardless of approval (`tools/edit.rs:refuse_protected_path`): this agent's own `Config::data_dir()`/config directory, and any `mcp.config.json`/`mcp.json`. The second is not cosmetic — an MCP config is executed as a child process on the next start, so an approved "write a json file" would otherwise have been arbitrary deferred code execution.
- There is **no workspace jail**. Writes outside the working directory are allowed but flagged in the approval modal (`⚠ OUTSIDE WORKSPACE`, `tools/mod.rs:outside_workspace_note`).
- The approval modal renders `tools::approval_preview`, **not** raw pretty-JSON: `serde_json` escapes newlines, which collapsed a 500-line `write` into one unreadable line the popup could neither grow to fit nor scroll.
