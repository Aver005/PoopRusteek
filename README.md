<div align="center">

```
██████╗  ██████╗  ██████╗ ██████╗ ██████╗ ██╗   ██╗███████╗████████╗███████╗███████╗██╗  ██╗
██╔══██╗██╔═══██╗██╔═══██╗██╔══██╗██╔══██╗██║   ██║██╔════╝╚══██╔══╝██╔════╝██╔════╝██║ ██╔╝
██████╔╝██║   ██║██║   ██║██████╔╝██████╔╝██║   ██║███████╗   ██║   █████╗  █████╗  █████╔╝
██╔═══╝ ██║   ██║██║   ██║██╔═══╝ ██╔══██╗██║   ██║╚════██║   ██║   ██╔══╝  ██╔══╝  ██╔═██╗
██║     ╚██████╔╝╚██████╔╝██║     ██║  ██║╚██████╔╝███████║   ██║   ███████╗███████╗██║  ██╗
╚═╝      ╚═════╝  ╚═════╝ ╚═╝     ╚═╝  ╚═╝ ╚═════╝ ╚══════╝   ╚═╝   ╚══════╝╚══════╝╚═╝  ╚═╝
```

### Пупра́стик · a fast, beautiful TUI coding agent in Rust, powered by DeepSeek

A free, terminal-native alternative to Claude Code — with parallel chats, sub-agents, an iterative goal loop, MCP with OAuth, multiple LLM providers, and a **fully local RAG layer** over skills, tools, and your own conversation history.

[![Rust](https://img.shields.io/badge/Rust-edition_2024-CE412B?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![MSRV](https://img.shields.io/badge/MSRV-1.91-blue)](https://www.rust-lang.org)
[![TUI](https://img.shields.io/badge/TUI-ratatui-7C3AED)](https://ratatui.rs)
[![Platform](https://img.shields.io/badge/platform-Windows_·_macOS_·_Linux-success)]()
[![License](https://img.shields.io/badge/license-MIT-green)](#-license)

</div>

---

Pooprusteek is a Rust rewrite of the TypeScript [**Poopseek**](https://github.com/aver005/poopseek). It talks to DeepSeek's web API directly (no paid API key), streams responses token-by-token, runs shell tools, speaks MCP, and lets you drive **several conversations and background agents at once** — all inside a single, snappy terminal UI. It also keeps several API keys and subscription logins in one place: accounts, routes, limits and secret storage live behind `/accounts`, and the same routing pool can answer OpenAI-compatible clients through `/serve`.

> [!NOTE]
> Pooprusteek uses the **reverse-engineered DeepSeek web API** (chat.deepseek.com), not the official API-key product. It needs a session token and solves DeepSeek's SHA-3 proof-of-work locally. It's unofficial and may break if the upstream API changes.

## ✨ Highlights

### 🗂️ Parallel conversations, no lost streams
Each chat is an isolated conversation with its **own forked session** — turns never collide.
- **`/new` + `/chats`** — open and switch between parallel sessions; `Tab` / `Shift+Tab` cycles focus.
- **`/btw <question>`** — fire a one-shot side-question that answers in the background without disturbing your main turn.
- Background chats keep streaming while you work elsewhere; the status bar shows how many are live.

### 🧠 Local RAG — the agent finds its own skills, tools, and memories
A **fully offline semantic layer**: multilingual embeddings (e5-small, quantized ONNX via [fastembed](https://github.com/Anush008/fastembed-rs)) + Snowball-stemmed keyword TF-IDF, fused with Reciprocal Rank Fusion. The model (~120 MB) downloads **once** on first launch into the data directory — zero network after that. Works cross-language: a Russian prompt matches English descriptions and vice versa.

Three corpora, one engine:
- **Skills** — every outgoing prompt is matched against the whole skill catalog; the top hits ride along as an ephemeral hint ("this skill may apply — load it with the `skill` tool"). The model discovers capabilities instead of you enabling them by hand.
- **MCP tools** — same matching, plus **deferred schemas**: with many tools connected (one Playwright server ≈ 25 tools), the system prompt carries only a *server-level summary* (`playwright (25 tools)`) — individual tools aren't enumerated at all, saving thousands of tokens per request. Full definitions arrive exactly when needed — inlined into the hint for matched tools, or on demand via the **`tool_search`** builtin. Never bricks: with embeddings unavailable it degrades to lexical search, and with RAG disabled full schemas return automatically.
- **Conversation history** — every saved session is chunked, embedded, and persisted (`data_dir/semantic/history.json`, incremental with per-session watermarks). Search it yourself with **`/search`** — a dedicated screen with sorting (relevance / newest / oldest), role filters, and per-session dedup — or let the agent recall past solutions itself via the **`history_search`** tool. Enter on a result loads that session.

Control it with **`/rag`** (status), **`/rag on|off`**, **`/rag reload`** (re-verify the model, re-embed everything). Retrieval quality is guarded by an MRR eval harness (`cargo test semantic::eval -- --ignored --nocapture`): skills **0.927**, MCP tools **0.836**.

### 🔀 Accounts, routes and limits — several subscriptions in one place
The built-in DeepSeek web client is the default. **`/accounts`** (the old name `/providers` still works) holds everything else:

- **API accounts** — OpenAI Chat Completions, Anthropic Messages, Google Gemini, any local OpenAI-compatible server. The key is a reference (`keyring:`, `env:`, `cmd:`, `file:` or `none`), never a literal in the shared file.
- **Subscriptions through the official CLI** — Claude Code and OpenCode. Each account can keep its own login folder, so a work and a personal subscription of the same kind do not share a session. The CLI runs as a **plain model**: your tools, your approval prompts, our loop. Agent mode for the CLI is a later release.
- **Routes** — an ordered chain of accounts. A step is tried until one answers; the next one is used only for reasons the route names (`rate_limit`, `quota`, `spend_cap`, `overloaded`, `timeout`, `server_error`, `auth`) and only before the first visible chunk.
- **Roles** — `main`, `title`, `summary`, `evaluator`. Chat titles are made by the `title` route (it receives the first user message and the first answer), `/compact` uses `summary`, the GOAL evaluator uses `evaluator`.
- **Limits** — windows come from response headers or the CLI's own events; each snapshot carries its age and source. `soft_pct` moves an account down the order, `hard_pct` takes it out, `switch_at_pct` switches before the wall. Old and estimated numbers never make a hard decision.

Add via the step-by-step wizard or the old one-liner (`/providers add <name> <base_url> [model] [api_key]`). Move accounts with `K`/`J` — the order of the list is the priority of the main route. Pick models with **`/models`**.

Two subscriptions of one kind, step by step:

1. `/accounts` — if the gateway is off, press `Enter` (or `a` to add straight away) to enable it. Existing `[[providers]]` and the DeepSeek token become accounts and a `main` route.
2. `a` — the add-account wizard. Choose **Claude Code** (or OpenCode).
3. Give it a name you will recognise, e.g. `Claude work`.
4. Choose **Own login folder** (recommended). The wizard shows the folder under `~/.pooprusteek/accounts/<id>` and the login command.
5. Repeat for the second subscription with a different name. A shared login is possible, but the wizard warns that signing out in one place, or a parallel session, may log the other out.
6. Run the login command it showed (`claude auth login` / `opencode auth login`) in a normal terminal, then press `u` on the account if it is still marked as needing attention.
7. `Tab` to routes: both accounts are steps of the main route; move or remove steps with `K`/`J`/`x`. `Tab` again for roles.

`/pin <account>[/model]` keeps one chat on one account without changing the global route (`/pin off` clears it). The route seam appears in history when a turn changes accounts.

> [!IMPORTANT]
> **Subscriptions and their terms.** Claude is supported **only through the real Claude Code CLI**; its token is never read, its usage endpoint is never called, and `cli:claude` is never served to other clients through `/serve`. Direct subscription tokens for other providers are opt-in behind an explicit warning — using them may break the provider's terms and get the account blocked. All CLI subscriptions (`cli:*`) are excluded from `/serve`: the gateway is for your own terminal, not a way to share a subscription.

> [!NOTE]
> **What was not verified in this release.** CLI adapters were exercised against a scripted stand-in (`pooprusteek mock-cli`) and their process environment was checked, but **two real logins of one CLI were never run side by side**. The Linux and macOS builds were not run here either (`cfg(unix)` code is compiled by CI, not locally), and **Codex does not work as a plain model** — it is direct-token only. Treat the first real subscription as a check, not a given.

### 🤖 Sub-agents
Spin off isolated agents for sub-tasks — inspired by Claude Code.
- The **model** can spawn them itself via the `task` tool; **you** can via **`/agent <task>`**.
- **Foreground** by default (only the conclusion returns into your turn — clean context), with **`background:true`** opt-in that detaches and notifies on completion.
- **`/agents`** lists running agents and lets you stop them.

### 🎯 GOAL mode — iterate until done
**`/goal`** arms a two-agent loop: a worker pursues your goal, then a separate **evaluator** judges whether it's actually met, feeding back concrete fixes until it passes. Sessions auto-swap after repeated failures to escape dead ends.

### 🛠️ Real tooling
- **Shell** (`bash` / `powershell`) — foreground, background, and **interactive PTY** processes.
- **`/jobs` · `/ps`** — list, kill, and prune background jobs; persistent jobs (dev servers) survive turns with an idle TTL.
- **Tool approval** with a **`/whitelist`** for auto-approving trusted tools.
- **`tool_search` · `history_search` · `skill`** — the semantic builtins that let the model navigate its own capability space.

### 🔌 MCP & 🧩 Skills
- **Model Context Protocol** over **stdio / HTTP / SSE**, auto-discovered from **8 config sources** (own, workspace, Claude Desktop, VS Code, Claude CLI, Cursor, Opencode…). Browse with **`/mcp`**.
- **`/mcp add`** — paste a JSON config, walk a wizard, or one-line it (`/mcp add <name> <command> [args…]`).
- **OAuth authorization** (`/mcp auth`) — full RFC 9728/8414/7591 + PKCE flow for servers that need it; tokens live **encrypted in the OS keyring**, never in plaintext config.
- **Skills** — reusable markdown instruction sets (`SKILL.md`), discovered from a dozen agent-ecosystem directories (`.skills/`, `.claude/skills/`, `~/.agents/skills/`, Cursor/Windsurf/Cline/Codex dirs, …). Enable manually with **`/skills`** to pin one into the system prompt — or let the semantic matcher suggest them per-prompt automatically.

### 💅 Polished terminal UX
Markdown + syntax highlighting, streaming with a live thinking indicator, multi-line input with history, `@file` mentions, session save/load, Markdown export/import, a dedicated search screen, and a Catppuccin Mocha theme — all on an **event-driven loop with no render races**. Logs go to a file in the data dir, never over your screen.

## 📦 Installation

**Windows** — grab **`pooprusteek-setup.exe`** from the
[latest release](https://github.com/Aver005/pooprusteek/releases/latest) and click
through: pick a folder, press Install. No admin rights. It adds `pooprusteek` to
your PATH and the Start menu and registers an uninstaller in *Apps*. Uninstalling
asks whether to delete your settings and sessions too. The installer isn't
code-signed yet, so SmartScreen may warn: *More info → Run anyway*.

**macOS (Apple Silicon) · Linux (x86_64 / arm64)**

```sh
curl -fsSL https://github.com/Aver005/pooprusteek/releases/latest/download/install.sh | sh
```

Installs to `~/.local/bin` and adds it to PATH. Options go after `sh -s --`:
`--dir <path>`, `--channel dev`, `--uninstall`. Linux builds need glibc 2.39+
(Ubuntu 24.04 / Debian 13 or newer); Intel Macs aren't supported, since ONNX
Runtime ships no prebuilt library for them.

The folder you install into is also where `/update` puts new versions, so it
must stay writable by you.

**From source** — `cargo install --path .` (Rust edition 2024, MSRV 1.91).

## 🚀 Usage

```bash
pooprusteek            # launch the TUI
pooprusteek --acp      # run as an ACP server (JSON-RPC over stdio, for IDEs)
pooprusteek --version  # print the version
pooprusteek --debug-log  # developer debug log (data dir; .dev/ in debug builds)
pooprusteek doctor     # config layers, secrets and problems; exit 1 on errors
pooprusteek secrets list          # names and state only
pooprusteek trust      # confirm a project config that changes request targets
```

On first launch, an onboarding screen asks for the DeepSeek token. The account wizard for API keys and subscriptions opens later from `/accounts`. The DeepSeek path: `Ctrl+O` opens chat.deepseek.com, you log in, press `F12` → **Console** and run the one-liner shown on screen (`Ctrl+Y` copies it for you):

```js
copy(JSON.parse(localStorage.userToken).value)
```

It copies your own token into the clipboard — paste it into the token box and press `Enter`. Prefer not to run code in the console? Copy `userToken` from `F12` → **Application** → **Local Storage** instead; the whole `{"value":…}` record is accepted as is.

> [!NOTE]
> **`CLIENT_VERSION_TOO_LOW (code 40005)`** means DeepSeek raised the minimum client version. Run `/update`. If no fixed build is out yet, open chat.deepseek.com with DevTools → **Network**, copy the `x-client-version` request header, and set it as `client_version` under `[provider]` in `config.toml`. **`Authorization Failed (code 40003)`** means the token expired — `/logout` and paste a fresh one.

> [!TIP]
> The first launch also downloads the embedding model (~120 MB) in the background and indexes your saved sessions — the status bar reports progress; everything else is usable meanwhile. Every launch after that is fully offline. Don't want it? `/rag off` or `[semantic] enabled = false`.

## ⌨️ Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `Enter` | Send. During a turn, ordinary text is queued and sent when the turn ends; `/commands` and GOAL-setup text are not queued |
| `Shift+Enter` | Insert a newline |
| `Tab` / `Shift+Tab` | Switch to next / previous chat |
| `Esc` | Cancel generation · cancel GOAL setup · return the queue to the input when idle. `/clear` wipes the chat, `Ctrl+C` quits |
| `Ctrl+C` | Cancel generation / quit |
| `Ctrl+L` | Redraw the screen and jump to the latest message (it does not clear the chat) |
| `Ctrl+P` | Toggle the stats panel |
| `Ctrl+A` | Select all input |
| `↑` / `↓` | Input history / scroll |
| `PageUp` / `PageDown` | Scroll history |

The landing screen keeps the card and the session list on top and the input always at the bottom. While a turn, a sub-agent or the GOAL loop is running, the **activity dock** above the input shows what is happening (at most four rows, then `+N more`); it is not a log of background jobs. Keys for that screen: `↓`/`↑` moves the session cursor, `Enter` opens, `Esc` returns to typing; typing wins over navigation.

## 💬 Commands

<details open>
<summary><b>Chat & sessions</b></summary>

| Command | Description |
|---------|-------------|
| `/new` · `/chats` | Open a new parallel chat · switch between chats |
| `/btw <question>` | One-shot background side-question |
| `/search [query]` | **History-search screen**: semantic + keyword search over all saved sessions — sort (`s`), role filter (`r`), unique-per-session (`u`), Enter opens the found session |
| `/clear` · `/home` | Clear history · return to the landing screen |
| `/compact` | Summarize history to shrink context |
| `/reset` · `/last` | Reset session · open the most recent session |
| `/load <id>` · `/sessions` · `/session` | Load a session · list sessions · current session info |
| `/delete [id]` · `/delete-local [id]` | Delete sessions — account + local, or local-only; multi-select picker with an All/Local/Remote filter and a confirm step |
| `/export [path]` · `/import <path.md>` | Export / import chat as Markdown |
| `/cwd` (`/cd`, `/move`) `<path>` | Change working directory |
| `/attach <path…>` | Attach files to the next message |

</details>

<details open>
<summary><b>Accounts, routes & secrets</b></summary>

| Command | Description |
|---------|-------------|
| `/accounts` · `/providers` | Accounts, routes and limits screen (the old name is an alias). `a` adds, `Enter` shows details, `Space` enables/disables, `e` limits and favourites, `d` removes, `r` joins/leaves the main route, `h` decision history, `u` rebuilds an unavailable account |
| `/accounts add` · `/accounts enable` | Open the add-account wizard · turn the gateway on (old providers and the DeepSeek token become accounts) |
| `/routes` · `/roles` | The same screen focused on route chains · on role routes (`K`/`J`/`x` move and remove steps, `←`/`→` switch route or role) |
| `/pin <account>[/model]` · `/pin off` | Keep this chat on one account without changing the route · clear the pin |
| `/usage` | Calls, failures, fallbacks, tokens and cache share per account over the last 24 hours (estimates are marked) |
| `/providers add [<name> [openai\|anthropic] <base_url> [model] [api_key]]` | Add an account — quick one-liner, confirmed in the wizard |
| `/providers tools <name> <native\|prompt>` | How an account declares tools: the API field or `<tool_use>` in the prompt |
| `/models [id]` | Pick a model from the active account's live list · or switch directly (validated) |
| `/secrets` · `/secrets set <name>` · `/secrets rm <name>` · `/secrets bind <ref> <host>` | Names and state of secrets (never values) · store one behind a mask · delete from the keyring · trust a key for a host of the project config |
| `/doctor` | Where every value came from, which secrets resolve, what the project config holds back, config errors, file modes |

</details>

<details open>
<summary><b>Agents & tools</b></summary>

| Command | Description |
|---------|-------------|
| `/agent <task>` · `/agents` | Launch a sub-agent · list and stop running agents |
| `/goal` | Toggle the iterative goal-driven loop |
| `/jobs [list\|kill <id>\|prune]` · `/ps` | Manage background jobs |
| `/tools` · `/whitelist` | List tools · manage tool auto-approval |
| `/mcp [add\|auth\|ttl <s>\|reload]` | MCP view · add a server (JSON / wizard / one-liner) · OAuth-authorize · cache TTL · reload |
| `/skills [enable\|disable <name>]` | Manage skills |
| `/rag [on\|off\|reload]` | Semantic matching: live status · full on/off switch · reinitialize (verify/re-download the model, re-embed skills + MCP tools + history) |

</details>


<details>
<summary><b>Settings & misc</b></summary>

| Command | Description |
|---------|-------------|
| `/rate <ms>\|<N>/min\|off` | Rate limit: ms between requests and/or max requests per minute |
| `/retry <N\|on\|off\|-1>` | Max retries on API failure (-1 = infinite) |
| `/update [channel stable\|dev]` · `/autoupdate [on\|off]` | Self-update now or pick the update channel · toggle the startup auto-check |
| `/debug [on\|off]` | Toggle debug logging to `.dev/debug.log` (no args = switch) |
| `/help` · `/version` · `/quit` | Help · version · exit |

</details>

## ⚙️ Configuration

The shared config is **`~/.pooprusteek/config.toml`** on every platform. The old location is read once, during the first start after upgrading, and is not read afterwards:

| | Linux | Windows | macOS |
|---|---|---|---|
| Shared config (new) | `~/.pooprusteek/config.toml` | `%USERPROFILE%\.pooprusteek\config.toml` | `~/.pooprusteek/config.toml` |
| Old config (migration only) | `~/.config/pooprusteek/config.toml` | `%APPDATA%\pooprusteek\config.toml` | `~/Library/Application Support/pooprusteek/config.toml` |
| Data, cache, model | `~/.local/share/pooprusteek` | `%LOCALAPPDATA%\pooprusteek` (not roaming) | `~/Library/Application Support/pooprusteek` |

`dirs::data_local_dir()` is used deliberately: the ~120 MB embedding model should not roam between machines. The first start copies user data by an explicit list and leaves everything else in the old folder; the old `config.toml` stays in place for rollback, with its secret values blanked once every reference is confirmed to read back from the keyring. Details and the full list: [`.memories/reference/CONFIG.md`](.memories/reference/CONFIG.md).

**Two layers.** The shared file above, plus an optional project layer `.pooprusteek/config.toml`. The project layer is searched upwards from the launch folder to the git root; outside git only the launch folder itself counts. It overrides the shared file by key; `[[accounts]]` and `[[routes]]` merge by `id`. Because it can be committed, it holds **references, not secrets** — and it is untrusted: a project account gets a key only for a host you confirmed (`pooprusteek secrets bind`), and changes to where requests go wait for `pooprusteek trust`.

`--config <file>` replaces both layers for one run and is the only file written to; `--data-dir <dir>` moves sessions, logs and the index. Use both for harness runs and experiments.

**Secrets.** Keys are not stored in the shared file. A secret field is one of `keyring:global/name` (the system credential store), `env:NAME`, `cmd:["op","read","…"]` (no shell; shared layer only), `file:/path` (shared layer only) or `none`. On first start, plain-text keys are moved into the keyring with a write → read-back → compare check; if the keyring is unavailable nothing is changed and `pooprusteek doctor` says why. The old config file and `config.toml.pre-gateway` never keep usable keys after that.

```toml
[gateway]
schema = 1
enabled = true

[provider]                       # the built-in DeepSeek web client
token = "keyring:global/deepseek-token"
model = "deepseek-chat"

[[accounts]]
id = "openai-work"               # never changes; routes and secrets point at it
label = "OpenAI (work)"          # safe to rename
provider = "openai"              # openai | anthropic | gemini | custom
base_url = "https://api.openai.com/v1"
secret = "keyring:global/openai-work"
model = "gpt-5.5"
favorites = ["gpt-5.5", "your-cheap-model"]

[[accounts]]
id = "claude-work"
label = "Claude Code (work)"
provider = "cli:claude"          # the real Claude Code CLI; no token here
home = "~/.pooprusteek/accounts/claude-work"

[[routes]]
id = "coding"
switch_at_pct = 90
on = ["rate_limit", "quota", "overloaded", "timeout", "server_error", "auth"]
steps = [
  { account = "claude-work", model = "claude-opus-5-5" },
  { account = "openai-work" },
]

[roles]
main = "coding"
title = "cheap"                  # chat titles: first user message and first answer
summary = "cheap"                # /compact
evaluator = "cheap"              # GOAL evaluator
```

The full annotated schema — limits, `allow_mixed_tools`, `[secrets.bindings]`, `[server]`, `[context]`, every section still read from the older files — is [`config.example.toml`](config.example.toml).

Other commands keep their old sections: `[ui]` (theme, glyphs), `[agent]` (steps, rate limits, retries), `[semantic]` (RAG), `[context]` (compaction), `[instructions]` (project rule files). `[[providers]]` and `active_provider` are still read so old files load, but they are no longer written; `/accounts` owns the new form.

> [!WARNING]
> **Upgrading changes the config path and moves the data dir.** Editing the old `%APPDATA%` / `~/.config` file after the first start does nothing: it is a migration source and a rollback copy. The embedding model is downloaded again in the new data folder; sessions, history, `mcp.json`, checkpoints and exports are copied.

### `doctor`, `secrets`, `trust`

```bash
pooprusteek doctor [--json] [--origins]   # layers, sources, secrets, problems; exit 1 on errors
pooprusteek secrets list                  # names and state, never values
pooprusteek secrets set openai-work       # value from stdin or a hidden prompt
pooprusteek secrets test                  # write, read back, delete (also a long value)
pooprusteek secrets rm openai-work
pooprusteek secrets bind keyring:global/openai-work api.openai.com
pooprusteek trust [--yes] [--revoke]      # confirm this project's .pooprusteek config
```

`doctor` never prints a secret value: it names the reference, says whether it resolves, where each setting came from, what the project layer holds back, which accounts the gateway rejects and which files are readable by other users. `trust` records the SHA-256 of the project file in the shared config; editing the file asks again. Confirming the file does not hand over a key — that is `secrets bind`. The same project dialog appears in the TUI at startup and when the file changes.

### What this does not protect against

The keyring keeps keys out of files, backups, cloud sync, commits, logs and accidental reads by the model. It does **not** protect against another process running as you. An agent with `bash` runs with your rights: it can read any file you can, inspect environment variables, call `secret-tool`, `security find-generic-password` or `cmdkey`, and reach the same keyring. Allowing `bash` allows that. `env:` is weaker still — every child process inherits the variable and on Linux it is visible in `/proc/<pid>/environ`. Administrators, malware and memory dumps are outside the threat model too. The model is blocked from writing `.pooprusteek/`, but `bash` is not, so a hostile repository can change the file while the process is running; the next `trust` check sees a different hash and asks again.

## 🌐 Serving the gateway

`/serve` (or `--serve`, `--proxy`) exposes the same accounts over an OpenAI-compatible HTTP API. With the gateway on:

- Models are `auto/<route>` and `<account>/<model>`. A model that is not in this catalog answers 404 without touching an upstream.
- A streaming request is held until the upstream produces its first text chunk, tool call or finish; a failure before that becomes a real HTTP status (`429` + `Retry-After`, `502`, `503`, `504`) and the fallback account's name is in the headers. After the first byte an error is sent as a frame in the stream and no second account is mixed in.
- `x-pooprusteek-account`, `-model`, `-route`, `-fallback` and `-fallback-reason` name who answered and who failed before it. `GET /v1/pooprusteek/status` reports accounts, limits (source and age) and recent decisions behind the server key; without a key it is trimmed and loopback-only.
- `POST /v1/embeddings` and `/v1/rerank` go to the named `<account>/<model>` when that account speaks the OpenAI protocol; `auto/…` is refused there (400).
- `cli:*` accounts are never listed or selected. The Anthropic inbound dialect (`/v1/messages`) still answers 501.

The server key (`[server] api_key`) is a reference too. `pooprusteek doctor` warns if the server listens beyond loopback without one. CORS does not expose the `x-pooprusteek-*` headers, so a foreign web page cannot read the account names.

## ⬆️ Self-update

**`/update`** checks your update channel and installs a newer build. The
binary is downloaded, verified against the release's `manifest.json`, staged
next to the current one and swapped in. It takes effect on the next launch,
because a running executable is renamed aside rather than overwritten.
**`/autoupdate on`** runs the same check in the background on every startup
(off by default, `[update] auto`).

| Channel | Source | Installs when |
|---|---|---|
| `stable` (default) | the latest tagged release `vX.Y.Z` | its version is **newer** than yours; never downgrades |
| `dev` | the rolling `dev` prerelease, rebuilt on every `develop` push | the binary differs from yours |

Switch with `/update channel stable|dev` (`[update] channel`). In a
`cargo run` dev build `/update` asks for confirmation first.

Releases are cut with `scripts/release.sh patch|minor|major` (see
[Releasing](#-releasing)). Integrity is guaranteed, authenticity isn't: trust
rests on TLS + GitHub, and binaries aren't signed yet. The contract between the
app and CI lives in
[`.memories/reference/AUTO-UPDATE.md`](.memories/reference/AUTO-UPDATE.md).

## 🔖 Releasing

```sh
bash scripts/release.sh minor      # or patch | major | 1.4.0; --dry-run to preview
```

The script checks that you're on a clean, up-to-date `develop` and that
`CHANGELOG.md` has something under `[Unreleased]`. It then bumps `Cargo.toml`,
turns `[Unreleased]` into the new version section, commits
`chore(release): 🔖 Release vX.Y.Z`, tags it and pushes both. Pushing the tag
starts `.github/workflows/release.yml`, which runs the checks and builds every
platform plus the Windows installer. It publishes the GitHub release that the
`stable` channel and `install.sh` read. Details:
[`.memories/reference/RELEASING.md`](.memories/reference/RELEASING.md).

## 🏗️ Architecture

Pooprusteek runs on a single `tokio::select!` event loop. The agent runs in a **spawned task** and communicates only through `AppEvent`s and channels — it never touches UI state directly, so there are no render races. All CPU-heavy work (ONNX embeddings, SHA-3 proof-of-work) runs on blocking threads.

```
main ─ App (thin coordinator on a single select! loop)
        ├─ AppState
        │    └─ Conversations ── focused + background chats
        │         each owns: messages · forked provider/session · agent task
        ├─ AgentRuntime.spawn(TurnSpec) ── the one place a turn launches
        │    └─ run_agent_loop ── LLM ↔ tools (events tagged by ConversationId)
        │         ├─ semantic hint ── skills + MCP tools matched per prompt
        │         └─ task tool ⇒ sub-agent (foreground) | detached (background)
        ├─ SemanticService ── local hybrid index (e5-small ONNX + TF-IDF + RRF)
        │    └─ corpora: skills · MCP tools · message history (persistent)
        ├─ ToolRegistry ── bash · powershell · shell_* · skill · tool_search
        │                  history_search · mcp__*
        ├─ MCPManager   ── external tool servers (stdio / http / sse, OAuth)
        ├─ gateway      ── accounts · routes · roles · limits · fallback · ledger
        │    ├─ RoutedProvider ── the LLMProvider every turn and /serve call sees
        │    └─ Backend impls ── API client · DeepSeek web · Claude/OpenCode CLI
        ├─ providers    ── DeepSeek web (PoW + SSE) · OpenAI-compat · Anthropic · Gemini
        │                  fork() ⇒ isolated session per conversation
        ├─ secrets/     ── SecretStore (keyring) · references · redaction · migration
        └─ TUI render ── reads the focused conversation; never mutates
```

```
src/
├── main.rs        — entry point, CLI flags, subcommands (secrets, doctor, trust, harness)
├── app/           — coordinator: conversations, event loop, runtime, keys, goal,
│                    multichat, accounts screen, search screen
├── gateway/       — accounts/routes/roles, engine, routed provider, API/CLI backends,
│                    limits, cooldown, ledger, pending build
├── secrets/       — system keyring store, secret references, resolver, redaction
├── provider/      — DeepSeek web API (auth, PoW, SSE, sessions), typed failures,
│                    OpenAI-compat, Anthropic Messages and Gemini protocol clients
├── agent/         — agent loop, sub-agent runner, tool-call parser
├── semantic/      — local RAG: embedder, hybrid index, corpora, history store, evals
├── tools/         — shell tools, PTY process registry, skill / tool_search / history_search
├── mcp/           — MCP clients, transports, config discovery, manager, OAuth
├── tui/           — ratatui UI: views, widgets, theme, markdown
├── commands/      — slash commands (one file each)
├── skills/        — skill discovery + injection
├── acp/           — Agent Client Protocol server mode
├── config/        — layered config, paths, save queue, migration, doctor/trust support
└── cli/           — `secrets`, `doctor`, `trust`, `@file` mentions
```

> Want the deep dive? The repo ships a curated knowledge base in [`.memories/`](.memories/INDEX.md) — start at `INDEX.md`.

## 🔨 Building

```bash
cargo build              # debug
cargo build --release    # optimized (LTO, single codegen unit, stripped)
```

## 🧪 Testing

```bash
cargo test --bin pooprusteek                              # 2000+ unit tests, no network
cargo test semantic::eval -- --ignored --nocapture        # retrieval-quality evals (MRR);
                                                          # downloads the embedding model once
```

## 🛡️ Git hooks

A pre-commit hook (`.githooks/pre-commit`) blocks commits that fail `cargo
fmt -- --check`, `cargo clippy -D warnings`, or `cargo test`. Not wired up
by default — enable it once per clone:

```bash
git config core.hooksPath .githooks
```

Skip it for one commit (CI runs the same gates, so this only delays the
failure, not avoids it):

```bash
git commit --no-verify
```

## 🙏 Acknowledgements

- [**Poopseek**](https://github.com/aver005/poopseek) — the original TypeScript project this is a rewrite of.
- [ratatui](https://ratatui.rs), [crossterm](https://github.com/crossterm-rs/crossterm), [tokio](https://tokio.rs), [fastembed-rs](https://github.com/Anush008/fastembed-rs), and the broader Rust ecosystem.

## 📄 License

MIT
