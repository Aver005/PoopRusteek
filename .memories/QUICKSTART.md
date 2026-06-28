# QUICKSTART
> Agent: read this first. ~10s.

## WHAT

**pooprusteek** — Rust TUI coding agent. DeepSeek-powered alternative to Claude Code.
- Lang: Rust (edition 2024, MSRV 1.85)
- TUI: ratatui + crossterm
- Build: `cargo build` | `cargo run` | `cargo run -- --help`

## WHERE

| Path | Role |
|------|------|
| `src/main.rs` | Entry, CLI args |
| `src/app/mod.rs` | Event loop, state, key handling |
| `src/provider/deepseek.rs` | DeepSeek web API client (biggest file, ~1800L) |
| `src/agent/runner.rs` | Agent loop — tool calls, streaming |
| `src/tools/registry.rs` | Tool registration & dispatch |
| `src/mcp/` | MCP client (stdio + HTTP) |
| `src/tui/` | Terminal UI (ratatui widgets) |
| `src/config/mod.rs` | Config load/save (TOML) |
| `assets/prompts/` | System prompt files |

## KEY ARCHITECTURE

```
main → App (event loop)
         ├─ AgentRunner (LLM ↔ tools)
         ├─ ToolRegistry (bash, powershell, question, MCP, skills)
         ├─ MCPManager (MCP server lifecycle)
         └─ TUI (render, input, modals)
```

## CRITICAL NOTES

- DeepSeek API requires PoW (SHA-3 via WASM) — see `src/provider/pow.rs`
- Agent parses tool calls from LLM text output (XML/JSON/legacy) — see `src/agent/tool_parser.rs`
- MCP auto-discovers config from 5 sources (workspace, global, Claude, VS Code, Cursor, Trae)
- No test suite exists yet. Verification = `cargo build`
- Config file: `~/.config/pooprusteek/config.toml`
- Sessions saved as JSON in `~/.local/share/pooprusteek/sessions/`
