#!/usr/bin/env bash
# Выпуск стабильного релиза одной командой: версия, CHANGELOG, коммит, тег, пуш.
# Сборку и публикацию делает .github/workflows/release.yml по пушу тега v*.
#
#   scripts/release.sh patch|minor|major|X.Y.Z [--dry-run] [--yes]
set -euo pipefail

BRANCH="develop"
REMOTE="origin"

bold=$'\033[1m' dim=$'\033[2m' red=$'\033[31m' green=$'\033[32m' reset=$'\033[0m'
[ -t 1 ] || { bold='' dim='' red='' green='' reset=''; }
step() { printf '%s›%s %s\n' "$dim" "$reset" "$1"; }
ok() { printf '%s✓%s %s\n' "$green" "$reset" "$1"; }
die() { printf '%s✗%s %s\n' "$red" "$reset" "$1" >&2; exit 1; }

usage() {
  cat <<EOF
Usage: scripts/release.sh <patch|minor|major|X.Y.Z> [--dry-run] [--yes]

Bumps Cargo.toml, turns CHANGELOG's [Unreleased] into the new version,
commits, tags vX.Y.Z and pushes both to $REMOTE — CI builds and publishes.

  --dry-run   check everything and show the plan, change nothing
  --yes       don't ask for confirmation before pushing
EOF
}

bump="" dry_run=0 assume_yes=0
for arg in "$@"; do
  case "$arg" in
    --dry-run) dry_run=1 ;;
    --yes | -y) assume_yes=1 ;;
    -h | --help) usage; exit 0 ;;
    -*) die "unknown option: $arg" ;;
    *) [ -z "$bump" ] || die "only one version argument"; bump=$arg ;;
  esac
done
[ -n "$bump" ] || { usage; exit 2; }

cd "$(git rev-parse --show-toplevel)"

# ── версии ───────────────────────────────────────────────────────────────
current=$(awk -F'"' '/^\[package\]/ { pkg = 1; next } /^\[/ { pkg = 0 } pkg && /^version *=/ { print $2; exit }' Cargo.toml)
[[ "$current" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]] || die "Cargo.toml version '$current' isn't X.Y.Z"
major=${BASH_REMATCH[1]} minor=${BASH_REMATCH[2]} patch=${BASH_REMATCH[3]}

case "$bump" in
  patch) next="$major.$minor.$((patch + 1))" ;;
  minor) next="$major.$((minor + 1)).0" ;;
  major) next="$((major + 1)).0.0" ;;
  *)
    [[ "$bump" =~ ^v?([0-9]+\.[0-9]+\.[0-9]+)$ ]] || die "version must be patch, minor, major or X.Y.Z"
    next=${BASH_REMATCH[1]}
    ;;
esac
tag="v$next"

# sort -V знает семвер-порядок; следующая версия должна быть строго больше.
newest=$(printf '%s\n%s\n' "$current" "$next" | sort -V | tail -n 1)
[ "$next" != "$current" ] && [ "$newest" = "$next" ] || die "$next isn't newer than $current"

# ── проверки состояния ───────────────────────────────────────────────────
step "Checking the repository"
[ "$(git branch --show-current)" = "$BRANCH" ] || die "switch to $BRANCH first"
[ -z "$(git status --porcelain)" ] || die "working tree isn't clean — commit or stash first"

# Без --tags: CI переносит теги `dev` и `latest`, и fetch отказался бы их перезаписывать.
git fetch --quiet "$REMOTE" "$BRANCH"
[ "$(git rev-list --count "HEAD..$REMOTE/$BRANCH")" = 0 ] || die "$BRANCH is behind $REMOTE — pull first"
git rev-parse -q --verify "refs/tags/$tag" >/dev/null && die "tag $tag already exists locally"
git ls-remote --exit-code --tags "$REMOTE" "refs/tags/$tag" >/dev/null && die "tag $tag already exists on $REMOTE"

notes=$(bash scripts/ci/changelog-section.sh Unreleased)
[ -n "$notes" ] || die "CHANGELOG.md has nothing under [Unreleased] — describe the release first"
ok "On $BRANCH, clean, up to date"

printf '\n  %sRelease %s → %s%s  (%s)\n\n' "$bold" "$current" "$next" "$reset" "$tag"
printf '%s\n' "$notes" | head -n 25 | sed "s/^/  $dim│$reset /"
[ "$(printf '%s\n' "$notes" | wc -l)" -le 25 ] || printf '  %s│ …%s\n' "$dim" "$reset"
printf '\n'

if [ "$dry_run" = 1 ]; then
  ok "Dry run — nothing changed"
  exit 0
fi
if [ "$assume_yes" = 0 ]; then
  read -r -p "Bump, commit, tag and push $tag? [y/N] " reply
  [[ "$reply" =~ ^[yY] ]] || die "aborted"
fi

# ── изменения ────────────────────────────────────────────────────────────
# Cargo.lock бампаем вместе с версией, если он под контролем git.
release_files=(Cargo.toml CHANGELOG.md)
git ls-files --error-unmatch Cargo.lock >/dev/null 2>&1 && release_files+=(Cargo.lock)

restore() {
  local file
  for file in "${release_files[@]}"; do
    git checkout --quiet HEAD -- "$file" || true
  done
}
trap 'restore; die "failed — changes rolled back"' ERR

step "Bumping the version"
awk -v next_version="$next" '
  /^\[package\]/ { pkg = 1 } /^\[/ && !/^\[package\]/ { pkg = 0 }
  pkg && !done && /^version *=/ { print "version = \"" next_version "\""; done = 1; next }
  { print }
' Cargo.toml > Cargo.toml.tmp && mv Cargo.toml.tmp Cargo.toml
if [[ " ${release_files[*]} " == *" Cargo.lock "* ]]; then
  cargo update --workspace --offline --quiet
fi

step "Closing the CHANGELOG section"
awk -v heading="## [$next] - $(date +%Y-%m-%d)" '
  !done && /^## \[Unreleased\]/ { print; print ""; print heading; done = 1; next }
  { print }
' CHANGELOG.md > CHANGELOG.md.tmp && mv CHANGELOG.md.tmp CHANGELOG.md

# Pre-commit hook прогоняет fmt, clippy и тесты; без него — проверяем сами.
if [ "$(git config core.hooksPath || true)" != ".githooks" ]; then
  step "Running checks (fmt, clippy, tests)"
  cargo fmt --all -- --check
  cargo clippy --bin pooprusteek -- -D warnings
  cargo test --bin pooprusteek
fi

step "Committing and tagging"
git add "${release_files[@]}"
git commit --quiet -m "chore(release): 🔖 Release $tag"
trap - ERR
git tag -a "$tag" -m "Pooprusteek $next" ||
  die "tagging failed after the release commit — fix it, then: git tag -a $tag -m 'Pooprusteek $next' && git push --atomic $REMOTE $BRANCH $tag"

step "Pushing $BRANCH and $tag"
if ! git push --atomic "$REMOTE" "$BRANCH" "$tag"; then
  cat >&2 <<EOF
${red}✗${reset} push failed — the release commit and tag exist only locally.
  If it was a network hiccup, retry:
    git push --atomic $REMOTE $BRANCH $tag
  If $BRANCH moved on $REMOTE, redo the release on top of it:
    git tag -d $tag && git reset --hard HEAD~1 && git pull && scripts/release.sh $bump
EOF
  exit 1
fi

repo=$(git remote get-url "$REMOTE" | sed -E 's#(git@github.com:|https://github.com/)##; s#\.git$##')
ok "Released $tag"
printf '\n  Build: https://github.com/%s/actions\n  Release (once CI finishes): https://github.com/%s/releases/tag/%s\n\n' "$repo" "$repo" "$tag"
