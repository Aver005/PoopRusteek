//! Телеметрия шага. Один и тот же префикс писался руками 18 раз.
//! Инвариант 11: это фасад над `debug_log`, а не второй поток.

use crate::agent::tool_parser::ParsedToolCall;
use crate::app::conversation::ConversationId;
use crate::debug_log;
use serde_json::json;

/// Всё, что трасса пишет про один результат инструмента. Семь позиционных
/// аргументов из двух строк и трёх чисел путались местами.
#[derive(Clone, Copy)]
pub struct ToolResultTrace<'a> {
    pub name: &'a str,
    pub is_error: bool,
    pub result: &'a str,
    pub preview: &'a str,
    /// Модели ушёл урезанный вывод (ступень 0 ладдера).
    pub capped: bool,
    pub chars_sent: usize,
}

/// Обёртка над `debug_log` для одного шага хода.
pub struct StepTrace {
    conversation: ConversationId,
    step: usize,
    max_steps: usize,
    /// Общий префикс всех строк шага. Собран один раз: поля не меняются,
    /// а записей за шаг набирается около десятка.
    head: String,
}

impl StepTrace {
    pub fn new(conversation: ConversationId, step: usize, max_steps: usize) -> Self {
        Self {
            conversation,
            step,
            max_steps,
            head: format!("conversation={conversation} step={step}/{max_steps}"),
        }
    }

    fn head(&self) -> &str {
        &self.head
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

    /// Разбор шага: короткая строка и полная выкладка. Обе записи читает
    /// харнесс, поэтому имена и поля прежние — метод один, чтобы сырой ответ
    /// не пересчитывался дважды.
    pub fn parsed(
        &self,
        got_stop: bool,
        provider_error: Option<&str>,
        raw: &str,
        visible: &str,
        calls: &[ParsedToolCall],
    ) {
        let visible_chars = visible.chars().count();
        debug_log::log(
            "agent.step.parsed",
            format!(
                "{} got_stop={got_stop} visible_chars={visible_chars} tool_calls={}",
                self.head(),
                calls.len()
            ),
        );
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
                "visible_text_chars": visible_chars,
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

    /// Вызов инструмента: строка и выкладка с аргументами.
    pub fn tool_call(&self, index: usize, total: usize, name: &str, arguments: &serde_json::Value) {
        debug_log::log(
            "agent.tool.call",
            format!("{} index={index}/{total} name={name}", self.head()),
        );
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

    /// Результат инструмента: строка и полная выкладка. Некапнутый результат
    /// пишется целиком — трасса не должна врать о том, что инструмент выдал.
    pub fn tool_result(&self, result: &ToolResultTrace<'_>) {
        let ToolResultTrace {
            name,
            is_error,
            result: text,
            preview,
            capped,
            chars_sent,
        } = *result;
        let result_chars = text.chars().count();
        debug_log::log(
            "agent.tool.result",
            format!(
                "{} name={name} is_error={is_error} result_chars={result_chars} preview={preview}",
                self.head()
            ),
        );
        debug_log::log_json(
            "agent.tool.result.payload",
            &json!({
                "conversation": self.conversation.0,
                "step": self.step,
                "max_steps": self.max_steps,
                "tool_name": name,
                "is_error": is_error,
                "result_chars": result_chars,
                "result": text,
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
