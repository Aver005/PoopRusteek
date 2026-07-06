#!/usr/bin/env bash
# Renders a `{{VAR}}` markdown template by substituting listed variables from
# the environment. Shared by .github/workflows/dev-release.yml and
# .gitlab-ci.yml so both pipelines produce identical dev-build release notes.
#
# Usage: render-release-notes.sh <template-file> <output-file> VAR1 [VAR2 ...]
# Each VARn must already be exported (empty string if unavailable) — this
# script only substitutes, it doesn't know how to compute any of the values.
set -euo pipefail

template_file="$1"
output_file="$2"
shift 2

text=$(cat "$template_file")
for name in "$@"; do
  token="{{${name}}}"
  value="${!name}"
  text="${text//$token/$value}"
done

printf '%s\n' "$text" > "$output_file"
