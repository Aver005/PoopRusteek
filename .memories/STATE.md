# STATE
> Live project snapshot. Update on every meaningful change.
> Last updated: 2026-06-30

## PHASE COMPLETION

| Phase | Status | What |
|-------|--------|------|
| 1 Core | `[DONE]` | TUI, provider trait, DeepSeek client, agent loop, tools, MCP types, PoW, streaming |
| 2 Features | `[DONE]` | Onboarding, sessions, 25 slash commands, markdown+syntect, compaction, @file, tool approval, input history |
| 3 Integration | `[DONE]` | MCP stdio/HTTP/SSE, 8-source auto-discovery, manager+caching, JSON-RPC, ACP server mode |
| 3.5 Agentic | `[DONE]` | GOAL mode (2-agent iterative loop), background/interactive PTY jobs, `/jobs` `/ps`, skills system |
| 4 Polish | `[WIP]` | Multi-theme, mouse, copy/paste, error recovery, rate limiting (retry/backoff exists), schema validation |
| 5 Distribution | `[TODO]` | Release builds, cross-compile, installers, CI/CD, man page |

## BUILD STATUS

| Check | Status |
|-------|--------|
| `cargo build` | ✅ Passes |
| `cargo clippy` | ⚠️ Not verified / not clean |
| Tests | Minimal — only `agent/runner.rs` + `agent/tool_parser.rs` have unit tests |

## CURRENT FOCUS

1. Phase-4 polish (multi-theme on hold; mouse, copy/paste, error recovery)
2. Hardening GOAL mode (add a hard iteration cap)
3. MCP tool-arg schema validation

## KNOWN GAPS

- **`.memories/` is not auto-loaded** by the agent (verified: nothing in `src/` reads it). The "Integrate memories" commit only created the docs.
- Single provider (DeepSeek only); `openai`/`custom` kinds declared but unimplemented.
- Tool-approval modal blocks the event loop while open.
- No max-iteration cap in GOAL cycle → infinite-loop risk.
- No schema validation on MCP tool arguments.
- DeepSeek streaming never reports token usage (`usage` always None).
- Retry loop with `max_retries=-1` can hang forever (no total-time cap, no jitter, no `Retry-After`).
- Theme hardcoded (Catppuccin Mocha); `ui.theme` ignored.
- No persistent RAG / codebase search.

## RECENT MILESTONES

| Date | Event |
|------|-------|
| 2026-06-24 | Project inception — core, features, integration built |
| 2026-06-28 | `.memories` system created (`32adf27`); GOAL mode + `/jobs` + `/ps` + PTY jobs (`e801dbe`) |
| 2026-06-30 | `.memories` deeply enriched: added ARCHITECTURE/GLOSSARY/CONVENTIONS + `reference/` (COMMANDS/PROVIDER/TOOLS/MCP/CONFIG/PROMPTS); corrected drift (commands 22→25, MCP sources 5→8, agent defaults 25/50→256/10) |

## FACTS CORRECTED THIS PASS (were wrong in older memory)

- Agent defaults: `max_steps=256`, `max_tools_per_step=10` (was "25 / 50").
- Command count: **25** (+2 aliases) (was "22–23").
- MCP config discovery: **8 sources** (was "5").
- Built-in tools: **7 default** + `skill` (background/interactive PTY family is substantial).
