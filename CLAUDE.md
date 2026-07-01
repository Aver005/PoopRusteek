# CLAUDE.md

Guidance for Claude Code (and any other agent) working in this repository.

## Project

Pooprusteek is a Rust TUI coding agent that talks to DeepSeek's reverse-engineered
web API (no paid API key — cookie/token auth, local SHA-3 proof-of-work) instead
of an official LLM API. Built on `tokio` + `ratatui`, it runs a single event-driven
terminal UI supporting parallel conversations, sub-agents, an iterative GOAL
worker/evaluator loop, MCP (stdio/HTTP/SSE tool servers), and markdown skills —
a free, terminal-native alternative to Claude Code.

## Read first

This repo keeps a curated knowledge base in `.memories/` that this file bridges
to. **Read `.memories/INDEX.md` first** — it gives the full read order
(`QUICKSTART.md` → `STATE.md` → `MAP.md` → `ARCHITECTURE.md` → `GLOSSARY.md` →
`BUGS.md` → `PLANS.md` → `LEARNINGS.md` → `CONVENTIONS.md` → `JOURNAL/`), plus a
`reference/` folder looked up on demand (commands, provider, tools, MCP, config,
prompts). `.memories/reference/AUDIT-2026-07-02.md` holds the latest full-codebase
audit — check it before assuming a subsystem is clean.

Note: Claude Code auto-loads this file, but **the app itself does not auto-load
`.memories/`** — an agent only benefits from that folder if told to read it.

## Build / test

```
cargo build
cargo test --bin pooprusteek
cargo clippy --bin pooprusteek
```

MSRV 1.91 (edition 2024). CI (`.github/workflows/ci.yml`) runs build + test on
Windows and Linux; clippy runs advisory (`continue-on-error`) until historical
warnings are paid down.

## Architecture (see `.memories/ARCHITECTURE.md` for the full picture)

A single `tokio::select!` event loop multiplexes ticks, terminal input, and an
internal `AppEvent` channel. `App` is a thin coordinator, not a god-object —
behavior lives in cohesive sub-state structs and controllers it owns.
`Conversations` is the multi-chat store: each `Conversation` owns its own
messages, a **forked** provider/session, and its agent task, so concurrent
turns never collide. `AgentRuntime::spawn(TurnSpec)` is the *only* place any
turn (normal, sidechat, sub-agent) launches — it runs `run_agent_loop` in a
spawned task. Every emitted event is tagged with a `ConversationId` so
background turns route into the right buffer instead of the focused one. The
TUI render path reads the focused conversation's state and never mutates it.

## Invariants for contributors (human or AI)

1. **Never block or `.await` network/LLM/file I/O inside `handle_event`** — it runs on the main `select!` loop; spawn a task and send an `AppEvent` back instead.
2. **Never hold the `MCPManager` lock (or any shared `Mutex`) across an I/O `.await`** — a slow call under the lock freezes every other consumer of that lock, including the UI.
3. **All user-data persistence goes through `util::atomic_write`, never `std::fs::write`** — direct writes truncate-in-place and corrupt on crash mid-write.
4. **Never byte-slice text** — use `util::truncate_at_char_boundary`; raw byte indices can split a multi-byte UTF-8 char or emoji.
5. **A slash command is one file in `src/commands/defs/` + registration in `commands/mod.rs`** — `name()` must be returned **without** a leading slash (dispatch strips the `/` before lookup; a registry test enforces this).
6. **The tools layer must never reach into the app layer** — `tools/` and `app/` communicate exclusively through `AppEvent`s, never direct calls or shared state upward.
7. **A new tool = implement the `Tool` trait + one registration line** in `tools/registry.rs`.
8. **Update `.memories` (`STATE.md`, `BUGS.md`, `JOURNAL/`) as part of "done"** for any non-trivial change. Anchor documentation to function/struct names, not line numbers — line numbers drift on the next edit.

## Conventions

Full style guide: `.memories/CONVENTIONS.md` (error handling, async/state
patterns, module decomposition, naming, TUI rules, test conventions).

Commits are **conventional commits with a scope + gitmoji**, e.g.
`refactor(app): 🧹 …`, `fix(deepseek): 🐛 …`, `feat(provider, app): ✨ …` — see
`git log` for the live pattern. Commits are the user's job; don't run
`git commit`/`git push` unless explicitly asked.
