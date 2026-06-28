# BUGS
> Known defects, sorted by impact. Update on discovery/fix.
> Last updated: 2026-06-28T17:12

## CRITICAL

None currently known.

## HIGH

None currently known.

## MEDIUM

- `[BUG]` Tool approval dialog blocks event loop — modal freezes input until resolved. `→ src/app/mod.rs`
- `[BUG]` No timeout/fallback if DeepSeek API stream hangs mid-response. `→ src/provider/deepseek.rs`
- `[BUG]` MCP tool argument JSON parsing is lenient — no schema validation. `→ src/mcp/client.rs`

## LOW

- `[BUG]` Session list doesn't show creation date, only filename. `→ src/commands/defs/session_list.rs`
- `[BUG]` `@file` mentions don't handle paths with spaces. `→ src/cli/file_mentions.rs`
- `[BUG]` Theme is hardcoded Catppuccin Mocha — no runtime switching. `→ src/tui/theme.rs`
- `[BUG]` No `.gitignore` entry for `.memories/JOURNAL/` — consider if journals should be tracked
- `[BUG]` Compact prompt can lose tool call history context on aggressive summarization. `→ assets/prompts/compact.prompt.md`

## WONTFIX / ACCEPTED

- `[?]` PoW WASM binary checked in as blob — increases repo size. Alternative requires native SHA-3 impl.
- `[?]` DeepSeek API is reverse-engineered, may break on server update. No SLA.
