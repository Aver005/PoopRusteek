# T3 Code: лимиты подписок, контекст/токены, запуск CLI, мульти-инстансы

Исследование только на чтение по `E:\Projects\Forks\t3code` (монорепо TypeScript).
Секреты и файлы с токенами не читались — только код, адреса и форматы.
Пометки: **[неофиц.]** — адрес не публичный API, используется самим CLI/вендором и может измениться;
**[предположение]** — вывод по коду вызывающей стороны, без чтения внутренностей пакета.

Дата: 2026-11-06.

---

## 0. Что вообще есть в T3

Список драйверов — `apps/server/src/provider/builtInDrivers.ts:31` (`BUILT_IN_DRIVERS`):

| driver kind | пакет / файл |
| --- | --- |
| `codex` | `provider/Drivers/CodexDriver.ts` + `CodexProvider.ts` |
| `claudeAgent` | `provider/Drivers/ClaudeDriver.ts` + `ClaudeProvider.ts` |
| `cursor` | `packages/provider-cursor` |
| `grok` | `packages/provider-grok` |
| `fx` | `packages/provider-fx` (без лимитов/usage) |
| `opencode` | `packages/provider-opencode` |
| `antigravity` | `provider/Drivers/AntigravityDriver.ts` |
| `pi` | `packages/provider-pi` |
| `muse` | `packages/provider-muse` |
| `acpRegistry` | `packages/provider-acp-registry` (Kimi, Copilot, Devin, Droid, claude-acp и пр.) |

Отдельного драйвера Kimi нет: Kimi приходит либо из ACP-реестра (`id: "kimi"`), либо как модель
`openrouter/kimi-*` внутри OpenCode.

Три независимых механизма, которые надо не путать:

1. **Лимиты подписки** (`ServerProvider["usageLimits"]`) — отдельная проба, обновляется фоновым
   health-опросом, у каждого драйвера свой источник.
2. **Расход токенов за ход и контекст** — из потока событий провайдера (`TurnTokenUsage`,
   `ThreadTokenUsageSnapshot`).
3. **История расхода за сессию/день** — `apps/server/src/usage/UsageService.ts` сканирует
   транскрипты на диске (или провайдерский API), считает деньги по ставкам LiteLLM.

`provider/agentContextBudget.json` (`claudeSystemAppendBytes`, `codexAdditionalContextBytes`,
`mcpToolListJsonBytes`) — это **не** лимиты подписки, а измеренный вклад промпта самого T3 и списка
MCP-инструментов; используется только в `AgentContextBudget.test.ts:108-130` как CI-бюджет, чтобы
T3 не раздувал свой оверхед. В рантайме лимиты не задаёт.

---

## 1. Общие контракты (пригодятся для схемы в Rust)

### 1.1 Окна лимитов

`packages/contracts/src/server.ts` + `packages/provider-core/src/server/usageLimits.ts:28`:

```
ServerProviderUsageLimits = {
  checkedAt: ISO,
  windows: [{ id, kind: "session"|"weekly"|"monthly"|"other", label,
              usedPercent: 0..100, resetsAt?, windowDurationMins? }],
  unavailable?: { reason: "unsupported"|"probeFailed", message? },
  resetCredits?: { availableCount, nextExpiresAt?, nextCreditId? }
}
```

Слияние частичных апдейтов: `applyUsageLimitsUpdate` (`usageLimits.ts:50`) — мержит по `id`,
не перетирает `resetsAt` пустым значением, не воскрешает снятый `unsupported`.
`resolveUsageLimitsAfterProbe` (`usageLimits.ts:102`) — «probeFailed не затирает последний удачный
снимок», и более свежий published-снимок побеждает по `checkedAt`.

### 1.2 Токены

`packages/contracts/src/providerRuntime.ts`:

- `ThreadTokenUsageSnapshot` (строка 30): `usedTokens`, `maxTokens?`, `inputTokens?`,
  `cachedInputTokens?`, `outputTokens?`, `reasoningOutputTokens?`, `last*` поля, `toolUses?`,
  `durationMs?`, `autoCompactThreshold?`, `cost?{amount,currency}`.
- `TurnTokenUsage` (строка 64): `usageStatus: "complete"|"partial"|"unavailable"`, `inputTokens`
  (включая cache read/write), `outputTokens`, `cachedInputTokens?`, `cacheCreationTokens?`,
  `reasoningTokens?` (подмножество output), `hasSubagents`, `usageScope: "main_agent"`.

Запись истории (`packages/provider-core/src/server/usage.ts:14` `UsageRecord`):
`{provider, timestampMs, model, sessionId, totals{uncachedInputTokens, cachedInputTokens,
cacheCreationTokens, outputTokens, reasoningTokens}, reportedCostUsd, speed, dedupeKey}`.

### 1.3 Частота опроса

`packages/contracts/src/settings.ts:944`:
`DEFAULT_PROVIDER_HEALTH_REFRESH_INTERVAL = Duration.minutes(5)`.
Цикл — `packages/provider-core/src/server/managedProvider.ts:280` и
`apps/server/src/usage/UsageLimitSources.ts:130-148` (там же `shouldRunScopeWork({type:"provider-status"})`
— фоновая активность может выключать опрос). Отдельно, во время хода, лимиты прилетают событиями
(Claude `rate_limit_event`, Codex `account/rateLimits/updated`) и обновляют снимок без ожидания 5 минут.

### 1.4 Модель инстансов

`packages/contracts/src/providerInstance.ts:75`:

```
ProviderInstanceConfig = {
  driver: ProviderDriverKind,          // "codex" | "claudeAgent" | ...
  displayName?, accentColor?,
  environment?: [{ name, value: string | SecretReference, sensitive, valueRedacted? }],
  enabled?, 
  config?: unknown                     // схема конкретного драйвера
}
```

`ProviderInstanceId` — slug ≤64 символов, `^[a-zA-Z][a-zA-Z0-9_-]*$` (строка 22), поэтому
`opencode-yandex` валиден. Хранение: `settings.ts:1339` `providerInstances: Record<Id, Config>`.
Секреты в `environment` могут быть ссылкой (`SecretReference`) — тогда значение тянется из
secret-provider (1Password и т.п.) через `SecretResolver` (`ProviderInstanceRegistry.ts:90`).
Мутации: `ProviderInstanceMutation` = `create | upsert | remove` (`providerInstance.ts:85`).

Живой реестр: `ProviderInstanceRegistry.ts:400` `makeProviderInstanceRegistry`; `buildEntry`
(строка 144) декодирует конфиг, подставляет секреты, вызывает `driver.create(...)`, а при ошибке
отдаёт «unavailable»-снимок с текстом причины. Пересоздание — по изменению записи
(`entryEqual`), по `rebuild`, либо по перечитыванию секретов (`reloadSecrets`, строка 445).

---

## 2. Claude

### 2.1 Лимиты подписки

Два независимых источника, оба в `apps/server/src/provider/claudeUsageLimits.ts`.

**(a) Проба через CLI/SDK** — `ClaudeProvider.ts:322` `probeClaudeCapabilities`:
создаётся SDK-запрос с «вечным» промптом (чтобы не ушёл в API), затем
`q.usage_EXPERIMENTAL_MAY_CHANGE_DO_NOT_RELY_ON_THIS_API_YET({ skipBehaviors: true })`
(`ClaudeProvider.ts:363`, таймаут `DEFAULT_TIMEOUT_MS`). Ответ `SDKControlGetUsageResponse`
урезается до `{ rate_limits_available, rate_limits }` (строки 366-372).

Разбор — `claudeUsageLimits.ts:175` `claudeUsageResponseToLimits`:

| поле ответа | что даёт |
| --- | --- |
| `rate_limits_available=false` | `unavailable.reason="unsupported"` |
| `rate_limits.five_hour{utilization,resets_at}` | окно `session`, 300 мин, label `Session` |
| `rate_limits.seven_day{...}` | окно `weekly`, 10080 мин |
| `rate_limits.model_scoped[]{display_name,utilization,resets_at}` | окно `seven_day_<slug>`, `Weekly · <name>` (строки 117-126, 197-203) |
| `rate_limits.spend{enabled, used{amount_minor,currency,exponent}, limit{...}}` | окно `monthly_spend`, `monthly` (строки 84-97, 204-212) |
| `rate_limits.extra_usage{is_enabled, used_credits, monthly_limit, utilization}` | тот же `monthly_spend` как fallback |

Проценты `get_usage` уже 0-100, поэтому только `clampPercent`. Ошибка/таймаут пробы → поле
`usage` не заполняется, вызывающая сторона ставит `makeUnavailableUsageLimits(reason:"probeFailed")`
(`ClaudeProvider.ts:245`, `usageLimits.ts:35`).

**(b) Потоковые события хода** — `claudeUsageLimits.ts:155` `claudeRateLimitEventToUpdate`:
`SDKRateLimitInfo{rateLimitType, utilization, resetsAt}`; `utilization` тут 0-1 → ×100;
`rateLimitType` из `{five_hour, seven_day, seven_day_overage_included}`; безымянные overage-бакеты
отбрасываются, пока проба не дала display_name (строки 154-172). Вызов — `ClaudeAdapterV2.ts:8184`.

**(c) Прямой HTTP, который T3 использует только для reset-кредитов** —
`apps/server/src/provider/claudeResetCredits.ts`:

- Токен: `<configDir>/.credentials.json` → `claudeAiOauth.accessToken`
  (`readAccessToken`, строки 137-143), причём **на darwin чтение отключено** (там Keychain),
  см. строку 140.
- Запрос: `GET https://api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1`
  (строка 159), заголовки `authorization: Bearer <token>`, `anthropic-beta: oauth-2025-04-20`,
  `user-agent: claude-cli/<version> (external, cli)` (`withClaudeHeaders`, строки 146-151). **[неофиц.]**
- Ответ (тот же формат, что читает hub-клиент `usage/cliproxyApi.ts:51-74`):
  `{ five_hour, seven_day, limits[]{kind:"weekly_scoped", percent, resets_at, scope.model.display_name},
     cedar_ember{eligible, grants[], next_grant_id} }`
- Списание кредита: `POST https://api.anthropic.com/api/organizations/<organizationUuid>/reset_rate_limits`
  с телом `{program:"cedar_ember", grant_id, request_id}` (строки 198-238); `organizationUuid` —
  из `<configDir>/.claude.json` → `oauthAccount.organizationUuid` (строки 175-188). Ошибки: 429 →
  `rateLimited`, 401/403 → `signedOut`, `cooldown`/`unavailable` — отдельные исходы.

**Вывод для Rust:** тот же `/api/oauth/usage` даёт и лимиты, и reset-кредиты; CLI для этого не нужен,
если есть OAuth-токен из `.credentials.json` (или свой OAuth). Адрес неофициальный, но это ровно тот,
что дергает сам Claude Code.

### 2.2 Контекст и токены

- За ход: `ClaudeTurnTokenUsage.ts:56` `normalizeClaudeTurnTokenUsage` из `SDKResultMessage.usage`:
  `input + cache_read + cache_creation`, `output_tokens`,
  `output_tokens_details.thinking_tokens` → `reasoningTokens`; при steering несколько сегментов
  суммируются (`withEarlierSegmentUsage`, строки 27-54). `complete` только при
  `subtype==="success"` и терминальном `completed`.
- Контекст (для метра): `ClaudeAdapterV2.ts:255-289` `claudeProviderTurnTokenUsage` —
  `usedTokens = inputTokens + outputTokens`, `maxTokens = claudeContextWindow(modelSelection)`
  из каталога моделей (`ClaudeModelCatalog.ts:283`, поле `contextWindowTokens` в
  `ClaudeModelManifest.ts:16`); там же `autoCompactThreshold`, `promptCacheTtlMs`.
- История за сессию: `Drivers/claudeUsage.ts:130` `claudeUsageReader.directories` →
  `<CLAUDE_CONFIG_DIR|~/.claude>/projects/**` (`*.jsonl`). Один ассистентский контент-блок = одна
  запись с полным `usage`, поэтому дедуп по `messageId:requestId` (`claudeUsage.ts:55`, комментарий
  строк 18-22: без дедупа переоценка ~2.4×). Advisor-итерации (`usage.iterations[].type=="advisor_message"`)
  пишутся отдельными записями под своей моделью (строки 78-108).

### 2.3 Как запускается

T3 **не собирает argv Claude сам** для чата: `@anthropic-ai/claude-agent-sdk` `query()` с
`pathToClaudeCodeExecutable` (`ClaudeProvider.ts:20,207`; `ClaudeAdapterV2.ts:821`) — SDK поднимает
CLI и общается с ним по своему stdio-протоколу (stream-json + control requests; точные флаги —
внутри SDK, **[предположение]**).

Прямой вызов CLI есть в одноразовой генерации текста (`textGeneration/ClaudeTextGeneration.ts:196-224`):

```
claude -p --output-format json --json-schema <schema> --model <apiModelId>
       [--effort <effort>] --settings <json> --tools "" --disable-slash-commands
       [--strict-mcp-config] --permission-mode dontAsk
```

Модель/усилие/thinking/fastMode передаются опциями селекта; окружение — `makeClaudeEnvironment`
(`Drivers/ClaudeHome.ts:30-43`): при заданном `homePath` ставится `CLAUDE_CONFIG_DIR`
(специально **не** `HOME`, иначе на macOS теряется Keychain-логин — комментарий строки 40).

### 2.4 Мульти-аккаунт

`ClaudeSettings.homePath` = «CLAUDE_CONFIG_DIR path» (`settings.ts:721`), резолв —
`resolveClaudeHomePath` (`ClaudeHome.ts:12`): `homePath` → env `CLAUDE_CONFIG_DIR` → `~/.claude`.
Каждый инстанс = своя папка `.claude` + свой `.claude.json` = свой логин.
`~/.claude.json` рядом (не внутри) — путь считает `claudeAccountConfigPath`
(`claudeResetCredits.ts:181`). Переключение аккаунта = переключение инстанса; отдельного
«сменить аккаунт в том же инстансе» нет, есть `/logout` в сессии.

UI: `apps/web/src/components/settings/AddProviderInstanceDialog.tsx:70` `deriveInstanceId` →
`<driver>_<slug>` (напр. `claudeAgent_work`), шаги визарда — `AddProviderInstanceWizardSteps.tsx`
(драйвер → имя/цвет/id → вход → переменные окружения).

---

## 3. Codex

### 3.1 Лимиты подписки

Основной путь — JSON-RPC к `codex app-server`, метод **`account/rateLimits/read`**
(`CodexProvider.ts:575`), параметр `null`. Ответ (`CodexProvider.ts:579-586`):
`{ rateLimits, rateLimitsByLimitId, rateLimitResetCredits, accountId }`.

Форма (`codexUsageLimits.ts:19-37`):

```
rateLimits: {
  limitId?, planType?, rateLimitReachedType?,
  primary:   { usedPercent, resetsAt (epoch s), windowDurationMins? },
  secondary: { ... },
  individualLimit: { remainingPercent, resetsAt }
}
rateLimitResetCredits: { availableCount, credits[]{status, expiresAt} }
```

Разбор — `codexUsageLimits.ts:59-98` `codexRateLimitsToWindows`:
`limitId` не `"codex"` → строки не показываются (модельные лимиты не мешают основному);
`kind` по `windowDurationMins`: ≥43200 → monthly, ≥10080 → weekly, иначе session;
для `planType` `free`/`go` у `primary` дефолт monthly, иначе 300 мин; `secondary` — 10080.
`individualLimit` → окно `individual`, `Monthly credit limit`, `100 - remainingPercent`.
Кредиты: `codexResetCreditsToContract` (строка 100) — только `status==="available"`, ближайший
`expiresAt`.

Поток: нотификация **`account/rateLimits/updated`** (`CodexAdapterV2.ts:4388`) →
`codexRateLimitsToUpdate` (`codexUsageLimits.ts:136`, отбрасывает неизменившиеся окна) и
`mergeCodexRateLimits` (строка 169) — частичные апдейты сохраняют прежние поля.

Ошибки: `codexRateLimitsFailureMessage` (`codexUsageLimits.ts:155`) различает JSON-RPC-код,
неудачный spawn, ранний выход и «не ответил»; результат пробы деградирует в
`{failure: "..."}`, аккаунт и модели при этом сохраняются (`CodexProvider.ts:569-594`).

**Прямой HTTP (без CLI)** — тот же ответ в формате CLI, использует hub-клиент
`usage/cliproxyApi.ts`: **[неофиц.]**

- `GET https://chatgpt.com/backend-api/wham/usage` (строка 298)
- заголовки: `Authorization: Bearer $TOKEN$`, `OpenAI-Beta: codex-1`,
  `Originator: Codex Desktop`, `Chatgpt-Account-Id: <chatgpt_account_id>` (строки 204-214)
- ответ: `{ plan_type, rate_limit: { primary_window{used_percent, reset_at, limit_window_seconds},
  secondary_window{...} } }` (строки 37-50); `limit_window_seconds/60` → `windowDurationMins`
- кредиты: `GET https://chatgpt.com/backend-api/wham/rate-limit-reset-credits` (строка 137),
  списание `POST .../rate-limit-reset-credits/consume {redeem_request_id, credit_id}` (строка 386),
  `request_id` — UUIDv5 от `(accountId, creditId)` для идемпотентности (строки 144-158)

### 3.2 Контекст и токены

- За ход/контекст: нотификация **`thread/tokenUsage/updated`** → `codexProviderTurnTokenUsage`
  (`CodexAdapterV2.ts:197`): `usedTokens = max(0, tokenUsage.last.totalTokens)`,
  `maxTokens = modelContextWindow`, плюс `inputTokens/cachedInputTokens/outputTokens/reasoningOutputTokens`.
- История: `<CODEX_HOME>/sessions/**/rollout-*.jsonl` (`Drivers/codexUsage.ts:384-397`).
  Парсер `parseCodexRecord` (строка 137): `session_meta` (id, `model_provider`, fork-копии
  подавляются, строки 109-121), `turn_context` (модель), `thread_settings_applied` (`service_tier` →
  `standard|fast|ultrafast`), `token_count` → `info.last_token_usage` + `info.total_token_usage`.
  Валидация кумулятивных счётчиков, рестарт процесса, форки — строки 215-276.
  `totals.uncachedInputTokens = input - cached - cache_write` (строка 285, у Codex `input` включает кэш).

### 3.3 Как запускается

`codexAppServerArgs` (`codexLaunchArgs.ts:12`) → `["app-server", ...доп.аргументы]`;
`resolveCodexLaunchArgs` подменяет их env-переменной `T3CODE_CODEX_LAUNCH_ARGS` (строка 8).
Запуск — `withCodexAppServerClient` (`CodexProvider.ts:481-521`): spawn с `CODEX_HOME`, затем
`initialize` (`buildCodexInitializeParams`) и `notify("initialized")`.

Протокол: JSON-RPC 2.0, **LF-разделённый JSONL по stdin/stdout**, лимит строки 128 МБ
(`packages/effect-codex-app-server/src/protocol.ts:20-21`), плюс отдельный stderr-tail
(`_internal/stdio.ts:60-78`). Модель/настройки — параметрами `thread/start` / `turn/start`,
чтение конфига — `config/read` (`CodexProvider.ts:562`), модели — `model/list` либо
`https://api.openai.com/v1/models` для ChatGPT-аккаунта (`CodexChatGptModels.ts:27`).

### 3.4 Мульти-аккаунт (самое интересное)

`CodexSettings` (`settings.ts:630-683`): `setupMode: "managed" | "existing"`, `homePath`
(`CODEX_HOME`), `shadowHomePath` («Account-specific Codex home. Keeps auth.json separate while
sharing state from CODEX_HOME»), `launchArgs`, `customModels`.

- **existing**: T3 запускает установленный `codex` и пользуется его `auth.json` внутри `CODEX_HOME`.
  Разные аккаунты = разные инстансы с разными `homePath`/`shadowHomePath`.
- **managed**: T3 сам делает OAuth ChatGPT и раздаёт токен CLI.
  - discovery: `https://auth.openai.com/.well-known/openid-configuration` (`CodexChatGptAuth.ts:37`),
    issuer проверяется строго (строки 192-202);
  - PKCE + loopback `http://127.0.0.1:<port>/auth/callback` (`Registration.redirectUri`, строка 76),
    scope **`chatgpt.tokens.use.direct`** (строка 36); обмен кода и refresh —
    `POST <token_endpoint>` form-urlencoded `grant_type=authorization_code|refresh_token` (строки 377-441, 767-781);
  - профили: `Sessions { activeClientId, sessions[] }`, методы входа `chatgpt-profile:<clientId>`
    и `chatgpt-change-account` (`CodexChatGptAuth.ts:455-458`) — так в одном инстансе живут
    несколько ChatGPT-аккаунтов, переключение = смена `activeClientId`;
  - хранение: `ServerSecretStore`, файл `<secretsDir>/provider-auth-<sha256(driver+instanceId)>.bin`,
    права 0600 (`ProviderCredentialStore.ts:6-22`, `ServerSecretStore.ts:171,189-224`);
  - раздача токена CLI: локальный брокер на named pipe (Windows) / Unix-сокете в приватной
    0700-папке (`CodexManagedTokenBroker.ts:45-102`), а Codex запускается с конфигом
    `model_providers.openai_token_sharing={base_url="https://api.openai.com/v1", wire_api="responses",
    auth={command=<self>, args=["__managed-chatgpt-token", endpoint, instanceId, environmentId, profileKey]}}`
    (`CodexManagedRuntime.ts:22-32`); окружение чистится от `OPENAI_API_KEY`, `OPENAI_BASE_URL`,
    `ACCESS_TOKEN`, `T3CODE_CODEX_LAUNCH_ARGS` (строки 117-124).
  - `profileKey = sha256([clientId, subject, idToken])` (строка 730) — привязка живой сессии к
    профилю: сменился аккаунт → сессия просит новый старт, а не тихо работает под другим.
  - ротация refresh-токена сериализуется файловой блокировкой `CodexChatGptSessionLock.ts`
    (в `access`, строка 801) — иначе два процесса сожгут токен друг другу.
  - shadow-home: `resolveManagedCodexHomeLayout` → `stateDir/providers/codex/<instanceId>/shadow`
    (`CodexManagedHome.ts:6-19`), `materializeCodexShadowHome` копирует общее состояние, оставляя
    `auth.json` своим (`Drivers/CodexHomeLayout.ts`).

Модель данных для «много подписок одного провайдера» у T3 = **инстанс на аккаунт** (+ managed-профили
внутри инстанса для Codex). Ровно это и надо повторить в pooprusteek.

### 3.5 Что можно в Rust

- Лимиты/кредиты: либо `codex app-server` + `account/rateLimits/read` (официальный протокол самого
  CLI, устойчиво), либо прямой `wham/usage` (быстро, но **[неофиц.]**).
- Токены: `thread/tokenUsage/updated` (в сессии) или парсинг `sessions/**/rollout-*.jsonl`
  (офлайн, без процессов).
- Запуск агента: только CLI (`codex app-server`), своего API у подписки нет.

---

## 4. Antigravity (Google Gemini / Claude&GPT через cloudcode)

### 4.1 Лимиты

`antigravityUsageLimits.ts:160` `makeAntigravityUsageProbe({profileDirectory, authMethod})`.

- Только для `authMethod` `oauth-personal` / `oauth-business` (строки 195-197).
- Токен-файл: `<profileDirectory>/antigravity-acp/acp_token.json`
  (`{client_id, client_secret, refresh_token, project_id?}`, строки 23-28, 170).
- Refresh: `POST https://oauth2.googleapis.com/token` form-urlencoded
  `{client_id, client_secret, refresh_token, grant_type:"refresh_token"}` (строка 216).
  Кэш access-токена до `expires_in` с запасом 60 с (строки 213-247).
- Идентификатор аккаунта: `GET https://www.googleapis.com/oauth2/v2/userinfo` Bearer (строка 228),
  затем `sha256("google-account\0<id>")` → `credentialFingerprint` (строка 294) — «один аккаунт на
  разных машинах» склеивается.
- Квоты: `POST {endpoint}/v1internal:retrieveUserQuotaSummary`, эндпоинты
  `https://daily-cloudcode-pa.googleapis.com` и `https://cloudcode-pa.googleapis.com` (строки 18-21),
  тело `{project}` при наличии `project_id`, заголовки `Authorization: Bearer`,
  `User-Agent: antigravity/hub/1.0.0 (aidev_client; os_type=<os>; arch=<arch>; cl=963137146)`,
  `redirect: "manual"` (строки 249-265). Fallback при пустом summary —
  `/v1internal:fetchAvailableModels` (строки 282-286). **[неофиц.]**
- Формат: `{groups[]{displayName, buckets[]}}` либо `{buckets[]}`;
  bucket `{bucketId, window:"5h"|"weekly", displayName, remainingFraction, resetTime, disabled}`
  (строки 34-55). `usedPercent = (1 - clamp(remainingFraction)) * 100`; семейство по префиксу
  `gemini-` / `3p-` («Claude & GPT (shared)») (строки 99-125). Старый формат —
  `models{ id: {modelProvider, quotaInfo, weeklyQuotaInfo} }` (строки 56-158).
- Таймауты: запрос 10 с, вся проба 30 с; любая ошибка → `probeFailed`
  («Could not read Antigravity limits. Refresh provider status to retry.»).

### 4.2 Контекст и токены

- ACP-нотификация `usage_update {used, size, cost?}` → `UsageUpdated` (`provider-acp/src/server/runtimeModel.ts:1566-1578`);
  `size` → `maxTokens`.
- История: скан `conversations` в `~/.gemini/antigravity*`, `~/.config/antigravity` и в
  `<profile>/antigravity-acp` (`Drivers/AntigravityUsage.ts:37-99`, `antigravityUsageReader.ts`).

### 4.3 Запуск

Скачивается бинарь `agy-acp-server` (`antigravityRelease.ts:24-79`, `dl.google.com/agy-extensions/releases/...`),
запускается как ACP-агент по stdio: args `["--uid="]` на linux, env
`GEMINI_HOME=<profile>`, `AGY_ACP_FORCE_FILE_STORAGE=1`, `BROWSER=<node-хелпер>`,
`ELECTRON_RUN_AS_NODE=1`, `ANTIGRAVITY_HARNESS_PATH` (`antigravityAuthSupport.ts:194-231, 422-448`).
Логин Google делает сам ACP-агент, токен кладёт в `antigravity-acp/acp_token.json`.

### 4.4 Мульти-аккаунт

Каталог профиля: `stateDir/providers/antigravity/<sha256(instanceId)>`
(`resolveAntigravityInstanceDirectories`, `antigravityAuthSupport.ts:178-188`) — хэш, чтобы
регистро-различимые id не слились на case-insensitive ФС. Значит: инстанс = отдельный Google-логин
в отдельном `GEMINI_HOME`. Переключение — переключением инстанса.

### 4.5 Rust

Лимиты можно целиком по HTTP, без CLI, но: (1) нужен `acp_token.json`, который создаёт только
ACP-агент при логине; (2) адрес внутренний. Практично: логин отдать CLI/ACP, чтение лимитов — Rust.

---

## 5. Cursor

### 5.1 Лимиты

`packages/provider-cursor/src/server/usageLimits.ts:64` `readCursorUsageLimits`.

- Источники токена по порядку: env `CURSOR_AUTH_TOKEN`; иначе macOS Keychain
  (`CursorKeychain.ts`, только при явном `allowKeychain`, иначе `unavailable` с просьбой включить);
  иначе файл `auth.json {accessToken}` в папке Cursor: Windows `%APPDATA%\Cursor`, macOS `~/.cursor`,
  Linux `$XDG_CONFIG_HOME/cursor` (строки 109-127). Если задан `CURSOR_API_KEY` без
  `CURSOR_AUTH_TOKEN` — `unsupported` (строки 80-83).
- Endpoint: `POST {endpoint}/aiserver.v1.DashboardService/GetCurrentPeriodUsage`,
  endpoint по умолчанию `https://api2.cursor.sh` (можно переопределить `apiEndpoint` или
  `CURSOR_API_ENDPOINT`), заголовки `connect-protocol-version: 1`, `x-cursor-client-type: cli`,
  тело `{}` (строки 130-139). **[неофиц.]**
- Ответ: `{billingCycleEnd, planUsage{totalPercentUsed, autoPercentUsed, apiPercentUsed}}`
  (строки 24-33) → до трёх окон `kind:"monthly"`, `resetsAt` из `billingCycleEnd` (строки 36-62).
  Комментарий: «проценты дашборда включают бонусный расход, spend/limit — нет».
- Таймаут 10 с, ошибка → `probeFailed` (строки 144-153).

### 5.2 Контекст и токены

- За ход: из `@cursor/sdk` (`RunResult`/`InteractionUpdate`), контекст и токены ведёт
  `CursorAgentSdk.ts`.
- История за период: `POST https://cursor.com/api/dashboard/get-filtered-usage-events`
  с `Origin: https://cursor.com`, пагинация `{page,pageSize,startDate,endDate}`
  (`accountUsage.ts:52-87,188`). **[неофиц.]**

### 5.3 Запуск

Не CLI, а **npm-пакет `@cursor/sdk` в процессе** (`CursorAgentSdk.ts:8`, `CursorSdk.ts`):
`Cursor.createAgent/resume`, сообщения через `send`, дельты через колбэк `onDelta`.
Модель/настройки — `AgentOptions`. Отсюда: у Cursor запуск агента нельзя повторить «через CLI».

### 5.4 Мульти-аккаунт

Отдельный `SdkCredentialStore` на инстанс + `ProviderCredentialStore` (`auth.ts:42-113`),
либо свой `CURSOR_API_KEY` в env инстанса. `withAccess` закрывает доступ при logout, чтобы
кэш creds не пережил выход (строки 116-120).

### 5.5 Rust

Лимиты — да (неофиц. dashboard RPC + `auth.json`), история — да (тот же dashboard), агент — нет
(только SDK/собственная интеграция с их backend).

---

## 6. Grok (xAI)

### 6.1 Лимиты

`packages/provider-grok/src/server/usageLimits.ts:160` `readGrokAccount`.

- Креды: env `GROK_AUTH` (JSON) либо `<GROK_HOME|~/.grok>/auth.json`; запись выбирается строго по
  ключам `"https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828"` или
  `"https://accounts.x.ai/sign-in"`, поле `{key, auth_mode, email}` (строки 18-26, 144-157).
  Если `XAI_API_KEY` задан, или включён любой из `GROK_OIDC_*`/`GROK_OAUTH2_*`/`GROK_*_BASE_URL`/
  `GROK_CONFIG*`, или в `config.toml` есть секции `[auth]`/`[grok_com_config]`/`[endpoints]` —
  лимиты **не читаются** (`unsupported`), чтобы не показать чужой аккаунт (строки 97-143).
- Endpoint: `GET https://cli-chat-proxy.grok.com/v1/billing?format=credits` Bearer `key`
  (строка 181). **[неофиц.]**
- Ответ: `{config:{creditUsagePercent, currentPeriod{type,start,end}}}`; тип
  `USAGE_PERIOD_TYPE_WEEKLY|MONTHLY` → окно `subscription` с `kind weekly|monthly`; если процента нет,
  но период валиден — 0% (строки 63-92).
- Таймаут 10 с; нет токена → `unsupported`, ошибка → `probeFailed`.

### 6.2 Контекст и токены

- ACP `usage_update` (общий механизм `provider-acp`).
- История: `<GROK_HOME|~/.grok>/sessions/**/updates.jsonl` (`usage.ts:201-209`), поля
  `inputTokens/outputTokens/cachedReadTokens/cacheCreationTokens/reasoningTokens/costUsdTicks`;
  `costUsdTicks / 1e10` = USD (строки 17-22).

### 6.3 Запуск

ACP по stdio (`acpSupport.ts:47-58` `grokAcpSpawnArgs`):

```
grok agent stdio
grok --permission-mode default|auto agent stdio
grok agent --always-approve stdio
```

env `GROK_OAuth2_REFERRER=t3code`; auth-метод `xai.api_key` при `XAI_API_KEY`, иначе `cached_token`
(строки 77-81). Модель/`reasoningEffort` — через `session/set_model`-механику ACP
(`applyGrokAcpModelSelection`, строки 205-231).

### 6.4 Мульти-аккаунт

Через env инстанса (`GROK_HOME`, `XAI_API_KEY`, OIDC-переменные). Файл `auth.json` общий, поэтому
разные аккаунты = разные `GROK_HOME` или разные переменные.

### 6.5 Rust

Лимиты — да (неофиц. proxy-endpoint), история — да (jsonl), агент — только CLI/ACP.

---

## 7. OpenCode

### 7.1 Лимиты (OpenCode Go / Zen)

`packages/provider-opencode/src/server/usageLimits.ts:75` `readOpenCodeGoUsageLimits`.

- Не читает, если инстанс смотрит на внешний сервер (`serverUrl`) — «чужие креды не трогаем».
- Откуда ключ, по порядку (строки 88-121):
  1. `OPENCODE_AUTH_CONTENT` (JSON) → `["opencode-go"].key`, `type:"api"`;
  2. `<XDG_DATA_HOME|~/.local/share>/opencode/auth.json` — то же;
  3. SQLite `<dataHome>/opencode/<OPENCODE_DB|opencode.db>`, таблица `credential`,
     `integration_id in ('opencode','opencode-go')`, `active IS NOT 0`,
     `value` = JSON `{type:"key", key, metadata?.server}`, `busy_timeout=100`;
     если `metadata.server` задан и его origin+path != `https://opencode.ai/console` — ключ не берётся.
- Endpoint: `GET https://opencode.ai/zen/go/v1/usage` Bearer (строка 127). **[неофиц.]**
- Ответ: `{usage:{rolling{percent,resetsAt}, weekly{...}, monthly{...}}}` → три окна
  `go_rolling` (session, 300 мин), `go_weekly` (10080), `go_monthly` (строки 36-42, 142-166).
- 401/403 → пробует следующий ключ; 403 после перебора → `unsupported` (нет Go-подписки);
  таймаут 5 с; `credentialFingerprint = sha256("opencode-go\0"+key)` (строки 167-175).

### 7.2 Контекст и токены

- За шаг: `v2/adapter.ts:1585-1593` — `usedTokens = input + cache.read + cache.write + output`,
  `maxTokens` из окна модели.
- История: `opencode.db` таблицы `message` / `session_message` (поле `data` — JSON сообщения,
  `tokens{input,output,reasoning,cache{read,write}}`) либо legacy `storage/message/**/*.json`
  (`usage.ts:104-292`). Дедуп по `opencode:<messageId>`.

### 7.3 Запуск

HTTP-сервер + официальный SDK:

- `opencode serve --hostname=127.0.0.1 --port=<свободный>` (`OpenCodeRuntime.ts:510`), версия ≥1.14.19
  проверяется через `global.health` (строки 45, 160-196);
- клиент `@opencode-ai/sdk/v2` `createOpencodeClient({baseUrl, directory, headers})`, Basic auth
  `opencode:<OPENCODE_SERVER_PASSWORD>` (строки 477-489);
- события — SSE `client.event.subscribe` (`adapter.ts:2685`), промпт — `session.promptAsync`
  (`adapter.ts:2961`);
- окружение: `OPENCODE_CONFIG_CONTENT`, `OPENCODE_DB`, `OPENCODE_DATA_DIR`, `XDG_DATA_HOME`;
  ретрай 3× на `SQLITE_BUSY` (строки 715-723).

### 7.4 Мульти-аккаунт

`OpenCodeSettings` (`serverUrl`, `homePath`, …). Два способа:
свой процесс на инстанс (свой порт, свой `OPENCODE_CONFIG_CONTENT`/`XDG_DATA_HOME`) или подключение
к внешнему уже запущенному серверу по URL. История и креды разделяются каталогами данных.

### 7.5 Rust

Самый удобный провайдер для Rust: `opencode serve` → обычный HTTP + SSE, SDK не обязателен.
Лимиты Go — отдельный HTTP + чтение локального `auth.json`/`opencode.db`.

---

## 8. Muse

- Лимиты: отдельного HTTP нет. Приходят в сессии: нотификация `usage/changed` →
  `MuseSubscriptionUsage {window{usedPercent, windowDurationMins, resetsAtMs}, weekly{usedPercent, resetsAtMs}}`
  (`adapter.ts:1292-1297`, `protocol.ts:170-183`); на 429 разбирается текст отказа
  (`usageLimits.ts:6-34`). Без процесса не получить.
- Контекст: нотификация `session/contextUsage` → `usedTokens`, `windowTokens`
  (`adapter.ts:1302-1312`).
- Запуск: `muse serve [--trust-workspace|--disable-shell --disable-write|--disable-sandbox|--no-session-log]`
  (`sdk.ts:130-142`), JSON-RPC 2.0, LF-разделение, stdio (`sdk.ts:152-166`).
- Мульти-инстанс: бинарь/env/home на инстанс.
- Rust: только через stdio-протокол.

---

## 9. Pi

- Лимиты: нет (локальный harness, подписки не знает).
- Контекст: `stats.contextUsage {tokens, contextWindow}` (`adapter.ts:661-664`),
  `get_state`/`get_available_models` (`status.ts:174-178`).
- Запуск: `pi --mode rpc` (`mcpInjection.ts:263-264`), JSON-RPC-подобные кадры LF по stdio
  (`rpc.ts:1` — «Pi frames are LF-delimited»).
- История: `~/.pi/agent/sessions/**/*.jsonl` (`usage.ts:131-149`).
- Rust: только CLI-протокол.

---

## 10. ACP-реестр (Kimi, Copilot, Devin, Droid, claude-acp, …)

- Реестр: `https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json`
  (`AcpRegistrySupport.ts:50`), кэш `…/acp-registry/registry.json`, скачивание архивов под платформу,
  запуск команды с args (`["acp"]`, `["acp","--stdio"]` и т.п.).
- Лимиты: своего API нет, только ACP `usage_update {used,size,cost}` внутри сессии.
- Мульти-инстанс: `acpRegistry_<agentId>`, ввод/env через общий визард.
- Rust: ACP-клиент по stdio реализуем; лимитов, кроме in-band, нет.

`fx` — ACP-подобный драйвер без лимитов и usage.

---

## 11. T3 уже умеет «много подписок одного провайдера» через hub (CLIProxyAPI)

Это прямо про задачу pooprusteek, поэтому отдельно.

`apps/server/src/usage/UsageLimitSources.ts` + `apps/server/src/usage/cliproxyApi.ts`.

Настройка (`settings.ts:856`):

```
usageLimitSources[id] = { kind: "cliproxy", url, managementKey, enabled, label? }
```

Management API (все запросы `Authorization: Bearer <managementKey>`, таймаут 15 с):

| запрос | смысл |
| --- | --- |
| `GET {url}/v0/management/auth-files` | список аккаунтов: `{files:[{id, auth_index, provider, email, disabled, id_token{chatgpt_account_id, plan_type, plan_type}}]}` |
| `POST {url}/v0/management/api-call` | прокси-вызов от имени аккаунта: `{auth_index, method, url, header, data?}`; в `header` плейсхолдер `Authorization: Bearer $TOKEN$` |
| `POST {url}/v0/management/reset-quota` | сброс кулдауна аккаунта: `{auth_index}` |

Дальше T3 сам дергает:

- Claude: `GET https://api.anthropic.com/api/oauth/usage`, заголовки `anthropic-beta: oauth-2025-04-20`
  и **обязательный** `User-Agent: claude-code/2.1.273` (комментарий: «endpoint can return 429 without
  a Claude Code user agent», `cliproxyApi.ts:218`);
- Codex: `GET https://chatgpt.com/backend-api/wham/usage` + `Chatgpt-Account-Id`.

и нормализует оба ответа в ту же форму `ServerProviderUsageLimits` (`cliproxyApi.ts:254-352`).
Опрос — тем же 5-минутным циклом, `concurrency: 4` (`UsageLimitSources.ts:95-99`), плюс отдельный
`consumeResetCredit` с идемпотентным `redeem_request_id`.

Вывод: в T3 уже есть готовый «агрегатор подписок» — но как клиент чужого хаба. Для pooprusteek это
хороший образец: (а) management-слой со списком аккаунтов и прокси-вызовом от имени аккаунта,
(б) идемпотентные id для списаний, (в) нормализация ответов разных провайдеров в одну схему окон.

---

## 12. `E:\Projects\Me\t3-fork` — полезно или нет

Верхний уровень: `code/`, `core/`, `docs/`, `.memories/`, `.build/`, `.sandbox/`, `scripts/`, `AGENTS.md`.

- `code/` — тот же монорепо T3 (форк, с `node_modules` и добавленным `workbench/`), т.е. копия
  `E:\Projects\Forks\t3code`, только старее. Кода про лимиты/токены, которого нет в основном
  репозитории, там нет.
- `core/` — **отдельный** сервис `t3-core` (Bun, `Bun.serve`, Postgres, API + React UI): проекты,
  heartbeat, снапшоты, очередь экспорта, `t3 thread start`. К подпискам/лимитам отношения не имеет.
- `docs/` — `portable.md` (портативный Windows-инсталлятор `T3-Code-Diary-Setup.exe`, `T3CODE_HOME`,
  `launch.cmd`), `vds.md`, `session-sync.md`, `marathon-2026-11-05.md`.
- `.memories/` — база знаний форка (`reference/diary-integration.md`, `cloud-sync.md`,
  `vds-deployment.md`, `session-sync.md`, `windows-installer.md`, `repos-and-remotes.md`).

Вердикт: **для задачи «лимиты/контекст/мульти-инстансы» бесполезно** — это деплой и форк-обвязка.
Полезно может быть только как контекст, если pooprusteek будет стыковаться с их форком (session-sync,
портативная сборка, VDS). Брать код оттуда не нужно.

---

## 13. Что реально переносится в Rust

### 13.1 Прямой HTTP, без CLI

| что | запрос | статус адреса |
| --- | --- | --- |
| Claude лимиты + reset-кредиты | `GET api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1` | **[неофиц.]**, тот же, что у Claude Code |
| Claude списание кредита | `POST api.anthropic.com/api/organizations/<org>/reset_rate_limits` | **[неофиц.]** |
| Codex лимиты | `GET chatgpt.com/backend-api/wham/usage` | **[неофиц.]** |
| Codex кредиты | `GET/POST chatgpt.com/backend-api/wham/rate-limit-reset-credits[/consume]` | **[неофиц.]** |
| Antigravity квоты | `POST {daily-cloudcode-pa|cloudcode-pa}.googleapis.com/v1internal:retrieveUserQuotaSummary` | **[неофиц.]**, внутренний Google |
| Cursor лимиты | `POST api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage` | **[неофиц.]** |
| Cursor история | `POST cursor.com/api/dashboard/get-filtered-usage-events` | **[неофиц.]** |
| Grok лимиты | `GET cli-chat-proxy.grok.com/v1/billing?format=credits` | **[неофиц.]** |
| OpenCode Go лимиты | `GET opencode.ai/zen/go/v1/usage` | **[неофиц.]** |

### 13.2 Только через CLI / локальный протокол

- агентские ходы всех провайдеров (нет публичного chat API под подписку);
- Codex `account/rateLimits/read`, `thread/tokenUsage/updated` (через `codex app-server`);
- Claude control-protocol `get_usage` (эквивалент есть по HTTP);
- ACP-агенты (Antigravity, Grok, Kimi/Copilot/Devin/Droid): лимиты только in-band `usage_update`;
- Muse (`muse serve`, `usage/changed`, `session/contextUsage`);
- Pi (`--mode rpc`);
- Cursor: только npm SDK, CLI нет вообще.

### 13.3 Локальные файлы, которые нужны для «без CLI»

| провайдер | что читать |
| --- | --- |
| Claude | `<CLAUDE_CONFIG_DIR|~/.claude>/.credentials.json` (`claudeAiOauth.accessToken`), `~/.claude.json` (`oauthAccount.organizationUuid`), `projects/**/*.jsonl` |
| Codex | `<CODEX_HOME>/auth.json`, `sessions/**/rollout-*.jsonl` |
| Antigravity | `<state>/providers/antigravity/<sha256(instanceId)>/antigravity-acp/acp_token.json`, `antigravity-acp/conversations` |
| Cursor | `auth.json` (`%APPDATA%\Cursor`, `~/.cursor`, `$XDG_CONFIG_HOME/cursor`), macOS Keychain |
| Grok | `~/.grok/auth.json`, `~/.grok/sessions/**/updates.jsonl` |
| OpenCode | `$XDG_DATA_HOME/opencode/auth.json`, `opencode.db` (`credential`, `message`), `OPENCODE_AUTH_CONTENT` |
| Muse | muse home, `runtime.session` записи |
| Pi | `~/.pi/agent/sessions/**/*.jsonl` |

### 13.4 Риски

- **Юридические/ToS**: все «прямые» адреса — внутренние API клиентов. Агрегация многих подписок
  одного провайдера и раздача токенов между процессами (как `openai_token_sharing` в T3) может
  нарушать условия; T3 идёт на это только для собственных аккаунтов пользователя и с явным скоупом
  `chatgpt.tokens.use.direct`.
- **Хрупкость**: проверка User-Agent (Claude 429 без `claude-code/*`), привязка к `Origin`,
  `x-cursor-client-type`, `connect-protocol-version`, версия в UA Antigravity (`cl=963137146`).
  Любой апдейт клиента может сломать.
- **Ротация refresh-токенов**: два процесса с одним refresh-токеном сожгут друг друга. T3 решает
  файловой блокировкой (`CodexChatGptSessionLock.ts`) и `profileKey`-привязкой; в Rust нужен
  тот же приём (lock-файл + сравнение identity перед использованием).
- **Хранение**: T3 кладёт свои креды в `<secretsDir>/provider-auth-<hash>.bin` с 0600 и каталогом
  0700; чужие креды (Claude/Codex/Cursor) остаются в родных файлах. В Rust минимум — 0600 и запрет
  логов с токенами (T3 специально не пишет тело ответа хаба в ошибки, `cliproxyApi.ts:112-127`).
- **macOS Keychain**: Claude `.credentials.json` на darwin не читается (T3 берёт логин из Keychain
  через CLI), Cursor Keychain — только по явному разрешению. В Rust это отдельная ветка кода.

---

## 14. Итоговая таблица

| провайдер | лимиты (источник, частота) | контекст/токены | как запустить | мульти-аккаунт | как повторить в Rust |
| --- | --- | --- | --- | --- | --- |
| **Claude** | SDK `get_usage` (проба 5 мин) + события `rate_limit_event`; HTTP `api.anthropic.com/api/oauth/usage` **[неофиц.]** | `SDKResultMessage.usage` → `TurnTokenUsage`; `usedTokens/maxTokens` по каталогу моделей; история `projects/**/*.jsonl` (дедуп по `messageId:requestId`) | SDK `query()` c `pathToClaudeCodeExecutable` (CLI внутри); одиночный вызов — `claude -p --output-format json …` | инстанс = свой `CLAUDE_CONFIG_DIR` (`homePath`) → свой логин | HTTP-лимиты + парсинг `.credentials.json` и jsonl; запуск — через CLI |
| **Codex** | `codex app-server` → `account/rateLimits/read` (5 мин) + `account/rateLimits/updated`; HTTP `chatgpt.com/backend-api/wham/usage` **[неофиц.]** | `thread/tokenUsage/updated` (`last.totalTokens`, `modelContextWindow`); история `sessions/**/rollout-*.jsonl` | `codex app-server`, JSON-RPC JSONL по stdio, `initialize`/`initialized` | `CODEX_HOME` + `shadowHomePath`; managed: OAuth в T3, профили `sessions[]/activeClientId`, раздача токена через локальный сокет | app-server JSON-RPC или wham/usage; история — парсинг rollout; запуск — CLI |
| **Antigravity** | Google OAuth refresh → `cloudcode-pa` `v1internal:retrieveUserQuotaSummary` / `fetchAvailableModels` **[неофиц.]**, проба 5 мин | ACP `usage_update{used,size,cost}`; история `antigravity-acp/conversations` | скачанный `agy-acp-server`, ACP по stdio (`--uid=` на linux), `GEMINI_HOME` на инстанс | инстанс = свой профиль `<state>/providers/antigravity/<sha256(id)>` = свой Google-логин | лимиты по HTTP (нужен `acp_token.json` от агента), запуск — ACP |
| **Cursor** | `POST api2.cursor.sh/...GetCurrentPeriodUsage` **[неофиц.]**, 5 мин | per-run из `@cursor/sdk`; история — dashboard `get-filtered-usage-events` **[неофиц.]** | npm `@cursor/sdk` в процессе, CLI нет | `SdkCredentialStore` на инстанс или `CURSOR_API_KEY` | лимиты/история по HTTP; агент — только через их SDK |
| **Grok** | `GET cli-chat-proxy.grok.com/v1/billing?format=credits` **[неофиц.]**, 5 мин | ACP `usage_update`; история `~/.grok/sessions/**/updates.jsonl` | `grok agent stdio` (ACP), `--permission-mode`/`--always-approve` | `GROK_HOME`/`XAI_API_KEY`/OIDC-переменные на инстанс | лимиты + jsonl; запуск — ACP |
| **OpenCode** | `GET opencode.ai/zen/go/v1/usage` **[неофиц.]**, ключ из `auth.json`/`opencode.db`; 5 мин | per-step tokens; история `opencode.db` / `storage/message` | `opencode serve` + HTTP/SSE, Basic auth, SDK не обязателен | свой процесс+данные на инстанс, либо внешний `serverUrl` | почти всё по HTTP + sqlite; самый простой для Rust |
| **Muse** | in-band `usage/changed` (только в сессии) | `session/contextUsage`; история в muse home | `muse serve`, JSON-RPC JSONL по stdio | бинарь/env/home на инстанс | только через CLI-протокол |
| **Pi** | нет | `stats.contextUsage`; история `~/.pi/agent/sessions/**/*.jsonl` | `pi --mode rpc`, JSONL по stdio | env/home на инстанс | только через CLI-протокол |
| **ACP-реестр (Kimi и др.)** | нет, только in-band `usage_update` | ACP `usage_update{used,size}` | установка из `cdn.agentclientprotocol.com`, ACP по stdio | `acpRegistry_<agent>` + env | ACP-клиент; лимитов нет |
| **Hub (CLIProxyAPI)** | `v0/management/auth-files` + `api-call` от имени аккаунта, ключ `managementKey` | — | — | список аккаунтов внутри хаба | готовый образец агрегатора: перенять схему |

---

## 15. Что не проверено / открытые вопросы

- Точные argv, которыми `@anthropic-ai/claude-agent-sdk` поднимает `claude` для чата, — внутри SDK,
  не читал (**[предположение]**: stream-json + control protocol).
- Формат `usage` в истории Antigravity (`antigravityUsageReader.ts` детально не разбирал) — нужен
  отдельный проход, если планируем считать их токены офлайн.
- Не проверял, как T3 ведёт себя при двух инстансах Claude с одинаковым `CLAUDE_CONFIG_DIR`
  (риск конфликта `.credentials.json` и refresh-ротации) — в коде защиты, аналогичной Codex-lock,
  для Claude не видел.
- Юридическая оценка «раздачи» токенов и агрегации подписок — вне кода, нужна отдельная проверка
  условий каждого вендора.
