---
alwaysApply: true
scene: git_message
---

Follow the Conventional Commits standard (GFlow Lite 1.2).

**Format:** `<type>(<scope>)!: <emoji> <description>`

**Allowed types:** `feat`, `fix`, `hotfix`, `chore`, `refactor`, `release`, `docs`, `test`, `build`, `ci`, `style`, `perf`

**Scope** (optional): either exactly one scope, or no scope at all — never multiple/comma-separated scopes. Lowercase letters, digits, dots, hyphens — e.g. `(auth)`, `(api)`, `(ui-kit)`

**`!`** (optional): marks a breaking change

**Description:** must start with a capital letter; must be preceded by an emoji right after `: `

**Rejected patterns:** do not start with `wip`, `WIP`, `tmp`, `temp`, `asdf`, `fixup!`, `squash!`, `Merge branch`, or dots only (`.`, `..`, `...`)

**Multi-line commits:** only the first line (subject) is validated per the rules above. The body, after a blank line, must follow the Keep a Changelog format:
- Split into one or more sections, each headed by `### <Category>`, where `<Category>` is one of: `Added`, `Changed`, `Deprecated`, `Removed`, `Fixed`, `Security`
- Each section is a bullet list (`- ...`), one entry per change, written for humans (clear, concise, no internal jargon or ticket-only descriptions)
- Only include sections that actually have entries — no empty headings
- Order of sections doesn't matter, but don't repeat a category twice

Examples:
- `feat: ✨ Add login page`
- `feat(auth): 🔐 Add OAuth2 flow`
- `fix(api)!: 💥 Breaking change in response format`
- `chore(deps): 📦 Bump lodash to 4.17.21`
- `docs: 📝 Update README`
- `fix: 🐛 Fix null pointer in payment service`

- `feat(payments): ✨ Add refunds`

  `### Added`
  `- Support for partial refunds via API`
  `- Refund history in the admin panel`

  `### Fixed`
  `- Incorrect currency conversion on refund amount`