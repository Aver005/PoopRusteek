# PLANS
> Roadmap, active priorities, and ideas.
> Last updated: 2026-06-30

## ACTIVE (`[WIP]`)

| Priority | What | Why |
|----------|------|-----|
| P0 | Multi-theme support | Only Catppuccin Mocha; `ui.theme` is currently ignored |
| P1 | Error recovery polish | Retry/backoff exists but no jitter, no total-time cap, no `Retry-After` |
| P1 | GOAL hard iteration cap | Prevent infinite agent↔evaluator loops |
| P2 | Mouse support | Scroll, click-to-select in TUI |
| P2 | Copy/paste | System clipboard integration |

## SHORT-TERM (`[TODO]` — next)

| Priority | What | Why |
|----------|------|-----|
| P0 | **Auto-load `.memories/` into the agent** | The "Integrate memories" commit only created the docs; `build_system_prompt` still doesn't read them. Goal: agents understand the project cold. (`app/mod.rs:1480`) |
| P1 | MCP tool-arg schema validation | Args passed unchecked to `tools/call` |
| P1 | `cargo clippy` clean pass | Lint debt |
| P1 | RAG / codebase search | Semantic search across project files |
| P2 | Test infrastructure | Smoke tests + parser/provider unit tests |
| P2 | Token usage tracking | DeepSeek streaming returns none; currently estimated `len/4` |

## LONG-TERM

| What | Why |
|------|-----|
| Multi-provider (OpenAI, Anthropic, local) | `ProviderKind` already has the slots; vendor independence |
| Plugin system | Third-party tool extensions |
| Remote session sharing | Multi-device workflow |
| Windows MSI installer + GitHub Actions CI/CD | Distribution (Phase 5) |

## IDEAS (`[IDEA]`)

- `[IDEA]` Inline image rendering in TUI (Sixel / Kitty protocol)
- `[IDEA]` Voice input via whisper.cpp
- `[IDEA]` Richer VSCode integration over ACP
- `[IDEA]` Built-in git integration (auto-commit suggestions, diff view)
- `[IDEA]` Sub-agents for parallel tasks
- `[IDEA]` Structured (JSON) GOAL verdict instead of markdown `**Status:**` parsing

## DECIDED AGAINST

- `[IDEA]` Electron/GUI wrapper — TUI is the identity, keep it terminal-native
- `[IDEA]` Database-backed sessions — JSON files are simpler and debuggable
