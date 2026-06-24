# Pooprusteek Architecture

## Overview

Pooprusteek is a Rust rewrite of Poopseek — a free Claude Code alternative using DeepSeek's web API. It focuses on clean architecture, performance, and a beautiful TUI experience.

## Design Principles

1. **Separation of Concerns** — Domain, infrastructure, and presentation are strictly separated
2. **Event-Driven** — All state changes flow through a single event channel
3. **Zero-Copy Where Possible** — Minimize allocations in hot paths
4. **Async-First** — All I/O is non-blocking via Tokio
5. **Testable** — Every module can be tested in isolation

## Layer Architecture

```
┌─────────────────────────────────────────────────┐
│                  Presentation                    │
│  ┌─────────┐  ┌──────────┐  ┌───────────────┐  │
│  │   TUI   │  │  Render  │  │   Widgets     │  │
│  └────┬────┘  └────┬─────┘  └───────┬───────┘  │
│       │            │                │           │
├───────┴────────────┴────────────────┴───────────┤
│                  Application                     │
│  ┌─────────┐  ┌──────────┐  ┌───────────────┐  │
│  │   App   │  │  Events  │  │    Config     │  │
│  └────┬────┘  └────┬─────┘  └───────┬───────┘  │
│       │            │                │           │
├───────┴────────────┴────────────────┴───────────┤
│                    Domain                        │
│  ┌─────────┐  ┌──────────┐  ┌───────────────┐  │
│  │  Agent  │  │ Provider │  │    Tools      │  │
│  └────┬────┘  └────┬─────┘  └───────┬───────┘  │
│       │            │                │           │
├───────┴────────────┴────────────────┴───────────┤
│                 Infrastructure                   │
│  ┌─────────┐  ┌──────────┐  ┌───────────────┐  │
│  │  MCP    │  │   HTTP   │  │   Storage     │  │
│  └─────────┘  └──────────┘  └───────────────┘  │
└─────────────────────────────────────────────────┘
```

## Core Components

### App (Application Layer)
- Owns application state (`AppState`)
- Receives events from all sources (keyboard, agent, tools)
- Dispatches events to appropriate handlers
- Triggers re-render after state changes

### Provider (Domain Layer)
- `LLMProvider` trait abstracts all LLM backends
- `DeepseekProvider` implements DeepSeek web API
- Supports both sync and streaming completion
- PoW challenge handling for DeepSeek

### Agent (Domain Layer)
- `AgentLoop` orchestrates multi-step tool use
- `ContextManager` manages conversation history
- `StreamingResponse` handles real-time token delivery
- Tool calls are parsed from LLM output and executed

### Tools (Domain Layer)
- `Tool` trait defines tool interface
- `ToolRegistry` manages available tools
- Built-in: `bash`, `powershell`
- MCP tools loaded dynamically

### TUI (Presentation Layer)
- `ratatui` for widget-based rendering
- `crossterm` for cross-platform terminal control
- Async event stream for non-blocking input
- Custom theme (Catppuccin Mocha palette)

### MCP (Infrastructure Layer)
- `MCPManager` handles server connections
- Supports stdio and HTTP transports
- Auto-discovery from IDE config files
- Tools exposed to agent dynamically

## Data Flow

```
User Input ──▶ App::handle_key() ──▶ AgentLoop::run()
                                           │
                                           ▼
                                    LLMProvider::complete_stream()
                                           │
                                           ▼
                                    CompletionChunk ──▶ AppEvent::AgentChunk
                                           │
                                           ▼
                                    App::handle_event() ──▶ Render
```

## Error Handling

- `thiserror` for typed error variants
- `color-eyre` for rich error reports
- `AppResult<T>` type alias throughout
- Errors propagate via `?` operator
- User-facing errors shown in status bar

## Configuration

TOML-based config at `~/.config/pooprusteek/config.toml`:
```toml
[provider]
kind = "deepseek"
token = ""
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
