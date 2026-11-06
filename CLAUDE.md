# CLAUDE.md

Guidance for Claude Code (and any other agent) working in this repository.

## Project

Pooprusteek is a Rust TUI coding agent that talks to DeepSeek's reverse-engineered
web API (no paid API key — cookie/token auth, local SHA-3 proof-of-work) instead
of an official LLM API, with optional extra providers (OpenAI-compatible /
Anthropic Messages / Gemini endpoints via `/providers`). A gateway layer
(`src/gateway/`) keeps several API keys and official-CLI subscriptions in one
place with routes, roles, limits and an explainable choice of account; secrets
live in the OS keyring and the config is a shared `~/.pooprusteek/config.toml`
plus an optional untrusted project layer. Built on `tokio` + `ratatui`, it runs
a single event-driven terminal UI supporting parallel conversations, sub-agents,
an iterative GOAL worker/evaluator loop, MCP (stdio/HTTP/SSE tool servers with
OAuth), markdown skills, and a **fully local semantic (RAG) layer** —
multilingual-e5-small ONNX embeddings + stemmed TF-IDF + RRF over three corpora:
skills, MCP tools, and persistent message history — a free, terminal-native
alternative to Claude Code.

## Read first

This repo keeps a curated knowledge base in `.memories/` that this file bridges
to. **Read `.memories/INDEX.md` first** — it gives the full read order
(`QUICKSTART.md` → `STATE.md` → `MAP.md` → `ARCHITECTURE.md` → `GLOSSARY.md` →
`BUGS.md` → `PLANS.md` → `LEARNINGS.md` → `CONVENTIONS.md` → `JOURNAL/`), plus a
`reference/` folder looked up on demand (commands, provider, tools, MCP, config,
prompts). `.memories/reference/HARNESS.md` documents the headless test harness and the
Docker sandbox — read it before testing agent *behaviour* rather than code.
`.memories/reference/AUDIT-2026-07-15-QUALITY.md` holds the latest
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

MSRV 1.91 (edition 2024). CI is split across four workflows: `checks.yml`
(reusable — tests on Windows/Ubuntu/macOS + `cargo fmt --check` + `cargo
clippy -D warnings`, blocking) gates `build.yml` (reusable — release binaries
for 5 targets plus the Windows Inno Setup installer). `ci.yml` runs `checks`
on every push/PR to `main`/`develop`, and on a `develop` push also runs
`build` and publishes the rolling `dev` prerelease (the `dev` update channel).
`release.yml` cuts the `stable` update channel from a `vX.Y.Z` tag pushed by
`scripts/release.sh`. A local pre-commit hook (`.githooks/pre-commit`, opt in
via `git config core.hooksPath .githooks`) runs the same test/fmt/clippy
checks before a commit is even made; bypass with `git commit --no-verify`.
`.gitlab-ci.yml` mirrors the same `check → build → release` shape for a
future GitLab project (unverified, no GitLab remote exists yet). Full
release/installer details: `.memories/reference/RELEASING.md`.

Semantic/retrieval changes have their own quality gate — an `#[ignore]`d MRR
eval harness that needs the ~120 MB embedding model on disk (downloaded once,
shared with the app):

```
cargo test --bin pooprusteek semantic::eval -- --ignored --nocapture
```

Run it whenever you touch `src/semantic/` ranking (thresholds, RRF, corpus
text composition) and record the numbers in the journal. Current baselines:
skills MRR 0.927, MCP tools MRR 0.836.

Agent **behaviour** has its own gate, separate from both: the headless
harness (`src/harness/`, full reference `.memories/reference/HARNESS.md`).
Unit tests cover code; the harness covers what the agent actually does.

```
# one real turn, no terminal, JSONL trace
pooprusteek --config <throwaway.toml> --data-dir .dev/run exec "<prompt>" --trace .dev/t.jsonl
# continue that conversation in a later process (implies --save-session)
pooprusteek --config <throwaway.toml> --data-dir .dev/run exec --resume <session-id> "<prompt>"
# a prompt repeated N times, expectations checked, metrics aggregated
pooprusteek --config <throwaway.toml> --data-dir .dev/run scenario sandbox/scenarios/live/<x>.toml --repeat 5
# rank failure patterns across every trace (and the saved-session corpus)
pooprusteek mine .dev/harness --sessions
```

Run it whenever you touch the agent loop, the tool prompt/parser, or the
semantic hint path, and record the pass rates in the journal. Always use
`--config` with a throwaway token: a run drives real tools under a policy.
Always use `--data-dir` too: without it a run writes into the real user's
sessions, checkpoints, semantic index and logs (`dirs::data_dir()` has no env
override on Windows). Both flags are forwarded to the children `scenario` and
`suite` spawn.
Inside Docker: `sandbox/README.md` (`./sandbox.ps1 suite live -Repeat 5`).
`sandbox/scenarios/mock/` runs against `mock-provider` for deterministic
regressions — the only reliable way to reach failure paths the live model
produces by accident. Gateway scenarios (`falls-back-after-429`,
`cli-refusal-falls-back-to-api`) additionally need a script from
`sandbox/mock-scripts/` and a config from `sandbox/gateway/`; mapping in
`.memories/reference/HARNESS.md` §7.5.

**Before accepting a branch or worktree, run `git ls-files --others --ignored --exclude-standard -- src`.**
`.gitignore` once matched `src/cli/secrets.rs` and `src/commands/defs/secrets.rs`
via `secrets.*`; the track built and tested locally because the files were on
disk, then the merge produced a tree that could not compile. A clean clone and a
build from it are the real gate; the ignored-file listing is the fast check.

Build flake to know about: parallel rustc + ort linking can exhaust the
Windows pagefile (`STATUS_STACK_BUFFER_OVERRUN` / os error 1455, "paging file
too small"). It is NOT a code error — retry, or `cargo test -j 1`.

## Architecture (see `.memories/ARCHITECTURE.md` for the full picture)

A single `tokio::select!` event loop multiplexes ticks, terminal input, and an
internal `AppEvent` channel. `App` is a thin coordinator, not a god-object —
behavior lives in cohesive sub-state structs and controllers it owns.
`Conversations` is the multi-chat store: each `Conversation` owns its own
messages, a **forked** provider/session, and its agent task, so concurrent
turns never collide. `app::runtime::run_guarded` is the *only* wrapper any
turn runs through: `AgentRuntime::spawn(TurnSpec)` spawns it for normal and
background turns, and the `task` sub-agent runs it inline inside the parent's
future. It catches a panicking turn as `Failed` and tags the turn's background
processes with its chat (`tools::background::with_owner`). Every emitted event is tagged with a `ConversationId` so
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
  with `fork()` for per-conversation session isolation. `provider/failure.rs`
  types every upstream refusal once (`FailureClass`, `Retry-After`, limit
  headers) and is the single source of truth for the gateway and `/serve`.
- **`src/gateway/` — accounts, routes, roles.** `Gateway` (facade,
  `gateway/handle.rs`) selects an account for every call with an explainable
  `Decision`; `RoutedProvider` is the `LLMProvider` the app and `/serve` see —
  it falls back only before the first visible chunk, forks per conversation,
  enforces per-account cooldown and concurrency, and records usage. `Backend`
  adapters cover API endpoints (`api_backend.rs`), the built-in DeepSeek web
  client (`deepseek_backend.rs`) and the Claude Code / OpenCode CLIs in plain
  model mode (`cli/`); Codex is direct-token only. Roles (`title`, `summary`,
  `evaluator`) are separate forked calls on their own routes. The gateway is
  off by default and then `main_provider` builds the old provider unchanged.
  Full reference: `.memories/reference/GATEWAY.md`.
- **`src/secrets/` + layered `src/config/` — credentials and settings.** Keys
  live in the OS keyring and appear in the file only as references
  (`keyring:`, `env:`, `cmd:`, `file:`, `none`). Config has a shared layer
  (`~/.pooprusteek/config.toml`) and an untrusted project layer
  (`.pooprusteek/config.toml`); changes to request targets wait for
  `pooprusteek trust`, and project account keys need `secrets bind`. Saves go
  through `toml_edit` as a diff, on one worker thread, never on the event loop.
  Reference: `.memories/reference/CONFIG.md`, `.docs/secrets-and-config.md`.
- **`src/mcp/` — servers + OAuth.** 8-source config discovery, `/mcp add`
  (JSON / wizard / one-liner), RFC 9728/8414/7591 + PKCE authorization with
  OS-keyring token storage.

- **`src/harness/` — headless behaviour testing.** A peer of `app/`, above it:
  `pooprusteek exec` assembles the same deps `App::new` does and drives one
  real `TurnSpec` with the human replaced by a policy. Two properties are
  load-bearing. (1) The trace is **the existing `debug_log`** in a new JSONL
  format, not a second telemetry stream — so it can never drift from the
  runner's instrumentation. (2) There is no approval bypass: the driver
  answers every `RequestToolApproval` (sub-agents' too, tagged with `origin`)
  from an `ApprovePolicy`. `TurnSpec.unattended` only removes what needs a
  watching human (questions, timers, nested `task`) — tools still ask.
  `scenario`/`suite` repeat a prompt as child processes and report a pass
  *rate*; `mine` ranks normalised failure shapes; `mock-provider` serves
  scripted replies.

## Invariants for contributors (human or AI)

1. **Never block or `.await` network/LLM/file I/O inside `handle_event`** — it runs on the main `select!` loop; spawn a task and send an `AppEvent` back instead.
2. **Never hold the `MCPManager` lock (or any shared `Mutex`) across an I/O `.await`** — a slow call under the lock freezes every other consumer of that lock, including the UI.
3. **All user-data persistence goes through `util::atomic_write`, never `std::fs::write`** — direct writes truncate-in-place and corrupt on crash mid-write.
4. **Never byte-slice text, and never decode child-process output with a bare `from_utf8_lossy`** — use `util::truncate_at_char_boundary` and `util::decode_process_output`. Raw byte indices split a multi-byte UTF-8 char or emoji; and some Windows tools write UTF-16 (`wsl.exe` does, for its own diagnostics), which lossy UTF-8 turns into mojibake the model cannot read. Decode once on a complete buffer — per-line decoding cannot work when the newline is `0A 00`.
5. **A slash command is one file in `src/commands/defs/` + registration in `commands/mod.rs`** — `name()` must be returned **without** a leading slash (dispatch strips the `/` before lookup; a registry test enforces this).
6. **The tools layer must never reach into the app layer** — `tools/` and `app/` communicate exclusively through `AppEvent`s, never direct calls or shared state upward.
7. **A new tool = implement the `Tool` trait + one registration line** in `tools/registry.rs`.
8. **Update `.memories` (`STATE.md`, `BUGS.md`, `JOURNAL/`) as part of "done"** for any non-trivial change. Anchor documentation to function/struct names, not line numbers — line numbers drift on the next edit.
9. **CPU-heavy work (ONNX embedding, PoW solving) runs on `tokio::task::spawn_blocking` only** — never on the event loop, never bare on an async worker. The `SemanticService` inner mutex is held across synchronous work only, never across an `.await`.
10. **Never print to stdout/stderr while the app runs** — the TUI owns the terminal and `--acp` owns stdout for JSON-RPC. `tracing` goes to the data-dir log file (set up in `main.rs`); ad-hoc debugging goes through `debug_log`. This includes dependencies: fastembed's download progress bar is explicitly disabled for this reason.
11. **The harness must never add instrumentation to the agent loop, and never bypass tool approval** — telemetry goes through the existing `debug_log` call sites (a parallel stream drifts), and every tool call (sub-agents included) is answered by the run's `ApprovePolicy`; `TurnSpec.auto_approve` was removed for exactly this reason (it let `task` launder approval).
12. **The semantic layer must degrade, never block features** — every entry point returns empty/falls back to lexical when the embedder is disabled, still initializing, or failed. Deferred MCP schemas are only allowed when semantic matching is enabled (`App::effective_mcp_schema_mode` forces `Full` otherwise); session files remain the source of truth for the history index (it is a wipeable cache).
13. **Gateway edits go one way: `snapshot()` in, `apply_edit()` out.** The accounts screen and its modals read `Gateway::snapshot()` and never mutate gateway state; every change is a `ConfigEdit` validated by `GatewayConfig::validate()`, persisted as a full `GatewayConfig` through `AppEvent::GatewayPersist` and `config::save`. A new setting must be readable from the snapshot (or explicitly fetched from `Config`), never reached for through a shared provider.
14. **Secrets are resolved at the adapter edge, never during config parse, and never on the event loop.** A secret field is a `SecretRef`; `SecretResolver::resolve` touches the keyring/`cmd:`/files and blocks, so it runs from `spawn_blocking` (that is why the gateway build is `gateway::pending`). Do not print a secret, do not put one in `Debug`, do not add a path that writes one back into the file. Wrong-form TOML errors must print the message and position, never the source line.
15. **A gateway target change never deletes the user's server-side session.** `RoutedProvider` may discard the fresh fork of a failed attempt only when it emitted nothing (`!error.emitted`); an adopted session (`/load`, `--resume`) and the user's own `/delete` must reach the server untouched. `history::flatten_for_switch` rewrites foreign calls into text before a new backend sees them — do not carry `provider_state` or tool-call ids across accounts.

## Conventions

Full style guide: `.memories/CONVENTIONS.md` (error handling, async/state
patterns, module decomposition, naming, TUI rules, test conventions).

Commits are **conventional commits with a scope + gitmoji**, e.g.
`refactor(app): 🧹 …`, `fix(deepseek): 🐛 …`, `feat(provider, app): ✨ …` — see
`git log` for the live pattern. Commits are the user's job; don't run
`git commit`/`git push` unless explicitly asked.
