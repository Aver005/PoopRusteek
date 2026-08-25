#!/usr/bin/env bash
# Container entrypoint: render the config from the template plus environment,
# then run whatever was asked for.
#
# The token arrives as POOPRUSTEEK_TOKEN at run time and is written to a file
# only inside the container's own filesystem — it is never part of an image
# layer, and `docker history` never sees it.
set -euo pipefail

CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/pooprusteek"
CONFIG_FILE="$CONFIG_DIR/config.toml"
TEMPLATE="${POOPRUSTEEK_CONFIG_TEMPLATE:-/opt/sandbox/config.template.toml}"

mkdir -p "$CONFIG_DIR" "${XDG_DATA_HOME:-$HOME/.local/share}/pooprusteek"

# A config mounted in from the host wins: it lets a run be pinned to an exact
# configuration without rebuilding the image.
if [[ -n "${POOPRUSTEEK_CONFIG_OVERRIDE:-}" && -f "${POOPRUSTEEK_CONFIG_OVERRIDE}" ]]; then
  tr -d '\015' < "${POOPRUSTEEK_CONFIG_OVERRIDE}" > "$CONFIG_FILE"
else
  token="${POOPRUSTEEK_TOKEN:-}"
  if [[ -z "$token" ]]; then
    # Not fatal: mock-provider scenarios need no token at all, and failing
    # here would make the container unusable for them.
    echo "sandbox: no POOPRUSTEEK_TOKEN set — live-provider runs will fail" >&2
  fi
  # Substitute with the shell rather than sed: a token can contain / or &.
  # `tr -d` strips carriage returns first — a Windows clone with
  # core.autocrlf=true hands us a CRLF template, and the TOML parser rejects
  # it with "carriage return must be followed by newline", after which the app
  # falls back to defaults and reports the far less helpful "no provider
  # configured". `.gitattributes` pins eol=lf; this is the belt to that brace.
  template_body="$(tr -d '\015' < "$TEMPLATE")"
  printf '%s' "${template_body//__POOPRUSTEEK_TOKEN__/$token}" > "$CONFIG_FILE"
fi
chmod 600 "$CONFIG_FILE"

# Skills live in the image; point the config at them if it does not already.
if ! grep -q '/opt/sandbox/skills' "$CONFIG_FILE" 2>/dev/null; then
  # `paths = []` is the template's value; rewrite that one line in place.
  tmp="$(mktemp)"
  sed 's|^paths = \[\]$|paths = ["/opt/sandbox/skills"]|' "$CONFIG_FILE" > "$tmp"
  mv "$tmp" "$CONFIG_FILE"
  chmod 600 "$CONFIG_FILE"
fi

# Bare harness subcommands are accepted without repeating the binary name, so
# `docker run … exec "prompt"` reads the way it should.
case "${1:-}" in
  exec|scenario|suite|mine|mock-provider)
    set -- pooprusteek "$@"
    ;;
esac

exec "$@"
