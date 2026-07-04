# CONVENTIONS
> Code style & patterns to follow when contributing. Match the surrounding code.
> Last updated: 2026-07-02 (MSRV 1.85→1.91)

## LANGUAGE & EDITION
- Rust **edition 2024**, MSRV **1.91**. Use modern idioms (`let-else`, `LazyLock`, `floor_char_boundary`, etc.).

## ERROR HANDLING
- Use `AppResult<T>` + `AppError` (`src/error.rs`) for fallible fns; propagate with `?`.
- `color-eyre` installed at startup for rich top-level reports.
- Pick the right `AppError` variant (`Provider`, `Mcp`, `Config`, `SessionNotFound`, …); fall back to `Custom(String)`.
- Hardcoded regexes use `.expect("hardcoded regex is valid")` — acceptable for compile-time-constant patterns.

## ASYNC
- Tokio everywhere. `LLMProvider` and `Tool` are `#[async_trait]`.
- Never block the event loop. Offload blocking work (PTY reads, sync waits) to `tokio::task::spawn_blocking`.
- Cross-task signalling uses `mpsc` channels and `Arc<Mutex<Option<T>>> + Notify` (see tool approval).

## STATE & CONCURRENCY
- Only the main loop mutates `AppState`. Async tasks communicate via `AppEvent`s, not shared mutable state.
- Shared services are `Arc<Mutex<…>>` (`MCPManager`) or `Arc<…>` (`ToolRegistry`, provider).
- Global singletons (background registry, foreground-PID slot) use `OnceLock`/atomics.
- **Per-conversation state** lives in a `Conversation` (`app/conversation.rs`), never globally on `AppState`. Reach the active one via `state.focused()/focused_mut()`; touch others only through the `Conversations` store API. Each conversation has its **own forked provider** (`LLMProvider::fork()`).
- **Agent events must carry a `ConversationId`** so background turns stream into the right buffer; route non-focused events through `handle_background_event`.
- **Launch every agent turn via `AgentRuntime::spawn(TurnSpec)`** — don't call `run_agent_loop` directly. Set `auto_approve:true` for background turns (sidechats/sub-agents).

## MODULE / DECOMPOSITION PATTERN (how the god-object was tamed — keep it this way)
- Prefer **cohesive sub-state structs** over loose fields on `AppState`: group fields that are always read/written together into a module struct (`GenerationState`, `InputState`, `McpStatus`, `BackgroundCounters`, `GoalState`) and move their behavior onto the struct.
- Prefer **controllers** that own *dependencies* with a narrow API over methods that take `&mut self` (all of `App`) just to touch a few fields/deps: `AgentRuntime` (turn launching), `system_prompt::build(...)` (explicit deps), `BackgroundCounters` (registry sync). New cross-cutting behavior should follow this shape, not grow `mod.rs`.
- `impl App` may still be split across files (`keys.rs`, `multichat.rs`, `goal.rs`) — that's organizational; the *architectural* win is narrow deps + cohesive state.

### File-size rule (agreed 2026-07-04)
- **Line count is a trigger, not a verdict.** ~500+ lines of *non-test* code
  (inline `#[cfg(test)]` modules don't count — never punish a file for being
  well-tested) prompts the question: *does this file have more than one
  reason to change?* If yes — split **by responsibility**; if no (homogeneous
  catalogs like endpoint wrappers, transport impls of one trait, per-modal
  renderers on a shared kit) — leave it, whatever the length.
- Never split mechanically by line count: a split that forces widening
  visibility (`pub(super)` fields/helpers that existed only for the split) or
  makes readers jump between files to follow one flow has made the code
  worse, not better.
- Hard smells regardless of file length: a single function > ~150 lines; a
  file mixing abstraction layers (e.g. key decoding interleaved with app
  effects); helpers made `pub(crate)` purely so a split could compile.

## NAMING
- Error binding spelled out: `|error|` / `|e|` both appear; prefer `error` in new code (dominant style).
- Full words over abbreviations in public APIs. Tool names are lowercase snake (`shell_output`).
- MCP tool names: `mcp__{server}__{tool}` (double underscore separators).

## TUI
- Build with `ratatui` widgets; theme colors come from `tui/theme.rs` (`Theme` struct) — never hardcode RGB in widgets.
- Account for Unicode width (`unicode-width`) when measuring/wrapping text; use char-boundary-safe truncation.
- Keep render pure: `render()` reads `&AppState`, never mutates.

## TESTS
- **189 tests** today (`cargo test --bin pooprusteek`, was 84 pre-2026-07-02), all in-file `#[cfg(test)] mod tests` — provider `fork` isolation (`deepseek.rs`), goal `apply_verdict` + iteration-cap (`goal.rs`), conversation ids, tool-parser, runner, command-registry round-trips, `mcp_row_layout`, overflow-marker one-shot behavior, etc.
- Edge cases worth testing: multibyte/emoji boundaries, partial tool-tag streaming, all 3 tool-call formats, fork session independence.
- Favor a **pure functional core** (like `goal::apply_verdict`) so logic is testable without a live `App`.
- **Verification baseline = `cargo build` + `cargo test --bin pooprusteek` + `cargo clippy`.** Clippy is now **0 warnings** (was ~220) — keep it that way; CI runs it advisory for now, but treat new warnings as build breaks.

## ASSETS
- Anything loaded at runtime (prompts, the PoW WASM) resolves via `CARGO_MANIFEST_DIR` → CWD → exe-dir. Add new assets under `assets/` and follow that resolution pattern.

## COMMANDS / TOOLS (extending)
- New slash command: add a file in `src/commands/defs/`, impl `Command`, register in `commands/mod.rs` `register_defaults`, add to `defs/mod.rs`, and update `/help`.
- New tool: impl `Tool`, register in `tools/registry.rs`, document its JSON schema in `definition()`.
- New prompt/skill: drop a `*.prompt.md` (or `{name}/SKILL.md`) in `assets/prompts/`.

## RELEASE PROFILE (`Cargo.toml`)
- `opt-level=3, lto="fat", codegen-units=1, strip=true`. Release builds are slow but small/fast.

## COMMIT STYLE (observed in git log)
- Conventional commits with a **scope + gitmoji**: `refactor(app): 🧹 …`, `feat(provider, app): ✨ …`, `fix(deepseek): 🐛 …`, `refactor(background): ♻️ …`, `docs(comparison): 📝 …`, `refactor(tui): 🚚 …`, `feat(app): 🎯 …`. Keep messages imperative and scoped.
- **Commits are the user's job** — the agent leaves changes uncommitted in the working tree for the user to review and commit. Don't run `git commit`/`push`.
