#!/usr/bin/env bash
# Собирает папку ассетов релиза: проверяет обязательные платформы, кладёт
# install.sh и пишет manifest.json + SHA256SUMS. Общий для dev- и stable-публикации.
#
#   collect-assets.sh <assets-dir> <version> <tag> <commit>
set -euo pipefail

dir=${1:?} version=${2:?} tag=${3:?} commit=${4:?}

# Без любой из платформ релиз не публикуется.
required=(
  pooprusteek-windows-x86_64.exe
  pooprusteek-windows-arm64.exe
  pooprusteek-linux-x86_64
  pooprusteek-linux-arm64
  pooprusteek-macos-arm64
  pooprusteek-setup.exe
)
missing=0
for name in "${required[@]}"; do
  if [ ! -f "$dir/$name" ]; then
    echo "::error::required release asset $name was not built"
    missing=1
  fi
done
[ "$missing" = 0 ] || exit 1

cp "$(dirname "$0")/../install.sh" "$dir/install.sh"
bash "$(dirname "$0")/make-manifest.sh" "$dir" "$version" "$tag" "$commit"
