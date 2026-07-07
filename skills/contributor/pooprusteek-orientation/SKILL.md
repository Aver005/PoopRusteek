---
name: PoopRusteek Orientation
description: Read-me-first onboarding for any agent working ON the PoopRusteek codebase — what it is, the mandatory .memories read order, the core architecture, build/test commands, and all 11 contributor invariants. Use when starting cold; skip if you already know the layout.
---

# PoopRusteek Orientation

PoopRusteek is a Rust TUI coding agent (repo root `c:/Work/.ME/pooprusteek`, ~15k LOC,
edition 2024, MSRV 1.91) — a free, terminal-native alternative to Claude Code. It talks to
DeepSeek's reverse-engineered **web** API (cookie/token auth + local SHA-3 proof-of-work,
no paid API key), with optional extra providers (OpenAI-compatible / Anthropic Messages /
Gemini via `/providers`). Built on `tokio` + `ratatui`. Features: parallel conversations,
sub-agents, a GOAL worker/evaluator loop, MCP tool servers (stdio/HTTP/SSE + OAuth),
markdown skills, and a fully local semantic (RAG) layer.

## Read first — mandatory order

The repo keeps a curated knowledge base in `.memories/`. **The app does NOT auto-load it at
runtime** — you only benefit if you read it. Start at `.memories/INDEX.md`, then follow its
read order:

`QUICKSTART.md` → `STATE.md` → `MAP.md` → `ARCHITECTURE.md` → `GLOSSARY.md` → `BUGS.md` →
`PLANS.md` → `LEARNINGS.md` → `CONVENTIONS.md` → `JOURNAL/`.

`.memories/reference/` is looked up on demand: `COMMANDS.md`, `PROVIDER.md`, `TOOLS.md`,
`MCP.md`, `CONFIG.md`, `PROMPTS.md`, `AUTO-UPDATE.md`, and the latest full-codebase audit
`AUDIT-2026-07-02.md`. Check the audit before assuming a subsystem is clean. If a memory
fact contradicts the code, **the code wins** — fix the memory.

## Core architecture

- A single `tokio::select!` event loop (`src/app/mod.rs`) multiplexes ticks, terminal input,
  and an internal `AppEvent` channel. It drains ≤256 events then renders once behind a dirty flag.
- `App` is a **thin coordinator**, not a god-object — behavior lives in cohesive sub-state
  structs/controllers it owns (`app/conversation.rs`, `multichat.rs`, `goal.rs`, `keys/`, …).
- `Conversations` (`app/conversation.rs`) is the multi-chat store: each `Conversation` owns
  its own messages, a **forked** provider/session (`LLMProvider::fork()`), and its agent task,
  so concurrent turns never collide.
- `AgentRuntime::spawn(TurnSpec)` (`app/runtime.rs`) is the **only** place any turn (normal,
  sidechat, sub-agent) launches — it runs `run_agent_loop` (`agent/runner.rs`) in a spawned task.
- Every emitted `AppEvent` is tagged with a `ConversationId` so background turns route into the
  right buffer, not the focused one. The TUI render path reads focused state and never mutates it.

## Hot files

`src/main.rs` (entry, `--acp`/`--serve` flags) · `src/app/mod.rs` (coordinator/event loop) ·
`src/app/conversation.rs` (multi-chat) · `src/app/runtime.rs` (`spawn(TurnSpec)`) ·
`src/app/events.rs` (`AppEvent`) · `src/agent/runner.rs` (agent loop) ·
`src/provider/deepseek.rs` + `src/provider/pow.rs` (DeepSeek + PoW) ·
`src/tools/registry.rs` (tools) · `src/mcp/manager.rs` (MCP) · `src/semantic/mod.rs` (RAG) ·
`src/commands/defs/` (slash commands) · `src/tui/render/` (views).

## Build / test

```
cargo build
cargo test --bin pooprusteek
cargo clippy --bin pooprusteek        # CI runs -D warnings; must be clean
cargo fmt --check
```

Build flake to know: parallel rustc + ort/ONNX linking can exhaust the Windows pagefile
(`STATUS_STACK_BUFFER_OVERRUN` / os error 1455, "paging file too small"). Not a code error —
retry or `cargo test -j 1`. Semantic changes have their own MRR eval gate (see the
`pooprusteek-semantic-rag` skill).

## The 11 contributor invariants (from CLAUDE.md — non-negotiable)

1. Never block or `.await` network/LLM/file I/O inside `handle_event` — spawn a task, send an `AppEvent` back.
2. Never hold the `MCPManager` lock (or any shared `Mutex`) across an I/O `.await` — it freezes every other consumer, including the UI.
3. All user-data persistence goes through `util::atomic_write`, never `std::fs::write`.
4. Never byte-slice text — use `util::truncate_at_char_boundary`; raw byte indices split multi-byte UTF-8/emoji.
5. A slash command = one file in `src/commands/defs/` + one registration in `commands/mod.rs`; `name()` returns the name **without** leading `/`.
6. The tools layer must never reach into the app layer — `tools/` and `app/` talk only via `AppEvent`.
7. A new tool = implement the `Tool` trait + one registration line in `tools/registry.rs`.
8. Update `.memories` (`STATE.md`, `BUGS.md`, `JOURNAL/`) as part of "done"; anchor docs to names, not line numbers.
9. CPU-heavy work (ONNX embedding, PoW solving) runs on `tokio::task::spawn_blocking` only — never on the event loop.
10. Never print to stdout/stderr while the app runs — the TUI owns the terminal, `--acp` owns stdout; use `tracing` (log file) or `debug_log`.
11. The semantic layer must degrade, never block — every entry point falls back to empty/lexical when the embedder is disabled/initializing/failed.

Commits are conventional-commit + scope + gitmoji (e.g. `fix(deepseek): 🐛 …`). Commits are
the user's job — don't run `git commit`/`git push` unless explicitly asked.
