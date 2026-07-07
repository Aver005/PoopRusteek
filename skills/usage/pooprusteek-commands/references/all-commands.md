# PoopRusteek — Complete Slash Command Reference

Every slash command that ships, one row each, generated from the source of
truth: `src/commands/defs/` (one file per command) + `register_defaults()` in
`src/commands/mod.rs`. Type `/name args` in the input; any line not starting
with `/` is sent to the agent as a message. Args in `[brackets]` are optional,
`<angle>` are required; a bare command with no args usually shows status or
opens a picker. Run `/help` in-app for the live list.

## Session & history

| Command | Aliases | Args | What it does | File |
|---------|---------|------|--------------|------|
| `/clear` | — | — | Clear chat history | `clear.rs` |
| `/home` | — | — | Go to home (landing) screen | `home.rs` |
| `/compact` | — | — | Compact context by summarizing history | `compact.rs` |
| `/reset` | — | — | Reset session completely (new session + provider reset) | `reset.rs` |
| `/sessions` | — | — | List local sessions (⚠ marks dead remote links) | `session_list.rs` |
| `/session` | — | — | Show current session info | `session_info.rs` |
| `/load` | — | `<session_id>` | Load a session by ID (local file or DeepSeek remote) | `load.rs` |
| `/last` | — | — | Open the most recent session, or start a fresh chat | `last.rs` |
| `/export` | — | `[path]` | Export current chat to a Markdown file | `export.rs` |
| `/import` | — | `<path>` | Import chat from a Markdown file (new session tagged `Imported`) | `import.rs` |
| `/delete` | — | `[session_id]` | Delete sessions — remote (DeepSeek account) + local copies; picker or direct id | `delete.rs` |
| `/delete-local` | — | `[session_id]` | Delete only locally stored session files (account copies stay) | `delete.rs` |
| `/search` | — | `[query]` | Full-screen semantic + keyword search over all saved sessions | `search.rs` |
| `/cwd` | `/cd`, `/move` | `<path>` | Change the current working directory (expands `~`) | `cwd.rs` |
| `/attach` | — | `<path1> [path2] ...` | Attach files to the current message | `attach.rs` |

## Chats & agents

| Command | Aliases | Args | What it does | File |
|---------|---------|------|--------------|------|
| `/new` | — | — | Open a new parallel chat and switch to it | `chats.rs` |
| `/chats` | — | — | Switch between parallel chats (picker) | `chats.rs` |
| `/btw` | — | `<question>` | Ask a quick one-shot side-question in the background | `btw.rs` |
| `/agent` | — | `<task>` | Launch a background sub-agent for a task | `agent.rs` |
| `/agents` | — | — | List and stop running background agents | `agent.rs` |
| `/goal` | — | — | Toggle GOAL mode: define a goal and iterate (worker + evaluator) until achieved | `goal.rs` |

## Tools, skills & MCP

| Command | Aliases | Args | What it does | File |
|---------|---------|------|--------------|------|
| `/tools` | — | — | Show all available tools (built-in + MCP) | `tools.rs` |
| `/skills` | — | `[list \| enable <name> \| disable <name>]` | Manage skills; no-arg opens the skill picker | `skills.rs` |
| `/whitelist` | — | — | Manage the tool auto-approval whitelist | `whitelist.rs` |
| `/mcp` | — | `[ttl <secs> \| reload \| auth \| oauth \| add [<name> <command> [args...] \| <json>]]` | Open MCP management; set cache TTL, reload, authorize (OAuth), or add a server | `mcp.rs` |
| `/jobs` | — | `[list \| kill <id> \| prune]` | Manage background jobs | `jobs.rs` |
| `/ps` | — | — | Alias for `/jobs list` | `ps.rs` |

## Providers, models & API server

| Command | Aliases | Args | What it does | File |
|---------|---------|------|--------------|------|
| `/providers` | — | `[add [<name> [openai\|anthropic] <base_url> [model] [api_key]]]` | Manage LLM providers (built-in DeepSeek + OpenAI-compat / Anthropic / Gemini); add via wizard or one-liner | `providers.rs` |
| `/models` | — | `[<model_id>]` | List the active provider's models (picker), or switch to one directly | `models.rs` |
| `/refetch-providers` | — | `<ms \| off>` | Set how often provider model lists are re-fetched (background period) | `provider_models.rs` |
| `/cache-providers` | — | `<ms \| off>` | Set how long fetched provider model lists stay valid across restarts | `provider_models.rs` |
| `/serve` | — | `[on \| off \| api <openai\|anthropic\|gemini>]` | API server: status, start/stop, wire dialect | `serve.rs` |
| `/server` | — | `<port>` | Set the API server port (persisted for future runs) | `serve.rs` |

## Tuning

| Command | Aliases | Args | What it does | File |
|---------|---------|------|--------------|------|
| `/rate` | — | `<ms> \| <N>/min \| off` | Set rate limit: ms between requests and/or max requests per rolling 60s (both settable independently; bare = show current) | `rate.rs` |
| `/retry` | — | `<number \| on \| off \| -1>` | Set max retries on request failure (-1/on = infinite, 0/off = disabled) | `retry.rs` |
| `/rag` | — | `[on \| off \| reload]` | Semantic matching (RAG): status, on/off, full reload | `rag.rs` |
| `/rag-limit` | — | `[<N> \| auto \| off]` | Embedder batch cap (ONNX RAM guard): auto, off, or a fixed number | `rag_limit.rs` |
| `/themes` | — | `[new \| <name>]` | Pick a color theme (live preview) or build your own with a step-by-step wizard | `themes.rs` |
| `/debug` | — | `[on \| off]` | Toggle debug logging to `.dev/debug.log` (no args = switch) | `debug.rs` |

## Lifecycle & misc

| Command | Aliases | Args | What it does | File |
|---------|---------|------|--------------|------|
| `/update` | — | — | Self-update from the latest dev release | `update.rs` |
| `/autoupdate` | — | `[on \| off]` | Auto-update on startup: status, on/off | `update.rs` |
| `/logout` | — | — | Log out — remove the saved DeepSeek token (confirms) | `logout.rs` |
| `/wipe` | — | — | Factory reset — delete ALL local Pooprusteek data (confirms) | `wipe.rs` |
| `/help` | — | — | Show available commands | `help.rs` |
| `/version` | — | — | Show version info | `version.rs` |
| `/quit` | — | — | Exit application | `quit.rs` |
