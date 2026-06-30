# CONVENTIONS
> Code style & patterns to follow when contributing. Match the surrounding code.
> Last updated: 2026-06-30

## LANGUAGE & EDITION
- Rust **edition 2024**, MSRV **1.85**. Use modern idioms (`let-else`, `LazyLock`, `floor_char_boundary`, etc.).

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

## NAMING
- Error binding spelled out: `|error|` / `|e|` both appear; prefer `error` in new code (dominant style).
- Full words over abbreviations in public APIs. Tool names are lowercase snake (`shell_output`).
- MCP tool names: `mcp__{server}__{tool}` (double underscore separators).

## TUI
- Build with `ratatui` widgets; theme colors come from `tui/theme.rs` (`Theme` struct) — never hardcode RGB in widgets.
- Account for Unicode width (`unicode-width`) when measuring/wrapping text; use char-boundary-safe truncation.
- Keep render pure: `render()` reads `&AppState`, never mutates.

## TESTS
- Sparse today. Where they exist (`agent/runner.rs`, `agent/tool_parser.rs`), they're `#[cfg(test)] mod tests` in-file.
- Edge cases worth testing: multibyte/emoji boundaries, partial tool-tag streaming, all 3 tool-call formats.
- **Verification baseline = `cargo build`.** `cargo clippy` not yet clean (run it before large changes).

## ASSETS
- Anything loaded at runtime (prompts, the PoW WASM) resolves via `CARGO_MANIFEST_DIR` → CWD → exe-dir. Add new assets under `assets/` and follow that resolution pattern.

## COMMANDS / TOOLS (extending)
- New slash command: add a file in `src/commands/defs/`, impl `Command`, register in `commands/mod.rs` `register_defaults`, add to `defs/mod.rs`, and update `/help`.
- New tool: impl `Tool`, register in `tools/registry.rs`, document its JSON schema in `definition()`.
- New prompt/skill: drop a `*.prompt.md` (or `{name}/SKILL.md`) in `assets/prompts/`.

## RELEASE PROFILE (`Cargo.toml`)
- `opt-level=3, lto="fat", codegen-units=1, strip=true`. Release builds are slow but small/fast.

## COMMIT STYLE (observed in git log)
- Conventional-ish prefixes: `feat:`, `fix:`, `imp:`, `refactor:`. Occasional emoji (🔥 for deletions). Keep messages imperative and scoped.
