# LEARNINGS
> Hard-won technical knowledge. Gotchas. Patterns. (Deep detail lives in `reference/`.)
> Last updated: 2026-06-30 (added refactor + conversation/fork learnings)

## CI И ЛОКАЛЬНЫЕ ПРОВЕРКИ

- **Локальный pre-commit не равен CI, даже когда команда буквально та же.** Хук и
  CI оба зовут `cargo clippy --bin pooprusteek -- -D warnings`, и всё равно CI
  падал восемь коммитов подряд, а хук был зелёным. Две причины, обе про среду,
  не про команду:
  1. **Версия тулчейна.** CI берёт `dtolnay/rust-toolchain@stable`, то есть
     *свежайший* stable. Локально стоял 1.96.0, в CI приезжал 1.98.0 — два
     релиза новых линтов, которых локальный clippy просто не знает. Так и
     всплыли `chunks_exact_to_as_chunks` и `result_large_err`.
  2. **Платформа.** Джоба `lint` крутится только на `ubuntu-latest`, а
     разработка идёт на Windows. Код под `#[cfg(unix)]` линтуется лишь в CI,
     код под `#[cfg(windows)]` — лишь локально. Дыра в обе стороны.
- **Проверять — установкой той же версии, а не рассуждением.**
  `rustup toolchain install <ver> --component clippy --profile minimal` и
  `cargo +<ver> clippy` воспроизводят падение CI за минуту, не трогая дефолтный
  тулчейн. `rustup check` показывает разрыв между локальным stable и свежим.
- **Логи упавшей джобы через API не скачать без прав админа** (`403`), а вот
  список прогонов и статусы шагов — открыты:
  `/repos/<o>/<r>/actions/runs` и `/actions/runs/<id>/jobs`. Их хватает, чтобы
  понять *какой* шаг и *с каких пор* красный.

## ARCHITECTURE & REFACTORING  (→ `ARCHITECTURE.md`)

- **Organizational ≠ architectural.** Splitting a god-file into more files is not a clean refactor by itself. The wins here came from (a) grouping data clumps into cohesive structs and (b) extracting **controllers** that own *dependencies* and expose narrow APIs — so methods stop taking `&mut self` (all of `App`) just to touch three fields.
- **Refactor in tiny verified increments.** One cluster at a time; `cargo build` + `cargo test` green after each step. Compiler-driven migration: after removing/renaming a field, let errors enumerate the call sites. Use anchored per-file `sed`/`perl` (read-only files → `focused()`, `&mut` files → `focused_mut()`); watch for multi-line expressions a line-based sed misses (`perl -0777`).
- **Borrow-conflict patterns when narrowing.** Disjoint fields of `self` *can* be borrowed together in one expression (`self.state.mcp_status.refresh_view(&self.mcp)` — `state` mut + `mcp` shared). For sequential conflicts, bind reads into locals first, then mutate; or add `push_message`/`push_system` helpers on the sub-state to avoid holding a borrow across a push.
- **`Arc` + deref coercion** lets a narrow fn signature (`&ToolRegistry`, `&Mutex<MCPManager>`) accept `&self.tools` / `&self.mcp` (which are `&Arc<…>`) without explicit deref.
- **Functional core / imperative shell**: `goal::apply_verdict` is a pure function (testable, no I/O); the `impl App` goal methods are the thin shell around it.

## CONVERSATIONS & PROVIDER FORK  (→ `ARCHITECTURE.md`, `reference/PROVIDER.md`)

- **`parent_message_id` desync = "evaporating messages".** DeepSeek's web session is a tree keyed by `parent_message_id`; a stale id silently forks onto an *invisible* branch — the UI shows the message but the web view (and the model's context) never sees it. An interrupted/errored stream was the trigger. Fix: incrementally persist `parent_message_id` and flush-on-error (`deepseek.rs`, commit `183712e`).
- **Structural prevention**: give each `Conversation` its own forked provider (fresh `SessionState`). Concurrent turns then can't collide on shared `session_state`. `fork()` is the cornerstone that makes parallel sessions / sidechats / sub-agents safe.
- **Event tagging is mandatory for concurrency**: agent events carry a `ConversationId`; `handle_event` → `on_agent_event` → `app::reduce::apply` применяет их к нужной беседе. Without the tag, a background turn's chunks would append to whatever's focused.
- **Одно и то же событие применялось в трёх местах** (фокус, фон, харнесс) и они успели разойтись; свели в `app/reduce.rs` 2026-08-27. Появится четвёртый потребитель — звать редьюсер, а не писать свой `match`.

## DEEPSEEK API  (full detail → `reference/PROVIDER.md`)

| Topic | Detail |
|-------|--------|
| Auth | DeepSeek **web** API: cookie/token session + spoofed Android client headers. NOT the public API-key product. |
| PoW | Every gated call needs a solved SHA-3 challenge in `x-ds-pow-response`. Solved by bundled WASM via `wasmtime`. `→ src/provider/pow.rs` |
| Prompt shape | Web API takes ONE `prompt` string; history is flattened with `### USER INPUT` / `### TOOL RESULT` / `### LOCAL MEMORY` labels. |
| `### LOCAL MEMORY` | Just a history-section label — does **NOT** load `.memories/` files. Don't conflate. |
| SSE | Mixed named (`ready/update_session/title/close`) + unnamed (`o`/`p`/`v` op-path-value) events; ~9 fallback text-extraction paths. |
| Usage | Token usage never returned by streaming → `usage` is always None; counts are estimated `len/4`. |
| Fragility | Undocumented, reverse-engineered; client version `1.8.0` hardcoded. May break anytime. |

## CONFIG / DEFAULTS  (→ `reference/CONFIG.md`)

- **Correct agent defaults**: `max_steps_per_turn = 256`, `max_tools_per_step = 10`, `max_context_messages = 256`. (Older memory wrongly said 25 / 50.)
- Paths use the `dirs` crate → platform-specific. Config = `{config_dir}/pooprusteek/config.toml`; data (sessions, history, mcp.json) = `{data_dir}/pooprusteek/`.
- Sessions are JSON; ids are time-sortable (`{rfc3339}-{uuid8}`). History capped at 500.

## RUST PATTERNS  (→ `CONVENTIONS.md`)

| Topic | Detail |
|-------|--------|
| Errors | `AppError` (`thiserror`) + `AppResult<T>` everywhere; `color-eyre` at the top level. |
| Async | Tokio; `LLMProvider`/`Tool` are `#[async_trait]`. Blocking PTY work on `spawn_blocking`. |
| Event loop | One `tokio::select!` @120ms over tick / crossterm / internal channel / Ctrl+C. Agent runs in a spawned task. |
| State | Only the main loop mutates `AppState`; async tasks talk via `AppEvent` + `Notify` handshakes. |
| Cross-task req/resp | `Arc<Mutex<Option<T>>> + tokio::Notify` (tool approval, questions). |
| Assets | Resolve via `CARGO_MANIFEST_DIR` → CWD → exe-dir (prompts, PoW WASM). |

## AGENT LOOP  (→ `reference/TOOLS.md`)

- Tool calls parsed from raw LLM text — 3 formats: XML `<tool_use>`, legacy `[TOOL:name]`, JSON. No native function-calling.
- Loop: up to `max_steps` steps, `max_tools_per_step` tools each. 120s idle stream timeout.
- `question` tool is **special-cased** (opens a modal, no approval); all other tools go through approval.
- `summarize_tool_result` truncates at 200 bytes on a **char boundary** (emoji-safe; tested).
- `stream_visible_text` hides partial tool tags during streaming — but over-eagerly cuts at any `<`.

## BACKGROUND / PTY  (→ `reference/TOOLS.md`)

- `tools/background.rs` runs detached (`spawn_background`, piped) or interactive (`spawn_interactive`, real PTY via `portable-pty`).
- Persistent jobs (dev servers) survive turns; idle TTL default 1800s. Non-persistent killed each user turn; all killed on exit.
- `shell_input` maps named keys to terminal escape sequences (up/down/enter/esc/ctrl+c…).
- Windows: foreground processes use `CREATE_NO_WINDOW`/DETACHED to avoid corrupting the TUI; force-kill via `taskkill /F /T`.

## MCP  (→ `reference/MCP.md`)

- Tool names prefixed `mcp__{server}__{tool}`. Protocol version `2024-11-05`.
- Three transports: stdio (spawn+stdin/stdout, 60s), HTTP (30s, `MCP-Session-Id`, SSE-fallback), SSE.
- Config auto-discovered from **8 sources** (own → workspace → global → Claude Desktop → VSCode → Claude CLI → Cursor → Opencode), first-found-wins. (Older memory said 5.)
- Tool lists cached per-server; TTL default 300s, set via `/mcp ttl`.
- Windows stdio retries `.cmd`/`.bat` so `npx` resolves.

## GOAL MODE  (→ `ARCHITECTURE.md`)

- `/goal` arms it. Flow: prompt → define goal → Agent 1 works → Evaluator (non-streaming) judges → SUCCESS or feedback-retry.
- Evaluator uses `goal-evaluator.prompt.md`, structured `**Status:** SUCCESS/FAILURE`.
- Session swap: 3 agent-1 failures → fresh worker session; 5 evaluator failures → fresh evaluator session.
- Evaluator sessions tagged `__goal_system__` (hidden from `/sessions`). ⚠ No hard iteration cap yet.

## META: THE `.memories` SYSTEM

- This folder is the canonical agent onboarding doc. **The app does not read it** — point agents at `.memories/INDEX.md`.
- Keep `→ file:line` references; when memory and code disagree, the code wins.

## BUILD
- `lto="fat"`, `codegen-units=1`, `strip=true` (release). `wasmtime` for PoW, `portable-pty` for shells, `syntect`+`pulldown-cmark` for rendering.

- **CI lints on a moving stable — keep the local toolchain current.**
  `dtolnay/rust-toolchain@stable` in `ci.yml` resolves to the *latest*
  stable on every run, so each ~6-week Rust release can promote new clippy
  lints that a stale local toolchain never shows: local gates green, CI
  `lint` red (first hit 2026-07-15: 1.97's `unneeded_wildcard_pattern` on
  `{ result: _, .. }` while local was 1.96). After a Rust release day, run
  `rustup update stable` before trusting a local `clippy -D warnings` run;
  the `.githooks/pre-commit` hook uses the local toolchain too, so it
  inherits the same blind spot until updated.
