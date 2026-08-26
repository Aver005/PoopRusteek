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
- [x] Context compaction — real as of 2026-08-26 (Phase 7 below): the stub `/compact` (user messages joined with "; ") is gone, replaced by the ladder — rungs 0-2 automatic, rung 3 (LLM summary) on demand via `/compact [1|2|3]`
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
> Reviewed 2026-08-26 by five agents: 42 findings, 5 data-loss ones fixed same day.
> The rest are in `.memories/BUGS.md` (`compaction-review-2026-08-26 #N`). No harness
> scenario can reach any rung yet — see #6, #7.
> Full research + plan: `context-compaction.md`
- [x] Step 1: measurement only, no behavior change — window size (provider `context_window()` + `[context] context_window` override), local `chars/3` estimate (real `prompt_tokens` not wired yet, see `context-compaction.md`), `ctx:` status-bar indicator (`src/context/`, `src/provider/compat_client.rs`, `src/provider/mod.rs`)
- [x] Step 2: rung 0 — tool output cap at capture time, before it enters history (`src/context/tool_output.rs`, applied in `src/agent/runner.rs` and `src/agent/sub_agent.rs`)
- [x] Step 3: rung 1 — clear old tool-result bodies, full output spilled to disk (`src/context/prune.rs`, `src/context/spec.rs`, `util::atomic_write`, new `read_file` tool). Checked before every step, in-flight tail protected. **Skipped for DeepSeek** — its wire format never resends a cleared local message (`LLMProvider::keeps_server_side_history()`); rung 1 only does anything for OpenAI-compatible providers until rung 2 (session reset) ships
- [x] Step 4: rung 2 for DeepSeek — reset the server session above 90% of the usable window, rung 1's clearing applied first and a "will the re-seed even fit" check before it; shipped 2026-08-26 through the existing `LLMProvider::reset()`, so `stream.rs`/`prompt.rs` were never touched (`src/agent/runner.rs::reset_server_session`, `src/context/mod.rs`). *(Ticked 2026-08-26 while recording steps 5-6 — it shipped earlier the same day and this line was left stale.)*
- [x] Step 5: rung 3 — LLM summary in three modes, **manual only, no automatic trigger** (`src/context/modes.rs`, `src/context/summary.rs`, `src/context/compact.rs`). Prompt asks for a form, never a compression ratio; a reply missing any section is refused and the history left alone; tools used are computed from the history, not asked of the model
- [x] Step 6: `/compact [1|2|3]` — manual trigger of rung 3, replacing the stub; per-chat mode (`Conversation.compact_mode`), `[context] compact_mode` default, new `/default-compact` to persist it; work spawned, result via `AppEvent::CompactFinished` (`src/commands/defs/compact.rs`, `src/commands/defs/default_compact.rs`, `src/app/multichat.rs`)
- [ ] Follow-up: run `/compact` in a live TUI session against a real model, all three modes. Rung 3 is covered by unit tests over `FakeProvider` only — the harness drives prompts, not slash commands — and every other rung had a defect that only a live run exposed
- [ ] Follow-up: `/compact` does nothing useful on DeepSeek (rewrites local history the wire never carries) and its summariser call runs in the chat's own server session — see `.memories/BUGS.md`

## Known Issues
- (none yet)
