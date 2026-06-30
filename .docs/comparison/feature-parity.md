# Feature parity matrix

Legend: ✅ present · ◑ partial/limited · ❌ absent · — n/a

> Sourced from a file-referenced audit of both repos (2026-06-30). pooprusteek figures
> cross-checked against direct knowledge of the code.

## High-level subsystems

| Subsystem | poopseek (TS) | pooprusteek (Rust) | Notes |
|---|:---:|:---:|---|
| Interface | ◑ line-based CLI / readline | ✅ full-screen **TUI** (ratatui) | Fundamentally different UI layer; not directly portable |
| LLM providers | ✅ 8 (multi-provider) | ◑ 1 (DeepSeek; OpenAI/Custom stubs) | poopseek: deepseek-web, openai, openrouter, hugging-face, claude, gemini, ollama, lm-studio |
| Agent loop (multi-step tools) | ✅ | ✅ | Both: system prompt + history → stream → parse tools → execute → loop |
| Tool-call parsing | ✅ streaming fenced-code parser | ◑ XML `<tool_use>` + legacy `[TOOL:]` | poopseek parses tools mid-stream as fences close; pooprusteek parses after each step |
| Context manager / compaction | ✅ module + `/compact` + rich prompt assembly | ◑ `/compact` (naive summary), no manager module | poopseek layers skills/MCP/role/poet/figma into the system prompt |
| Sub-agents | ✅ `agent.ask` / `agent.parallel` | ❌ | poopseek spawns isolated cloned-provider sub-agents for analysis/JSON tasks |
| Sidechat (`/btw`) | ✅ | ❌ | Async side-question without forking main history |
| Roles / personas | ✅ `/role` + guided creation | ❌ | `.role.md` personas injected into system prompt |
| GOAL mode (worker/evaluator loop) | ❌ | ✅ | **pooprusteek original**: two agents iterate to a goal, cap 10, session swaps |
| RAG / semantic code search | ✅ e5-small + BM25 (FTS5) | ❌ | `/rag` + `codebase.index`/`codebase.search` tools |
| Figma design pipeline | ✅ full (server + plugin + JSX→ops) | ❌ | `/figma`, `/scope figma`; ~73 files |
| MCP | ✅ stdio + HTTP, 9-source discovery | ◑ stdio only (HTTP/SSE stubbed), fewer sources | Both: lazy discovery, status cache, `/mcp` |
| ACP (Zed Agent Client Protocol) | ✅ client + server, `/acp` registry | ◑ server stub only (`--acp`), no `/acp` cmds | poopseek can drive/host external ACP agents |
| Skills (SKILL.md discovery) | ✅ (49+ dirs) | ✅ (11+ dirs) | Parity on concept; both inject into prompt |
| Security / permission gate | ✅ strict/relaxed/off + decisions + audit | ◑ tool whitelist only | poopseek gates by file-path patterns (.ssh, .env, *.pem…) with audit log |
| Workspace lock / isolation | ✅ `/workspace lock` | ❌ | Restrict agent file access to workspace |
| Background process mgmt (PTY) | ◑ via bash tool | ✅ rich (`background/` module) | pooprusteek: detached pipes + PTY, idle TTL (1800s), persistent dev servers |
| Session persistence (local JSON) | ✅ | ✅ | Both store JSON sessions; pooprusteek tags GOAL system sessions |
| Remote session import (DeepSeek) | ✅ `/history` | ✅ `/load` remote, `fetch_remote_session_messages` | |
| Multi-provider auth flow | ✅ `/auth`, `/provider`, `/logout` | ❌ (DeepSeek token only) | |
| Web search tools | ✅ `web.search`/`web.fetch` + `/web` | ❌ | DuckDuckGo + native toggle |
| Extended thinking toggle | ◑ `/think` | ◑ model_type "expert" | pooprusteek picks expert by model name |
| POET mode | ✅ `/poet` | ❌ | Novelty/style mode |
| Themes | ✅ `/theme dark|light` | ◑ single dark theme | |

## Built-in tools

poopseek exposes **~30 structured tools**; pooprusteek exposes **8** and expects the model
to use the shell for file/git/search work.

| Tool family | poopseek | pooprusteek | Note |
|---|:---:|:---:|---|
| `bash` / `powershell` | ✅ | ✅ | pooprusteek unified into one `ShellTool` + adapter; background/PTY modes |
| `shell_output/kill/list/input` | ◑ (via bash bg) | ✅ | pooprusteek drives long-running/interactive procs explicitly |
| `file.read/write/edit/find/list/remove` | ✅ | ❌ | **Biggest tool gap** — pooprusteek does file ops via shell |
| `grep` (ripgrep) | ✅ | ❌ | via shell in pooprusteek |
| `git` / `git.edit` | ✅ | ❌ | via shell in pooprusteek |
| `memory.save/read/list` | ✅ | ❌ | persistent named memory |
| `todo.read/write` | ✅ | ❌ | |
| `user.ask/choice/confirm` | ✅ | ◑ single `question` tool (yes_no / multiple_choice) | |
| `agent.ask/parallel` (sub-agents) | ✅ | ❌ | |
| `codebase.index/search` (RAG) | ✅ | ❌ | |
| `mcp.describe/read` | ✅ | ◑ (MCP tools dispatched, no explicit describe/read tool) | |
| `web.search/fetch` | ✅ | ❌ | |
| `skill.read` / `skill` | ✅ `skill.read` | ✅ `skill` (list/load) | parity |
| `role.save` | ✅ | ❌ | |
| `tools.list` | ✅ | ◑ `/tools` command | |

## Slash commands (counts)

| | poopseek | pooprusteek |
|---|:---:|:---:|
| Total slash commands | ~47 (+aliases) | ~25 (+aliases) |
| Unique-to-poopseek | `/auth /provider /logout /relogin /history /deattach /model /switch /think /web /poet /refactor /review /workspace /role /scope /acp /figma /skills-folder /rag /security /update /maestro /noob /back /btw /stats /theme` | — |
| Unique-to-pooprusteek | — | `/goal` (GOAL mode), `/jobs`·`/ps` (bg procs), `/mcp reload`, `/rate`, `/retry` |
| Shared (names) | `/attach /load /export /session(s) /clear /compact /reset /help /quit /tools /mcp /skills` | same |

## Agent-loop knobs

| | poopseek | pooprusteek |
|---|---|---|
| Max steps / turn | 256 (configurable; sidechat 6) | 256-ish (`config.agent.max_steps_per_turn`) |
| Max tools / step | 10 (CLI override 24) | 10 (`max_tools_per_step`) |
| Stream idle timeout | — | 120s (abort turn) |
| Rate-limit retry | exp backoff 2→30s, 5 tries | configurable `/rate` + `/retry` (−1 = infinite) |
| Tool format | fenced ```json/ts blocks | XML `<tool_use>` / `[TOOL:…]` |

## Architecture notes that affect portability

- **UI layer is not shared**: poopseek = readline + ANSI view-manager; pooprusteek =
  ratatui full-screen + event loop. Input/rendering features (file-mention completion,
  view-manager, generation indicator) do **not** port directly — only their *behavior*
  can be reimplemented.
- **Provider abstraction differs**: poopseek's `ILLMProvider` already supports `clone()`,
  `withImages()`, `listModels()`, capabilities — that interface shape is what makes
  multi-provider + sub-agents cheap. pooprusteek's `LLMProvider` trait is narrower.
- **Tool philosophy differs**: poopseek = many structured, individually-gated tools;
  pooprusteek = shell-centric. Porting poopseek tools means also porting the security gate
  to keep them safe.
