#!/bin/sh
# Установщик Pooprusteek для macOS и Linux. Кладётся в каждый релиз:
#   curl -fsSL https://github.com/Aver005/pooprusteek/releases/latest/download/install.sh | sh
#   … | sh -s -- --dir ~/bin --channel dev
#   … | sh -s -- --uninstall
set -eu

REPO_URL="https://github.com/Aver005/pooprusteek"
BIN_NAME="pooprusteek"
PATH_MARKER="# added by pooprusteek installer"

install_dir="${POOPRUSTEEK_INSTALL_DIR:-$HOME/.local/bin}"
channel="stable"
action="install"

# ── вывод ────────────────────────────────────────────────────────────────
if [ -t 1 ]; then
  bold=$(printf '\033[1m') dim=$(printf '\033[2m') red=$(printf '\033[31m')
  green=$(printf '\033[32m') yellow=$(printf '\033[33m') reset=$(printf '\033[0m')
else
  bold='' dim='' red='' green='' yellow='' reset=''
fi
step() { printf '%s›%s %s\n' "$dim" "$reset" "$1"; }
ok() { printf '%s✓%s %s\n' "$green" "$reset" "$1"; }
warn() { printf '%s!%s %s\n' "$yellow" "$reset" "$1" >&2; }
die() { printf '%s✗%s %s\n' "$red" "$reset" "$1" >&2; exit 1; }

usage() {
  cat <<EOF
Install Pooprusteek, a terminal coding agent.

Usage: install.sh [--dir DIR] [--channel stable|dev] [--uninstall] [--help]

  --dir DIR        where to put the binary (default: ~/.local/bin,
                   or \$POOPRUSTEEK_INSTALL_DIR). /update writes here too.
  --channel NAME   stable (tagged releases, default) or dev (every develop push)
  --uninstall      remove the binary, the PATH line and, if you agree, your data
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --dir) [ "$#" -ge 2 ] || die "--dir needs a value"; install_dir=$2; dir_given=1; shift 2 ;;
    --dir=*) install_dir=${1#--dir=}; dir_given=1; shift ;;
    --channel) [ "$#" -ge 2 ] || die "--channel needs a value"; channel=$2; shift 2 ;;
    --channel=*) channel=${1#--channel=}; shift ;;
    --uninstall) action="uninstall"; shift ;;
    -h | --help) usage; exit 0 ;;
    *) die "unknown option: $1 (see --help)" ;;
  esac
done

case "$channel" in
  stable) manifest_url="$REPO_URL/releases/latest/download/manifest.json" ;;
  dev) manifest_url="$REPO_URL/releases/download/dev/manifest.json" ;;
  *) die "unknown channel '$channel' — use stable or dev" ;;
esac
case "$install_dir" in
  "~"/*) install_dir="$HOME/${install_dir#"~/"}" ;;
esac
target_path="$install_dir/$BIN_NAME"

# ── окружение ────────────────────────────────────────────────────────────
detect_target() {
  os=$(uname -s)
  arch=$(uname -m)
  case "$os" in
    Linux) os_name="linux" ;;
    Darwin) os_name="macos" ;;
    *) die "unsupported OS: $os — on Windows use pooprusteek-setup.exe from $REPO_URL/releases" ;;
  esac
  case "$arch" in
    x86_64 | amd64) arch_name="x86_64" ;;
    aarch64 | arm64) arch_name="arm64" ;;
    *) die "unsupported CPU architecture: $arch" ;;
  esac
  # Rosetta отдаёт x86_64, хотя железо — Apple Silicon.
  if [ "$os_name" = "macos" ] && [ "$arch_name" = "x86_64" ] &&
    [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = "1" ]; then
    arch_name="arm64"
  fi
  if [ "$os_name" = "macos" ] && [ "$arch_name" = "x86_64" ]; then
    die "Intel Macs aren't supported: ONNX Runtime ships no prebuilt library for them"
  fi
  printf '%s-%s' "$os_name" "$arch_name"
}

download() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 3 -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$2" "$1"
  else
    die "need curl or wget to download"
  fi
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    die "need sha256sum or shasum to verify the download"
  fi
}

# manifest.json пишет scripts/ci/make-manifest.sh: одна пара "ключ": "значение" на строку.
manifest_value() {
  sed -n "s/^[[:space:]]*\"$1\": \"\\([^\"]*\\)\".*/\\1/p" "$2" | head -n 1
}

# Спросить да/нет у терминала: при `curl | sh` stdin занят самим скриптом.
confirm() {
  [ -r /dev/tty ] || return 1
  printf '%s [y/N] ' "$1" > /dev/tty
  read -r reply < /dev/tty || return 1
  case "$reply" in y | Y | yes | YES) return 0 ;; *) return 1 ;; esac
}

shell_rc_files() {
  shell_path=${SHELL:-sh}
  case "${shell_path##*/}" in
    zsh) echo "${ZDOTDIR:-$HOME}/.zshrc" ;;
    bash) if [ "$(uname -s)" = "Darwin" ]; then echo "$HOME/.bash_profile"; else echo "$HOME/.bashrc"; fi ;;
    fish) echo "$HOME/.config/fish/config.fish" ;;
    *) echo "$HOME/.profile" ;;
  esac
}

path_line() {
  case "$1" in
    *config.fish) printf 'fish_add_path "%s" %s' "$install_dir" "$PATH_MARKER" ;;
    *) printf 'export PATH="%s:$PATH" %s' "$install_dir" "$PATH_MARKER" ;;
  esac
}

# Убрать нашу строку из rc. Пишем поверх через `cat >`, чтобы сохранить симлинк и права.
strip_path_line() {
  grep -qs "$PATH_MARKER" "$1" || return 1
  { grep -v "$PATH_MARKER" "$1" || true; } > "$1.pooprusteek-tmp"
  cat "$1.pooprusteek-tmp" > "$1"
  rm -f "$1.pooprusteek-tmp"
}

# ── установка ────────────────────────────────────────────────────────────
do_install() {
  target=$(detect_target)
  asset="$BIN_NAME-$target"
  printf '\n  %sPooprusteek%s  %s%s channel · %s%s\n\n' "$bold" "$reset" "$dim" "$channel" "$target" "$reset"

  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT INT TERM

  step "Fetching release manifest"
  download "$manifest_url" "$tmp/manifest.json" ||
    die "can't fetch the $channel release — nothing published on this channel yet, or no network"
  version=$(manifest_value version "$tmp/manifest.json")
  tag=$(manifest_value tag "$tmp/manifest.json")
  expected=$(manifest_value "$asset" "$tmp/manifest.json")
  [ -n "$version" ] && [ -n "$tag" ] || die "release manifest is malformed"
  [ -n "$expected" ] || die "the $channel release has no build for $target"

  step "Downloading $version"
  download "$REPO_URL/releases/download/$tag/$asset" "$tmp/$BIN_NAME" || die "download failed"
  actual=$(sha256_of "$tmp/$BIN_NAME")
  [ "$actual" = "$expected" ] || die "checksum mismatch — the download is corrupt, try again"
  ok "Verified checksum"

  mkdir -p "$install_dir" || die "can't create $install_dir"
  cp "$tmp/$BIN_NAME" "$target_path.new" || die "can't write to $install_dir"
  chmod 755 "$target_path.new"
  # Проверяем копию в папке установки (не в /tmp — он бывает noexec) и до замены:
  # несовместимый бинарник не должен затереть рабочий.
  if ! "$target_path.new" --version >/dev/null 2>&1; then
    rm -f "$target_path.new"
    warn "the downloaded binary doesn't start on this system — nothing was changed."
    if [ "$(uname -s)" = "Linux" ]; then
      warn "Linux builds need glibc 2.39+ (Ubuntu 24.04, Debian 13 or newer)."
    fi
    exit 1
  fi
  # rename — атомарно, даже поверх запущенного бинарника.
  mv -f "$target_path.new" "$target_path"
  ok "Installed to $target_path"

  case ":$PATH:" in
    *":$install_dir:"*) ;;
    *)
      rc=$(shell_rc_files)
      line=$(path_line "$rc")
      if ! grep -qsF "$line" "$rc"; then
        # Прежняя строка могла указывать на другую папку — заменяем, а не копим.
        strip_path_line "$rc" || true
        mkdir -p "$(dirname "$rc")"
        # Перевод строки дописываем, только если файла нет в конце — пустые строки не копятся.
        if [ -s "$rc" ] && [ -n "$(tail -c 1 "$rc")" ]; then
          printf '\n' >> "$rc"
        fi
        printf '%s\n' "$line" >> "$rc"
      fi
      ok "Added $install_dir to PATH in $rc"
      restart_hint=1
      ;;
  esac

  printf '\n  %sDone!%s Run %s%s%s to start.\n' "$green$bold" "$reset" "$bold" "$BIN_NAME" "$reset"
  if [ "${restart_hint:-0}" = 1 ]; then
    printf '  %sOpen a new terminal first so PATH picks it up.%s\n' "$dim" "$reset"
  fi
  if [ "$channel" = "dev" ]; then
    printf '  %sInside the app, run /update channel dev to keep updates on this channel.%s\n' "$dim" "$reset"
  fi
  printf '\n'
}

# ── удаление ─────────────────────────────────────────────────────────────
data_dirs() {
  if [ "$(uname -s)" = "Darwin" ]; then
    echo "$HOME/Library/Application Support/pooprusteek"
  else
    echo "${XDG_CONFIG_HOME:-$HOME/.config}/pooprusteek"
    echo "${XDG_DATA_HOME:-$HOME/.local/share}/pooprusteek"
  fi
}

do_uninstall() {
  printf '\n  %sUninstalling Pooprusteek%s\n\n' "$bold" "$reset"
  # Папка не задана и по умолчанию пусто — ищем через PATH, но удаляем только с согласия:
  # это может быть чужая копия (например, из cargo install).
  if [ ! -e "$target_path" ] && [ "${dir_given:-0}" = 0 ] && [ -z "${POOPRUSTEEK_INSTALL_DIR:-}" ]; then
    found=$(command -v "$BIN_NAME" 2>/dev/null || true)
    if [ -n "$found" ] && confirm "Remove $found?"; then
      target_path=$found
    fi
  fi
  if [ -e "$target_path" ]; then
    rm -f "$target_path"
    ok "Removed $target_path"
  else
    warn "no binary at $target_path (pass --dir if you installed elsewhere)"
  fi
  rm -f "$target_path.new" "$target_path.old"

  for rc in "$HOME/.profile" "$HOME/.bashrc" "$HOME/.bash_profile" \
    "${ZDOTDIR:-$HOME}/.zshrc" "$HOME/.config/fish/config.fish"; do
    if strip_path_line "$rc"; then
      ok "Removed the PATH line from $rc"
    fi
  done

  data_dirs | while IFS= read -r dir; do
    [ -d "$dir" ] || continue
    if confirm "Also delete $dir (settings, sessions, downloaded model)?"; then
      rm -rf "$dir"
      ok "Deleted $dir"
    else
      printf '%s  kept %s%s\n' "$dim" "$dir" "$reset"
    fi
  done
  printf '\n'
}

case "$action" in
  install) do_install ;;
  uninstall) do_uninstall ;;
esac
