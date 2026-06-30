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

A free, terminal-native alternative to Claude Code — with parallel chats, sub-agents, an iterative goal loop, MCP, and skills.

[![Rust](https://img.shields.io/badge/Rust-edition_2024-CE412B?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![MSRV](https://img.shields.io/badge/MSRV-1.85-blue)](https://www.rust-lang.org)
[![TUI](https://img.shields.io/badge/TUI-ratatui-7C3AED)](https://ratatui.rs)
[![Platform](https://img.shields.io/badge/platform-Windows_·_macOS_·_Linux-success)]()
[![License](https://img.shields.io/badge/license-MIT-green)](#-license)

</div>

---

Pooprusteek is a Rust rewrite of the TypeScript [**Poopseek**](https://github.com/aver005/poopseek). It talks to DeepSeek's web API directly (no paid API key), streams responses token-by-token, runs shell tools, speaks MCP, and lets you drive **several conversations and background agents at once** — all inside a single, snappy terminal UI.

> [!NOTE]
> Pooprusteek uses the **reverse-engineered DeepSeek web API** (chat.deepseek.com), not the official API-key product. It needs a session token and solves DeepSeek's SHA-3 proof-of-work locally. It's unofficial and may break if the upstream API changes.

## ✨ Highlights

### 🗂️ Parallel conversations, no lost streams
Each chat is an isolated conversation with its **own forked session** — turns never collide.
- **`/new` + `/chats`** — open and switch between parallel sessions; `Tab` / `Shift+Tab` cycles focus.
- **`/btw <question>`** — fire a one-shot side-question that answers in the background without disturbing your main turn.
- Background chats keep streaming while you work elsewhere; the status bar shows how many are live.

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

### 🔌 MCP & 🧩 Skills
- **Model Context Protocol** over **stdio / HTTP / SSE**, auto-discovered from **8 config sources** (own, workspace, Claude Desktop, VS Code, Claude CLI, Cursor, Opencode…). Browse with **`/mcp`**.
- **Skills** — reusable markdown instruction sets injected into the system prompt on demand. Manage with **`/skills`**.

### 💅 Polished terminal UX
Markdown + syntax highlighting, streaming with a live thinking indicator, multi-line input with history, `@file` mentions, session save/load, Markdown export/import, and a Catppuccin Mocha theme — all on an **event-driven loop with no render races**.

## 📦 Installation

```bash
cargo install --path .
```

Requires a Rust toolchain (edition 2024, MSRV 1.85).

## 🚀 Usage

```bash
pooprusteek            # launch the TUI
pooprusteek --acp      # run as an ACP server (JSON-RPC over stdio, for IDEs)
pooprusteek --debug_log  # write a debug log to .dev/debug.log
```

On first launch, an onboarding flow helps you set your DeepSeek token and model.

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
| `/clear` · `/home` | Clear history · return to the landing screen |
| `/compact` | Summarize history to shrink context |
| `/reset` · `/last` | Reset session · open the most recent session |
| `/load <id>` · `/sessions` · `/session` | Load a session · list sessions · current session info |
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
| `/mcp [ttl <s>\|reload]` | Open the MCP view · set cache TTL · reload servers |
| `/skills [enable\|disable <name>]` | Manage skills |

</details>

<details>
<summary><b>Settings & misc</b></summary>

| Command | Description |
|---------|-------------|
| `/rate <ms>` | Min delay between API requests (0 = off) |
| `/retry <N\|on\|off\|-1>` | Max retries on API failure (-1 = infinite) |
| `/help` · `/version` · `/quit` | Help · version · exit |

</details>

## ⚙️ Configuration

Config lives at `{config_dir}/pooprusteek/config.toml` — e.g. `~/.config/pooprusteek/config.toml` on Linux, `%APPDATA%\pooprusteek\` on Windows, `~/Library/Application Support/pooprusteek/` on macOS. Sessions, history, and `mcp.json` live under the platform data directory.

```toml
[provider]
kind = "deepseek"
token = "your-deepseek-token"
model = "deepseek-chat"
temperature = 0.7
max_tokens = 4096

[ui]
theme = "default"
show_status_bar = true

[agent]
max_steps_per_turn = 256
max_tools_per_step = 10
max_context_messages = 256
rate_limit_ms = 0
max_retries = 0      # -1 = infinite, 0 = none, N = N+1 attempts
```

## 🏗️ Architecture

Pooprusteek runs on a single `tokio::select!` event loop. The agent runs in a **spawned task** and communicates only through `AppEvent`s and channels — it never touches UI state directly, so there are no render races.

```
main ─ App (thin coordinator on a single select! loop)
        ├─ AppState
        │    └─ Conversations ── focused + background chats
        │         each owns: messages · forked provider/session · agent task
        ├─ AgentRuntime.spawn(TurnSpec) ── the one place a turn launches
        │    └─ run_agent_loop ── LLM ↔ tools (events tagged by ConversationId)
        │         └─ task tool ⇒ sub-agent (foreground) | detached (background)
        ├─ ToolRegistry ── bash · powershell · shell_* · skill · mcp__*
        ├─ MCPManager   ── external tool servers (stdio / http / sse)
        ├─ DeepseekProvider ── PoW + SSE streaming; fork() ⇒ isolated session
        └─ TUI render ── reads the focused conversation; never mutates
```

```
src/
├── main.rs        — entry point, CLI flags (--acp, --debug_log)
├── app/           — coordinator: conversations, event loop, runtime, keys, goal, multichat
├── provider/      — LLM providers (DeepSeek web API: auth, PoW, SSE, sessions)
├── agent/         — agent loop, sub-agent runner, tool-call parser
├── tools/         — shell tools, background/PTY process registry, skills
├── mcp/           — MCP clients, transports, config discovery, manager
├── tui/           — ratatui UI: views, widgets, theme, markdown
├── commands/      — 30 slash commands (one file each)
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
cargo test --bin pooprusteek
```

## 🙏 Acknowledgements

- [**Poopseek**](https://github.com/aver005/poopseek) — the original TypeScript project this is a rewrite of.
- [ratatui](https://ratatui.rs), [crossterm](https://github.com/crossterm-rs/crossterm), [tokio](https://tokio.rs), and the broader Rust ecosystem.

## 📄 License

MIT
