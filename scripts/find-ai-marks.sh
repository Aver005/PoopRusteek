#!/usr/bin/env bash
# Ищет в последних N коммитах со-авторство и прочие следы ИИ-агентов:
# трейлеры Co-authored-by / Generated-by, адреса ИИ-сервисов, фразы о
# генерации, эмодзи-робота и ИИ-имена в подписи автора или коммитера.
#
# Usage: scripts/find-ai-marks.sh [N]      (N по умолчанию 20)
# Коды возврата: 0 — чисто, 1 — что-то найдено, 2 — ошибка запуска.
set -euo pipefail
shopt -s nocasematch

case "${1:-}" in
  -h | --help)
    sed -n '2,7p' "$0"
    exit 0
    ;;
esac

count=${1:-20}
if ! [[ $count =~ ^[0-9]+$ ]] || [[ $count -eq 0 ]]; then
  echo "N должно быть целым числом больше нуля, получено: $count" >&2
  exit 2
fi

if ! git rev-parse --git-dir > /dev/null 2>&1; then
  echo "Не репозиторий git: $PWD" >&2
  exit 2
fi

labels=(
  'со-авторство'
  'трейлер генерации'
  'адрес ИИ-сервиса'
  'фраза о генерации'
  'эмодзи-робот'
  'ИИ в подписи'
)
patterns=(
  '^co-authored-by:'
  '^(generated-by|assisted-by|created-by|ai-assisted-by|x-generated-by):'
  '(claude\.(ai|com)|anthropic\.com|openai\.com|chat\.openai|cursor\.(com|sh)|codeium\.com|aider\.chat|copilot@|devin-ai-integration|\[bot\]@)'
  '(generated (with|by)|co-?authored with|written (with|by)|assisted by)[^[:alnum:]]*(claude|copilot|chatgpt|gpt-[0-9]|cursor|codex|gemini|devin|aider|нейросет|ии\b|ai\b)'
  '🤖'
  '^(author|committer): .*(claude|copilot|chatgpt|codex|gemini|devin|aider|\[bot\])'
)

marker='@@@pooprusteek-commit@@@'
found=0
scanned=0
cur_short=''
cur_subject=''
cur_lines=()

# Печатает один коммит, если в его строках нашлась хоть одна метка.
report() {
  if [[ -z $cur_short ]]; then
    return 0
  fi
  scanned=$((scanned + 1))
  local hits=() line joined i
  for line in "${cur_lines[@]}"; do
    joined=''
    for i in "${!patterns[@]}"; do
      if [[ $line =~ ${patterns[$i]} ]]; then
        joined+="${joined:+, }${labels[$i]}"
      fi
    done
    if [[ -n $joined ]]; then
      hits+=("    [$joined] $line")
    fi
  done
  if [[ ${#hits[@]} -gt 0 ]]; then
    found=$((found + 1))
    printf '%s %s\n' "$cur_short" "$cur_subject"
    printf '%s\n' "${hits[@]}"
    printf '\n'
  fi
}

field=-1
while IFS= read -r line; do
  if [[ $line == "$marker" ]]; then
    report
    cur_short=''
    cur_subject=''
    cur_lines=()
    field=0
    continue
  fi
  case $field in
    0)
      cur_short=$line
      field=1
      ;;
    1)
      cur_subject=$line
      cur_lines+=("$line")
      field=2
      ;;
    *)
      if [[ -n $line ]]; then
        cur_lines+=("$line")
      fi
      ;;
  esac
done < <(git log -n "$count" \
  --format="${marker}%n%h%n%s%nauthor: %an <%ae>%ncommitter: %cn <%ce>%n%b")
report

if [[ $found -eq 0 ]]; then
  printf 'Проверено коммитов: %d — меток ИИ не найдено.\n' "$scanned"
  exit 0
fi

printf 'Проверено коммитов: %d, с метками ИИ: %d.\n' "$scanned" "$found"
exit 1
