# Full-codebase Rust review — 2026-08-26

Scope: the whole crate, not a diff. 176 `.rs` files, 46 544 lines under `src/`.
Edition 2024, MSRV 1.91, single crate (no workspace), tokio 1 `features = ["full"]`,
manual multi-thread runtime (`main.rs:118`).

Written in English to match the rest of `.docs/`, `.memories/`, and `CLAUDE.md`.

## Method and its limits

- **No build was run.** The machine was under load from other work, so this pass is
  read-only: `grep`/`find`/`sed` sweeps plus full reads of every cited symbol.
- The previous session's `cargo clippy --all-targets --all-features -- -D warnings`
  died on the known pagefile flake documented in `CLAUDE.md`
  (`STATUS_STACK_BUFFER_OVERRUN` / os error 1455 while linking `ort`), **not** on
  code. Its output carries no `error[E…]` or `warning:` lines attributable to this
  crate. Nothing below duplicates a compiler or clippy diagnostic — but nothing below
  has been confirmed *by* one either. **Re-run clippy before acting on the Minor items.**
- Every Critical/High finding was verified by reading the full enclosing function,
  not a diff hunk. Findings are anchored to symbol names where practical; line numbers
  are as of this date and will drift.

## Findings

| # | Problem | Cause | Consequence | Options | Effort | Severity |
|---|---|---|---|---|---|---|
| 1 | `util::atomic_write` removes the target before `rename` (`util.rs:136-139`) | The comment claims "on Windows, `rename` fails if the target exists". False for Rust: `std::fs::rename` is `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING` and does replace | Opens a window where the file does not exist at all. A crash between `remove` and `rename` loses the data *entirely* — worse than the truncation the function exists to prevent. Sessions, config, whitelist and `mcp.json` all go through it | Delete the `#[cfg(windows)]` block; `rename` is already replacing on both platforms | S | **Critical** |
| 2 | `Config`'s `provider`/`ui`/`agent`/`mcp` lack `#[serde(default)]` (`config/mod.rs:7-10`); every `AgentConfig` field is required (`:273-283`) | Only 8 of 12 sections carry `default`; nested fields carry none | A config missing one section or one key fails to parse. The TUI silently falls back to `Config::default()` (the user's real settings are ignored for that run); the harness exits 3. Any new field breaks every existing config. This class already cost debugging time — see the CRLF note at `main.rs:93-98` | `#[serde(default)]` on the four sections and on all `AgentConfig`/`ProviderConfig` fields. Do **not** add `deny_unknown_fields` — it would break older keys | S | **Critical** |
| 3 | `max_context_messages` and `auto_compact` are never read (`config/mod.rs:275-276`) | Declared, defaulted (256 / true), serialized into the user's `config.toml` and into `sandbox/config.template.toml`, but no reader exists anywhere in `src/` | No automatic context trimming. For OpenAI-compatible providers `build_step_request` (`agent/runner.rs`) resends the whole message array every step → token cost grows quadratically within a turn and long turns eventually get rejected upstream. DeepSeek is partly shielded by server-side session threading (`system_sent_for_session`), which is why this went unnoticed. The user configures a limit that does nothing | Wire both knobs into `build_step_request`, **or** delete them and the template lines. Note finding 4: there is no reusable summarizer to wire them to | M | **Critical** |
| 4 | `/compact` is a stub presented as a feature (`commands/defs/compact.rs:26-42`) | "Summary" = every user message's full `content` joined with `"; "`; assistant and tool messages are dropped outright, then `messages.clear()` | The replacement text can be nearly as long as what it replaced while losing all assistant/tool context — the user loses the conversation and may not shrink the context. `.docs/TODO.md` lists "Context compaction (/compact command)" as complete | Either implement a real summarization pass (an LLM call, or structural truncation keeping the last N turns) or relabel the command and the TODO entry honestly | M | **High** |
| 5 | `Access-Control-Allow-Origin: *` on every response (`server/http.rs:210-219`) while `api_key` defaults to `None` (`config/mod.rs:100-101`) | The auth gate at `:255` only engages when a key is set; the CORS headers are unconditional. `OPTIONS` answers 204 with `allow-headers: content-type`, so the preflight passes | With `/serve` on, any page the user visits can issue a cross-origin `POST /v1/chat/completions` to `127.0.0.1:11111` **and read the response**. A third-party site drives the user's DeepSeek account and any paid keys in `[[providers]]` | Omit `ACAO: *` when `api_key.is_none()`; or echo an allow-listed `Origin`; or require a key for any host | S | **High** |
| 6 | The `MCPManager` lock is held across network I/O on admin paths — `keys/dispatch.rs:53`, `keys/mcp.rs:84`, `:109`, `:134`, `:236` | `mcp.lock().await.<op>().await`: the temporary guard lives to the end of the statement. The work is correctly off the event loop (invariant 1 holds), but invariant 2 does not | `settle_frame` polls the same mutex every frame in `View::Mcp` (`app/mod.rs:543-545`). While `reload_all` reconnects servers (seconds per server) the UI is frozen. This is the exact bug already fixed in `dispatch_generic_tool` (`agent/runner.rs:713` — short lock, cloned client) | Apply the same shape: take only the handle/list under the lock, call on the owned value. Or split into per-server locks | M | **High** |
| 7 | Background agents run with `auto_approve: true` (`app/multichat.rs:89`) | The flag bypasses both the approval modal and the persisted whitelist (`agent/runner.rs:646`) | `/btw "look at this repo"` can execute arbitrary `shell` / `powershell` with no prompt. The whitelist the user curated via `/whitelist` does not exist on this path | Consult the whitelist instead of an unconditional `true`; or allow only read-only tools for background turns; or queue background approvals for the user | M | **High** |
| 8 | Harness attributes `ToolError` to `tools.last_mut()` (`harness/driver.rs:502-507`) | No correlation: the `conversation` field is explicitly discarded (`conversation: _`), and `ToolInvocation` is pushed at *approval* time, not at result time | With more than one tool per step, or with concurrent sub-agents sharing the event channel, an error lands on the wrong invocation. The tool that measures agent behaviour silently reports wrong per-tool metrics | Key `ToolInvocation` by `(conversation, tool_id)` and look up by it — `ToolError` already carries `conversation` | M | **High** |
| 9 | `Config::data_dir()` is not redirected by `--config` (`config/mod.rs:619`) | `dirs::data_dir()` with no env override. The doc comment at `:638-640` promises a "throwaway token **and data set**"; `HARNESS.md:200` admits the opposite | A harness run on Windows writes sessions into the real `%APPDATA%/pooprusteek/sessions`, reads the real whitelist (`ApprovePolicy::Whitelist`), and shares the semantic index with the working TUI. `CLAUDE.md` tells contributors to "always use `--config`", implying isolation | Add `POOPRUSTEEK_DATA_DIR` (or `--data-dir`) and honour it in `data_dir()`; fix the misleading comment either way | S | **High** |
| 10 | Check-then-add on the shared output budget is not atomic (`tools/shell.rs:66-79`) | `budget.load(Relaxed)` → compute `take` → `fetch_add` as three steps, with two readers (stdout, stderr) sharing one budget | Both readers can observe the same `remaining` and each append that much — the 1 MiB cap overshoots by up to one chunk. Not a data race (the buffer has a mutex), but the cap is soft contrary to its documentation | `fetch_update`, or a `compare_exchange_weak` loop, or reserve via `fetch_add` and give back the excess | S | Medium |
| 11 | Bare `String::from_utf8_lossy` on child output (`harness/scenario.rs:529`, `:533`) | Violates invariant 4 in new code; `util::decode_process_output` exists for exactly this | stdout is our own JSON (ASCII, low risk), but stderr can carry localized OS text or UTF-16 → a failure report shows mojibake instead of the cause | Swap in `crate::util::decode_process_output` | S | Medium |
| 12 | `atomic_write` has no `fsync` and uses a fixed `.tmp` sibling name (`util.rs:130-141`) | No `File::sync_all()` on the temp file or its directory; `with_extension("…tmp")` is deterministic | After a crash a zero-length file is possible (metadata ordered before data). Two concurrent writers to one path share the temp name and clobber each other | `sync_all()` before `rename`; add pid + counter or a uuid to the temp name | S | Medium |
| 13 | Tests write into the real `Config::data_dir()` / `sessions_dir()` — `session.rs:275-280` and ~13 other files | The path is a global, and as its own comment admits, "non-injectable" | `cargo test` litters the user's data directory; a failing test leaves files behind; parallel tests share global state and go flaky | Inject the root (a `OnceLock<PathBuf>` with a setter, or the same env var as finding 9) and point tests at a `tempdir` | M | Medium |
| 14 | `AppError::Custom(e.to_string())` at 24 call sites (`error.rs:39`) | `Custom(String)` is a catch-all with no `#[source]`; `.to_string()` severs the chain | `io::ErrorKind` and the root cause are lost: "Custom(access denied)" with no file and no operation. Diagnosing harness and MCP failures is materially harder | Use `#[from]`/`#[source]` where the type is known; give the catch-all a `#[source] Box<dyn Error>` field | M | Medium |
| 15 | `Cargo.toml` has `[lints.rust]` but no `[lints.clippy]` | The only gate is `-D warnings` in CI over default lints | `redundant_clone`, `large_enum_variant`, `needless_collect` and the `perf` group are not enforced. 10 `#[allow(…)]` against 17 `#[expect(…)]`, though MSRV 1.91 makes `expect` available everywhere and it self-cleans | Add `[lints.clippy]` with the `perf` group and the lints above; convert the remaining `allow` to `expect` | S | Medium |
| 16 | `sandbox/config.template.toml:20-24` sets `show_status_bar`, `show_line_numbers`, `max_message_length` | `UiConfig` (`config/mod.rs:248-256`) now holds only `theme` and `custom_themes`; serde ignores unknown keys silently | The template advertises settings that no longer exist, and would break if `deny_unknown_fields` is ever added | Drop the three keys from the template | S | Low |
| 17 | `std::env::set_current_dir` in the harness driver (`harness/driver.rs:232`) | cwd is process-global and is changed while the runtime already has live tasks (semantic init, MCP connects) | A logical race: a background task can resolve a relative path against a different directory. Harmless today (one turn per process) but blocks any future in-process `exec` | Pass the workspace explicitly — `Command::current_dir` and a tool parameter — rather than through a global | M | Low |
| 18 | `copy_tree` (blocking fs) inside an `async fn` (`harness/scenario.rs:411`) | Scratch directories are prepared in the setup loop before `tokio::spawn` | Copying a fixture blocks a runtime worker. Invisible on small fixtures, stalls the whole suite on a large template | Wrap in `spawn_blocking` | S | Low |
| 19 | UTF-16 detection samples only the first 512 bytes and treats any byte < 0x20 as implausible (`util.rs:52-84`) | ESC (0x1B) in ANSI sequences matches `implausible_in_text`; mixed streams (UTF-8 then UTF-16) are classified from their opening bytes | The thresholds (`dominant*5 > pairs*2` and `other*4 < dominant`) hold up in practice, but dense-ANSI PTY output or a mid-stream encoding change can decode wrong | Exclude 0x1B from the implausible set; with no BOM, decide per block rather than from the head | M | Informational |
| 20 | Bearer comparison uses `==` (`server/http.rs:261`) | Not constant-time | A timing channel on the key. Barely exploitable over loopback — but the key exists precisely for the non-loopback case | `subtle::ConstantTimeEq`, or a manual XOR compare (a new dependency is your call) | S | Informational |

## Patterns worth preserving

- `agent/runner.rs:706-716` — the reference for MCP lock discipline:
  `let client = { mcp.lock().await.client_for(name) };`, then the call on the owned
  handle. The comment names the bug that was fixed. Finding 6 is asking for exactly
  this shape on the admin paths.
- `app/system_prompt.rs:34-38` — snapshot under the lock, all text assembly outside it.
- `main.rs:106-121` — the manual runtime for `shutdown_timeout`, with the reasoning
  for why `#[tokio::main]` does not fit and why abandoning blocking threads is safe here.
- `tools/background/types.rs:170-265` (`win_job`) — minimal `unsafe`, a `// SAFETY:`
  on every block, every Win32 return checked, `CloseHandle` on all paths. No objections.
- `util.rs:1-9` — `floor_char_boundary` instead of hand-rolled arithmetic; MSRV 1.91
  was chosen for it deliberately.

## Suggested order

1, 2, 3 first — all three are cases where a comment or a config key asserts something
the code does not do, which is the failure mode most likely to mislead the next reader.
Then 5 (the only finding with an external attack surface), then 6-9.

Findings 10-20 are worth a clippy run first: 15 in particular may surface more once
`[lints.clippy]` is in place.
