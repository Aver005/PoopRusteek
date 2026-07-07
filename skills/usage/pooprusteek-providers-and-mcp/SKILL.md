---
name: PoopRusteek — Providers & MCP
description: Configure extra LLM providers (OpenAI-compat / Anthropic / Gemini) and MCP tool servers in PoopRusteek. Use when adding a provider endpoint, switching models, or wiring up MCP servers and OAuth. Not for editing PoopRusteek's Rust source.
---

# PoopRusteek — Providers & MCP

## LLM providers

The built-in **DeepSeek web client** is the default backend (cookie/token + local
PoW, no API key). When you need more, `/providers` manages any number of extra
endpoints speaking three wire protocols:

- **OpenAI Chat Completions** — LM Studio, Ollama `/v1`, vLLM, OpenRouter, …
- **Anthropic Messages** — the Claude API and compatible proxies
- **Google Generative Language** — Gemini

### Adding & switching

```
/providers                 # open the panel: Enter activates, a adds, d removes
/providers add             # step-by-step wizard
/providers add <name> [openai|anthropic] <base_url> [model] [api_key]   # one-liner
/models                    # pick a model from the active provider's live list
/models <model_id>         # switch directly (validated against the live list)
```

Notes:
- The wizard's Model step is optional. An entry with an empty `model` serves the
  fetched list only; a bare `<name>` routes to the first fetched model, and
  activating that entry auto-opens the `/models` picker.
- Extra endpoints persist in `config.toml` as `[[providers]]` entries
  (`protocol = "openai" | "anthropic" | "gemini"`); `active_provider` picks the live one.

### Model-list freshness

Provider model lists are fetched and cached in `data_dir/provider_models.json`.

```
/refetch-providers <ms|off>   # background refetch period (default 180000; off = startup + on-add only)
/cache-providers  <ms|off>    # how long the persisted cache stays valid across restarts (default 180000)
```

Bare `/refetch-providers` (or `/cache-providers`) shows both knobs plus per-provider
fetch state (count, age).

## MCP tool servers

MCP lets external servers expose tools to the agent. Tool names are namespaced
`mcp__{server}__{tool}`. Browse connected servers and their tools with `/mcp`.

### Discovery — 8 config sources (first-found-wins)

1. Own — `{data}/pooprusteek/mcp.json` (also stores enabled/disabled state)
2. Workspace — `./mcp.config.json`, `./.vscode/mcp.json`
3. Global — `{config}/pooprusteek/mcp.config.json`
4. Claude Desktop — `claude_desktop_config.json`
5. VS Code — `settings.json` → `mcp.servers`
6. Claude CLI — `~/.claude/mcp.json`
7. Cursor — `~/.cursor/mcp.json`
8. Opencode — `{config}/opencode/…`

### Adding a server (`/mcp add`)

Three entry points, all converging on the same connect-and-persist path (written to
`mcp.json` as an "Own" server):

```
/mcp add                                # 2-way modal: paste JSON | wizard
/mcp add <name> <command> [args…]       # one-line stdio shorthand (npx handled on Windows)
/mcp add {"mcpServers": {"name": {…}}}  # paste a Claude-Desktop-style JSON blob
```

The quick shorthand only covers **stdio** servers. For **http**/**sse** transports,
or servers needing env vars / headers, use the wizard or paste JSON (any parse
failure falls back to the wizard modal with an explanation).

Transports: **stdio** (subprocess, JSON over stdin/stdout, 60s/req), **http**
(POST JSON, tracks `MCP-Session-Id`, 30s), **sse** (`text/event-stream`).

### Cache & reload

```
/mcp ttl <secs>   # tools/list cache TTL (1–86400; default 300), persisted to config
/mcp reload       # force a fresh tools/list on all servers
```

### OAuth (`/mcp auth`)

For HTTP/SSE servers that answer 401, `/mcp auth` (alias `/mcp oauth`) filters the
list to servers needing authorization; Enter on one runs the full RFC 9728/8414/7591
+ PKCE flow — it opens your browser, captures the redirect on a local port, and
exchanges tokens. Tokens are stored **encrypted in the OS keyring** (never in
`mcp.json`), and any client rebuild (connect/toggle/reconnect) picks them up
automatically, refreshing when near expiry. Only authorization servers supporting
RFC 7591 dynamic client registration work today; one requiring a pre-registered
`client_id` fails with a clear error. Linux needs a working Secret Service backend.

### Semantic tip

With many tools connected, PoopRusteek defers MCP schemas — the system prompt carries
only a per-server summary (`playwright (25 tools)`), and full definitions arrive via
per-turn hints or the `tool_search` builtin. See the `pooprusteek-skills-and-rag` skill.
