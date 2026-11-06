# Контракты шлюза: кто что делает дальше

Дата: 2026-11-06. Дорожка GW-0. Код контрактов: `src/gateway/{types,backend,handle,history}.rs`,
`src/provider/failure.rs`. Шлюз выключен (`[gateway] enabled = false`), поведение прежнее.
Исходные документы: `release-gateway-plan.md` (решения владельца), `research-2026-11/02-gateway-architecture.md`,
`ideas/I-05-10-gateway-critique.md`, `research-2026-11/06-cli-spike.md` (эксперимент с CLI).

## 1. Что зафиксировано

| Контракт | Где | Суть |
|---|---|---|
| Класс отказа | `provider::failure::FailureClass` | единственный источник правды; в `gateway` он же — `ErrorClass`. `AppError::failure_class()`, `AppError::retry_after()` |
| Отказ апстрима | `AppError::Upstream(Box<ProviderFailure>)` | класс, статус, `Retry-After`, заголовки лимитов (OpenAI, Anthropic) |
| Расход | `provider::Usage` + `CompletionChunk.usage` | `prompt_tokens` — весь вход с кэшем; Anthropic/Gemini — в терминальном чанке, OpenAI — отдельным чанком **после** `finish_reason` |
| Лимиты API | `LLMProvider::rate_limits()` | заголовки последнего ответа, общие для форков аккаунта |
| Данные | `gateway::types` | `Account`, `Route` (плоские шаги), `Role`, `Credential`, `GatewayConfig::validate()`, `from_legacy()` |
| Адаптер | `gateway::backend::Backend` | `call/list_models/probe/fork`, `Capabilities`, `CallContext`, `CallReport`, `BackendError`, `EmitTracker` |
| Фасад | `gateway::handle::Gateway` (`GatewayHandle = Arc<dyn Gateway>`) | `snapshot/provider_for_role/provider_for_model_id/decide/apply_edit/subscribe/ledger`; `DisabledGateway`; `main_provider()` |
| Решение | `handle::Decision` | цель, `Why`, пропущенные с причинами, возраст снимка, `retry_at`, `explain()` |
| Лимиты | `handle::LimitSnapshot` | возраст, источник (`Header/CliEvent/Ledger/Declared`); жёсткие решения только по `is_hard_signal` |
| История | `gateway::history::flatten_for_switch` | снимает `provider_state` и id вызовов при смене цели |

Отличия от `02-gateway-architecture.md`:
- `SpendCap` определяется по типу ошибки `billing_error` (документация Anthropic: `429 billing_error`, без `retry-after`), а не по отсутствию заголовка. Вживую не проверено.
- `Credential` собирается из плоских полей `secret/api_key/home/direct_token` (как в примере конфига); отдельного поля `env` нет, переменные CLI выводятся из `home` (`CliTool::home_env_var`).
- Нет `Draining`, `Open`/half-open, `spread`, вложенных маршрутов, активных проб, роли `subagent`.
- `id` аккаунта не меняется никогда: переименование правит только `label`.
- `Backend::call` получает `CallContext` (аккаунт, модель, `ToolMode`); `request.model` игнорируется.

## 2. Закон файлов

Один файл — один владелец. Чужие файлы правятся только строками из таблицы «общие файлы».

| Дорожка | Волна | Владеет | Реализует |
|---|---|---|---|
| **SECRETS** | 2 | `src/secrets/**`, `src/config/**`, `src/cli/**` (подкоманды `secrets`, `doctor`), `src/commands/defs/secrets.rs` | `SecretStore`, слои конфига, `toml_edit`, миграция `[[providers]]` → `GatewayConfig::from_legacy`, встраивание `GatewayConfig` в `Config`, `SecretResolver`, чистка логов, `doctor` |
| **GW-ENGINE** | 2 | `src/gateway/{engine,state,routed,ledger,api_backend,deepseek_backend,clock}.rs`, `src/provider/model_cache.rs`, тело `provider::build_provider` | `impl Gateway`, `RoutedProvider`, выбор и fallback, cooldown, семафоры, учёт, события, `Session.route_state` |
| **GW-CLI** | 2 | `src/gateway/cli/**`, `src/harness/mock_cli.rs`, `src/harness/mod.rs` (одна строка `mock-cli`) | `impl Backend` для Claude и OpenCode, поиск бинарей, процесс, `mock-cli` |
| **GW-TUI** | 3 | `src/tui/**`, `src/app/keys/**`, `src/app/gateway_view.rs`, `src/commands/defs/{accounts,routes,roles,usage,providers}.rs` | экран аккаунтов/маршрутов, панель лимитов, `/pin`, мастер добавления |
| **GW-SERVE** | 3 | `src/server/**`, `src/app/serve.rs`, `src/main.rs::run_acp_server` | `/serve` через `Gateway`, `auto/<route>`, заголовки `x-pooprusteek-*`, статус-эндпоинт, ACP |
| **GW-ROLES** | 3 | `src/app/{multichat,goal,titles}.rs`, `src/agent/sub_agent.rs`, `src/agent/runner.rs::context_used` | роли `title/summary/evaluator`, заголовки чатов, реальное заполнение окна, `Session.title` |

Общие файлы и порядок правок:

| Файл | Кто правит | Что именно |
|---|---|---|
| `src/config/mod.rs` | только SECRETS | поля `Config`: `gateway`, `accounts`, `routes`, `roles` (типы из `gateway::types`) |
| `src/app/events.rs`, `src/app/mod.rs` | только ENGINE | `AppEvent::Gateway(GatewayEvent)`, поле `gateway: GatewayHandle`, диспетчер; остальные дорожки работают в своих файлах |
| `src/session.rs` | ENGINE (`route_state`), затем ROLES (`title`) | два разных поля, `#[serde(default)]`, тест «старый файл грузится» |
| `src/commands/mod.rs` | GW-TUI | регистрация команд; SECRETS регистрирует `secrets` одной строкой после слияния TUI или до него в отдельном коммите |
| `src/harness/mock.rs` | ENGINE | скрипты на аккаунт (`/a/v1`, `/b/v1`): сейчас `route` срезает только `/v1` (критик A10) |
| `Cargo.*` | никто | новых зависимостей нет (`toml_edit` добавит SECRETS, решение владельца) |
| `.memories/*` общие | только интегратор | журналы — свои файлы |

Порядок слияния волны 2: ENGINE и CLI не пересекаются по файлам и встречаются на `Backend`; SECRETS
встречается с ENGINE на `SecretResolver` (ниже). Для параллельной работы ENGINE использует заглушку
`EnvOnlyResolver`, SECRETS её потом заменяет.

```rust
// контракт для SECRETS ↔ ENGINE (определяет SECRETS в src/secrets/)
pub trait SecretResolver: Send + Sync {
    /// Разрешается при сборке адаптера, не при разборе конфига. Ошибка называет имя, не значение.
    fn resolve(&self, reference: &SecretRef, account: &AccountId) -> Result<Secret, SecretError>;
}
```

## 3. Требования по дорожкам

### SECRETS
- Строка из `secret` разбирается в свой `enum` (`keyring:`, `env:`, `cmd:`, `file:`, `none`); форму уже проверяет `SecretRef::parse`.
- **Ошибка разбора TOML печатает строку исходника вместе со значением** (проверено тестом `a_secret_that_is_not_a_reference…`): нигде не выводить `toml::de::Error` целиком, брать `message()`.
- Один `deepseek-web`, токен остаётся в `[provider]`; `api_key` в аккаунте — наследие, `doctor` ругается.
- Проектный слой: аккаунт из проекта недоверенный, `Credential::DirectToken` и `cmd:`/`file:` там запрещены.
- Прямой токен: уже запрещён для Claude и требует `direct_token_ack` в `GatewayConfig::validate`; не ослаблять.

### GW-ENGINE
- `RoutedProvider::native_tools()` отвечает **константой маршрута** (`Route::tool_mode`): `agent/runner.rs` читает её один раз за ход. Адаптер подчиняется `CallContext.tool_mode`: в промптовом режиме API-адаптер не шлёт `tools`.
- Шаг с `!Capabilities::usable_for_tool_turn(mode)` пропускается (`SkipReason::NotPureModel/ToolModeMismatch`). CLI с `pure_model = false` не берётся никогда.
- Fallback только пока `EmitTracker::emitted() == false`; чанки без текста и вызовов (живые сигналы, расход) не считаются.
- `fork()` и суб-агенты делят движок и **семафор на аккаунт** (`Account::max_concurrency`: CLI и DeepSeek 1, API 4); занятый первый шаг не ждёт, а пропускается (`SkipReason::Busy`), очередь только если больше некого.
- Смена цели в беседе: `history::flatten_for_switch` до первого вызова, новый `Backend::fork()`, серверная сессия DeepSeek уходит в `discard_session`. `provider_state` не переносить.
- Cooldown: `Retry-After` соблюдать всегда, успешный вызов снимает его, после перезапуска `StateHint` — только подсказка (порядок «в конец»), решает первый вызов.
- Читать поток бэкенда **до конца**, даже когда получатель остановился на `finish_reason` (`collect_stream` выходит сразу, ждёт задачу `AFTER_STOP_GRACE` = 5 с): чанк расхода OpenAI приходит позже.
- Все точки сборки провайдера заменяют вызов на `gateway::handle::main_provider(&config, gateway)`: `app/mod.rs` (`new`, `rebuild_provider`), `app/keys/onboarding.rs` (передаёт TUI), `harness/driver.rs`, а `model_cache::refresh` ходит через живые `Backend::list_models`.
- `win_job` не трогать: он нужен CLI-адаптеру (`tools::background::win_job`).

### GW-CLI (по `06-cli-spike.md`)
- Claude: `claude -p --output-format stream-json --verbose --include-partial-messages --restricted --tools "" --disable-slash-commands --strict-mcp-config --permission-mode dontAsk --permission-prompts none --no-session-persistence --system-prompt-file <файл> --model <m>`; промпт через stdin; пустая рабочая папка; из окружения убрать `CLAUDE_*` родителя; `CLAUDE_CONFIG_DIR` = `home`. Минимальная версия 2.1.248 (иначе `--safe-mode`). Тест на argv обязателен.
- OpenCode: `--standalone` обязателен, `XDG_CONFIG_HOME` = пустая папка адаптера, `OPENCODE_CONFIG_CONTENT` с `permission: {"*":"deny","skill":"deny"}` и своим агентом, `XDG_DATA_HOME` = `home`. Бинарь запускать по пути `…\@opencode\cli\bin\opencode.exe`, не через npm-шим.
- Codex: `pure_model = false`, в ходы с инструментами не берётся; живой прогон нужен владельцу.
- Дельты текста: только Claude (`streaming = true`); `context_window` только у Claude (`modelUsage[…].contextWindow`); лимиты: `rate_limit_event.unifiedWindows` → `LimitSnapshot` с источником `CliEvent`.
- Классификатор: Claude `is_error` + текст (`Not logged in` → `Auth`; `subtype` у ошибки тоже `success`); OpenCode — событие `error` и код выхода. Нативные вызовы несуществующих инструментов (`tool_use` со статусом `error`) игнорировать.
- Холодный старт 3–4 с (`slow_start = true`); процесс убивать деревом после `result`/`step_finish(stop)` и по сторожу простоя; `kill_on_drop` на Windows недостаточен.
- `mock-cli` проверяет: успех, 429, обрыв, «не вошёл», молчание дольше сторожа, внук-процесс.

### GW-TUI
- Читает только `Gateway::snapshot()`, пишет только `apply_edit()`; рендер ничего не мутирует.
- Показывает `Decision::explain()`, у лимитов — возраст и источник, у оценок `~`.
- Мастер: предупреждения из `GatewayConfig::validate()`; `DIRECT_TOKEN_WARNING` перед включением прямого токена; для CLI два пути: «свой вход» (папка `home`) и «общий вход» с предупреждением о выходе из сессии.
- Нужна команда `/pin` (чат на конкретный аккаунт без смены глобального маршрута).

### GW-SERVE
- Модели: `provider_for_model_id`; ошибки уже типизированы (`UpstreamError::from_class`), честные коды и `Retry-After` есть.
- Подписка Claude наружу не раздаётся: `CallSource::Serve` не выбирает `cli:claude`.
- Расход стрима: `CompletionChunk.usage` (для OpenAI приходит после `finish_reason`); `stream_options.include_usage` клиента управляет только тем, что уходит наружу.
- ACP (`main.rs::run_acp_server`) и `server/proxy.rs` собирают провайдера мимо `build_provider`: перевести на `main_provider`.

### GW-ROLES
- `provider_for_role(Role::Title|Summary|Evaluator, CallSource::Role(..))`; без роли шлюз возвращает `None`, вызывающий остаётся на прежнем пути.
- Роли `title`/`summary` не ставить на `slow_start`-аккаунты.
- `Session.title: Option<String>` и тест «сохранение не затирает заголовок» (`derive_title` — запасной).

## 4. Что не входит в релиз
`spread`, маршруты внутри маршрутов, активные пробы, `Draining`/half-open, режим агента для CLI, lock-файл ledger, роль `subagent`.
