# GLOSSARY
> Project-specific terms a fresh agent will hit. Last updated: 2026-11-07 (шлюз, аккаунты, маршруты, роли, секреты, слои конфига). Before: 2026-06-30 (added conversation/sub-agent/controller terms)

| Term | Meaning |
|------|---------|
| **Pooprusteek / Пупра́стик** | This project: a Rust TUI coding agent powered by DeepSeek's web API. Rust rewrite of **Poopseek** (TS). |
| **Poopseek** | The original TypeScript project this is forked/rewritten from. Repo: github.com/aver005/poopseek. |
| **Provider** | An `LLMProvider` impl: DeepSeek web, an OpenAI-compatible/Anthropic/Gemini `CompatClient`, `FakeProvider` in tests, or the gateway's `RoutedProvider`. |
| **`RoutedProvider`** | The `LLMProvider` the app sees when the gateway is on. Chooses an account, falls back before the first emitted chunk, forks per conversation, records usage. |
| **Gateway** | The account/route layer (`src/gateway/`). Off by default (`[gateway] enabled = false`); when on, `main_provider` returns a `RoutedProvider`. |
| **Account** | One destination with its own credential: an API endpoint, the built-in DeepSeek web client, or a CLI subscription. `id` never changes; `label` is the display name. |
| **Route** | An ordered chain of `Step`s. A step is an `account` plus an optional `model`. The first step that is ready answers. |
| **Step** | One element of a route (`{ account, model? }`). No nested routes. |
| **Fallback trigger** | The failure classes that allow moving to the next step (`rate_limit`, `quota`, `spend_cap`, `overloaded`, `timeout`, `server_error`, `auth`). An empty `on` means all of them. |
| **Cooldown** | An account's temporary block after a failure, derived from `Retry-After` or backoff. Survives a restart only as a hint in `state.json`. |
| **Soft / hard limit** | `soft_pct` moves an account down the order; `hard_pct` removes it from selection. Both only from a fresh `Header`/`CliEvent` snapshot. |
| **Role** | A service call with its own route: `main`, `title` (chat names), `summary` (`/compact`), `evaluator` (GOAL). A role without its own route inherits `main`. |
| **`/pin`** | A chat-level binding to one account/model (`Binding::Fixed`); it does not change the route. |
| **Seam** | The `ui_only` history line inserted on a fallback: `── switched A → B: reason`. Never sent to the model. |
| **CLI account** | A subscription reached through the official binary (`cli:claude`, `cli:opencode`, `cli:codex`). Runs as a **plain model**: our loop, our tools, our approvals. Agent mode is a later release. |
| **Plain model** | The CLI only produces text/tool calls; it does not execute its own tools. Enforced by flags and `Dialect::verify`. |
| **Secret reference** | `keyring:global/name`, `env:NAME`, `cmd:["op","read",…]`, `file:/path` or `none`. A literal key in the shared file is migrated into the keyring. |
| **Secret store** | The OS credential store (Credential Manager / Keychain / Secret Service), service `pooprusteek`; accessed through `SecretStore`. |
| **Project layer** | `<repo>/.pooprusteek/config.toml`. Untrusted: may not hold keys or request targets until `pooprusteek trust`; account keys need `secrets bind`. |
| **`trust`** | `pooprusteek trust` records the project file's path and SHA-256 in the shared config. Editing the file asks again. |
| **Ledger** | Usage accounting at `data_dir/gateway/usage.json`, merged every ~30 s; `/usage` reads it. |
| **`mock-provider`** | Scripted OpenAI-compatible endpoint (`pooprusteek mock-provider --port … --script …`) for deterministic scenarios. |
| **`mock-cli`** | Scripted stand-in for `claude`/`opencode`, used through `POOPRUSTEEK_CLI_*` in the harness. |
| **Conversation** | One independent chat thread (`app/conversation.rs`): owns its messages, **its own forked provider/session**, generation status, and agent task. Kinds: `Main`, `Session`, `Sidechat`, `SubAgent`. |
| **Conversations** | The store of all open conversations + a `focused` id. No live/parked split — every conversation is a full record; switching focus is just changing an id. |
| **Focused conversation** | The one currently rendered and targeted by input/abort. `state.focused()/focused_mut()`. Others are **background** — they keep streaming into their own buffers. |
| **`fork()`** | `LLMProvider::fork()` — returns a fresh-session sibling provider sharing config/token. Gives each conversation an isolated DeepSeek session (no `parent_message_id` cross-talk). poopseek's `provider.clone()` analog. |
| **Sidechat (`/btw`)** | A one-shot background side-answer in its own ephemeral `Sidechat` conversation; streams in without disturbing the main turn. |
| **Sub-agent** | An isolated agent run (`SubAgent` conversation) spawned by the model (`task` tool) or user (`/agent`); foreground returns only its conclusion into the turn, `background:true` detaches + notifies. `/agents` lists/stops. |
| **`task` tool** | Model-driven sub-agent spawn; special-cased in `agent/tools_step.rs` (like `question`), not a normal `Tool`. |
| **AgentRuntime / TurnSpec** | The controller (`app/runtime.rs`) that owns `tools`/`mcp`/`event_tx` and is the single launch point for every agent turn; `TurnSpec` describes one turn. |
| **Controller** | A type that owns its *dependencies* and exposes a narrow API (`AgentRuntime`, `system_prompt::build`, `BackgroundCounters`), so behavior stops reaching into all of `App`. |
| **`auto_approve`** | `TurnSpec` flag: background turns (sidechats/sub-agents) auto-approve tools so they never block on a modal nobody's watching; the focused user turn does not. |
| **DeepSeek web API** | The *unofficial* chat.deepseek.com endpoints (cookie/token auth), NOT the public API-key product. Reverse-engineered, no SLA. |
| **PoW** | Proof-of-Work. DeepSeek requires a solved SHA-3 challenge (`x-ds-pow-response` header) on gated calls. Solved by a bundled WASM blob via `wasmtime`. |
| **Agent loop** | `run_agent_loop` in `agent/runner.rs` — the multi-step LLM↔tool conversation driver. |
| **Tool** | A capability the agent can invoke (`bash`, `powershell`, `question`, `shell_*`, `skill`, or any `mcp__*`). |
| **Tool call** | Parsed from raw LLM text (XML `<tool_use>`, `[TOOL:…]`, or JSON). No native function-calling. |
| **Tool approval** | Modal asking the user to allow a tool call, with three grants: once, this scope, or the whole tool. `/whitelist` lists the saved rules (`approved_tools`: tool + optional `Scope::Command`/`Scope::Path`). |
| **MCP** | Model Context Protocol. External servers exposing tools/resources, namespaced `mcp__{server}__{tool}`. |
| **ACP** | Agent Client Protocol. `--acp` runs Pooprusteek as a JSON-RPC-over-stdio server for an external client (e.g. IDE). |
| **Skill** | A reusable markdown instruction set (`SKILL.md` or `*.prompt.md`) injected into the system prompt when enabled. |
| **GOAL mode** | Two-agent iterative loop: worker (Agent 1) + evaluator (Agent 2) iterate until a stated goal is met. Toggled by `/goal`. |
| **Evaluator / Agent 2** | The non-streaming LLM pass that judges whether Agent 1 met the goal; uses `goal-evaluator.prompt.md`. |
| **`__goal_system__`** | Session tag marking internal evaluator sessions; hidden from `/sessions`. |
| **Background job** | A detached `bash`/`powershell` process tracked in the `BackgroundRegistry` (`tools/background.rs`). |
| **Interactive job** | A background job running in a real PTY (via `portable_pty`); accepts keystrokes via `shell_input`. |
| **Persistent job** | A background job that survives across user turns (e.g. dev servers); has an idle TTL (default 1800s). |
| **Compaction** | Summarizing chat history to fit context (`/compact`, `compact.prompt.md`, `auto_compact`). |
| **`.memories/`** | THIS folder — the human-curated agent knowledge base. NOT auto-loaded by the app; agents must be pointed here. |
| **`### LOCAL MEMORY`** | A label inside the DeepSeek prompt for the history section — unrelated to `.memories/`. Don't confuse them. |
| **Catppuccin Mocha** | The (only) color theme; defined in `tui/theme.rs`. |
| **Landing view** | The empty-state screen with the big "POOPRUSTEEK" logo shown before any messages. |
