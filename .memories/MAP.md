# MAP
> Codebase map — file → purpose. Agent navigation aid.
> Last updated: 2026-06-28T17:12

## ENTRY

| File | Purpose | Key Lines |
|------|---------|-----------|
| `src/main.rs` | CLI parsing, init, launch App or ACP | 1-100 |
| `src/error.rs` | `AppError` enum, `AppResult<T>` | All |
| `src/config/mod.rs` | Config struct, TOML I/O | All |

## APP LAYER

| File | Purpose | Key Lines |
|------|---------|-----------|
| `src/app/mod.rs` | Event loop, state, input, autocomplete, MCP view, sessions | ~1800L |
| `src/app/events.rs` | `AppEvent`, `Modal`, `PickerState`, `QuestionState` | All |

## PROVIDER (Domain)

| File | Purpose | Key Lines |
|------|---------|-----------|
| `src/provider/mod.rs` | `LLMProvider` trait, `ChatMessage`, types | All |
| `src/provider/deepseek.rs` | DeepSeek web API — full reverse-engineered client | ~1800L |
| `src/provider/pow.rs` | SHA-3 PoW solver via WASM | ~245L |
| `src/provider/types.rs` | API response types (SSE events, etc.) | ~637L |

## AGENT (Domain)

| File | Purpose | Key Lines |
|------|---------|-----------|
| `src/agent/runner.rs` | Agent orchestrator — multi-step tool conversation | All |
| `src/agent/tool_parser.rs` | Parse tool calls from LLM (XML/JSON/legacy) | All |

## TOOLS

| File | Purpose | Key Lines |
|------|---------|-----------|
| `src/tools/mod.rs` | `Tool` trait, `ToolDefinition`, `ToolResult` | All |
| `src/tools/registry.rs` | `ToolRegistry` — register, resolve, execute | All |
| `src/tools/bash.rs` | Bash execution tool | All |
| `src/tools/powershell.rs` | PowerShell execution tool | All |
| `src/tools/question.rs` | User question dialog tool | All |
| `src/tools/background.rs` | Background PTY process management | All |
| `src/tools/shell_control.rs` | Shell output/kill/list/input tools | All |
| `src/tools/skill.rs` | Custom skill adapter | All |

## MCP (Infrastructure)

| File | Purpose | Key Lines |
|------|---------|-----------|
| `src/mcp/client.rs` | JSON-RPC 2.0 MCP client | All |
| `src/mcp/transport.rs` | Stdio + HTTP transports | All |
| `src/mcp/config.rs` | Multi-source config discovery (5 sources) | All |
| `src/mcp/manager.rs` | Server lifecycle, tool resolution | All |
| `src/mcp/jsonrpc.rs` | JSON-RPC wire types | All |
| `src/mcp/types.rs` | MCP domain types | All |

## TUI (Presentation)

| File | Purpose | Key Lines |
|------|---------|-----------|
| `src/tui/mod.rs` | Terminal init/restore | All |
| `src/tui/render.rs` | Main render function (all views) | ~1288L |
| `src/tui/theme.rs` | Catppuccin Mocha theme | All |
| `src/tui/markdown.rs` | Markdown + syntax highlight renderer | All |
| `src/tui/widgets/chat.rs` | Chat history scroll widget | All |
| `src/tui/widgets/input.rs` | Input box with cursor/selection | All |
| `src/tui/widgets/panel.rs` | Stats panel | All |
| `src/tui/widgets/status.rs` | Status bar | All |

## COMMANDS

| File | Purpose |
|------|---------|
| `src/commands/mod.rs` | `Command` trait, `CommandRegistry` |
| `src/commands/defs/` | 23 command impls (one file each) |
| `src/commands/defs/goal.rs` | `/goal` — toggle GOAL mode |

## ACP

| File | Purpose |
|------|---------|
| `src/acp/server.rs` | ACP protocol server (ND-JSON over stdio) |
| `src/acp/types.rs` | ACP protocol types |

## SKILLS

| File | Purpose |
|------|---------|
| `src/skills/mod.rs` | Skill trait |
| `src/skills/discovery.rs` | Skill discovery from config |

## CLI

| File | Purpose |
|------|---------|
| `src/cli/onboarding.rs` | First-launch setup flow |
| `src/cli/file_mentions.rs` | `@file:line` expansion |

## ROOT

| File | Purpose |
|------|---------|
| `assets/prompts/` | System prompt library |
| `.docs/` | Human-readable documentation |

## CROSS-REFERENCES

- Provider ↔ Agent: `runner.rs` calls `LLMProvider::complete_stream()`
- Agent ↔ Tools: `runner.rs` → `ToolRegistry::execute()`
- App ↔ MCP: `app/mod.rs` manages `MCPManager` lifecycle
- App ↔ TUI: `app/mod.rs` calls `render.rs` each frame
