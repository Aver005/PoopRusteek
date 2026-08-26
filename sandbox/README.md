# Sandbox — autonomous behaviour testing

Runs real agent turns in a throwaway container and measures them, so agent
*behaviour* can be tested and regressed the way code already is.

## Why this exists

`cargo test` covers code. It cannot tell you that the model emits a malformed
`<tool_use>` block once in twenty turns, that a RAG hint points at the wrong
skill, or that a turn stops one step before answering. Those only show up in
live runs — and before this, the only way to produce a live run was for a
human to sit at the TUI and read the screen.

Two things were missing, and neither was isolation:

1. **A headless entry point that runs the agent loop.** `--acp` is a bare
   prompt relay with no tools; the `--proxy` API server is a provider gateway
   that explicitly ignores tool calls. Neither runs a turn. `pooprusteek exec`
   does (see `src/harness/`).
2. **A machine-readable trace.** The agent loop was already well instrumented
   through `debug_log`; it just wrote human lines. It now also writes JSONL,
   and that stream is the trace.

The container is what makes it *safe* to run those turns unattended: a
harness answers tool approvals from a policy, and the tool set includes a
shell.

## What it needs

- Docker Desktop running, with **20+ GB free on the drive holding its data
  disk** (usually `C:`). The `rust:1.91-trixie` builder alone is 2.3 GB, and
  the cargo registry plus target caches add several more. Running low is not a
  graceful failure: the daemon dies mid-build, and in the worst case the VM's
  filesystem goes read-only and containerd loses snapshots — recoverable only
  by resetting Docker's data.
- **Do not run a host `cargo build` at the same time as the image build.** They
  compete for the same Windows pagefile; the host build dies with `os error
  1455` and takes the daemon with it.
- **Check `~/.docker/daemon.json` before your first build.** Docker Desktop
  ships `builder.gc.defaultKeepStorage`, and on this machine it was `20GB` —
  which is a licence for the build cache to consume an entire drive, and it
  did, twice, taking the daemon's filesystem read-only with it each time. Every
  code change here means a full image rebuild, so the cache refills fast. Set
  it to something the drive can actually afford (`4GB` is plenty for this
  image) and the problem stops recurring:

  ```json
  { "builder": { "gc": { "enabled": true, "defaultKeepStorage": "4GB" } } }
  ```

- `docker builder prune -f` is always safe to reclaim space (build cache is
  regenerable by definition). Note that Docker's data disk does not shrink on
  its own even after pruning, so a disk that has already ballooned has to be
  reset, not pruned.

## Quick start

```powershell
cp sandbox/.env.example sandbox/.env    # then paste a throwaway token
cd sandbox
./sandbox.ps1 build                     # or: ./sandbox.ps1 build -BuildProfile dev
./sandbox.ps1 doctor
./sandbox.ps1 exec "List the files here and name the package version"
./sandbox.ps1 suite live -Repeat 5
./sandbox.ps1 mine -Sessions
```

`sandbox.sh` is the same CLI for WSL, Linux, macOS and CI.

Everything lands in `sandbox/out/` (bind-mounted, git-ignored): one JSONL
trace per run plus a `report.json` per scenario.

## Commands

| Command | What it does |
|---|---|
| `build [-BuildProfile dev\|release]` | Build the image. `dev` rebuilds far faster; `release` gives realistic timings. |
| `doctor` | Check engine, image, token, scenario count. Run this first when something is off. |
| `exec "<prompt>" [args…]` | One turn, one trace. Extra args pass through to `pooprusteek exec`. |
| `scenario <name> [-Repeat N]` | One scenario file, repeated, with expectations checked. |
| `suite [live\|mock\|dev\|all] [-Repeat N]` | Every scenario in that directory. Exit code 4 means expectations failed. |

`dev` is the set that matters most: real developer tasks (build a static
page, fix a failing test, follow explicit constraints, answer without
editing), each in a fresh scratch workspace, each judged on the files that
came out rather than on how confident the answer sounded.
| `mine [-Sessions]` | Rank failure patterns across every trace, and optionally the saved sessions. |
| `mock <script>` | Start the scripted provider with `mock-scripts/<script>.toml`. |
| `stop` / `report` / `reset` | Tear down / list recent verdicts / wipe volumes and out. |

## How a scenario works

A scenario is a prompt plus the conditions to run it under plus what the
result must look like. It runs **N times** — one sample of a nondeterministic
model is not evidence — and passes if the pass rate clears `min_pass_rate`.

```toml
name = "shell-reads-workspace"
prompt = "…tell me the version of the package declared in Cargo.toml."
workspace = "../../fixtures/tiny-repo"   # relative to this file
approve = "all"                          # all | none | whitelist | except:a,b
semantic = "off"                         # off | background | ready[:seconds]
timeout = 240

[expect]
status = "completed"
max_steps = 6
final_matches = ['0\.4\.2']              # literal strings: "…" eats backslashes
no_malformed = true
min_pass_rate = 0.8
```

`deny_unknown_fields` is on for both tables: a mistyped expectation is an
error, not a silent pass. And a run that produced nothing to judge — the child
never started, no turn ran, or no step ran — **fails**, whatever `[expect]`
says. Expecting a failure is still possible; it has to be declared
(`status = "failed" | "timed_out" | "setup_failed"`).

Two optional tables do the rest of the work:

```toml
[context]                    # compaction settings, forwarded to the child as flags
window = 12000               # without a window the ladder has nothing to measure
reserved_tokens = 1000       # against and stays off — so rungs 1-3 were untestable
preserve_recent_tokens = 500
tool_output_limit = 20000
auto_compact = true

[[expect.trace]]             # assertions over the trace, i.e. over what happened
action = "context.prune"     # *inside* the loop, not just over how the run ended
min_count = 1                # (default 1: a typo'd action fails, never passes)
field = "cleared"            # numeric field totalled across matching records
min_total = 1                # `max_count = 0` asserts an action never happened
```

Each repeat runs as its own child process. That gives one trace file per
repeat, no state bleeding between them, and a hung turn that can be killed
without taking the runner down.

### Live vs mock

`scenarios/live/` runs against the real DeepSeek web API — that is where
"живые ответы" and the pattern corpus come from, and it is inherently noisy.

`scenarios/mock/` runs against `mock-provider`, a scripted
OpenAI-compatible endpoint. Same client, same streaming path, same tool
parser — fixed replies. That is what makes a *regression gate* possible, and
it is the only way to reliably reach failure paths the live model produces
only by accident. `mock-scripts/malformed-then-recovers.toml` drives the
malformed-tool-call retry path on demand, and
`mock-scripts/rung-one-clears-tool-output.toml` drives three large reads that
push the window past rung 1's trigger — the mock is where the compaction
ladder can be exercised at all, because DeepSeek keeps its history server-side
and skips rung 1 outright:

```powershell
./sandbox.ps1 mock rung-one-clears-tool-output
./sandbox.ps1 scenario rung-one-clears-tool-output -Repeat 2
```

## Reading a trace

One JSON object per line, `{seq, ts, action, message|data}`. The interesting
actions:

| Action | Carries |
|---|---|
| `harness.run.started` / `.finished` | Run configuration and verdict |
| `system_prompt.assembled` | Prompt size breakdown (base/tools/mcp/skills) |
| `agent.step.start` | Step number of max |
| `agent.step.parsed.payload` | Raw model output, visible text, parsed tool calls |
| `agent.step.malformed_tool_use` | The parse errors fed back to the model |
| `agent.tool.call.payload` / `.result.payload` | Tool name, arguments, result, `is_error` |
| `agent.semantic_hint` | What retrieval injected |
| `agent.turn.done` / `.error` | How the turn ended |
| `pow.solver.solve`, `completion.stream.*` | Proof-of-work and SSE detail |

```bash
jq -r 'select(.action|startswith("agent.")) | "\(.action) \(.message // "")"' out/exec-*.jsonl
```

## Isolation, honestly stated

What the container gives you: a throwaway filesystem, `cap_drop: ALL`,
`no-new-privileges`, a 512-pid cap and a 4 GB memory cap, a non-root user,
and no bind mount of the real source tree — only read-only fixtures.

What it does **not** give you: network isolation. Live scenarios need to
reach the provider, so the container has outbound network. A tool call can
therefore still make outbound requests. For a genuinely network-isolated run,
use the mock suite and add `--network none` to the compose service.

The token is passed as an environment variable at run time and written only
inside the container's own filesystem, so it never enters an image layer and
`docker history` never shows it. `sandbox/.env` is git-ignored.

## Running the harness without Docker

Everything works on the host too, which is how you debug the harness itself:

```powershell
cargo build --bin pooprusteek
./target/debug/pooprusteek --config path\to\config.toml exec "prompt" --trace .dev\t.jsonl
```

`--config` exists for exactly this: it keeps a harness run off your real
config and token. Note that the *data* dir (sessions, semantic index) is
still the real one on Windows — `dirs::data_dir()` has no env override there.
Inside the container `XDG_DATA_HOME` handles it.
