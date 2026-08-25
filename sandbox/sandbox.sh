#!/usr/bin/env bash
# Control CLI for the pooprusteek harness sandbox — the bash counterpart of
# sandbox.ps1, for WSL, Linux, macOS and CI. Same commands, same output paths.
#
#   ./sandbox.sh build [dev|release]
#   ./sandbox.sh doctor
#   ./sandbox.sh shell
#   ./sandbox.sh exec "prompt" [extra harness args...]
#   ./sandbox.sh scenario <name|path> [repeat] [extra...]
#   ./sandbox.sh suite [live|mock|dev|all] [repeat]
#   ./sandbox.sh mine [--sessions]
#   ./sandbox.sh mock [script]
#   ./sandbox.sh stop | report | reset
set -euo pipefail

# Git Bash / MSYS rewrites any argument that looks like a POSIX path into a
# Windows one, so `/out/x.jsonl` reached the container as
# `C:/Program Files/Git/out/x.jsonl`. Only the *container's* paths must be
# spared — host paths (the compose file, `--env-file`) still need converting,
# so this excludes the two container prefixes rather than disabling
# translation wholesale. No-op on real Unix.
export MSYS2_ARG_CONV_EXCL='/out;/opt'

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
compose_file="$here/docker-compose.yml"
out_dir="$here/out"
env_file="$here/.env"

compose() {
  local args=(compose -f "$compose_file")
  [[ -f "$env_file" ]] && args+=(--env-file "$env_file")
  docker "${args[@]}" "$@"
}

harness() {
  mkdir -p "$out_dir"
  compose run --rm sandbox "$@"
}

stamp() { date -u +'%Y-%m-%dT%H-%M-%S-000Z'; }

# Bare name or path -> the container's view of sandbox/scenarios.
resolve_scenario() {
  local name="$1"
  case "$name" in
    /opt/sandbox/*) printf '%s' "$name"; return ;;
  esac
  local matches
  mapfile -t matches < <(find "$here/scenarios" -name '*.toml' \
    \( -name "$name" -o -name "$name.toml" \) | sort)
  if [[ ${#matches[@]} -eq 1 ]]; then
    printf '/opt/sandbox/%s' "${matches[0]#"$here/"}"
    return
  fi
  if [[ ${#matches[@]} -gt 1 ]]; then
    echo "Ambiguous scenario '$name':" >&2
    printf '  %s\n' "${matches[@]}" >&2
    exit 2
  fi
  printf '/opt/sandbox/%s' "${name#./}"
}

command="${1:-}"
shift || true

case "$command" in
  build)
    profile="${1:-release}"
    echo "Building sandbox image (profile: $profile)..."
    BUILD_PROFILE="$profile" compose build sandbox
    echo "Image ready: pooprusteek-sandbox:latest"
    ;;

  doctor)
    echo "── sandbox doctor ──"
    if ! docker version --format '{{.Server.Version}}' 2>/dev/null; then
      echo "docker engine : NOT REACHABLE — start Docker" >&2
      exit 1
    fi
    if [[ -n "$(docker images -q pooprusteek-sandbox:latest)" ]]; then
      echo "image         : present"
    else
      echo "image         : missing — run: ./sandbox.sh build"
    fi
    if [[ -f "$env_file" ]] && grep -q 'POOPRUSTEEK_TOKEN=..' "$env_file"; then
      echo "token         : set in sandbox/.env"
    else
      echo "token         : not set (mock runs still work)"
    fi
    echo "scenarios     : $(find "$here/scenarios" -name '*.toml' | wc -l | tr -d ' ')"
    [[ -d "$out_dir" ]] && \
      echo "reports in out: $(find "$out_dir" -name 'report.json' 2>/dev/null | wc -l | tr -d ' ')"
    ;;

  shell)
    compose run --rm -it sandbox bash
    ;;

  exec)
    [[ $# -ge 1 ]] || { echo "exec needs a prompt" >&2; exit 2; }
    prompt="$1"; shift
    harness exec "$prompt" --trace "/out/exec-$(stamp).jsonl" "$@"
    ;;

  scenario)
    [[ $# -ge 1 ]] || { echo "scenario needs a name or path" >&2; exit 2; }
    path="$(resolve_scenario "$1")"; shift
    repeat="${1:-3}"; [[ $# -ge 1 ]] && shift || true
    harness scenario "$path" --repeat "$repeat" --out /out "$@"
    ;;

  suite)
    which="${1:-live}"; [[ $# -ge 1 ]] && shift || true
    case "$which" in
      live) dir=/opt/sandbox/scenarios/live ;;
      mock) dir=/opt/sandbox/scenarios/mock ;;
      dev)  dir=/opt/sandbox/scenarios/dev ;;
      all)  dir=/opt/sandbox/scenarios ;;
      *) echo "suite takes live | mock | dev | all, got '$which'" >&2; exit 2 ;;
    esac
    [[ "$which" != "live" ]] && \
      echo "Note: mock scenarios need the mock service — ./sandbox.sh mock <script>" >&2
    repeat="${1:-3}"; [[ $# -ge 1 ]] && shift || true
    harness suite "$dir" --repeat "$repeat" --out /out "$@"
    ;;

  mine)
    harness mine /out "$@"
    ;;

  mock)
    if [[ $# -lt 1 ]]; then
      echo "Available scripts:"
      find "$here/mock-scripts" -name '*.toml' -exec basename {} .toml \; | sed 's/^/  /'
      exit 0
    fi
    script="$1"
    [[ "$script" == *.toml ]] || script="$script.toml"
    echo "Starting mock provider with $script..."
    MOCK_SCRIPT="$script" compose up -d mock
    ;;

  stop)
    compose down --remove-orphans
    ;;

  report)
    [[ -d "$out_dir" ]] || { echo "No runs yet."; exit 0; }
    if command -v jq >/dev/null 2>&1; then
      find "$out_dir" -name '*.json' -newermt '-30 days' 2>/dev/null | sort | while read -r file; do
        jq -r 'if .scenarios then
                 "[\(if .passed then "PASS" else "FAIL" end)] suite  \(.passed_scenarios)/\(.total)"
               elif .pass_rate then
                 "[\(if .passed then "PASS" else "FAIL" end)] \(.name)  \(.passed_runs)/\(.repeats)"
               else empty end' "$file" 2>/dev/null || true
      done
    else
      find "$out_dir" -name 'report.json' | sed 's/^/  /'
    fi
    echo
    echo "Full reports under $out_dir"
    ;;

  reset)
    echo "This removes the data volume (embedding model, sessions, index) and sandbox/out."
    read -r -p 'Type "yes" to continue: ' answer
    [[ "$answer" == "yes" ]] || { echo "Cancelled."; exit 0; }
    compose down -v --remove-orphans
    rm -rf "$out_dir"
    echo "Reset done. Next run re-downloads the embedding model."
    ;;

  *)
    sed -n '2,15p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
