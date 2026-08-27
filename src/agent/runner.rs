use crate::agent::retry::{MAX_EMPTY_RESPONSE_RETRIES, MAX_MALFORMED_TOOL_RETRIES, RetryBudget};
use crate::agent::stream::{StreamEnd, StreamOutcome, StreamVerdict, collect_stream};
use crate::agent::tool_parser::{
    StreamTextTracker, parse_tool_calls_with_errors, strip_tool_calls,
};
use crate::agent::tools_step::{ToolExecContext, run_tool_calls};
use crate::agent::trace::{self, StepTrace};
use crate::app::conversation::ConversationId;
use crate::app::events::{AgentEvent, AgentResult, AppEvent};
use crate::app::runtime::TurnSpec;
use crate::debug_log;
use crate::mcp::MCPManager;
use crate::provider::{ChatMessage, CompletionRequest, LLMProvider, Role};
use crate::semantic::SemanticService;
use crate::tools::registry::ToolRegistry;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Drive one agent turn described by `spec`. Every event it emits is tagged
/// with the spec's conversation id so the app routes it to the right
/// conversation (focused or background). When `spec.auto_approve` is set
/// (background sidechats / sub-agents, where no user is watching) tool calls
/// run without an approval prompt and `question` calls are declined instead
/// of blocking.
pub async fn run_agent_loop(
    spec: TurnSpec,
    tools: Arc<ToolRegistry>,
    mcp: Arc<tokio::sync::Mutex<MCPManager>>,
    semantic: Arc<SemanticService>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) {
    let TurnSpec {
        conversation,
        provider,
        mut messages,
        system_prompt,
        model,
        temperature,
        max_tokens,
        max_steps,
        max_tools_per_step,
        auto_approve,
        tool_output_limit,
        context,
    } = spec;
    let mut collected_tool_calls = Vec::new();
    let mut retries = RetryBudget::default();
    let mut compaction = CompactionState::default();

    inject_semantic_hint(conversation, &semantic, &mut messages).await;

    let compaction_ctx = CompactionCtx {
        conversation,
        provider: &provider,
        context: &context,
        system_prompt: &system_prompt,
        event_tx: &event_tx,
    };

    let ctx = ToolExecContext {
        conversation,
        provider: &provider,
        tools: &tools,
        mcp: &mcp,
        system_prompt: &system_prompt,
        model: &model,
        temperature,
        max_tokens,
        max_steps,
        max_tools_per_step,
        auto_approve,
        tool_output_limit,
        event_tx: &event_tx,
    };

    for step in 0..max_steps {
        let trace = StepTrace::new(conversation, step + 1, max_steps);
        trace.start(messages.len());

        let used = apply_ladder(&compaction_ctx, &mut messages, &mut compaction).await;

        ctx.emit(AgentEvent::BeginAssistantMessage);
        let request =
            build_step_request(&system_prompt, &messages, &model, temperature, max_tokens);
        ctx.emit(AgentEvent::ContextUsage(used));

        let outcome = stream_step(conversation, &provider, request, &event_tx).await;
        let provider_error = match classify_stream(&outcome, &trace) {
            Ok(error) => error,
            Err(fatal) => {
                ctx.emit(AgentEvent::Failed(fatal));
                return;
            }
        };
        let StreamOutcome {
            text: raw,
            got_stop,
            ..
        } = outcome;

        let (tool_calls, parse_errors) = parse_tool_calls_with_errors(&raw);
        let visible_text = strip_tool_calls(&raw);
        trace.parsed(got_stop, &visible_text, &tool_calls);
        trace.parsed_payload(
            got_stop,
            provider_error.as_deref(),
            &raw,
            &visible_text,
            &tool_calls,
        );

        // Оборванный стрим, из которого всё же вынулся целый вызов, — не
        // повод терять ход: продолжаем и предупреждаем.
        if let Some(error) = provider_error {
            if tool_calls.is_empty() {
                ctx.emit(AgentEvent::Failed(error));
                return;
            }
            trace.salvaged(&error, tool_calls.len());
            ctx.emit(AgentEvent::Message(ChatMessage::system(
                "Warning: stream ended early, but a complete tool call was recovered. Continuing.",
            )));
        }

        if tool_calls.is_empty() {
            match finish_or_retry(
                &ctx,
                &trace,
                &mut retries,
                &mut messages,
                &raw,
                &visible_text,
                &parse_errors,
            ) {
                StepEnd::Retry => continue,
                StepEnd::Answer(text) => {
                    trace.turn_done(&text, collected_tool_calls.len());
                    ctx.emit(AgentEvent::Done(AgentResult {
                        text,
                        tool_calls: collected_tool_calls,
                    }));
                    return;
                }
                StepEnd::GiveUp => return,
            }
        }

        messages.push(ChatMessage::assistant(&visible_text));
        if visible_text.is_empty() {
            ctx.emit(AgentEvent::DiscardEmptyAssistant);
        }
        run_tool_calls(
            tool_calls,
            &ctx,
            &trace,
            &mut messages,
            &mut collected_tool_calls,
        )
        .await;
    }

    trace::turn_out_of_steps(conversation, max_steps, collected_tool_calls.len());
    let _ = event_tx.send(AppEvent::Agent {
        conversation,
        event: AgentEvent::Failed(
            "Reached max agent steps before producing a final answer".to_string(),
        ),
    });
}

/// Пройти ступени компакции и вернуть заполненность окна для этого шага.
///
/// Зовётся на каждом шаге, а не на границе хода: один ход бывает в дюжину
/// вызовов, и после роста истории её больше никто не проверит.
async fn apply_ladder(
    ctx: &CompactionCtx<'_>,
    messages: &mut [ChatMessage],
    compaction: &mut CompactionState,
) -> u32 {
    // Мерим здесь: только это место видит, что реально уходит на провод.
    let mut used = context_used(ctx.provider, ctx.system_prompt, messages);
    if prune_tool_output(ctx, used, messages, compaction).await {
        used = context_used(ctx.provider, ctx.system_prompt, messages);
    }
    // Ступень 2 на том же чекпойнте и с порогом выше: ступень 1 бесплатна,
    // поэтому право первого отказа у неё.
    if reset_server_session(ctx, used, messages, compaction).await {
        used = context_used(ctx.provider, ctx.system_prompt, messages);
    }
    used
}

/// Чем кончился шаг без вызовов инструментов.
enum StepEnd {
    /// Модель попросили переписать — идём на следующий шаг.
    Retry,
    /// Готовый ответ хода.
    Answer(String),
    /// Ход провален, событие уже отправлено.
    GiveUp,
}

/// Разобрать конец стрима. `Ok(None)` — можно продолжать, `Ok(Some(e))` —
/// провайдер сбежал, но, может, вызов ещё спасётся; `Err` — ход не спасти.
fn classify_stream(outcome: &StreamOutcome, trace: &StepTrace) -> Result<Option<String>, String> {
    let verdict = outcome.verdict();
    // Таймаут — это своя строка; вторая про «закрылся без stop» удвоила бы
    // его в отчёте `mine`, где каждая запись считается отдельно.
    if !outcome.got_stop && verdict != StreamVerdict::IdleTimeout {
        trace.stream_closed(outcome.text.len());
    }
    match &outcome.end {
        StreamEnd::Completed if outcome.got_stop => trace.provider_ok(outcome.text.len()),
        StreamEnd::ProviderError(error) => {
            trace.error(&format!("reason=provider_error error={error}"))
        }
        StreamEnd::TaskFailed(error) => {
            trace.error(&format!("reason=stream_task_join_error error={error}"))
        }
        StreamEnd::IdleTimeout | StreamEnd::Completed => {}
    }
    match verdict {
        StreamVerdict::IdleTimeout => {
            trace.error("reason=stream_timeout");
            Err("Stream timed out (no data for 120s). Cancelling turn.".to_string())
        }
        StreamVerdict::Ok => Ok(None),
        StreamVerdict::ClosedWithoutStop => Ok(Some(trace.closed_without_stop(outcome.text.len()))),
        StreamVerdict::Failed(error) => Ok(Some(error)),
    }
}

/// Шаг без вызовов инструментов: сломанный `<tool_use>`, пустой ответ или
/// готовый текст. Границы повторов держит `RetryBudget`.
fn finish_or_retry(
    ctx: &ToolExecContext<'_>,
    trace: &StepTrace,
    retries: &mut RetryBudget,
    messages: &mut Vec<ChatMessage>,
    raw: &str,
    visible: &str,
    parse_errors: &[String],
) -> StepEnd {
    // Ноль разобранных вызовов при непустых ошибках — это сломанный блок, а
    // не финальный ответ. Молча закончить ход здесь и значило «агент завис».
    if !parse_errors.is_empty() {
        if let Some(attempt) = retries.take_malformed() {
            // Кладём сырое: провайдеру с полной историей нужно увидеть ошибку.
            messages.push(ChatMessage::assistant(raw));
            messages.push(ChatMessage::user(&malformed_tool_feedback(parse_errors)));
            trace.malformed(attempt, MAX_MALFORMED_TOOL_RETRIES, parse_errors);
            ctx.emit(AgentEvent::Message(ChatMessage::system(&format!(
                "⚠ Malformed tool call (attempt {attempt}/{MAX_MALFORMED_TOOL_RETRIES}) — asking the model to retry"
            ))));
            return StepEnd::Retry;
        }
        trace.malformed_exhausted(parse_errors);
        ctx.emit(AgentEvent::Message(ChatMessage::system(
            "⚠ The model kept emitting malformed tool calls; stopping this turn. Try rephrasing your request.",
        )));
    }

    if !visible.is_empty() {
        messages.push(ChatMessage::assistant(visible));
        return StepEnd::Answer(visible.to_string());
    }

    // Ни текста, ни вызова — ход не сделал ничего. Считать это успехом и
    // значило вернуть «готово» над пустым каталогом.
    trace.empty_assistant();
    ctx.emit(AgentEvent::DiscardEmptyAssistant);
    if let Some(attempt) = retries.take_empty() {
        messages.push(ChatMessage::user(EMPTY_RESPONSE_FEEDBACK));
        trace.empty_retry(attempt, MAX_EMPTY_RESPONSE_RETRIES);
        ctx.emit(AgentEvent::Message(ChatMessage::system(&format!(
            "⚠ Empty reply from the model (attempt {attempt}/{MAX_EMPTY_RESPONSE_RETRIES}) — retrying"
        ))));
        return StepEnd::Retry;
    }
    trace.turn_error("status=empty_response_exhausted");
    ctx.emit(AgentEvent::Failed(
        "The model returned an empty reply and did nothing. Nothing was changed — try rephrasing the request.".to_string(),
    ));
    StepEnd::GiveUp
}

/// How full the window is, from the one source that knows. A provider holding
/// the history itself counts what it was actually sent since its session
/// began; everyone else is measured by the local history we re-send each step.
///
/// The single caller feeds all three consumers — `AgentEvent::ContextUsage`,
/// rung 1 and rung 2 — from the same value, so the status bar can never
/// disagree with what the ladder acted on.
fn context_used(
    provider: &Arc<dyn LLMProvider>,
    system_prompt: &str,
    messages: &[ChatMessage],
) -> u32 {
    provider
        .session_tokens()
        .unwrap_or_else(|| crate::context::conversation_tokens(system_prompt, messages))
}

/// What the ladder already did this turn. Each skip is logged once per turn,
/// not once per step, and rung 2 resets the session at most once.
#[derive(Default)]
struct CompactionState {
    prune_skip_logged: bool,
    session_reset_done: bool,
    /// A re-seed judged pointless once stays judged: retrying it every step
    /// would repeat the whole estimate and the log line with it.
    reset_refused: bool,
    reset_skip_logged: bool,
}

/// What every rung of the ladder needs and no rung changes. Built once per
/// turn; the history and the per-turn state are passed alongside it.
struct CompactionCtx<'a> {
    conversation: ConversationId,
    provider: &'a Arc<dyn LLMProvider>,
    context: &'a crate::context::ContextSpec,
    system_prompt: &'a str,
    event_tx: &'a mpsc::UnboundedSender<AppEvent>,
}

/// Rung 1 of the compaction ladder: once the window is filling up, clear the
/// bodies of the oldest tool results, spilling each full text to disk so
/// `read_file` can fetch it back. Rewrites this turn's message copy and emits
/// `ToolOutputCleared` so the app applies the same edit to its own history.
///
/// Takes the caller's already-computed `used` (one history walk per step) and
/// returns whether it rewrote anything, which makes that number stale.
///
/// Skipped entirely when the provider keeps the history on its own side: those
/// messages are never sent again, so clearing them costs a disk write for
/// nothing (`.docs/context-compaction.md` §2.1).
async fn prune_tool_output(
    ctx: &CompactionCtx<'_>,
    used: u32,
    messages: &mut [ChatMessage],
    state: &mut CompactionState,
) -> bool {
    if !ctx.context.auto_compact {
        return false;
    }
    if ctx.provider.keeps_server_side_history() {
        if !state.prune_skip_logged {
            state.prune_skip_logged = true;
            debug_log::log(
                "context.prune.skipped",
                format!(
                    "conversation={} reason=server_side_history",
                    ctx.conversation
                ),
            );
        }
        return false;
    }
    let Some(snapshot) = ctx.context.budget().snapshot(used) else {
        return false; // Unknown window: the ladder stays off (invariant 12).
    };
    if snapshot.percent_used() < crate::context::PRUNE_TRIGGER_PERCENT {
        return false;
    }
    clear_tool_bodies(ctx, &snapshot, messages).await
}

/// Verbatim tail kept out of every rung's reach, in tokens: the explicit
/// setting, or a quarter of the usable window.
fn protect_tokens(
    ctx: &CompactionCtx<'_>,
    snapshot: &crate::context::budget::BudgetSnapshot,
) -> u32 {
    match ctx.context.preserve_recent_tokens {
        0 => (snapshot.usable / 4).clamp(2_000, 15_000),
        explicit => explicit,
    }
}

/// The clearing itself, without the decision to run it: spill the bodies of
/// the settled tool results, leave a marker in their place, tell the app.
/// Shared by rung 1 and by rung 2, which applies the same edit to the history
/// it is about to re-seed a fresh session with.
///
/// A body is traded for its marker only once it is on disk: a failed spill
/// leaves the result whole rather than naming a file that is not there.
async fn clear_tool_bodies(
    ctx: &CompactionCtx<'_>,
    snapshot: &crate::context::budget::BudgetSnapshot,
    messages: &mut [ChatMessage],
) -> bool {
    let victims = crate::context::prune::plan_prune(
        messages,
        protect_tokens(ctx, snapshot),
        &ctx.context.spill_dir,
    );
    if victims.is_empty() {
        return false;
    }

    let mut spills = Vec::with_capacity(victims.len());
    let mut markers = Vec::with_capacity(victims.len());
    for index in &victims {
        let Some(message) = messages.get(*index) else {
            continue;
        };
        // Named by tool-call id so a re-clear overwrites the same file; the id
        // is model output, so `spill_file_name` sanitises it into a file name.
        let name = crate::context::prune::spill_file_name(message.tool_call_id.as_deref(), *index);
        let path = ctx.context.spill_dir.join(name);
        markers.push((
            *index,
            crate::context::prune::cleared_marker(&path.to_string_lossy()),
        ));
        spills.push((*index, path, message.content.clone()));
    }
    // Blocking file I/O never runs on an async worker (invariant 9); awaited so
    // the spill exists before the model can be told to read it back.
    let written: Vec<usize> = tokio::task::spawn_blocking(move || {
        let mut written = Vec::with_capacity(spills.len());
        for (index, path, content) in spills {
            match crate::util::atomic_write(&path, content.as_bytes()) {
                Ok(()) => written.push(index),
                Err(e) => tracing::warn!("Failed to spill tool output to {}: {e}", path.display()),
            }
        }
        written
    })
    .await
    .unwrap_or_else(|e| {
        tracing::warn!("Tool-output spill task failed: {e}");
        Vec::new()
    });

    let mut cleared = Vec::with_capacity(written.len());
    let mut freed = 0u32;
    let mut rewrote = 0usize;
    for (index, marker) in markers {
        if !written.contains(&index) {
            continue;
        }
        let Some(message) = messages.get_mut(index) else {
            continue;
        };
        freed = freed.saturating_add(crate::context::budget_tokens(&message.content));
        rewrote += 1;
        if let Some(id) = message.tool_call_id.clone() {
            cleared.push((id, marker.clone()));
        }
        message.content = marker;
    }

    debug_log::log(
        "context.prune",
        format!(
            "conversation={} cleared={rewrote} spill_failed={} freed_tokens={freed} percent_used={}",
            ctx.conversation,
            victims.len().saturating_sub(rewrote),
            snapshot.percent_used()
        ),
    );

    // Sent after the spill so the marker never points at a file that is not
    // there yet.
    if !cleared.is_empty() {
        let _ = ctx.event_tx.send(AppEvent::Agent {
            conversation: ctx.conversation,
            event: AgentEvent::ToolOutputCleared {
                cleared,
                freed_tokens: freed,
            },
        });
    }
    rewrote > 0
}

/// Rung 2 for providers that keep the history on their side (DeepSeek's web
/// API). Their branch lives on the server, so the only lever is a new session:
/// `reset()` drops the session state, and the next request re-seeds a fresh
/// one with the system prompt plus the local history as `LOCAL MEMORY`
/// (`.docs/context-compaction.md` §2.1).
///
/// Rung 1's clearing is applied here even though it is normally skipped for
/// these providers: it is worthless while nothing is re-sent, and it is the
/// whole point once everything is — so it runs only on the path that actually
/// re-seeds: a refused or failed reset must not cost a single body. The old
/// server-side session is left in place, an ordinary chat in the account.
///
/// Returns whether it rewrote the history, which makes the caller's `used`
/// stale.
async fn reset_server_session(
    ctx: &CompactionCtx<'_>,
    used: u32,
    messages: &mut [ChatMessage],
    state: &mut CompactionState,
) -> bool {
    if !ctx.context.auto_compact || !ctx.provider.keeps_server_side_history() {
        return false;
    }
    let Some(snapshot) = ctx.context.budget().snapshot(used) else {
        return false; // Unknown window: the ladder stays off (invariant 12).
    };
    if snapshot.percent_used() < crate::context::SESSION_RESET_PERCENT {
        return false;
    }
    let conversation = ctx.conversation;
    if state.session_reset_done || state.reset_refused {
        // A second reset in one turn means the re-seed did not help, and a
        // refusal stands: the history only grows from here.
        if !state.reset_skip_logged {
            state.reset_skip_logged = true;
            let reason = if state.session_reset_done {
                "already_reset_this_turn"
            } else {
                "still_over_usable"
            };
            debug_log::log(
                "context.session_reset.skipped",
                format!(
                    "conversation={conversation} reason={reason} percent_used={}",
                    snapshot.percent_used()
                ),
            );
        }
        return false;
    }

    // Prospective, before anything is touched: the local history the re-seed
    // would carry, less what clearing frees, plus the markers it leaves.
    let victims = crate::context::prune::plan_prune(
        messages,
        protect_tokens(ctx, &snapshot),
        &ctx.context.spill_dir,
    );
    let markers: u32 = victims
        .iter()
        .filter_map(|index| messages.get(*index).map(|message| (*index, message)))
        .map(|(index, message)| {
            crate::context::prune::marker_tokens(
                &ctx.context.spill_dir,
                message.tool_call_id.as_deref(),
                index,
            )
        })
        .sum();
    let after = crate::context::conversation_tokens(ctx.system_prompt, messages)
        .saturating_sub(crate::context::prune::freed_tokens(messages, &victims))
        .saturating_add(markers);
    if after > snapshot.usable {
        // The re-seed would be oversized too — rung 3's job. Nothing is
        // cleared: a body that is never re-sent is cleared for nothing.
        state.reset_refused = true;
        state.reset_skip_logged = true;
        debug_log::log(
            "context.session_reset.skipped",
            format!(
                "conversation={conversation} reason=still_over_usable before_tokens={used} after_tokens={after} usable={}",
                snapshot.usable
            ),
        );
        return false;
    }
    if let Err(e) = ctx.provider.reset().await {
        tracing::warn!("Context session reset failed: {e}");
        debug_log::log(
            "context.session_reset.failed",
            format!("conversation={conversation} error={e}"),
        );
        return false;
    }

    state.session_reset_done = true;
    // Only now: the bodies are cleared for the re-seed that is actually going
    // to happen, and the real figure replaces the estimate.
    clear_tool_bodies(ctx, &snapshot, messages).await;
    let after = crate::context::conversation_tokens(ctx.system_prompt, messages);
    debug_log::log(
        "context.session_reset",
        format!(
            "conversation={conversation} before_tokens={used} after_tokens={after} percent_used={}",
            snapshot.percent_used()
        ),
    );
    let _ = ctx.event_tx.send(AppEvent::Agent {
        conversation,
        event: AgentEvent::SessionReset {
            before_tokens: used,
            after_tokens: after,
        },
    });
    true
}

/// Semantic hint: match the newest user message against the skill and
/// MCP-tool corpora and insert an advisory note the model can act on
/// (`skill` tool / listed MCP tools). Mutates only this turn's local message
/// copy — it is never persisted to the conversation. ONNX inference is
/// CPU-bound, so it runs on a blocking thread, not this task; a
/// not-yet-initialized matcher returns nothing instantly.
///
/// The hint is inserted *before* the user message it annotates, never
/// after: providers that send only the newest tail (DeepSeek's flat prompt)
/// must always deliver the user's text as the final USER INPUT — a trailing
/// system note used to displace it entirely.
async fn inject_semantic_hint(
    conversation: ConversationId,
    semantic: &Arc<SemanticService>,
    messages: &mut Vec<ChatMessage>,
) {
    let Some(user_idx) = messages
        .iter()
        .rposition(|m| matches!(m.role, Role::User) && !m.content.trim().is_empty())
    else {
        return;
    };
    let user_text = messages[user_idx].content.clone();
    let service = Arc::clone(semantic);
    let matches = tokio::task::spawn_blocking(move || service.match_prompt(&user_text))
        .await
        .unwrap_or_default();
    if let Some(hint) = semantic.render_hint(&matches) {
        let described: Vec<String> = matches
            .skills
            .iter()
            .map(|m| format!("skill:{}(d={:.3},s={:.3})", m.slug, m.dense, m.sparse))
            .chain(
                matches
                    .mcp_tools
                    .iter()
                    .map(|m| format!("mcp:{}(d={:.3},s={:.3})", m.full_name, m.dense, m.sparse)),
            )
            .collect();
        debug_log::log(
            "agent.semantic_hint",
            format!(
                "conversation={conversation} matches={}",
                described.join(", ")
            ),
        );
        messages.insert(user_idx, ChatMessage::system(&hint));
    }
}

/// Stream one step's completion, emitting visible-text deltas as they
/// arrive: tool-call syntax is stripped from the accumulated text and only
/// the newly-appended visible suffix is streamed. The tracker is per-step —
/// the accumulated text is append-only within one completion, which is
/// exactly what its incremental freezing relies on.
async fn stream_step(
    conversation: ConversationId,
    provider: &Arc<dyn LLMProvider>,
    request: CompletionRequest,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
) -> StreamOutcome {
    let mut streamed_visible = String::new();
    let mut visible_tracker = StreamTextTracker::default();
    let progress_tx = event_tx.clone();
    collect_stream(provider, request, |full_response| {
        let next_visible = visible_tracker.visible(full_response);
        if next_visible.starts_with(&streamed_visible) {
            let delta = &next_visible[streamed_visible.len()..];
            if !delta.is_empty() {
                let _ = progress_tx.send(AppEvent::Agent {
                    conversation,
                    event: AgentEvent::Chunk(delta.to_string()),
                });
            }
        } else if !next_visible.is_empty() {
            let _ = progress_tx.send(AppEvent::Agent {
                conversation,
                event: AgentEvent::Message(ChatMessage::system(
                    "⚠ Streaming sync issue — agent will continue",
                )),
            });
        }
        streamed_visible = next_visible;
    })
    .await
}

/// Build one step's completion request: system prompt first, then the
/// running history. Shared by the main and sub-agent loops (the two copies
/// had already been flagged as drift-prone).
pub(crate) fn build_step_request(
    system_prompt: &str,
    messages: &[ChatMessage],
    model: &str,
    temperature: f32,
    max_tokens: u32,
) -> CompletionRequest {
    let mut request_messages = Vec::with_capacity(messages.len() + 1);
    request_messages.push(ChatMessage::system(system_prompt));
    request_messages.extend(messages.iter().cloned());
    CompletionRequest {
        messages: request_messages,
        model: model.to_string(),
        temperature,
        max_tokens,
        stream: true,
    }
}

/// Nudge sent back after an empty reply. Deliberately concrete about the two
/// acceptable shapes of a turn, since the failure is the model producing
/// neither.
pub(crate) const EMPTY_RESPONSE_FEEDBACK: &str = "Your last reply was completely empty. Answer the request now: either      write the text of the answer, or emit a tool call in the documented      <tool_use> form. Do not reply with nothing.";

/// Correction message handed back to the model after it emitted a `<tool_use>`
/// block that couldn't be parsed. Names each concrete problem and restates the
/// exact required shape so the model can re-issue the call. Shared by the
/// main and sub-agent loops.
pub(crate) fn malformed_tool_feedback(errors: &[String]) -> String {
    format!(
        "Your previous message contained a tool call that could NOT be parsed, \
         so it was NOT executed:\n- {}\n\nRe-issue it using exactly this shape, \
         and nothing after </tool_use>:\n\
         <tool_use>\n<name>TOOL_NAME</name>\n<arguments>\n{{ \"key\": \"value\" }}\n</arguments>\n</tool_use>\n\n\
         The arguments must be a single valid JSON object. Inside JSON string \
         values, escape every backslash as \\\\ and every double quote as \\\" \
         (e.g. a Windows path: \"C:\\\\Users\\\\Aver\\\\file.png\").",
        errors.join("\n- ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::fake::FakeProvider;

    /// The provider seam is substitutable: with a `FakeProvider` the agent loop
    /// runs to completion — streaming visible text, then `AgentEvent::Done` — with no
    /// network, no proof-of-work, and no DeepSeek token.
    #[tokio::test]
    async fn agent_loop_streams_plain_response_then_done() {
        let provider: Arc<dyn LLMProvider> =
            Arc::new(FakeProvider::with_response("Hello, world!").chunked(3));
        let tools = Arc::new(ToolRegistry::new());
        let mcp = Arc::new(tokio::sync::Mutex::new(MCPManager::new()));
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let cid = ConversationId::next();

        run_agent_loop(
            TurnSpec {
                conversation: cid,
                provider,
                messages: vec![ChatMessage::user("hi")],
                system_prompt: "system".to_string(),
                model: "fake".to_string(),
                temperature: 0.0,
                max_tokens: 128,
                max_steps: 4,
                max_tools_per_step: 4,
                auto_approve: false,
                tool_output_limit: 0,
                context: crate::context::ContextSpec::default(),
            },
            tools,
            mcp,
            SemanticService::disabled(),
            event_tx,
        )
        .await;

        let mut events = Vec::new();
        while let Ok(ev) = event_rx.try_recv() {
            events.push(ev);
        }

        // First event opens the assistant message, tagged with our conversation.
        assert!(
            matches!(events.first(), Some(AppEvent::Agent { conversation: id, event: AgentEvent::BeginAssistantMessage }) if *id == cid)
        );

        // The streamed deltas reassemble into the full visible response.
        let streamed: String = events
            .iter()
            .filter_map(|e| match e {
                AppEvent::Agent {
                    conversation: _,
                    event: AgentEvent::Chunk(s),
                } => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(streamed, "Hello, world!");

        // The turn ends with AgentEvent::Done carrying the final text and no tool calls.
        let done = events
            .iter()
            .find_map(|e| match e {
                AppEvent::Agent {
                    conversation: _,
                    event: AgentEvent::Done(result),
                } => Some(result),
                _ => None,
            })
            .expect("agent loop should finish with AgentEvent::Done");
        assert_eq!(done.text, "Hello, world!");
        assert!(done.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn agent_loop_reports_error_when_stream_ends_without_stop() {
        let provider: Arc<dyn LLMProvider> = Arc::new(
            FakeProvider::with_response("partial")
                .chunked(2)
                .abrupt_eof(),
        );
        let tools = Arc::new(ToolRegistry::new());
        let mcp = Arc::new(tokio::sync::Mutex::new(MCPManager::new()));
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let cid = ConversationId::next();

        run_agent_loop(
            TurnSpec {
                conversation: cid,
                provider,
                messages: vec![ChatMessage::user("hi")],
                system_prompt: "system".to_string(),
                model: "fake".to_string(),
                temperature: 0.0,
                max_tokens: 128,
                max_steps: 4,
                max_tools_per_step: 4,
                auto_approve: false,
                tool_output_limit: 0,
                context: crate::context::ContextSpec::default(),
            },
            tools,
            mcp,
            SemanticService::disabled(),
            event_tx,
        )
        .await;

        let mut saw_done = false;
        let mut saw_error = None;
        while let Ok(ev) = event_rx.try_recv() {
            match ev {
                AppEvent::Agent {
                    conversation: _,
                    event: AgentEvent::Done(_),
                } => saw_done = true,
                AppEvent::Agent {
                    conversation: id,
                    event: AgentEvent::Failed(message),
                } if id == cid => saw_error = Some(message),
                _ => {}
            }
        }

        assert!(!saw_done, "abrupt EOF must not look like a successful turn");
        let error = saw_error.expect("abrupt EOF should surface as AgentEvent::Failed");
        assert!(error.contains("without stop"));
    }

    /// Found by the harness on a real "build me a page" run: DeepSeek closed
    /// the stream with `got_stop=true` and zero bytes, and the loop reported
    /// `turn.done status=success`. The user asked for three files and got an
    /// empty directory, in 1.4 seconds, with no error anywhere.
    #[tokio::test]
    async fn agent_loop_retries_an_empty_reply_then_succeeds() {
        // Empty, empty, then a real answer: within the retry budget.
        let provider: Arc<dyn LLMProvider> = Arc::new(FakeProvider::with_responses(vec![
            String::new(),
            String::new(),
            "Here is the answer.".to_string(),
        ]));
        let (done, error) = drive_turn(provider, 6).await;
        assert!(error.is_none(), "should have recovered: {error:?}");
        assert_eq!(
            done.expect("turn should finish").text,
            "Here is the answer."
        );
    }

    #[tokio::test]
    async fn agent_loop_reports_an_error_when_every_reply_is_empty() {
        // One more empty reply than the budget allows.
        let provider: Arc<dyn LLMProvider> = Arc::new(FakeProvider::with_responses(vec![
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        ]));
        let (done, error) = drive_turn(provider, 6).await;
        assert!(
            done.is_none(),
            "an empty turn must not be reported as a success"
        );
        let message = error.expect("empty turn should surface an error");
        assert!(message.contains("empty"), "{message}");
        assert!(
            message.contains("Nothing was changed"),
            "the message must say no work happened: {message}"
        );
    }

    /// Run one turn and collect its terminal events.
    async fn drive_turn(
        provider: Arc<dyn LLMProvider>,
        max_steps: usize,
    ) -> (Option<AgentResult>, Option<String>) {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let cid = ConversationId::next();
        run_agent_loop(
            TurnSpec {
                conversation: cid,
                provider,
                messages: vec![ChatMessage::user("hi")],
                system_prompt: "system".to_string(),
                model: "fake".to_string(),
                temperature: 0.0,
                max_tokens: 128,
                max_steps,
                max_tools_per_step: 4,
                auto_approve: true,
                tool_output_limit: 0,
                context: crate::context::ContextSpec::default(),
            },
            Arc::new(ToolRegistry::new()),
            Arc::new(tokio::sync::Mutex::new(MCPManager::new())),
            SemanticService::disabled(),
            event_tx,
        )
        .await;

        let (mut done, mut error) = (None, None);
        while let Ok(event) = event_rx.try_recv() {
            match event {
                AppEvent::Agent {
                    conversation: id,
                    event: AgentEvent::Done(result),
                } if id == cid => done = Some(result),
                AppEvent::Agent {
                    conversation: id,
                    event: AgentEvent::Failed(message),
                } if id == cid => error = Some(message),
                _ => {}
            }
        }
        (done, error)
    }

    #[tokio::test]
    async fn agent_loop_salvages_complete_tool_call_when_stream_ends_early() {
        let response = r#"<tool_use><name>question</name><arguments>{"question":"q","options":[],"allow_custom":false}</arguments></tool_use>"#;
        // Two scripted replies: the salvaged tool call, then a normal answer.
        // A single reply used to be enough because the drained queue returned
        // an empty string and the loop ended the turn on it — that silent
        // "success on nothing" is now an error, so the fake has to behave like
        // a real provider and actually answer.
        let provider: Arc<dyn LLMProvider> = Arc::new(
            FakeProvider::with_responses(vec![response.to_string(), "All done.".to_string()])
                .abrupt_eof(),
        );
        let tools = Arc::new(ToolRegistry::new());
        let mcp = Arc::new(tokio::sync::Mutex::new(MCPManager::new()));
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let cid = ConversationId::next();

        run_agent_loop(
            TurnSpec {
                conversation: cid,
                provider,
                messages: vec![ChatMessage::user("hi")],
                system_prompt: "system".to_string(),
                model: "fake".to_string(),
                temperature: 0.0,
                max_tokens: 128,
                max_steps: 4,
                max_tools_per_step: 4,
                auto_approve: true,
                tool_output_limit: 0,
                context: crate::context::ContextSpec::default(),
            },
            tools,
            mcp,
            SemanticService::disabled(),
            event_tx,
        )
        .await;

        let mut saw_done = None;
        let mut saw_error = None;
        while let Ok(ev) = event_rx.try_recv() {
            match ev {
                AppEvent::Agent {
                    conversation: id,
                    event: AgentEvent::Done(result),
                } if id == cid => saw_done = Some(result),
                AppEvent::Agent {
                    conversation: id,
                    event: AgentEvent::Failed(message),
                } if id == cid => saw_error = Some(message),
                _ => {}
            }
        }

        assert!(saw_error.is_none(), "complete tool call should be salvaged");
        let done = saw_done.expect("salvaged tool call should still finish the turn");
        assert_eq!(done.tool_calls.len(), 1);
        assert_eq!(done.tool_calls[0].name, "question");
    }

    /// A tool whose output is deliberately larger than any sane cap.
    struct LoudTool;

    #[async_trait::async_trait]
    impl crate::tools::Tool for LoudTool {
        fn definition(&self) -> crate::tools::ToolDefinition {
            crate::tools::ToolDefinition {
                name: "loud".to_string(),
                description: "emits a lot".to_string(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }
        }
        async fn execute(&self, _args: serde_json::Value) -> crate::tools::ToolResult {
            crate::tools::ToolResult {
                content: format!("START{}END", "x".repeat(5_000)),
                is_error: false,
            }
        }
    }

    /// Rung 0 end to end: the model's next request must carry the capped tool
    /// result, while the event stream still reports what the tool really said.
    #[tokio::test]
    async fn tool_output_is_capped_before_it_reaches_the_model() {
        let call = r#"<tool_use><name>loud</name><arguments>{}</arguments></tool_use>"#;
        let fake = Arc::new(FakeProvider::with_responses(vec![
            call.to_string(),
            "Done.".to_string(),
        ]));
        let provider: Arc<dyn LLMProvider> = Arc::clone(&fake) as Arc<dyn LLMProvider>;
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(LoudTool));
        let mcp = Arc::new(tokio::sync::Mutex::new(MCPManager::new()));
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let cid = ConversationId::next();

        run_agent_loop(
            TurnSpec {
                conversation: cid,
                provider,
                messages: vec![ChatMessage::user("go")],
                system_prompt: "system".to_string(),
                model: "fake".to_string(),
                temperature: 0.0,
                max_tokens: 128,
                max_steps: 4,
                max_tools_per_step: 4,
                auto_approve: true,
                tool_output_limit: 500,
                context: crate::context::ContextSpec::default(),
            },
            tools,
            mcp,
            SemanticService::disabled(),
            event_tx,
        )
        .await;

        let second = fake
            .request(1)
            .expect("a second request after the tool ran");
        let tool_message = second
            .iter()
            .find(|m| m.role == Role::Tool)
            .expect("tool result in the follow-up request");
        assert!(
            tool_message.content.chars().count() < 1_000,
            "capped, got {} chars",
            tool_message.content.chars().count()
        );
        assert!(tool_message.content.starts_with("START"), "head kept");
        assert!(tool_message.content.ends_with("END"), "tail kept");

        // The event stream is the user's and the harness's view: uncapped.
        let mut reported = None;
        while let Ok(ev) = event_rx.try_recv() {
            if let AppEvent::Agent {
                conversation: id,
                event: AgentEvent::Done(result),
            } = ev
                && id == cid
            {
                reported = result.tool_calls.first().and_then(|c| c.result.clone());
            }
        }
        let reported = reported.expect("tool call reported on the event stream");
        assert!(
            reported.chars().count() > 5_000,
            "event stream keeps the full output, got {}",
            reported.chars().count()
        );
    }

    /// A tool whose single result on its own crosses the prune trigger.
    struct HugeTool;

    #[async_trait::async_trait]
    impl crate::tools::Tool for HugeTool {
        fn definition(&self) -> crate::tools::ToolDefinition {
            crate::tools::ToolDefinition {
                name: "huge".to_string(),
                description: "emits a whole context window".to_string(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }
        }
        async fn execute(&self, _args: serde_json::Value) -> crate::tools::ToolResult {
            crate::tools::ToolResult {
                content: "h".repeat(30_000),
                is_error: false,
            }
        }
    }

    /// A turn that fills the window with its own tool calls. Rung 1 has to run
    /// per step: checked once at the turn boundary it would never fire again.
    #[tokio::test]
    async fn an_old_tool_result_is_cleared_mid_turn() {
        let call = r#"<tool_use><name>huge</name><arguments>{}</arguments></tool_use>"#;
        let dir = std::env::temp_dir().join(format!("pooprusteek-prune-{}", uuid::Uuid::new_v4()));
        let fake = Arc::new(FakeProvider::with_responses(vec![
            call.to_string(),
            call.to_string(),
            "Done.".to_string(),
        ]));
        let provider: Arc<dyn LLMProvider> = Arc::clone(&fake) as Arc<dyn LLMProvider>;
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(HugeTool));
        let (event_tx, _event_rx) = mpsc::unbounded_channel();

        run_agent_loop(
            TurnSpec {
                conversation: ConversationId::next(),
                provider,
                messages: vec![ChatMessage::user("go")],
                system_prompt: "system".to_string(),
                model: "fake".to_string(),
                temperature: 0.0,
                max_tokens: 128,
                max_steps: 5,
                max_tools_per_step: 4,
                auto_approve: true,
                tool_output_limit: 0,
                context: crate::context::ContextSpec {
                    auto_compact: true,
                    context_window: 15_000,
                    provider_window: 0,
                    reserved_tokens: 3_000,
                    preserve_recent_tokens: 500,
                    spill_dir: dir.clone(),
                },
            },
            tools,
            Arc::new(tokio::sync::Mutex::new(MCPManager::new())),
            SemanticService::disabled(),
            event_tx,
        )
        .await;

        let tool_bodies = |n: usize| -> Vec<String> {
            fake.request(n)
                .unwrap_or_else(|| panic!("request {n} went to the provider"))
                .iter()
                .filter(|m| m.role == Role::Tool)
                .map(|m| m.content.clone())
                .collect()
        };

        // Second step: the only result so far is the one just handed back, so
        // it is still whole even though the window is already over the trigger.
        let second = tool_bodies(1);
        assert_eq!(second.len(), 1);
        assert!(
            !crate::context::prune::is_cleared(&second[0]),
            "the in-flight result must survive"
        );

        // Third step: the first result is settled now, and goes.
        let third = tool_bodies(2);
        assert_eq!(third.len(), 2);
        assert!(
            crate::context::prune::is_cleared(&third[0]),
            "the older result should be a marker by now, got {} chars",
            third[0].chars().count()
        );
        assert_eq!(third[1], "h".repeat(30_000), "the newest one is untouched");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Same turn, but the provider keeps the history on its side: clearing a
    /// local message would change nothing that is ever sent again, so rung 1
    /// must not run at all (`.docs/context-compaction.md` §2.1). The window is
    /// sized to land between the two triggers — over rung 1's, under rung 2's,
    /// which does clear for these providers before resetting the session.
    #[tokio::test]
    async fn rung_one_does_nothing_when_the_provider_keeps_the_history() {
        let call = r#"<tool_use><name>huge</name><arguments>{}</arguments></tool_use>"#;
        let dir = std::env::temp_dir().join(format!("pooprusteek-prune-{}", uuid::Uuid::new_v4()));
        let fake = Arc::new(
            FakeProvider::with_responses(vec![
                call.to_string(),
                call.to_string(),
                "Done.".to_string(),
            ])
            .server_side_history(),
        );
        let provider: Arc<dyn LLMProvider> = Arc::clone(&fake) as Arc<dyn LLMProvider>;
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(HugeTool));
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        run_agent_loop(
            TurnSpec {
                conversation: ConversationId::next(),
                provider,
                messages: vec![ChatMessage::user("go")],
                system_prompt: "system".to_string(),
                model: "fake".to_string(),
                temperature: 0.0,
                max_tokens: 128,
                max_steps: 5,
                max_tools_per_step: 4,
                auto_approve: true,
                tool_output_limit: 0,
                context: crate::context::ContextSpec {
                    auto_compact: true,
                    // usable = 25k against ~20k on the third step: 80%.
                    context_window: 28_000,
                    provider_window: 0,
                    reserved_tokens: 3_000,
                    preserve_recent_tokens: 500,
                    spill_dir: dir.clone(),
                },
            },
            tools,
            Arc::new(tokio::sync::Mutex::new(MCPManager::new())),
            SemanticService::disabled(),
            event_tx,
        )
        .await;

        // The same third step that clears the older result for a local-history
        // provider: here both bodies reach the model whole.
        let third: Vec<String> = fake
            .request(2)
            .expect("request 2 went to the provider")
            .iter()
            .filter(|m| m.role == Role::Tool)
            .map(|m| m.content.clone())
            .collect();
        assert_eq!(third.len(), 2);
        assert_eq!(third[0], "h".repeat(30_000), "nothing may be cleared");
        assert_eq!(third[1], "h".repeat(30_000), "nothing may be cleared");

        let mut cleared_events = 0;
        while let Ok(ev) = event_rx.try_recv() {
            if matches!(
                ev,
                AppEvent::Agent {
                    event: AgentEvent::ToolOutputCleared { .. },
                    ..
                }
            ) {
                cleared_events += 1;
            }
        }
        assert_eq!(cleared_events, 0, "no ToolOutputCleared may be emitted");
        assert_eq!(fake.resets(), 0, "80% is below rung 2's trigger");
        assert!(!dir.exists(), "no spill file may be written");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Property: a result the model has not reacted to yet is never cleared,
    /// whatever the budget says — otherwise it gets a marker for what it just
    /// asked for (`.docs/context-compaction.md` §5.3).
    #[tokio::test]
    async fn a_tool_result_the_model_has_not_answered_yet_is_never_cleared() {
        let call = r#"<tool_use><name>huge</name><arguments>{}</arguments></tool_use>"#;
        let dir = std::env::temp_dir().join(format!("pooprusteek-prune-{}", uuid::Uuid::new_v4()));
        let fake = Arc::new(FakeProvider::with_responses(vec![
            call.to_string(),
            "Done.".to_string(),
        ]));
        let provider: Arc<dyn LLMProvider> = Arc::clone(&fake) as Arc<dyn LLMProvider>;
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(HugeTool));
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        run_agent_loop(
            TurnSpec {
                conversation: ConversationId::next(),
                provider,
                messages: vec![ChatMessage::user("go")],
                system_prompt: "system".to_string(),
                model: "fake".to_string(),
                temperature: 0.0,
                max_tokens: 128,
                max_steps: 5,
                max_tools_per_step: 4,
                auto_approve: true,
                tool_output_limit: 0,
                context: crate::context::ContextSpec {
                    auto_compact: true,
                    context_window: 15_000,
                    provider_window: 0,
                    reserved_tokens: 3_000,
                    // Absurdly low on purpose: only the tail guard can save it.
                    preserve_recent_tokens: 1,
                    spill_dir: dir.clone(),
                },
            },
            tools,
            Arc::new(tokio::sync::Mutex::new(MCPManager::new())),
            SemanticService::disabled(),
            event_tx,
        )
        .await;

        let request = fake
            .request(1)
            .expect("a second request after the tool ran");
        let tool_message = request
            .iter()
            .find(|m| m.role == Role::Tool)
            .expect("the tool result reaches the model");
        assert_eq!(
            tool_message.content,
            "h".repeat(30_000),
            "the model must see the result it just asked for, not a marker"
        );

        while let Ok(ev) = event_rx.try_recv() {
            assert!(
                !matches!(
                    ev,
                    AppEvent::Agent {
                        event: AgentEvent::ToolOutputCleared { .. },
                        ..
                    }
                ),
                "nothing was settled enough to clear"
            );
        }
        assert!(!dir.exists(), "nothing should have been spilled");
    }

    /// Rung 1 end to end: an old tool body reaches the provider as a marker,
    /// the recent one is untouched, the original text is on disk under the
    /// tool-call id, and the app is told so it can clear its own history.
    #[tokio::test]
    async fn old_tool_output_is_cleared_and_spilled_before_the_first_step() {
        let old_body = "o".repeat(30_000);
        let recent_body = "r".repeat(300);
        let dir = std::env::temp_dir().join(format!("pooprusteek-prune-{}", uuid::Uuid::new_v4()));

        let fake = Arc::new(FakeProvider::with_response("Done."));
        let provider: Arc<dyn LLMProvider> = Arc::clone(&fake) as Arc<dyn LLMProvider>;
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let cid = ConversationId::next();

        run_agent_loop(
            TurnSpec {
                conversation: cid,
                provider,
                // Ids deliberately unlike the message indices, so a spill named
                // after the index would not pass.
                messages: vec![
                    ChatMessage::user("go"),
                    ChatMessage::assistant("<tool_use>…</tool_use>"),
                    ChatMessage::tool("call/old..9", &old_body),
                    ChatMessage::assistant("<tool_use>…</tool_use>"),
                    ChatMessage::tool("call-new-8", &recent_body),
                ],
                system_prompt: "system".to_string(),
                model: "fake".to_string(),
                temperature: 0.0,
                max_tokens: 128,
                max_steps: 1,
                max_tools_per_step: 4,
                auto_approve: true,
                tool_output_limit: 0,
                context: crate::context::ContextSpec {
                    auto_compact: true,
                    // usable = 12k against ~10k used: over the 70% trigger.
                    context_window: 15_000,
                    provider_window: 0,
                    reserved_tokens: 3_000,
                    preserve_recent_tokens: 500,
                    spill_dir: dir.clone(),
                },
            },
            Arc::new(ToolRegistry::new()),
            Arc::new(tokio::sync::Mutex::new(MCPManager::new())),
            SemanticService::disabled(),
            event_tx,
        )
        .await;

        let request = fake.request(0).expect("one request went to the provider");
        let tool_messages: Vec<&ChatMessage> =
            request.iter().filter(|m| m.role == Role::Tool).collect();
        assert_eq!(tool_messages.len(), 2);
        assert!(
            crate::context::prune::is_cleared(&tool_messages[0].content),
            "old body replaced by the marker, got {}",
            tool_messages[0].content
        );
        assert_eq!(
            tool_messages[1].content, recent_body,
            "the protected tail is untouched"
        );

        // Named after the sanitised tool-call id, not the message index.
        let spilled = dir.join("call_old__9.txt");
        let on_disk = std::fs::read_to_string(&spilled)
            .unwrap_or_else(|e| panic!("spill at {}: {e}", spilled.display()));
        assert_eq!(on_disk, old_body);
        assert!(
            tool_messages[0].content.contains("call_old__9.txt"),
            "the marker names the spill path, got {}",
            tool_messages[0].content
        );

        // The app needs the same edit applied to its own history.
        let mut cleared_event = None;
        while let Ok(ev) = event_rx.try_recv() {
            if let AppEvent::Agent {
                conversation,
                event:
                    AgentEvent::ToolOutputCleared {
                        cleared,
                        freed_tokens,
                    },
            } = ev
            {
                assert_eq!(conversation, cid);
                assert!(freed_tokens > 0, "the freed estimate is reported");
                cleared_event = Some(cleared);
            }
        }
        let cleared = cleared_event.expect("ToolOutputCleared emitted");
        assert_eq!(cleared.len(), 1, "only the old result was cleared");
        assert_eq!(cleared[0].0, "call/old..9", "carries the raw tool-call id");
        assert_eq!(
            cleared[0].1, tool_messages[0].content,
            "the marker matches what the provider saw"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A spill that cannot be written leaves the body where it is. The marker
    /// would name a file that is not there, and the text is gone from the
    /// runner's history, the app's, and the saved session at once.
    #[tokio::test]
    async fn a_body_whose_spill_fails_is_never_replaced_by_a_marker() {
        let old_body = "o".repeat(30_000);
        // Unwritable in a portable way: the spill directory's parent is an
        // ordinary file, so `create_dir_all` cannot succeed on any platform.
        let blocker =
            std::env::temp_dir().join(format!("pooprusteek-blocked-{}", uuid::Uuid::new_v4()));
        std::fs::write(&blocker, b"not a directory").expect("the blocking file is created");
        let dir = blocker.join("spill");

        let fake = Arc::new(FakeProvider::with_response("Done."));
        let provider: Arc<dyn LLMProvider> = Arc::clone(&fake) as Arc<dyn LLMProvider>;
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        run_agent_loop(
            TurnSpec {
                conversation: ConversationId::next(),
                provider,
                messages: vec![
                    ChatMessage::user("go"),
                    ChatMessage::assistant("<tool_use>…</tool_use>"),
                    ChatMessage::tool("call-old-1", &old_body),
                    ChatMessage::assistant("<tool_use>…</tool_use>"),
                    ChatMessage::tool("call-new-2", &"r".repeat(300)),
                ],
                system_prompt: "system".to_string(),
                model: "fake".to_string(),
                temperature: 0.0,
                max_tokens: 128,
                max_steps: 1,
                max_tools_per_step: 4,
                auto_approve: true,
                tool_output_limit: 0,
                context: crate::context::ContextSpec {
                    auto_compact: true,
                    // The same over-the-trigger window as the rung-1 test.
                    context_window: 15_000,
                    provider_window: 0,
                    reserved_tokens: 3_000,
                    preserve_recent_tokens: 500,
                    spill_dir: dir.clone(),
                },
            },
            Arc::new(ToolRegistry::new()),
            Arc::new(tokio::sync::Mutex::new(MCPManager::new())),
            SemanticService::disabled(),
            event_tx,
        )
        .await;

        let request = fake.request(0).expect("one request went to the provider");
        let tool_messages: Vec<&ChatMessage> =
            request.iter().filter(|m| m.role == Role::Tool).collect();
        assert_eq!(tool_messages.len(), 2);
        assert_eq!(
            tool_messages[0].content, old_body,
            "the body outlives a spill that could not be written"
        );
        while let Ok(ev) = event_rx.try_recv() {
            assert!(
                !matches!(
                    ev,
                    AppEvent::Agent {
                        event: AgentEvent::ToolOutputCleared { .. },
                        ..
                    }
                ),
                "the app must not be told to clear what is only in memory"
            );
        }
        assert!(!dir.exists(), "the spill directory could not be created");

        let _ = std::fs::remove_file(&blocker);
    }

    /// A history that is over rung 2's trigger: one old tool result, one
    /// in-flight one, and an assistant reply between them.
    fn overfull_history(user_body: &str, old_body: &str, recent_body: &str) -> Vec<ChatMessage> {
        vec![
            ChatMessage::user(user_body),
            ChatMessage::assistant("<tool_use>…</tool_use>"),
            ChatMessage::tool("call-old-1", old_body),
            ChatMessage::assistant("<tool_use>…</tool_use>"),
            ChatMessage::tool("call-new-2", recent_body),
        ]
    }

    /// Rung 2 end to end: over 90%, the server-side history is re-seeded — the
    /// bodies are cleared first (rung 1's edit, applied here because this is
    /// the moment it starts to count), then the session is dropped once and
    /// the app is told (`.docs/context-compaction.md` §2.1).
    #[tokio::test]
    async fn a_full_server_side_history_is_cleared_and_the_session_reset() {
        let dir = std::env::temp_dir().join(format!("pooprusteek-reset-{}", uuid::Uuid::new_v4()));
        let fake = Arc::new(FakeProvider::with_response("Done.").server_side_history());
        let provider: Arc<dyn LLMProvider> = Arc::clone(&fake) as Arc<dyn LLMProvider>;
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let cid = ConversationId::next();

        run_agent_loop(
            TurnSpec {
                conversation: cid,
                provider,
                messages: overfull_history("go", &"o".repeat(33_000), &"r".repeat(300)),
                system_prompt: "system".to_string(),
                model: "fake".to_string(),
                temperature: 0.0,
                max_tokens: 128,
                max_steps: 1,
                max_tools_per_step: 4,
                auto_approve: true,
                tool_output_limit: 0,
                context: crate::context::ContextSpec {
                    auto_compact: true,
                    // usable = 12k against ~11.1k used: over the 90% trigger.
                    context_window: 15_000,
                    provider_window: 0,
                    reserved_tokens: 3_000,
                    preserve_recent_tokens: 500,
                    spill_dir: dir.clone(),
                },
            },
            Arc::new(ToolRegistry::new()),
            Arc::new(tokio::sync::Mutex::new(MCPManager::new())),
            SemanticService::disabled(),
            event_tx,
        )
        .await;

        let request = fake.request(0).expect("one request went to the provider");
        let tool_messages: Vec<&ChatMessage> =
            request.iter().filter(|m| m.role == Role::Tool).collect();
        assert_eq!(tool_messages.len(), 2);
        assert!(
            crate::context::prune::is_cleared(&tool_messages[0].content),
            "the settled body is cleared before the session is re-seeded"
        );
        assert_eq!(
            tool_messages[1].content,
            "r".repeat(300),
            "the in-flight tail survives rung 2 exactly as it survives rung 1"
        );
        assert_eq!(fake.resets(), 1, "the session is reset exactly once");

        let mut reset_event = None;
        while let Ok(ev) = event_rx.try_recv() {
            if let AppEvent::Agent {
                conversation,
                event:
                    AgentEvent::SessionReset {
                        before_tokens,
                        after_tokens,
                    },
            } = ev
            {
                assert_eq!(conversation, cid);
                reset_event = Some((before_tokens, after_tokens));
            }
        }
        let (before, after) = reset_event.expect("SessionReset emitted");
        assert!(
            after < before,
            "the fresh session is re-seeded with less than the old one held: {after} vs {before}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Step 4 of rung 2: when even the cleared history would not fit, a reset
    /// would only re-send something oversized, so it does not happen — and
    /// nothing is cleared either. For a provider that keeps the history, a body
    /// cleared without a re-seed never goes on the wire again: it is pure loss,
    /// and the app applies the same edit to the session it saves. Here the bulk
    /// is a user message, which no rung below 3 can touch.
    #[tokio::test]
    async fn no_session_reset_when_the_cleared_history_still_overflows() {
        let dir = std::env::temp_dir().join(format!("pooprusteek-reset-{}", uuid::Uuid::new_v4()));
        let fake = Arc::new(FakeProvider::with_response("Done.").server_side_history());
        let provider: Arc<dyn LLMProvider> = Arc::clone(&fake) as Arc<dyn LLMProvider>;
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        run_agent_loop(
            TurnSpec {
                conversation: ConversationId::next(),
                provider,
                // 15k tokens of user text alone, against a 12k usable window.
                messages: overfull_history(&"u".repeat(45_000), &"o".repeat(30_000), "r"),
                system_prompt: "system".to_string(),
                model: "fake".to_string(),
                temperature: 0.0,
                max_tokens: 128,
                max_steps: 1,
                max_tools_per_step: 4,
                auto_approve: true,
                tool_output_limit: 0,
                context: crate::context::ContextSpec {
                    auto_compact: true,
                    context_window: 15_000,
                    provider_window: 0,
                    reserved_tokens: 3_000,
                    preserve_recent_tokens: 500,
                    spill_dir: dir.clone(),
                },
            },
            Arc::new(ToolRegistry::new()),
            Arc::new(tokio::sync::Mutex::new(MCPManager::new())),
            SemanticService::disabled(),
            event_tx,
        )
        .await;

        let request = fake.request(0).expect("one request went to the provider");
        let old = request
            .iter()
            .find(|m| m.role == Role::Tool)
            .expect("the old tool result is still in the history");
        assert_eq!(
            old.content,
            "o".repeat(30_000),
            "a refused re-seed must not cost a single body"
        );
        assert_eq!(
            fake.resets(),
            0,
            "a re-seed that does not fit is worse than no re-seed"
        );
        while let Ok(ev) = event_rx.try_recv() {
            assert!(
                !matches!(
                    ev,
                    AppEvent::Agent {
                        event: AgentEvent::SessionReset { .. }
                            | AgentEvent::ToolOutputCleared { .. },
                        ..
                    }
                ),
                "nothing was reset or cleared, so nothing may be announced"
            );
        }
        assert!(!dir.exists(), "nothing may be spilled on the refusal path");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The refusal is decided once per turn, like every other skip: without a
    /// guard the estimate is redone — and its trace line repeated — on every
    /// step, since `session_reset_done` is only set by a reset that happened.
    #[tokio::test]
    async fn a_refused_session_reset_is_decided_once_per_turn() {
        let trace =
            std::env::temp_dir().join(format!("pooprusteek-trace-{}.log", uuid::Uuid::new_v4()));
        let dir = std::env::temp_dir().join(format!("pooprusteek-reset-{}", uuid::Uuid::new_v4()));
        crate::debug_log::configure(trace.clone(), crate::debug_log::Format::Human);
        crate::debug_log::set_enabled(true).expect("the trace sink opens");

        let call = r#"<tool_use><name>loud</name><arguments>{}</arguments></tool_use>"#;
        let fake = Arc::new(
            FakeProvider::with_responses(vec![
                call.to_string(),
                call.to_string(),
                "Done.".to_string(),
            ])
            .server_side_history(),
        );
        let provider: Arc<dyn LLMProvider> = Arc::clone(&fake) as Arc<dyn LLMProvider>;
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(LoudTool));
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let cid = ConversationId::next();

        run_agent_loop(
            TurnSpec {
                conversation: cid,
                provider,
                // The same history rung 2 refuses to re-seed, over three steps.
                messages: overfull_history(&"u".repeat(45_000), &"o".repeat(30_000), "r"),
                system_prompt: "system".to_string(),
                model: "fake".to_string(),
                temperature: 0.0,
                max_tokens: 128,
                max_steps: 5,
                max_tools_per_step: 4,
                auto_approve: true,
                tool_output_limit: 0,
                context: crate::context::ContextSpec {
                    auto_compact: true,
                    context_window: 15_000,
                    provider_window: 0,
                    reserved_tokens: 3_000,
                    preserve_recent_tokens: 500,
                    spill_dir: dir.clone(),
                },
            },
            tools,
            Arc::new(tokio::sync::Mutex::new(MCPManager::new())),
            SemanticService::disabled(),
            event_tx,
        )
        .await;
        crate::debug_log::set_enabled(false).expect("the trace is closed again");

        assert!(
            fake.request(2).is_some(),
            "the turn has to run more than one step for this to mean anything"
        );
        assert_eq!(fake.resets(), 0, "the re-seed is refused, not performed");
        assert!(
            trace.exists(),
            "the trace went elsewhere: the sink was already configured"
        );
        // The id makes the count immune to whatever else logs in parallel.
        let needle = format!("conversation={cid} reason=still_over_usable");
        let refusals = std::fs::read_to_string(&trace)
            .expect("the trace is readable")
            .lines()
            .filter(|line| line.contains(&needle))
            .count();
        assert_eq!(refusals, 1, "one refusal per turn, not one per step");

        let _ = std::fs::remove_file(&trace);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Rung 2 is only a lever for providers whose history lives upstream. A
    /// provider that is sent the whole array every step is compacted by rungs 1
    /// and 3 instead, and its session is never reset.
    #[tokio::test]
    async fn a_local_history_provider_never_resets_the_session() {
        let dir = std::env::temp_dir().join(format!("pooprusteek-reset-{}", uuid::Uuid::new_v4()));
        let fake = Arc::new(FakeProvider::with_response("Done."));
        let provider: Arc<dyn LLMProvider> = Arc::clone(&fake) as Arc<dyn LLMProvider>;
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        run_agent_loop(
            TurnSpec {
                conversation: ConversationId::next(),
                provider,
                // The same 92%-full history that resets a server-side session.
                messages: overfull_history("go", &"o".repeat(33_000), &"r".repeat(300)),
                system_prompt: "system".to_string(),
                model: "fake".to_string(),
                temperature: 0.0,
                max_tokens: 128,
                max_steps: 1,
                max_tools_per_step: 4,
                auto_approve: true,
                tool_output_limit: 0,
                context: crate::context::ContextSpec {
                    auto_compact: true,
                    context_window: 15_000,
                    provider_window: 0,
                    reserved_tokens: 3_000,
                    preserve_recent_tokens: 500,
                    spill_dir: dir.clone(),
                },
            },
            Arc::new(ToolRegistry::new()),
            Arc::new(tokio::sync::Mutex::new(MCPManager::new())),
            SemanticService::disabled(),
            event_tx,
        )
        .await;

        assert_eq!(fake.resets(), 0, "rung 2 is not for this provider");
        while let Ok(ev) = event_rx.try_recv() {
            assert!(!matches!(
                ev,
                AppEvent::Agent {
                    event: AgentEvent::SessionReset { .. },
                    ..
                }
            ));
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One turn against a provider that reports its own session tally, with a
    /// `ContextSpec` that leaves `usable` at 12k.
    async fn turn_with_session_tokens(
        fake: &Arc<FakeProvider>,
        messages: Vec<ChatMessage>,
    ) -> (ConversationId, mpsc::UnboundedReceiver<AppEvent>) {
        let dir = std::env::temp_dir().join(format!("pooprusteek-tally-{}", uuid::Uuid::new_v4()));
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let cid = ConversationId::next();

        run_agent_loop(
            TurnSpec {
                conversation: cid,
                provider: Arc::clone(fake) as Arc<dyn LLMProvider>,
                messages,
                system_prompt: "system".to_string(),
                model: "fake".to_string(),
                temperature: 0.0,
                max_tokens: 128,
                max_steps: 1,
                max_tools_per_step: 4,
                auto_approve: true,
                tool_output_limit: 0,
                context: crate::context::ContextSpec {
                    auto_compact: true,
                    context_window: 15_000,
                    provider_window: 0,
                    reserved_tokens: 3_000,
                    preserve_recent_tokens: 500,
                    spill_dir: dir.clone(),
                },
            },
            Arc::new(ToolRegistry::new()),
            Arc::new(tokio::sync::Mutex::new(MCPManager::new())),
            SemanticService::disabled(),
            event_tx,
        )
        .await;

        let _ = std::fs::remove_dir_all(&dir);
        (cid, event_rx)
    }

    fn reported_usage(event_rx: &mut mpsc::UnboundedReceiver<AppEvent>) -> Option<u32> {
        let mut used_reported = None;
        while let Ok(ev) = event_rx.try_recv() {
            if let AppEvent::Agent {
                event: AgentEvent::ContextUsage(used),
                ..
            } = ev
            {
                used_reported = Some(used);
            }
        }
        used_reported
    }

    /// The live defect: after a session reset the server holds only the
    /// re-seed, but the local history is as long as ever. Measured locally,
    /// the meter reads full again on the very next turn and the session is
    /// reset every turn. A provider that reports its own tally is believed.
    #[tokio::test]
    async fn a_reported_session_tally_outranks_the_local_history() {
        let fake = Arc::new(
            FakeProvider::with_response("Done.")
                .server_side_history()
                // The freshly re-seeded session: 8% of the 12k usable window,
                // while the local history below is over 90% of it.
                .with_session_tokens(1_000),
        );

        let (_, mut event_rx) =
            turn_with_session_tokens(&fake, overfull_history("go", &"o".repeat(33_000), "r")).await;

        assert_eq!(
            fake.resets(),
            0,
            "the session the provider actually holds is nearly empty"
        );
        assert_eq!(
            reported_usage(&mut event_rx),
            Some(1_000),
            "the status bar is fed the same number the rungs judged"
        );
    }

    /// And the other direction: a short local history does not excuse a
    /// session the provider says is full.
    #[tokio::test]
    async fn a_full_reported_tally_resets_even_with_a_short_local_history() {
        let fake = Arc::new(
            FakeProvider::with_response("Done.")
                .server_side_history()
                // 95% of the 12k usable window, against ~2 tokens of history.
                .with_session_tokens(11_500),
        );

        let (cid, mut event_rx) =
            turn_with_session_tokens(&fake, vec![ChatMessage::user("go")]).await;

        assert_eq!(fake.resets(), 1, "rung 2 acted on the provider's number");
        let mut reset_event = None;
        while let Ok(ev) = event_rx.try_recv() {
            if let AppEvent::Agent {
                conversation,
                event: AgentEvent::SessionReset { before_tokens, .. },
            } = ev
            {
                assert_eq!(conversation, cid);
                reset_event = Some(before_tokens);
            }
        }
        assert_eq!(
            reset_event,
            Some(11_500),
            "what was reset is what the provider reported holding"
        );
    }
}
