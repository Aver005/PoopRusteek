//! Безголовый прогон суб-агента.
//!
//! Суб-агент крутит отдельный цикл шагов на **форкнутом провайдере** (своя
//! сессия, без истории родителя) и отдаёт только финальный текст —
//! промежуточных шагов родитель не видит. Инструменты идут без подтверждения
//! (смотреть некому), а `task` и `question` отклоняются: глубина ограничена
//! единицей, и блокировать ход вопросом здесь некому.

use crate::agent::retry::RetryBudget;
use crate::agent::runner::{EMPTY_RESPONSE_FEEDBACK, build_step_request, malformed_tool_feedback};
use crate::agent::stream::{StreamVerdict, collect_stream};
use crate::agent::tool_parser::{parse_tool_calls_with_errors, strip_tool_calls};
use crate::agent::tools_step::{dispatch_generic_tool, tool_skip_message};
use crate::mcp::MCPManager;
use crate::provider::{ChatMessage, LLMProvider, Role};
use crate::tools::registry::ToolRegistry;
use crate::tools::{QUESTION_TOOL_NAME, TASK_TOOL_NAME};
use std::sync::Arc;

/// Один запуск суб-агента. То же, что `TurnSpec` делает для обычного хода:
/// одиннадцать позиционных аргументов путались местами при вызове.
pub struct SubAgentSpec {
    pub provider: Arc<dyn LLMProvider>,
    pub tools: Arc<ToolRegistry>,
    pub mcp: Arc<tokio::sync::Mutex<MCPManager>>,
    pub system_prompt: String,
    pub user_prompt: String,
    pub model: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub max_steps: usize,
    pub max_tools_per_step: usize,
    /// Наследуется от заводящего хода: история суб-агента растёт так же,
    /// значит и ступень 0 у неё та же.
    pub tool_output_limit: usize,
}

pub async fn run_sub_agent(spec: SubAgentSpec) -> Result<String, String> {
    let SubAgentSpec {
        provider,
        tools,
        mcp,
        system_prompt,
        user_prompt,
        model,
        temperature,
        max_tokens,
        max_steps,
        max_tools_per_step,
        tool_output_limit,
    } = spec;
    let mut messages = vec![ChatMessage::user(&user_prompt)];
    let mut retries = RetryBudget::default();

    for _step in 0..max_steps {
        let request =
            build_step_request(&system_prompt, &messages, &model, temperature, max_tokens);

        // Безголово: колбэка прогресса нет, стримить некуда.
        let outcome = collect_stream(&provider, request, |_| {}).await;
        match outcome.verdict() {
            StreamVerdict::IdleTimeout => return Err("sub-agent stream timed out".to_string()),
            StreamVerdict::Failed(error) => return Err(error),
            // В отличие от главного цикла, обрыв без stop здесь не ошибка:
            // родителю важен только финальный текст.
            StreamVerdict::Ok | StreamVerdict::ClosedWithoutStop => {}
        }
        let full = outcome.text;

        let (tool_calls, parse_errors) = parse_tool_calls_with_errors(&full);
        let visible = strip_tool_calls(&full);

        if tool_calls.is_empty() {
            // Та же развилка, что и в главном цикле: ноль разобранных вызовов
            // при непустых ошибках — это сломанный `<tool_use>`, а не ответ.
            if !parse_errors.is_empty() {
                if retries.take_malformed().is_none() {
                    return Err(format!(
                        "sub-agent kept emitting malformed tool calls: {}",
                        parse_errors.join(" | ")
                    ));
                }
                messages.push(ChatMessage::assistant(&full));
                messages.push(ChatMessage::user(&malformed_tool_feedback(&parse_errors)));
                continue;
            }
            if !visible.is_empty() {
                return Ok(visible);
            }
            // Ни текста, ни вызова. Главный цикл ловит это с 2026-08-25
            // (харнесс нашёл: DeepSeek закрывает стрим со stop и нулём байт),
            // а суб-агент отдавал пустоту родителю как успешный ответ.
            if retries.take_empty().is_none() {
                return Err("sub-agent returned an empty reply and did nothing".to_string());
            }
            messages.push(ChatMessage::user(EMPTY_RESPONSE_FEEDBACK));
            continue;
        }

        messages.push(ChatMessage::assistant(&visible));
        let total_calls = tool_calls.len();
        for (call_index, tool_call) in tool_calls.into_iter().enumerate() {
            let tool_id = uuid::Uuid::new_v4().to_string();
            // Тот же контракт: пропущенный вызов получает явный tool_result.
            if call_index >= max_tools_per_step {
                messages.push(ChatMessage::tool(
                    &tool_id,
                    &tool_skip_message(max_tools_per_step, total_calls),
                ));
                continue;
            }
            let result = if tool_call.name == TASK_TOOL_NAME || tool_call.name == QUESTION_TOOL_NAME
            {
                format!("'{}' is not available inside a sub-agent.", tool_call.name)
            } else {
                // Общее с главным циклом (та же дисциплина короткого лока).
                // Суб-агент отдаёт ошибку обычным текстом, флаг не нужен.
                dispatch_generic_tool(&tools, &mcp, &tool_call.name, tool_call.arguments)
                    .await
                    .text
            };
            messages.push(ChatMessage::tool(
                &tool_id,
                &crate::context::cap_tool_output(&result, tool_output_limit),
            ));
        }
    }

    // Шаги кончились — отдаём лучшее, что есть: последний непустой текст
    // ассистента. Пустые здесь обычны: шаг с одним `<tool_use>` кладёт именно
    // такое сообщение, и `find` по роли выдавал бы родителю пустоту как ответ.
    messages
        .iter()
        .rev()
        .filter(|m| m.role == Role::Assistant)
        .map(|m| m.content.trim())
        .find(|text| !text.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "sub-agent ran out of steps without producing any text".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::fake::FakeProvider;

    const CALL: &str = "<tool_use>\n<name>nope</name>\n<arguments>\n{}\n</arguments>\n</tool_use>";

    /// Заготовка запуска: тест меняет в ней одно-два поля.
    fn spec() -> SubAgentSpec {
        SubAgentSpec {
            provider: Arc::new(FakeProvider::with_response("")),
            tools: Arc::new(ToolRegistry::new()),
            mcp: Arc::new(tokio::sync::Mutex::new(MCPManager::new())),
            system_prompt: "system".to_string(),
            user_prompt: "do the thing".to_string(),
            model: "fake".to_string(),
            temperature: 0.0,
            max_tokens: 128,
            max_steps: 4,
            max_tools_per_step: 4,
            tool_output_limit: 0,
        }
    }

    fn scripted(responses: Vec<String>) -> Arc<dyn LLMProvider> {
        Arc::new(FakeProvider::with_responses(responses))
    }

    /// Суб-агент работает безголово на своём провайдере и отдаёт только
    /// финальный текст — без событий и без сети.
    #[tokio::test]
    async fn returns_final_text() {
        let out = run_sub_agent(SubAgentSpec {
            provider: scripted(vec!["Done: 42".to_string()]),
            user_prompt: "compute the answer".to_string(),
            ..spec()
        })
        .await;
        assert_eq!(out.unwrap(), "Done: 42");
    }

    /// Сломанный `<tool_use>` отдают на переписывание — тот же контракт, что
    /// в главном цикле, — а не возвращают пустотой.
    #[tokio::test]
    async fn malformed_tool_call_is_retried_not_swallowed() {
        let out = run_sub_agent(SubAgentSpec {
            provider: scripted(vec![
                "<tool_use>\n<name>shell</name>\n<arguments>\n{ not json }\n</arguments>\n</tool_use>"
                    .to_string(),
                "Recovered".to_string(),
            ]),
            ..spec()
        })
        .await;
        assert_eq!(out.unwrap(), "Recovered");
    }

    /// Пустой ответ нельзя отдавать родителю как успешный: главный цикл ловит
    /// это с 2026-08-25, а суб-агент раньше возвращал пустую строку.
    #[tokio::test]
    async fn an_empty_reply_is_retried_then_reported() {
        let out = run_sub_agent(SubAgentSpec {
            provider: scripted(vec![String::new(); 3]),
            max_steps: 8,
            ..spec()
        })
        .await;
        assert!(out.is_err(), "пустой ответ не должен быть успехом: {out:?}");
    }

    /// Но пустой первый ответ ещё не приговор — после подсказки модель
    /// отвечает, и ход засчитывается.
    #[tokio::test]
    async fn an_empty_reply_recovers_when_the_model_answers() {
        let out = run_sub_agent(SubAgentSpec {
            provider: scripted(vec![String::new(), "Recovered".to_string()]),
            max_steps: 8,
            ..spec()
        })
        .await;
        assert_eq!(out.unwrap(), "Recovered");
    }

    /// Шаг с одним `<tool_use>` кладёт пустое ассистентское сообщение. На
    /// исчерпании шагов «последнее сообщение ассистента» — это оно, и родитель
    /// получал пустоту с `is_error = false`.
    #[tokio::test]
    async fn running_out_of_steps_with_no_text_is_an_error() {
        let out = run_sub_agent(SubAgentSpec {
            provider: scripted(vec![CALL.to_string(); 2]),
            max_steps: 2,
            ..spec()
        })
        .await;
        assert!(out.is_err(), "пустой хвост не должен быть успехом: {out:?}");
    }

    /// Но текст, сказанный по дороге, на исчерпании шагов не теряется.
    #[tokio::test]
    async fn running_out_of_steps_keeps_the_last_real_text() {
        let out = run_sub_agent(SubAgentSpec {
            provider: scripted(vec![
                format!("Смотрю, что тут есть.\n{CALL}"),
                CALL.to_string(),
            ]),
            max_steps: 2,
            ..spec()
        })
        .await;
        assert_eq!(out.unwrap(), "Смотрю, что тут есть.");
    }
}
