# Donor roadmap — what to port, which way, and how hard

Bidirectional. Effort is a T-shirt size for a focused implementer who knows both stacks:
**S** ≈ hours · **M** ≈ 1–2 days · **L** ≈ several days · **XL** ≈ a week+.

Each item notes the **donor** (source of the idea/code) and **recipient**, plus caveats —
especially the TUI-vs-CLI and provider-interface differences that block literal copying.

---

## A. poopseek → pooprusteek (TS original feeds the Rust port)

The Rust port is missing most of poopseek's surface. Highest leverage first.

| # | Port | Effort | Why / caveat |
|---|---|:---:|---|
| A1 | **Structured file tools** (`file.read/write/edit/find/list/remove`, `grep`) | **M** | Biggest day-to-day gap. pooprusteek does file ops via shell — slower, riskier, no overwrite-protection. Port as Rust `Tool` impls. **Pair with A2.** |
| A2 | **Security/permission gate** (path patterns, allow/deny once/always, audit) | **M** | Prereq for safely adding structured tools. pooprusteek only has a flat whitelist. Port poopseek's `tool-executor` security check + decision store. |
| A3 | **Multi-provider** (`ILLMProvider`-style trait with `clone()`/`listModels()`/capabilities; OpenAI-compat, Claude, Gemini, Ollama) | **L** | pooprusteek's `LLMProvider` trait is too narrow. Widen it first (add `clone`, capabilities), then add an OpenAI-compatible adapter (covers openrouter/ollama/lm-studio/HF at once). Claude/Gemini are separate adapters. |
| A4 | **Sub-agents** (`agent.ask`/`agent.parallel`) | **M** | Depends on A3's `clone()`. Spawn isolated cloned-provider agents for analysis/JSON. Reuse the clone-provider isolation pattern (robustness §5). |
| A5 | **RAG** (`codebase.index`/`codebase.search`, `/rag`) | **XL** | e5-small embeddings + BM25 over SQLite FTS5. In Rust: `fastembed`/`candle` for embeddings + `rusqlite` FTS5. Heaviest item; high value for large repos. |
| A6 | **Roles** (`/role`, `.role.md` personas) | **S–M** | Persona text injected into system prompt. Cheap; mostly prompt plumbing + a picker (pooprusteek already has pickers). |
| A7 | **Context-manager discipline** (a real module; layered system-prompt assembly; smarter `/compact`) | **M** | pooprusteek assembles prompts ad hoc. Port the layered snapshot (skills/MCP/role/web/poet) into a dedicated module. |
| A8 | **`memory.*` and `todo.*` tools** | **S** | Small, self-contained, useful. Straight ports. |
| A9 | **MCP HTTP transport** | **S–M** | pooprusteek stubs HTTP/SSE; poopseek uses the official SDK's `StreamableHTTPClientTransport`. Implement HTTP transport in `mcp/transport.rs`. |
| A10 | **`/refactor` and `/review` flows** | **M** | Internal agent loops with tuned prompts + step budgets and (review) git-diff scoping. Mostly prompt + orchestration; reuses the existing loop. |
| A11 | **web.search/web.fetch + `/web`** | **S–M** | DuckDuckGo + fetch tools, gated by a toggle. |
| A12 | **Sidechat (`/btw`)** | **S** | Needs A3 `clone()`. Small once sub-agents exist. |
| A13 | **ACP client + `/acp` registry** | **L** | pooprusteek only has a server stub. Port the Zed ACP client to drive/host external agents. Niche unless IDE integration matters. |
| A14 | **Figma pipeline** | **XL** | ~73 files, plugin + server + JSX→ops. Only if Figma is a goal; otherwise skip. |

**Suggested first wave for pooprusteek:** A2 + A1 (safe structured tools), then A3 (widen
provider trait) → A4 (sub-agents). That closes the most painful gaps without the XL items.

---

## B. pooprusteek → poopseek (Rust port feeds the TS original)

Fewer items, but real.

| # | Port | Effort | Why / caveat |
|---|---|:---:|---|
| B1 | **Signal-aware `waitForNext()`** (fix the input-queue/`ask-user` abort leak) | **S** | Direct fix for poopseek's only HIGH-severity robustness bug (robustness §2). Thread the turn `AbortSignal` into the queue; reject/clear `pendingWaiter` on abort; call it from *every* abort path, not just `/home`. pooprusteek's "wait-inside-abortable-task" is the conceptual donor. |
| B2 | **GOAL mode** (two-agent worker/evaluator iterate loop) + its hardening | **M** | poopseek has no iterate mode. Port `GoalState` + `apply_verdict` semantics **and** the guards (cancel-on-abort, `MAX_ITERATIONS` cap, no-provider bail, empty-input nudge — robustness §3). The naive version wedges; bring the tests. |
| B3 | **Background/PTY process tooling** (`shell_output/kill/list/input`, persistent dev-servers, idle TTL) | **M** | pooprusteek manages long-running/interactive processes explicitly; poopseek leans on the bash tool. Port the explicit job model if poopseek users run dev servers/wizards. |
| B4 | **Decomposition/testing discipline** (pure tested cores: state machines, SSE buffer, view-model) | **pattern, not code** | Architectural influence, not a literal port (different language). poopseek is already modular; mainly a reminder to keep pure cores unit-tested. |

**Suggested first wave for poopseek:** B1 (cheap, fixes a real "input disappears" bug),
then B2 if an autonomous iterate mode is wanted.

---

## C. Shared follow-ups (both)

| Item | Effort | Note |
|---|:---:|---|
| **Session-head resync** after an abnormal stream that captured no id | M | Closes the last residual fork window in *both* (robustness §1). Needs a "get session head message id" call to the DeepSeek API. |
| **Real tokenizer** instead of `chars/4` | M | Both estimate naively; affects compaction/budget accuracy. |

---

## How to use this roadmap

- If the goal is **make the Rust TUI a daily driver** → do **A2+A1, A3, A4** first; they
  remove the sharpest friction (no file tools, one provider, no sub-agents).
- If the goal is **harden the mature TS agent** → do **B1** now (it's an S that kills a
  real bug), consider **B2** for an autonomous mode.
- The **XL** items (RAG, Figma) and **L** items (multi-provider, ACP client) are projects
  in their own right — schedule deliberately, not opportunistically.
- Remember the two hard walls: **UI layer doesn't port** (TUI vs readline — reimplement
  behavior, not code), and **provider-interface width** gates A3/A4/A12 (do A3 first).
