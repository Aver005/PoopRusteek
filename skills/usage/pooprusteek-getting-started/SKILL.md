---
name: PoopRusteek — Getting Started
description: Install, authenticate, and run the PoopRusteek TUI coding agent. Use when setting up PoopRusteek, choosing a run mode, understanding DeepSeek auth, or locating config/session files. Not for editing PoopRusteek's Rust source.
---

# PoopRusteek — Getting Started

PoopRusteek (Пупра́стик) is a Rust TUI coding agent — a free, terminal-native
alternative to Claude Code. It talks to DeepSeek's reverse-engineered **web** API
(no paid API key) and adds parallel chats, sub-agents, a GOAL loop, MCP tools,
markdown skills, and a fully local RAG layer. Optional extra providers
(OpenAI-compatible / Anthropic / Gemini) plug in via `/providers`.

## Install & build

```bash
cargo install --path .     # install the binary (needs Rust edition 2024, MSRV 1.91)
cargo build                # debug build
cargo build --release      # optimized (LTO, stripped)
```

## Run

```bash
pooprusteek               # launch the TUI (default)
cargo run                 # same, from a checkout
cargo run -- --help       # list CLI flags
```

Run modes and flags:

| Flag | Effect |
|------|--------|
| *(none)* | Interactive TUI (default) |
| `--acp` | ACP server — JSON-RPC over stdio, for IDE/editor integration |
| `--serve` / `--server` / `--api` | Start the TUI with the built-in HTTP API gateway already on (see `/serve`, `/server <port>`) |
| `--debug_log` | Write a debug log to `.dev/debug.log` (also toggleable at runtime via `/debug`) |

## Authentication (DeepSeek web API)

PoopRusteek uses the **reverse-engineered DeepSeek web API** (chat.deepseek.com),
NOT the official API-key product. Auth is a browser **session token** (cookie/token),
and every gated request is unlocked by solving DeepSeek's **SHA-3 proof-of-work
locally** (via an embedded WASM solver) — no paid key, no server round-trip for PoW.

On **first launch** (or after `/logout` / `/wipe`) an in-TUI onboarding screen asks
for your DeepSeek session token and lets you pick the model
(`deepseek-chat` or `deepseek-reasoner`, the latter enabling thinking/expert mode).
It's unofficial and may break if the upstream API changes.

First launch also downloads the ~120 MB embedding model in the background and indexes
saved sessions; the status bar shows progress and the app is usable meanwhile.
Every launch after that is fully offline. Opt out with `/rag off` or
`[semantic] enabled = false`.

## Where things live

Paths are platform-specific (via the `dirs` crate):

| Item | Path |
|------|------|
| Config | `{config_dir}/pooprusteek/config.toml` |
| Data (sessions, history, semantic index, model, `mcp.json`) | `{data_dir}/pooprusteek/` |

- Linux: `~/.config/pooprusteek/config.toml` + `~/.local/share/pooprusteek/`
- Windows: `%APPDATA%\pooprusteek\` (both)
- macOS: `~/Library/Application Support/pooprusteek/` (both)

A missing config just loads defaults (no crash). See the `pooprusteek-commands`
skill for the full command list.

## Lifecycle commands

- `/logout` — clear the DeepSeek token, save config, return to onboarding. Keeps data.
- `/wipe` — factory reset: delete the config-file parent dir + data dir, clear
  whitelist/history, return to onboarding. Never touches foreign configs (`~/.claude`,
  `~/.cursor`, VS Code, etc.). Both prompt for confirmation.
- `/update` — self-update from the GitHub release tagged `latest`: compares the
  running binary's SHA-256, downloads + verifies the new binary on mismatch, stages
  it, and swaps it in on next launch. A `cargo run` dev build asks to confirm first.
- `/autoupdate [on|off]` — run that check in the background on every startup (off by
  default). Bare `/autoupdate` shows status.
