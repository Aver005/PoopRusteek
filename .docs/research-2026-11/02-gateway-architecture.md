# 02 — Gateway: аудит, рынок, архитектура, этапы

Дата: 2026-11-06. Автор: design sub-agent. Код проекта не менялся, приложение не запускалось.
Метки: **[код]** — проверено чтением исходников; **[веб]** — проверено на первоисточнике (ссылка рядом);
**[гипотеза]** — не проверено, нужно проверить в первом шаге этапа.

---

## 0. Коротко

1. Технически «два Codex» уже можно: две записи `[[providers]]` с разными именами (`validate_name` проверяет только уникальность имени). Но это **две несвязанные записи**: нет семейства провайдера, нет порядка, нет цепочек, активна ровно одна на всё приложение (`Config.active_provider`), а DeepSeek web — синглтон (`[provider]`).
2. Главные дыры, из-за которых агрегатор сейчас невозможен:
   - ошибка апстрима теряет тип: `CompatClient::send` превращает любой не-2xx в `AppError::Provider("{status}: …")` — статус, `Retry-After` и заголовки лимитов пропадают; **429 от OpenAI/Anthropic в `/serve` уходит клиенту как 502** (маппинг есть только для `AppError::RateLimited`, а его рожает лишь DeepSeek);
   - usage из стрима не приходит: `CompletionChunk` без usage, `stream_options.include_usage` нигде не ставится, `message_delta.usage` у Anthropic игнорируется; `Usage` — три числа без кэша и reasoning;
   - `request.model` у compat-клиентов игнорируется (берётся `entry.model`) — модель на вызов и «роль» не задать;
   - `/serve` строит провайдера на каждый запрос (`build_entry_provider` → новый `reqwest::Client`), снимает настройки при запуске (`ServerSettings`), на голое имя модели берёт первую подходящую запись.
3. Решение: **не переписывать `LLMProvider`**, а поставить за ним фасад `RoutedProvider` (новый модуль `src/gateway/`). Цикл агента (`run_agent_loop`), компакция, `/serve` видят тот же трейт. Под фасадом — трейт `Backend` (API-адаптеры = обёртка над `CompatClient`, CLI-адаптеры = процессы `codex`/`claude`/`opencode`), движок маршрутов, учёт.
4. Подписки (Codex/Claude/…): **только через официальный бинарь пользователя**, не через копирование OAuth-токенов. Anthropic в документации прямо запрещает вторым приложениям маршрутизировать запросы через учётные данные подписки и хранить их токены, но разрешает пользователю входить в **немодифицированный** Claude Code [веб: code.claude.com/docs/en/legal-and-compliance]. Это даёт и юридически чистый путь, и данные по лимитам из самих CLI.
5. План: этап 0 (контракты, один агент) → волна 1 (3 агента: API-бэкенды+секреты / движок / CLI-бэкенды) → волна 2 (3 агента: TUI / `/serve` / роли+заголовки+учёт в чатах) → интеграция. Новых зависимостей нет.

---

## 1. Аудит текущего состояния

### 1.1 Что уже есть

| Что | Где (структура / функция) | Состояние |
|---|---|---|
| Запись провайдера | `config::ProviderEntry { name, base_url, api_key: Option<String>, model, protocol, tools }` | один ключ на запись, ключ в `config.toml` открытым текстом (0600 только на unix, `config::save`) |
| Имя | `app::providers::validate_name` | уникально по `entry.name`, `deepseek` зарезервировано. Поэтому две записи одного вендора возможны |
| Активный | `Config.active_provider: Option<String>`, `Config::active_provider_entry`, `provider::build_provider` | **один на приложение**; смена = `App::rebuild_provider`, все беседы форкаются от него |
| DeepSeek web | `Config.provider` (`ProviderConfig`, один `token`), `deepseek::DeepseekProvider` | синглтон; своя сессия `SessionState`, PoW, свой rate-limit (`/rate`), ретраи `http.rs` |
| Протоколы | `provider::compat_client::{CompatProtocol, CompatClient<P>}` + `openai_/anthropic_/gemini_compat` | stateless, `fork()` = копия конфига. Хороший шов: транспорт один, различия — в `CompatProtocol` |
| Трейт | `provider::LLMProvider` (`complete`, `complete_stream`, `fork`, `list_models`, `context_window`, `session_identity`, `keeps_server_side_history`, …) | уже содержит всё про сессии и `fork()`; расширяется default-методами без правки реализаций |
| Токены | `provider::Usage {prompt, completion, total}`; `CompletionResponse.usage: Option<Usage>` | только для блокирующего ответа; стрим usage не несёт; DeepSeek считает оценкой (`estimated_usage`, `budget_tokens_for_chars`) |
| Окно контекста | `LLMProvider::context_window` → в `CompatClient` заново ходит в `GET /models` и ищет `context_length` | работает лишь у OpenRouter-подобных; результат не кэшируется |
| Каталог моделей | `provider::model_cache::ProviderModelCache` (`data_dir/provider_models.json`, `Vec<String>` на запись) | только id; нет контекста, лимитов вывода, возможностей, цен |
| Ошибки | `error::AppError::{Provider(String), RateLimited(String), Http, …}` | `RateLimited` создаёт только DeepSeek (`deepseek/http.rs`, `deepseek/mod.rs::drive_completion`) |
| Повторы | `agent::retry::RetryBudget` | это повторы **ответа модели** (битый `<tool_use>`, пустой ответ), к транспорту не относится. Транспортные ретраи есть только у DeepSeek (`send_json_request`, `drive_completion`: одна повторная попытка на `RateLimited`, backoff без jitter, `Retry-After` не читается) |
| Лимиты | `deepseek::http::enforce_rate_limit` (мин. интервал + окно 60 с) | только DeepSeek; у API-записей нет ничего |
| Сервер | `server::{catalog, http, openai, proxy}`, `ServerSettings`, `ServerContext`, `ServerStats` | OpenAI Chat/legacy/embeddings; `ServerApi::{Anthropic,Gemini}` зарезервированы (501) |
| Маршрутизация id | `server::catalog::resolve_model` | `<entry>/<model>`, голое имя записи, голая модель → **первая** запись, у которой она есть |
| Служебный текст | `context::compact::compact` (суммаризатор на `provider.fork()` той же модели), `app::goal` (оценщик, `provider.fork()`), `session::derive_title` | заголовок чата — эвристика без модели; суммаризатор и оценщик берут ту же модель, что чат |
| Секреты | `keyring` + `mcp::oauth_store` (обход `ensure_default_store`) | есть только для MCP OAuth |
| Тестовый стенд | `harness::mock` (`mock-provider`: `status`, `delay_ms`, скрипт ответов) | нет заголовков, usage, обрыва посреди стрима, нескольких «аккаунтов» |

### 1.2 Где упираемся

1. **Нет слоя «аккаунт».** `ProviderEntry` смешивает вендора, endpoint, ключ и модель. Нельзя сказать «Codex — работа / Codex — личный» как одно семейство с общими моделями и разными подписками.
2. **Нет слоя «маршрут».** Ничто не говорит «если A исчерпан — иди в B». Fallback невозможен даже в виде заготовки.
3. **Ошибка без типа.** `AppError::Provider(String)`: чтобы отличить «подожди» от «ключ сдох» от «контекст велик», пришлось бы разбирать текст. `stream::StreamEnd::ProviderError(String)` и `StreamVerdict::Failed(String)` стрингифицируют дальше. Движку нужен `ErrorClass`.
4. **Заголовки и usage теряются на границе `CompatClient::send`.** Они доступны именно там (`response.headers()`), и больше нигде.
5. **Модель привязана к записи.** `CompatClient.model` фиксируется в `new`; `TurnSpec::background`, `goal.rs` кладут в `CompletionRequest.model` строку DeepSeek-секции (`config.provider.model`) — безвредно лишь потому, что её игнорируют.
6. **`Session.model_type = config.provider.model`** — для записи-не-DeepSeek это неверная подпись; в сессии нет поля «аккаунт/маршрут», а `provider_session_id` понятен только DeepSeek.
7. **Сервер не живой.** Снимок настроек при старте (комментарий в `server/mod.rs`: изменения применяются после `/serve off|on`), провайдер на запрос, счётчики — два атомика. Нет общего состояния с TUI.
8. **Состояние нигде не живёт.** Нужен долгоживущий объект, переживающий `fork()` и запросы `/serve`: статусы аккаунтов, cooldown, счётчики. Сейчас такого нет (DeepSeek-лимитер — единственный пример: `ServerContext.deepseek`, «один лимитер на весь серверный трафик»).

### 1.3 Что уже проброшено / что теряется

| Данные | Проходит? |
|---|---|
| Текст, `finish_reason`, нативные tool-calls | да |
| `usage` блокирующего ответа | да (`CompletionResponse.usage`, `/serve` отдаёт настоящий, если он есть) |
| `usage` стрима (OpenAI `include_usage`, Anthropic `message_start`/`message_delta`, Gemini `usageMetadata`) | **нет** — `/serve` в стриме считает оценку |
| кэш-токены, reasoning-токены | **нет** (в `Usage` нет полей) |
| `Retry-After`, `x-ratelimit-*`, `anthropic-ratelimit-*` | **нет** (сняты бы в `CompatClient::send`) |
| HTTP-статус ошибки | только как текст `"{status}: …"` |
| Реальный размер контекста | нет (оценка `conversation_tokens`; `session_tokens()` только у DeepSeek) |
| Задержка (TTFT, длительность) | не измеряется |

---

## 2. Рынок (проверено)

Звёзды/активность — `gh api`, 2026-11-06.

### 2.1 LiteLLM — BerriAI/litellm (≈62.8k★, лицензия в API: `NOASSERTION`; есть `enterprise/`-часть — код не копируем)

Сильное [веб: context7 `/websites/litellm_ai`, docs.litellm.ai/docs/routing, /proxy/reliability, /proxy/config_settings]:
- `fallbacks: [{"a": ["b"]}]` на уровне модели; `order: 1,2,3` у деплойментов — сначала все `order=1`, потом `order=2`, **у каждого уровня свои ретраи**, потом `fallbacks`;
- `cooldown_time`, `allowed_fails`, а также `allowed_fails_policy` и `retry_policy` **по типам ошибок** (Auth/Timeout/RateLimit/InternalServer/…);
- стратегии `simple-shuffle | least-busy | usage-based-routing | latency-based-routing`; `rpm/tpm` на деплоймент;
- `enable_pre_call_checks` — отсев деплойментов по окну контекста;
- бюджеты `max_budget` + `budget_duration` на ключ/команду/тег, несколько окон сразу (`budget_limits`: 24h и 30d).

Боли пользователей (issues, прочитаны):
- [#34399](https://github.com/BerriAI/litellm/issues/34399): `Router._time_to_sleep_before_retry` читает `Retry-After` только если деплоймент **один**; при нескольких возвращает 0 и ретраит мгновенно.
- [#15526](https://github.com/BerriAI/litellm/issues/15526): под постоянными 429 прокси не успевает отвечать на readiness-пробы, поды перезапускаются — каскад.
- Память: [#12685](https://github.com/BerriAI/litellm/issues/12685) (61 комментарий), [#27954](https://github.com/BerriAI/litellm/issues/27954) (открыт), [#14384](https://github.com/BerriAI/litellm/issues/14384) (новые SSL-соединения на каждый вызов — родственно нашему «клиент на запрос»).
- Fallback посреди стрима: в коде есть `MidStreamFallbackError` (поля `generated_content`, `is_pre_first_chunk`, `emitted_disqualifying_content`), и вокруг него открытые баги: [#49795](https://github.com/BerriAI/litellm/issues/49795) (префикс из одних рассуждений считается «первым чанком» — fallback теряется, хотя клиент ничего не получил), [#35578](https://github.com/BerriAI/litellm/issues/35578) (fallback берётся по выбранному деплойменту, а не по запрошенной группе — вся цепочка теряется), [#39462](https://github.com/BerriAI/litellm/issues/39462) (usage при fallback теряется), [#45645](https://github.com/BerriAI/litellm/issues/45645) (429 посреди стрима записывается на fallback-деплоймент и «остужает» его).

**Чем обойти:** `Retry-After` уважать всегда; cooldown приписывать тому аккаунту, который ответил; «первый чанк» = первый ушедший наружу байт; usage — отдельным путём, не зависящим от fallback; один статический бинарь без Python/Redis; состояние виден в TUI.

### 2.2 OpenRouter — provider routing

[веб: openrouter.ai/docs/features/provider-routing.md, прочитано целиком по ключевым полям]
Поля: `order`, `only`, `ignore`, `allow_fallbacks` (по умолчанию `true`), `require_parameters`, `data_collection`, `zdr`, `sort` (`price|throughput|latency`, объект с `partition: "model"|"none"`), `preferred_min_throughput` (процентили p50/p75/p90/p99), `max_price`. Ярлыки `:nitro` = сортировка по throughput, `:floor` = по цене. Пороги производительности — **предпочтение, не запрет**; `max_price` — запрет.

Берём: `allow_fallbacks=false` как «закрепить»; `require_parameters` → фильтр по возможностям (tools/vision/окно) **до** fallback; различие «предпочтение/запрет» в порогах.

### 2.3 Portkey — Portkey-AI/gateway (13.3k★, MIT, последний push 2026-05-25)

[веб: portkey.ai/docs/product/ai-gateway/fallbacks.md] Конфиг `strategy.mode = fallback | loadbalance | conditional`, `targets[]`, по умолчанию fallback срабатывает на **любой не-2xx**, сужается `on_status_codes: [429, 503]`; цели вкладываются (балансировщик внутри fallback). Circuit breaker есть, но [issue #1289](https://github.com/Portkey-AI/gateway/issues/1289): после открытия цепи запросы всё ещё идут сначала в «больную» цель — breaker не отфильтровал её. Урок: breaker надо тестировать сценарием «после N падений N+1-й запрос не касается больной цели».

### 2.4 new-api / one-api (QuantumNous/new-api 51.5k★, **AGPL-3.0** — код не берём; songquanpeng/one-api 37k★, MIT, последний push 2026-01)

- new-api: каналы с `priority` (выше — раньше) и `weight` (случайный выбор внутри уровня); ретраи спускаются по уровням приоритета; мульти-ключ в канале с автоотключением отдельных ключей [веб: README строка «channel priorities and weights, retries, channel affinity, and multiple upstream keys»; детали приоритета/ретрая — со вторичных зеркал, **[гипотеза]**].
- Боль: [#3537](https://github.com/QuantumNous/new-api/issues/3537) — автоотключённые ключи в режиме `multi_to_single` **никогда не включаются обратно**; канал деградирует молча, пока не умрёт целиком. Урок: у состояния «отключён системой» обязан быть путь возврата (проба) и видимое предупреждение.
- one-api: выбор канала суффиксом ключа `TOKEN-CHANNEL_ID` [веб: README, строка 348] — удобный приём «закрепить аккаунт»; у нас это префикс `account/model`.

### 2.5 CLIProxyAPI — router-for-me/CLIProxyAPI (57.8k★, MIT, push сегодня)

[веб: README и `config.example.yaml` с первоисточника] Оборачивает Codex, Claude Code, Gemini/Antigravity, Kimi, Grok, Devin и др. подписки в OpenAI/Claude/Gemini/Codex-совместимый API, несколько аккаунтов на вендора. В конфиге: `routing.strategy: round-robin | weighted-round-robin | fill-first`, вес в JSON авторизации, `session-affinity` (с приоритетом заголовков Claude Code/Codex/OpenCode, `prompt_cache_key`), `session-affinity-rebind-on: any|quota`, `retry.request-retry`, `max-retry-credentials`, `max-retry-interval`, `cooldown.disable-cooling`, `save-cooldown-status`.

Боли (issues, прочитаны):
- [#1015](https://github.com/router-for-me/CLIProxyAPI/issues/1015): все 17 аккаунтов Antigravity отвечают 429, хотя API квот показывает 60–100% остатка → **«квота» и «реальный отказ» расходятся; доверять нельзя ни одному источнику слепо**.
- [#5404](https://github.com/router-for-me/CLIProxyAPI/issues/5404): сохранённый cooldown (`save-cooldown-status: true`) пережил рестарт и блокировал failover на живой аккаунт ~98 часов, пока вручную не переключили статус. **Устаревшее состояние хуже отсутствия состояния.**
- [#1637](https://github.com/router-for-me/CLIProxyAPI/issues/1637): Google заблокировал 3 аккаунта (403 «violation of Terms of Service»).
- [#2599](https://github.com/router-for-me/CLIProxyAPI/issues/2599): запросы через проксированный OAuth Anthropic считает как «extra usage» (по тарифу API), а не как включённый лимит подписки.
- [#3200](https://github.com/router-for-me/CLIProxyAPI/issues/3200), [#3187](https://github.com/router-for-me/CLIProxyAPI/issues/3187): пользователи требуют вернуть статистику использования (и чтобы она переживала Docker-пересоздание) — статистику сначала убрали.
- [#5545](https://github.com/router-for-me/CLIProxyAPI/issues/5545): Codex Responses SSE умирает после ~30 с тишины апстрима на «глубоких» моделях (15–20% обрывов) — нужны keep-alive и корректный idle-guard.
- [#3530](https://github.com/router-for-me/CLIProxyAPI/issues/3530): 150 комментариев про зависания Codex-аутентификации без ответа (по заголовку; детали не вчитывал).
- Рядом есть десктоп-клиент [EasyCLIProxyAPI](https://github.com/router-for-me/EasyCLIProxyAPI) — сам проект признаёт спрос на GUI-настройку.

**Чем обойти:** не хранить и не проксировать чужие токены (см. §3.4 и решение 1); персистентно хранить только то, что можно перепроверить; статистика — часть ядра; TUI вместо yaml.

### 2.6 Claude Code Router — musistudio/claude-code-router (38k★, MIT, push 2026-10-10)

[веб: README, musistudio.github.io/claude-code-router; детали ролей — со сторонних руководств, **[гипотеза]**] Роутер для Claude Code: ключи `default | background | think | longContext (+порог) | webSearch`, пара `provider,model`. README заявляет «retries, credential pools, key rotation, ordered fallback models» и условия по заголовкам/телу. Но запросы пользователей о fallback/балансировке открыты давно: [#208](https://github.com/musistudio/claude-code-router/issues/208), [#275](https://github.com/musistudio/claude-code-router/issues/275), [#589](https://github.com/musistudio/claude-code-router/issues/589) («один модель — легко 429, добавьте балансировку»).

Берём идею **ролей** (`background` ≈ наши `title/summary`); чем обходим: роли + цепочки + состояние аккаунтов в одном месте, и это же работает внутри приложения, а не только как внешний прокси.

### 2.7 Сводка «чем лучше»

| Область | Аналоги | Наше преимущество |
|---|---|---|
| Состояние | Python/Go-сервис, Redis/БД | один бинарь, состояние в процессе + атомарные снапшоты, видно в TUI |
| Ошибки | строки, часть классов | типизированный `ErrorClass`, таблица «класс → действие», `Retry-After` и 429-без-Retry-After (spend cap) различаются |
| Подписки | копирование OAuth-токенов (ban-риск) | запуск официального CLI; лимиты берутся из самого CLI |
| Лимиты | либо header, либо оценка | единый `LimitSnapshot` с источником и свежестью; устаревшее не участвует в жёстких решениях |
| Внутренние чаты | прокси внешний | тот же движок обслуживает TUI-чаты, под-агентов, служебные роли и `/serve` |
| Роли | `background` в CCR | `title/summary/evaluator/subagent` + свой fallback у каждой |

Проверено про Anthropic: `retry-after` бывает не у всех 429 — **429 «spend cap» не несёт `retry-after`**, и «ретраи, включая автоматические SDK, падают, пока доступ не вернётся» [веб: platform.claude.com/docs/en/api/rate-limits]. Поэтому правило «429 без Retry-After → короткий exponential backoff» неверно; для такого 429 нужен класс `SpendCap` (долгий cooldown + проба).

---

## 3. Архитектура

### 3.1 Принципы

- Шов один: `LLMProvider`. Приложение, цикл агента, компакция, `/serve` получают `Arc<dyn LLMProvider>` как раньше; фасад `RoutedProvider` скрывает цели и fallback.
- Слои: `gateway/` — доменный уровень, рядом с `provider/` и `server/`. `gateway` знает `provider` и `config`; `app/` и `server/` знают `gateway` через публичный API (`GatewayHandle`). `tools/` про gateway не знает (инвариант 6). События в TUI — только `AppEvent::Gateway(GatewayEvent)` (один вариант, как `AppEvent::Agent`).
- Инварианты CLAUDE.md, которые ограничивают дизайн: 1 (никаких `.await` I/O в `handle_event`: проба лимитов, запись ledger — фоновые задачи), 2 (замок `GatewayState` — `std::sync::Mutex`, живёт только на синхронной работе, никогда через `.await`), 3 (данные — через `util::atomic_write`), 4 (вывод процессов CLI — `util::decode_process_output`, без `from_utf8_lossy`), 9 (CPU-тяжёлое — `spawn_blocking`; парсинг JSONL лёгкий, не нужно), 10 (CLI-адаптер не печатает в stdout/stderr приложения: stderr ребёнка перехватывается), 11 (харнесс: никаких обходов согласования; движок не добавляет инструментирования в `run_agent_loop` — телеметрия через `debug_log`), 12 (деградировать, не блокировать: нет keyring → `file`; нет CLI → аккаунт `Unavailable`, остальные работают).
- Правило владельца «процесс без прогресса > 30 с — убить»: CLI-дети запускаются с `kill_on_drop(true)`, сторожем простоя (см. 3.6).
- Новых зависимостей нет: поиск бинарей в `PATH` — 30 строк нативно; keyring уже в `Cargo.toml`.

### 3.2 Модель данных

```
Provider (семейство: тип адаптера)        Account (подключение; ИМЕНУЕМАЯ единица)
  openai | anthropic | gemini | custom      id: slug "codex-work"      label: "Codex · работа"
  deepseek-web | cli:codex | cli:claude     kind: Api{protocol, base_url} | DeepseekWeb | Cli{tool, bin, home}
  | cli:opencode                            secret: keyring:<id> | env:NAME | file | none
                                            enabled, order (положение в списке), limits (заявленные), tools
Model (каталог на аккаунт)                Route (цепочка)
  id, context_window, max_output,           id: "coding"  strategy: ordered | spread
  supports{tools,vision,reasoning},         steps: [Target{account,model} | RouteRef]
  price_in/out (необяз.), source,           triggers: какие ErrorClass запускают переход
  fetched_at                                switch_at_pct: порог «заранее»; require: возможности
Role
  main | title | summary | evaluator | subagent  ->  RouteId | "inherit"
```

Формулировка адресов (одна грамматика везде — TUI, конфиг, `/serve`):
- `account/model` — закрепить аккаунт и модель (`codex-work/gpt-6.1-sol`; совместимо со старым `entry/model`);
- `auto/<route>` — виртуальная модель-цепочка (`auto/coding`);
- голое имя модели → «лучший доступный аккаунт, у которого она есть» (вместо прежнего «первый в списке»).

Именование и ранжирование:
- `id` — slug, стабильный ключ в маршрутах; `label` — любое отображаемое имя. Уникальность `id` глобальная, `label` — в пределах вендора. `/accounts rename` переписывает ссылки в маршрутах одной атомарной записью конфига (тест обязателен).
- Ранг = **порядок в массиве** `[[accounts]]` (и `steps` внутри маршрута). Числовых приоритетов нет: нет коллизий и нет «почему 10 раньше 20»; в TUI `J/K` двигает строку и переписывает массив.
- Предложение имени при подключении CLI: из `whoami`-подобных данных (локальная часть e-mail/название организации), пользователь правит. [гипотеза: что именно отдаёт каждый CLI — проверка в этапе 3.]

### 3.3 Формат конфига и миграция

Новые таблицы (в `src/config/gateway.rs`, в `Config` добавляются три поля с `#[serde(default)]`):

```toml
[gateway]
schema = 1
enabled = true                 # false = старое поведение (на время раскатки)

[[accounts]]
id = "codex-work"
label = "Codex · работа"
provider = "cli:codex"          # openai | anthropic | gemini | custom | deepseek-web | cli:codex | cli:claude | cli:opencode
home = "~/.codex-work"          # изоляция CLI-аккаунта (CODEX_HOME / CLAUDE_CONFIG_DIR …)
enabled = true
[accounts.limits]               # заявленные пользователем, дополняют снимки
soft_pct = 85
hard_pct = 97
budget_usd_month = 50

[[accounts]]
id = "lmstudio"
provider = "openai"
base_url = "http://localhost:1234/v1"
secret = "none"
model = "qwen2.5-coder"          # модель по умолчанию аккаунта

[[routes]]
id = "coding"
strategy = "ordered"
switch_at_pct = 90
on = ["rate_limit", "quota", "spend_cap", "overloaded", "timeout", "server_error", "auth"]
steps = [
  { account = "codex-work",     model = "gpt-6.1-sol" },
  { account = "codex-personal", model = "gpt-6.1-sol" },
  { route = "cheap" },
]

[roles]
main = "coding"
title = "cheap"
summary = "cheap"
evaluator = "coding"
```

**Миграция** (чистая функция `config::gateway::from_legacy(&Config) -> (Vec<Account>, Vec<Route>, Roles)`, зовётся из `config::parse` рядом с `apply_migrations`, использует уже существующий `migration_notices`):
- каждая `[[providers]]` → `Account` (`id = name`, `provider` из `protocol`: openai/anthropic/gemini, `base_url`, `model`, `tools`, ключ остаётся `secret = "file"` с инлайновым `api_key` — как сейчас);
- `[provider]` с токеном → `Account { id = "deepseek", provider = "deepseek-web" }`; без токена — не создаётся;
- `active_provider` → `roles.main` = неявный маршрут из одного шага;
- `server.*` остаётся как есть. Старые id `entry/model` в `/serve` продолжают работать (id аккаунта = старое имя записи).
- до первого сохранения файл не трогаем; перед первой записью новых таблиц один раз делается копия `config.toml.pre-gateway` (`atomic_write`). После этого `[[providers]]` не пишется (решение 3).
- тесты: золотые файлы «старый конфиг → ожидаемые accounts/routes/roles», круговой прогон (parse → save → parse), конфиг без новых таблиц, конфиг с обеими формами одновременно (новые таблицы выигрывают, уведомление).

Модель `ProviderModelCache`: версия файла 1→2, значения `Vec<String>` → `Vec<ModelInfo>` с `#[serde(default)]` и чтением старой формы (кэш пересобираем, как сегодня при порче — «пересобираемый кэш»).

### 3.4 Секреты

Новый модуль `src/secrets/` (общий ключ-хранилищный слой). Кандидаты `SecretRef`: `keyring:<id>`, `env:NAME`, `file` (инлайн, как сейчас), `none`.
- keyring-инициализацию берём из `mcp::oauth_store::ensure_default_store` (вынести в `secrets::store`; в `oauth_store.rs` — однострочный вызов, но менять его в этом релизе не обязательно — тогда дублируем 20 строк и отмечаем TODO, чтобы не создавать конфликт слияния).
- Харнесс/Docker/Linux без secret-service: `keyring` недоступен → автоматически `file`, предупреждение один раз (инвариант 12). Сервис в keyring включает хэш `data_dir`, чтобы `--data-dir` прогон не читал реальные ключи пользователя.
- Старые инлайновые ключи не переезжают молча: команда `/accounts secure [id|all]` предлагает перенос (решение 4).
- Для CLI-аккаунтов секрета нет вообще: авторизацией владеет сам CLI в своей `home`.
- Ключи не попадают в `debug_log`, `/serve` ответы и сообщения об ошибках (в `UpstreamError::from_app` уже вырезается URL апстрима — оставить).

### 3.5 Трейт адаптера

Адаптер — не `LLMProvider`: у него другой контракт (типизированный итог, вызов приписан аккаунту, свежий сеанс по запросу).

```rust
// src/gateway/backend.rs
#[async_trait]
pub trait Backend: Send + Sync {
    fn kind(&self) -> BackendKind;                  // Api | DeepseekWeb | Cli(tool)
    fn capabilities(&self) -> Capabilities;         // tools_native, streaming, usage_in_stream, reports_limits, server_side_history

    /// Один вызов модели. Чанки уходят в `tx` по мере прихода; итог типизирован.
    async fn call(&self, req: CompletionRequest, model: &str,
                  tx: UnboundedSender<CompletionChunk>) -> Result<CallReport, BackendError>;

    async fn list_models(&self) -> Result<Vec<ModelInfo>, BackendError>;
    /// Дёшево и без расхода токенов, если источник это позволяет (CLI-проба, `GET /models`).
    async fn probe(&self) -> ProbeReport;           // состояние авторизации, LimitSnapshot?, версия CLI
    /// Свежая сессия того же аккаунта (аналог `LLMProvider::fork`).
    fn fork(&self) -> Arc<dyn Backend>;
    // сессионные методы (adopt/check/discard/reset) — default-заглушки, как в LLMProvider
}

pub struct CallReport {
    pub usage: UsageDetail,            // input, output, cache_read, cache_write, reasoning, estimated: bool
    pub limits: Option<LimitSnapshot>, // из заголовков / события CLI
    pub finish_reason: Option<String>,
    pub ttft: Option<Duration>, pub elapsed: Duration,
    pub upstream_id: Option<String>,   // request-id для разбора жалоб
}

pub struct BackendError {
    pub class: ErrorClass, pub status: Option<u16>, pub message: String,
    pub emitted: bool,                 // ушёл ли в `tx` хоть один чанк (разрешён ли failover)
    pub limits: Option<LimitSnapshot>,
}

pub enum ErrorClass {
    RateLimit { retry_after: Option<Duration> },
    QuotaExhausted { resets_at: Option<SystemTime> },
    SpendCap,                          // 429 без Retry-After (Anthropic spend cap) — долгий cooldown + проба
    Overloaded, Timeout, Server, Network,
    Auth, AccountBlocked,              // 401 / 403-«нарушение ToS», account_on_hold, billing_error
    ContextTooLong, ModelNotFound, ContentFilter, BadRequest,
    Cancelled, Protocol,
}
```

Реализации:
1. `ApiBackend<P: CompatProtocol>` — **тонкая обёртка вокруг существующего `CompatClient`**: транспорт не переписывается. Добавляются: возврат `(headers, status)` из `send`, `CompatProtocol::parse_limits(&HeaderMap) -> Option<LimitSnapshot>` (default `None`; OpenAI: `x-ratelimit-{limit,remaining,reset}-{requests,tokens}` [веб: platform.openai.com/docs/guides/rate-limits]; Anthropic: `anthropic-ratelimit-{requests,tokens,input-tokens,…}-{limit,remaining,reset}` + `retry-after` [веб: platform.claude.com/docs/en/api/rate-limits]; Gemini — **[гипотеза]**, в первом этапе проверить), разбор usage в стриме, `model` из аргумента `call`, один общий `reqwest::Client` на аккаунт.
2. `DeepseekBackend` — обёртка над `DeepseekProvider` (его `fork()`/сессия/PoW/лимитер не трогаем). Usage — оценка с флагом `estimated = true`.
3. `CliBackend` — см. 3.6.

`LLMProvider` получает два default-метода (не ломают реализации): `fn last_call(&self) -> Option<CallSummary>` (итог последнего вызова: аккаунт, модель, usage, был ли fallback) и `fn route_status(&self) -> Option<RouteStatus>`. `Usage` расширяется полями с `#[serde(default)]` (`cache_read`, `cache_write`, `reasoning`) + `Default`.

### 3.6 CLI-адаптеры (Codex / Claude Code / OpenCode / …)

Что проверено:
- Claude Code: `claude -p` с `--output-format json|stream-json`; JSON содержит `total_cost_usd` и разбивку по моделям (клиентская оценка, может отличаться от счёта); события `system/init`, `system/api_retry` с полем `error` из списка `authentication_failed | account_on_hold | billing_error | rate_limit | overloaded | …`, `session_id`, `--resume` [веб: code.claude.com/docs/en/headless]. **`--bare` не читает OAuth и keychain** — для подписки нужен обычный режим. Изоляция аккаунта — `CLAUDE_CONFIG_DIR` [веб: code.claude.com/docs/en/env-vars]. Событие `rate_limit_event` описано только на сторонних страницах — **[гипотеза]**.
- Codex: `codex app-server` — JSON-RPC по stdio; в исходниках протокола есть `account/rateLimits/read` и уведомление `account/rateLimits/updated` (файлы `app-server-protocol/src/protocol/common.rs`, `…/AccountRateLimitsUpdatedNotification.ts`) [веб: github.com/openai/codex]; форма ответа по сторонним источникам: окна `primary`/`secondary` с `usedPercent`, `windowDurationMins`, `resetsAt` — **[гипотеза]**, точная схема снимается командой генерации схемы своей версии (имя команды проверить `--help`). `CODEX_HOME` встречается в исходниках (изоляция аккаунта) — формат использования проверить. На этой машине `codex` **не установлен** (`which codex` пусто), `claude` и `opencode` установлены.
- OpenCode: `opencode run --format json`, `opencode serve`, `opencode acp`, `opencode auth login|list`, ключи в `~/.local/share/opencode/auth.json` [веб: opencode.ai/docs/cli]; несколько аккаунтов одного провайдера и изоляцию по каталогу данных — **[гипотеза]**.
- ACP (Agent Client Protocol): реестр агентов включает Claude Agent, Codex, Gemini CLI, OpenCode [веб: agentclientprotocol.com/registry]. В проекте уже есть **ACP-сервер** (`--acp`), клиента нет. Отдаёт ли ACP usage/лимиты — **[гипотеза]**.

Два режима CLI-аккаунта:
- **completion** — CLI как «текст в текст». Наши инструменты едут через уже существующий промптовый протокол `<tool_use>` (тот же путь, что у DeepSeek web: инструменты нативно CLI не объявляются). Годится для чатов, ролей `title/summary/evaluator`, `/serve` без клиентских tools. Минусы: старт процесса на каждый вызов, системный промпт самого CLI, нужно, чтобы CLI не вызывал свои инструменты — как именно (флаги/режим) — **[гипотеза]**, первая задача этапа 3 (спайк по каждому CLI).
- **agent** — делегирование целой задачи CLI (его цикл, его инструменты). Отдаётся как инструмент/под-агент `task`-подобный и как «агентная» модель в `/serve` без tools. Позже (после спайка).

Процессная дисциплина: `tokio::process::Command` с `kill_on_drop(true)`; JSONL читается построчно; сторож: нет события дольше N секунд (по умолчанию 90, как `PROVIDER_READ_TIMEOUT`) → процесс убит, `ErrorClass::Timeout`; вывод ошибок — через `util::decode_process_output` целиком; stderr ребёнка не уходит в терминал TUI. Один **долгоживущий** процесс на аккаунт (app-server у Codex, `opencode serve`) допустим, если спайк покажет выигрыш; иначе процесс на вызов. Тесты — без реальных CLI: подкоманда `pooprusteek mock-cli --dialect codex|claude|opencode` (по образцу `mock-provider`).

Легальность: адаптер **никогда не читает и не копирует** файлы авторизации CLI (`~/.codex/auth.json`, `~/.claude/.credentials.json` и т. п.) и не подсовывает токены своим HTTP-клиентам. Это и граница нашей ответственности (см. решение 1).

### 3.7 Движок маршрутизации

Объект `Gateway` (один на процесс; в TUI создаётся в `App::new`, в `--proxy` — в `server::proxy::run`). Внутри: `Arc<Engine>`; состояние аккаунтов под `std::sync::Mutex` (мелкие синхронные операции), персистентные снимки — фоновая задача.

**Состояние аккаунта**

```
Ready ──ошибка RateLimit──▶ Cooling{until}      ──timeout/проба ok──▶ Ready
Ready ──QuotaExhausted────▶ Exhausted{resets_at?}──проба ok / сброс окна──▶ Ready
Ready ──SpendCap──────────▶ Exhausted{long}     ──проба ok──▶ Ready
Ready ──N ошибок в окне───▶ Open (breaker)      ──half-open: 1 проба──▶ Ready | Open
Ready ──Auth/Blocked──────▶ NeedsAttention      ──пользователь /accounts login / retest──▶ Ready
любое ──пользователь──────▶ Disabled
Ready ──used_pct ≥ soft───▶ Draining            ──used_pct < soft−5 или сброс окна──▶ Ready
```

Правила:
- **cooldown**: `RateLimit{retry_after: Some(d)}` → `until = now + d` (всегда, в т. ч. при нескольких аккаунтах — анти-#34399); `None` → exponential 5 с → 5 мин с jitter; `SpendCap` → час и проба раз в N минут; `QuotaExhausted{resets_at}` → до `resets_at`, но **не доверять слепо**: через `min(resets_at−now, 30 мин)` делается проба/пробный вызов (анти-#5404, анти-#1015).
- **персистентность**: cooldown/Open/Exhausted при рестарте **не восстанавливаются как есть**; восстанавливается только «был Exhausted с `resets_at`» как *подсказка* — первая же проба решает (анти-#5404).
- **breaker**: считает только `Overloaded/Timeout/Server/Network`; открывается после K ошибок за окно, пока открыт — аккаунт **не выбирается вообще** (тест Portkey #1289: после открытия следующий запрос не касается цели).
- **выбор**: фильтр (enabled, не Cooling/Exhausted/Open/NeedsAttention, `require`: окно ≥ входа, tools/vision) → стратегия. `ordered`: первый подходящий по порядку шагов, `Draining` — после тех, кто ниже порога, но **до** исчерпанных. `spread`: взвешенный по остатку/least-used окна. Если не остался никто — `RouteExhausted` с самым ранним `until` в тексте ошибки и в `Retry-After` для `/serve`.
- **порог «заранее»**: `switch_at_pct` маршрута (по умолчанию 90) и `soft_pct` аккаунта; входной сигнал — `LimitSnapshot` с `fresh` (возраст ≤ TTL, источник Header/CliProbe). `Ledger`-оценка и заявленный бюджет работают как *мягкий* сигнал (Draining), но не как *жёсткий* (Exhausted). Сигнал устарел → не участвует (анти-#5404).
- **липкость**: беседа держится за свой аккаунт (кэш промпта у OpenAI/Anthropic привязан к ключу; у CLIProxyAPI тоже есть session-affinity [веб: config.example.yaml]), пока аккаунт `Ready`; смена по состоянию, а не по счётчику.
- **шаги цепочки**: у каждого шага собственные ретраи (как `order` у LiteLLM); `Route`-шаг рекурсивен, глубина ≤ 4, цикл ловится при валидации конфига.
- **таблица «класс → действие»**:

| класс | аккаунт | запрос |
|---|---|---|
| RateLimit | Cooling | следующий шаг |
| QuotaExhausted / SpendCap | Exhausted | следующий шаг |
| Overloaded / Server / Network / Timeout | счёт для breaker | следующий шаг (до 1 повтора на этом же, если `retry_after` мал) |
| Auth / AccountBlocked | NeedsAttention + событие в TUI | следующий шаг |
| ContextTooLong | — | следующий шаг, где `context_window` больше; иначе ошибка |
| ModelNotFound | пометить модель недоступной у аккаунта | следующий шаг |
| BadRequest / ContentFilter | — | **без** fallback (везде будет то же) |
| Cancelled | — | стоп |

**Идущий поток.** `RoutedProvider::complete_stream` оборачивает `tx`: пока ничего не ушло наружу — failover прозрачен; как только ушёл первый чанк — failover **запрещён**, ошибка идёт вверх (в цикле агента это обычная ошибка шага; в `/serve` — кадр ошибки в SSE, как сейчас). Причина: «пришить» продолжение чужой моделью без согласия и без дублей надёжно не выходит (все четыре бага LiteLLM выше — из этой зоны). «Рассуждения» считаются ушедшими (в отличие от #49795): клиент их уже видел. Улучшение для `/serve`: перед отправкой HTTP-статуса дождаться первого чанка (с таймаутом) — тогда провал всех целей отдаётся честным 429/503, а не `200 + кадр ошибки`. Продолжение «префиксом» в TUI — отдельное решение (решение 5), не в этом релизе.

**Сессия и `fork()`.** `RoutedProvider { engine: Arc<Engine>, route, affinity: Mutex<Affinity> }`. `fork()` создаёт новый хэндл с общим `engine` и **пустой** привязкой: первая же отправка выбирает цель и берёт `backend.fork()` (свежая сессия). При смене цели в той же беседе:
- новый `Backend` — всегда `fork()` свежий, история уходит локальной копией целиком (компакция и остальное работают как сегодня);
- целевой DeepSeek web: `SystemDelivery::Fresh` отрабатывает сам (новая сессия, «### LOCAL MEMORY» + история); прежняя серверная сессия уходит в `discard_remote_session` (через `SessionDiscard`), чтобы не плодить мусор;
- `keeps_server_side_history()`, `session_tokens()`, `session_identity()` отвечают **по текущей цели** (их читают `context::compact`, `multichat`, `app::auto_save_session`); `adopt_session` работает, только если маршрут сейчас указывает на `DeepseekBackend` и id принадлежит ему.
- В `Session` добавляется `route_state: Option<{route, account, model}>` (`#[serde(default)]`; как тесты «старый формат без поля грузится»), и `model_type` перестаёт врать.
Событие `GatewayEvent::FallbackTaken { conversation?, from, to, class }` показывается в строке статуса и в журнале чата как `ui_only`-заметка (`ChatMessage::ui_system` — модель её не увидит).

**Снимок лимитов.**

```rust
pub struct LimitSnapshot {
    pub account: AccountId, pub scope: Option<String>,   // например модельная группа
    pub plan: Option<String>,
    pub windows: Vec<LimitWindow>,
    pub observed_at: SystemTime,
}
pub struct LimitWindow {
    pub name: String,                  // "5h", "weekly", "rpm", "tpm", "daily", "spend"
    pub kind: WindowKind,              // Requests | Tokens | Spend | Percent
    pub used: Option<f64>, pub limit: Option<f64>, pub remaining: Option<f64>,
    pub used_pct: Option<f32>, pub resets_at: Option<SystemTime>,
    pub source: LimitSource,           // Header | CliProbe | Ledger | Declared
}
```
`fresh(ttl)` решает, годится ли снимок для решений. Источники: заголовки ответа (API), проба CLI (фон, редко, только если аккаунтом пользовались недавно), ledger (локальный подсчёт против заявленного бюджета), заявленное пользователем.

### 3.8 Учёт (`UsageLedger`)

```rust
pub struct UsageRecord {
    pub at: SystemTime, pub source: Source,      // Chat | SubAgent | Role(RoleName) | Serve
    pub account: AccountId, pub model: String, pub route: Option<RouteId>,
    pub usage: UsageDetail,                      // input/output/cache_read/cache_write/reasoning + estimated
    pub cost_usd: Option<f64>,                   // если цена известна
    pub latency_ms: u32, pub ttft_ms: Option<u32>,
    pub outcome: Outcome,                        // Ok | Failed(class) | Cancelled
    pub fallback_from: Option<AccountId>,
}
```
- Хранение: **агрегаты** по `(день, аккаунт, модель, источник)` + счётчики текущих окон в `data_dir/gateway/usage.json`; запись `util::atomic_write` раз в ~30 с и при выходе (инвариант 3). Потеря при падении — не больше интервала сброса. Сырые записи не пишем на диск (при желании — в `debug_log`, который и так JSONL-трасса).
- `data_dir/gateway/limits.json` — последние снимки с `observed_at` (читаются как *устаревшие* при старте).
- Один писатель: если одновременно живут TUI и `--proxy` на одном `data_dir`, второй процесс открывает ledger «только для чтения» (lock-файл с pid; **[гипотеза]**: нужно проверить поведение на Windows, `File::try_lock`).
- Оценка помечается `estimated = true` и в TUI видна (`~`), чтобы не выдавать оценку DeepSeek за счёт.
- Контекст чата: `AgentEvent::ContextUsage` сейчас считает из `session_tokens()`/локальной оценки; с `CallReport.usage.input` последнего вызова можно показывать **реальное** заполнение окна там, где usage настоящий (мелкая правка в `runner::context_used`, этап 6).

### 3.9 Роли (служебная модель)

`gateway.provider_for(RoleName) -> Arc<dyn LLMProvider>` (отдельный `RoutedProvider`, привязанный к маршруту роли). Кому менять вызов:
- суммаризатор компакции: `app/multichat.rs` (`provider.fork()` → `gateway.provider_for(Summary)`; `context::compact::compact` не меняется — получает `&Arc<dyn LLMProvider>`);
- оценщик GOAL: `app/goal.rs` (то же);
- **заголовки чатов**: новая задача после первого ответа ассистента: `Role::Title` → короткий запрос → `AppEvent::Gateway(GatewayEvent::TitleReady { conversation, title })`; при ошибке/таймауте остаётся `session::derive_title` (эвристика); результат не блокирует `handle_event` (инвариант 1). Для чистого DeepSeek-web это лишняя серверная сессия — берём `fork()` + `SessionDiscard`, как компакция, и добавляем опцию `[roles] title = "off"`.
- `subagent`: `Conversation.provider` под-агента строится из роли, если она задана (`agent/sub_agent.rs` берёт `ctx.provider.fork()` — тоже один вызов).
`CompletionRequest.model` для таких вызовов не нужен: цель определяет маршрут. Поле остаётся «устаревшим подсказчиком», чтобы не править десяток конструкторов.

### 3.10 Экраны и команды TUI

Принцип: один экран «Gateway» с вкладками, команды — быстрые ярлыки; все изменения применяются живыми (движок читает конфиг через общий хэндл, а не снимок).

```
╭ Gateway ─ Accounts │ Routes │ Roles │ Usage ───────────────────────────────╮
│ #  Account            Provider    State        5h/Week       Today   Ctx   │
│ 1  codex-work         cli:codex   ● ready      42% / 71%     1.2M    272k  │
│ 2  codex-personal     cli:codex   ◐ draining   88% / 40%     0.4M    272k  │
│ 3  claude-max         cli:claude  ✖ attention  auth expired    –      –    │
│ 4  openrouter         openai      ● ready      rpm 12/200    0.1M    128k  │
│ 5  deepseek           web         ● ready      ~est          0.9M~    64k  │
│ J/K move  e on/off  a add  r rename  t test  T retest  s secure  Enter open │
╰──────────────────────────────────────────────────────────────────────────────╯
```
- `Accounts`: ранг/порядок, состояние (с причиной и временем возврата), окна лимитов, потрачено, окно контекста модели, `~` у оценок. Мастер добавления сначала предлагает **найденные CLI** (поиск `codex`, `claude`, `opencode` в `PATH`), затем API-ключ, затем DeepSeek web.
- `Routes`: цепочка шагов с порогом; `J/K` — порядок; `t` — «сухой прогон» (какую цель выберет сейчас и почему).
- `Roles`: `main / title / summary / evaluator / subagent → маршрут`.
- `Usage`: по аккаунтам/моделям/источникам за сегодня/неделю/месяц, спарклайн, доля оценок, число fallback.
Команды (каждая — файл в `src/commands/defs/`, имя без `/`): `/accounts` (+ `add|rename|move|on|off|test|secure`), `/routes`, `/roles`, `/usage`. `/providers` остаётся как алиас на вкладку Accounts (старые привычки не ломаем). Статус-бар: `acct·model  ctx 38%  limit 71%`; на fallback — разовая заметка.

### 3.11 Поверхность `/serve`

- Модели в `GET /v1/models`: аккаунты (`account/model` из каталога), маршруты как `auto/<route>`, DeepSeek-режимы. В объекте модели — расширения (`context_window`, `max_output`, `pooprusteek: { kind, targets }`): лишние поля клиенты OpenAI терпят.
- Выбор по id: точный маршрут → `auto/…` → префикс аккаунта (в т. ч. старое `entry/model`) → DeepSeek → голая модель = «лучший доступный аккаунт» (вместо «первый в списке»).
- Ответ: заголовки `x-pooprusteek-route|account|model|attempts|fallback` (для стрима — после первого чанка, см. 3.7); `usage` в стриме по `stream_options.include_usage` клиента, а внутри — всегда собираем настоящий usage апстрима.
- Ошибки: честные статусы — `429` с `Retry-After` (минимум из `until` кандидатов), `503` когда все цели `NeedsAttention/Open`, `502` — апстрим сломан. Исправляется нынешний дефект «429 → 502» у compat-апстримов.
- Живая конфигурация: `ServerContext` держит `Arc<GatewayHandle>` вместо снимка `entries`; правки в TUI применяются сразу; долгоживущие `Backend` (общий `reqwest::Client`) вместо провайдера на запрос.
- Дополнительно (loopback + `api_key`): `GET /v1/pooprusteek/status` — аккаунты, состояния, снимки лимитов, usage.
- Диалект Anthropic `/v1/messages` (сейчас `ServerApi::Anthropic` — 501, исходящая половина `anthropic_compat` уже есть): нужен Claude Code-подобным клиентам; отдельный этап/решение 8.

### 3.12 Тесты и стенд

- `harness::mock` расширяется (этап 0): поля ответа `headers`, `retry_after`, `usage`, `fail_after_chunks`, `stall_ms`; несколько «аккаунтов» — путь `/a/v1`, `/b/v1` в одном процессе.
- Модульные: машина состояний с инжектируемыми часами (`Clock` трейт, без `sleep`), таблица классов, выбор (golden), миграция конфига (golden), разбор заголовков/usage (по образцам из доков), cooldown приписан правильному аккаунту (анти-#45645), `Retry-After` соблюдается при нескольких аккаунтах (анти-#34399), breaker не выбирает открытую цель (анти-#1289), устаревший снимок не участвует (анти-#5404).
- Сценарии `sandbox/scenarios/mock/gateway-*.toml`: 429→второй аккаунт; порог по заголовку остатка; обрыв до первого чанка — прозрачно, после — ошибка; роль `title`; `/serve` `auto/coding`.

---

## 4. Этапы

Макс. 3 агента одновременно, каждый — в своём git worktree от общей ветки интеграции `develop`. Правило слияния: этап вливается целиком после зелёных `cargo test --bin pooprusteek`, `cargo fmt --check`, `cargo clippy -D warnings` (pre-commit хук проекта). Размеры: S ≤ 1 дня, M ≈ 2–3, L ≈ 4–5 дней одного агента.

### Принцип против конфликтов

Все «горячие» файлы правятся **только на этапе 0** — одной строкой-регистрацией (stub): `config/mod.rs` (поля `Config`), `main.rs` (подкоманда `mock-cli`), `commands/mod.rs` (регистрации `accounts/routes/roles/usage`), `app/events.rs` (`AppEvent::Gateway`), `app/mod.rs` (поле `gateway`, диспетчер `on_gateway_event` → вызов функции из чужого файла), `provider/mod.rs` (`Usage`, default-методы трейта), `harness/mock.rs`. Дальше каждый этап наполняет **свои** файлы. Журналы — отдельные файлы `JOURNAL/2026-11-xx-gateway-sN.md`; `STATE.md`/`BUGS.md` правит только интегратор. Cargo.toml не меняется → нет конфликтов Cargo.lock.

### Этап 0 — Контракты и скелет (один агент, вливается первым, M)

Файлы-владельцы (создаёт): `src/gateway/{mod,types,error,limits,usage,backend,handle}.rs`, `src/config/gateway.rs`, заглушки `src/commands/defs/{accounts,routes,roles,usage}.rs`, расширение `src/harness/mock.rs`.
Правит по одной строке: `config/mod.rs`, `commands/mod.rs`, `main.rs`, `app/events.rs`, `app/mod.rs`, `provider/mod.rs`.
Содержание: типы (§3.2, 3.5, 3.7, 3.8), трейт `Backend`, `ErrorClass`, `LimitSnapshot`, `UsageRecord`, `GatewayEvent`, `GatewayHandle` (публичный API: `snapshot()`, `provider_for_role()`, `provider_for_model_id()`, `apply_edit()`, `subscribe()`), чистая миграция `from_legacy` + валидация (дубли id, циклы маршрутов, ссылки на несуществующие аккаунты), `Gateway` v0 «сквозной» (один шаг, без fallback), включается только `[gateway] enabled = true` (по умолчанию `false`, `build_provider` не меняется).
Критерий готовности: приложение работает как раньше; золотые тесты миграции и круговой прогон конфига; `GatewayHandle::snapshot()` отдаёт аккаунты из старого конфига; mock умеет заголовки/usage/обрыв/несколько путей.
Тесты: `config::gateway::tests`, `harness::mock::tests`, регистрация команд (существующий тест реестра).

### Волна 1 (параллельно 3 агента, после слияния этапа 0)

**Этап 1 — API-бэкенды и секреты (M–L).**
Владелец: `src/provider/{compat_client,openai_compat,anthropic_compat,gemini_compat,openai_client,anthropic_client,gemini_client}.rs`, `src/gateway/backend/{api,deepseek}.rs`, `src/secrets/`.
Контракт: реализует `Backend` из этапа 0. Не трогает `gateway/{engine,state,routed}.rs`.
Делает: типизированные ошибки и снятие заголовков в `CompatClient::send`; `parse_limits`; usage в стриме (OpenAI `include_usage` с автоотступлением на 400 «unknown parameter» и флагом `stream_usage` на аккаунт; Anthropic `message_start/message_delta`; Gemini `usageMetadata`); модель на вызов; общий `reqwest::Client`; `DeepseekBackend` (оценка usage); `secrets` (keyring/env/file/none, `ensure_default_store` вынесен копией).
Готово: тесты разбора на образцах из доков (в т. ч. 429 с `Retry-After` и без, spend-cap); `/serve` отдаёт 429 вместо 502 для compat-апстрима (проверка без движка — через `UpstreamError`).
Риск: `CompletionChunk` без `Default` — не менять; usage идёт в `CallReport`.

**Этап 2 — Движок (L).**
Владелец: `src/gateway/{engine,state,routed,ledger,policy,clock}.rs`, `src/session.rs` (поле `route_state`), небольшие правки `src/provider/mod.rs::build_provider` (ветка при `gateway.enabled`).
Контракт: работает **только** через `Backend`/типы этапа 0; тестируется `FakeBackend` (скриптуемые исходы), без этапов 1 и 3.
Делает: §3.7 целиком (состояния, cooldown, breaker, пороги, липкость, шаги, таблица классов, failover до первого чанка), `RoutedProvider` (фасад `LLMProvider`, `fork()`, сессионные методы по текущей цели), `UsageLedger` + снимки на диск, фоновые задачи (сброс ledger, пробы), события `GatewayEvent`.
Готово: сценарные тесты на часах-инжекции для каждой строки таблицы «класс → действие»; тест анти-#34399/#45645/#1289/#5404; `fork()` независим; смена цели с DeepSeek-сессией корректно сбрасывает/восстанавливает.

**Этап 3 — CLI-бэкенды (L).**
Владелец: `src/gateway/backend/cli/{mod,discover,claude,codex,opencode,process}.rs`, `src/harness/mock_cli.rs`.
Контракт: `Backend`; никаких чтений файлов авторизации; выход в `ErrorClass`/`LimitSnapshot`.
Делает: поиск бинарей в `PATH` без новых зависимостей; **спайк-задача 1** на каждый CLI — проверить гипотезы §3.6 (режим completion, изоляция `home`, формат событий, источник лимитов) и записать итог в `JOURNAL/…-gateway-s3.md` до написания парсеров; процессная дисциплина; `mock-cli`.
Готово: `mock-cli` покрывает успех/429/обрыв/авторизацию-умерла/молчание > таймаута (процесс убит); Windows-пути (`.cmd`/`.exe` шимы npm) обработаны.
Оговорка: Codex на этой машине не установлен — его спайк делается по схеме/докам, живой прогон нужен от владельца.

### Волна 2 (параллельно 3 агента, после слияния этапов 1 и 2; этап 3 может ещё идти)

**Этап 4 — TUI и команды (L).**
Владелец: `src/tui/**` (новый view Gateway), `src/app/{providers,view_state}.rs`, `src/app/keys/{providers,gateway}.rs`, `src/commands/defs/{accounts,routes,roles,usage,providers}.rs`, `src/app/gateway_view.rs`.
Контракт: читает только `GatewayHandle::snapshot()`, пишет только `apply_edit()`; события — `on_gateway_event`.
Готово: вкладки Accounts/Routes/Roles/Usage; перенос порядка клавишами; мастер с найденными CLI; `/providers` ведёт в Accounts; рендер не мутирует состояние; тесты на модель вида (без терминала) и на `rename` с переписыванием ссылок.

**Этап 5 — `/serve` (M–L).**
Владелец: `src/server/**`, `src/app/serve.rs`.
Готово: §3.11 (кроме диалекта Anthropic): живая конфигурация, `auto/<route>`, заголовки, usage в стриме, честные статусы с `Retry-After`, `/v1/pooprusteek/status`, общий пул соединений; существующие тесты `server::openai` проходят без правок (кроме ожидаемых ответов 429).

**Этап 6 — Роли, заголовки, реальный usage в чатах (M).**
Владелец: `src/app/{multichat,goal,titles}.rs`, `src/agent/sub_agent.rs` (одна строка), `src/agent/runner.rs::context_used` (одна функция).
Готово: суммаризатор/оценщик/под-агент берут роль; заголовок чата генерируется фоном с откатом на `derive_title`; контекст показывает реальное заполнение, когда usage настоящий; `Session.model_type` корректен.

### Этап 7 — Интеграция (один агент, M)

`[gateway] enabled` по умолчанию `true`; сценарии harness (`gateway-*.toml`) и прогон `--repeat 5` на mock; ревью чужой моделью (правило владельца) каждого этапа; документация: `.memories/` (`STATE`, `BUGS`, `reference/GATEWAY.md`, `CONFIG.md`), `pooprusteek-usage-providers-and-mcp` skill; проверка утечек памяти/соединений под нагрузкой `/serve` (боль LiteLLM).
Необязательно: этап 8 — диалект Anthropic `/v1/messages` (решение 8).

### Таблица зависимостей и слияния

```
0 ──▶ 1 ┐
0 ──▶ 2 ├─▶ (1+2) ──▶ 4, 5, 6 ──▶ 7 ──▶ [8]
0 ──▶ 3 ┘ (3 нужен к 7; к 4 — для мастера CLI, можно подставить заглушку)
```
Порядок слияния: 0 → 1 → 2 → 3 → 5 → 6 → 4 → 7. Причина: 1 и 2 не пересекаются по файлам, но 2 использует типы 1; 5 и 6 малые, 4 самый широкий по `tui/`.

### Где возможны конфликты и как избегаем

| Зона | Кто пересекается | Мера |
|---|---|---|
| `config/mod.rs` | всех касается | только этап 0, остальным — `config/gateway.rs` (их) |
| `provider/mod.rs` | 0 (трейт, `Usage`), 2 (`build_provider`) | этап 0 правит всё, кроме тела `build_provider`; этап 2 — только его тело |
| `app/mod.rs` | 0, 2, 4, 6 | этап 0 ставит поле и диспетчер-заглушку; остальные — только в своих файлах (`gateway_view.rs`, `titles.rs`) |
| `server/*` | 1 (тест 429→502), 5 | этап 1 не правит `server/`; его проверка — тест в `provider/` |
| `session.rs` | 2, 6 | поле `route_state` — этап 2; этап 6 только читает |
| `harness/mock.rs` | 1, 2, 3 | только этап 0 |
| `Cargo.toml` / lock | — | зависимости не добавляются |
| `.memories/*` | все | журналы в отдельных файлах; общие файлы — интегратор |
| `commands/defs/providers.rs` | 4 | единственный владелец |

---

## 5. Решения владельца (рекомендация — первой)

1. **Как подключать подписки (Codex/Claude/…).**
   - **A. Только запуск официального CLI пользователя (рекомендуется).** Токены не читаем. Цена: старт процесса на вызов, зависимость от формата вывода CLI, меньше «чистых» моделей. Плюс: нет бана, лимиты берём у самого CLI.
   - B. Ещё и прокси OAuth-токенов (как CLIProxyAPI). Цена: риск бана и нарушения условий — Anthropic прямо запрещает маршрутизацию через подписку и хранение токенов [веб]; у CLIProxyAPI реальные жалобы: блок Google (#1637), учёт как «extra usage» (#2599). Сам по себе дешевле в реализации и быстрее.
2. **Что делает CLI-аккаунт.**
   - **A. Сначала `completion` (текст-в-текст, наши инструменты через промпт-протокол), `agent`-режим — следующим релизом (рекомендуется).** Цена: нужен спайк «как заставить CLI не звать свои инструменты».
   - B. Сразу оба режима. Цена: +1 этап, шире поверхность ошибок.
   - C. Только `agent`. Цена: теряем цикл pooprusteek, роли и `/serve` как LLM.
3. **Формат конфига и обратная совместимость.**
   - **A. Новые таблицы + одноразовая миграция + копия `config.toml.pre-gateway`, `[[providers]]` дальше не пишется (рекомендуется).** Цена: откат на старую версию теряет записи без ручного восстановления из копии.
   - B. Дуальная запись `[[providers]]` один релиз. Цена: двойная логика и риск расхождения.
   - C. Расширить `[[providers]]` вместо новых таблиц. Цена: дешевле сейчас, но аккаунт/маршрут/роль не ложатся — через релиз будет вторая миграция.
4. **Секреты.**
   - **A. Новые ключи в keyring (если доступен, иначе файл), старые — по команде `/accounts secure` (рекомендуется).** Цена: до команды старые ключи остаются открытым текстом.
   - B. Автоперенос всех при первом запуске. Цена: ломается на средах без keyring (Docker/харнесс), риск потери ключа при сбое.
   - C. Только env/файл. Цена: проще, но слабее защита и нет единого места.
5. **Fallback посреди стрима.**
   - **A. Только до первого ушедшего чанка; после — ошибка вверх (рекомендуется).** Цена: редкий обрыв посреди ответа виден пользователю.
   - B. Дополнительно «продолжение префиксом» в TUI. Цена: сложность и дубли; у LiteLLM именно здесь четыре открытых бага.
   - C. Без fallback в стриме совсем (только блокирующие). Цена: теряем главное для чатов.
6. **Хранение учёта.**
   - **A. Агрегаты + `atomic_write` раз в ~30 с (рекомендуется).** Цена: при падении теряется ≤ 30 с статистики, нет сырых записей.
   - B. Append-only JSONL. Цена: точнее, но нарушает букву инварианта 3 (запись не через `atomic_write`), нужна ротация.
   - C. SQLite. Цена: новая зависимость (требует вашего согласия), миграции схемы.
7. **Порог «заранее» по умолчанию.**
   - **A. Включён (мягко: Draining при 85–90%) только при свежем снимке из заголовков/CLI; по оценке — только предупреждение (рекомендуется).** Цена: у аккаунтов без достоверного источника «заранее» не работает.
   - B. Включён всегда, в том числе по оценке ledger. Цена: ложные переключения при неточной оценке (как расхождение квот в CLIProxyAPI #1015).
   - C. Выключен, включается вручную. Цена: фича, которую просили, не видна.
8. **Диалект Anthropic в `/serve` (`/v1/messages`) в этом релизе.**
   - **A. Да, как этап 8 после интеграции (рекомендуется).** Цена: +M; даёт подключать Claude Code-подобные клиенты через `auto/…`.
   - B. Нет, позже. Цена: проще релиз; `ServerApi::Anthropic` остаётся 501.

---

## 6. Риски и непроверенное

- **Непроверено (нужен спайк этапа 3):** способ заставить каждый CLI работать как «чистый LLM»; точная схема `rateLimits` Codex (источник — исходники протокола, не живой вызов); есть ли у Claude Code немедленный неблокирующий источник лимитов подписки (событие `rate_limit_event` описано только сторонними страницами); изоляция аккаунтов OpenCode; заголовки лимитов Gemini; поведение `include_usage` на LM Studio/Ollama (поэтому флаг на аккаунт с откатом).
- **Юридическое:** условия OpenAI/Codex и Google по запуску их CLI вторым приложением в этом обзоре не проверялись. Для Anthropic прочитан официальный текст (запрет на маршрутизацию через подписку и хранение токенов, разрешение на вход в неизменённый Claude Code); трактовка «наш процесс запускает `claude -p` за пользователя» формально текстом не покрыта — это вывод, не цитата. Рекомендуется показать предупреждение в мастере CLI-аккаунта.
- **Лицензии:** new-api — AGPL-3.0, LiteLLM — `NOASSERTION` + `enterprise/`: идеи берём, код нет. CLIProxyAPI/CCR/Portkey — MIT.
- **Технические:** два процесса на одном `data_dir` (TUI + `--proxy`) — единственный писатель ledger; долгие `Draining`-флапы (гистерезис −5 п.п. заложен); рост числа `Backend` с процессами CLI (лимит одновременных на аккаунт); Windows `PATH`/шимы `.cmd`; пагинация «Usage» на больших ledger (агрегаты ограничивают рост).
- **Не вошло:** цены моделей (кроме заявленных пользователем и тех, что отдаёт API) — каталог цен без проверенного источника не вшиваем; бюджеты в долларах на уровне ключей/команд (мульти-тенантность LiteLLM) — вне объёма (локальный инструмент одного пользователя).

## Источники

- LiteLLM: context7 `/websites/litellm_ai` (routing, reliability, config_settings, users/team/tag budgets); issues [#34399](https://github.com/BerriAI/litellm/issues/34399), [#15526](https://github.com/BerriAI/litellm/issues/15526), [#12685](https://github.com/BerriAI/litellm/issues/12685), [#27954](https://github.com/BerriAI/litellm/issues/27954), [#14384](https://github.com/BerriAI/litellm/issues/14384), [#49795](https://github.com/BerriAI/litellm/issues/49795), [#35578](https://github.com/BerriAI/litellm/issues/35578), [#39462](https://github.com/BerriAI/litellm/issues/39462), [#45645](https://github.com/BerriAI/litellm/issues/45645); исходник `litellm/exceptions.py` (`MidStreamFallbackError`).
- OpenRouter: https://openrouter.ai/docs/features/provider-routing
- Portkey: https://portkey.ai/docs/product/ai-gateway/fallbacks ; issue https://github.com/Portkey-AI/gateway/issues/1289
- new-api: https://github.com/QuantumNous/new-api (README), issue [#3537](https://github.com/QuantumNous/new-api/issues/3537); one-api: https://github.com/songquanpeng/one-api (README)
- CLIProxyAPI: https://github.com/router-for-me/CLIProxyAPI (README, `config.example.yaml`), issues [#1015](https://github.com/router-for-me/CLIProxyAPI/issues/1015), [#5404](https://github.com/router-for-me/CLIProxyAPI/issues/5404), [#1637](https://github.com/router-for-me/CLIProxyAPI/issues/1637), [#2599](https://github.com/router-for-me/CLIProxyAPI/issues/2599), [#3200](https://github.com/router-for-me/CLIProxyAPI/issues/3200), [#3187](https://github.com/router-for-me/CLIProxyAPI/issues/3187), [#5545](https://github.com/router-for-me/CLIProxyAPI/issues/5545), [#3530](https://github.com/router-for-me/CLIProxyAPI/issues/3530)
- Claude Code Router: https://github.com/musistudio/claude-code-router , issues [#208](https://github.com/musistudio/claude-code-router/issues/208), [#275](https://github.com/musistudio/claude-code-router/issues/275), [#589](https://github.com/musistudio/claude-code-router/issues/589)
- Anthropic: https://code.claude.com/docs/en/legal-and-compliance , https://code.claude.com/docs/en/headless , https://code.claude.com/docs/en/env-vars , https://platform.claude.com/docs/en/api/rate-limits
- OpenAI: https://platform.openai.com/docs/guides/rate-limits (заголовки `x-ratelimit-*`); Codex: https://github.com/openai/codex (`app-server`, поиск `account/rateLimits/read`)
- OpenCode: https://opencode.ai/docs/cli ; ACP: https://agentclientprotocol.com/registry
