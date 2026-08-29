# REFERENCE: Prompt Library & Skills
> System prompts and skill discovery. Source: `src/prompts.rs`, `assets/prompts/`, `src/skills/`.
> Last updated: 2026-07-13 (prompt diet: base 10.8→3.7 KB / tools 8.2→2.8 KB, single-source dedup; `[skills] injection` modes; byte-budget tests)

## PromptFiles (`src/prompts.rs:5`)

Three prompts loaded at startup via `load_prompt_files()` (:11), searching exe-dir / parent / `CARGO_MANIFEST_DIR` / CWD:
- `base_prompt` ← `base.prompt.md`
- `tools_prompt` ← `tools.prompt.md`
- `goal_evaluator_prompt` ← `goal-evaluator.prompt.md`

## System prompt assembly (`app/system_prompt.rs::build`)

```
base_prompt   (placeholders {{user}}, {{folder}}, {{os}} substituted)
+ tools_prompt ({{builtin_tools}} ← tools.definitions(); {{mcp_tools}} ← MCP tools+resources)
+ enabled skills content (full or summary — see `[skills] injection` below)
+ project instructions (AGENTS.md and kin — see below; LAST on purpose)
```
> Still does NOT read `.memories/` itself. It **does** now read the repo's root `CLAUDE.md` as a project instruction file (see below), which is a partial answer to the PLANS item — the curated `.memories/` read order is still opt-in.

### Project instructions (`src/instructions.rs`, `[instructions]`)

`instructions::load(workspace, max_bytes)` collects a **chain** of rule files —
`POOPRUSTEEK.md` → `AGENTS.md` → `CLAUDE.md` → `GEMINI.md`, first match per
directory — walking up from the workspace and stopping at the repository root
(`.git`, file or directory) or the home directory. Nearest file goes **last**.
`~/.pooprusteek/` supplies global user rules, prepended. Result is cached in
`AppState::instructions_section` and refreshed by `app::reload_instructions`
(startup, `/cwd`, `/instructions reload`) — never per turn, because assembly
runs on the event loop (invariant 1).

**This is untrusted text in the most privileged part of the prompt** — it comes
from whatever repository the user cloned. Three defences, none removable
without thought:
- files are read with `symlink_metadata` and **symlinks are refused** — an
  `AGENTS.md -> ~/.ssh/id_rsa` would otherwise be shipped to the provider;
- the content sits inside an envelope marked with a per-process nonce, and any
  occurrence of that nonce is stripped from the content, so a file cannot forge
  the closing marker and continue as "system" text;
- the absolute rules are **re-asserted after** the envelope, so the last word in
  the prompt belongs to the system, not to the file.

`[instructions] enabled` (default true) turns the whole thing off;
`max_bytes` (default 16 KiB) caps the **whole section**, not each file.

Context-budget behaviors (2026-07-13):
- **Skills injection** (`SkillInjectionMode`, `[skills] injection = auto|full|summary`, default auto): full content while combined enabled-skill bytes ≤ 8 KB (`AUTO_FULL_BUDGET_BYTES`), otherwise a compact `slug — description` list + an instruction to `skill load` on demand (the per-turn semantic hint names the matching skill). `discovery.rs::load_enabled_skills_content(skills, injection)`.
- **MCP resources** itemization sits behind the same deferred gate as tool schemas: when deferred, only a count line is emitted.
- **Size telemetry**: every assembly logs `system_prompt.assembled` (per-section bytes: base/tools/builtin_defs/mcp/skills/instructions/total) to the debug log; `instructions::load` logs `instructions.loaded` (`files=` / `bytes=` / `truncated=`) — the file **count** is what the harness asserts on, because byte size drifts with whatever repository the scenario runs in.
- **Byte budgets enforced by tests**: `prompts.rs` (base < 5000 B, tools < 4500 B), `tools/registry.rs` (formatted builtin defs < 10000 B), `instructions.rs` (section ≤ `max_bytes` + a ~1830 B envelope). Grow them only deliberately, in the same commit as the justification.

## assets/prompts/ catalog

| File | Lang | Role |
|------|------|------|
| `base.prompt.md` | RU | Agent persona "Pooprusteek" + 7 ranked rules (no-invention, read-before-edit, reversibility, secrets, error handling, question discipline, skills/MCP priority) + short `<thinking>` note + response style. Slimmed 2026-07-13 (10.8→3.7 KB): tool-syntax duplicate, decision-framework table, error/thinking protocols and the `最少` artifact removed — single source for tool syntax is `tools.prompt.md` |
| `tools.prompt.md` | RU | Tool-use reference: the one canonical `<tool_use>` XML wrapper + rules, compressed background/interactive guidance (details live in tool schemas: `shell_input` keys enum, `question` modes), skills note, `{{builtin_tools}}`/`{{mcp_tools}}` lists. Slimmed 2026-07-13 (8.2→2.8 KB) |
| `compact.prompt.md` | RU | History-compaction prompt: dense summary (context, constraints, decisions, done, todo, risks, artifacts) — used by `/compact` |
| `goal-evaluator.prompt.md` | EN | GOAL-mode evaluator: returns `**Status:** SUCCESS/FAILURE` + summary/issues/feedback |
| `poet.prompt.md` | RU | "Rhyme mode" persona — forces prose into verse (tools not rhymed) |
| `review.prompt.md` | RU | Code-review persona: critical/important/moderate/nitpick tiers |
| `refactor.prompt.md` | RU | 7 refactoring strategies (S01–S07) with triggers/impact/effort/playbook |
| `role-creator.prompt.md` | RU | 8-step interactive custom-role creation guide |
| `figma.prompt.md` + `figma/*.prompt.md` | EN | Figma design-agent suite: `main, intent, builder, designer, enhancer, handyman, uikit, censor` — staged token→primitive→compose→compile flow |

These `*.prompt.md` files double as **built-in skills** (see below), except `base`/`tools` which are excluded.

## SKILLS (`src/skills/`)

- **`SkillDefinition`** (`skills/mod.rs:22`): `name, slug, description, source(BuiltIn|Local|Installed), content, enabled`.
- **Discovery** (`skills/discovery.rs:84`) scans, in order: project (`./.skills/`, `./.claude/skills/`) → home agent dirs (`~/.claude`, `~/.agents`, `~/.cursor`, `~/.windsurf`, `~/.cline`, `~/.codex`, `~/.github/copilot`, …) → config dirs (`~/.config/{skills,opencode,Claude,cursor,windsurf,cline,zed}/skills`) → data dirs (`~/.local/share/{skills,pooprusteek/skills}`) → built-in (`assets/prompts/`) → custom paths from `config.skills.paths`.
- **Formats**: primary = `{dir}/{skill}/SKILL.md` (recursive, owner-repo style); legacy = `*.prompt.md` (built-in only, excl. base/tools). Optional 3-line YAML frontmatter (`name`, `description`) overrides defaults.
- **Injection**: enabled skills appended to the system prompt under `# Active Skills` — full content or a compact on-demand list depending on `[skills] injection` + the 8 KB auto budget (see "System prompt assembly" above).
- **Runtime**: the `skill` tool lets the agent `list`/`load` skills mid-conversation; registry holds them in `Arc<RwLock<Vec<SkillDefinition>>>`.
