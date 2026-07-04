# REFERENCE: DeepSeek Provider
> The LLM backend. Reverse-engineered DeepSeek **web** API (chat.deepseek.com), not the public API key product.
> Source: `src/provider/`. Last updated: 2026-07-04 (constructor gained `rate_limit_per_minute`; see note below on file split)

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

`DeepseekProvider` is the only real impl; `FakeProvider` (`provider/fake.rs`, `#[cfg(test)]`) is the test double. `provider` is `Option<Arc<dyn LLMProvider>>` and lives **per `Conversation`** (each gets its own via `fork()`) — `None` when token is empty.

## CORE TYPES (`src/provider/mod.rs`)

- **`Role`** (:8) — `System|User|Assistant|Tool`, serde lowercase.
- **`ChatMessage`** (:18) — `role, content, name?, tool_call_id?, display_content?, tool_error, created_at, total_tokens?, model, status?, think_elapsed_secs, references_count, search_triggered`. Many fields `#[serde(skip)]` (display-only).
  - Constructors: `system` (:50), `user` (:68), `assistant` (:86), `tool` (:104), `tool_with_display` (:122).
- **`CompletionRequest`** (:164) — `messages, model, temperature, max_tokens, stream`.
- **`CompletionChunk`** (:173) — `content, finish_reason?`.
- **`CompletionResponse`** (:179) — `content, finish_reason?, usage?`.
- **`Usage`** (:186) — `prompt_tokens, completion_tokens, total_tokens`. NOTE: streaming responses never populate `usage` → always `None` in practice.

## DEEPSEEK CLIENT (`src/provider/deepseek.rs`)

- **Constructor** `new(config, rate_limit_ms, rate_limit_per_minute, max_retries)` (`deepseek/mod.rs`). `max_retries`: -1=infinite, 0=none, N=N+1 attempts.
- **Base URL**: `https://chat.deepseek.com/api/v0` (:20).
- **Auth**: cookie/token session (NOT an API key). `auth_headers()` (:144) sets `Authorization: Bearer {token}` + spoofed Android client headers (`x-client-platform: android`, `x-client-version: 1.8.0`, `x-client-locale: zh_CN`, a Chrome/YaBrowser UA).
- **`SessionState`** (:84) — `session_id?, parent_message_id?, system_sent_for_session`. Held in a `Mutex`. Tracks DeepSeek-side conversation continuity; system prompt sent once per session.
  - ⚠ **Session-fork hazard (fixed)**: the DeepSeek session is a tree keyed by `parent_message_id`; a stale id silently forks onto an *invisible* branch — messages show in the TUI but never reach the web view/model context. Interrupted/errored streams used to desync it. Fix (`183712e`): persist `parent_message_id` incrementally + flush-on-error. Per-conversation `fork()` isolation prevents cross-conversation desync structurally.

### Reverse-engineered endpoints (~30, declared :19–75)
- **Chat**: `chat/create_pow_challenge`, `chat/completion` (SSE), `chat/history`, `chat/history_messages`, `chat/edit_message`†, `chat/regenerate`†, `chat/continue`, `chat/stop_stream`, `chat/resume_stream`, `chat/message_feedback`.
- **Sessions**: `chat_session/create`, `.../fetch_page`, `.../delete`, `.../delete_all`, `.../update_title`, `.../update_pinned`.
- **Files**: `file/upload_file`† (multipart), `file/fetch_files`, `file/fork_file_task`.
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
4. WASM blob `assets/sha3_wasm_bg.7b9ca65ddd.wasm` (SHA-3) run via **`wasmtime`**. Exports used: `memory`, `__wbindgen_add_to_stack_pointer`, `__wbindgen_export_0` (alloc), `wasm_solve`. Result region: `[status:i32 | pad | value:f64]`; `status==0` → no answer; else answer = `value.floor() as u64`.
5. `encode_solution()` (:194): JSON → base64 → header value.
6. Asset path resolution (:233): `CARGO_MANIFEST_DIR/assets/…` → CWD → exe-dir. Runtime cached in a `OnceLock`.

**Fragility**: no check that the challenge hasn't expired before submit; slow solves can send a stale challenge with no retry.

## SSE STREAMING (`deepseek.rs:841`+)

- `process_stream_line()` (:841): requires `data:` prefix; skips empty/`[DONE]`; unwraps up to 3 nested JSON layers; returns `(text_chunk?, parent_message_id?)`.
- `parse_sse_event()` (:887) → `ParsedSSEEvent` enum (`types.rs:260`): **named** events `ready|update_session|title|close`; **unnamed** events keyed by `o` (op: APPEND/SET/BATCH) + `p` (path) + `v` (value) → `FragmentAppend|ContentAppend|FieldSet|Batch|Response|TokenDelta|Unknown`.
- `extract_text_from_event()` (:656): tries `p` contains `/content`, then `v.response`, then ~9 OpenAI-style fallback paths (`choices[0].delta.content`, etc.).
- On `[DONE]`: emit final chunk with `finish_reason="stop"`, then `mark_session_after_success()` updates `parent_message_id` + `system_sent_for_session`.

## PROMPT ASSEMBLY (`build_prompt`, `deepseek.rs:460–531`)

DeepSeek web API takes a single `prompt` string, so history is flattened:
- First message of a session: `system prompt` + `### LOCAL MEMORY` + history, tool results as `### TOOL RESULT: {name}`, user as `### USER INPUT`.
- Subsequent: only the new turn (batched tool results, or user input).
- **`### LOCAL MEMORY` is just a history-section label — it does NOT load `.memories/` files.** (Verified: nothing in `src/` reads `.memories/`.)
- Code blocks > 300 chars are stripped to `[...]` via regex (:79) to save tokens.
- `model_type`: "expert" (thinking) if model name contains "reasoner"/"expert", else "default"/null.

## RESILIENCE

- **Rate limit** (`enforce_rate_limit`, `deepseek/http.rs`): two independent, composable gates — a min spacing between requests (`rate_limit_ms`) and a sliding 60s-window request cap (`rate_limit_per_minute`, tracked via a `VecDeque<Instant>` in `request_history`); both set via `/rate` and either can be 0 to disable.
- **Retry/backoff** (`send_json_request`, :225): exponential `min(30s, 1000ms·2^(n-1))` on 5xx/network errors; **no jitter, no `Retry-After` parsing, no total-time cap** → with `max_retries=-1` can hang forever.
- No request-level timeout set (relies on reqwest defaults). Token usage never tracked (estimated `len/4`).
