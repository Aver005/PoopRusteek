# HARNESS — headless behaviour testing

> Deep reference for `src/harness/` + `sandbox/`. Added 2026-08-25.
> Last updated: 2026-08-25

## 1. WHY IT EXISTS

`cargo test` covers code. It cannot catch the failures that actually matter in
a coding agent, because those are *behavioural*: a malformed `<tool_use>` the
model can't recover from, a RAG hint pointing at the wrong skill, a turn that
stops one step before answering, a tool result the model can't read.

Before this, producing such a run required a human at the TUI. The two
existing headless modes do **not** run the agent loop:

| Mode | Reality | `→` |
|------|---------|-----|
| `--acp` | 190-line prompt relay. No tools, hardcoded system prompt. | `src/acp/server.rs` |
| `--proxy` / `--serve --uiless` | Provider gateway. Stateless per request; inbound `tools` explicitly ignored with a `tracing::warn`. | `src/server/openai.rs::chat_completions` |

So the harness adds a third: **`pooprusteek exec`** — one real turn, no
terminal, one machine-readable trace.

## 2. TRACE = THE EXISTING DEBUG LOG, IN JSONL

The single most important design decision: **no new instrumentation was added
to the agent loop.** `agent/runner.rs` was already densely instrumented
through `debug_log` (`agent.step.parsed.payload` carries the raw model
output, visible text, parsed calls and parse errors). Adding a parallel
telemetry stream would have guaranteed drift between the two.

Instead `debug_log` gained a second line format:

- `Format::Human` — the old `[ts] [action] message`, pretty-printed JSON.
- `Format::Jsonl` — one object per line: `{seq, ts, action, message|data}`.

`debug_log::configure(path, format)` points the sink at a per-run file before
first use. A monotonic `SEQ` gives a total order that millisecond timestamps
cannot (`→ src/debug_log.rs`).

Consequence: a trace mixes `agent.*` / `pow.*` / `completion.*` /
`system_prompt.*` records emitted by the app itself with `harness.*` records
the driver adds for what only it knows (policy decisions, verdict). One
stream, one envelope.

**Trace envelope is deliberately loose** (`action` a dotted string, `data`
untyped) because the producers are scattered `debug_log` call sites. Analysis
pulls typed views out of it; an unknown action never breaks a consumer, so
adding instrumentation upstream is always safe. `→ src/harness/trace.rs`

## 3. FIDELITY: WHY `auto_approve` STAYS FALSE

`run_agent_loop` refuses sub-agents when `auto_approve` is on — depth limit 1,
`"Nested sub-agents are not allowed."` (`→ src/agent/runner.rs`, the
`TASK_TOOL_NAME` branch). A harness that took the easy path and set
`auto_approve: true` would therefore be unable to test sub-agents at all, and
would skip the approval machinery entirely.

So the driver runs with `auto_approve: false` and answers
`AppEvent::RequestToolApproval` itself from an [`ApprovePolicy`]
(`all` / `none` / `whitelist` / `except:a,b`). Three consequences worth
keeping: the real approval path is exercised, denial becomes testable, and
`task` still works. `scenarios/live/spawns-sub-agent.toml` guards this — if
the harness ever starts bypassing approval, that scenario stops passing.

`RequestQuestion` is answered the same way (canned `--answer`, else the first
offered option). Both resolve through an async `Notify`, so resolution is
spawned rather than awaited inline — the driver loop must not block on the
task it is unblocking.

`SpawnSubAgent` events are honoured for real: a new `ConversationId`, a forked
provider, `max_steps` clamped to 8, `auto_approve: true` — the same recipe as
`App::spawn_background_agent`. The run completes when every conversation in
the tree is done, so sub-agent work is inside the measured window.

## 4. LAYOUT

| File | Purpose |
|------|---------|
| `harness/mod.rs` | clap subcommands (`exec`, `scenario`, `suite`, `mine`, `mock-provider`) + dispatch. Returns a process exit code. |
| `harness/driver.rs` | One turn: assembles the same deps as `App::new`, spawns a `TurnSpec`, services events under a wall-clock deadline. |
| `harness/trace.rs` | `TraceRecord` / `Trace` — the reading side of the JSONL. |
| `harness/metrics.rs` | `RunMetrics::from_trace` — steps, tool calls/errors, malformed count, stream timeouts, `TurnEnd`. |
| `harness/scenario.rs` | Scenario + `Expect` TOML model, expectation checking, child-process orchestration. |
| `harness/report.rs` | `RunReport` / `ScenarioReport` / `SuiteReport`, `Stat` (min/mean/median/max), renderers. |
| `harness/mine.rs` | Pattern mining: normalise → bucket → rank, over traces and saved sessions. |
| `harness/mock.rs` | Scripted OpenAI-compatible endpoint. |

Exit codes: `0` completed, `1` agent error, `2` timeout, `3` setup failure,
`4` expectations not met (`scenario`/`suite`).

## 5. SCENARIOS ARE REPEATED, AND RUN AS CHILDREN

A single sample of a nondeterministic model is not evidence, so a scenario
declares `min_pass_rate` and the runner reports a *rate* plus min/median/max
of steps, tool calls and duration.

Each repeat is a **child process** (`pooprusteek exec --json`), not an
in-process loop. Three reasons: the debug-log sink is process-global (one
trace file per process), no state bleeds between repeats, and a wedged turn
can be killed (`kill_on_drop` + a `timeout + 60s` backstop) without taking the
runner down. `--config` is forwarded to children — without it they would load
the user's real config instead of the one the run was pointed at.

`deny_unknown_fields` on both `Scenario` and `Expect`: a mistyped expectation
that silently passes is the worst failure mode a harness can have.

**Gotcha:** regex expectations must use TOML *literal* strings
(`final_matches = ['0\.4\.2']`). A basic `"…"` string consumes the
backslashes and the file fails to parse.

## 6. JUDGING BY THE FILESYSTEM, NOT THE ANSWER

For a development task the answer text is the least reliable evidence
available: a model will describe three files it never wrote, confidently and in
detail. Two fields exist for this.

`workspace_template` is copied into a **fresh scratch directory per repeat**,
which is where the turn then runs. So a writing task starts from a known state
every time, repeats cannot see each other's output, and the copies are kept
under the report directory so what the agent produced can be read afterwards.
An empty template is a greenfield task. It is mutually exclusive with
`workspace`, which stays shared and is treated as read-only.

The expectations that read that directory:

```toml
[expect]
files_exist   = ["index.html", "style.css", "script.js"]
files_absent  = ["README.md", "__pycache__"]   # catches "helpful" extras

[[expect.file_matches]]
path    = "index.html"
pattern = 'style\.css'                          # it linked what it wrote

[[expect.file_matches]]
path    = "test_calculator.py"
pattern = 'average\(\[\]\), 0'                  # the test was not edited away
absent  = false
```

TOML ordering matters and is easy to get wrong: every `[expect]` scalar must
appear **before** the `[[expect.file_matches]]` arrays, or it lands in the
wrong table.

`files_absent` is worth as much as `files_exist`. It is how an agent that
scatters scratch files, or writes to the path it was told to leave alone, gets
caught — and how "create exactly two files and nothing else" becomes testable
at all.

The `dev` scenario set (`./sandbox.sh suite dev`) is built entirely on this:
build a static page, fix a failing test, respect explicit constraints, explain
without editing. `python3` is in the image so the red-to-green loop is real
work rather than a simulation of it.

## 7. LIVE VS MOCK

`scenarios/live/` hits the real DeepSeek web API — the source of live answers
and of the mining corpus, and inherently noisy.

`scenarios/mock/` hits `mock-provider`: the same `openai_compat` client, same
streaming path, same tool parser, fixed replies. That is what makes a
regression *gate* possible, and it is the only reliable way to reach failure
paths the live model produces by accident. `mock-scripts/` scripts support
`when` (substring match on the last user message), positional order with the
last reply repeating, `delay_ms` for timeout tests and `status` for error
paths (429/500).

The mock's JSON is hand-rolled rather than reusing `openai_compat`'s
serializers on purpose: a double that shares wire code with the thing under
test cannot catch a wire-format bug.

## 8. MINING

`harness mine` ranks failure shapes across every trace (and, with
`--sessions`, the saved session corpus that predates the harness).

The whole trick is **normalisation** — raw messages are near-unique, so they
are reduced to a shape before counting. Three passes, in this order:

1. quoted spans → `<str>` (they may contain both spaces and separators);
2. whole path-like tokens → `<path>`;
3. digit runs → `<n>`.

Order matters: collapsing paths first shreds a quoted path into fragments and
re-splits the bucket. Buckets: `malformed-tool-calls`, `tool-errors`,
`stream-problems`, `hints-without-tool-use`, `repeated-answers` (loops; short
shapes filtered out — a repeated ```` ```xml ```` is punctuation, not a loop).

## 9. SANDBOX CONTAINER

`sandbox/` — Dockerfile (two stages, pinned `rust:1.91-trixie`), compose
(`sandbox` one-shot + `mock` long-running), `sandbox.ps1` / `sandbox.sh`
control CLIs, fixtures, scenarios, mock scripts. Full usage:
`sandbox/README.md`.

Points that cost time to discover:

- **The base image must be Debian trixie, not bookworm.** The prebuilt
  static ONNX Runtime `ort` downloads is linked against glibc >= 2.38, which
  redirects `strtoll`/`strtoull` to the C23 symbols `__isoc23_strtoll` /
  `__isoc23_strtoull`. Bookworm ships glibc 2.36 and has neither, so the link
  dies with a wall of `undefined symbol` errors out of `libort_sys`. Both
  stages must stay on the same or newer glibc — this is the constraint on
  which Linux bases can build this crate at all.
- **Never run a host `cargo build` and the image build at the same time.**
  They compete for the same Windows pagefile: the host build dies with
  `os error 1455` ("paging file too small", the flake `CLAUDE.md` already
  documents) and the Docker daemon dies with it. Cost two failed builds to
  learn.
- **Docker needs real headroom on C:.** With ~4 GB free the daemon died
  mid-build twice, then the VM's filesystem went read-only and containerd
  lost snapshots — a state only a Docker Desktop restart clears. The
  `rust:1.91-trixie` image alone is 2.3 GB, plus the cargo registry and
  target caches. `docker builder prune -f` reclaims the failed builds' cache
  (10 GB, in one case) and is always safe: build cache is regenerable by
  definition.
- **Build parallelism is capped** (`CARGO_BUILD_JOBS=4`). Unbounded rustc plus
  the ort/wasmtime link step exhausts the WSL2 VM and takes the Docker daemon
  with it — two builds died that way (`rpc error: Unavailable … EOF`, then
  `_ping` 500). Same failure class as the host-build flake in `CLAUDE.md`.
- **`BUILD_PROFILE=dev`** exists because release is `lto = "fat"` +
  `codegen-units = 1`; validating a Dockerfile change does not need that.
- **XDG paths are set explicitly** so `dirs::config_dir()` / `data_dir()` are
  predictable; the data dir is a named volume so the ~120 MB embedding model
  downloads once.
- **The token never enters an image layer** — env var at run time, written by
  the entrypoint into the container's own filesystem. `sandbox/.env` is
  git-ignored via `*.env`.
- **Network is NOT isolated.** Live scenarios must reach the provider, so the
  container has outbound network and a tool call could too. `--network none`
  plus the mock suite is the isolated configuration.

## 10. RUNNING WITHOUT DOCKER

The harness works on the host, which is how you debug the harness itself:

```
cargo build --bin pooprusteek
./target/debug/pooprusteek --config .dev/harness-config.toml exec "prompt" --trace .dev/t.jsonl
```

`--config` (new, global) keeps a run off the real config and token. Note the
*data* dir is still the real one on Windows — `dirs::data_dir()` has no env
override there, unlike XDG on Linux.

## 11. WHAT THE HARNESS FOUND ON ITS FIRST RUNS

- Its own trace path was resolved *after* the `chdir` into the workspace, so
  relative paths landed inside the directory under test — the measuring tool
  contaminated the measured environment (the agent listed the harness's own
  `.dev/` folder). Fixed: absolutise before `chdir`.
- `bash` on Windows resolves to WSL, and `wsl.exe` writes its own errors in
  **UTF-16LE** while `shell.rs` decodes with `from_utf8_lossy` — the model
  receives mojibake instead of a readable error. See `BUGS.md`. The existing
  `POWERSHELL_UTF8_PREFIX` fixes the sibling OEM-codepage problem but not
  this one.
- `message_field` (harness-side) cut values at the first space, reducing
  `errors=tool \`bash\`: … not valid JSON` to the single word `tool`. Values
  now run to the next ` <ident>=` token.
