# Pooprústeek (Пупра́стик)

A fast, beautiful TUI coding agent written in Rust, powered by DeepSeek.

Fork of [Poopseek](https://github.com/aver005/poopseek) — a free Claude Code alternative.

## Features

- **TUI Interface** — Beautiful terminal UI with Catppuccin Mocha theme
- **DeepSeek Integration** — Native web API with PoW challenge support
- **Streaming Responses** — Real-time token delivery
- **Tool System** — Bash and PowerShell tool execution
- **MCP Support** — Model Context Protocol integration (WIP)
- **Event-Driven Architecture** — No render races, smooth UI

## Installation

```bash
cargo install --path .
```

## Usage

```bash
pooprusteek
```

### Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `Ctrl+C` | Quit |
| `Ctrl+L` | Clear chat |
| `Enter` | Send message |
| `Arrow keys` | Navigate input / scroll |

## Configuration

Config file: `~/.config/pooprusteek/config.toml`

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
```

## Architecture

```
src/
├── main.rs              — Entry point
├── error.rs             — Error types
├── config/              — Configuration
├── app/                 — Application state + event loop
├── provider/            — LLM providers (DeepSeek)
├── agent/               — Agent loop + context
├── tools/               — Tool system (bash, powershell)
├── tui/                 — Terminal UI (ratatui)
└── mcp/                 — MCP integration
```

## Building

```bash
# Debug build
cargo build

# Release build (optimized)
cargo build --release
```

## License

MIT
