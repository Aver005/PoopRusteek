# .memories INDEX
> Agent entry point. Last updated: 2026-06-28T17:12

## 1. READ ORDER

| Step | File | Why |
|------|------|-----|
| 1 | `QUICKSTART.md` | 10s orientation — what, where, how |
| 2 | `STATE.md` | Current snapshot — what's done, broken, cooking |
| 3 | `MAP.md` | Codebase map — file → purpose → lines |
| 4 | `BUGS.md` | Known bugs sorted by pain |
| 5 | `PLANS.md` | Roadmap & active priorities |
| 6 | `LEARNINGS.md` | Hard-won technical knowledge |
| 7 | `JOURNAL/` | Recent agent activity log |

## 2. KEY SIGNALS

| Signal | Meaning |
|--------|---------|
| `[DONE]` | Implemented and working |
| `[WIP]` | In progress, partial |
| `[TODO]` | Not started, planned |
| `[BUG]` | Known defect |
| `[IDEA]` | Proposed but not committed |
| `[?]` | Needs investigation |
| `→ path:line` | Cross-reference to source |

## 3. EXTERNAL CONTEXT

- Repo: https://github.com/aver005/poopseek
- Rewrite target: Rust (from TS)
- API: DeepSeek web API (reverse-engineered, v0)
- License: MIT
- Primary test: `cargo build` — no test suite yet
