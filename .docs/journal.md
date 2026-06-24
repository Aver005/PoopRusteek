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
- [x] Wire up agent loop to app event loop
- [x] Implement DeepSeek PoW challenge
- [x] Add keyboard shortcuts (Ctrl+L clear)
- [ ] Add MCP connection (stdio transport)
- [ ] Implement tool execution in agent loop
- [ ] Add onboarding flow
- [ ] Add session persistence
- [ ] Add markdown rendering in TUI
- [ ] Add syntax highlighting
- [ ] Add slash commands
- [ ] Add file mention (@file) support

### FIX
- (none yet)

### Learnings
- Ratatui v0.29 works well with crossterm v0.28
- `tokio::spawn` requires `'static` lifetime — use `Arc` for shared providers
- `color_eyre::Report` needs manual `From` impl for custom error types
- DeepSeek PoW uses SHA-3_256 with difficulty-based target calculation
- PoW prefix format: `{salt}_{expire_at}_{nonce}`
- PoW response sent as `x-ds-pow-response` header (Base64-encoded JSON)

## 2026-06-24 — Agent Wiring + PoW + Streaming

### What was done
- Wired agent loop to app event loop (spawned task streams chunks via mpsc)
- Implemented DeepSeek PoW challenge (SHA-3_256 nonce finding)
- Added `x-ds-pow-response` header to all DeepSeek requests
- Streaming response works: chunks flow from provider → event channel → TUI
- "Thinking..." indicator shown during streaming
- Auto-scroll chat to bottom
- Fixed borrow checker issues in MCP manager
- First successful `cargo build`!

### Files changed
- `src/app/mod.rs` — Agent wiring, spawn task, event handling
- `src/provider/deepseek.rs` — PoW integration, headers
- `src/provider/pow.rs` — NEW: SHA-3 PoW solver
- `src/provider/mod.rs` — Added pow module
- `src/tui/widgets/chat.rs` — Streaming indicator, scroll fix
- `src/mcp/manager.rs` — Fixed borrow checker
- `Cargo.toml` — Added sha3, base64

## 2026-06-24 — Phase 2: Commands, Onboarding, Markdown

### What was done
- Slash commands system: /help, /clear, /compact, /version, /quit, /reset
- Command registry with trait-based dispatch
- Onboarding flow for first launch (token + model selection)
- Session persistence (save/load/list/delete to JSON files)
- Markdown rendering in TUI using pulldown-cmark
- Assistant messages now render with styled headings, code blocks, lists, links

### Files added
- `src/commands/mod.rs` — Command registry + trait
- `src/commands/defs/` — 6 command implementations
- `src/cli/mod.rs` — CLI utilities
- `src/cli/onboarding.rs` — First-launch setup
- `src/session.rs` — Session persistence
- `src/tui/markdown.rs` — Markdown renderer for TUI

### Learnings
- pulldown-cmark v0.12 uses struct variants for Tag (not tuple variants)
- CodeBlockKind::Fenced contains CowStr, needs pattern matching
- Onboarding runs before TUI starts (raw terminal mode)

## 2026-06-24 — File Mentions + Syntax Highlighting

### What was done
- File mention support: `@file.rs`, `@file.rs:10-20` (line ranges)
- Mentions expanded before sending to agent
- Syntax highlighting for code blocks using syntect
- Automatic language detection from markdown code fences
- Theme: base16.ocean.dark for syntax colors

### Files added
- `src/cli/file_mentions.rs` — @file parser and expander
- `src/tui/markdown.rs` — Updated with syntect highlighting

### Learnings
- syntect uses OnceLock for static SyntaxSet/ThemeSet
- HighlightLines is stateful (tracks context across lines)
- Need to flush_line before rendering highlighted code blocks
