//! Телеметрия шага. Один и тот же префикс писался руками 18 раз.
//! Инвариант 11: это фасад над `debug_log`, а не второй поток.

use crate::agent::tool_parser::ParsedToolCall;
use crate::app::conversation::ConversationId;
use crate::debug_log;
use serde_json::json;

/// Обёртка над `debug_log` для одного шага хода.
pub struct StepTrace {
    conversation: ConversationId,
    step: usize,
    max_steps: usize,
}

impl StepTrace {
    pub fn new(conversation: ConversationId, step: usize, max_steps: usize) -> Self {
        Self {
            conversation,
            step,
            max_steps,
        }
    }

    /// Общий префикс всех строк шага.
    fn head(&self) -> String {
        format!(
            "conversation={} step={}/{}",
            self.conversation, self.step, self.max_steps
        )
    }

    pub fn start(&self, message_count: usize) {
        debug_log::log(
            "agent.step.start",
            format!("{} message_count={message_count}", self.head()),
        );
    }

    /// Причина, по которой шаг не удался.
    pub fn error(&self, reason: &str) {
        debug_log::log("agent.step.error", format!("{} {reason}", self.head()));
    }

    pub fn stream_closed(&self, bytes: usize) {
        debug_log::log(
            "agent.step.stream_closed",
            format!("{} got_stop=false collected_bytes={bytes}", self.head()),
        );
    }

    /// Обрыв без stop-сигнала. Строка одна и та же в трассе и в ошибке
    /// наверх, поэтому метод её возвращает, а не дублирует вызывающий.
    pub fn closed_without_stop(&self, bytes: usize) -> String {
        let message = format!(
            "Stream ended without stop signal. {} response_bytes={bytes}",
            self.head()
        );
        debug_log::log("agent.step.error", &message);
        message
    }

    pub fn provider_ok(&self, bytes: usize) {
        debug_log::log(
            "agent.step.provider_ok",
            format!("{} got_stop=true response_bytes={bytes}", self.head()),
        );
    }

    pub fn parsed(&self, got_stop: bool, visible: &str, calls: &[ParsedToolCall]) {
        debug_log::log(
            "agent.step.parsed",
            format!(
                "{} got_stop={got_stop} visible_chars={} tool_calls={}",
                self.head(),
                visible.chars().count(),
                calls.len()
            ),
        );
    }

    /// Полная выкладка шага — это и есть трасса харнесса, поэтому здесь
    /// лежит сырой ответ, а не только его размер.
    pub fn parsed_payload(
        &self,
        got_stop: bool,
        provider_error: Option<&str>,
        raw: &str,
        visible: &str,
        calls: &[ParsedToolCall],
    ) {
        debug_log::log_json(
            "agent.step.parsed.payload",
            &json!({
                "conversation": self.conversation.0,
                "step": self.step,
                "max_steps": self.max_steps,
                "got_stop": got_stop,
                "provider_error": provider_error,
                "full_response_chars": raw.chars().count(),
                "full_response": raw,
                "visible_text_chars": visible.chars().count(),
                "visible_text": visible,
                "tool_calls": calls
                    .iter()
                    .map(|call| json!({ "name": call.name, "arguments": call.arguments }))
                    .collect::<Vec<_>>(),
            }),
        );
    }

    pub fn salvaged(&self, error: &str, calls: usize) {
        debug_log::log(
            "agent.step.salvaged",
            format!(
                "{} reason=provider_error_with_complete_tool_call error={error} tool_calls={calls}",
                self.head()
            ),
        );
    }

    pub fn malformed(&self, attempt: u32, max: u32, errors: &[String]) {
        debug_log::log(
            "agent.step.malformed_tool_use",
            format!(
                "{} retry={attempt}/{max} errors={}",
                self.head(),
                errors.join(" | ")
            ),
        );
    }

    pub fn malformed_exhausted(&self, errors: &[String]) {
        debug_log::log(
            "agent.step.malformed_tool_use_exhausted",
            format!("{} errors={}", self.head(), errors.join(" | ")),
        );
    }

    pub fn empty_assistant(&self) {
        debug_log::log(
            "agent.step.empty_assistant",
            format!("{} reason=empty_response_without_tool_calls", self.head()),
        );
    }

    pub fn empty_retry(&self, attempt: u32, max: u32) {
        debug_log::log(
            "agent.step.empty_retry",
            format!("{} retry={attempt}/{max}", self.head()),
        );
    }

    pub fn tool_call(&self, index: usize, total: usize, name: &str) {
        debug_log::log(
            "agent.tool.call",
            format!("{} index={index}/{total} name={name}", self.head()),
        );
    }

    pub fn tool_call_payload(
        &self,
        index: usize,
        total: usize,
        name: &str,
        arguments: &serde_json::Value,
    ) {
        debug_log::log_json(
            "agent.tool.call.payload",
            &json!({
                "conversation": self.conversation.0,
                "step": self.step,
                "max_steps": self.max_steps,
                "tool_name": name,
                "call_index": index,
                "total_calls": total,
                "arguments": arguments,
            }),
        );
    }

    pub fn tool_skipped(&self, name: &str) {
        debug_log::log(
            "agent.tool.skipped",
            format!("{} name={name} reason=max_tools_per_step", self.head()),
        );
    }

    pub fn tool_result(&self, name: &str, is_error: bool, result: &str, preview: &str) {
        debug_log::log(
            "agent.tool.result",
            format!(
                "{} name={name} is_error={is_error} result_chars={} preview={preview}",
                self.head(),
                result.chars().count()
            ),
        );
    }

    /// Некапнутый результат тоже пишем: трасса не должна врать о том, что
    /// инструмент на самом деле выдал.
    pub fn tool_result_payload(
        &self,
        name: &str,
        is_error: bool,
        result: &str,
        preview: &str,
        capped: bool,
        chars_sent: usize,
    ) {
        debug_log::log_json(
            "agent.tool.result.payload",
            &json!({
                "conversation": self.conversation.0,
                "step": self.step,
                "max_steps": self.max_steps,
                "tool_name": name,
                "is_error": is_error,
                "result_chars": result.chars().count(),
                "result": result,
                "preview": preview,
                "result_capped": capped,
                "result_chars_sent": chars_sent,
            }),
        );
    }

    pub fn turn_done(&self, visible: &str, total_calls: usize) {
        debug_log::log(
            "agent.turn.done",
            format!(
                "{} status=success text_chars={} tool_calls_total={total_calls}",
                self.head(),
                visible.chars().count()
            ),
        );
    }

    pub fn turn_error(&self, status: &str) {
        debug_log::log("agent.turn.error", format!("{} {status}", self.head()));
    }
}

/// Ход исчерпал шаги — вне шага, поэтому своя функция без префикса шага.
pub fn turn_out_of_steps(conversation: ConversationId, max_steps: usize, total_calls: usize) {
    debug_log::log(
        "agent.turn.error",
        format!(
            "conversation={conversation} status=max_steps_exceeded max_steps={max_steps} tool_calls_total={total_calls}"
        ),
    );
}
