# MAP
> Codebase map — file → purpose. Navigation aid. (~15k LOC)
> Last updated: 2026-06-30

## ENTRY / ROOT
| File | Purpose | Lines |
|------|---------|-------|
| `src/main.rs` | CLI (`--acp`, `--debug_log`), init order, launch TUI or ACP | 85 |
| `src/error.rs` | `AppError` enum, `AppResult<T>` | — |
| `src/prompts.rs` | `PromptFiles`, asset resolution for prompts | 67 |
| `src/session.rs` | `Session`, save/load/list, tags, history file | 191 |
| `src/debug_log.rs` | Optional `.dev/debug.log` logger | 68 |

## APP LAYER (Application)
| File | Purpose | Lines |
|------|---------|-------|
| `src/app/mod.rs` | **God-file**: App+AppState, event loop, key handling, autocomplete, MCP view, sessions, GOAL mode, helpers | ~2382 |
| `src/app/events.rs` | `AppEvent`, `Modal`, `PickerState`, `QuestionState`, `GoalStage`, `GoalVerdict`, `ToolApprovalRequest` | 474 |
| `src/config/mod.rs` | `Config` schema (provider/ui/agent/mcp/skills), paths, load/save | 134 |

## PROVIDER (Domain)
| File | Purpose | Lines |
|------|---------|-------|
| `src/provider/mod.rs` | `LLMProvider` trait, `ChatMessage`, `Role`, request/response types | 213 |
| `src/provider/deepseek.rs` | DeepSeek web client: auth, ~30 endpoints, session state, prompt build, SSE, retry | ~1819 |
| `src/provider/pow.rs` | SHA-3 PoW solver via `wasmtime` | 245 |
| `src/provider/types.rs` | API/SSE response types, `ParsedSSEEvent` | 637 |

## AGENT (Domain)
| File | Purpose | Lines |
|------|---------|-------|
| `src/agent/runner.rs` | `run_agent_loop` — multi-step LLM↔tool conversation, streaming, approval, summarize | 272 |
| `src/agent/tool_parser.rs` | Parse/strip tool calls (XML / `[TOOL:]` / JSON), stream-visible text | 195 |

## TOOLS (Domain)
| File | Purpose | Lines |
|------|---------|-------|
| `src/tools/mod.rs` | `Tool` trait, `ToolDefinition`, `ToolResult`, interactive/persistent heuristics | 82 |
| `src/tools/registry.rs` | `ToolRegistry` — register/resolve/execute, skill injection | — |
| `src/tools/bash.rs` | bash tool (fg/bg/interactive) | 250 |
| `src/tools/powershell.rs` | powershell tool | 254 |
| `src/tools/question.rs` | question tool (special-cased in agent loop) | — |
| `src/tools/background.rs` | Background + PTY process registry, spawn/read/kill/prune/ttl | 744 |
| `src/tools/shell_control.rs` | `shell_output/kill/list/input` tools; key→escape mapping | 278 |
| `src/tools/skill.rs` | `skill` tool (list/load) | 89 |

## MCP (Infrastructure)
| File | Purpose | Lines |
|------|---------|-------|
| `src/mcp/client.rs` | `MCPClient`, JSON-RPC, content flattening | 248 |
| `src/mcp/transport.rs` | Stdio + HTTP + SSE + Dummy transports | 453 |
| `src/mcp/config.rs` | 8-source config discovery (precedence) | 397 |
| `src/mcp/manager.rs` | Server lifecycle, tool caching/TTL, execution | 374 |
| `src/mcp/jsonrpc.rs` | JSON-RPC 2.0 wire types | — |
| `src/mcp/types.rs` | `MCPTool/Resource/ServerConfig`, states | 101 |

## TUI (Presentation)
| File | Purpose | Lines |
|------|---------|-------|
| `src/tui/mod.rs` | Terminal init/restore | — |
| `src/tui/render.rs` | All views: landing, chat, MCP, modals, status bar | ~1336 |
| `src/tui/theme.rs` | Catppuccin Mocha palette (`Theme`) | — |
| `src/tui/markdown.rs` | Markdown + syntect highlight renderer | 253 |
| `src/tui/widgets/input.rs` | Multi-line input, cursor, selection, wrapping | 269 |
| `src/tui/widgets/chat.rs` | Chat history widget | 199 |
| `src/tui/widgets/panel.rs` | Right stats panel | 202 |
| `src/tui/widgets/status.rs` | Status bar | 108 |

## COMMANDS
| File | Purpose |
|------|---------|
| `src/commands/mod.rs` | `Command` trait, `CommandRegistry`, `CommandResult` |
| `src/commands/defs/*.rs` | 25 commands (one per file) — see `reference/COMMANDS.md` |

## OTHER
| File | Purpose |
|------|---------|
| `src/acp/server.rs` | ACP JSON-RPC-over-stdio server (`--acp`) | 178 |
| `src/acp/types.rs` | ACP request/response/content types |
| `src/skills/mod.rs` | `SkillDefinition`, `SkillSource`, frontmatter parse |
| `src/skills/discovery.rs` | Skill discovery (many dirs), formats | 264 |
| `src/cli/onboarding.rs` | First-launch token/model setup | 79 |
| `src/cli/file_mentions.rs` | `@file:line` expansion | 88 |
| `assets/prompts/` | base/tools/compact/goal-evaluator + persona & figma prompts |
| `assets/sha3_wasm_bg.*.wasm` | DeepSeek PoW solver blob |
| `.docs/` | Human docs (partly aspirational — trust code/`.memories` over it) |

## CROSS-REFERENCES
- Provider ↔ Agent: `runner.rs` → `LLMProvider::complete_stream()`
- Agent ↔ Tools: `runner.rs` → `ToolRegistry::execute()` / `MCPManager::call_tool()`
- App ↔ MCP: `app/mod.rs` owns `Arc<Mutex<MCPManager>>`
- App ↔ TUI: `app/mod.rs` → `render.rs` each frame
- App ↔ Agent: spawned task ↔ main loop via `AppEvent` channels + `Notify` handshakes
