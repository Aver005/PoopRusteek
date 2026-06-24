# Pooprusteek TODO

## Phase 1: Core (Complete)
- [x] Project setup (Cargo.toml, modules)
- [x] TUI skeleton (render loop, widgets)
- [x] Provider trait + DeepSeek implementation
- [x] Agent loop + context manager
- [x] Tool trait + bash/powershell tools
- [x] MCP type definitions
- [x] Event-driven architecture
- [x] Wire agent loop to app (send_to_agent)
- [x] DeepSeek PoW challenge implementation
- [x] Streaming response in TUI (real-time tokens)
- [x] First cargo build

## Phase 2: Features (Complete)
- [x] Onboarding flow (first launch)
- [x] Session persistence (save/load)
- [x] Slash commands (/help, /clear, /compact, etc.)
- [x] Markdown rendering in TUI (pulldown-cmark)
- [x] Context compaction (/compact command)
- [x] File mentions (@file with line ranges)
- [x] Syntax highlighting for code blocks (syntect)
- [ ] Tool approval dialog

## Phase 3: Integration
- [x] MCP stdio transport
- [x] MCP HTTP transport
- [x] MCP auto-discovery (Claude Desktop, VS Code, Cursor, Trae)
- [x] MCP manager with tool resolution
- [x] MCP JSON-RPC client
- [ ] Wire MCP tools into agent loop
- [ ] ACP server mode
- [ ] RAG (codebase semantic search)

## Phase 4: Polish
- [x] Keyboard shortcuts (Ctrl+L clear)
- [ ] Multiple themes
- [ ] Mouse support
- [ ] Copy/paste support
- [ ] Error recovery
- [ ] Rate limiting
- [ ] Request retry with backoff

## Phase 5: Distribution
- [ ] Release builds (LTO, strip)
- [ ] Cross-compilation (Windows, Linux, macOS)
- [ ] Installer scripts
- [ ] GitHub Actions CI/CD
- [ ] Man page

## Known Issues
- (none yet)
