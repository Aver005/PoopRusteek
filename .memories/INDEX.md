# .memories INDEX
> Agent entry point. If you were handed this project cold, START HERE. Last updated: 2026-08-26 (review fix batch → `.docs/review-2026-08-26-rust.md` (20 findings, 7 closed, 1 refuted) + vision Stage 0 → `.docs/vision-local-image-understanding.md` + context-compaction plan decided, not built → `.docs/context-compaction.md`; `JOURNAL/2026-08-26.md`; tests 504). Before: 2026-08-25 (headless harness + Docker sandbox → `reference/HARNESS.md`, `JOURNAL/2026-08-25.md`; tests 474). Before: 2026-07-15 (quality/structure audit → `reference/AUDIT-2026-07-15-QUALITY.md`; BUGS.md refreshed (+8 entries, 2 stale corrected) — clippy 0, tests 434, MSRV 1.91)

> ⚠️ `CLAUDE.md` at the repo root is now the auto-loaded bridge into this folder — **Claude Code**
> reads it automatically and it points here. The **app itself** still does **NOT** auto-load
> `.memories/` at runtime; any other agent only benefits from this folder if it is explicitly told
> to read `.memories/INDEX.md` first. (Wiring runtime auto-load is an open PLANS item.)

## 1. READ ORDER

### Core (read top-to-bottom for a full mental model)
| Step | File | Why |
|------|------|-----|
| 1 | `QUICKSTART.md` | 10s orientation — what/where/how |
| 2 | `STATE.md` | Current snapshot — done / broken / cooking |
| 3 | `MAP.md` | File → purpose → lines map of the whole tree |
| 4 | `ARCHITECTURE.md` | Layers, event loop, data flows, GOAL state machine |
| 5 | `GLOSSARY.md` | Project-specific terms (PoW, ACP, GOAL, jobs…) |
| 6 | `BUGS.md` | Known defects by severity |
| 7 | `PLANS.md` | Roadmap & active priorities |
| 8 | `LEARNINGS.md` | Hard-won gotchas |
| 9 | `CONVENTIONS.md` | Code style to follow when editing |
| 10 | `JOURNAL/` | Dated activity log |

### Reference (look up on demand — deep, file:line-cited)
| File | Covers |
|------|--------|
| `reference/COMMANDS.md` | All 40 slash commands + registry (incl. `/serve`, `/server`, `/refetch-providers`, `/cache-providers`, `/themes`, `/btw`, `/new`, `/chats`, `/agent`, `/agents`, `/logout`, `/wipe`) |
| `reference/PROVIDER.md` | DeepSeek API, endpoints, SSE, PoW, auth |
| `reference/TOOLS.md` | Tool system, agent loop, background PTY |
| `reference/MCP.md` | MCP clients, transports, 8 config sources |
| `reference/CONFIG.md` | Config schema, storage paths, sessions |
| `reference/AUTO-UPDATE.md` | Self-updater (`/update`, `/autoupdate`) + the `latest` CI release channel — swap mechanics, **contract points that must not desync**, failure modes |
| `reference/PROMPTS.md` | Prompt library + skills discovery |
| `reference/HARNESS.md` | **Headless behaviour testing** — `pooprusteek exec`, JSONL traces, scenarios/expectations, pattern mining, mock provider, the Docker sandbox, and why `auto_approve` must stay false |
| `reference/AUDIT-2026-07-02.md` | Full-codebase audit (2026-07-02): severity-ranked defects, `[FIXED]`/`[OPEN]`/`[ACCEPTED]` status |
| `reference/AUDIT-2026-07-04-CLEANUP.md` | Cleanup audit (2026-07-04): duplication / dead code / inefficiency / coupling, with verified-vs-reported status and execution order |
| `reference/AUDIT-2026-07-15-QUALITY.md` | Quality/structure audit (2026-07-15): duplication, dead code, branching, overloaded modules — verified/reported status, execution order, plus verdicts on what NOT to split |

## 2. KEY SIGNALS

| Signal | Meaning |
|--------|---------|
| `[DONE]` | Implemented and working |
| `[WIP]` | In progress, partial |
| `[TODO]` | Planned, not started |
| `[BUG]` | Known defect |
| `[IDEA]` | Proposed, not committed |
| `[?]` | Needs investigation |
| `→ path:line` | Cross-reference to source |

## 3. EXTERNAL CONTEXT

- Original repo (TS): https://github.com/aver005/poopseek
- This project: Rust rewrite (edition 2024, MSRV 1.91), ~42k LOC incl. inline tests (166 files), License MIT.
- LLM backend: DeepSeek **web** API (reverse-engineered, v0) — cookie/token auth, requires PoW.
- Primary verification: `cargo build` + `cargo test --bin pooprusteek` (434 passing) + `cargo clippy` (0 warnings). CI (`.github/workflows/ci.yml`): `test` (build+test, win+linux) and `lint` (fmt --check + clippy -D warnings) are both blocking and gate the dev-build release.

## 4. MAINTENANCE RULE

Update the relevant file on every meaningful change and bump its `Last updated`. Add a `JOURNAL/{date}.md`
entry for notable sessions. Keep claims tied to `→ file:line`. If a fact here contradicts the code, the **code wins** — fix the memory.
