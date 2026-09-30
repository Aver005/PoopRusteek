# REFERENCE: Tool System & Background Processes
> How the agent acts on the world. Source: `src/tools/`, `src/agent/`.
> Last updated: 2026-06-30

## Tool TRAIT & REGISTRY

- **`Tool` trait** (`tools/mod.rs:78`): `fn definition() -> ToolDefinition` + `async fn execute(args: Value) -> ToolResult`.
- **`ToolDefinition`** (`tools/mod.rs:12`): `name, description, parameters(JSON schema)`.
- **`ToolResult`** (`tools/mod.rs:19`): `content: String, is_error: bool`. Helpers `::success()`, `::error()`.
- **`ToolRegistry`** (`tools/registry.rs:6`): `Mutex<HashMap<String, Arc<dyn Tool>>>` + optional skill tool. API: `register`, `get`, `definitions()`, `execute(name,args)` (returns "Unknown tool" error if missing), `update_skills()`.

### Built-in tools (13 default + `skill` dynamic) — `registry.rs:register_default_tools`
`bash` · `powershell` · `question` · `task` · `timer` · `shell_output` · `shell_kill` · `shell_list` · `shell_input` · `read_file` · `edit` · `write` · `todo` (+ `skill` via `update_skills`, `tool_search`/`history_search` via `register_semantic_tools`).

| Tool | Args | Returns | Notes |
|------|------|---------|-------|
| `bash` | `command`(req), `background`, `interactive`, `wait_seconds`(0–10, def 2), `persistent`, `ttl_seconds`(def 1800; 0=∞) | foreground: stdout/stderr; bg/interactive: `Job #{id}` + initial output | runs `bash -c`; Windows uses `CREATE_NO_WINDOW`/DETACHED (0x08) to protect TUI |
| `powershell` | same as bash | same | runs `powershell -NoProfile -Command` |
| `question` | `question`(req), `type`(yes_no\|multiple_choice), `options`, `allow_custom` | special-cased in agent loop (not via registry) | no approval prompt; opens a modal, waits for user |
| `shell_output` | `id`(req) | `Job #{id} · {status}\n{output}` | **destructive drain** — reads only new bytes since last call; removes job if finished |
| `shell_kill` | `id`(req) | `Stopped job #{id}…\nFinal output:…` | force-kill + remove |
| `shell_list` | — | formatted job table | prunes finished first; shows pid, kind, persist, age, idle, ttl |
| `shell_input` | `id`(req), `text`, `keys[]` | confirmation | interactive jobs only; `keys` → escape seqs (up/down/enter/esc/tab/ctrl+c…) |
| `skill` | `action`(list\|load), `name` | list or `SkillDefinition::as_attached_file` (`[file name]: <slug>.md` / `[file content begin]` … `[file content end]`) | backed by `Arc<RwLock<Vec<SkillDefinition>>>` |
| `timer` | `action`(set\|list\|cancel, def `set`), `after`("20m") **or** `at`("18:30"), `note`(req for set), `wake`, `id`(cancel) | `Timer set — #3 — 2026-08-29 18:30 (in 3h 12m), wake: …` | special-cased in the agent loop (needs the conversation id); no approval prompt; refused when `auto_approve`. See DEFERRED TASKS below |
| `read_file` | `path`(req), `offset`(1-based line, def 1), `limit`(def 400) | `{path} (lines a-b of N)
{slice}` | expands `~`; escape hatch for the compaction ladder's file-path markers |
| `edit` | `path`(req), `old_string`(req), `new_string`(req), `replace_all` | `Edited {path} (N replacements)` + a `-`/`+` diff of the changed region | anchor is a **literal substring**, must be unique unless `replace_all`; strict UTF-8 (binary refused); follows symlinks and preserves permissions; aborts if the file changed since it was read; `replace_all` with >1 hit omits the diff body so a short anchor cannot dump the file into context |
| `todo` | `todos`(req): array of `{content, status}`, status one of `pending`/`in_progress`/`done` | `Plan (N done, N in progress, N pending, N total)` + a `[x]`/`[>]`/`[ ]` row per item | **stateless** — the plan lives in the history as the tool result, so parallel conversations and sub-agents never share one. No approval prompt (`requires_approval() == false`) and shown whole in the chat (`result_is_its_own_summary() == true`). Caps: 30 items, 200 bytes per `content`; whitespace runs (newlines included) collapse so an item cannot forge a checklist row. Status matching is case/dash tolerant. At most one `in_progress`; an empty list is refused |
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

## TOOL-CALL PARSING (`agent/tool_parser/`)

DeepSeek web has no native function calling: the model writes calls as text.
Every format that vLLM `ccfd1cea` and sglang `6aacca2d` parse is read, mixed
freely in one reply, calls run in text order. Plan, decisions and deliberate
deviations: `.docs/tool-call-formats.md`. Entry points: `parse_step(raw,
native, &ParseCtx)` (native calls win; text parsing is the fallback) and
`parse_text`; result `ParsedReply { calls, errors, visible, suspect }`.

**One pass, left to right (`scan.rs`).** Tokens: format openers (built from
`formats::families()`), code fences, inline code, `<thinking>`/`<think>` and a
bare closer.
- One opener can belong to several formats (`<tool_call>`: Hermes JSON, GLM
  key-value, Qwen3-Coder `<function=`; `<function_calls>`: Claude-like `<invoke>`
  and Olmo 3). `Scanner::parse_at` tries every family whose opener matches there,
  in table order; each grammar returns `None` when the body is not its shape.
- A finding goes through acceptance at once; one with nothing left is prose.
- Nothing inside a finding is scanned: a call inside a `write` argument is data.
  An unclosed call that already has a body is an error spanning to the end —
  nothing after it runs.
- Fence: its calls are taken only if the fence holds nothing but calls. If a call
  is cut exactly at a ``` line, a second pass scans through (a DSML `write` of
  markdown carries its own fences). An unclosed fence with text around a call is
  not a fence (the prompt says "stop after the call", so fences stay open).
- Inline code is prose — unless its closing backtick sits inside a call
  (PowerShell escape char).
- Reasoning is never parsed; an unclosed block ends at the first real call; a call
  written only inside reasoning is a diagnostic.
- Weak formats (pythonic list, bare JSON) count only when the whole visible reply
  is calls and there are no strong findings (`formats::whole_reply`).

**Acceptance (`accept.rs`, `catalog.rs`).** `ToolCatalog` = builtin + MCP names
and schemas, snapshotted once per turn. Name: exact; else case/`-`/`_` fold if the
match is unique; `mcp__…` is never folded. Values: `Declared::Text` as is,
`Json` = JSON with string fallback, `BySchema` = non-string only when the schema
explicitly allows it (`"007"` stays a string). A lone `arguments`/`input` param
absent from the schema is unwrapped. Adjacent identical call in a different
format is dropped (DSML echo of a `<tool_use>`).

**Trust.** Trusted: `tool_use`, `legacy`, `dsml`, `deepseek_v3`, `deepseek_v31`
(`accept::TRUSTED`). Anything else, or a renamed call, gets
`CallOrigin::needs_person()`: the approval modal always shows
(`ToolApprovalRequest::always_ask` bypasses the whitelist; the modal says why),
including `task` and `timer`; unattended (auto-approve, sub-agent) → refused with
"re-send as `<tool_use>`". Untrusted markup copied verbatim from a tool output is
refused. An untrusted call to an unknown tool is prose (someone's example).

**Residue** (`accept::residue`): a catalog tool name inside call-like markup that
no grammar took → `ParsedReply::suspect` → one retry (own budget), or a line in
the beside-calls note when other calls ran.

**Loop.** Parse errors beside good calls: the calls run, the errors go back as a
system note (`### NOTE`) listing the handled calls; no retry budget is spent.
`RetryBudget::reset` after a step that ran tools.

**Display.** Each step ends with `EndAssistantMessage { text }` — the parsed
visible text — and `reduce` replaces what streamed. The stream (`visible.rs`)
still cuts at the first `<` and at `[TOOL:`/`[TOOL_CALLS`/`functools[`, and holds
back a reply starting with `[` or `{`.

**DeepSeek reasoning.** The provider holds `THINK` fragments and emits them as a
leading `<thinking>` block when the answer fragment starts; no answer fragment →
the held text is the answer (`deepseek::stream::ThinkRouter`). Not verified live.

**Formats** (`formats/`, format id in parentheses):
- `tool_use.rs` — `<tool_use>` (`tool_use`, all old tolerances; JSON end found by
  parsing), `[TOOL:name] {json}` (`legacy`, nested objects).
- `invoke.rs` — invoke/parameter dialects: DSML V3.2/V4/V4.1 (`dsml`; wrappers
  `function_calls`/`tool_calls`/`calls`/`toolcalls`/`tool`, self-closing invoke,
  JSON body, strings never trimmed), Claude-like / MiniMax M2 / dots (`invoke`),
  GigaChat 3.5 (`gcml`), Step3 inner (`step3`).
- `tokens.rs` — DeepSeek V3/R1 (`deepseek_v3`), V3.1 (`deepseek_v31`), Step3 outer.
- `json_wrap.rs` — marker + JSON: Hermes/Qwen2.5/Granite/Ernie, Longcat, Jamba,
  Hunyuan-A13B, granite-20b, Cohere, Inkling, InternLM, Apertus, Mistral (3
  forms), Phi-4 mini, GigaChat 3, dots JSON, Llama `<|python_tag|>`, OpenAI
  `tool_calls` object.
- `tagged.rs` — key-value (GLM 4.5/4.7, Ling3, Spark, Poolside; Hunyuan/Hy3/Hy4;
  K2 Horizon) and `function=` (Qwen3-Coder, Seed-OSS, MiMo, Step 3.5, Nemotron;
  MiniCPM5 `<function name=…>`).
- `kimi.rs` — Kimi K2, K3. `gemma.rs` — Gemma 4, FunctionGemma.
  `minimax_m3.rs` — MiniMax M3. `channels.rs` — Harmony (gpt-oss), Muse Glimmer.
- `weak.rs` — pythonic sub-grammar; LFM2, Olmo 3, Llama 4 (strong, with markers);
  `whole_reply` for bare pythonic / bare JSON.

Adding a format: a grammar file with `families()` + one line in
`formats::families()`; its `parse` must return `None` for bodies that are not its
shape. The contract written for the grammar authors is summarised above.

## SKILLS as tools

`skill` tool can `list`/`load` skills at runtime. Skills discovered from many dirs (see `MCP.md`/`CONFIG.md` siblings and `skills/discovery.rs`); enabled ones are injected into the system prompt by `app::system_prompt::build(...)` (`app/system_prompt.rs`).

## SAFETY MODEL

- `bash`/`powershell` run **arbitrary** commands — no sandbox, no command allow/deny list. Trust boundary = the **tool-approval modal** + the `/whitelist` rules of auto-approved calls (`approved_tools`, a `whitelist::Whitelist`).
- Approval currently **blocks the event loop** until the user answers (known limitation).
- **Whitelist rules carry a scope** (`src/whitelist.rs`): `Rule { tool, scope: Option<Scope> }`, where `Scope` is either `Command(["cargo","test"])` or `Path(<absolute dir>)`. A rule with no scope means the whole tool. Commands compare word-by-word, paths component-by-component (case-insensitively on Windows); the two kinds never match each other. Rules are persisted (`whitelist::persist`) and survive restarts; there is no expiry.
- **A compound command gets no scope at all.** `approval_scope` returns `None` the moment the command contains `; & | 
 
 ` $ ( ) { } < >` — `git status && rm -rf /` starts with `git status` and does something else entirely, and `$(...)` runs *before* the "allowed" command. The only permanent approval available for such a call is tool-wide, shown as `⚠ … · ANYTHING`. Proper sub-command parsing (Claude Code splits on `&&`/`||`/`;`/`|`, strips `timeout`/`env` wrappers and treats redirect targets as file writes) is the right answer and is an open item in `BUGS.md`.
- **File scopes are absolute and normalised** through `safe_write::resolve_for_compare`; a raw `src/app/../../../etc/passwd` used to pass a rule scoped to `src/app`. A volume root is never a scope (one click would hand over the whole drive), and a file in the workspace root scopes to the workspace.
- **There is no deny list**, no project-level rules, and no sandbox. With no sandbox, a rule is a convenience, **not a security boundary** — it decides when to ask, not what is possible. Codex separates those two axes; this does not.
- **`edit`/`write` get no approval at all under `auto_approve`** — background sub-agents (`multichat.rs`) and `sub_agent.rs` (which calls `dispatch_generic_tool` directly). The policy is inherited from `bash` and is not new, but the blast radius grew: `write` is far easier for a model to reach for than a quoted heredoc. Deliberately left as-is; restricting it would break legitimate sub-agent refactors.
- `edit`/`write` **do** hard-refuse two path classes regardless of approval (`safe_write::refuse_protected`, shared with the undo path): this agent's own `Config::data_dir()`/config directory, and any `mcp.config.json`/`mcp.json`. The second is not cosmetic — an MCP config is executed as a child process on the next start, so an approved "write a json file" would otherwise have been arbitrary deferred code execution.
- Path comparison resolves the nearest existing ancestor before comparing, and compares component-wise — a plain `canonicalize` left files that do not exist yet outside the guard entirely, which on a fresh install meant the model could write its own `whitelist.json`. There is **no workspace jail**. Writes outside the working directory are allowed but flagged in the approval modal (`⚠ OUTSIDE WORKSPACE`, `tools/mod.rs:outside_workspace_note`).
- **Every successful `edit`/`write` leaves a snapshot** (`src/checkpoints.rs`), so `/undo` can put the file back. The snapshot is taken **after** the write, not before — before, a refused write to a protected path still copied that file's content into the checkpoint store. Files that look like secrets (`.env`, `*.pem`, `id_rsa`…) and files over 8 MiB are recorded as `Before::Skipped` and are **not** copied anywhere; `/undo` refuses them by name rather than silently skipping to an older entry.
- The approval modal renders `tools::approval_preview`, **not** raw pretty-JSON: `serde_json` escapes newlines, which collapsed a 500-line `write` into one unreadable line the popup could neither grow to fit nor scroll.
