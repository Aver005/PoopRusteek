# REFERENCE: Slash Commands
> Complete catalog of in-TUI slash commands. Source of truth: `src/commands/`.
> Last updated: 2026-07-06 session 3 (added /refetch-providers + /cache-providers — provider model-list freshness; earlier today: /serve + /server — API-server control). Before: 2026-07-05 (added /themes — theme gallery with live preview + custom-theme wizard; earlier: /rag — semantic-matching status/on/off/reload); 2026-07-04 (added /debug; /rate gained a per-minute mode + bare-`/rate`-shows-current-settings + change confirmation)

## HOW COMMANDS WORK

- **Trait** `Command` → `src/commands/mod.rs` — methods: `name()`, `description()`, `usage()`, `execute(args, state, config)`.
- **Registry** `CommandRegistry` → `src/commands/mod.rs:7` — HashMap by name. `register_defaults()` (~:58) registers every built-in.
- **Dispatch** `CommandRegistry::execute()` → `src/commands/mod.rs:91` — parses `/{command} [args]`, returns a `CommandResult`.
- Input NOT starting with `/` → returns `CommandResult::NeedsAgent(text)` → goes to the agent loop.
- Command files live in `src/commands/defs/` (one file per command).

### `CommandResult` variants (`src/commands/mod.rs:26`)
`Handled` · `NeedsAgent(String)` · `LoadSession(String)` · `ResetProvider` · `Quit` · `Error(String)` · `TtlUpdate(u64)` · `ReloadMcp` · `ShowTools` · `Jobs(JobCommandAction)` · `OpenWhitelist` · `ShowSkills` · `ToggleSkill(String,bool)` · `OpenConfirm(ConfirmAction)`

## FULL COMMAND LIST (42 commands, +2 `/cwd` aliases)

| Command | Aliases | Args | What it does | File |
|---------|---------|------|--------------|------|
| `/help` | — | — | List available commands | `defs/help.rs` |
| `/version` | — | — | Show version info | `defs/version.rs` |
| `/clear` | — | — | Clear chat history | `defs/clear.rs` |
| `/home` | — | — | Go to landing screen (clears messages) | `defs/home.rs` |
| `/quit` | — | — | Exit application | `defs/quit.rs` |
| `/compact` | — | — | Summarize history to shrink context (uses `compact.prompt.md`) | `defs/compact.rs` |
| `/reset` | — | — | Reset session completely (new session + provider reset) | `defs/reset.rs` |
| `/cwd` | `/cd`, `/move` | `<path>` | Change working directory (expands `~`) | `defs/cwd.rs` |
| `/attach` | — | `<path1> [path2]…` | Attach files to the NEXT message (supports quoted paths) | `defs/attach.rs` |
| `/export` | — | `[path]` | Export chat to Markdown (default `{data}/exports/{session_id}.md`) | `defs/export.rs` |
| `/import` | — | `<path.md>` | Import chat from Markdown; session tagged `Imported` | `defs/import.rs` |
| `/sessions` | — | — | List local sessions (hides `__goal_system__`-tagged); sessions with a confirmed-dead remote link show a ⚠ prefix and render yellow (`PickerItem::warn`) | `defs/session_list.rs` |
| `/session` | — | — | Show current session info (stats, tokens, model) | `defs/session_info.rs` |
| `/load` | — | `<session_id>` | Load session by ID (local, or DeepSeek remote) | `defs/load.rs` |
| `/last` | — | — | Open most recent session, or start fresh | `defs/last.rs` |
| `/goal` | — | — | Toggle GOAL mode (iterative goal-driven loop) | `defs/goal.rs` |
| `/jobs` | — | `[list\|kill <id>\|prune]` | Manage background jobs (default `list`) | `defs/jobs.rs` |
| `/ps` | — | — | Alias for `/jobs list` | `defs/ps.rs` |
| `/mcp` | — | `[ttl <secs>\|reload]` | Open MCP view; set cache TTL (1–86400); reload servers | `defs/mcp.rs` |
| `/tools` | — | — | Show all available tools (built-in + MCP) | `defs/tools.rs` |
| `/skills` | — | `[list\|enable <name>\|disable <name>]` | Manage skills; no-arg opens picker | `defs/skills.rs` |
| `/whitelist` | — | — | Open tool auto-approval whitelist manager | `defs/whitelist.rs` |
| `/rate` | — | `[<ms>\|<N>/min\|<N>rpm\|off]` | Set min delay between requests (`rate_limit_ms`) and/or max requests per rolling 60s window (`rate_limit_per_minute`) — independent gates, both may be active; `off` zeroes both. No args → prints current settings (`AgentConfig::rate_limit_display`) instead of just a usage error; every change pushes a confirmation system message | `defs/rate.rs` |
| `/retry` | — | `<N\|on\|off\|-1>` | Set max retries on API failure → `config.agent.max_retries` (-1/on = infinite, 0/off = none) | `defs/retry.rs` |
| `/btw` | — | `<question>` | One-shot side-question answered in the background (ephemeral `Sidechat` conversation, forked provider) — doesn't disturb the main turn | `defs/btw.rs` |
| `/new` | — | — | Open a new parallel chat (`Session` conversation, forked provider, fresh session) and switch to it | `defs/chats.rs` (`NewChatCommand`) |
| `/chats` | — | — | Picker to switch between parallel chats (Tab/Ctrl also cycles focus) | `defs/chats.rs` (`ChatsCommand`) |
| `/agent` | — | `<task>` | Launch a background sub-agent for a task | `defs/agent.rs` (`AgentCommand`) |
| `/agents` | — | — | List and stop running background sub-agents (picker) | `defs/agent.rs` (`AgentsCommand`) |
| `/logout` | — | — | Confirm → cancel all turns, clear `provider.token`, save config, show onboarding (`reset_to_onboarding`) | `defs/logout.rs` |
| `/wipe` | — | — | Confirm → cancel all turns, `remove_dir_all` over `wipe_roots()` (config-file parent + data dir, deduped), factory reset to `Config::default`, clear whitelist/history, show onboarding | `defs/wipe.rs` |
| `/debug` | — | `[on\|off]` | Toggle debug logging at runtime via `debug_log::set_enabled` (no args = flip current state); lazily opens `.dev/debug.log` on first enable | `defs/debug.rs` |
| `/search` | — | `[query]` | Opens the full-screen `View::Search`: query line + sort (`s`: relevance/newest/oldest) + role filter (`r`) + unique-per-session dedup (`u`); Enter on a result loads that session. Lookup runs off-loop (`spawn_history_search` → `AppEvent::HistorySearchDone`, stale replies dropped). Bare `/search` opens with an empty query | `defs/search.rs`, `app/search.rs`, `keys/search.rs`, `tui/render/search.rs` |
| `/rag` | — | `[on\|off\|reload]` | Semantic matching control. Bare = how-it-works + live status (state/model/indexed counts/config) + subcommand list. `on`/`off` = persist `semantic.enabled` + flip the running `SemanticService` (off → hints stop, full MCP schemas return; on → `spawn_init` if not ready). `reload` = drop model+corpora, re-verify/re-download model, re-embed skills and a freshly-fetched MCP tool list. Effects live in `apply_rag_action` (keys/dispatch.rs) via `CommandResult::Rag(RagAction)` | `defs/rag.rs` |
| `/rag-limit` | — | `[<N>\|auto\|off]` | Embedder batch cap = the ONNX memory guard (peak ≈ `batch × 12 heads × 512² × 4B`). Bare = status (mode + detected total RAM + effective batch). `auto` (default) sizes from RAM: `clamp(RAM×5% / 48 MiB, 1, 64)`, falls back to 8 when RAM can't be probed. `off` = no cap (fastembed default 256). `<N>` = fixed (not clamped — explicit override). Persists `semantic.rag_limit`, updates the live embedder immediately + on reload. Routed via `CommandResult::Rag(RagAction::{LimitStatus,SetLimit})` → `apply_rag_action` | `defs/rag_limit.rs` |
| `/serve` | — | `[on\|off\|api <openai\|anthropic\|gemini>]` | API-server control. Bare = status (state/addr/uptime/request counters/auth/exposed models + subcommand list). `on`/`off` = start/stop the hyper listener (`src/server/`). `api <d>` = persist the wire dialect (only `openai` implemented; others rejected with a message). Parsing is pure (`parse_serve_args`); effects live in `app/serve.rs::apply_serve_action` via `CommandResult::Serve(ServeAction)`. Also: `--serve`/`--server`/`--api` CLI flags start the TUI with the server already on | `defs/serve.rs` (`ServeCommand`) |
| `/server` | — | `<port>` | Set the API server port → `config.server.port` (saved for all future runs); a running server is restarted onto the new port (bind-retry absorbs the listener-close race). Bare `/server` = same status as `/serve`. Port 0 rejected | `defs/serve.rs` (`ServerCommand`) |
| `/refetch-providers` | — | `<ms\|off>` | Background period for re-fetching every `/providers` entry's model list (`GET /models`) → `config.provider_models.refetch_ms` (default 180000; off = only startup + provider add). Bare = show both knobs + per-provider fetch state (count, age). Runs off `AppEvent::Tick`; effects in `apply_provider_models_action` | `defs/provider_models.rs` |
| `/cache-providers` | — | `<ms\|off>` | How long persisted model lists (`data_dir/provider_models.json`) stay valid across restarts → `config.provider_models.cache_ms` (default 180000; off = every startup re-fetches). Bare = same status as `/refetch-providers` | `defs/provider_models.rs` |
| `/update` | — | — | Self-update: fetches `SHA256SUMS` from the GitHub release tagged `latest`, compares against the running binary's SHA-256, on mismatch downloads the raw platform binary, verifies its hash, **stages it at `<exe>.new`, then swaps** (Unix atomic rename; Windows move-to-`.old`+rename; applies on next launch). In a **debug/`cargo run`** build (`cfg!(debug_assertions)`) it opens a confirm modal first (`ConfirmAction::Update`) — a dev binary always mismatches the released hash. All work off-loop (`app::spawn_update_task` → `AppEvent::UpdateStatus`); an `update_in_flight` AtomicBool blocks concurrent passes. **Contract points** (asset names ↔ CI, `latest` tag ↔ URL) in `reference/AUTO-UPDATE.md` | `defs/update.rs` (`UpdateCommand`), `src/update.rs` |
| `/autoupdate` | — | `[on\|off]` | Startup auto-update switch → `config.update.auto` (default off). When on, every TUI launch runs the same check/install in the background — quiet if already current (status line only), chat message on install/failure. Bare = status + explanation. Intents: `CommandResult::Update(UpdateAction)` → `apply_update_action` (keys/dispatch.rs) | `defs/update.rs` (`AutoUpdateCommand`) |
| `/themes` | — | `[new\|<name>]` | Theme gallery (full-screen `View::Themes`): 10 built-in presets + `[[ui.custom_themes]]` entries; moving the cursor redraws the *whole frame* in the highlighted theme (live preview — `ThemesViewState::preview_theme` resolved in `render()`), Enter applies + saves `ui.theme`. `n`/`e`/`d` = new/edit/delete custom themes. `new`/`add`/`create` arg jumps straight into the step-by-step wizard (name → base preset → one step per `theme::ROLES` color role, hex prefilled from the base so Enter-through is fast, Ctrl+S = finish early → confirm saves + applies). `<name>` switches directly (validated against presets+customs). Intents: `OpenThemes`/`OpenThemeWizard`/`SetTheme` | `defs/themes.rs`, `app/themes.rs`, `keys/themes.rs`, `tui/render/themes.rs`, `tui/theme.rs` |

## NOTES & GOTCHAS

- `/btw`, `/new`+`/chats`, `/agent`+`/agents` arrived with the multi-chat wave (commits `438e60d`, `6c04774`, `38ce06f`). They rely on the `Conversation`/`Conversations` model and `LLMProvider::fork()` — see `ARCHITECTURE.md`.
- `/logout` and `/wipe` route through `CommandResult::OpenConfirm(ConfirmAction)` → `Modal::Confirm(ConfirmState)` rendered by `render_confirm` (dynamic-height, error-red border, Enter/y + n/Esc hints). `/wipe` deletes only paths in `wipe_roots()` — foreign configs (e.g. `~/.claude`, `~/.cursor`, VS Code) are never touched.
- The model can also spawn a sub-agent itself via the `task` tool (special-cased in `run_agent_loop`), independent of `/agent`.

- `/jobs` and `/ps` were added in commit `e801dbe` (2026-06-28) alongside GOAL mode.
- `/goal`'s `name()` returns `"/goal"` (with leading slash) — minor inconsistency vs. other commands; harmless because dispatch strips the slash.
- `/import` always overwrites the tag with `"Imported"` — original export metadata is lost.
- `/mcp ttl <secs>` persists to `config.mcp.cache_ttl`; `/mcp reload` forces a fresh `tools/list` on all servers.
- Settings commands (`/rate`, `/retry`, `/mcp ttl`) mutate `Config` and save it to disk.
