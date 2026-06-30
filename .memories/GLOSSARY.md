# GLOSSARY
> Project-specific terms a fresh agent will hit. Last updated: 2026-06-30

| Term | Meaning |
|------|---------|
| **Pooprusteek / Пупра́стик** | This project: a Rust TUI coding agent powered by DeepSeek's web API. Rust rewrite of **Poopseek** (TS). |
| **Poopseek** | The original TypeScript project this is forked/rewritten from. Repo: github.com/aver005/poopseek. |
| **Provider** | An `LLMProvider` impl. Only `DeepseekProvider` exists. |
| **DeepSeek web API** | The *unofficial* chat.deepseek.com endpoints (cookie/token auth), NOT the public API-key product. Reverse-engineered, no SLA. |
| **PoW** | Proof-of-Work. DeepSeek requires a solved SHA-3 challenge (`x-ds-pow-response` header) on gated calls. Solved by a bundled WASM blob via `wasmtime`. |
| **Agent loop** | `run_agent_loop` in `agent/runner.rs` — the multi-step LLM↔tool conversation driver. |
| **Tool** | A capability the agent can invoke (`bash`, `powershell`, `question`, `shell_*`, `skill`, or any `mcp__*`). |
| **Tool call** | Parsed from raw LLM text (XML `<tool_use>`, `[TOOL:…]`, or JSON). No native function-calling. |
| **Tool approval** | Modal asking the user to allow a tool call. `/whitelist` auto-approves chosen tools (`approved_tools`). |
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
