# Pooprusteek Development Journal

## 2026-06-24 — Project Inception

### What was done
- Analyzed original Poopseek (TypeScript/Bun) architecture
- Designed Rust fork architecture (Clean Architecture / Hexagonal)
- Created project skeleton with all core modules
- First successful compilation

### Architecture decisions
- **Async runtime**: Tokio (multi-threaded)
- **TUI framework**: Ratatui + Crossterm
- **HTTP client**: Reqwest with streaming
- **Error handling**: thiserror + color-eyre
- **Config format**: TOML
- **Pattern**: Event-driven with mpsc channels

### Module structure
```
src/
├── main.rs              — Entry point
├── error.rs             — AppError enum
├── config/mod.rs        — Configuration (TOML)
├── app/
│   ├── mod.rs           — App state + event loop
│   └── events.rs        — Event types
├── provider/
│   ├── mod.rs           — LLMProvider trait
│   ├── deepseek.rs      — DeepSeek web API
│   └── types.rs         — API response types
├── agent/
│   ├── mod.rs           — Agent types
│   ├── loop_runner.rs   — Agent execution loop
│   ├── context.rs       — Context manager
│   └── streaming.rs     — Streaming response handler
├── tools/
│   ├── mod.rs           — Tool trait + registry
│   ├── registry.rs      — Tool registry
│   ├── bash.rs          — Bash tool
│   └── powershell.rs    — PowerShell tool
├── tui/
│   ├── mod.rs           — Terminal init/restore
│   ├── render.rs        — Main render function
│   ├── theme.rs         — Color theme (Catppuccin Mocha)
│   └── widgets/
│       ├── mod.rs
│       ├── chat.rs      — Chat message display
│       ├── input.rs     — Input box with cursor
│       └── status.rs    — Status bar
└── mcp/
    ├── mod.rs           — MCP barrel
    ├── manager.rs       — MCP server manager
    └── types.rs         — MCP config types
```

### Key patterns

#### Event Loop Architecture
```
┌─────────────┐     ┌──────────────┐     ┌─────────────┐
│ Crossterm   │────▶│ Event Channel│────▶│ App State   │
│ EventStream │     │ (mpsc)       │     │ + Render    │
└─────────────┘     └──────────────┘     └─────────────┘
                          ▲
┌─────────────┐           │
│ Agent Loop  │───────────┘
│ (spawned)   │
└─────────────┘
```

#### Provider Abstraction
```rust
#[async_trait]
pub trait LLMProvider: Send + Sync {
    async fn complete(&self, request: CompletionRequest) -> AppResult<CompletionResponse>;
    async fn complete_stream(&self, request, tx) -> AppResult<()>;
    fn name(&self) -> &str;
    fn model(&self) -> &str;
}
```

### TODO
- [ ] Wire up agent loop to app event loop
- [ ] Implement DeepSeek PoW challenge
- [ ] Add MCP connection (stdio transport)
- [ ] Implement tool execution in agent loop
- [ ] Add onboarding flow
- [ ] Add session persistence
- [ ] Add markdown rendering in TUI
- [ ] Add syntax highlighting
- [ ] Add slash commands
- [ ] Add file mention (@file) support
- [ ] Add keyboard shortcuts (Ctrl+L clear, etc.)

### FIX
- (none yet)

### Learnings
- Ratatui v0.29 works well with crossterm v0.28
- `tokio::spawn` requires `'static` lifetime — use `Arc` for shared providers
- `color_eyre::Report` needs manual `From` impl for custom error types
