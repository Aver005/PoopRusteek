# REFERENCE: Config, storage and sessions

> Where everything lives and how the config is assembled.
> Source: `src/config/` (`paths`, `layers`, `load`, `write`, `queue`, `migrate`,
> `startup`, `fields`, `describe`), `src/session.rs`, `src/gateway/types.rs`.
> Описание для людей: `.docs/secrets-and-config.md`. Пример: `config.example.toml`.
> Last updated: 2026-11-07 (новая схема шлюза, два слоя, перенос и откат).

## ГДЕ ЧТО ЛЕЖИТ

| Что | Где | Как получается |
|---|---|---|
| Общий конфиг | `~/.pooprusteek/config.toml` | `paths::global_dir()` = `home_dir()/.pooprusteek` |
| Проектный слой | `.pooprusteek/config.toml` | `paths::find_project_layer(start)`: вверх до `.git`; вне git только папка запуска |
| Файл из `--config` | любой путь | заменяет оба слоя; `Config::path()` = `--config`, иначе общий файл |
| Данные и кэш | `dirs::data_local_dir()/pooprusteek` | `paths::default_data_dir()`; на Windows `%LOCALAPPDATA%`, не roaming |
| Старый конфиг | `dirs::config_dir()/pooprusteek/config.toml` | `paths::legacy_config_path()` — только источник переноса и отката |
| Старая папка данных | `dirs::data_dir()/pooprusteek` | `paths::legacy_data_dir()` — источник переноса |
| Копия перед первой правкой | `config.toml.pre-gateway` рядом с конфигом | `migrate::backup_path` |
| Метка переноса данных | `data_dir/.migrated-from-legacy` | `paths::DATA_MARKER` |

Платформы:

| | Linux | Windows | macOS |
|---|---|---|---|
| Новый конфиг | `~/.pooprusteek/config.toml` | `%USERPROFILE%\.pooprusteek\config.toml` | `~/.pooprusteek/config.toml` |
| Новые данные | `~/.local/share/pooprusteek` | `%LOCALAPPDATA%\pooprusteek` | `~/Library/Application Support/pooprusteek` |
| Старый конфиг | `~/.config/pooprusteek/config.toml` | `%APPDATA%\pooprusteek\config.toml` | `~/Library/Application Support/pooprusteek/config.toml` |
| Старые данные | `~/.local/share/pooprusteek` | `%APPDATA%\pooprusteek` | `~/Library/Application Support/pooprusteek` |

На Windows и macOS старый конфиг лежит **внутри** старой папки данных. Поэтому
перенос данных идёт по явному списку, а не «всё, кроме»: иначе открытые ключи
уехали бы в новую папку вместе с настройками.

## ДВА СЛОЯ

`config::load_from_optional` читает общий слой, затем проектный и сливает их:

- скалярные ключи: проект переопределяет общий;
- `[[accounts]]` и `[[routes]]`: массивы сливаются по `id` (аккаунт или
  маршрут с тем же `id` заменяется/дополняется, порядок — из общего слоя);
- `[trust]` проект менять не может;
- `Config.source` помнит, откуда взято каждое значение (`origins`), какие слои
  применены или отвергнуты (`layers`) и что ждёт подтверждения
  (`pending_trust`).

Проектный слой недоверенный, потому что может приехать из чужого репозитория.

**Запрещено целиком** (слой не применяется, причина в `doctor` и в заметке при
запуске): `[server]`, `[update]`, `[instructions]`, `[[providers]]`,
`active_provider`, `[secrets]`, `[trust]`, `provider.{token,base_url,kind,client_version}`,
`skills.paths`; у аккаунтов — `api_key`, `direct_token`,
`direct_token_ack`, `home`, `secret` вида `cmd:`/`file:` или литерал; смена
`base_url`/`provider` у аккаунта с известным общим `id`; любое значение,
похожее на ключ.

**Ждёт подтверждения** (fail closed): `[gateway]`, `[[accounts]]`, `[[routes]]`,
`[roles]` — всё, что меняет, куда уходят запросы. До подтверждения действуют
общие значения, остальное из проекта (тема, лимиты агента) применяется.
`pooprusteek trust` пишет путь и SHA-256 файла в `[trust.projects]` общего
конфига; изменение файла меняет хэш и спрашивает снова. `pooprusteek trust
--revoke` снимает подтверждение.

Подтверждение файла не даёт ключ. Аккаунт из проекта получает секрет только для
подтверждённого хоста: `pooprusteek secrets bind <ссылка> <хост>` (или
`/secrets bind`). Привязка держится за схему: голый `host` — это
`https://host`, для http надо писать `http://host`. Проектный аккаунт без
`base_url` ключа не получает.

> Форма подтверждения и то, что именно считается «изменением, куда уходят
> запросы», названы итоговым намерением релиза. Отдельная правка перед
> слиянием может их уточнить — сверить с `JOURNAL/2026-11-07-fix-*.md`; пока
> таких журналов нет.

## СЕКРЕТЫ

Поле секрета — ссылка, а не значение:

| Ссылка | Что делает | Где разрешена |
|---|---|---|
| `keyring:global/имя` | системное хранилище, служба `pooprusteek` | оба слоя |
| `env:ИМЯ` | переменная окружения | оба слоя |
| `cmd:["op","read","…"]` | вывод команды без оболочки, 10 с | только общий слой |
| `file:/путь` | файл с ключом, на unix 0600 | только общий слой |
| `none` | секрет не нужен | оба слоя |

Любая другая строка — ошибка загрузки, а не «просто строка». Значение читается
при сборке адаптера (`SecretResolver` / `StoreResolver`), а старые открытые поля
(`provider.token`, `providers[].api_key`, `server.api_key`) разворачиваются в
память при загрузке, чтобы остальной код не менялся. В файл они не
возвращаются: `config::save` пишет только разницу.

Длинное значение режется на куски по 900 байт (предел Windows — 2560 байт,
пароль хранится в UTF-16). Перезапись идёт в новое поколение кусков, заголовок
переключается последним; падение на любом шаге оставляет читаемым либо старое
значение целиком, либо новое, но не смесь.

`SecretResolver::resolve` блокирует (хранилище, `cmd:`, файл), поэтому его зовут
с `spawn_blocking`, не с цикла событий. Сборка шлюза это и делает
(`gateway::pending`).

## СХЕМА

```toml
[gateway]
schema = 1                 # конфиг новее нашей схемы читается только на чтение
enabled = false            # true включает аккаунты, маршруты и роли

[[accounts]]
id = "openai-work"         # не меняется никогда
label = "OpenAI (work)"    # можно править
provider = "openai"        # openai | anthropic | gemini | custom
                           # | cli:claude | cli:opencode | cli:codex
base_url = "https://api.openai.com/v1"
secret = "keyring:global/openai-work"
model = "gpt-5.5"
enabled = true
favorites = ["gpt-5.5"]
tools = "prompt"           # prompt | native
home = "~/.pooprusteek/accounts/openai-work"   # только CLI
direct_token = "keyring:global/…"              # только не-Claude и с ack
direct_token_ack = false

[accounts.limits]
soft_pct = 85              # с этого процента аккаунт уходит вниз по списку
hard_pct = 97              # с этого — не берётся
requests_per_minute = 30
budget_usd_month = 50      # заявлен, но в релизе не применяется
max_concurrency = 4        # CLI и DeepSeek 1, API 4

[[routes]]
id = "coding"
strategy = "ordered"       # в релизе только ordered
switch_at_pct = 90
on = ["rate_limit", "quota", "spend_cap", "overloaded", "timeout", "server_error", "auth"]
allow_mixed_tools = false
steps = [{ account = "claude-work", model = "claude-opus-5-5" }, { account = "openai-work" }]

[roles]
main = "coding"
title = "cheap"
summary = "cheap"
evaluator = "cheap"

[secrets.bindings]
"keyring:global/openai-work" = ["api.openai.com"]

[trust.projects]
"E:\\repo" = "sha256:…"    # пишет только pooprusteek trust
```

`Account`, `Route`, `GatewaySettings`, `GatewayConfig` — `deny_unknown_fields`:
опечатка в ключе шлюза — ошибка, а не молчаливое игнорирование. `GatewayConfig::validate()`
проверяет ссылки на несуществующие аккаунты, пустые маршруты, `hard_pct < soft_pct`,
смесь инструментов без `allow_mixed_tools`, модель у API-шага, `max_concurrency = 0`,
две одинаковые `home` у одного CLI и запрет прямого токена для Claude.

Старый `[[providers]]` продолжает **читаться** (и зеркалится в `[[accounts]]`
при первом запуске), но больше не пишется: `Config.providers` помечен
`skip_serializing_if`. `active_provider` работает только при выключенном шлюзе.

Остальные секции: `[provider]` (DeepSeek web), `[ui]` (theme, glyphs,
`panel` = auto|shown|hidden — правая панель чата, `landing` = center|sheet — вид главной),
`[agent]`, `[context]`, `[mcp]`, `[skills]`, `[instructions]`, `[semantic]`,
`[server]`, `[provider_models]`, `[update]`.

## ПЕРЕНОС

### Данные

`config::choose_data_dir()` при старте:

1. `--data-dir` выигрывает сразу; переноса нет.
2. Есть `data_dir/.migrated-from-legacy` — работаем с новой папкой.
3. Иначе `migrate::migrate_data_dir(old, new)` копирует по списку:
   `history.json`, `whitelist.json`, `checkpoints.jsonl`,
   `checkpoints-undone.jsonl`, `mcp.json` и папки `sessions/`, `checkpoints/`,
   `tool-output/`, `exports/`, `skills/`.
4. Всё остальное остаётся в старой папке и перечисляется в заметке при запуске:
   там могут лежать ключи от старого конфига. Модель, семантический индекс,
   `provider_models.json` и журналы не переносятся — перекачиваются и
   пересоздаются.
5. Если файл есть в обеих папках и различается, остаётся более свежий по
   времени изменения, проигравший ложится рядом как `имя.legacy`
   (`.legacy.2`, …). Повторный запуск копий не плодит.
6. Метка `.migrated-from-legacy` ставится только после полного успеха. До этого
   работаем со старой папкой; старая папка не удаляется никогда.

### Конфиг

1. `migrate::copy_legacy_config` копирует старый файл в новый, пока нового нет,
   и проверяет, что прочиталось то же самое.
2. `migrate::run` переносит открытые ключи в хранилище: запись → чтение мимо
   кэша → сравнение. Только после этого в новом файле на месте значения
   появляется ссылка.
3. `[[providers]]` зеркалятся в `[[accounts]]` (`GatewayConfig::from_legacy`) с
   общей записью хранилища; `[gateway] schema = 1, enabled = false`.
4. Перед первой правкой делается `config.toml.pre-gateway`.
5. Когда для каждой ссылки хранилище отдаёт то же значение, старый файл
   чистится через `toml_edit`: значения секретов — пустой строкой, ключи
   `[[providers]]` — убраны. Настройки, комментарии и `[server]` остаются.
   Копия `config.toml.pre-gateway` чистится после запуска, где все ссылки
   читаются.
6. Любое сомнение (хранилище не подтверждает значение, новый файл не читается)
   — старый файл не трогается.

### Откат

Старый бинарь читает только старый файл. Он остаётся на месте, настройки в нём
целы, ключи заменены пустыми значениями. После отката старая версия попросит
токен заново: токен лежит в хранилище и старой версии недоступен. Старая папка
данных остаётся целиком. Новый `~/.pooprusteek/config.toml` старый бинарь не
читает.

### Флаг `--config`

`config::mark_explicit_config` закрепляет файл: запись идёт только в него,
настоящий конфиг, хранилище, переносы и настоящие папки не трогаются
(`config::is_isolated`). Харнесс обязан передавать `--config` и `--data-dir`
(см. `reference/HARNESS.md` §10).

## ЗАПИСЬ КОНФИГА

`config::save` не переписывает файл целиком: сравнивает загруженное состояние с
текущим и применяет разницу через `toml_edit`. Комментарии, порядок и незнакомые
ключи остаются. Запись идёт под блокировкой `config.toml.lock` с перечитыванием
файла внутри неё; замок считается брошенным через 60 с, ожидание — 75 с.

В TUI запись не ждёт замок и диск на цикле событий: `config::save` ставит
задание в очередь одного рабочего потока и возвращается сразу, порядок правок
сохраняется, ошибка приходит событием в чат. Перед выходом, `/wipe` и
перезагрузкой конфига очередь сбрасывается. В CLI и тестах запись идёт на месте.
Файл, который не читается как TOML или как конфиг, не затирается.

## СЕССИИ (`src/session.rs`)

- **`Session`**: `version(=1), id, created_at, updated_at, workspace_root,
  model_type, messages, tag?, title?, provider_session_id?,
  provider_parent_message_id?, broken, route_state?`.
- `title` — служебный заголовок от роли `title`; `None` — запасной
  `derive_title` по первой реплике.
- `route_state` — на каком аккаунте/модели идёт беседа. `save_session` при
  `None` в метаданных берёт значение из файла, поэтому автосейв не затирает
  известную цель.
- **ID**: `{YYYY-MM-DDTHH-MM-SS-fff}Z-{uuid8}` — сортируется по времени.
- **Сохранение**: `save_session(id, created_at, messages, config, workspace_root,
  meta: &SessionMeta)`. `SessionMeta { tag, broken, remote: RemoteLink, route,
  title }`. Красивый JSON в `{sessions}/{id}.json`.
- **Загрузка**: `load_local(id)` → `SessionNotFound`; `list_sessions()` даёт
  `SessionSummary` (заголовок: сохранённый служебный, иначе `derive_title`).
- **Автосейв**: `App::auto_save_session()` после каждого хода. Берёт
  `Conversation.tag/broken`, `provider.session_identity()` и `route_state()`,
  а `title` — из беседы. `RemoteLink::Unknown` не стирает сохранённую ссылку на
  серверный тред; стирает только явная перезапись
  (`finalize_broken_session`).
- **Особые теги**: `__goal_system__` (сессии оценщика, скрыты из `/sessions`),
  `Imported`.
- **История ввода** `{data}/history.json`: до 500 строк, соседние дубликаты
  схлопнуты; строки с ключами и команды, принимающие секрет, не пишутся.

### Серверная сессия DeepSeek

`session_identity()` — живая пара `(session_id, parent_message_id)`;
`check_session()` — три состояния `Alive/Gone/Unknown`; `adopt_session()`
подхватывает известную серверную сессию вместо создания новой. Только `Gone`
(API ответил именно про эту сессию и отказал) даёт право стереть ссылку;
`Unknown` (сеть, протухший токен, HTML вместо JSON) файл не трогает. Подробности
и история дефектов — `reference/PROVIDER.md` и `BUGS.md`.

## ПРАВИЛА АВТООДОБРЕНИЯ

`whitelist.json` — массив правил:

```json
[
  { "tool": "read_file" },
  { "tool": "bash",  "scope": { "command": ["cargo", "test"] } },
  { "tool": "edit",  "scope": { "path": "E:\\Projects\\Me\\pooprusteek\\src" } }
]
```

Правило без `scope` — весь инструмент. Старый формат (голый массив имён)
читается и переносится с уведомлением. Нечитаемый файл уезжает в
`whitelist.json.bad`, а не затирается. Составная команда области не получает —
сузить её честно нельзя, поэтому она спрашивает каждый раз (см. `BUGS.md`).
`always_ask` у запроса отменяет белый список: такой вызов решает только человек.

## ОНБОРДИНГ, `/logout`, `/wipe`

Первый запуск без конфига или после `/logout`/`/wipe` показывает
`View::Onboarding` (DeepSeek-токен). `/logout` убирает токен из конфига и
открывает онбординг; запись в хранилище остаётся (удаляется `secrets rm`).
`/wipe` чистит записи хранилища, токены MCP и файлы конфига, но не
`~/.pooprusteek` целиком (там пользовательские правила); в прогоне с
`--config`/`--data-dir` отключён.

## ОШИБКИ И ЖУРНАЛЫ

- `AppError` (`src/error.rs`): `Io | Http | Json | Config | Provider | Mcp |
  Join | SessionNotFound | Upstream(Box<ProviderFailure>) | Custom`.
  `AppError::failure_class()` и `retry_after()` — единственный разбор отказов для
  шлюза и `/serve`.
- `Config::startup_problem()`: конфиг не загружен как есть или токен не
  прочитался — онбординг не открывать.
- `debug_log` — JSONL или человеческие строки; включается `--debug-log` или
  `/debug`. Путь: `.dev/debug.log` в debug-сборке, `{data}/debug.log` в release.
  Записи проходят `secrets::redact`; `errors.log` и stderr — нет.
