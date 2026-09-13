<!--
  stable-release.template.md — заметки стабильного релиза, рендерит
  scripts/render-release-notes.sh из .github/workflows/release.yml.
  Плейсхолдеры: {{VERSION}} {{TAG}} {{REPO_URL}} {{CHANGES}} {{PREVIOUS_TAG}}
-->
## Install

**Windows** — download **`pooprusteek-setup.exe`** below and click through.
SmartScreen may warn about an unrecognized app (the installer isn't code-signed yet): *More info → Run anyway*.

**macOS (Apple Silicon) / Linux**

```sh
curl -fsSL {{REPO_URL}}/releases/latest/download/install.sh | sh
```

Already installed? Run `/update` inside the app.

## What's changed

{{CHANGES}}

<sub>Full diff: [{{PREVIOUS_TAG}}…{{TAG}}]({{REPO_URL}}/compare/{{PREVIOUS_TAG}}...{{TAG}}) · checksums in `SHA256SUMS`</sub>
