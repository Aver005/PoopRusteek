<div align="center">

```
██████╗  ██████╗  ██████╗ ██████╗ ██████╗ ██╗   ██╗███████╗████████╗███████╗███████╗██╗  ██╗
██╔══██╗██╔═══██╗██╔═══██╗██╔══██╗██╔══██╗██║   ██║██╔════╝╚══██╔══╝██╔════╝██╔════╝██║ ██╔╝
██████╔╝██║   ██║██║   ██║██████╔╝██████╔╝██║   ██║███████╗   ██║   █████╗  █████╗  █████╔╝
██╔═══╝ ██║   ██║██║   ██║██╔═══╝ ██╔══██╗██║   ██║╚════██║   ██║   ██╔══╝  ██╔══╝  ██╔═██╗
██║     ╚██████╔╝╚██████╔╝██║     ██║  ██║╚██████╔╝███████║   ██║   ███████╗███████╗██║  ██╗
╚═╝      ╚═════╝  ╚═════╝ ╚═╝     ╚═╝  ╚═╝ ╚═════╝ ╚══════╝   ╚═╝   ╚══════╝╚══════╝╚═╝  ╚═╝
```

### Пупра́стик · a fast, beautiful TUI coding agent in Rust, powered by DeepSeek

A free, terminal-native alternative to Claude Code — with parallel chats, sub-agents, an iterative goal loop, MCP with OAuth, multiple LLM providers, and a **fully local RAG layer** over skills, tools, and your own conversation history.

[![Rust](https://img.shields.io/badge/Rust-edition_2024-CE412B?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![MSRV](https://img.shields.io/badge/MSRV-1.91-blue)](https://www.rust-lang.org)
[![TUI](https://img.shields.io/badge/TUI-ratatui-7C3AED)](https://ratatui.rs)
[![Platform](https://img.shields.io/badge/platform-Windows_·_macOS_·_Linux-success)]()
[![License](https://img.shields.io/badge/license-MIT-green)](#-license)

</div>

---

Pooprusteek is a Rust rewrite of the TypeScript [**Poopseek**](https://github.com/aver005/poopseek). It talks to DeepSeek's web API directly (no paid API key), streams responses token-by-token, runs shell tools, speaks MCP, and lets you drive **several conversations and background agents at once** — all inside a single, snappy terminal UI. When you outgrow DeepSeek, plug in any OpenAI-compatible, Anthropic, or Gemini endpoint via `/providers`.

> [!NOTE]
> Pooprusteek uses the **reverse-engineered DeepSeek web API** (chat.deepseek.com), not the official API-key product. It needs a session token and solves DeepSeek's SHA-3 proof-of-work locally. It's unofficial and may break if the upstream API changes.

## ✨ Highlights

### 🗂️ Parallel conversations, no lost streams
Each chat is an isolated conversation with its **own forked session** — turns never collide.
- **`/new` + `/chats`** — open and switch between parallel sessions; `Tab` / `Shift+Tab` cycles focus.
- **`/btw <question>`** — fire a one-shot side-question that answers in the background without disturbing your main turn.
- Background chats keep streaming while you work elsewhere; the status bar shows how many are live.

### 🧠 Local RAG — the agent finds its own skills, tools, and memories
A **fully offline semantic layer**: multilingual embeddings (e5-small, quantized ONNX via [fastembed](https://github.com/Anush008/fastembed-rs)) + Snowball-stemmed keyword TF-IDF, fused with Reciprocal Rank Fusion. The model (~120 MB) downloads **once** on first launch into the data directory — zero network after that. Works cross-language: a Russian prompt matches English descriptions and vice versa.

Three corpora, one engine:
- **Skills** — every outgoing prompt is matched against the whole skill catalog; the top hits ride along as an ephemeral hint ("this skill may apply — load it with the `skill` tool"). The model discovers capabilities instead of you enabling them by hand.
- **MCP tools** — same matching, plus **deferred schemas**: with many tools connected (one Playwright server ≈ 25 tools), the system prompt carries only a *server-level summary* (`playwright (25 tools)`) — individual tools aren't enumerated at all, saving thousands of tokens per request. Full definitions arrive exactly when needed — inlined into the hint for matched tools, or on demand via the **`tool_search`** builtin. Never bricks: with embeddings unavailable it degrades to lexical search, and with RAG disabled full schemas return automatically.
- **Conversation history** — every saved session is chunked, embedded, and persisted (`data_dir/semantic/history.json`, incremental with per-session watermarks). Search it yourself with **`/search`** — a dedicated screen with sorting (relevance / newest / oldest), role filters, and per-session dedup — or let the agent recall past solutions itself via the **`history_search`** tool. Enter on a result loads that session.

Control it with **`/rag`** (status), **`/rag on|off`**, **`/rag reload`** (re-verify the model, re-embed everything). Retrieval quality is guarded by an MRR eval harness (`cargo test semantic::eval -- --ignored --nocapture`): skills **0.927**, MCP tools **0.836**.

### 🔀 Providers — DeepSeek free tier to anything
The built-in DeepSeek web client is the default; **`/providers`** manages any number of extra endpoints speaking three wire protocols:
- **OpenAI Chat Completions** — LM Studio, Ollama `/v1`, vLLM, OpenRouter, …
- **Anthropic Messages** — the Claude API and compatible proxies (modern no-sampling models handled correctly).
- **Google Generative Language** — Gemini.

Add via a step-by-step wizard or one line (`/providers add <name> <base_url> [model] [api_key]`), switch instantly from the panel, pick models with **`/models`** (validated against the provider's live list).

### 🤖 Sub-agents
Spin off isolated agents for sub-tasks — inspired by Claude Code.
- The **model** can spawn them itself via the `task` tool; **you** can via **`/agent <task>`**.
- **Foreground** by default (only the conclusion returns into your turn — clean context), with **`background:true`** opt-in that detaches and notifies on completion.
- **`/agents`** lists running agents and lets you stop them.

### 🎯 GOAL mode — iterate until done
**`/goal`** arms a two-agent loop: a worker pursues your goal, then a separate **evaluator** judges whether it's actually met, feeding back concrete fixes until it passes. Sessions auto-swap after repeated failures to escape dead ends.

### 🛠️ Real tooling
- **Shell** (`bash` / `powershell`) — foreground, background, and **interactive PTY** processes.
- **`/jobs` · `/ps`** — list, kill, and prune background jobs; persistent jobs (dev servers) survive turns with an idle TTL.
- **Tool approval** with a **`/whitelist`** for auto-approving trusted tools.
- **`tool_search` · `history_search` · `skill`** — the semantic builtins that let the model navigate its own capability space.

### 🔌 MCP & 🧩 Skills
- **Model Context Protocol** over **stdio / HTTP / SSE**, auto-discovered from **8 config sources** (own, workspace, Claude Desktop, VS Code, Claude CLI, Cursor, Opencode…). Browse with **`/mcp`**.
- **`/mcp add`** — paste a JSON config, walk a wizard, or one-line it (`/mcp add <name> <command> [args…]`).
- **OAuth authorization** (`/mcp auth`) — full RFC 9728/8414/7591 + PKCE flow for servers that need it; tokens live **encrypted in the OS keyring**, never in plaintext config.
- **Skills** — reusable markdown instruction sets (`SKILL.md`), discovered from a dozen agent-ecosystem directories (`.skills/`, `.claude/skills/`, `~/.agents/skills/`, Cursor/Windsurf/Cline/Codex dirs, …). Enable manually with **`/skills`** to pin one into the system prompt — or let the semantic matcher suggest them per-prompt automatically.

### 💅 Polished terminal UX
Markdown + syntax highlighting, streaming with a live thinking indicator, multi-line input with history, `@file` mentions, session save/load, Markdown export/import, a dedicated search screen, and a Catppuccin Mocha theme — all on an **event-driven loop with no render races**. Logs go to a file in the data dir, never over your screen.

## 📦 Installation

```bash
cargo install --path .
```

Requires a Rust toolchain (edition 2024, MSRV 1.91).

## 🚀 Usage

```bash
pooprusteek            # launch the TUI
pooprusteek --acp      # run as an ACP server (JSON-RPC over stdio, for IDEs)
pooprusteek --debug_log  # write a debug log to .dev/debug.log
```

On first launch, an onboarding flow helps you set your DeepSeek token and model.

> [!TIP]
> The first launch also downloads the embedding model (~120 MB) in the background and indexes your saved sessions — the status bar reports progress; everything else is usable meanwhile. Every launch after that is fully offline. Don't want it? `/rag off` or `[semantic] enabled = false`.

## ⌨️ Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `Enter` | Send message (trailing `\` continues on a new line) |
| `Shift+Enter` | Insert a newline |
| `Tab` / `Shift+Tab` | Switch to next / previous chat |
| `Esc` | Cancel generation · clear chat · quit (context-dependent) |
| `Ctrl+C` | Cancel generation / quit |
| `Ctrl+L` | Clear current chat |
| `Ctrl+P` | Toggle the stats panel |
| `Ctrl+A` | Select all input |
| `↑` / `↓` | Input history / scroll |
| `PageUp` / `PageDown` | Scroll history |

## 💬 Commands

<details open>
<summary><b>Chat & sessions</b></summary>

| Command | Description |
|---------|-------------|
| `/new` · `/chats` | Open a new parallel chat · switch between chats |
| `/btw <question>` | One-shot background side-question |
| `/search [query]` | **History-search screen**: semantic + keyword search over all saved sessions — sort (`s`), role filter (`r`), unique-per-session (`u`), Enter opens the found session |
| `/clear` · `/home` | Clear history · return to the landing screen |
| `/compact` | Summarize history to shrink context |
| `/reset` · `/last` | Reset session · open the most recent session |
| `/load <id>` · `/sessions` · `/session` | Load a session · list sessions · current session info |
| `/delete [id]` · `/delete-local [id]` | Delete sessions — account + local, or local-only; multi-select picker with an All/Local/Remote filter and a confirm step |
| `/export [path]` · `/import <path.md>` | Export / import chat as Markdown |
| `/cwd` (`/cd`, `/move`) `<path>` | Change working directory |
| `/attach <path…>` | Attach files to the next message |

</details>

<details open>
<summary><b>Agents & tools</b></summary>

| Command | Description |
|---------|-------------|
| `/agent <task>` · `/agents` | Launch a sub-agent · list and stop running agents |
| `/goal` | Toggle the iterative goal-driven loop |
| `/jobs [list\|kill <id>\|prune]` · `/ps` | Manage background jobs |
| `/tools` · `/whitelist` | List tools · manage tool auto-approval |
| `/mcp [add\|auth\|ttl <s>\|reload]` | MCP view · add a server (JSON / wizard / one-liner) · OAuth-authorize · cache TTL · reload |
| `/skills [enable\|disable <name>]` | Manage skills |
| `/rag [on\|off\|reload]` | Semantic matching: live status · full on/off switch · reinitialize (verify/re-download the model, re-embed skills + MCP tools + history) |

</details>

<details open>
<summary><b>Providers & models</b></summary>

| Command | Description |
|---------|-------------|
| `/providers` | Provider panel: built-in DeepSeek + your OpenAI-compat / Anthropic / Gemini endpoints; Enter activates, `a` adds, `d` removes |
| `/providers add [<name> <base_url> [model] [api_key]]` | Add an endpoint — quick one-liner or step-by-step wizard |
| `/models [id]` | Pick a model from the active provider's live list · or switch directly (validated) |

</details>

<details>
<summary><b>Settings & misc</b></summary>

| Command | Description |
|---------|-------------|
| `/rate <ms>\|<N>/min\|off` | Rate limit: ms between requests and/or max requests per minute |
| `/retry <N\|on\|off\|-1>` | Max retries on API failure (-1 = infinite) |
| `/debug [on\|off]` | Toggle debug logging to `.dev/debug.log` (no args = switch) |
| `/help` · `/version` · `/quit` | Help · version · exit |

</details>

## ⚙️ Configuration

Config lives at `{config_dir}/pooprusteek/config.toml` — e.g. `~/.config/pooprusteek/config.toml` on Linux, `%APPDATA%\pooprusteek\` on Windows, `~/Library/Application Support/pooprusteek/` on macOS. Sessions, history, the semantic index, the embedding model, and `mcp.json` live under the platform data directory.

```toml
[provider]
kind = "deepseek"
token = "your-deepseek-token"
model = "deepseek-chat"
temperature = 0.7
max_tokens = 4096

# Extra endpoints managed via /providers land here as [[providers]] entries
# (protocol = "openai" | "anthropic" | "gemini"); `active_provider` picks one.

[ui]
theme = "default"
show_status_bar = true

[agent]
max_steps_per_turn = 256
max_tools_per_step = 10
max_context_messages = 256
rate_limit_ms = 0
rate_limit_per_minute = 0   # max requests per rolling 60s window (0 = off)
max_retries = 0      # -1 = infinite, 0 = none, N = N+1 attempts

[semantic]
enabled = true          # the whole RAG layer; /rag on|off flips this too
top_k = 3               # max suggestions per corpus per turn
min_dense_score = 0.80  # cosine floor for hint candidates without keyword overlap
mcp_schemas = "auto"    # "auto" (defer above 12 tools) | "full" | "deferred"
```

## 🏗️ Architecture

Pooprusteek runs on a single `tokio::select!` event loop. The agent runs in a **spawned task** and communicates only through `AppEvent`s and channels — it never touches UI state directly, so there are no render races. All CPU-heavy work (ONNX embeddings, SHA-3 proof-of-work) runs on blocking threads.

```
main ─ App (thin coordinator on a single select! loop)
        ├─ AppState
        │    └─ Conversations ── focused + background chats
        │         each owns: messages · forked provider/session · agent task
        ├─ AgentRuntime.spawn(TurnSpec) ── the one place a turn launches
        │    └─ run_agent_loop ── LLM ↔ tools (events tagged by ConversationId)
        │         ├─ semantic hint ── skills + MCP tools matched per prompt
        │         └─ task tool ⇒ sub-agent (foreground) | detached (background)
        ├─ SemanticService ── local hybrid index (e5-small ONNX + TF-IDF + RRF)
        │    └─ corpora: skills · MCP tools · message history (persistent)
        ├─ ToolRegistry ── bash · powershell · shell_* · skill · tool_search
        │                  history_search · mcp__*
        ├─ MCPManager   ── external tool servers (stdio / http / sse, OAuth)
        ├─ providers    ── DeepSeek web (PoW + SSE) · OpenAI-compat · Anthropic · Gemini
        │                  fork() ⇒ isolated session per conversation
        └─ TUI render ── reads the focused conversation; never mutates
```

```
src/
├── main.rs        — entry point, CLI flags (--acp, --debug_log), file-based tracing
├── app/           — coordinator: conversations, event loop, runtime, keys, goal,
│                    multichat, search screen, providers panel
├── provider/      — DeepSeek web API (auth, PoW, SSE, sessions) + OpenAI-compat,
│                    Anthropic Messages, and Gemini protocol clients
├── agent/         — agent loop, sub-agent runner, tool-call parser
├── semantic/      — local RAG: embedder, hybrid index, corpora, history store, evals
├── tools/         — shell tools, PTY process registry, skill / tool_search / history_search
├── mcp/           — MCP clients, transports, config discovery, manager, OAuth
├── tui/           — ratatui UI: views, widgets, theme, markdown
├── commands/      — 35 slash commands (one file each)
├── skills/        — skill discovery + injection
├── acp/           — Agent Client Protocol server mode
└── config/        — configuration schema + storage paths
```

> Want the deep dive? The repo ships a curated knowledge base in [`.memories/`](.memories/INDEX.md) — start at `INDEX.md`.

## 🔨 Building

```bash
cargo build              # debug
cargo build --release    # optimized (LTO, single codegen unit, stripped)
```

## 🧪 Testing

```bash
cargo test --bin pooprusteek                              # 326 unit tests, no network
cargo test semantic::eval -- --ignored --nocapture        # retrieval-quality evals (MRR);
                                                          # downloads the embedding model once
```

## 🛡️ Git hooks

A pre-commit hook (`.githooks/pre-commit`) blocks commits that fail `cargo
fmt -- --check`, `cargo clippy -D warnings`, or `cargo test`. Not wired up
by default — enable it once per clone:

```bash
git config core.hooksPath .githooks
```

Skip it for one commit (CI runs the same gates, so this only delays the
failure, not avoids it):

```bash
git commit --no-verify
```

## 🙏 Acknowledgements

- [**Poopseek**](https://github.com/aver005/poopseek) — the original TypeScript project this is a rewrite of.
- [ratatui](https://ratatui.rs), [crossterm](https://github.com/crossterm-rs/crossterm), [tokio](https://tokio.rs), [fastembed-rs](https://github.com/Anush008/fastembed-rs), and the broader Rust ecosystem.

## 📄 License

MIT
