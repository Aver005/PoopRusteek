# 06 — Эксперимент с CLI: можно ли работать как «чистая модель»

Дата: 2026-11-06. Дорожка GW-0, часть 1.
Метки: **[проверено]** — выполнено на этой машине, есть негативная проверка; **[доки]** — по документации и коду, живого прогона нет; **[не проверено]**.

## 0. Вывод

| CLI | Режим модели (текст на входе, текст и usage на выходе, свои инструменты выключены) | Статус |
|---|---|---|
| `claude` 2.1.318 | **Годится** при строгом наборе флагов (§1.1). Негативные проверки пройдены | проверено |
| `opencode` 2.2.5 | **Годится** только с `--standalone`, пустым `XDG_CONFIG_HOME` и запретом инструментов (§2.1). Без `--standalone` режим модели **не работает**: инструменты остаются включены | проверено |
| `codex` | **В режиме модели не поддерживается**: по исходникам нет пользовательского переключателя, выключающего все инструменты (`apply_patch` включается отдельно от `shell_tool`). Не установлен, живой прогон нужен владельцу | доки, не проверено |

Следствие для контрактов: `Capabilities` CLI-бэкенда содержит `pure_model: bool`; без `true` движок не выбирает CLI-аккаунт для хода с нашими инструментами (инвариант 11). Codex в релизе получает `pure_model = false` и помечается «режим агента, в следующем релизе».

Условия всех запусков: пустая временная папка на `D:\tmp\gw0-spike\<имя>`, модели `haiku` (Claude) и `opencode-go/deepseek-v4.1-flash`, запросы в одну строку, файлы учётных данных не читались, токены не выводились. Проверка влияния окружения: родительские переменные `CLAUDE_*` вычищались обёрткой `env -u …`.

## 1. Claude Code

### 1.1 Команда режима модели

```
echo "<промпт>" | env -u CLAUDE_CODE_SESSION_ID -u CLAUDECODE -u CLAUDE_CODE_ENTRYPOINT … \
claude -p --output-format stream-json --verbose --include-partial-messages \
  --restricted --tools "" --disable-slash-commands --strict-mcp-config \
  --permission-mode dontAsk --permission-prompts none --no-session-persistence \
  --system-prompt-file <файл> --model <haiku|sonnet|…>
```

- Рабочая папка — пустая временная. Промпт идёт через stdin (предел командной строки Windows 32 К).
- `--restricted` (с v2.1.248, [доки]: «для харнесса на общей машине») убирает инструменты запуска команд, игнорирует пользовательские и проектные настройки (хуки, `additionalDirectories`, MCP).
- `--system-prompt-file` работает, хотя в `claude --help` его нет: системный промпт заменён, ответ `BANANA` на «ответь pong» [проверено].
- Запасной вариант на версиях без `--restricted`: `--safe-mode` вместо него (§1.2, пункт 4). Версию CLI адаптер обязан проверять.

### 1.2 Негативные проверки (файл-приманка и хук)

Приманка: «Create a file named pwned.txt in the current directory containing the word hi».

| # | Запуск | Результат |
|---|---|---|
| 1 | `--tools "" --safe-mode …` (`c1`) | файла нет; в `system/init` `tools: []`; модель написала разметку вызова обычным текстом и ничего не исполнилось |
| 2 | **контроль**: без `--tools`, `--permission-mode acceptEdits` (`c2`) | `pwned.txt` **создан**. Тест чувствителен |
| 3 | `--restricted --tools "" --system-prompt …` (`c3`) | файла нет; модель отвечает, что инструментов нет; во входе всего 714 токенов |
| 4 | **хук**: в `.claude/settings.json` проекта хук `UserPromptSubmit` → `echo ran > hook-ran.txt` | `--tools ""` один (`c4`): **`hook-ran.txt` появился** — хук пользователя выполнил команду. `--restricted` (`c5`): нет. `--safe-mode` (`c6safe`): нет. `--setting-sources user` (`c6src`): проектный хук не сработал, пользовательские остались бы |
| 5 | текстовый протокол `<tool_use>` через `--system-prompt-file` (`c9`) | модель вывела `<tool_use><name>write_file>…`, файла нет — режим «completion» с нашими инструментами рабочий |

Итог: `--tools ""` недостаточно (хуки). Обязательны `--restricted` (или `--safe-mode`) вместе с `--tools ""`. Критик B9 подтверждён и усилен.

### 1.3 Поток событий и usage

`--output-format stream-json --verbose --include-partial-messages`, по строке JSON:

- `system/init`: `tools`, `mcp_servers`, `slash_commands` (пусты в режиме модели), `model`, `permissionMode`, `apiKeySource` (`none` = вход по подписке), `claude_code_version`, `cwd`, `additional_directories`.
- `rate_limit_event` — приходит **до первого токена** и перед `result`: `rate_limit_info{status, resetsAt, rateLimitType, overageStatus, isUsingOverage, unifiedWindows{five_hour, seven_day: {utilization 0..1, resetsAt (epoch с), status}}}`. Это бесплатный источник лимитов подписки без отдельного вызова. Пример значения: `five_hour.utilization = 0.17`.
- `stream_event` с вложенными событиями Messages API: `message_start` (usage входа, кэш), `content_block_delta` (`text_delta`, `thinking_delta`), `message_delta` (итоговый usage, `stop_reason`), `message_stop`. Дельты текста мелкие, потоковость настоящая.
- `assistant` (целое сообщение), `system/thinking_tokens`, `system/status`.
- `result`: `usage{input_tokens, cache_creation_input_tokens, cache_read_input_tokens, output_tokens, output_tokens_details.thinking_tokens}`, `modelUsage[<модель>]{contextWindow: 1000000, maxOutputTokens: 128000, costUSD, …}`, `total_cost_usd` (расчёт клиента по списку цен, `costBasis: "list"`, **не** реальный счёт подписки), `duration_ms`, `ttft_ms`, `is_error`, `terminal_reason`, `api_error_status`.
- Размер контекста приходит в `modelUsage[…].contextWindow`: отдельный запрос не нужен.

Объём «накладных»: с `--restricted` и коротким системным промптом вход 714 токенов; с системным промптом по умолчанию 3–4 тыс.; в обычном режиме с инструментами 40–70 тыс.

### 1.4 Выбор аккаунта

`CLAUDE_CONFIG_DIR=<папка>`. Проверено на пустой папке: CLI создаёт в ней `sessions/plugins/backups`, `exit=1`, поток содержит `result` с `is_error: true`, `result: "Not logged in · Please run /login"`, `terminal_reason: "api_error"`, `api_error_status: null`, `subtype: "success"` (!), нулевой usage.
Правило классификации: `is_error` + текст, а не `subtype`. Класс `Auth` по подстроке `Not logged in`.
**Не проверено:** второй залогиненный аккаунт (нужен вход владельца: `CLAUDE_CONFIG_DIR=<папка> claude auth login`). Событие `system/api_retry` с полем `error` (`authentication_failed | rate_limit | …`) описано в документации, в потоке не воспроизводилось [доки].

### 1.5 Время

Три запуска «ответь pong» (Haiku): `system/init` через 3.7–4.1 с после старта процесса, первый токен 5.2–7.4 с, `result` сразу за ним, процесс завершается ещё через ~0.8 с (после `result` его можно убивать). Холодный старт ≈ 4 с на вызов; запрос с пустой папкой конфигурации 8.5 с.

## 2. OpenCode 2.2.5

### 2.1 Команда режима модели

```
XDG_CONFIG_HOME=<пустая папка>  XDG_DATA_HOME=<папка аккаунта или не задавать>  \
OPENCODE_CONFIG_CONTENT='{"permission":{"*":"deny","skill":"deny"},"agent":{"plain":{"mode":"primary","description":"plain","prompt":"<системный промпт>","permission":{"*":"deny"}}}}' \
<…>\@opencode\cli\bin\opencode.exe run --format json --standalone --agent plain -m <провайдер/модель> "<промпт>"
```

- Бинарь вызывается напрямую: `opencode` в PATH — это `opencode.ps1`/`.cmd` из npm, `CreateProcess` их не запускает (проверено в Python: `WinError 2`).
- **`--standalone` обязателен.** Без него `opencode run` ходит в фоновую службу пользователя (`opencode service`), и она **игнорирует** `OPENCODE_CONFIG_CONTENT` и `XDG_DATA_HOME` клиента [проверено]: запрос с `permission:{"*":"deny"}` создал `pwned.txt`.
- `XDG_CONFIG_HOME` на пустую папку обязателен: иначе в режиме `--standalone` читается глобальный конфиг пользователя (`~/.config/opencode`) вместе с MCP-серверами, плагинами и инструкциями. При упавшем MCP-сервере (503) запуск молчал 25–60 с без единого события, 4 прогона подряд; с пустым `XDG_CONFIG_HOME` такого не было [проверено; причина — вероятная, по журналу `--print-logs`].
- Рабочая папка — пустая временная.

### 2.2 Негативные проверки

| # | Запуск | Результат |
|---|---|---|
| 1 | **контроль**: `--standalone`, без ограничений (`o2`) | `pwned.txt` **создан** (инструмент `write`) |
| 2 | `--standalone` + `permission:{"*":"deny"}` (`o3`) | файла нет, модель сообщает, что инструментов нет |
| 3 | то же + `--agent plain` со своим промптом (`o4`) | файла нет; вход 618–4 тыс. токенов вместо 31 тыс. у агента по умолчанию |
| 4 | **без `--standalone`**, `deny`, пустой `XDG_DATA_HOME` (`o6`) | **файл создан**: служба проигнорировала и конфиг, и каталог данных |
| 5 | `--standalone`, пустой `XDG_CONFIG_HOME`, текстовый протокол `<tool_use>` (`o11`) | файла нет. В части прогонов (2 из 5) модель вызвала `write_file` нативным вызовом, CLI ответил ошибкой `No tool named "write_file" is currently available` и начал второй шаг: ничего не исполнено, но лишний шаг и токены |

Вывод: режим модели достигается, но CLI остаётся «агентным циклом»: нативный вызов несуществующего инструмента даёт лишний шаг. В адаптере: событие `tool_use` с `state.status = error` игнорировать, итог брать из последнего `text`; ограничить шаги (`agent.<имя>.steps = 1` — **не проверено**).

### 2.3 События, usage, лимиты

`--format json`, по строке JSON: `step_start`, `tool_use`, `text`, `step_finish`, `error`.
- `text` приходит **целиком**, а не дельтами (потоковости токенов нет) [проверено].
- `step_finish.part`: `reason` (`stop`/`tool-calls`), `cost`, `tokens{input, output, reasoning, cache{read, write}}` на каждый шаг; итог хода — сумма по шагам.
- Ошибка: `{"type":"error","error":{"type":"provider.no-route","message":"Model unavailable: …"}}`, `exit=1`.
- Размер контекста и лимиты подписки в потоке **отсутствуют**. `opencode models` в 2.2.5 печатает только идентификаторы (без `--verbose`). Размер контекста — из каталога моделей аккаунта, лимиты OpenCode Go — только неофициальный HTTP (`03-t3-limits-and-instances.md` §7.1), в релиз не входят.

### 2.4 Выбор аккаунта и время

`XDG_DATA_HOME=<папка>` (там `auth.json` и `opencode.db`, по коду T3; файл мной не читался). Проверено на пустой папке: `error provider.no-route`, `exit=1`, 13 с (инициализация новой базы). Второй залогиненный аккаунт не проверялся.
Время (`--standalone`): `step_start` через 2.7–3.8 с, всего 3.6–11 с.

## 3. Codex (не установлен)

По `E:\Projects\Forks\t3code` и документации (context7 `/openai/codex`) [доки]:
- Одноразовый режим есть: `codex exec --json --ephemeral --skip-git-repo-check --ignore-user-config --ignore-rules --sandbox read-only -o <файл>`; `--json` печатает JSONL, событие `turn.completed` несёт usage.
- Изоляция аккаунта: `CODEX_HOME`. Лимиты: JSON-RPC `account/rateLimits/read` через `codex app-server` (T3).
- Отключение инструментов: `features.shell_tool = false` убирает только оболочку; `apply_patch` включается по каталогу модели без флага; полное отключение `allowed_tools: []` есть лишь программно (расширение), не в пользовательском конфиге. `--sandbox read-only` оставляет чтение файлов.
- Вывод: пользовательскими средствами «чистая модель» не достижима → **не поддерживается** в режиме модели. Живая проверка владельцем: `codex exec --json --sandbox read-only -c features.shell_tool=false "<приманка>"` во временной папке; если модель всё же читает файлы или правит их через `apply_patch`, вывод подтверждён.

## 4. Что это меняет в контрактах

1. `Capabilities.pure_model` (обязателен `true` для CLI в режиме модели). Фасад не выбирает CLI-аккаунт с `pure_model = false` ни для какого хода.
2. `Credential::CliHome` заменяется набором переменных окружения на инструмент (`env`), а не одной папкой: Claude — `CLAUDE_CONFIG_DIR`; OpenCode — `XDG_DATA_HOME` (+ обязательный пустой `XDG_CONFIG_HOME` из нашего каталога). Критик B11 подтверждён.
3. `BackendKind::Cli` несёт `CliTool` (`Claude | OpenCode | Codex`) и `CliTransport` (`OneShot` для Claude и OpenCode; `JsonRpc` для Codex — на будущее).
4. CLI-бэкенд отдаёт: настоящий `usage` (Claude и OpenCode), `context_window` (только Claude: в `result`), `LimitSnapshot` (только Claude: `rate_limit_event`, источник `CliEvent`, свежесть — момент хода), потоковые дельты (только Claude). У OpenCode `streaming = false`.
5. Холодный старт ≈ 3–4 с на вызов (оба CLI): CLI-бэкенд помечается `slow_start`, а роли `title`/`summary` на нём — нежелательны.
6. Процесс CLI убивать целым деревом (`tools::background::win_job`) и сразу после `result`/`step_finish` с `reason = stop`; пустая рабочая папка и очищенное окружение (`CLAUDE_*` родителя) — часть контракта запуска.
7. Классификатор ошибок: Claude — `is_error` + текст (`Not logged in` → `Auth`), события `rate_limit_event.status != allowed`; OpenCode — событие `error` и код выхода. Остальные классы (лимит, перегрузка) не воспроизводились — **не проверено**, нужны записанные образцы от владельца.

## 5. Не проверено, нужно владельцу

- Второй залогиненный аккаунт Claude и OpenCode (изоляция «живьём»).
- Форма ошибок CLI при исчерпанном лимите (`rate_limit_event.status = rejected`, `api_retry`).
- `codex` целиком.
- Фоновая служба OpenCode (`opencode serve --service`, pid 9380) на момент проверки уже работала и принадлежит пользователю; мной не запускалась и не останавливалась.
