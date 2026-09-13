#!/usr/bin/env bash
# Упаковывает release-бинарник цели в dist/: «сырой» файл для самообновления
# и архив для ручной загрузки. Имена — контракт с src/update/mod.rs::platform_asset.
#
#   package.sh <target>     # windows-x86_64, linux-arm64, macos-arm64, …
set -euo pipefail

target=${1:?usage: $0 <target>}
mkdir -p dist stage

# Документы кладём в архив, только если они есть: их отсутствие не повод ронять сборку.
docs=()
for doc in LICENSE README.md; do
  [ -f "$doc" ] && docs+=("$doc")
done

case "$target" in
  windows-*)
    cp target/release/pooprusteek.exe "dist/pooprusteek-$target.exe"
    cp target/release/pooprusteek.exe stage/
    ;;
  *)
    cp target/release/pooprusteek "dist/pooprusteek-$target"
    chmod 755 "dist/pooprusteek-$target"
    cp "dist/pooprusteek-$target" stage/pooprusteek
    ;;
esac
[ "${#docs[@]}" -eq 0 ] || cp "${docs[@]}" stage/

case "$target" in
  windows-*) pwsh -NoProfile -Command "Compress-Archive -Path stage/* -DestinationPath dist/pooprusteek-$target.zip" ;;
  *) tar -C stage -czf "dist/pooprusteek-$target.tar.gz" . ;;
esac

rm -rf stage
ls -la dist
