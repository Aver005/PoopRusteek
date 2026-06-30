# Robustness diff — where each is more (or less) correct

This is the more interesting half of the comparison: same product, but each sibling got a
different set of edge cases right. Each project's bug is a **checklist item for the other.**

Confidence is marked **[verified]** (confirmed in code) or **[plausible]** (mechanism
confirmed, exact manifestation not fully traced).

---

## 1. DeepSeek session threading (`parent_message_id`) — split decision

**Background.** DeepSeek stores a chat as a **tree**; every completion sends
`{chat_session_id, parent_message_id}` and the server attaches the new turn as a *child*
of that id. The web UI shows one active branch. If the client's stored `parent_message_id`
is stale, the next message is accepted (HTTP 200, shows locally) but lands on an
off-branch node — **invisible in the web view.** Symptom: "my message vanished."

### poopseek — CORRECT ✅ [verified]
`src/providers/deepseek-web.ts:163-183` persists the id **two ways**:
1. **Incrementally**, inside the stream loop, the moment an id event arrives (`:175`).
2. In a **`finally`** block (`:180-183`) so abort/error/throw still flushes the last id.

So an interrupted turn keeps the thread; the next message stays on the live branch.

### pooprusteek — WAS BROKEN, now FIXED ✅ [verified]
The Rust port had regressed this: `mark_session_after_success` ran **only on clean
completion** (`[DONE]` or end-of-loop). On a stream error `let chunk = chunk?` returned
early; on `Esc` the task was `abort()`ed — both skipped the mark → stale `parent_message_id`
→ **the conversation forked onto an invisible branch.** This is exactly the reported
"agent ended abnormally, then my messages evaporate (not in web)."

Fix applied (`src/provider/deepseek.rs`, `complete` + `complete_stream`):
- persist `parent_message_id` **immediately** when seen in the stream;
- on a chunk error, **flush before returning** the error.
This restores poopseek's behavior. Tests: `provider::deepseek::tests` (persist-survives-empty-mark, ignore-stale-session).

**Verdict:** poopseek was the reference; pooprusteek regressed during the port and is now
back to parity. Residual edge (both, narrow): if the stream dies *before the first id
event*, neither client learns the new server head — a full cure is a "fetch session head"
resync. Not yet implemented in either.

---

## 2. Cancellation of a waiting user-prompt — split the other way

### poopseek — has a bug ◑ [verified mechanism / plausible manifestation]
`src/cli/input-queue.ts:27-30`: `waitForNext()` stores a `pendingWaiter` resolver with
**no cancel path**. `src/cli/ask-user.ts:32,53` awaits it **without passing any abort
signal.** `resolveWaiter` is only called from `interrupts.ts` on `/home` (`onGoHome`) and
role-creation cancel — **not** on a normal Ctrl+C/Esc turn abort.

So: model calls a text `user.ask` → user aborts the turn → the waiter is never settled →
the tool `await` hangs → `runTurn` never returns → its `finally` (which resets
`isMainTurnActiveRef`) never runs → subsequent input is swallowed/mis-routed. Same
symptom *class* as the pooprusteek session bug ("input disappears"), different cause.

Fix shape: thread the turn's `AbortSignal` into `waitForNext()` and reject/clear the
waiter on abort (or give the queue a `cancel()` called on every abort path, not just
`/home`).

### pooprusteek — structurally avoids it ✅ [verified]
The approval/question wait (`ToolApprovalRequest::wait` / `QuestionRequest::wait`) lives
**inside** the spawned agent task. `Esc` calls `handle.abort()`, which **drops that future**
— the waiter goes away with the task, no leak. Different concurrency model (wait-in-task
vs wait-in-shared-queue) makes pooprusteek immune to this specific class.

**Verdict:** pooprusteek's task-abort model is the donor here; poopseek's shared input
queue needs signal-aware cancellation.

---

## 3. GOAL-mode pipeline robustness — pooprusteek-only (no analog in poopseek)

poopseek has **no goal/iterate mode**, so this is unique to pooprusteek. Worth recording
because pooprusteek's GOAL pipeline was itself hardened recently against:
- **abort/error mid-cycle** leaving the stage stuck "running" (wedge) → now
  `cancel_goal_cycle` on Esc/Ctrl+C/`AgentError` when `is_running()`;
- **no-provider** at the worker or evaluator step → cancel instead of wedge;
- **infinite retries** → `MAX_GOAL_ITERATIONS = 10` with a `GaveUp` outcome;
- **empty prompt/goal** → user nudge instead of silent no-op.
Tests in `app::goal::tests`. (See the project's own commit history / `.docs`.)

If GOAL mode is ported to poopseek, **port these guards with it** — the naive version
wedges easily.

---

## 4. Turn lifecycle "stuck busy" flag

| | poopseek | pooprusteek |
|---|---|---|
| Busy flag | `isMainTurnActiveRef`, reset in `finally` of turn-runner ✅ [verified] | `generation.active`, reset in `AgentDone`/`AgentError` + abort handlers ✅ [verified] |
| Stuck risk | only **indirectly**, via the input-queue waiter hang (§2) | low; abort path explicitly sets `active=false` |

Both reset their busy flag on the normal error path. poopseek's only realistic "stuck"
route is the §2 waiter hang (which prevents the `finally` from running at all).

---

## 5. Sub-task isolation (poopseek) — correct ✅ [verified]

poopseek's sub-agents, sidechat, refactor, review all run on **cloned providers** with
fresh sessions (`getProvider().clone()`), so an abort/error in a side-task can't corrupt
the main conversation's `parent_message_id`. This is a good pattern pooprusteek would need
if/when it gains sub-agents.

---

## 6. Minor / lower-severity

| Item | Where | Severity | Note |
|---|---|---|---|
| Session saved only on success path, not `finally` | poopseek `turn-runner.ts:217-226` | LOW [plausible] | On error, state persists only after the *next* successful turn — misleading UX, not data loss |
| Naive token estimate (`chars/4`) | both | LOW | neither does real tokenization |
| MCP HTTP/SSE transport stubbed | pooprusteek `mcp/transport.rs` | LOW | stdio only; poopseek has real HTTP transport |
| Pre-existing dead code (URL consts, unused fields) | pooprusteek `deepseek.rs` etc. | trivial | warning noise, not bugs |

---

## Scoreboard

| Bug class | Winner | Loser action |
|---|---|---|
| Session threading on abnormal stream | poopseek (pooprusteek now matches) | — (fixed) |
| Cancel a waiting user-prompt | **pooprusteek** | poopseek: signal-aware `waitForNext` |
| GOAL-loop wedge/cap/empty | **pooprusteek** (only impl) | poopseek: adopt guards if porting GOAL |
| Sub-task session isolation | **poopseek** (only impl) | pooprusteek: clone-provider pattern when adding sub-agents |
| Busy-flag reset | tie | — |
