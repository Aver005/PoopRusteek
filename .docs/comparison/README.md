# poopseek ↔ pooprusteek — cross-project comparison

A side-by-side study of the two sibling DeepSeek coding agents, written to map where
each can act as a **donor** to the other.

- **poopseek** — the original, mature TypeScript/Bun CLI agent (v1.2.0, ~339 source
  files). Line-based readline UI. Broad feature surface.
- **pooprusteek** — the leaner Rust **TUI** port (v0.1.0, ~75 source files). Full-screen
  ratatui interface. Fewer features, tighter core, some original ideas (GOAL mode).

Generated 2026-06-30 from a file-referenced audit of both repos. Findings are marked
**[verified]** (read in code) or **[inferred]** (from structure/names, not line-confirmed).

## TL;DR

- They are the **same product in two languages.** poopseek was clearly the source; the
  DeepSeek session/stream/prompt code in pooprusteek is a near-line-for-line port.
- **poopseek is far broader**: 47+ commands, 8 LLM providers, RAG, sub-agents, roles,
  sidechat, Figma, ACP, security gate, ~30 structured tools (file/git/grep/memory/todo).
- **pooprusteek is narrower but has two things poopseek lacks**: a real **TUI**, and
  **GOAL mode** (a two-agent worker/evaluator iterate loop).
- **Robustness is split**: poopseek got DeepSeek session-threading right (incremental +
  `finally`); pooprusteek had regressed it (now fixed). Conversely poopseek has an
  input-queue/`ask-user` abort leak that pooprusteek's task-abort model avoids.
- The biggest **structured-tools gap**: pooprusteek has **no** `file.*`/`git`/`grep`/
  `memory`/`todo` tools — it does all file work by shelling out to `bash`/`powershell`.

## How to read this folder

| File | What it answers |
|---|---|
| [feature-parity.md](feature-parity.md) | What exists in each — commands, tools, subsystems, side by side. The matrix. |
| [robustness.md](robustness.md) | Where each is more/less correct — the bug classes (session threading, cancellation, tool-approval, goal loop) and who wins. |
| [donor-roadmap.md](donor-roadmap.md) | Prioritized, bidirectional "port this → there" list with effort sizes and caveats. |

## One-paragraph identity of each

**poopseek** (`C:\Work\.ME\poopseek`): Bun + TypeScript, strict mode, no Node. Readline-style
CLI with a status line. Multi-provider (DeepSeek web, OpenAI-compatible, Claude, Gemini,
Ollama, LM-Studio, OpenRouter, HuggingFace). Rich agent loop with a streaming fenced-code
tool parser, sub-agents, context-manager, RAG (e5-small + BM25 over SQLite FTS5), Figma
design pipeline, ACP (Zed protocol) client+server, a security/permission gate, and ~47
slash commands. Config in `~/.poopseek/` and `~/.config/poopseek/`.

**pooprusteek** (`C:\Work\.ME\pooprusteek`): Rust 2024, tokio + ratatui + crossterm.
Full-screen TUI. Single provider (DeepSeek web; `Fake` for tests; OpenAI/Custom are enum
stubs). Agent loop with an XML `<tool_use>`/legacy `[TOOL:]` parser, shell-centric tools
(bash/powershell + background/PTY process management), MCP (stdio), skills, and **GOAL
mode**. ~25 slash commands. Recently refactored: `AppState` decomposed into tested
sub-state modules; session-threading and the GOAL pipeline hardened.
