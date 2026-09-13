#!/usr/bin/env bash
# Печатает раздел CHANGELOG.md для версии — тело `## [X.Y.Z] …` до следующего `## [`.
# Общий для scripts/release.sh (проверка перед релизом) и release.yml (заметки релиза).
#
#   changelog-section.sh <version> [changelog-path]
set -euo pipefail

version=${1:?usage: $0 <version> [changelog-path]}
changelog=${2:-CHANGELOG.md}

# Пустые строки по краям раздела срезаются; только awk — одинаково на GNU и BSD.
awk -v heading="## [$version]" '
  index($0, heading) == 1 { found = 1; next }
  found && (/^## \[/ || /^\[[^]]+\]: /) { exit }
  found {
    if ($0 ~ /^[[:space:]]*$/) { if (printed) blank++ ; next }
    while (blank > 0) { print ""; blank-- }
    print
    printed = 1
  }
' "$changelog"
