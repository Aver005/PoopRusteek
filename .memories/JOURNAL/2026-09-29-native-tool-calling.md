# 2026-09-29 — родной tool calling для записей `/providers`

До этого `[[providers]] tools = "native"` существовал только в конфиге: его
никто не читал, цикл всегда слал пустой `tools`, а клиенты протоколов не
разбирали вызовы из ответа. Сторона агента (`CompletionChunk::tool_calls`,
`ChatMessage::tool_calls`, приоритет родных вызовов в `parse_step`) была
готова с 2026-08-30 — не хватало клиентов и проводки.

## Что сделано

- **Контракт.** `LLMProvider::native_tools()` (у `CompatClient` — из
  `entry.tools`). `CompatProtocol` получил `type StreamState`: вызовы OpenAI
  и Anthropic приходят кусками, наверх уходят только целыми.
- **Цикл.** `runner::native_tool_definitions` — встроенные + все MCP, один раз
  за ход; `build_step_request` принимает список. Суб-агент так же.
- **Промпт.** `tools.prompt.md` разбит: `{{call_format}}` ←
  `tool-calls.prompt.md`, `{{tool_list}}` ← `tool-list.prompt.md`. На родном
  пути оба пусты. Промптовый путь байт-в-байт прежний (сборка проверена
  `assert` при разбиении) — поведение DeepSeek, измеренное харнессом, не
  тронуто. Части исключены из скиллов (`discovery::SYSTEM_PROMPT_PARTS`).
- **Протоколы.** История: вызовы ассистента — структурой, результат — родным
  блоком, только если отвечает на вызов **более раннего** сообщения того же
  запроса; иначе прежний текст. Без инструментов и родной истории тело
  запроса прежнее (тесты на это есть у всех трёх).
  - OpenAI: сборка `delta.tool_calls` по `index`; сервер без `index` — «следующая
    позиция»; без id — свой `call_…`; без имени — в лог и мимо.
  - Anthropic: `content_block_start/delta(input_json_delta)/stop`,
    `stop_reason` → `tool_use`/`length`/`stop`.
  - Gemini: `functionDeclarations` (схема чистится от `$schema`, `$id`,
    `additionalProperties`), `functionCall` целиком, `STOP` с вызовами →
    `tool_calls`. **Gemini 3** (сверено в документации через Context7):
    историю надо возвращать с `thoughtSignature`, а `functionResponse` — с id
    вызова, иначе generateContent отвечает пустым `STOP`. Для этого у
    `ToolCall` появилось непрозрачное `provider_state`.
- **Сервер.** Запись с родным протоколом: `ToolBridge::declare` кладёт `tools`
  клиента в запрос, история не сплющивается в `<tool_use>`, ответ —
  `ToolBridge::native_reply` с id провайдера. `tool_choice` туда не
  передаётся — у `CompletionRequest` нет такого поля.
- **Команда.** `/providers tools <name> <native|prompt>` — сохраняет и
  пересобирает провайдера, если запись активна.

## Живая проверка (Ollama `qwen3:14b`, `.dev/native-config.toml`)

- OpenAI и Anthropic (`/v1/messages` у Ollama): `read_file` → результат →
  верный ответ; в трассе текст шага с вызовом пустой, у вызова нет формата
  разметки — пришёл структурой.
- Системный промпт 8,9 КБ против 20,5 КБ на промптовом пути.
- Сервер: запрос к `ollama-native/qwen3:14b` с `tools` → `tool_calls` с id
  самой Ollama, `prompt_tokens: 29`.
- Gemini живьём не проверен — ключа нет; только тесты.

## Как шла работа

Три протокола — три параллельных агента по непересекающимся файлам, проводка —
ведущим. Агент Gemini завис в ожидании своей сборки на ~30 минут; его код был
почти готов (один неверный тест), доделан вручную вместе с `provider_state`.

## Открыто

- `tool_choice` для родного пути на сервере.
- Родной ответ без стрима (`CompletionResponse` без `tool_calls`) — агенту и
  серверу не нужен, они идут стримом.
- Мастер `/providers add` выбора протокола инструментов не предлагает.

Тесты 1145 → 1178.
