# BUGS
> Known defects, sorted by impact. Update on discovery/fix.
> Last updated: 2026-06-30

## CRITICAL
None currently known.

## HIGH

- `[BUG]` GOAL cycle has **no max-iteration cap** → if the evaluator never returns SUCCESS, agent↔evaluator can loop indefinitely. `→ src/app/mod.rs:1596`
- `[BUG]` Retry with `max_retries=-1` (infinite) has **no total-time cap and no jitter** → a persistently-failing endpoint can hang the turn forever. `→ src/provider/deepseek.rs:225`

## MEDIUM

- `[BUG]` Tool-approval modal **blocks the event loop** — input is frozen until the user answers (agent task is also parked on `.wait()`). `→ src/app/mod.rs` (handle_event ~:521) + `src/agent/runner.rs:146`
- `[BUG]` No request-level timeout on DeepSeek HTTP calls (only a 120s **idle** stream timeout in the agent loop). A stalled connection between bytes relies on reqwest defaults. `→ src/provider/deepseek.rs`
- `[BUG]` MCP tool arguments are passed to `tools/call` with **no schema validation**. `→ src/mcp/client.rs`
- `[BUG]` `stream_visible_text` truncates at the first bare `<` → legitimate text containing `<` (C++ templates, `a < b`, HTML) is hidden mid-stream. `→ src/agent/tool_parser.rs:100`
- `[BUG]` PoW challenge **expiry not checked** before submit; a slow WASM solve can send a stale challenge with no retry. `→ src/provider/pow.rs`
- `[BUG]` Background output buffer is capped at 256 KiB; on overflow data is dropped (flagged but unrecoverable). `→ src/tools/background.rs:94`

## LOW

- `[BUG]` `/import` overwrites the session tag with `"Imported"`, losing any original tag/metadata. `→ src/commands/defs/import.rs:56`
- `[BUG]` `@file` mention expansion may mishandle paths with spaces. `→ src/cli/file_mentions.rs`
- `[BUG]` Autocomplete file paths resolve against `current_dir()`, which can drift from `workspace_path` after a subprocess `cd`. `→ src/app/mod.rs:1285`
- `[BUG]` Theme is hardcoded Catppuccin Mocha; `ui.theme` config is ignored. `→ src/tui/theme.rs`
- `[BUG]` MCP image content is dropped to `[Image: {mime}]` text; non-text/image/resource content types ignored. `→ src/mcp/client.rs:216`
- `[BUG]` `mcp__` uses `__` as separator → name collision possible if a server/tool name contains `__`. `→ src/mcp/manager.rs:186`
- `[BUG]` History deduplication only catches **consecutive** duplicates. `→ src/session.rs:182`

## WONTFIX / ACCEPTED

- `[?]` PoW WASM binary checked in as a blob — increases repo size. Native SHA-3 reimpl would avoid it.
- `[?]` DeepSeek web API is reverse-engineered; may break on any server update. No SLA.
- `[?]` `bash`/`powershell` run arbitrary commands with no sandbox — by design; trust = tool-approval + `/whitelist`.

## RESOLVED / MOOT
- `~~No .gitignore entry for .memories/JOURNAL/~~` — JOURNAL is intentionally tracked in git.
