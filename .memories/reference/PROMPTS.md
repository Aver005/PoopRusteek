# REFERENCE: Prompt Library & Skills
> System prompts and skill discovery. Source: `src/prompts.rs`, `assets/prompts/`, `src/skills/`.
> Last updated: 2026-06-30

## PromptFiles (`src/prompts.rs:5`)

Three prompts loaded at startup via `load_prompt_files()` (:11), searching exe-dir / parent / `CARGO_MANIFEST_DIR` / CWD:
- `base_prompt` ← `base.prompt.md`
- `tools_prompt` ← `tools.prompt.md`
- `goal_evaluator_prompt` ← `goal-evaluator.prompt.md`

## System prompt assembly (`App::build_system_prompt`, `app/mod.rs:1480`)

```
base_prompt   (placeholders {{user}}, {{folder}}, {{os}} substituted)
+ tools_prompt ({{builtin_tools}} ← tools.definitions(); {{mcp_tools}} ← MCP tools+resources)
+ enabled skills content
```
> Does NOT read `.memories/`. To onboard an agent on this project you must point it at `.memories/INDEX.md` explicitly (or wire auto-loading — see PLANS).

## assets/prompts/ catalog

| File | Lang | Role |
|------|------|------|
| `base.prompt.md` | RU | Agent persona "Pooprusteek" + 7 core directives (think-first, hypotheses, atomic precision, reversibility, security, skills, MCP priority) + decision framework |
| `tools.prompt.md` | EN | Tool-use reference: XML tool syntax, background/interactive mode, `shell_input` keys, skills overview |
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
- **Injection**: enabled skills concatenated under `# Active Skills`, joined by `\n---\n`, appended to the system prompt.
- **Runtime**: the `skill` tool lets the agent `list`/`load` skills mid-conversation; registry holds them in `Arc<RwLock<Vec<SkillDefinition>>>`.
