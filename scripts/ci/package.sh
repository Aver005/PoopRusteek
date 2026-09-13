#!/usr/bin/env bash
# Упаковывает release-бинарник цели в dist/: «сырой» файл для самообновления
# и архив для ручной загрузки. Имена — контракт с src/update/mod.rs::platform_asset.
#
#   package.sh <target>     # windows-x86_64, linux-arm64, macos-arm64, …
set -euo pipefail

target=${1:?usage: $0 <target>}
mkdir -p dist stage

case "$target" in
  windows-*)
    cp target/release/pooprusteek.exe "dist/pooprusteek-$target.exe"
    cp target/release/pooprusteek.exe LICENSE README.md stage/
    pwsh -NoProfile -Command "Compress-Archive -Path stage/* -DestinationPath dist/pooprusteek-$target.zip"
    ;;
  *)
    cp target/release/pooprusteek "dist/pooprusteek-$target"
    chmod 755 "dist/pooprusteek-$target"
    cp "dist/pooprusteek-$target" stage/pooprusteek
    cp LICENSE README.md stage/
    tar -C stage -czf "dist/pooprusteek-$target.tar.gz" pooprusteek LICENSE README.md
    ;;
esac

rm -rf stage
ls -la dist
