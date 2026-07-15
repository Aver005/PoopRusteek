# CLAUDE.md

Guidance for Claude Code (and any other agent) working in this repository.

## Project

Pooprusteek is a Rust TUI coding agent that talks to DeepSeek's reverse-engineered
web API (no paid API key — cookie/token auth, local SHA-3 proof-of-work) instead
of an official LLM API, with optional extra providers (OpenAI-compatible /
Anthropic Messages / Gemini endpoints via `/providers`). Built on `tokio` +
`ratatui`, it runs a single event-driven terminal UI supporting parallel
conversations, sub-agents, an iterative GOAL worker/evaluator loop, MCP
(stdio/HTTP/SSE tool servers with OAuth), markdown skills, and a **fully local
semantic (RAG) layer** — multilingual-e5-small ONNX embeddings + stemmed TF-IDF
+ RRF over three corpora: skills, MCP tools, and persistent message history —
a free, terminal-native alternative to Claude Code.

## Read first

This repo keeps a curated knowledge base in `.memories/` that this file bridges
to. **Read `.memories/INDEX.md` first** — it gives the full read order
(`QUICKSTART.md` → `STATE.md` → `MAP.md` → `ARCHITECTURE.md` → `GLOSSARY.md` →
`BUGS.md` → `PLANS.md` → `LEARNINGS.md` → `CONVENTIONS.md` → `JOURNAL/`), plus a
`reference/` folder looked up on demand (commands, provider, tools, MCP, config,
prompts). `.memories/reference/AUDIT-2026-07-15-QUALITY.md` holds the latest
full-codebase audit (quality/structure; defects: `AUDIT-2026-07-02.md`, cleanup:
`AUDIT-2026-07-04-CLEANUP.md`) — check it before assuming a subsystem is clean.

Note: Claude Code auto-loads this file, but **the app itself does not auto-load
`.memories/`** — an agent only benefits from that folder if told to read it.

## Build / test

```
cargo build
cargo test --bin pooprusteek
cargo clippy --bin pooprusteek
```

MSRV 1.91 (edition 2024). CI (`.github/workflows/ci.yml`) is one sequential
pipeline: `test` (build+test, Windows+Linux) and `lint` (`cargo fmt --check`
+ `cargo clippy -D warnings`, blocking) gate everything else — the rolling
`dev-build` release (`release-build` + `publish` jobs, see
`scripts/dev-release.template.md`) only builds and publishes once both pass,
and only for a push to `develop`. A local pre-commit hook
(`.githooks/pre-commit`, opt in via `git config core.hooksPath .githooks`)
runs the same three checks before a commit is even made; bypass with
`git commit --no-verify`. `.gitlab-ci.yml` mirrors the same
`check → build → release` shape for a future GitLab project (unverified,
no GitLab remote exists yet).

Semantic/retrieval changes have their own quality gate — an `#[ignore]`d MRR
eval harness that needs the ~120 MB embedding model on disk (downloaded once,
shared with the app):

```
cargo test --bin pooprusteek semantic::eval -- --ignored --nocapture
```

Run it whenever you touch `src/semantic/` ranking (thresholds, RRF, corpus
text composition) and record the numbers in the journal. Current baselines:
skills MRR 0.927, MCP tools MRR 0.836.

Build flake to know about: parallel rustc + ort linking can exhaust the
Windows pagefile (`STATUS_STACK_BUFFER_OVERRUN` / os error 1455, "paging file
too small"). It is NOT a code error — retry, or `cargo test -j 1`.

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

Key subsystems added on top of that core:

- **`src/semantic/` — local RAG.** `SemanticService` is a shared handle over
  one embedder + three corpora (skills, MCP tools, message history). Init is
  background (`spawn_init` on `spawn_blocking`; first run downloads the model,
  then backfills history from saved session files). Each turn gets an
  ephemeral hint (`match_prompt` in `run_agent_loop`) suggesting skills and
  MCP tools; the `tool_search` / `history_search` builtins expose the same
  index to the model on demand. **Deferred MCP schemas**: above 12 tools
  (`[semantic] mcp_schemas = auto`), the system prompt carries only a
  server-level summary (`<server> (N tools)`) — individual tools are not
  enumerated; full definitions come from hints or `tool_search`. Enabled
  skills follow the same idea (`[skills] injection = auto`): above 8 KB of
  combined content the prompt carries a `slug — description` list and the
  model `skill load`s on demand.
  The history index (`data_dir/semantic/history.json`) is a rebuildable cache
  over session files (per-session watermarks, model-stamp wipe); `/search`
  (View::Search) and `/rag` are the user-facing surfaces.
- **`src/provider/` — multi-protocol.** The built-in DeepSeek web client
  (PoW + SSE) plus `/providers`-managed entries speaking OpenAI Chat
  Completions, Anthropic Messages, or Gemini. All implement `LLMProvider`
  with `fork()` for per-conversation session isolation.
- **`src/mcp/` — servers + OAuth.** 8-source config discovery, `/mcp add`
  (JSON / wizard / one-liner), RFC 9728/8414/7591 + PKCE authorization with
  OS-keyring token storage.

## Invariants for contributors (human or AI)

1. **Never block or `.await` network/LLM/file I/O inside `handle_event`** — it runs on the main `select!` loop; spawn a task and send an `AppEvent` back instead.
2. **Never hold the `MCPManager` lock (or any shared `Mutex`) across an I/O `.await`** — a slow call under the lock freezes every other consumer of that lock, including the UI.
3. **All user-data persistence goes through `util::atomic_write`, never `std::fs::write`** — direct writes truncate-in-place and corrupt on crash mid-write.
4. **Never byte-slice text** — use `util::truncate_at_char_boundary`; raw byte indices can split a multi-byte UTF-8 char or emoji.
5. **A slash command is one file in `src/commands/defs/` + registration in `commands/mod.rs`** — `name()` must be returned **without** a leading slash (dispatch strips the `/` before lookup; a registry test enforces this).
6. **The tools layer must never reach into the app layer** — `tools/` and `app/` communicate exclusively through `AppEvent`s, never direct calls or shared state upward.
7. **A new tool = implement the `Tool` trait + one registration line** in `tools/registry.rs`.
8. **Update `.memories` (`STATE.md`, `BUGS.md`, `JOURNAL/`) as part of "done"** for any non-trivial change. Anchor documentation to function/struct names, not line numbers — line numbers drift on the next edit.
9. **CPU-heavy work (ONNX embedding, PoW solving) runs on `tokio::task::spawn_blocking` only** — never on the event loop, never bare on an async worker. The `SemanticService` inner mutex is held across synchronous work only, never across an `.await`.
10. **Never print to stdout/stderr while the app runs** — the TUI owns the terminal and `--acp` owns stdout for JSON-RPC. `tracing` goes to the data-dir log file (set up in `main.rs`); ad-hoc debugging goes through `debug_log`. This includes dependencies: fastembed's download progress bar is explicitly disabled for this reason.
11. **The semantic layer must degrade, never block features** — every entry point returns empty/falls back to lexical when the embedder is disabled, still initializing, or failed. Deferred MCP schemas are only allowed when semantic matching is enabled (`App::effective_mcp_schema_mode` forces `Full` otherwise); session files remain the source of truth for the history index (it is a wipeable cache).

## Conventions

Full style guide: `.memories/CONVENTIONS.md` (error handling, async/state
patterns, module decomposition, naming, TUI rules, test conventions).

Commits are **conventional commits with a scope + gitmoji**, e.g.
`refactor(app): 🧹 …`, `fix(deepseek): 🐛 …`, `feat(provider, app): ✨ …` — see
`git log` for the live pattern. Commits are the user's job; don't run
`git commit`/`git push` unless explicitly asked.
