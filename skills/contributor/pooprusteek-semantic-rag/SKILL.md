---
name: PoopRusteek Semantic / RAG Layer
description: How to safely work in PoopRusteek's local RAG layer (src/semantic/) — the embedder + three corpora, hybrid RRF ranking, background init, deferred MCP schemas, the degrade-never-block invariant, and the MRR eval gate you must run and journal. Use before touching semantic ranking or corpus text.
---

# PoopRusteek Semantic / RAG Layer

Sources: `src/semantic/`, the `src/semantic/` section of `CLAUDE.md`. Fully local, offline after
one model download — a free RAG layer, not a hosted API.

## Shape

`SemanticService` (`semantic/mod.rs`) is a cheap shared handle over **one embedder + three corpora**:

- **Embedder** (`embedder.rs`): fastembed wrapper pinned to **multilingual-e5-small** (ONNX),
  cached at `Config::data_dir()/models`. Honors the e5 `query:` / `passage:` prefix contract and
  L2-normalizes outputs. Batch cap is the ONNX memory guard (`/rag-limit`, `semantic.rag_limit`).
- **Three corpora**: skills (`SkillCorpus`), MCP tools (`McpCorpus`, carries `input_schema`), and
  persistent message history (`HistoryStore`, `history.rs`).
- **Ranking** (`index.rs` `HybridIndex`): dense embeddings **+ stemmed TF-IDF** sparse vectors
  (`sparse.rs`, ru/en Snowball), fused with **RRF** plus a dense-floor / lexical-overlap gate.

Each turn gets an ephemeral hint (`match_prompt` in `run_agent_loop`) suggesting skills and MCP
tools; the `tool_search` / `history_search` builtins expose the same index to the model on demand.
The history index (`data_dir/semantic/history.json`) is a **rebuildable cache** over session files
(per-session watermarks, model-stamp wipe) — session files are the source of truth.

## Threading rules (invariant #9 + #11)

- **Init is background**: `spawn_init` runs on `spawn_blocking`; first run downloads the model, then
  backfills the history index from saved session files. The service answers instantly (with nothing)
  until init lands. Readiness is implicit — `inner.embedder.is_some()`.
- **All embedding/inference runs on `spawn_blocking` only** — never the event loop, never bare on an
  async worker.
- **The inner `Mutex` is held across synchronous work ONLY, never across an `.await`.** Per-turn
  hints wait at most `HINT_LOCK_BUDGET` (150 ms) for the lock, then proceed without one.
- **Degrade, never block** (invariant #11): every entry point returns empty / falls back to lexical
  when the embedder is disabled, still initializing, or failed. No semantic path may gate a feature.

## Deferred MCP schemas

Above 12 tools (`[semantic] mcp_schemas = auto`), the system prompt carries only a server-level
summary (`<server> (N tools)`) — individual tools are not enumerated. Full definitions come from the
per-turn hint or from the `tool_search` builtin (its lexical fallback is what makes this safe
pre-init). Deferral is only allowed when semantic matching is enabled —
`App::effective_mcp_schema_mode` forces `Full` otherwise.

## Eval gate — run and journal when you touch ranking

Retrieval changes (thresholds, RRF, corpus text composition — anything in `src/semantic/` ranking)
have their own quality gate: an `#[ignore]`d MRR harness (`semantic/eval.rs`) that needs the ~120 MB
model on disk (downloaded once, shared with the app).

```
cargo test --bin pooprusteek semantic::eval -- --ignored --nocapture
```

Run it whenever you touch ranking and **record the numbers in the journal** (`.memories/JOURNAL/`).
Current baselines: **skills MRR 0.927, MCP tools MRR 0.836** — don't regress them silently.

## Control surfaces

`/rag [on|off|reload]` (status / persist `semantic.enabled` / re-verify model + re-embed corpora),
`/rag-limit [<N>|auto|off]` (embedder batch cap), `/search` (`View::Search` over history),
`history_search` + `tool_search` builtins (agent-facing).
