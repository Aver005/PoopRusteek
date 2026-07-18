# Memories templates & workflows

Ready-to-paste starter content and checklists. Read this when actually writing
or auditing a `.memories/` KB — the `assets/bootstrap-memories.sh` script emits
the skeletal versions; the templates here are richer and annotated.

## Contents
- [Per-file templates](#per-file-templates)
- [JOURNAL entry template](#journal-entry-template)
- [Audit checklist (existing project)](#audit-checklist-existing-project)
- [Quality bar / anti-patterns](#quality-bar--anti-patterns)

---

## Per-file templates

The bootstrap script writes minimal stubs for all of these. Below is guidance on
*filling* them well — the distinctions that make a KB worth reading.

### INDEX.md
The router, not a knowledge store. Three sections: (1) a read-order table with a
one-line "why" per file, (2) the status-signal legend used across the KB, (3) the
maintenance rule + a stale-detection timestamp. Add an "external context" block
if the project has an upstream/original repo, a license, or a size/LOC figure —
those orient a newcomer fast. Keep it under a screen.

### QUICKSTART.md
Optimize for 15 seconds. Three headers: **WHAT** (one paragraph — project +
stack + who it's for), **WHERE** (a hot-files table: `path | role`, ~10 rows
max, only files someone edits in week one), **HOW** (build/run/test one-liners +
the CI gate). End with **CRITICAL NOTES**: the two or three facts that will
surprise a newcomer (e.g. "config isn't read from `.memories/`", "no native
function-calling — tool calls are parsed from text"). These notes are the
highest-value lines in the whole KB.

### STATE.md
The living pulse. Lead with a dated **Current focus** line so a returning agent
knows where work stopped. Then three buckets — **Working `[DONE]`**, **Broken
`[BUG]`** (link to BUGS.md), **In progress `[WIP]`**. This is the file you touch
most; keep it honest and current or it poisons every decision made from it.

### MAP.md
`path → purpose`, one line each, for every meaningful file and directory. This is
the "where does X live" lookup. Group by directory. Don't explain *how* the code
works (that's ARCHITECTURE) — just *what each file is responsible for*. A big
tree is fine here; it's a lookup table, not prose.

### ARCHITECTURE.md
The *why*. Start with the single mental model that makes the codebase click (the
core loop, the data-flow spine, the state machine). Then layers/modules and their
responsibilities, then **key decisions with their rationale** — the tradeoff each
one resolves and what breaks if you undo it. Decisions-without-why rot into
cargo-cult rules; the reasoning is what lets a future agent judge when a decision
no longer applies.

### GLOSSARY.md
Only project-coined terms and acronyms — the words a smart outsider wouldn't
know. One line each. Skip general programming vocabulary. If your codebase says
"PoW", "GOAL loop", "sidechat", "forked session" — define them here so every
other file can use the shorthand freely.

### BUGS.md
Severity-ranked. Per entry: **Symptom** (observable), **Where** (`→ file:line`
or function name), **Status** signal, and root cause if known. Keep an **Open**
and a **Resolved** section; when a bug is fixed, move it to Resolved (with the
date and the fix) or delete it — an open-looking entry for a fixed bug wastes an
agent's time chasing a ghost.

### PLANS.md
**Active** (in-flight priorities), **Planned** (next up), **Owner decisions
pending** (`[?]` questions only a human can answer). No done work, no ownerless
wishlist. When a plan ships, it leaves this file and lands in JOURNAL + STATE.

### LEARNINGS.md
The scar tissue. Each line is a gotcha that already cost someone time: "X looks
like Y but is actually Z", a build flake and its workaround, a footgun in a
dependency, a wrong assumption a previous agent made. This is the file that
saves the most hours per byte. Nothing derivable from a plain read of the code
belongs here — only the non-obvious.

### CONVENTIONS.md
Two parts: **Invariants** (rules the codebase structurally depends on — with the
*why*, because an unexplained invariant gets "cleaned up" by the next refactor)
and **Style** (naming, error handling, module decomposition the code already
follows). Don't restate the linter config; capture what the linter can't enforce.

---

## JOURNAL entry template

One file per notable session: `JOURNAL/YYYY-MM-DD.md`. For multiple sessions in a
day, append `# Part 2 — ...` sections under the same file (see this repo's
`.memories/JOURNAL/2026-07-15.md` for the multi-part pattern). Keep it factual
and verification-anchored.

```markdown
# YYYY-MM-DD — <one-line title of the session>

**What**: <what was done and the method — e.g. "7 parallel subsystem sweeps +
lead verification" or "fixed the exit-hang the user reported">.

**Output**: <artifacts produced — files changed, a reference/ doc written, tests
added>.

**Headlines**:
- <the 3–7 most important findings or changes, each one line>

**Verification**: <the gate that proves it — "437 tests pass, clippy 0, fmt
clean" — state failures honestly if any>.

**Next**: <what's queued, and any owner decisions waiting on a human>.
```

Why this shape: a JOURNAL entry answers "what happened and can I trust it?" months
later. The **Verification** line is what separates a log from a wish — always
record the actual result (pass counts, build status), and if something was
skipped or failed, say so plainly.

---

## Audit checklist (existing project)

Use when converting scattered notes or repairing a drifted KB.

1. **Inventory** — collect every existing knowledge source: README(s), doc
   comments, wiki, issue threads, `grep -rn "TODO\|FIXME\|HACK\|XXX"` over the
   source, design docs, chat scrollback.
2. **Sort** each fact into the file table (see SKILL.md). Discard anything the
   code states plainly — memories are for the non-derivable only.
3. **Verify before importing** — run the build, run the tests, grep the claims.
   Anything you can't confirm gets marked `[?]`, never laundered into "fact".
4. **Detect drift** — where an existing note contradicts the current code, the
   code wins: correct the note and leave a one-line "(old note said X — wrong as
   of DATE)" so the next reader trusts the correction.
5. **Scaffold** with `bootstrap-memories.sh`, then move the verified facts into
   the right files.
6. **Seed one JOURNAL entry** — "KB established from prior notes on DATE"; do not
   reconstruct full history.
7. **Wire the bridge** (`CLAUDE.md`/`AGENTS.md`) to point at `.memories/INDEX.md`.
8. **Prune** — delete resolved bugs, shipped plans, and stale prose. Value is
   inversely proportional to noise.

---

## Quality bar / anti-patterns

A memory is good when acting on it changes what an agent does and it can't be
derived from the repo. Red flags to avoid:

- **Restating the code** — "the `parse()` function parses input". If a plain read
  reveals it, cut it.
- **Duplicating a fact across files** — state it once, link from the rest.
  Duplication guarantees the copies diverge.
- **Line-number anchors in living files** — they drift on the next edit. Anchor to
  names; reserve `→ file:line` for `reference/`/`BUGS.md` and accept it needs
  refreshing.
- **Undated living files** — a reader can't judge staleness. Stamp `Last updated`.
- **Empty stub files** — a header with no content reads as "covered" when it
  isn't. Delete a file you can't fill.
- **Aspirational fiction** — documenting how you *wish* it worked. The KB must
  describe reality; wishes go in PLANS.md marked `[IDEA]`/`[TODO]`.
- **Unmaintained** — the worst state. An agent trusts the KB and acts on stale
  facts. If you can't keep a file current, delete it rather than let it lie.
