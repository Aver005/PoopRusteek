---
name: PoopRusteek — Skills & Local RAG
description: Author/install markdown skills and drive PoopRusteek's local semantic (RAG) layer. Use when writing a SKILL.md, enabling/disabling skills, or controlling per-turn hints, tool_search, deferred schemas, and history search. Not for editing PoopRusteek's Rust source.
---

# PoopRusteek — Skills & Local RAG

## Skills

A **skill** is a reusable markdown instruction set the agent can load into its system
prompt. PoopRusteek discovers them from a dozen agent-ecosystem directories.

### Authoring / installing

Primary format: one directory per skill containing a `SKILL.md`:

```
{skills_dir}/{skill-name}/SKILL.md
```

`SKILL.md` may start with an optional 3-line YAML frontmatter that overrides the
defaults, then a markdown body:

```
---
name: <short title>
description: <one sentence: what it teaches + when to use it>
---

# <Title>
<lean, actionable body>
```

Legacy format: `*.prompt.md` files (built-in only, e.g. `assets/prompts/`; `base`
and `tools` are excluded).

### Discovery order (first-found-wins per name)

project (`./.skills/`, `./.claude/skills/`) → home agent dirs (`~/.claude`,
`~/.agents`, `~/.cursor`, `~/.windsurf`, `~/.cline`, `~/.codex`, `~/.github/copilot`, …)
→ config dirs (`~/.config/{skills,opencode,Claude,cursor,windsurf,cline,zed}/skills`)
→ data dirs (`~/.local/share/{skills,pooprusteek/skills}`) → built-in
(`assets/prompts/`) → custom paths from `config.skills.paths`.

Drop your skill in any of those. To install into PoopRusteek's own data dir, use
`{data_dir}/pooprusteek/skills/{skill}/SKILL.md`.

### Enabling / disabling

```
/skills                    # picker to toggle skills
/skills list
/skills enable <name>      # pin the skill into the system prompt
/skills disable <name>
```

Enabled skills are concatenated under `# Active Skills` and appended to the system
prompt. Extra search dirs live in `config.skills.paths`. The agent can also
`list`/`load` skills itself mid-conversation via the `skill` tool — so you often
don't need to enable anything by hand.

## Local RAG (semantic layer)

A fully offline hybrid index — multilingual e5-small ONNX embeddings + stemmed TF-IDF,
fused with Reciprocal Rank Fusion — over three corpora: **skills**, **MCP tools**, and
your **conversation history**. The ~120 MB model downloads once on first launch, then
zero network. It degrades gracefully (falls back to lexical, never blocks features).

What it does for you automatically:

- **Per-turn hints** — every prompt is matched against the skill and MCP-tool catalogs;
  top hits ride along as an ephemeral hint ("this skill may apply — load it with the
  `skill` tool"), so the agent discovers capabilities without you enabling them.
- **Deferred MCP schemas** — above 12 tools (`[semantic] mcp_schemas = auto`), the
  system prompt carries only a server-level summary (`playwright (25 tools)`), not each
  tool. Full definitions arrive via the hint or the **`tool_search`** builtin. Disabling
  RAG forces full schemas back on.
- **Builtins for the model** — `tool_search` (find MCP tools on demand) and
  `history_search` (recall solutions from past sessions).

### Controlling it

```
/rag              # how-it-works + live status (state / model / indexed counts / config)
/rag on | off     # persist semantic.enabled and flip the running service
/rag reload       # drop model + corpora, re-verify/re-download, re-embed everything
/rag-limit [<N>|auto|off]   # embedder batch cap (ONNX memory guard); auto sizes from RAM
```

Config (`config.toml`):

```toml
[semantic]
enabled = true          # whole RAG layer; /rag on|off flips this
top_k = 3               # max suggestions per corpus per turn
min_dense_score = 0.80  # cosine floor for hint candidates without keyword overlap
mcp_schemas = "auto"    # "auto" (defer above 12 tools) | "full" | "deferred"
rag_limit = "auto"      # embedder batch cap; set via /rag-limit
```

### Searching your history

```
/search [query]
```

Opens a full-screen search over all saved sessions: `s` cycles sort
(relevance / newest / oldest), `r` filters by role, `u` dedups to one hit per session.
Enter on a result loads that session. The history index lives at
`{data_dir}/semantic/history.json` — a rebuildable cache; session files are the source
of truth.
