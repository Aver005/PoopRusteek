# REFERENCE: DeepSeek Provider
> The LLM backend. Reverse-engineered DeepSeek **web** API (chat.deepseek.com), not the public API key product.
> Source: `src/provider/`. Last updated: 2026-11-07 (типизированные отказы
> `failure.rs`, расход в потоке, лимиты API, запрет редиректов). Before:
> 2026-07-07 (corrected from code: PoW WASM is now **embedded** via
> `include_bytes!`, a file on disk only overrides it; `build_body` always sends
> the literal `model: "deepseek-chat"`; fixed the `### LOCAL MEMORY` vs
> newest-turn label description). Before: 2026-07-04 (constructor gained
> `rate_limit_per_minute`; trait gained
> `session_identity`/`session_is_alive`/`adopt_session` for cross-restart
> session resume — see `reference/CONFIG.md`'s "Remote session resume" section
> for the full flow)

> Line numbers below (`deepseek.rs:NNN`) predate the module split into
> `src/provider/deepseek/{mod,http,session,stream,endpoints}.rs` — treat them as
> approximate section pointers, not exact; `http.rs` now owns rate limiting and
> retry/backoff, `mod.rs` owns the type + constructor.

## LLMProvider TRAIT (`src/provider/mod.rs:192`)

| Method | Signature | Purpose |
|--------|-----------|---------|
| `complete` | `async (CompletionRequest) -> AppResult<CompletionResponse>` | Non-streaming; accumulates all chunks |
| `complete_stream` | `async (CompletionRequest, tx: UnboundedSender<CompletionChunk>) -> AppResult<()>` | Streaming via mpsc |
| `model` | `() -> &str` | Model id string |
| `reset` | `async () -> AppResult<()>` | Reset session state (default no-op; DeepSeek impl clears `SessionState`) |
| `fetch_remote_session_messages` | `async (session_id) -> AppResult<Vec<ChatMessage>>` | Pull a remote DeepSeek session (default: error) |
| `fork` | `() -> Arc<dyn LLMProvider>` (:211) | Fresh-session sibling sharing config/token. DeepSeek rebuilds via `fork_session()` (:144) with a new `SessionState`; `FakeProvider` returns a new instance. Tested for session independence (`deepseek.rs:1773`). |
| `session_identity` | `() -> Option<(String, Option<i64>)>` | Sync (no I/O) read of the live `(session_id, parent_message_id)`. Default `None`; DeepSeek locks `session_state` and clones. Sampled by `App::auto_save_session` every turn to persist resumable identity. |
| `check_session` | `async (session_id) -> SessionLiveness` | Three-state, **never a `bool`**: `Alive` / `Gone(reason)` / `Unknown(reason)`. Default `Unknown` — a provider without server-side sessions must not invite callers to destroy a link it knows nothing about. DeepSeek asks `GET chat/history_messages?chat_session_id=…` via `fetch_remote_history_biz` (the biz_data call, **not** `fetch_remote_history`: liveness must not hinge on the message array parsing). Only an envelope refusal about that session (`data.biz_code != 0`, e.g. `invalid chat session id`) is `Gone`; network, non-2xx, HTML, an expired token (outer `code`) are `Unknown`. **Only `Gone` may lead to erasing the stored link** — `App::apply_session_availability` wipes on `Gone` and leaves the file alone on `Unknown`. |
| `adopt_session` | `async (session_id, parent_message_id) -> AppResult<()>` | Resume a previously-known remote session instead of creating a new one. Default no-op; DeepSeek sets `SessionState{session_id, parent_message_id, system_sent_for_session: true}` directly, skipping `chat_session/create`. |
| `rate_limits` | `() -> Option<RateLimitSnapshot>` | Последние заголовки лимитов ответа, общие для форков аккаунта. Шлюз читает их для `LimitSnapshot`; жёсткое решение — только по свежему источнику `Header`/`CliEvent`. |
| `native_tools` | `() -> bool` | Родной канал вызовов (`tools` в запросе) вместо `<tool_use>`. У DeepSeek всегда `false`; у записи `/providers` с `tools = "native"` — `true`. |
| `served_by` | `() -> Option<ServedBy>` | Кто отвечает на вызов, для заголовков `/serve`; умолчание `None`, шлюз отдаёт аккаунт/маршрут/отказы. |
| `context_window` | `async () -> Option<u32>` | Окно модели, если провайдер умеет его назвать. DeepSeek — `None`; `CompatClient` читает `context_length` из `GET /models` (расширение OpenRouter); CLI Claude — из `modelUsage[…].contextWindow`. |

`DeepseekProvider` is the only real impl; `FakeProvider` (`provider/fake.rs`, `#[cfg(test)]`) is the test double. `provider` is `Option<Arc<dyn LLMProvider>>` and lives **per `Conversation`** (each gets its own via `fork()`) — `None` when token is empty.

## CORE TYPES (`src/provider/mod.rs`)

- **`Role`** (:8) — `System|User|Assistant|Tool`, serde lowercase.
- **`ChatMessage`** (:18) — `role, content, name?, tool_call_id?, display_content?, tool_error, created_at, total_tokens?, model, status?, think_elapsed_secs, references_count, search_triggered`. Many fields `#[serde(skip)]` (display-only).
  - Constructors: `system` (:50), `user` (:68), `assistant` (:86), `tool` (:104), `tool_with_display` (:122).
- **`CompletionRequest`** (:164) — `messages, model, temperature, max_tokens, stream`.
- **`CompletionChunk`** — `content, finish_reason?, usage: Option<Usage>`.
- **`CompletionResponse`** — `content, finish_reason?, usage?`.
- **`Usage`** — `prompt_tokens, completion_tokens, total_tokens, cache_read_tokens?, cache_write_tokens?, reasoning_tokens?` (`src/provider/mod.rs`). `prompt_tokens` — **весь вход с кэшем** у Anthropic (`input_tokens` + `cache_read_input_tokens` + `cache_creation_input_tokens`) и OpenAI (`prompt_tokens` уже включает `cached_tokens`); у Gemini это `promptTokenCount`, а кэш лежит отдельно в `cache_read_tokens` (`cachedContentTokenCount`). У DeepSeek web расхода нет (оценка `len/4`).
- **Когда приходит расход.** Anthropic/Gemini — в терминальном чанке вместе с `message_delta`/`usageMetadata`. OpenAI — **отдельным чанком после `finish_reason`** (`stream_options.include_usage`); поэтому читатель, который останавливается на `finish_reason`, расхода не увидит. `gateway::api_backend::drive_provider` дочитывает поток до конца; `/serve` ждёт такой чанк не дольше `USAGE_GRACE` = 2 с после `finish_reason` и только если клиент просил `include_usage`. 200 без `usage` — «расхода нет», а не ошибка.

## DEEPSEEK CLIENT (`src/provider/deepseek.rs`)

- **Constructor** `new(config, rate_limit_ms, rate_limit_per_minute, max_retries)` (`deepseek/mod.rs`). `max_retries`: -1=infinite, 0=none, N=N+1 attempts.
- **Base URL**: `https://chat.deepseek.com/api/v0` (:20).
> **У конверта два кода, и при деловом отказе внешний нулевой.** Успех —
> `{code: 0, data: {biz_code: 0, biz_data: …}}`; несуществующая сессия — это
> HTTP 200, `code: 0`, `data.biz_code: 1`, `biz_msg: "invalid chat session id"`,
> `biz_data: null`. Проверять только `code` значит принимать отказ за успех с
> пустыми данными (так и вышло — `BUGS.md`, RESOLVED 2026-09-06 (2)). Разбор
> один на весь клиент: `http::api_refusal` → `ApiRefusal::{Transport, Business}`,
> где `Transport` (внешний код) про запрошенный объект не говорит ничего, а
> `Business` — говорит.
>
> **Читая любой ответ этого API, идите через `DeepseekProvider::read_json`.**
> Снесённый путь отдаёт 200 OK и HTML-оболочку сайта, а не 404, поэтому
> `response.json()` падает ошибкой serde про неожиданный `<`, по которой
> невозможно догадаться о причине. `read_json` смотрит на `content-type` (а при
> пустом — на первый символ тела) и говорит прямо: «endpoint answered text/html
> in N bytes instead of JSON — the API path is probably gone».

- **Auth**: cookie/token session (NOT an API key). `auth_headers()` sets `Authorization: Bearer {token}` + the client identity from `deepseek/client.rs` (`insert_identity`): the **real web client's** headers — `x-client-platform: web`, `x-client-version: 2.5.0` (`DEFAULT_CLIENT_VERSION`, override `[provider] client_version`), `x-client-bundle-id: com.deepseek.chat`, `x-client-locale: en_US`, a Chrome 153 UA.
  - ⚠ **Version gate (`40005 CLIENT_VERSION_TOO_LOW`)**: the server refuses old clients per platform, in waves, per account. Until 2026-09-24 we spoofed `android 1.8.0` — exactly the android floor — and some accounts were cut off. `web` had no floor when probed. The version also changes response *shape* (web `1.0.0` puts the session straight into `biz_data`), so never invent one: copy it from DevTools → Network. Full probe log: `JOURNAL/2026-09-24-client-version-40005.md`.
  - **Refusals arrive as `200 OK` + JSON** on every endpoint, streaming ones included: `chat/completion` answers `application/json` `{"code":40300,"msg":"MISSING_HEADER"}` instead of an event stream. `send_request` checks `is_json_response` before streaming (otherwise the line is skipped and the turn ends as an empty success); the PoW challenge checks `api_refusal` before the typed parse. Codes 40005/40003 get a what-to-do hint (`client::refusal_hint`).
- **`SessionState`** (`deepseek/session.rs`) — `session_id?, parent_message_id?, system_sent_for_session, held_system_prompt?, pending_system_prompt?`. Held in a `Mutex`. Tracks DeepSeek-side conversation continuity; the system prompt is sent in full once per session and **re-sent whole when it changes** (since 2026-09-30).
  - `plan_system_delivery(system_prompt) -> SystemDelivery`: `Fresh` (not sent yet) / `Held` (server holds exactly this text) / `Changed` (anything else, incl. an adopted session whose prompt is unknown). The sent text goes to `pending_system_prompt`; `mark_session_after_success` promotes it to `held_system_prompt` — same acknowledgement point as `system_sent_for_session`, so a send that never reached the server is re-sent next time.
  - Why: the prompt is rebuilt every turn, but before this the provider sent it only once, so a skill enabled mid-conversation, an MCP server connected after the first message, or a reloaded `AGENTS.md` never reached DeepSeek. `MCPManager::get_all_tools`/`get_all_resources` are sorted so a reconnect does not reorder the prompt and trigger a false resend.
  - ⚠ **Session-fork hazard (fixed)**: the DeepSeek session is a tree keyed by `parent_message_id`; a stale id silently forks onto an *invisible* branch — messages show in the TUI but never reach the web view/model context. Interrupted/errored streams used to desync it. Fix (`183712e`): persist `parent_message_id` incrementally + flush-on-error. Per-conversation `fork()` isolation prevents cross-conversation desync structurally.

### Reverse-engineered endpoints (~30, declared :19–75)
- **Chat**: `chat/create_pow_challenge`, `chat/completion` (SSE), ~~`chat/history`~~ (**removed upstream 2026-09 — answers 200 OK + the site's HTML shell, not 404; see `BUGS.md`**), `chat/history_messages` (GET, `?chat_session_id=`), `chat/edit_message`†, `chat/regenerate`†, `chat/continue`, `chat/stop_stream`, `chat/resume_stream`, `chat/message_feedback`.
- **Sessions**: `chat_session/create`, `.../fetch_page`, `.../delete`, `.../delete_all`, `.../update_title`, `.../update_pinned`.
- **Files**: `file/upload_file`† (multipart), `file/fetch_files`, `file/fork_file_task`. **Used for binary attachments only** (PDF, images, Office, non-UTF-8 text; `deepseek/attachments.rs`, list `UPLOADABLE`): the server extracts their text. Text files are *not* uploaded — measured 2026-09-30, an upload costs the same tokens as the same text in `prompt` (2.4 MB: 407,114 vs 411,732 incl. our ~4.6K system prompt); skills use the same frame inline (`prompt::attached_file`). Flow: PoW for **the endpoint's own path** (`pow_target_path`; a completion PoW is refused), multipart without our JSON `Content-Type` (else 400 «Invalid boundary»), then poll `fetch_files` until `SUCCESS` (`token_usage`), `error_code` is a **number** on rejection (`40000` for a broken PDF) → `ref_file_ids`. Only the tail's attachments are uploaded; a fresh-session replay notes old ones instead of re-uploading. A failed file becomes a `### NOTE`, never a failed turn. Expert mode: the web UI blocks uploads since 2026-05-14, but the API accepted one on 2026-09-30. Details: `JOURNAL/2026-09-30-attachments.md`.
- **Share**: `share/create|list|content|delete|fork`.
- **Search**: `index/prepare`, `index/query`.
- **User**: `users/current|settings|update_settings|logout_all_sessions|set_birthday`.
- **Client/telemetry**: `client/settings`, `client/settings/report`, `client/span`.
- **Export**: `download_export_history`, `export_all`.
  († requires PoW.)

## PROOF-OF-WORK (`src/provider/pow.rs`) — the crux of DeepSeek access

Every PoW-gated call needs a solved challenge in the `x-ds-pow-response` header (`deepseek.rs:290` `get_chat_headers`).

1. `POST chat/create_pow_challenge` with `{ "target_path": "/api/v0/chat/completion" }`.
2. Response → `PowChallengeData` (:33): `algorithm, challenge, difficulty(str), salt, signature, target_path, expire_at`.
3. `solve_pow()` (:71): only `"DeepSeekHashV1"` supported. Builds prefix `"{salt}_{expire_at}_"`, runs the WASM solver.
4. SHA-3 WASM solver run via **`wasmtime`** (`WasmPowRuntime::load`, `pow.rs`). The blob is **embedded in the binary** — `const EMBEDDED_WASM = include_bytes!("../../assets/sha3_wasm_bg.7b9ca65ddd.wasm")` (`pow.rs:14`), so a release binary is self-contained. A wasm file found on disk **overrides** the embedded copy (dev checkout / manual drop-in of a newer upstream solver, no rebuild needed). Exports used: `memory`, `__wbindgen_add_to_stack_pointer`, `__wbindgen_export_0` (alloc), `wasm_solve`. Result region: `[status:i32 | pad | value:f64]`; `status==0` → no answer; else answer = `value.floor() as u64`.
5. `encode_solution()` (:194): JSON → base64 → header value.
6. On-disk override lookup (`resolve_wasm_path`, `pow.rs:255`): first existing of `CARGO_MANIFEST_DIR/assets/…` → CWD/`assets/…` → exe-dir/`assets/…`; if none exists, falls back to `EMBEDDED_WASM`. Runtime cached in a `OnceLock`.

**Fragility**: no check that the challenge hasn't expired before submit; slow solves can send a stale challenge with no retry.

## SSE STREAMING (`deepseek.rs:841`+)

- `process_stream_line()` (:841): requires `data:` prefix; skips empty/`[DONE]`; unwraps up to 3 nested JSON layers; returns `(text_chunk?, parent_message_id?)`.
- `parse_sse_event()` (:887) → `ParsedSSEEvent` enum (`types.rs:260`): **named** events `ready|update_session|title|close`; **unnamed** events keyed by `o` (op: APPEND/SET/BATCH) + `p` (path) + `v` (value) → `FragmentAppend|ContentAppend|FieldSet|Batch|Response|TokenDelta|Unknown`.
- `extract_text_from_event()` (:656): tries `p` contains `/content`, then `v.response`, then ~9 OpenAI-style fallback paths (`choices[0].delta.content`, etc.).
- On `[DONE]`: emit final chunk with `finish_reason="stop"`, then `mark_session_after_success()` updates `parent_message_id` + `system_sent_for_session`.

## PROMPT ASSEMBLY (`build_prompt`, `src/provider/prompt.rs`)

DeepSeek web API takes a single `prompt` string, so history is flattened. Reworked 2026-07-13 around a **tail batch**: the "newest turn" is *everything after the last assistant message* (not just `messages.last()`), each piece rendered as its own section via `format_tail_message` — `### NOTE` (system notes, e.g. the semantic hint), `### USER INPUT`, `### TOOL RESULT: {name}`.
- **First send of a session** (`SystemDelivery::Fresh`): `system prompt` + `### LOCAL MEMORY` + flattened history *before the tail* + the tail sections. Inside `### LOCAL MEMORY`, each prior message is labelled by role via `format_history_message` → `[ASSISTANT]` / `[USER]` / `[SYSTEM]` / `[TOOL]`.
- **Subsequent sends** (`Held`): only the tail is sent (DeepSeek retains the rest server-side).
- **Changed prompt** (`Changed(FileChanges)`): `### SYSTEM PROMPT UPDATE` (`SYSTEM_UPDATE_HEADER`: "replaces the earlier instructions entirely") + file lines from `FileChanges::describe` + the whole new prompt, then the tail. Sent even when the tail is empty, because the session counts it delivered. `completion.context` in the trace logs `system_delivery`.
  - `FileChanges::between(held, current)` diffs the `[file name]: ` lines (the `attached_file` frame, used by skills). It names removed/added files and always lists the files active now, revoking "everything else attached earlier". For an adopted session (`held = None`) only the active list is possible. Why names: live 2026-09-30, the bare "replaces entirely" header left the model following a disabled skill it had already used in the conversation (0/1); with the active-list line, 2/3 stopped. **Disabling a skill in a live DeepSeek session is best-effort**: earlier replies in the history pull the model back; only a new session is reliable.
- **Every non-empty send ends with `FORMAT_REMINDER`** — a one-line RU recency anchor restating the `<tool_use>` format (~60 tokens/turn; a weak model holds format by recency far better than by primacy).
- The tail design is the fix for the 2026-07-13 CRITICAL: a trailing system hint used to be sent as the sole `### USER INPUT`, silently dropping the user's message (see `BUGS.md` RESOLVED).
- `is_first_conversational_send` (role-aware: system notes don't count) feeds `should_reset` in `stream.rs` — a hint accompanying the first user message must not mask a fresh conversation.
- So `### USER INPUT` / `### TOOL RESULT` / `### NOTE` mark **only the newest tail**; the `[ROLE]` labels are what appear inside the `### LOCAL MEMORY` history block. Don't conflate the two.
- **`### LOCAL MEMORY` is just a history-section label — it does NOT load `.memories/` files.** (Verified: nothing in `src/` reads `.memories/`.)
- Code blocks > 300 chars are stripped to `[...]` via regex (`strip_long_code_blocks`) to save tokens.
- **Request body** (`build_body`, `deepseek/stream.rs`) always sends the literal `"model": "deepseek-chat"`; the TUI's model string is parsed by `DeepseekMode` (`deepseek/mode.rs`) into three independent switches: model (`deepseek-chat` → Instant, `"default"` on the first turn and `null` later; `deepseek-expert` → `"expert"` every turn), `-think` → `thinking_enabled`, `-search` → `search_enabled`. `deepseek-reasoner` is kept as Expert + DeepThink. Unknown ids fall back to Instant with both off. All combinations verified live 2026-09-29 (Instant + think returns `THINK` fragments; Expert without think does not).
- **Output limit** (~16–17 KB of text per reply, live 2026-09-29): the stream ends with `{"p":"response/status","o":"SET","v":"INCOMPLETE"}` (plus `quasi_status` in a BATCH) and a clean EOF. `StreamEvent::incomplete` turns that into `Ending::Length`; the provider then calls the web client's own «Continue» — `POST /api/v0/chat/continue` with `{"chat_session_id","message_id","fallback_to_resume":true}`, **no PoW** (captured from the live web client) — up to `MAX_NATIVE_CONTINUES` times. That stream starts with a snapshot of the whole reply so far; `SnapshotSkip` drops what was already delivered. If continue fails, `finish_reason: "length"` goes up and `agent::continuation` continues by prompt.

## ОТКАЗЫ, РАСХОД, ЛИМИТЫ, РЕДИРЕКТЫ

### `src/provider/failure.rs`

Один разбор отказа на всех: `FailureClass` — класс по смыслу, а не по тексту
ошибки. Шлюз использует его как есть (`gateway::types::FallbackTrigger::matches`
сопоставляет класс триггеру маршрута).

| Класс | Код | Когда |
|---|---|---|
| `RateLimit { retry_after }` | `rate_limit` | 429 с `Retry-After` (секунды, HTTP-дата) |
| `QuotaExhausted { resets_at }` | `quota` | квота/оплата кончились; окно сброса, если известно |
| `SpendCap` | `spend_cap` | Anthropic `billing_error` — по типу ошибки, не по отсутствию заголовка (вживую не проверено) |
| `Overloaded` | `overloaded` | 503/529 |
| `Timeout` / `Network` | `timeout` / `network` | срок / обрыв |
| `Server` | `server_error` | 5xx без более точного смысла |
| `Auth` | `auth` | 401/403 без признаков блокировки |
| `AccountBlocked` | `account_blocked` | аккаунт заблокирован/приостановлен |
| `ContextTooLong` | `context_too_long` | вход больше окна |
| `ModelNotFound` | `model_not_found` | модели нет у аккаунта |
| `ContentFilter` | `content_filter` | фильтр содержимого |
| `BadRequest` | `bad_request` | запрос неверен сам по себе |
| `Cancelled` | `cancelled` | вызов отменён (создаёт `/serve`) |
| `Protocol` | `protocol` | ответ не разобрался |

`ProviderFailure` несёт `class`, `status`, `message` и снятые `limits`
(`RateLimitHeaders { retry_after, windows, observed_at }`). В приложении это
`AppError::Upstream(Box<ProviderFailure>)`; `AppError::failure_class()` и
`AppError::retry_after()` — тонкие обёртки. `Display` сохраняет прежний текст
`"{status}: {message}"`.

`ProviderFailure::from_response` разбирает статус, заголовки, тело и уже
вынутый текст ошибки. `Retry-After: 0` считается отсутствующим (берётся
backoff). Заголовки лимитов OpenAI (`x-ratelimit-*`) и Anthropic
(`anthropic-ratelimit-*`) превращаются в `HeaderWindow` с именем окна, пределом,
остатком и временем сброса. Windows у Gemini берутся из `retryDelay` ошибки.
Пустой набор окон и без `Retry-After` — `RateLimitHeaders::is_empty`, его
выбрасывают.

### `LLMProvider::rate_limits()`

Возвращает последние заголовки лимитов ответа, общие для форков аккаунта.
Шлюз переводит их в `LimitSnapshot` с источником `Header`; возраст снимка
хранится рядом. Жёсткое решение (`soft_pct`, `hard_pct`, `switch_at_pct`)
принимается только по источнику `Header` или `CliEvent` и возрасту ≤ 120 с.
`Ledger` (оценка по заявленному `requests_per_minute`) и `Declared` — только
показ. Устаревшее не решает.

### Расход в потоке

См. `Usage` выше. Ключевое следствие: OpenAI присылает расход **после**
`finish_reason`, поэтому любой потребитель, который останавливается на
`finish_reason`, обязан дочитать поток. В шлюзе это делает `drive_provider`;
в `/serve` насос ждёт чанк расхода до 3 с (или 1 с через шлюз) и только если
клиент просил `stream_options.include_usage`.

### Расход и окно контекста

- **`Usage` не выведен в `LLMProvider` как отдельный метод.** Индикатор
  заполнения окна у API-аккаунтов берёт прежнюю оценку `len/4`
  (`agent/runner.rs::context_used`): настоящий расход последнего вызова живёт
  в `CallReport`, но до трейта не доходит. Это открытый пункт (`BUGS.md`).
- `/serve` отдаёт расход клиенту только по просьбе; в ledger он попадает всегда
  (оценка помечается).

### Редиректы запрещены

Клиенты, которые кладут ключ в заголовок (`Authorization`, `x-api-key`,
`x-goog-api-key`), не следуют редиректам: `CompatClient` строит клиент с
`reqwest::redirect::Policy::none()` и на 3xx возвращает понятную ошибку с
адресом назначения и подсказкой «поправь `base_url`». Иначе ключ ушёл бы на
чужой хост. Passthrough `/serve` держит то же правило. DeepSeek не менялся:
его ключ и так уходит только на `chat.deepseek.com`, и поведение там прежнее.

Проверено настоящим локальным сервером: редирект на другой порт, три протокола,
«чужой» сервер не получает ни одного запроса
(`a_redirect_to_another_host_never_carries_the_key_for_any_protocol`); без
редиректа ключ доходит до настроенного хоста
(`without_a_redirect_the_key_still_reaches_the_configured_host`).

## RESILIENCE

- **Rate limit** (`enforce_rate_limit`, `deepseek/http.rs`): two independent, composable gates — a min spacing between requests (`rate_limit_ms`) and a sliding 60s-window request cap (`rate_limit_per_minute`, tracked via a `VecDeque<Instant>` in `request_history`); both set via `/rate` and either can be 0 to disable.
- **Retry/backoff** (`send_json_request`): `Retry-After` (секунды, HTTP-дата) соблюдается; без него — экспоненциальный `min(30s, 1000ms·2^(n-1))` на 5xx и сетевых ошибках. Джиттера и общего потолка времени нет → с `max_retries=-1` возможен бесконечный цикл. Ошибки типизированы (`ProviderFailure`), поэтому этот же `Retry-After` читает шлюз.
- У клиента DeepSeek есть `connect_timeout(10s)` и `read_timeout(120s)`; отдельного общего срока на весь запрос нет. Расход web-API не сообщает — оценивается `len/4`.

## Native tool calling (`[[providers]] tools = "native"`)

`LLMProvider::native_tools()` switches a `/providers` entry from `<tool_use>` text to the API's own channel. The loop declares builtin + all MCP tools (`runner::native_tool_definitions`) and the system prompt drops the call format and tool list (`{{call_format}}`/`{{tool_list}}` empty). Each `CompatProtocol` assembles streamed calls in its `StreamState` and emits them whole with the terminal chunk. History goes native only for a tool result that answers a call declared by an earlier assistant message in the same request; everything else keeps the prompt-path text. Gemini needs its call `id` and `thoughtSignature` back — carried in `ToolCall::provider_state`. The built-in DeepSeek web client has no tool channel and always uses the prompt path. Verified live 2026-09-29 against Ollama (OpenAI + Anthropic dialects); Gemini by tests only.
