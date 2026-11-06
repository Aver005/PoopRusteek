# REFERENCE: шлюз аккаунтов и маршрутов

> Как несколько API-ключей и подписок превращаются в один `LLMProvider`.
> Код: `src/gateway/`, `src/provider/failure.rs`, `src/server/routing.rs`,
> `src/app/gateway_view*`, `src/commands/defs/{accounts,routes,roles,pin,usage}.rs`.
> Контракты дорожек и история решений: `.docs/gateway-contracts.md`,
> `.docs/release-gateway-plan.md`, `JOURNAL/2026-11-06-gw-*.md`.
> Last updated: 2026-11-07.

## 1. ЗАЧЕМ

Один аккаунт DeepSeek — единственная точка отказа: кончился лимит, протух
токен, сервер ответил 429 — работа встала. Шлюз держит несколько целей и
выбирает одну на каждый вызов: API-ключи, встроенный DeepSeek и подписки через
официальные CLI. Наружу (в агент, в роли, в `/serve`) видна одна реализация
`LLMProvider` — `RoutedProvider`. Она же отвечает за переход на следующий шаг
маршрута и за учёт.

Шлюз выключен по умолчанию (`[gateway] enabled = false`), и тогда всё прежнее:
`main_provider` собирает провайдера старым путём `build_provider`, `/serve`
работает по `[[providers]]`, никаких аккаунтов и маршрутов.

## 2. МОДЕЛЬ ДАННЫХ (`gateway/types.rs`)

| Тип | Что это |
|---|---|
| `AccountId`, `RouteId` | стабильные идентификаторы; `id` аккаунта не меняется никогда, переименование правит только `label` |
| `Account` | `id`, `label`, `provider`, `base_url`, `model`, `enabled`, `favorites`, `limits`, `tools`, `secret`, `api_key` (наследие), `home`, `direct_token`, `direct_token_ack` |
| `AccountProvider` | `deepseek-web` (встроенный web), `openai`, `anthropic`, `gemini`, `custom`, `cli:claude`, `cli:opencode`, `cli:codex` |
| `Credential` | плоские поля `secret/api_key/home/direct_token`; отдельного `env` нет — переменные CLI выводятся из `home` (`CliTool::home_env_var`) |
| `AccountLimits` | `soft_pct`, `hard_pct`, `budget_usd_month`, `requests_per_minute`, `max_concurrency` |
| `Route` | `id`, `strategy` (`ordered` — единственная), `steps`, `on`, `switch_at_pct`, `allow_mixed_tools` |
| `Step` | `{ account, model? }`; вложенных маршрутов нет |
| `FallbackTrigger` | `rate_limit`, `quota`, `spend_cap`, `overloaded`, `timeout`, `server_error`, `auth`; пустой `on` = все |
| `Role` / `Roles` | `main`, `title`, `summary`, `evaluator`; роль без своего маршрута идёт по `main` |
| `ToolMode` | `native` (поле `tools` запроса) или `prompt` (`<tool_use>` в тексте) |
| `GatewaySettings` | `schema` (по умолчанию 1), `enabled` |
| `GatewayConfig` | четыре таблицы: `gateway`, `accounts`, `routes`, `roles` |

`GatewayConfig::validate()` — единственная проверка формы. Она ловит ссылки на
неизвестный аккаунт, пустой маршрут, `switch_at_pct > 100`,
`hard_pct < soft_pct`, `max_concurrency = 0`, смесь родных и промптовых
инструментов без `allow_mixed_tools`, API-шаг без модели (ни на шаге, ни на
аккаунте), две одинаковые `home` у одного CLI, прямой токен Claude и
`direct_token` без `direct_token_ack`. `Account`, `Route`, `GatewaySettings`,
`GatewayConfig` — `deny_unknown_fields`: опечатка в ключе не игнорируется.

`GatewayConfig::from_legacy()` зеркалит `[[providers]]` в `[[accounts]]` при
первом запуске; обратной записи нет.

`Route::tool_mode(accounts)` фиксирует режим на ход: родной только если **все**
шаги родные; смесь с `allow_mixed_tools = true` работает промптовым протоколом,
без флага — ошибка конфига. `agent/runner.rs` читает `native_tools()` один раз
за ход, поэтому смена цели не меняет формат вызова на полпути.

## 3. ДВИЖОК

`gateway::engine::Engine` реализует `Gateway`. Внутри — `Core::select`, одна
функция выбора над конфигом, состоянием аккаунтов и часами (`gateway/clock.rs`:
`Clock`, `SystemClock`, `ManualClock` для тестов).

Порядок выбора:

1. `target` из `/pin` (`DecisionQuery.pinned`) — жёсткая привязка; другие
   аккаунты получают `PinnedElsewhere`.
2. Шаги маршрута по порядку. Для каждого проверяются `enabled`, состояние
   (`AccountStatus`), свежие лимиты, режим инструментов, окно контекста и
   занятость (`max_concurrency`, семафор на аккаунт).
3. Первый подходящий берётся; если это тот же аккаунт, что в прошлый раз, —
   `Why::Sticky` (ради кэша промпта), иначе `FirstReady`.
4. Всё занято, но есть живые: ждём очередь, только если больше некого.
   Иначе — `Why::RouteExhausted`, и `RoutedProvider` отдаёт сохранённую
   настоящую ошибку либо `RateLimit`/`Overloaded` с ожиданием.

`AccountState` (`gateway/state.rs`) — не «открыт/закрыт», а политика cooldown:

| Причина | Пауза |
|---|---|
| `Retry-After` | ровно заголовок, потолок 24 ч |
| без заголовка, `RateLimit` | 5 с → 5 мин ×2 с разбросом ±20 % (хеш, без `rand`) |
| `QuotaExhausted` без известного сброса или далёкий сброс | проба через 30 мин |
| `SpendCap` | 1 ч |
| `Overloaded/Timeout/Server/Network` | короткая пауза, иначе упавший аккаунт долбится каждый вызов |
| `Auth` / `AccountBlocked` | `NeedsAttention`, проба через 10 мин / 1 ч (`recheck_at`) |

Успешный вызов снимает любой запрет. Выключенное человеком не возвращается.
После перезапуска `state.json` содержит только подсказки `Exhausted`: подсказка
с будущим сроком ставит аккаунт в конец порядка, но не блокирует; первый вызов
решает, успех стирает подсказку. Битый файл игнорируется.

`Decision` — не только «кого взяли», но и почему: `target`, `why`,
`skipped: Vec<Skipped>` с `SkipReason`, `retry_at`, возраст снимка лимитов.
`Decision::explain()` — строка для экрана и трассы. Пропуски объясняются
словами (`Cooling`, `Disabled`, `ToolModeMismatch`, `NotPureModel`,
`ContextTooSmall`, `OverSoftLimit`/`OverHardLimit` со свежестью,
`NotForServe`, `InvalidConfig`, `NoAttachmentSupport`, `Busy`,
`FailedThisCall`, `PinnedElsewhere`).

`apply_edit(ConfigEdit)` — единственный путь правок из UI: `UpsertAccount`,
`RemoveAccount`, `SetLabel`, `SetEnabled`, `MoveAccount`, `UpsertRoute`,
`RemoveRoute`, `SetRole`, `SetEnabledGateway`. Каждая правка проверяется
`validate()`; отказ только по **новым** ошибкам, чтобы сломанный конфиг можно
было чинить по одной. Правка превращается в полный новый
`GatewayConfig`, уходит событием `GatewayPersist`, приложение кладёт его в
`Config` и сохраняет через `config::save`.

Сборка движка читает секреты (хранилище, `cmd:`, файлы) и потому идёт на
`spawn_blocking`: приложение стартует с `PendingGateway`/`PendingHandle`, по
готовности приходит `AppEvent::GatewayBuilt`. Пока сборка идёт, панели
отвечают из конфига (`Why::Starting`, `ResolveError::Starting`), а запрос
ждёт готовности. Выключенный шлюз готов сразу и ничего не читает.

`main_provider(&config, gateway)` — общая точка сборки для приложения
(`App::new`, `rebuild_provider`), онбординга, харнесса, ACP и `server/proxy.rs`.

## 4. АДАПТЕРЫ (`gateway/backend.rs`)

`Backend` — трейт одного аккаунта:

```text
call(request, &CallContext, EmitTracker) -> Result<CallReport, BackendError>
list_models(), probe(), fork()
context_window(), accepts_attachment()
list_remote_sessions(), delete_remote_session_by_id(),
fetch_remote_session_messages(), check_session(), adopt_session()
proxy_json()  # /embeddings, /rerank для OpenAI-аккаунта
```

`Capabilities` говорит, годится ли аккаунт в ход с инструментами
(`usable_for_tool_turn(mode)`), поддерживает ли вложения, серверные сессии,
`pure_model`, `slow_start`. `CallContext` несёт аккаунт, модель и `ToolMode`:
`request.model` игнорируется — модель выбирает маршрут. `CallReport` —
последнее слово сервера: `Usage`, лимиты, `finish_reason`, окно контекста,
`ttft`, `upstream_id`; текст и вызовы уже ушли через `EmitTracker`.
`EmitTracker` помнит, ушёл ли уже видимый чанк: это граница fallback.

| Файл | Адаптер |
|---|---|
| `api_backend.rs` | `ApiBackend` поверх `build_entry_provider`; `drive_provider` дочитывает поток до конца ради чанка расхода OpenAI; `proxy_json` для embeddings |
| `deepseek_backend.rs` | `DeepseekBackend` поверх встроенного web-клиента; расход — оценка |
| `cli/adapter.rs` | `CliBackend: Backend` для Claude Code и OpenCode |
| `cli/codex.rs` | `CodexBackend`: `pure_model = false`, в ходы с инструментами не берётся |

Фабрики: `BackendFactory` спрашивают по порядку, первая взявшая аккаунт
побеждает. `BuiltinFactory` собирает DeepSeek и API-аккаунты, `CliFactory` —
`cli:*`. `EngineDeps::for_config` подставляет `StoreResolver` (секреты) и обе
фабрики. `BuiltinFactory` отказывает `direct_token` для Claude (по провайдеру
или хосту) и без `direct_token_ack` независимо от `validate()`.

### Подписки через CLI

CLI работает **как чистая модель**: наш системный промпт, наши инструменты и
подтверждения, наш цикл. Режим агента — следующий релиз.

- **Claude Code**: `claude -p --output-format stream-json --verbose
  --include-partial-messages --restricted --tools "" --disable-slash-commands
  --strict-mcp-config --permission-mode dontAsk --permission-prompts none
  --no-session-persistence --system-prompt-file <файл> --model=<m>`; промпт
  через stdin; пустая рабочая папка; `CLAUDE_CONFIG_DIR = home`;
  родительские `CLAUDE_*`, `CLAUDECODE`, `ANTHROPIC_*` убираются. Минимум
  2.1.248 (флаг `--restricted`).
- **OpenCode**: `--standalone` обязателен, пустой `XDG_CONFIG_HOME`,
  `OPENCODE_CONFIG_CONTENT` с `permission: {"*":"deny","skill":"deny"}` и своим
  агентом, `XDG_DATA_HOME = home`; запускать по пути
  `…\@opencode\cli\bin\opencode.exe`, не через npm-шим.
- **Codex**: только прямой токен с ручным подтверждением. В режиме модели не
  поддерживается: по исходникам все инструменты не отключаются, живой прогон
  не делался.
- `Dialect::verify` проверяет собранный запуск перед каждым стартом: потерянный
  охранный флаг, второй `--tools`, чужая `CLAUDE_*` — отказ, процесс не
  стартует. `Launch` в `Debug` печатает только имена переменных.
- Процесс живёт в своей задаче (`win_job::CommandJob` на Windows, группа на
  unix), сторож простоя и потолок вызова убивают дерево; `Drop` гасит его в
  любом случае. `kill_on_drop` на Windows недостаточен.
- `POOPRUSTEEK_CLI_CLAUDE` / `POOPRUSTEEK_CLI_OPENCODE` подменяют бинарь
  (`путь`, JSON-массив argv или `mock:<сценарий>`) только в тестах и в
  харнессе с `--config`.
- Текст шага OpenCode буферизуется; шаг `tool-calls` (нативный вызов
  несуществующего инструмента) отбрасывается; успех — шаг `stop`. У Claude
  дельты текста настоящие, `contextWindow` и `rate_limit_event` приходят в
  потоке. У OpenCode ни лимитов, ни контекста из потока нет.

## 5. `RoutedProvider` И FALLBACK

`gateway/routed.rs` — `LLMProvider`, который видит агент и `/serve`.

- **Fallback только до первого видимого чанка**:
  `EmitTracker::emitted() == false`. Чанки без текста и вызовов (живые
  сигналы, расход) не считаются. После первого байта ошибка уходит как есть,
  второй аккаунт не подмешивается.
- Помимо `route.on` всегда разрешены `ContextTooLong` и `ModelNotFound`
  (`BadRequest`, `ContentFilter`, `Cancelled` — нет: на другом аккаунте будет
  то же).
- Закреплённый аккаунт и `account/model` без запасных не переходят.
- `fork()` и суб-агенты делят движок и семафор на аккаунт; занятый первый шаг
  не ждёт, а пропускается (`SkipReason::Busy`), очередь — только если больше
  некого.
- Смена цели в беседе: `history::flatten_for_switch` разворачивает вызовы
  прежнего аккаунта в текст до первого вызова, затем новый `Backend::fork()`.
  `provider_state` и чужие id не переносятся. Развёрнутая часть запоминается
  (`flatten_prefix`) и разворачивается снова на каждом следующем вызове.
  Смена модели у того же аккаунта тоже считается сменой (подписи Gemini).
- `CancelGuard`: уход клиента `/serve` или отмена хода дают одну запись
  `Outcome::Cancelled` с оценкой расхода, `report_failure` не зовётся, аккаунт
  не остывает.
- `served_by()` отдаёт `ServedBy` для заголовков `/serve`; адресов и ключей в
  нём нет.

## 6. РОЛИ

`provider_for_role(role, CallSource::Role(role))`: без роли шлюз возвращает
`None`, вызывающий остаётся на прежнем пути. Роль без своего маршрута
наследует `main`.

- **`title`** (`app/titles.rs`): после первого законченного хода
  `maybe_request_chat_title` ставит `Conversation.title_requested` и уходит в
  `tokio::spawn`; запрос идёт на форке провайдера роли, таймаут 10 с, серверная
  сессия форка удаляется (`SessionDiscard`), в историю беседы вызов не
  попадает. Модель получает **первую просьбу пользователя и первый ответ**
  (по 800 байт, после `secrets::redact`). Ответ чистится `clean_title`:
  недоверенный текст → одна строка, без разметки, кавычек, управления, до 60
  символов. Не вышло — остаётся `session::derive_title`. Повтора нет.
- **`summary`**: `summariser_for` отдаёт провайдера роли для `/compact`;
  без роли — форк провайдера чата.
- **`evaluator`**: `evaluator_for` для цикла GOAL; без роли — форк чата.
- `validate()` запрещает явные `title`/`summary` на маршруте с `cli:*`:
  холодный старт 3–4 с дороже самого заголовка. Наследование от `main` не
  наказывается.

Заголовок сохраняется в `Session.title` (`#[serde(default)]`), автосейв его не
затирает, `/chats` и `/sessions` показывают служебный заголовок, если он есть.

## 7. `/serve`

`src/server/routing.rs` (`GatewayCell`, каталог, заголовки, статус) и
`server/openai.rs::resolve_backend`.

- При включённом шлюзе каждый запрос читает живой `Gateway` из `GatewayCell`,
  поэтому правки конфига и пересборка (`/wipe`, `GatewayBuilt`) видны без
  перезапуска сервера.
- Каталог `/v1/models`: `auto/<route>` (если в маршруте есть отдаваемый шаг) и
  `<account>/<model>` (модель по умолчанию, избранные, найденные у апстрима,
  режимы DeepSeek). Не показываются и не принимаются: любые `cli:*`,
  выключенные и недоступные аккаунты, модели вне каталога. Неизвестный аккаунт,
  маршрут, модель или грязный адрес — 404 `model_not_found`, апстрим не
  трогается. Голое имя модели или аккаунта за шлюзом не принимается.
- **`CallSource::Serve` никогда не выбирает `cli:*`** (`closed_to_serve`):
  подписка наружу не раздаётся. Это относится к Claude, OpenCode и Codex.
- Поток держится до первого видимого чанка (`held_stream_completion`,
  `await_first`): отказ до него — настоящий HTTP-код (429 с `Retry-After`,
  502, 504), а заголовки называют того, кто отвечает. После первого байта —
  ошибка кадром в потоке. Ответ с инструментами при `stream: true` копится и
  уходит одним ответом.
- Заголовки `x-pooprusteek-account`, `-model`, `-route`, `-fallback`
  (аккаунты, отказавшие раньше), `-fallback-reason` (коды классов). Только
  видимый ASCII, ≤ 200 символов, без адресов и ключей. На ошибочных ответах не
  ставятся.
- `GET /v1/pooprusteek/status` (и без `/v1`) — за тем же bearer-ключом:
  аккаунты, состояния, лимиты (источник и возраст), последние решения,
  замечания конфига; тексты проходят `scrub` (URL → `<url>`). Без ключа —
  урезанный вид (имена и состояния) и только на loopback; на сетевом адресе без
  ключа 403. Без шлюза 404.
- `POST /v1/embeddings` и `/v1/rerank`: `<account>/<model>` → `Backend::proxy_json`
  (адрес и ключ аккаунта, без редиректов); `auto/…` → 400; `cli:*`,
  неизвестное, Anthropic/Gemini/DeepSeek → 400/404.
- Сборка шлюза: `GET /v1/models`, чат и embeddings во время `Starting` — 503
  `gateway_starting` + `Retry-After: 2`; `Failed` — 503 `gateway_failed` с
  причиной без `Retry-After`.
- **Диалект Anthropic (`/v1/messages`) не сделан** — 501; что для него нужно,
  перечислено в `JOURNAL/2026-11-06-gw-serve.md`.

Клиент не может задать `base_url`, ключ, заголовки аккаунта или опции CLI;
лишние поля тела игнорируются, его `Authorization` апстриму не уходит. Текст
отказа авторизации апстрима и ключ в клиент не попадают.

## 8. СОБЫТИЯ И ЖУРНАЛ

В приложении: `AppEvent::Gateway(GatewayEvent)` (решения, лимиты, переходы),
`AppEvent::GatewayPersist` (сохранение правки), `AppEvent::GatewayBuilt`
(сборка готова). На `FallbackTaken` в историю встаёт `ui_only`-строка со
статусом `SEAM`: `── switched claude-max → opencode-go: rate limit, back in
2m 10s`; в контекст модели она не идёт.

`debug_log` (он же трасса харнесса) получает:

| Запись | Что несёт |
|---|---|
| `gateway.decision` | `explain()`, цель, пропуски |
| `gateway.fallback` | откуда, куда, класс отказа |
| `gateway.cli.finished` | класс, длительность, сводка вызова |
| `gateway.cli.refused` | запуск отвергнут `verify` (потерян охранный флаг) |
| `gateway.cli.unrecognized_failure` | полный неопознанный текст от CLI |
| `gateway.cli.note` | пометка адаптера (например, шаг без текста) |

Слой шлюза ничего не добавляет в цикл агента: все записи — существующие
`debug_log` в новых точках.

## 9. УЧЁТ (`gateway/ledger.rs`)

Файл `data_dir/gateway/usage.json`. Строка — ключ `(день, аккаунт, модель,
источник)`; источник: `chat`, `sub_agent`, `serve`, `role:main/title/summary/evaluator`.
Поля: `calls`, `failed`, `fallbacks`, `prompt_tokens`, `completion_tokens`,
`cache_read_tokens`, `estimated`.

- Расход берётся из `CallReport` (последнее слово сервера), а не суммой чанков.
- Дедуп по `(request_id, attempt, account)`: повторная запись того же вызова не
  считается дважды.
- Копится в памяти, раз в 30 с сливается фоновой задачей на `spawn_blocking` и
  при выходе. `flush` читает файл, прибавляет дельту, пишет `atomic_write` и
  берёт слитое как новый вид; при ошибке записи дельта возвращается. Два
  процесса складывают, а не затирают друг друга. Замков нет — маленькая гонка
  между чтением и записью принята.
- `cost_usd` пуст (нет проверенного источника цен), `budget_usd_month` не
  применяется.
- `/usage` показывает таблицу за 24 часа по аккаунтам.

## 10. ИЗВЕСТНЫЕ ОГРАНИЧЕНИЯ

Честно, без «уже работает»:

- **Подписки настоящими двумя аккаунтами не проверены.** Раздельность папок
  проверена по окружению процесса; живой второй вход (`claude auth login`,
  вход OpenCode) не делался ни разу.
- **Сборка и прогон под Linux и macOS не запускались.** Код `cfg(unix)`
  (`process_group`, права 0700/0600) повторяет `tools/shell.rs`, но на этой
  машине не компилировался и не исполнялся; его прогонит CI.
- **Codex в режиме модели не поддерживается** (только прямой токен).
- Нет `spread`, вложенных маршрутов, активных проб, `Draining`/half-open, роли
  `subagent`.
- `limits.json` между запусками не пишется: после перезапуска снимки всё равно
  были бы устаревшими. `state.json` — только подсказки.
- Аккаунт, у которого не собрался адаптер (нет переменной, нет бинаря, старая
  версия CLI), остаётся `Unavailable` до правки аккаунта, клавиши `u` или
  перезапуска; автопересборки нет.
- Пометки «модель недоступна у аккаунта» нет — защищает только фактический
  отказ вызова.
- После ухода с DeepSeek на другой аккаунт и обратно DeepSeek получает свежую
  сессию; оставленная серверная сессия жива, но `/load` по ней подхватит ветку
  без реплик, которые шли на другом аккаунте.
- Диалект Anthropic в `/serve` — 501.
- Реальный расход последнего вызова не выведен в `LLMProvider`: индикатор
  заполнения окна у API-аккаунтов остаётся оценкой.
- Настоящих образцов исчерпания лимита CLI нет: у Claude ловится только
  структурно (`rejected` в окне) или по HTTP-статусу, у OpenCode — как
  неопознанный `Server` с коротким cooldown.
- Живой прогон DeepSeek через включённый шлюз делался, живой прогон CLI — нет.
