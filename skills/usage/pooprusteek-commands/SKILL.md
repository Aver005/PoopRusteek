---
name: PoopRusteek — Slash Commands
description: Cheat sheet of PoopRusteek's slash commands grouped by theme. Use when you need the exact syntax or purpose of an in-app command (/new, /goal, /mcp, /rag, /providers, etc.). Not for editing PoopRusteek's Rust source.
---

# PoopRusteek — Slash Commands

Type `/name args` in the input to run a command. Any input **not** starting with `/`
goes straight to the agent as a message. `~40` commands ship;
`src/commands/defs/` (one file per command) is the source of truth — run `/help`
in-app for the live list. For the exhaustive per-command reference (args,
aliases, source file), read `references/all-commands.md`.

## Session & history

| Command | Purpose |
|---------|---------|
| `/clear` · `/home` | Clear chat history · return to landing screen |
| `/compact` | Summarize history to shrink context |
| `/reset` | New session + provider reset |
| `/sessions` · `/session` | List local sessions (⚠ marks dead remote links) · current session info |
| `/load <id>` · `/last` | Load a session by id (local or DeepSeek remote) · open most recent |
| `/export [path]` · `/import <path.md>` | Export chat to Markdown · import one (tagged `Imported`) |
| `/delete [id]` · `/delete-local [id]` | Delete sessions account+local, or local-only (multi-select picker) |
| `/search [query]` | Full-screen semantic + keyword search over all saved sessions |
| `/cwd` (`/cd`, `/move`) `<path>` | Change working directory (expands `~`) |
| `/attach <path…>` | Attach files to the next message |

## Chats & agents

| Command | Purpose |
|---------|---------|
| `/new` · `/chats` | Open a new parallel chat · picker to switch (Tab/Shift+Tab also cycles) |
| `/btw <question>` | One-shot background side-question, doesn't disturb the main turn |
| `/agent <task>` · `/agents` | Launch a background sub-agent · list and stop running ones |
| `/goal` | Toggle GOAL mode (worker + evaluator iterate until the goal is met) |

## Tools, skills & MCP

| Command | Purpose |
|---------|---------|
| `/tools` | Show all tools (built-in + MCP) |
| `/skills [list\|enable <name>\|disable <name>]` | Manage skills; no-arg opens a picker |
| `/whitelist` | Manage tool auto-approval |
| `/mcp [add\|auth\|ttl <secs>\|reload]` | MCP view · add server · OAuth-authorize · set cache TTL · reload |
| `/jobs [list\|kill <id>\|prune]` · `/ps` | Manage background jobs (`/ps` = `/jobs list`) |

## Providers, models & API server

| Command | Purpose |
|---------|---------|
| `/providers` · `/providers add [<name> [openai\|anthropic] <base_url> [model] [api_key]]` | Provider panel · add an OpenAI-compat / Anthropic / Gemini endpoint (wizard or one-liner) |
| `/models [<model_id>]` | Pick a model from the active provider's live list · or switch directly |
| `/refetch-providers <ms\|off>` | Background period for re-fetching provider model lists |
| `/cache-providers <ms\|off>` | How long persisted model lists stay valid across restarts |
| `/serve [on\|off\|api <openai\|anthropic\|gemini>]` | Start/stop the HTTP API gateway · set wire dialect |
| `/server <port>` | Set the API server port (persisted) |

## Tuning

| Command | Purpose |
|---------|---------|
| `/rate [<ms>\|<N>/min\|off]` | Min delay between requests and/or max requests per rolling 60s (bare = show current) |
| `/retry <N\|on\|off\|-1>` | Max retries on API failure (-1/on = infinite) |
| `/rag [on\|off\|reload]` | Semantic matching: status · on/off · reinitialize |
| `/rag-limit [<N>\|auto\|off]` | Embedder batch cap (ONNX memory guard) |
| `/themes [new\|<name>]` | Theme gallery with live preview + custom-theme wizard |
| `/debug [on\|off]` | Toggle debug logging to `.dev/debug.log` |

## Lifecycle & misc

| Command | Purpose |
|---------|---------|
| `/update` · `/autoupdate [on\|off]` | Self-update now · toggle the startup auto-check |
| `/logout` · `/wipe` | Clear token → onboarding · factory reset → onboarding (both confirm) |
| `/help` · `/version` · `/quit` | Help · version · exit |
