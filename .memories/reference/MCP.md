# REFERENCE: MCP (Model Context Protocol)
> Dynamic external tools/resources. Source: `src/mcp/`.
> Last updated: 2026-07-04 (added OAuth authorization — see AUTHORIZATION section)

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

## AUTHORIZATION (`/mcp auth` / `/mcp oauth`) — added 2026-07-04

An HTTP/SSE server's 401 during `connect_one`'s `client.initialize()` now
surfaces as `AppError::McpAuthRequired { www_authenticate }`
(`transport.rs`: both `HttpTransport`/`SseTransport::send_request` check
`status == 401` via shared `auth_required_error` before their usual body
handling) and maps to `MCPServerStatus::AuthRequired(hint)` (`manager.rs`
`connect_one`) — `hint` is the `resource_metadata` URL parsed out of
`WWW-Authenticate` (`oauth::extract_resource_metadata`), if the server sent
one (RFC 9728 §5.1).

- **`/mcp auth` / `/mcp oauth`** → `CommandResult::OpenMcpAuth` →
  `McpViewState.auth_mode = true`. The server list (`McpViewState.servers`)
  stays the full unfiltered list (kept live by `McpStatus::update_stats`'s 2s
  poll); `McpViewState::visible_indices()` filters to `needs_auth` servers
  only while `auth_mode` is set, and both `app/keys.rs::handle_mcp_auth_key`
  and `tui/render.rs::render_mcp_list` index through it instead of the raw
  list.
- **Enter** on a picked server: `app/keys.rs::handle_mcp_auth_key` reads
  `MCPManager::oauth_context(name)` (config + hint), spawns
  `mcp::oauth::run_flow(base_url, hint)` off the event loop, then
  `mcp::oauth_store::save(name, tokens)` on success. Result comes back as
  `AppEvent::McpOAuthResult { server, result }`; `Ok` triggers the existing
  `reconnect_server(name)` (its outcome reuses the pre-existing
  `AppEvent::McpOperationDone`, not a new event).
- **`mcp/oauth.rs`**: `discover()` — RFC 9728 protected-resource metadata
  (`{origin}/.well-known/oauth-protected-resource{path}`, falling back to
  the bare-origin well-known path, then to treating the MCP server itself as
  its own AS) → RFC 8414 AS metadata (`{as}/.well-known/oauth-authorization-server{path}`,
  well-known **before** the path) or OIDC discovery
  (`{as}/{path}/.well-known/openid-configuration`, well-known **after** the
  path — RFC 8414 and OIDC disagree on this insertion rule, a real interop
  wrinkle, not a bug) → `register_client()` (RFC 7591 dynamic client
  registration, public client, `token_endpoint_auth_method: "none"`; **no
  DCR support on the server = hard error**, not a silent no-op — manual
  `client_id` config is an unimplemented follow-up) → PKCE S256 (`sha2`) →
  local `TcpListener` on `127.0.0.1:0` for the redirect (`open::that` opens
  the browser) → hand-parsed single `GET /callback?...` request (no new
  HTTP-server dependency; query parsed via `reqwest::Url::query_pairs()`,
  not manual percent-decoding) → token exchange. `oauth::refresh()` reuses
  the same token endpoint for `grant_type=refresh_token`.
- **`mcp/oauth_store.rs`**: `TokenSet` (access/refresh/expiry/token_endpoint/
  client_id) persisted one-per-server in the OS keyring (`keyring` crate,
  service `"pooprusteek-mcp"`, account = server name) — **never** touches
  `mcp.json` or any plaintext file. All keyring calls run on
  `spawn_blocking` (the crate's API is synchronous). `load`/`save` are
  best-effort: any failure (missing entry, no Secret Service on a headless
  Linux box, corrupt blob) just looks like "never authorized" rather than a
  hard error.
- **`build_client`** (manager.rs) — now the *only* place any HTTP/SSE
  transport client is constructed (previously `add_server` duplicated the
  `Http`/`Sse` match arms instead of calling it). For `Http`/`Sse` it calls
  `oauth_store::with_bearer_header(name, headers)` first, which loads a
  stored token (refreshing it first if `TokenSet::is_expiring_soon()` and a
  refresh token exists), and merges in `Authorization: Bearer <token>` if one
  was found. This means every path that (re)builds a client — initial
  connect, `toggle_server`, `reconnect_server` — automatically picks up a
  newly stored token with zero other manager changes.

### AUTHORIZATION GOTCHAS

- Only authorization servers supporting RFC 7591 dynamic client registration
  are supported today; a server requiring a pre-registered `client_id` fails
  the flow with a clear error.
- `keyring` needs a working OS credential backend (Secret Service/D-Bus on
  Linux); headless boxes without one will never successfully store a token —
  `/mcp auth` will keep offering the server indefinitely in that case.
- `McpViewState.selected` in `auth_mode` indexes into the *filtered* list
  (via `visible_indices()`), not `servers` directly — don't compare it
  against a raw `servers` index outside that helper.
- **`keyring` 4.1.3's own `v1::Entry::new` auto-init is broken** (verified
  against its `src/v1.rs`): it only calls its internal
  `set_credential_store()` when
  `AtomicBool::compare_exchange(false, true, ..) == Ok(true)`, but a
  successful CAS from `false` always returns `Ok(false)` (the *previous*
  value) — that branch can never run, so `Entry::new` always fails with
  `Error::NoDefaultStore` ("No default store has been set..."), and the
  underlying flag flip means retrying never helps within the same process.
  Worked around in `oauth_store.rs::ensure_default_store` (called from
  `entry()` before every `keyring::Entry::new`): does the same one-time init
  ourselves via `std::sync::Once` + `keyring_core::set_default_store`,
  using the same per-target backend crates keyring's own `v1` feature
  already compiles in (`windows-native-keyring-store` /
  `apple-native-keyring-store` / `zbus-secret-service-keyring-store`, added
  as direct deps in `Cargo.toml`'s `[target...]` tables — no new dependency
  edges). If `keyring` ships a fix upstream, this workaround (and the extra
  direct deps) can be dropped.
