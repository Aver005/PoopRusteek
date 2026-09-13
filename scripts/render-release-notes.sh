#!/usr/bin/env bash
# Renders a `{{VAR}}` markdown template by substituting listed variables from
# the environment. Used for dev notes (ci.yml, .gitlab-ci.yml) and stable
# notes (release.yml).
#
# Usage: render-release-notes.sh <template-file> <output-file> VAR1 [VAR2 ...]
# Each VARn must already be exported (empty string if unavailable) — this
# script only substitutes, it doesn't know how to compute any of the values.
set -euo pipefail

template_file="$1"
output_file="$2"
shift 2

declare -A allowed=()
for name in "$@"; do
  allowed[$name]=1
done

# Один проход по шаблону: значения вставляются как есть — без обработки `&`, `\`
# и без повторной подстановки `{{…}}`, случайно попавших в текст коммита.
rest=$(cat "$template_file")
out=""
while [[ "$rest" == *"{{"* ]]; do
  out+="${rest%%"{{"*}"
  rest="${rest#*"{{"}"
  name="${rest%%"}}"*}"
  if [[ -n "$name" && "$rest" == *"}}"* && -n "${allowed[$name]:-}" ]]; then
    out+="${!name}"
    rest="${rest#*"}}"}"
  else
    out+="{{"
  fi
done

printf '%s\n' "$out$rest" > "$output_file"
