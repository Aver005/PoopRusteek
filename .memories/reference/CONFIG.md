# REFERENCE: Config, Storage & Sessions
> Where everything lives on disk. Source: `src/config/mod.rs`, `src/session.rs`.
> Last updated: 2026-07-04 (added `agent.rate_limit_per_minute`; debug log is now runtime-toggleable via `/debug`, not just the CLI flag)

## FILE LOCATIONS (`config/mod.rs:100`)

Paths come from the `dirs` crate, so they are **platform-specific**:

| Item | Code | Linux | Windows | macOS |
|------|------|-------|---------|-------|
| Config | `config_dir()/pooprusteek/config.toml` | `~/.config/pooprusteek/config.toml` | `%APPDATA%\pooprusteek\config.toml` | `~/Library/Application Support/pooprusteek/config.toml` |
| Data dir | `data_dir()/pooprusteek/` | `~/.local/share/pooprusteek/` | `%APPDATA%\pooprusteek\` | `~/Library/Application Support/pooprusteek/` |
| Sessions | `{data}/sessions/{id}.json` | … | … | … |
| History | `{data}/history.json` | … | … | … |
| MCP own config | `{data}/mcp.json` | … | … | … |
| Debug log | `.dev/debug.log` (relative to CWD; enabled by `--debug_log` at startup or toggled at runtime via `/debug`) | — | — | — |

> On this machine (Windows), config + data both resolve under `%APPDATA%\Roaming\pooprusteek\`.

## CONFIG SCHEMA (`config/mod.rs:5`, TOML)

```toml
[provider]
kind = "deepseek"          # deepseek | openai | custom (only deepseek implemented)
token = ""                 # DeepSeek web session token (required to use the agent)
model = "deepseek-chat"    # or "deepseek-reasoner" (enables thinking/expert mode)
base_url = ""              # Option<String>, default None
temperature = 0.7
max_tokens = 4096

[ui]
theme = "default"          # only Catppuccin Mocha exists; theme is effectively hardcoded
show_status_bar = true
show_line_numbers = false
max_message_length = 4096

[agent]
max_steps_per_turn = 256   # agent loop hard cap (NOT 25)
max_tools_per_step = 10    # tools executed per step (NOT 50)
max_context_messages = 256
auto_compact = true
rate_limit_ms = 0          # set via /rate <ms>; 0 = disabled
rate_limit_per_minute = 0  # set via /rate <N>/min; 0 = disabled; #[serde(default)] so old config.toml files without this key still load
max_retries = 0            # set via /retry; -1 = infinite

[mcp]
cache_ttl = 300            # MCP tools/list cache TTL secs; set via /mcp ttl

[skills]
enabled = []               # list of enabled skill names
paths = []                 # extra skill search dirs
```

- `load()` returns `Config::default()` if the file is missing (no crash). `save()` writes pretty TOML, creating parent dirs.
- Unknown fields are ignored (serde defaults). `ProviderKind` is serde-lowercase.

## SESSIONS (`src/session.rs`)

- **`Session`** (:8): `version(=1), id, created_at, updated_at, workspace_root, model_type, messages, tag?`.
- **ID format** (:34): `{YYYY-MM-DDTHH-MM-SS-fff}Z-{uuid8}` → naturally sortable by time.
- **Save**: `save_session(...)` (:42) → `save_session_with_tag(..., None)` (:52). Pretty JSON at `{sessions}/{id}.json`; refreshes `updated_at`.
- **Load**: `load_local(id)` (:81) errors `SessionNotFound` if absent. `list_sessions()` (:92) enumerates `*.json`, sorts by `updated_at` desc → `Vec<SessionSummary>` (title derived from first non-empty message, ≤80 chars).
- **Auto-save**: `App::auto_save_session()` after every agent turn.
- **Special tags**: `__goal_system__` (GOAL evaluator sessions, hidden from `/sessions`), `Imported` (`/import`).
- **History file** `{data}/history.json` (:165): JSON array of input strings, capped at **500**, consecutive dups deduped.

## ONBOARDING (`View::Onboarding`)

First launch (no config, or after `/logout`/`/wipe`): in-TUI full-screen onboarding (`View::Onboarding`). The former CLI prompt (`src/cli/onboarding.rs`) is **deleted**. Shows an animated `pulsing_title` logo (shared with the landing screen), a steps panel, a token input field, and a deepseek-chat/reasoner selector. Keys handled by `handle_onboarding_key` (`src/app/keys.rs`), rendered by `render_onboarding` (`src/tui/render.rs`). State tracked in `OnboardingState` (`src/app/events.rs`). On submit: saves config, hot-creates `DeepseekProvider` (init failure shows an error on-screen — does not proceed), then calls `Conversation::fresh_main` to swap in a new main conversation.

`/logout` clears `provider.token`, saves config, and transitions to `View::Onboarding` via `reset_to_onboarding`. `/wipe` factory-resets all app-owned paths (config-file parent dir + data dir, via `wipe_roots()`, deduped) plus in-memory state, then transitions to onboarding. Neither command touches foreign configs (`~/.claude`, `~/.cursor`, VS Code, etc.).

## ERRORS & LOGGING

- **`AppError`** (`src/error.rs:3`): `Io | Http | Json | Config(String) | Provider(String) | Mcp(String) | Join | SessionNotFound(String) | Custom(String)`. Alias `AppResult<T>`.
- **Debug log** (`src/debug_log.rs`): enabled by `--debug_log`; writes timestamped `[action] message` lines to `.dev/debug.log` via a `Mutex<File>` `OnceLock`. `log()` and `log_json()`.
