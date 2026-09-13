#!/usr/bin/env bash
# Пишет manifest.json и SHA256SUMS для папки ассетов релиза.
# Формат manifest.json читает src/update/manifest.rs и scripts/install.sh — меняются вместе.
#
#   make-manifest.sh <assets-dir> <version> <tag> <commit>
set -euo pipefail

if [ "$#" -ne 4 ]; then
  echo "usage: $0 <assets-dir> <version> <tag> <commit>" >&2
  exit 2
fi
dir=$1 version=$2 tag=$3 commit=$4

cd "$dir"
shopt -s nullglob

# «Сырые» бинарники — то, что качает самообновление: pooprusteek-<target>[.exe].
binaries=()
for file in pooprusteek-*; do
  case "$file" in
    *.zip | *.tar.gz | *-setup.exe) ;;
    *) binaries+=("$file") ;;
  esac
done
if [ "${#binaries[@]}" -eq 0 ]; then
  echo "no raw binaries in $dir" >&2
  exit 1
fi

# Чексуммы по всем файлам релиза — для ручной проверки скачанного.
files=()
for file in *; do
  case "$file" in
    SHA256SUMS | manifest.json) ;;
    *) [ -f "$file" ] && files+=("$file") ;;
  esac
done
sha256sum "${files[@]}" > SHA256SUMS

# Один ассет на строку: install.sh разбирает манифест без jq.
{
  printf '{\n'
  printf '  "schema": 1,\n'
  printf '  "version": "%s",\n' "$version"
  printf '  "tag": "%s",\n' "$tag"
  printf '  "commit": "%s",\n' "$commit"
  printf '  "assets": {\n'
  last=$((${#binaries[@]} - 1))
  for i in "${!binaries[@]}"; do
    name=${binaries[$i]}
    hash=$(sha256sum "$name" | cut -d' ' -f1)
    sep=','
    [ "$i" -eq "$last" ] && sep=''
    printf '    "%s": "%s"%s\n' "$name" "$hash" "$sep"
  done
  printf '  }\n'
  printf '}\n'
} > manifest.json

if command -v jq >/dev/null; then
  jq empty manifest.json
fi
cat manifest.json
