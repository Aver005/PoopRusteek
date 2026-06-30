---
alwaysApply: true
scene: git_message
---

Follow the Conventional Commits standard (GFlow Lite 1.2).

**Format:** `<type>(<scope>)!: <emoji> <description>`

**Allowed types:** `feat`, `fix`, `hotfix`, `chore`, `refactor`, `release`, `docs`, `test`, `build`, `ci`, `style`, `perf`

**Scope** (optional): lowercase letters, digits, dots, hyphens — e.g. `(auth)`, `(api)`, `(ui-kit)`

**`!`** (optional): marks a breaking change

**Description:** must start with a capital letter; must be preceded by an emoji right after `: `

**Rejected patterns:** do not start with `wip`, `WIP`, `tmp`, `temp`, `asdf`, `fixup!`, `squash!`, `Merge branch`, or dots only (`.`, `..`, `...`)

**Multi-line commits:** only the first line (subject) is validated; body goes after a blank line

Examples:
- `feat: ✨ Add login page`
- `feat(auth): 🔐 Add OAuth2 flow`
- `fix(api)!: 💥 Breaking change in response format`
- `chore(deps): 📦 Bump lodash to 4.17.21`
- `docs: 📝 Update README`
- `fix: 🐛 Fix null pointer in payment service`