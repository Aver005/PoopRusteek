#!/usr/bin/env sh
# bootstrap-memories.sh — scaffold a `.memories/` knowledge base in the current repo.
#
# Idempotent and non-destructive: it creates only files that do not already
# exist and never overwrites your content. Run it from the repo root.
#
#   sh bootstrap-memories.sh            # scaffold .memories/ + a bridge file
#   sh bootstrap-memories.sh --minimal  # only INDEX/QUICKSTART/STATE/ARCHITECTURE/JOURNAL
#   sh bootstrap-memories.sh --no-bridge  # skip creating CLAUDE.md/AGENTS.md
#
# The stubs are deliberately skeletal — fill them from evidence (read the code,
# run the build), not assumption. See references/templates.md for richer starters.
set -eu

MINIMAL=0
BRIDGE=1
for arg in "$@"; do
  case "$arg" in
    --minimal) MINIMAL=1 ;;
    --no-bridge) BRIDGE=0 ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done

DIR=".memories"
TODAY="$(date +%Y-%m-%d)"
created=0
skipped=0

# write_if_absent <path> <<'EOF' ... EOF   (content on stdin)
write_if_absent() {
  path="$1"
  if [ -e "$path" ]; then
    printf '  skip (exists): %s\n' "$path"
    skipped=$((skipped + 1))
    cat >/dev/null   # drain heredoc
    return 0
  fi
  mkdir -p "$(dirname "$path")"
  cat >"$path"
  printf '  create:        %s\n' "$path"
  created=$((created + 1))
}

echo "Scaffolding $DIR/ ..."

write_if_absent "$DIR/INDEX.md" <<EOF
# .memories INDEX
> Agent entry point. If you were handed this project cold, START HERE. Last updated: $TODAY

The **app does not auto-load this folder** — an agent only benefits if it is told
to read \`.memories/INDEX.md\` first (that is what the root bridge file is for).

## 1. Read order (top-to-bottom for a full mental model)
| Step | File | Why |
|------|------|-----|
| 1 | QUICKSTART.md | 10s orientation — what / where / how |
| 2 | STATE.md | Current snapshot — done / broken / cooking |
| 3 | MAP.md | File → purpose map of the tree |
| 4 | ARCHITECTURE.md | Layers, core loop/model, data flows, key decisions |
| 5 | GLOSSARY.md | Project-specific terms |
| 6 | BUGS.md | Known defects by severity |
| 7 | PLANS.md | Roadmap & active priorities |
| 8 | LEARNINGS.md | Hard-won gotchas |
| 9 | CONVENTIONS.md | Code style + invariants to follow when editing |
| 10 | JOURNAL/ | Dated activity log |

\`reference/\` is looked up on demand (subsystem deep-dives + audits, file:line-cited).

## 2. Status signals
| Signal | Meaning | | Signal | Meaning |
|--------|---------|-|--------|---------|
| \`[DONE]\` | Implemented & working | | \`[BUG]\` | Known defect |
| \`[WIP]\` | In progress | | \`[IDEA]\` | Proposed, not committed |
| \`[TODO]\` | Planned, not started | | \`[?]\` | Needs investigation |
| \`→ path:line\` | Cross-reference into source | | | |

## 3. Maintenance rule
Update the relevant file on every meaningful change and bump its \`Last updated\`.
Add a \`JOURNAL/$TODAY.md\`-style entry for notable sessions. Anchor claims to
function/struct **names**, not line numbers. If a fact here contradicts the
code, **the code wins** — fix the memory.
EOF

write_if_absent "$DIR/QUICKSTART.md" <<EOF
# QUICKSTART
> Agent: read this first. ~15s. Last updated: $TODAY

## WHAT
<One paragraph: what this project is, who it's for, the core tech stack.>

## WHERE (hot files)
| Path | Role |
|------|------|
| <src/main.*> | <entry point> |
| <...> | <...> |

## HOW
- Build: \`<build cmd>\`  ·  Run: \`<run cmd>\`  ·  Test: \`<test cmd>\`
- Verification baseline the CI enforces: \`<lint/test gate>\`

## CRITICAL NOTES
- <The one or two things that will surprise a newcomer.>
EOF

write_if_absent "$DIR/STATE.md" <<EOF
# STATE
> Living snapshot — the most-updated file. Last updated: $TODAY

## Current focus
<$TODAY: what is being worked on right now, and by whom if relevant.>

## Working \`[DONE]\`
- <feature/subsystem that works>

## Broken / degraded \`[BUG]\`
- <what's broken — link to BUGS.md entry>

## In progress \`[WIP]\`
- <what's partially done>
EOF

write_if_absent "$DIR/ARCHITECTURE.md" <<EOF
# ARCHITECTURE
> Layers, the core loop/model, data flows, and the *why* behind big decisions.
> Last updated: $TODAY

## The mental model
<The single diagram/paragraph that, once understood, makes the codebase click.>

## Layers / modules
- <layer> — <responsibility>

## Key decisions (and why)
- <decision> — <the tradeoff it resolves; what breaks if you undo it>
EOF

write_if_absent "$DIR/JOURNAL/$TODAY.md" <<EOF
# $TODAY — .memories bootstrapped

**What**: Established the \`.memories/\` knowledge base from <cold repo | prior
notes>. Scaffolded the skeleton; filled QUICKSTART + STATE + ARCHITECTURE from a
first read of the code and a build run.

**Still unknown / \`[?]\`**: <gaps to close in the next pass>.

**Next**: <the first real task now that the KB exists>.
EOF

if [ "$MINIMAL" -eq 0 ]; then
  write_if_absent "$DIR/MAP.md" <<EOF
# MAP
> \`path → purpose\` for every meaningful file/dir. The "where is X" index.
> Last updated: $TODAY

| Path | Purpose |
|------|---------|
| <dir/> | <what lives here> |
EOF

  write_if_absent "$DIR/GLOSSARY.md" <<EOF
# GLOSSARY
> Project-coined terms & acronyms. One line each. Last updated: $TODAY

- **<Term>** — <definition, one line>.
EOF

  write_if_absent "$DIR/BUGS.md" <<EOF
# BUGS
> Known defects, severity-ranked. Move fixed ones to RESOLVED or delete.
> Last updated: $TODAY

## Open
### [SEVERITY] <short title>
- **Symptom**: <what the user/dev sees>
- **Where**: → <file:line or function name>
- **Status**: \`[BUG]\`

## Resolved
- <date> — <what was fixed> \`[DONE]\`
EOF

  write_if_absent "$DIR/PLANS.md" <<EOF
# PLANS
> Roadmap, active priorities, pending owner decisions. Last updated: $TODAY

## Active
- \`[WIP]\` <current priority>

## Planned
- \`[TODO]\` <next up>

## Owner decisions pending
- \`[?]\` <question that needs a human call>
EOF

  write_if_absent "$DIR/LEARNINGS.md" <<EOF
# LEARNINGS
> Gotchas & non-obvious facts that already cost someone time. Last updated: $TODAY

- **<Thing that looks like X but is actually Y>** — <why, and what to do instead>.
EOF

  write_if_absent "$DIR/CONVENTIONS.md" <<EOF
# CONVENTIONS
> Style rules + invariants to follow when editing. Last updated: $TODAY

## Invariants (do not break)
1. <rule that the codebase depends on — and why>

## Style
- <naming / error-handling / module patterns the code already follows>
EOF

  write_if_absent "$DIR/reference/.gitkeep" <<EOF
EOF
fi

if [ "$BRIDGE" -eq 1 ]; then
  if [ -e "CLAUDE.md" ] || [ -e "AGENTS.md" ]; then
    echo "  skip (exists): bridge file (CLAUDE.md/AGENTS.md already present)"
    skipped=$((skipped + 1))
  else
    write_if_absent "AGENTS.md" <<EOF
# AGENTS.md — bridge into the knowledge base

<One-line description of this project.>

## Read first
This repo keeps a curated knowledge base in \`.memories/\`. **Read
\`.memories/INDEX.md\` first** — it gives the full read order. The app does not
auto-load it; you only benefit if you read it.

## Build / test
\`\`\`
<build cmd>
<test cmd>
<lint cmd>
\`\`\`

## Invariants (non-negotiable)
1. <the rules a contributor must not break — see .memories/CONVENTIONS.md>

## Maintenance
Update \`.memories\` (STATE.md, BUGS.md, JOURNAL/) as part of "done" for any
non-trivial change. Anchor docs to names, not line numbers.
EOF
  fi
fi

echo ""
echo "Done: $created created, $skipped skipped."
echo "Next: fill QUICKSTART.md and STATE.md from evidence (read the code, run the build)."
