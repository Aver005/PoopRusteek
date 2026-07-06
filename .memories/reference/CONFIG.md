# REFERENCE: Config, Storage & Sessions
> Where everything lives on disk. Source: `src/config/mod.rs`, `src/session.rs`.
> Last updated: 2026-07-06 (added `[server]` — the API-gateway section behind `--serve`/`/serve`/`/server <port>`). Before: 2026-07-04 (added `agent.rate_limit_per_minute`; debug log is now runtime-toggleable via `/debug`, not just the CLI flag)

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

[server]                   # API gateway (src/server/) — /serve, /server <port>, --serve, --proxy
host = "127.0.0.1"         # loopback by default; widen deliberately
port = 7667                # persisted by /server <port> ("poop" on T9)
api = "openai"             # openai | anthropic | gemini — wire dialect; only openai implemented (others answer 501)
# api_key = "…"            # optional; when set every request needs Authorization: Bearer <api_key>

[provider_models]          # per-entry model lists (provider/model_cache.rs) — feed /v1/models + routing
refetch_ms = 180000        # background refetch period; /refetch-providers <ms|off>; 0 = off
cache_ms = 180000          # persisted-cache validity across restarts; /cache-providers <ms|off>; 0 = off
```

- `ProviderEntry.model` may be **empty** (wizard's Model step is optional): the API catalog
  then serves the fetched list only, bare `<name>` routes to the first fetched model, and
  activating the entry in the TUI auto-opens the `/models` picker.
- Fetched lists persist in `data_dir/provider_models.json` (atomic_write; rebuildable cache).

- `load()` returns `Config::default()` if the file is missing (no crash). `save()` writes pretty TOML, creating parent dirs.
- Unknown fields are ignored (serde defaults). `ProviderKind` is serde-lowercase.

## SESSIONS (`src/session.rs`)

- **`Session`**: `version(=1), id, created_at, updated_at, workspace_root, model_type, messages, tag?, provider_session_id?, provider_parent_message_id?, broken`. The last three (`#[serde(default)]`, added 2026-07-04) mirror the DeepSeek-side session identity so a reload can attempt to continue the same remote thread — see "Remote session resume" below.
- **ID format**: `{YYYY-MM-DDTHH-MM-SS-fff}Z-{uuid8}` → naturally sortable by time. This is a purely local id — historically **unrelated** to DeepSeek's own `chat_session.id`, which is why remote continuity needed its own fields rather than reusing this one.
- **Save**: `save_session(id, created_at, messages, config, workspace_root, meta: &SessionMeta)`. `SessionMeta { tag, broken, provider_session_id, provider_parent_message_id }` bundles the non-derived fields so the function doesn't grow a new positional arg each time one is added. Pretty JSON at `{sessions}/{id}.json`; refreshes `updated_at`.
- **Load**: `load_local(id)` errors `SessionNotFound` if absent. `list_sessions()` enumerates `*.json`, sorts by `updated_at` desc → `Vec<SessionSummary>` (title derived from first non-empty message, ≤80 chars; `broken` passed through for the `/sessions` picker).
- **Auto-save**: `App::auto_save_session()` after every agent turn. Reads `conv.tag`/`conv.broken` (mirrored on `Conversation`, not re-derived from disk) and `provider.session_identity()` (sync, in-memory) to build the `SessionMeta` — this is also what fixed a latent bug where any auto-save after `/load`/`/import` silently wiped the file's `tag` back to `None`.
- **Special tags**: `__goal_system__` (GOAL evaluator sessions, hidden from `/sessions`), `Imported` (`/import`).
- **History file** `{data}/history.json`: JSON array of input strings, capped at **500**, consecutive dups deduped.

### Remote session resume (2026-07-04)

Bug: every local-session load called `provider.reset()` unconditionally, and the DeepSeek-side `SessionState` (`provider/deepseek/session.rs`) was never persisted anywhere — so reopening the app (or `/load`ing an old session) always silently created a **brand-new** remote `chat_session`, even though the old one was still live on DeepSeek's servers.

Fix — `LLMProvider` gained three methods (`provider/mod.rs`, defaults are no-ops so only DeepSeek implements them for real):
- `session_identity() -> Option<(String, Option<i64>)>` — sync read of the live `(session_id, parent_message_id)`, sampled by `auto_save_session` every turn.
- `session_is_alive(session_id) -> bool` — best-effort check via `fetch_remote_history` (`chat/history`); a non-2xx or network error both read as "not alive" (can't safely resume onto a session you can't verify).
- `adopt_session(session_id, parent_message_id)` — sets `SessionState` directly (`system_sent_for_session = true`, since the remote thread already has the system prompt/history) instead of calling `chat_session/create`.

`App::handle_load_session` (`app/mod.rs`): if the loaded `Session` has a `provider_session_id`, spawns `session_is_alive` off the event loop (never blocks the TUI) and reacts via `AppEvent::SessionAvailabilityChecked` → `apply_session_availability`: alive → `adopt_session` (silent, continues the same thread); dead → `provider.reset()` + `finalize_broken_session` (sets `broken = true`, clears the remote pointer, saves, pushes a system message, and — because the reset session has `system_sent_for_session = false` — the **next** message automatically replays full local history as one flattened prompt via the existing first-turn `build_prompt` logic, no new code needed for that part). No known remote id → just `reset()`, unchanged default behavior. `broken` clears itself again once `auto_save_session` next observes a live `session_identity()` — it reflects current status, not a permanent scar.

`/sessions` (`commands/defs/session_list.rs`) flags `broken` entries with `PickerItem::warn(true)` (⚠ prefix + yellow text, `tui/render.rs::render_picker` reads `item.warn`).

Known gap: the *other* remote-import path (`apply_fetched_session`, triggered when `/load <id>` misses locally and falls back to fetching a remote session by id) does **not** attempt resume — `fetch_remote_history`'s parsing of `chat/history` never extracts a message id, so there's no reliable `parent_message_id` to adopt. It still saves `provider_session_id: None`, falling back to default fresh-session behavior.

## ONBOARDING (`View::Onboarding`)

First launch (no config, or after `/logout`/`/wipe`): in-TUI full-screen onboarding (`View::Onboarding`). The former CLI prompt (`src/cli/onboarding.rs`) is **deleted**. Shows an animated `pulsing_title` logo (shared with the landing screen), a steps panel, a token input field, and a deepseek-chat/reasoner selector. Keys handled by `handle_onboarding_key` (`src/app/keys.rs`), rendered by `render_onboarding` (`src/tui/render.rs`). State tracked in `OnboardingState` (`src/app/events.rs`). On submit: saves config, hot-creates `DeepseekProvider` (init failure shows an error on-screen — does not proceed), then calls `Conversation::fresh_main` to swap in a new main conversation.

`/logout` clears `provider.token`, saves config, and transitions to `View::Onboarding` via `reset_to_onboarding`. `/wipe` factory-resets all app-owned paths (config-file parent dir + data dir, via `wipe_roots()`, deduped) plus in-memory state, then transitions to onboarding. Neither command touches foreign configs (`~/.claude`, `~/.cursor`, VS Code, etc.).

## ERRORS & LOGGING

- **`AppError`** (`src/error.rs:3`): `Io | Http | Json | Config(String) | Provider(String) | Mcp(String) | Join | SessionNotFound(String) | Custom(String)`. Alias `AppResult<T>`.
- **Debug log** (`src/debug_log.rs`): enabled by `--debug_log`; writes timestamped `[action] message` lines to `.dev/debug.log` via a `Mutex<File>` `OnceLock`. `log()` and `log_json()`.
