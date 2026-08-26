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
- [ ] Context compaction — **stub only**: `/compact` exists, but its "summary" is user messages joined with "; " and assistant/tool messages are dropped. Real implementation is Phase 7
- [x] File mentions (@file with line ranges)
- [x] Syntax highlighting for code blocks (syntect)
- [x] Tool approval dialog (modal overlay, Y/N)

## Phase 3: Integration (Complete)
- [x] MCP stdio transport
- [x] MCP HTTP transport
- [x] MCP auto-discovery (Claude Desktop, VS Code, Cursor, Trae)
- [x] MCP manager with tool resolution
- [x] MCP JSON-RPC client
- [x] Wire MCP tools into agent loop
- [x] System prompt with MCP tool descriptions
- [x] Tool call parser ([TOOL:name] format)
- [x] ACP server mode (--acp flag, ND-JSON over stdio)
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

## Phase 6: Local vision (planned)
> Full research + plan: `vision-local-image-understanding.md`
- [ ] Stage 0: stop dropping attached images silently (`app/keys/chat.rs`)
- [ ] Stage 1: `VisionService` + OCR backend (`ocrs`) + `image_read` tool
- [ ] Stage 2: Florence-2-base-ft ONNX backend via the already-linked `ort`
- [ ] Stage 3: `remote` backend over `CompatClient` (local Ollama / llama.cpp)
- [ ] Stage 4: keep MCP screenshot bytes instead of `[Image: image/png]`

## Phase 7: Context compaction (planned)
> Full research + plan: `context-compaction.md`
- [x] Step 1: measurement only, no behavior change — window size (provider `context_window()` + `[context] context_window` override), local `chars/3` estimate (real `prompt_tokens` not wired yet, see `context-compaction.md`), `ctx:` status-bar indicator (`src/context/`, `src/provider/compat_client.rs`, `src/provider/mod.rs`)
- [x] Step 2: rung 0 — tool output cap at capture time, before it enters history (`src/context/tool_output.rs`, applied in `src/agent/runner.rs` and `src/agent/sub_agent.rs`)
- [ ] Step 3: rung 1 — clear old tool-result bodies, full output spilled to disk (`src/context/`, `util::atomic_write`)
- [ ] Step 4: rung 2 for DeepSeek — reset the server session with a compressed `LOCAL MEMORY` (`src/provider/deepseek/stream.rs`, `src/provider/prompt.rs`)
- [ ] Step 5: rung 3 — LLM summary (`src/context/`, `src/agent/runner.rs`)
- [ ] Step 6: `/compact` — manual trigger of the same ladder, replacing today's stub; update this file (`src/commands/defs/compact.rs`)

## Known Issues
- (none yet)
