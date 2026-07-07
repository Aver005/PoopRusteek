# Provider protocol mapping (wire-level)

Per-protocol reference for the four `LLMProvider` backends. Anchored to
`file::function`/`struct` names (line numbers drift). Verified against source
2026-07-07. Where `.memories/reference/PROVIDER.md` disagrees, the notes below
follow the code.

## `LLMProvider` trait surface (`provider/mod.rs`)

The abstraction every backend implements. Two required completion methods, one
model accessor, one isolation primitive; everything else has a default.

| Method | Signature | Default | Notes |
|--------|-----------|---------|-------|
| `complete` | `async (CompletionRequest) -> AppResult<CompletionResponse>` | required | Non-streaming; accumulates the whole answer. |
| `complete_stream` | `async (CompletionRequest, tx: UnboundedSender<CompletionChunk>) -> AppResult<()>` | required | Streams `CompletionChunk`s over an mpsc sender. |
| `model` | `() -> &str` | required | Configured model id. |
| `fork` | `() -> Arc<dyn LLMProvider>` | required | Isolation primitive — see below. |
| `list_models` | `async () -> AppResult<Vec<String>>` | `Err` (unsupported) | For `/models`. |
| `reset` | `async () -> AppResult<()>` | `Ok(())` | DeepSeek clears `SessionState`; others no-op (stateless). |
| `discard_remote_session` | `async () -> AppResult<()>` | `Ok(())` | Delete an ephemeral server session (DeepSeek only). |
| `list_remote_sessions` / `delete_remote_session_by_id` | `async …` | `Err` | `/delete` picker (DeepSeek only). |
| `fetch_remote_session_messages` | `async (session_id) -> AppResult<Vec<ChatMessage>>` | `Err` | Pull a remote DeepSeek session. |
| `session_identity` | `() -> Option<(String, Option<i64>)>` | `None` | **Sync**, no I/O — sampled every auto-save. Live `(session_id, parent_message_id)`. |
| `session_is_alive` | `async (session_id) -> bool` | `false` | Best-effort reachability; "couldn't tell" == "gone". |
| `adopt_session` | `async (session_id, parent_message_id) -> AppResult<()>` | `Ok(())` | Resume a known remote session instead of creating one. |

**Streaming vs non-streaming is chosen by which method the caller invokes**, not
by `CompletionRequest.stream`. That `stream` bool only rides into the
OpenAI-compatible *server* wire format (`openai_compat::request_to_openai`); each
client's `send()` sets the real streaming flag itself.

### Core types (`provider/mod.rs`)
- `Role` — `System | User | Assistant | Tool`, serde `lowercase`.
- `ChatMessage` — `role, content, name?, tool_call_id?, ui_only, …` plus many
  `#[serde(skip)]` display-only fields. `ui_only` messages are UI chrome and are
  **filtered out of every provider's request body** (each compat layer skips
  them), so decorating the UI can never change what the model sees.
- `CompletionRequest` — `messages, model, temperature, max_tokens, stream`.
- `CompletionChunk` — `content, finish_reason?`.
- `CompletionResponse` — `content, finish_reason?, usage?`.
- `Usage` — `prompt_tokens, completion_tokens, total_tokens`.

### `fork()` semantics
Creates a sibling that **shares configuration/connection but starts a fresh
session**. Each `Conversation` and sub-agent gets its own fork so concurrent
turns never collide on session state.
- **DeepSeek** (`deepseek/mod.rs::fork` → `fork_session`): clones the reqwest
  client (its pool is internally `Arc`'d) + all config, but installs a fresh
  `Mutex<SessionState::default()>` and fresh rate-limit clocks. Kept concrete so
  tests assert session independence (`deepseek/mod.rs` `fork_has_independent_session_state`).
- **OpenAI / Anthropic / Gemini**: stateless — `fork()` is a plain field copy
  (client, base_url, api_key, model). No session id, no `parent_message_id`.

### Construction (`provider/mod.rs`)
`build_provider(config)` picks the active `/providers` entry if set, else the
built-in DeepSeek client (or `None` when the DeepSeek token is empty).
`build_entry_provider(entry)` dispatches on `entry.protocol`
(`ProviderProtocol::{Openai,Anthropic,Gemini}`) to the matching `*_client`.

---

## DeepSeek — web API (PoW + SSE)

Reverse-engineered `chat.deepseek.com` web API, **not** the paid API-key product.
Source: `provider/deepseek/{mod,http,session,stream,endpoints}.rs`, plus
`provider/prompt.rs` (prompt assembly) and `provider/pow.rs` (proof-of-work).

**Endpoint** — `POST https://chat.deepseek.com/api/v0/chat/completion` (SSE
response). Constants in `deepseek/stream.rs` (`COMPLETION_URL`, `CREATE_POW_URL`,
`SESSION_HISTORY_URL`); the ~30-endpoint parity surface is in
`deepseek/endpoints.rs` (mostly `#[allow(dead_code)]`).

**Auth** — cookie/token session, **not an API key**. `deepseek/http.rs::auth_headers`
sets `Authorization: Bearer {token}` plus spoofed Android web-client headers:
`x-client-platform: android`, `x-client-version: 1.8.0`, `x-client-locale: zh_CN`,
`Host: chat.deepseek.com`, and a Chrome/YaBrowser desktop `User-Agent`
(`USER_AGENT` const). Every PoW-gated call additionally carries an
`x-ds-pow-response` header (`deepseek/stream.rs::get_chat_headers`).

**Proof-of-work** (`provider/pow.rs`, crux of access):
1. `POST chat/create_pow_challenge` with `{ "target_path": "/api/v0/chat/completion" }`
   (`deepseek/stream.rs::solve_pow_challenge`).
2. Response → `PowChallengeResponse` → `.data.biz_data.challenge` = `PowChallengeData`
   `{ algorithm, challenge, difficulty (str-or-number, custom deser), salt,
   signature, target_path, expire_at }`.
3. `pow::solve_pow`: only `"DeepSeekHashV1"` is accepted. Prefix is
   `format!("{salt}_{expire_at}_")`; the SHA-3 hash loop runs in a **WASM module
   via `wasmtime`**. Exports used: `memory`, `__wbindgen_add_to_stack_pointer`,
   `__wbindgen_export_0` (alloc), `wasm_solve`. Result region
   `[status:i32 | pad | value:f64]`; `status==0` → no answer, else
   `answer = value.floor() as u64`.
4. `pow::encode_solution`: serialize `PowSolution` → JSON → base64 → header value.
5. The solve is dispatched on `tokio::task::spawn_blocking`
   (`solve_pow_challenge`) — CPU-bound, must not stall the async workers.

> **Correction vs PROVIDER.md**: the WASM blob is now **embedded** via
> `include_bytes!("../../assets/sha3_wasm_bg.7b9ca65ddd.wasm")` (`EMBEDDED_WASM`),
> so an installed binary needs no `assets/` folder. A file on disk still wins —
> `resolve_wasm_path` probes `CARGO_MANIFEST_DIR/assets` → CWD → exe-dir; only
> when none exists does it fall back to the embedded bytes. Runtime cached in a
> `OnceLock<Result<WasmPowRuntime, String>>`.

**Request mapping** — the web API takes a single flat `prompt` string, not a
role array. `provider/prompt.rs`:
- `split_system_prompt` pulls the first `System` message out.
- `build_prompt(messages, system_prompt, system_sent_for_session)`:
  - **First turn of a session** (`system_sent_for_session == false`): system
    prompt, then (if there is prior history) a `### LOCAL MEMORY` section whose
    lines are `format_history_message` labels — `[ASSISTANT]` / `[USER]` /
    `[TOOL]` (assistant code fences ≥300 chars collapsed to `[...]` via
    `LONG_CODE_BLOCK_RE`), then the newest turn as `### USER INPUT` or
    `### TOOL RESULT: {name}`.
  - **Later turns**: only the newest input — `### USER INPUT\n…`, or a
    back-to-back batch of trailing tool results joined as
    `### TOOL RESULT: {name}\n{content}`.
  - `### LOCAL MEMORY` is only a history label — it does **not** load `.memories/`.
- `deepseek/stream.rs::build_body` wraps it: `{ prompt, model: "deepseek-chat"
  (always literal), model_type, stream: true, temperature, max_tokens,
  ref_file_ids: [], thinking_enabled, search_enabled: false, chat_session_id,
  parent_message_id }`. `model_type` comes from `prompt::resolve_model_type`:
  `"expert"` when the model name contains `reasoner`/`expert`, else `"default"`
  on the first message (`parent_message_id` is `None`), else omitted (`null`).
  `thinking_enabled = (model_type == "expert")`.

**Session lifecycle** (`deepseek/session.rs`) — `SessionState { session_id?,
parent_message_id?, system_sent_for_session }` behind a `Mutex`.
`send_request` decides `should_reset` (system already sent AND exactly one
non-system message ⇒ a new top-level turn), calls `ensure_session` (reuse or
`POST chat_session/create`, id at `data.biz_data.chat_session.id`), builds the
prompt, gets PoW headers, POSTs. The DeepSeek session is a **tree keyed by
`parent_message_id`**; a stale id silently forks onto an invisible branch, so
`mark_session_after_success` persists the id *incrementally and on error* (only
when the event's `session_id` still matches, and never clobbers a known id with
`None`).

**Streaming / parse** (`deepseek/stream.rs::process_stream_line`) — bytes framed
by `sse::SseLineBuffer`, one `data:` line at a time:
- Requires a `data:` prefix; empty payload → skip; `[DONE]` → `finished`.
- `normalize_event_payload` unwraps **up to 3 nested JSON string layers**.
- `extract_text_from_event`: primary path is DeepSeek's patch shape
  (`o` = op `APPEND`/`SET`, `p` = path containing `/content`, `v` = value; also
  `v.response.fragments[].content`), then ~9 OpenAI-style fallback paths
  (`choices[0].delta.content`, `.message.content`, `.text`, `data.…`, etc.).
- `extract_parent_message_id`: tries `response_message_id`, `parent_message_id`,
  `message_id`, `id`, `v.response.message_id`, etc.
- `event_signals_finished`: a `{"v":"FINISHED"}` status patch, a terminal
  `{"o":"BATCH","v":[…]}` containing one, or `v.response.status == "FINISHED"`.
- **Clean EOF is treated as a normal stop** (`finish_reason = "stop"`): the web
  endpoint routinely just closes the chunked body with no `[DONE]`. A severed
  connection surfaces as a read `Err` in the loop, not a clean EOF.

**Response extraction** — `complete` accumulates all text chunks into
`CompletionResponse { content, finish_reason: Some("stop"), usage: None }`
(DeepSeek never reports real token usage). `complete_stream` forwards each text
chunk and emits a final empty chunk with `finish_reason: Some("stop")`.

**Resilience** (`deepseek/http.rs`) — `enforce_rate_limit` applies two
composable gates: min spacing (`rate_limit_ms`) and a rolling-60s cap
(`rate_limit_per_minute`, via `VecDeque<Instant>`); either `0` disables it.
`send_json_request`/`send_get_request` retry on 5xx/network with
`retry_backoff` (exponential `1s<<n`, capped 30s, no jitter); `max_retries`
`-1`=infinite / `0`=none / `N`=`N+1` attempts.

---

## OpenAI Chat Completions

Source: transport `provider/openai_client.rs` (`OpenAiCompatProvider`), wire
format `provider/openai_compat.rs`. Targets LM Studio, Ollama `/v1`, vLLM,
OpenRouter, etc. Stateless.

**Endpoint** — `POST {base_url}/chat/completions` (`completions_url`); `base_url`
is the entry's, trailing slash trimmed (usually `…/v1`). Also
`GET {base_url}/models` for `list_models`.

**Auth** — `bearer_auth(api_key)` (`Authorization: Bearer …`) when the entry has
a key; omitted otherwise.

**Request mapping** (`openai_compat::request_to_openai`) — standard OpenAI shape:
`{ model, messages: [{role, content, name?, tool_call_id?}], temperature,
max_tokens, stream }`. Roles map straight (`role_to_openai`). `ui_only` messages
filtered. `openai_client::send` then overrides `body["stream"]` and, crucially,
`body["model"]` with the entry's configured model (the internal request's model
field is a DeepSeek-ism). Structured `tools`/`tool_calls` are **not** emitted —
pooprusteek's tool protocol is prompt-encoded text.

**Streaming / parse** — SSE reassembled by `sse::SseLineBuffer`; each `data:`
line stripped, `[DONE]` terminates. Payloads parse as
`ChatCompletionChunk { choices: [{ delta: Delta { role?, content?,
reasoning_content? }, finish_reason? }] }` → `chunk_from_openai` yields
`CompletionChunk { content, finish_reason }`. A malformed chunk is logged and
skipped, not fatal. Stream ending without `[DONE]` returns `Ok(())` (the runner
treats a missing stop as a provider error).

**Response extraction** — non-streaming parses `ChatCompletionResponse` →
`response_from_openai` takes `choices[0].message.content` (+ `finish_reason`,
real `usage`).

> This module is also the **server-side** boundary (pooprusteek exposing
> `POST /v1/chat/completions`): the inbound direction (`to_internal_request`,
> `response_to_openai`, `split_delta_chunk`/`final_chunk`) plus `<think>`→
> `reasoning_content` hoisting (`split_reasoning`, `ReasoningStreamSplitter`)
> live here too but are not part of the client path.

---

## Anthropic Messages

Source: transport `provider/anthropic_client.rs` (`AnthropicCompatProvider`),
wire format `provider/anthropic_compat.rs`. Stateless.

**Endpoint** — `POST {base_url}/messages`; `GET {base_url}/models` for
`list_models`.

**Auth** — `x-api-key: {key}` (**not** a bearer header) plus a pinned
`anthropic-version: 2023-06-01` (`ANTHROPIC_VERSION` const, set by
`with_headers`).

**Request mapping** (`anthropic_compat::request_to_anthropic`) — real
restructuring, not renaming:
- System messages are concatenated (`\n\n`) into a **top-level `system`** field,
  never a message.
- `messages` carry only `user`/`assistant` and **must strictly alternate**:
  consecutive same-role runs are merged with `\n\n`; internal `Tool` messages
  become `user` text prefixed `"[tool result]\n"`.
- An assistant-first (or empty) conversation gets a synthetic
  `("user", "(continue)")` opener prepended (the API rejects otherwise).
- Body: `{ model, messages, max_tokens (required), temperature, stream }` +
  `system` when present. `anthropic_client::send` overrides `model`/`stream`, and
  **drops `temperature` for sampling-rejecting families** — `model_rejects_sampling`
  returns true for `claude-opus-4-7`, `claude-opus-4-8`, `claude-sonnet-5`,
  `claude-fable-5`, `claude-mythos-5` (they 400 on any sampling param).

**Streaming / parse** — typed SSE events. `parse_stream_event` → `StreamEvent`:
`content_block_delta` (`delta.text`) → `Text`; `message_stop` → `Done` (emits a
terminal empty chunk with `finish_reason: "stop"`); `error` → `Error(message)`
(fails the stream); everything else (`message_start`, `content_block_start/stop`,
`ping`, unparseable) → `Ignore`.

**Response extraction** (`response_from_anthropic`) — non-streaming parses
`MessagesResponse { content: [ContentBlock{kind,text?}], stop_reason?, usage? }`;
concatenates only `kind == "text"` blocks (thinking blocks dropped). `stop_reason`
mapped via `map_stop_reason`: `end_turn`/`stop_sequence` → `"stop"`,
`max_tokens` → `"length"`, else passthrough. `usage.input_tokens` +
`output_tokens` → internal `Usage`.

---

## Gemini (Google Generative Language API)

Source: transport `provider/gemini_client.rs` (`GeminiProvider`), wire format
`provider/gemini_compat.rs`. Stateless.

**Endpoint** — model id lives **in the URL**, not the body
(`gemini_client::send`):
- non-stream: `POST {base_url}/models/{model}:generateContent`
- stream: `POST {base_url}/models/{model}:streamGenerateContent?alt=sse`
`base_url` is usually `…/v1beta`. `GET {base_url}/models` for `list_models`
(names come back `models/<id>`; the transport strips the prefix).

**Auth** — `x-goog-api-key: {key}` header (`with_key`).

**Request mapping** (`request_to_gemini`):
- History is `contents[].parts[].text` with roles `user`/**`model`** (assistant →
  `model`); consecutive same-role turns merged (`\n\n`); a `model`-first/empty
  history gets a `("user", "(continue)")` opener.
- `Tool` messages → `user` text prefixed `"[tool result]\n"`.
- System messages → top-level **`systemInstruction.parts[].text`**.
- Sampling knobs live under `generationConfig`
  (`temperature`, `maxOutputTokens`). Note: the request body carries **no
  `stream` flag** — streaming is purely the URL/`alt=sse` choice.

**Streaming / parse** — SSE with **no typed events and no `[DONE]`**; each
`data:` line is the same `GenerateContentResponse` shape as a full response.
`gemini_client::complete_stream` parses each, calls `extract_piece`, forwards
non-empty text, and terminates when a chunk carries a `finishReason`
(emitting a final empty chunk with the mapped reason). Malformed chunks logged
and skipped.

**Response extraction** (`extract_piece` / `response_from_gemini`) — first
candidate's `content.parts[].text` concatenated; `finishReason` mapped via
`map_finish_reason` (`STOP` → `"stop"`, `MAX_TOKENS` → `"length"`, else
lowercased passthrough, e.g. `SAFETY` → `"safety"`). `usageMetadata`
(`promptTokenCount`/`candidatesTokenCount`/`totalTokenCount`, camelCase) →
internal `Usage`.

---

## Shared SSE primitive (`provider/sse.rs`)

All four streaming paths (and the MCP HTTP transports) frame bytes with
`SseLineBuffer::push_bytes`: accumulates raw bytes, yields complete `\n`-delimited
lines, retains a trailing partial line. Decodes **per line, not per chunk**, so a
multibyte UTF-8 char split across network chunks never becomes `�`. Force-flushes
a single line past 4 MiB. Decoding the JSON inside each `data:` line is each
provider's own job (the event shapes differ).
