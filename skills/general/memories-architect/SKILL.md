---
name: Memories Architect
description: Design, bootstrap, audit, and maintain a durable `.memories/` knowledge base so any agent can regain full project context across sessions. Use this whenever the user wants to set up, structure, clean up, convert, or improve an agent knowledge base / project memory — including phrases like ".memories", "memory bank", "agent context files", "AGENTS.md/CLAUDE.md structure", "onboarding docs for agents", "capture project knowledge", or "what goes in which memory file" — even if they don't say ".memories" verbatim. Applies to both new repos (bootstrapping from scratch) and existing repos (fixing a stale, scattered, or ad-hoc KB). Project-agnostic; the worked exemplar is this repo's own `.memories/`.
---

# Memories Architect

An agent loses everything between sessions. `.memories/` is the fix: a small,
curated, human-and-agent-readable knowledge base checked into the repo that lets
any agent regain a full mental model of a project in minutes — what it is, how
it's built, what's broken, what's next, and the gotchas that already cost
someone a day. It is **not** documentation for end users and **not** a dump of
everything; it is the institutional memory a codebase can't derive from its own
source or git history.

Use this skill to **bootstrap** a KB in a new project, **audit/convert** a messy
or ad-hoc one, or settle **what-goes-where** questions while writing memories.

## The one rule that makes it work

**Write only what the code and git can't tell you.** File structure, function
signatures, and past commits are already in the repo — re-stating them rots the
moment someone edits. Memories capture the *non-derivable*: intent, current
status, hard-won gotchas, the roadmap, and a map that points *into* the code.
If a memory ever contradicts the code, **the code wins** — fix the memory.

## Canonical structure

One folder at the repo root, plus a bridge file the host agent auto-loads.

```
.memories/
  INDEX.md          # entry point — read order + status signals + maintenance rule
  QUICKSTART.md     # ~15s orientation: what / where (hot files) / how (build+run)
  STATE.md          # living snapshot: done / broken / in-progress. The most-updated file.
  MAP.md            # file → purpose map of the whole tree (the "where is X" index)
  ARCHITECTURE.md   # layers, data flows, the core loop/model, key decisions & why
  GLOSSARY.md       # project-specific terms & acronyms (one line each)
  BUGS.md           # known defects, severity-ranked, with status markers
  PLANS.md          # roadmap / active priorities / owner decisions pending
  LEARNINGS.md      # gotchas & non-obvious facts that already bit someone
  CONVENTIONS.md    # code style + invariants to follow when editing
  JOURNAL/          # dated activity log — one YYYY-MM-DD.md per notable session
  reference/        # deep, on-demand, file:line-cited docs (per subsystem) + audits
```

**Bridge file** (`CLAUDE.md`, `AGENTS.md`, `.cursorrules`, … — whatever the host
agent auto-loads): a short file at the repo root that points into `.memories/`.
Most agents do **not** auto-load `.memories/`; they only benefit if the bridge
tells them to read `.memories/INDEX.md` first. Keep the bridge thin — project
one-liner, build/test commands, the non-negotiable invariants, and "read
`.memories/INDEX.md` first." Everything else lives in the KB.

Scale down for small projects: `INDEX.md` + `QUICKSTART.md` + `STATE.md` +
`ARCHITECTURE.md` + `JOURNAL/` is a complete KB. Add the rest as the project
earns them (a BUGS.md when bugs pile up, a GLOSSARY.md when jargon appears).
Never create an empty file to "complete the set" — a stub with no content is
worse than an honest absence.

## What each file is for (and what it is NOT)

| File | Holds | Does NOT hold |
|------|-------|---------------|
| `INDEX.md` | Read order table, status-signal legend, maintenance rule, external context | Actual knowledge — it only routes |
| `QUICKSTART.md` | 15-second answer to what/where/how; a hot-files table with roles | Deep architecture, full file lists |
| `STATE.md` | What works, what's broken, what's cooking *right now* + a dated "current focus" | Historical narrative (that's JOURNAL) |
| `MAP.md` | `path → purpose` for every meaningful file/dir | Line-by-line code explanations |
| `ARCHITECTURE.md` | Layers, the core loop/model, data flows, *why* decisions were made | Transient status, bug lists |
| `GLOSSARY.md` | Project-coined terms, acronyms, one line each | General programming terms |
| `BUGS.md` | Defects with severity + status + `→ file:line` | Fixed-and-forgotten bugs (move to RESOLVED or delete) |
| `PLANS.md` | Roadmap, active priorities, pending owner decisions | Done work, speculative wishlists nobody owns |
| `LEARNINGS.md` | Gotchas: "X looks like Y but is actually Z", flakes, footguns | Anything obvious from reading the code |
| `CONVENTIONS.md` | Style rules + invariants a contributor must not break | A restatement of a linter config |
| `JOURNAL/` | Dated: what changed, why, verification result, what's next | Real-time chat log; keep it to notable sessions |
| `reference/` | Subsystem deep-dives + full audits, `file:line`-cited | Anything read top-to-bottom every session |

## Conventions that keep it usable

- **Status signals** — a shared legend in `INDEX.md`, used everywhere:
  `[DONE]` `[WIP]` `[TODO]` `[BUG]` `[IDEA]` `[?]`, and `→ path:line` for a
  source cross-reference. Consistent markers make the KB skimmable and greppable.
- **Anchor to names, not line numbers** — reference `run_agent_loop` /
  `AgentRuntime::spawn`, not `line 412`. Line numbers drift on the next edit;
  names survive refactors. Use `→ file:line` only in `reference/`/`BUGS.md`
  where precision matters, and accept it will need refreshing.
- **Timestamp living files** — put `Last updated: YYYY-MM-DD` (+ a one-line
  what-changed) at the top of `STATE.md`, `BUGS.md`, `INDEX.md`. A reader must
  be able to judge staleness at a glance. Use absolute dates, never "yesterday".
- **One fact, one home** — don't repeat the architecture in three files. State
  it in `ARCHITECTURE.md` and *link* from the others. Duplication guarantees
  divergence.
- **Lean and actionable** — every line earns its place. If a paragraph doesn't
  change what an agent would *do*, cut it. A 3k-line KB nobody reads is worthless.

## Bootstrapping a new project

1. **Scaffold** the skeleton — run `assets/bootstrap-memories.sh` from the repo
   root (creates `.memories/` with all template stubs + `JOURNAL/` +
   `reference/`, and a starter bridge file if none exists). It refuses to
   overwrite existing files.
2. **Fill QUICKSTART + STATE first** — these two give the next agent 80% of the
   value. What is this, where are the hot files, how do I build/run/test it, and
   what currently works vs is broken.
3. **Draw MAP + ARCHITECTURE** — walk the tree, one line of purpose per
   meaningful file; then the core loop/model and the *why* behind the big
   decisions. This is where you read the code, not guess.
4. **Seed GLOSSARY / CONVENTIONS** from what you had to learn to be productive —
   the terms that confused you and the rules you inferred from the existing code
   (error handling, naming, the linter/CI gates).
5. **Wire the bridge** — make `CLAUDE.md`/`AGENTS.md` point at
   `.memories/INDEX.md` and carry the build/test commands + invariants.
6. **Open the JOURNAL** — a first entry recording that the KB was bootstrapped,
   from what state, and what's still unknown.

Fill from evidence, not assumption: read the code, run the build, check CI
config. A confidently wrong memory is worse than a missing one.

## Auditing / converting an existing project

Symptoms you're fixing: scattered `NOTES.md`/`TODO.txt`, a stale README doing a
KB's job, tribal knowledge only in someone's head, or a `.memories/` that has
drifted from the code.

1. **Inventory** existing knowledge — READMEs, doc comments, wiki, issue
   threads, `TODO`/`FIXME` in source, chat scrollback. Sort each fact into the
   table above; discard what the code already states plainly.
2. **Verify before importing** — run the build, grep the claims. Mark anything
   you couldn't confirm `[?]` rather than laundering a guess into "fact".
3. **Backfill the JOURNAL sparsely** — you don't need to reconstruct history;
   a single "KB established from prior notes on YYYY-MM-DD" entry is enough.
4. **Fix drift, don't paper over it** — when a memory contradicts the code,
   correct the memory and note the correction in the relevant file (see this
   repo's own `.memories/QUICKSTART.md` lines that say "old memory was wrong").
5. **Prune ruthlessly** — a resolved bug leaves `BUGS.md`, a shipped plan leaves
   `PLANS.md`. The KB's value is inversely proportional to the noise in it.

## Maintenance discipline (the part everyone skips)

Updating the KB is **part of "done"**, not a separate chore. For any non-trivial
change: update `STATE.md` (and `BUGS.md`/`PLANS.md` if relevant), add a
`JOURNAL/{date}.md` entry (what/why/verification/next), and bump the touched
files' `Last updated`. A KB that isn't maintained becomes actively harmful — an
agent trusts it and acts on stale facts. If you can't keep a file current,
delete it rather than let it lie.

## Reference material

- `assets/bootstrap-memories.sh` — idempotent scaffold script (run from repo root).
- `references/templates.md` — ready-to-paste starter content for every file,
  a JOURNAL-entry template, and the full audit checklist.
- **Live exemplar**: this repo's own `.memories/` (start at `.memories/INDEX.md`)
  is a mature, real-world instance of everything above — read it as the worked
  example.
