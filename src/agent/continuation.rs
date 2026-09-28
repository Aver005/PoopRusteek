//! Продолжение ответа, оборванного лимитом вывода посреди вызова инструмента.
//! Переписывать большой `write` целиком бессмысленно — он упрётся в тот же
//! лимит; модель просят дописать с места обрыва, куски склеиваются.

use crate::agent::stream::{StreamVerdict, collect_stream};
use crate::provider::{ChatMessage, CompletionRequest, LLMProvider};
use std::sync::Arc;

/// Сколько раз подряд дозапрашивать один ответ.
pub const MAX_CONTINUATIONS: u32 = 3;

const CONTINUE_PROMPT: &str = "Your previous reply was cut off by the output limit in the \
middle of a tool call. Continue EXACTLY from the character where it stopped. Output only the \
remaining characters: no preamble, no apology, no repetition of what was already written, \
and do not reopen the tool call or a code fence.";

/// Повтор хвоста короче этого считается совпадением, а не эхом.
const MIN_OVERLAP_CHARS: usize = 16;
/// Дальше эхо не ищется: модель повторяет строку-другую, не страницу.
const MAX_OVERLAP_CHARS: usize = 512;
/// Кусок начат тем же маркером, что оборванный вызов, — модель начала заново.
const RESTART_OPENER_CHARS: usize = 8;

/// Итог дозапросов.
pub struct Continued {
    pub text: String,
    /// Сколько кусков дозапрошено.
    pub attempts: u32,
    /// Почему остановились с оборванным ответом, если так вышло.
    pub failure: Option<String>,
}

/// Дозапрашивать, пока `cut_at` видит обрыв (начало оборванного вызова).
/// `base` — запрос, на который пришёл `text`; `on_attempt` зовётся перед
/// каждым дозапросом.
pub async fn continue_cut_off(
    provider: &Arc<dyn LLMProvider>,
    base: &CompletionRequest,
    mut text: String,
    cut_at: impl Fn(&str) -> Option<usize>,
    mut on_attempt: impl FnMut(u32),
) -> Continued {
    let mut attempts = 0;
    let mut failure = None;
    while let Some(cut) = cut_at(&text) {
        if attempts == MAX_CONTINUATIONS {
            failure = Some(format!(
                "still cut off after {MAX_CONTINUATIONS} continuations"
            ));
            break;
        }
        attempts += 1;
        on_attempt(attempts);
        let outcome = collect_stream(provider, next_slice_request(base, &text), |_| {}).await;
        match outcome.verdict() {
            StreamVerdict::IdleTimeout | StreamVerdict::Failed(_) => {
                failure = Some(format!(
                    "continuation request failed: {:?}",
                    outcome.verdict()
                ));
                break;
            }
            _ if outcome.text.trim().is_empty() => {
                failure = Some("the model sent an empty continuation".to_string());
                break;
            }
            _ => {}
        }
        text = join(&text, cut, &outcome.text, &cut_at);
    }
    Continued {
        text,
        attempts,
        failure,
    }
}

/// Запрос на следующий кусок: та же история, оборванный ответ и просьба.
fn next_slice_request(base: &CompletionRequest, partial: &str) -> CompletionRequest {
    let mut request = base.clone();
    request.messages.push(ChatMessage::assistant(partial));
    request.messages.push(ChatMessage::user(CONTINUE_PROMPT));
    request
}

/// Склеить кусок с ответом. Модель, начавшая вызов заново, получает его на
/// месте оборванного: так вызов цел, а не записан дважды.
fn join(text: &str, cut: usize, next: &str, cut_at: &impl Fn(&str) -> Option<usize>) -> String {
    let fresh = next.trim_start();
    let opener: String = text[cut..].chars().take(RESTART_OPENER_CHARS).collect();
    if fresh.starts_with(&opener) {
        let restarted = format!("{}{fresh}", &text[..cut]);
        if cut_at(&restarted).is_none() {
            return restarted;
        }
    }
    stitch(text, next)
}

/// Приклеить `next` к `partial`, срезав повтор хвоста `partial` в начале `next`.
fn stitch(partial: &str, next: &str) -> String {
    let overlap = next
        .char_indices()
        .map(|(index, ch)| index + ch.len_utf8())
        .take(MAX_OVERLAP_CHARS)
        .skip(MIN_OVERLAP_CHARS - 1)
        .filter(|&end| partial.ends_with(&next[..end]))
        .last()
        .unwrap_or(0);
    format!("{partial}{}", &next[overlap..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool_parser::parse_reply;
    use crate::provider::fake::FakeProvider;

    fn cut_at(text: &str) -> Option<usize> {
        parse_reply(text).cut_at
    }

    fn base() -> CompletionRequest {
        CompletionRequest {
            messages: vec![ChatMessage::user("write it")],
            tools: Vec::new(),
            model: "m".to_string(),
            temperature: 0.0,
            max_tokens: 10,
            stream: true,
        }
    }

    const HEAD: &str =
        "<tool_use><name>write</name><arguments>{\"path\": \"a.rs\", \"content\": \"fn main() {";

    #[test]
    fn stitch_drops_an_echoed_tail_but_keeps_short_coincidences() {
        let partial = "0123456789abcdefghijklmnop";
        assert_eq!(stitch(partial, "abcdefghijklmnopQ"), format!("{partial}Q"));
        // Короткое совпадение — это не эхо.
        assert_eq!(stitch("x = {}", "{}}"), "x = {}{}}");
        // Многобайтные символы не режутся посередине.
        let partial = "ё".repeat(20);
        let next = format!("{}!", "ё".repeat(17));
        assert_eq!(stitch(&partial, &next), format!("{partial}!"));
    }

    #[test]
    fn a_restarted_call_replaces_the_cut_one() {
        let text = format!("Writing.\n{HEAD}");
        let cut = cut_at(&text).unwrap();
        let next = "<tool_use><name>write</name><arguments>{\"path\": \"a.rs\", \"content\": \"x\"}</arguments></tool_use>";
        let joined = join(&text, cut, next, &cut_at);
        assert_eq!(joined, format!("Writing.\n{next}"));
        assert_eq!(parse_reply(&joined).calls.len(), 1);
    }

    #[tokio::test]
    async fn slices_are_joined_until_the_call_closes() {
        let provider: Arc<dyn LLMProvider> = Arc::new(FakeProvider::with_responses(vec![
            " println!(\\\"hi\\\");".to_string(),
            " }\"}</arguments></tool_use>".to_string(),
        ]));
        let mut seen = Vec::new();
        let done = continue_cut_off(&provider, &base(), HEAD.to_string(), cut_at, |n| {
            seen.push(n)
        })
        .await;
        assert_eq!(seen, [1, 2]);
        assert!(done.failure.is_none());
        let calls = parse_reply(&done.text).calls;
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].arguments["content"],
            "fn main() { println!(\"hi\"); }"
        );
    }

    #[tokio::test]
    async fn gives_up_after_the_limit() {
        let provider: Arc<dyn LLMProvider> =
            Arc::new(FakeProvider::with_responses(vec![" more".to_string(); 5]));
        let done = continue_cut_off(&provider, &base(), HEAD.to_string(), cut_at, |_| {}).await;
        assert_eq!(done.attempts, MAX_CONTINUATIONS);
        assert!(done.failure.is_some());
    }

    #[tokio::test]
    async fn a_whole_reply_is_left_alone() {
        let provider: Arc<dyn LLMProvider> = Arc::new(FakeProvider::with_response("unused"));
        let done =
            continue_cut_off(&provider, &base(), "Plain answer.".into(), cut_at, |_| {}).await;
        assert_eq!(done.attempts, 0);
        assert_eq!(done.text, "Plain answer.");
    }
}
