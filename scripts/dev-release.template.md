<!--
  dev-release.template.md — release notes template for the rolling
  "dev-build" release, rendered by scripts/render-release-notes.sh from
  both .github/workflows/dev-release.yml and .gitlab-ci.yml.

  Available placeholders (empty string if a pipeline can't compute one):
    {{TAG}}                          release tag, e.g. v0.1.0-dev
    {{VERSION}}                      version from Cargo.toml, e.g. 0.1.0
    {{PREVIOUS_TAG}}                 tag/commit this release replaces, or
                                      "(none — first dev build)"
    {{BRANCH}}                       source branch (develop)
    {{REPO}}                         owner/repo or namespace/project
    {{REPO_URL}}                     web URL of the repo
    {{COMMIT_SHA}}                   full commit hash
    {{COMMIT_SHORT_SHA}}             short commit hash
    {{COMMIT_TITLE}}                 first line of the commit message
    {{COMMIT_MESSAGE}}                full commit message
    {{COMMIT_AUTHOR_NAME}}
    {{COMMIT_AUTHOR_EMAIL}}
    {{COMMIT_TIMESTAMP}}             commit author date, ISO 8601
    {{PUSHED_BY}}                    who triggered the pipeline
    {{BUILD_DATE}}                   UTC time the release job ran
    {{RUN_ID}}                       CI run/pipeline id
    {{RUN_URL}}                      link to the CI run/pipeline
    {{RUST_VERSION}}                 `rustc --version` used for the build
    {{COMMIT_COUNT_SINCE_LAST_BUILD}} commits since the previous dev build
    {{COMMITS_SINCE_LAST_BUILD}}     bullet list of those commits
    {{CONTRIBUTORS_SINCE_LAST_BUILD}} unique author names in that range
    {{CHANGED_FILES_COUNT}}          files touched since the previous dev build
    {{ARTIFACT_COUNT}}               number of built artifacts
    {{ARTIFACT_LIST}}                bullet list of artifact names + sizes

  Add a new placeholder here first, then wire it up in both pipelines and
  scripts/render-release-notes.sh's argument list.
-->
## 🚧 Dev build `{{TAG}}`

> {{COMMIT_TITLE}}

Rolling preview built from `{{BRANCH}}` — always the latest commit that passed CI.
**This is not a stable release.**

`{{VERSION}}`  ·  [`{{COMMIT_SHORT_SHA}}`]({{REPO_URL}}/commit/{{COMMIT_SHA}})  ·  built {{BUILD_DATE}}  ·  [CI run #{{RUN_ID}}]({{RUN_URL}})

### 📦 Downloads

{{ARTIFACT_LIST}}

### 📝 Changes since `{{PREVIOUS_TAG}}`

{{COMMITS_SINCE_LAST_BUILD}}

<sub>{{COMMIT_COUNT_SINCE_LAST_BUILD}} commit(s) · {{CHANGED_FILES_COUNT}} file(s) changed · {{CONTRIBUTORS_SINCE_LAST_BUILD}}</sub>
