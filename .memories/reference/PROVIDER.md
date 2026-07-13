# REFERENCE: DeepSeek Provider
> The LLM backend. Reverse-engineered DeepSeek **web** API (chat.deepseek.com), not the public API key product.
> Source: `src/provider/`. Last updated: 2026-07-07 (corrected from code: PoW WASM is now **embedded** via `include_bytes!`, a file on disk only overrides it; `build_body` always sends the literal `model: "deepseek-chat"`; fixed the `### LOCAL MEMORY` vs newest-turn label description). Before: 2026-07-04 (constructor gained `rate_limit_per_minute`; trait gained `session_identity`/`session_is_alive`/`adopt_session` for cross-restart session resume — see `reference/CONFIG.md`'s "Remote session resume" section for the full flow)

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
| `session_is_alive` | `async (session_id) -> bool` | Best-effort existence check. Default `false`; DeepSeek delegates to `fetch_remote_history` (`chat/history`) — any error (deleted/expired/network) reads as not-alive. |
| `adopt_session` | `async (session_id, parent_message_id) -> AppResult<()>` | Resume a previously-known remote session instead of creating a new one. Default no-op; DeepSeek sets `SessionState{session_id, parent_message_id, system_sent_for_session: true}` directly, skipping `chat_session/create`. |

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
- **First send of a session** (`system_sent_for_session == false`): `system prompt` + `### LOCAL MEMORY` + flattened history *before the tail* + the tail sections. Inside `### LOCAL MEMORY`, each prior message is labelled by role via `format_history_message` → `[ASSISTANT]` / `[USER]` / `[SYSTEM]` / `[TOOL]`.
- **Subsequent sends**: only the tail is sent (DeepSeek retains the rest server-side).
- **Every non-empty send ends with `FORMAT_REMINDER`** — a one-line RU recency anchor restating the `<tool_use>` format (~60 tokens/turn; a weak model holds format by recency far better than by primacy).
- The tail design is the fix for the 2026-07-13 CRITICAL: a trailing system hint used to be sent as the sole `### USER INPUT`, silently dropping the user's message (see `BUGS.md` RESOLVED).
- `is_first_conversational_send` (role-aware: system notes don't count) feeds `should_reset` in `stream.rs` — a hint accompanying the first user message must not mask a fresh conversation.
- So `### USER INPUT` / `### TOOL RESULT` / `### NOTE` mark **only the newest tail**; the `[ROLE]` labels are what appear inside the `### LOCAL MEMORY` history block. Don't conflate the two.
- **`### LOCAL MEMORY` is just a history-section label — it does NOT load `.memories/` files.** (Verified: nothing in `src/` reads `.memories/`.)
- Code blocks > 300 chars are stripped to `[...]` via regex (`strip_long_code_blocks`) to save tokens.
- **Request body** (`build_body`, `deepseek/stream.rs`) always sends the literal `"model": "deepseek-chat"`; the TUI's model string only drives `model_type` via `resolve_model_type(model, parent_message_id)` (`prompt.rs`): name contains `reasoner`/`expert` → `"expert"` (+ `thinking_enabled: true`); a chat model on the first turn (`parent_message_id == None`) → `"default"`; a chat model on later turns → `null`.

## RESILIENCE

- **Rate limit** (`enforce_rate_limit`, `deepseek/http.rs`): two independent, composable gates — a min spacing between requests (`rate_limit_ms`) and a sliding 60s-window request cap (`rate_limit_per_minute`, tracked via a `VecDeque<Instant>` in `request_history`); both set via `/rate` and either can be 0 to disable.
- **Retry/backoff** (`send_json_request`, :225): exponential `min(30s, 1000ms·2^(n-1))` on 5xx/network errors; **no jitter, no `Retry-After` parsing, no total-time cap** → with `max_retries=-1` can hang forever.
- No request-level timeout set (relies on reqwest defaults). Token usage never tracked (estimated `len/4`).
