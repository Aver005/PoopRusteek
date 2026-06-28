# PLANS
> Roadmap, active priorities, and ideas.
> Last updated: 2026-06-28T17:12

## ACTIVE (`[WIP]`)

| Priority | What | Why |
|----------|------|-----|
| P0 | Multi-theme support | Only Catppuccin Mocha currently, users want variety |
| P1 | Error recovery | No retry logic on API failures, no backoff |
| P1 | Rate limiting | DeepSeek may throttle, no client-side limiting |
| P2 | Mouse support | Scroll, click-to-select in TUI |
| P2 | Copy/paste | System clipboard integration |

## SHORT-TERM (`[TODO]` — next)

| Priority | What | Why |
|----------|------|-----|
| P0 | `.memories` integration into agent loop | Agent should auto-read `.memories/INDEX.md` on start |
| P1 | RAG / codebase search | Semantic search across project files |
| P1 | `cargo clippy` pass | Clean up lint warnings |
| P2 | Test infrastructure | At least smoke tests, unit tests for parsers |

## LONG-TERM

| What | Why |
|------|-----|
| Multi-provider support (OpenAI, Anthropic, local) | Vendor independence |
| Plugin system | Third-party tool extensions |
| Remote session sharing | Multi-device workflow |
| Windows MSI installer | Distribution |
| GitHub Actions CI/CD | Automated builds & releases |

## IDEAS (`[IDEA]`)

- `[IDEA]` Inline image rendering in TUI (Sixels / Kitty protocol)
- `[IDEA]` Voice input via whisper.cpp
- `[IDEA]` VSCode extension via ACP protocol
- `[IDEA]` Built-in git integration (auto-commit suggestions, diff view)
- `[IDEA]` Agent spawns sub-agents for parallel tasks

## DECIDED AGAINST

- `[IDEA]` Electron/GUI wrapper — TUI is the identity, keep it terminal-native
- `[IDEA]` Database-backed sessions — JSON files are simpler and debuggable
