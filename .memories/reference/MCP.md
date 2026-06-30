# REFERENCE: MCP (Model Context Protocol)
> Dynamic external tools/resources. Source: `src/mcp/`.
> Last updated: 2026-06-30

## OVERVIEW

MCP lets external servers expose tools/resources to the agent. Tool names are namespaced `mcp__{server}__{tool}` and dispatched in the agent loop when the name starts with `mcp__` (`agent/runner.rs:158`).

## CLIENT & PROTOCOL

- **`MCPClient`** (`mcp/client.rs:10`): wraps a transport + server name + request counter. Constructors `from_stdio` (:17), `from_http` (:40), `from_sse` (:53); disabled servers get a `DummyTransport`.
- **JSON-RPC 2.0** (`mcp/jsonrpc.rs`): `JsonRpcRequest{jsonrpc,id,method,params?,_meta?}`, `JsonRpcResponse{…,result?,error?}`, one-way `Notification`.
- **Protocol version**: `"2024-11-05"` (`client.rs:68`).
- Handshake: `initialize` RPC → `notifications/initialized` → `tools/list`. Tool calls via `tools/call`. Content array flattened to text (`client.rs:216`; images become `[Image: {mime}]`, only text/image/resource handled).

## TRANSPORTS (`mcp/transport.rs`)

| Transport | Mechanics | Timeout |
|-----------|-----------|---------|
| **Stdio** (:20) | spawn subprocess, write JSON+`\n` to stdin, read line from stdout | 60s/req |
| **HTTP** (:137) | reqwest w/ cookie store; POST JSON; tracks `MCP-Session-Id` header; SSE-fallback if body looks like `data:`/`event:` | 30s |
| **SSE** (:232) | expects `text/event-stream`; buffers bytes, splits on `\n\n`, matches by request id; JSON-fallback | — |
| **Dummy** (:440) | always errors "Server is disabled" | — |

**Windows gotcha** (`transport.rs:52`): stdio spawn auto-retries with `.cmd`/`.bat` extensions (so `npx` resolves to `npx.cmd`).

## MANAGER (`mcp/manager.rs:16`)

- `initialize()` (:46): load config → load enabled map → add servers → connect all.
- **Server states** (`types.rs:31`): `Pending | Connecting | Connected | Error(String) | Disabled`.
- **Tool caching** (:142): per-server cache with timestamp; reused while `elapsed < TTL`.
  - **Default TTL = 300s** (`manager.rs:38`); override via `/mcp ttl <secs>` → `config.mcp.cache_ttl`.
- `call_tool(full_name, args)` (:221) · `toggle_server` · `reconnect_server` · `remove_server` · `persist_config()` (:361 → writes `mcp.json`).
- Full tool name built at `manager.rs:186`: `mcp__{server}__{tool}`.

## TYPES (`mcp/types.rs`)

- `MCPServerConfig` (:5): `Stdio{command,args,env?,cwd?} | Http{url,headers} | Sse{url,headers}`.
- `McpServerDef` (:74): `{ enabled(default true), #[flatten] config }`.
- `MCPTool` (:40): `name, description, input_schema, server_name`. `MCPResource` (:48): `uri, name, description`. `MCPToolResult` (:67): `content, is_error`.

## CONFIG DISCOVERY (`mcp/config.rs:34`) — 8 sources, first-found-wins

1. **Own**: `{data}/pooprusteek/mcp.json` (also the source of enabled/disabled state) — `{ "mcpServers": { name: { enabled, …config } } }`.
2. **Workspace**: `./mcp.config.json`, `./.vscode/mcp.json`.
3. **Global**: `{config}/pooprusteek/mcp.config.json`.
4. **Claude Desktop**: `…/Claude/claude_desktop_config.json` (mac: `~/Library/Application Support/Claude/`).
5. **VSCode**: `…/Code/User/settings.json` → `mcp.servers`.
6. **Claude CLI**: `~/.claude/mcp.json` or `{config}/claude/mcp.json`.
7. **Cursor**: `~/.cursor/mcp.json`.
8. **Opencode**: `{config}/opencode/{opencode.json[c], mcp.json, mcp.config.json, opencode.mcp.json}` (two formats supported).

> ⚠️ The original memory said "5 sources" — it's actually **8** as of `config.rs`.

## GOTCHAS

- `mcp__` uses double underscores — name collisions possible if a server/tool name contains `__`.
- Cache may serve stale tools if a server goes offline between TTL windows.
- TTL is only consulted at connect time; `cache_ttl=0` is not specially handled (the `elapsed < 0` check just always misses).
