//! Классификация отказов CLI. Образцы живут в таблицах: новый образец — одна строка.
//! Настоящих образцов исчерпанного лимита нет: неизвестный текст — `Server`, без потери текста.

use crate::provider::failure::{FailureClass, classify_http};
use std::time::SystemTime;

/// Во что превращается совпавший образец.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    Auth,
    RateLimit,
    Quota,
    SpendCap,
    Overloaded,
    Blocked,
    ContextTooLong,
    ModelNotFound,
}

impl Rule {
    pub fn class(self, resets_at: Option<SystemTime>) -> FailureClass {
        match self {
            Self::Auth => FailureClass::Auth,
            Self::RateLimit => FailureClass::RateLimit { retry_after: None },
            Self::Quota => FailureClass::QuotaExhausted { resets_at },
            Self::SpendCap => FailureClass::SpendCap,
            Self::Overloaded => FailureClass::Overloaded,
            Self::Blocked => FailureClass::AccountBlocked,
            Self::ContextTooLong => FailureClass::ContextTooLong,
            Self::ModelNotFound => FailureClass::ModelNotFound,
        }
    }
}

/// Подстрока (в нижнем регистре) → правило.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub needle: &'static str,
    pub rule: Rule,
}

impl Sample {
    pub const fn new(needle: &'static str, rule: Rule) -> Self {
        Self { needle, rule }
    }
}

/// Только то, что видели вживую (`06-cli-spike.md` §1.4): пустая папка входа.
pub const CLAUDE_SAMPLES: &[Sample] = &[Sample::new("not logged in", Rule::Auth)];

/// Только то, что видели вживую (§2.4): пустая папка данных даёт `provider.no-route`.
pub const OPENCODE_SAMPLES: &[Sample] = &[Sample::new("provider.no-route", Rule::Auth)];

/// Первое совпавшее правило.
pub fn match_sample(table: &[Sample], text: &str) -> Option<Rule> {
    let lower = text.to_lowercase();
    table
        .iter()
        .find(|sample| lower.contains(sample.needle))
        .map(|sample| sample.rule)
}

/// Сколько символов сообщения об ошибке несёт `BackendError`; полный текст идёт в журнал.
pub const MESSAGE_CAP: usize = 2000;

pub fn cap_message(text: &str) -> String {
    safe_text(text, MESSAGE_CAP)
}

/// Сырой вывод CLI для сообщения или журнала: сначала вычёркиваем секреты, потом режем по
/// границе символа (секрет, разрезанный раньше, не распознался бы).
pub fn safe_text(text: &str, cap: usize) -> String {
    let clean = crate::secrets::redact::redact(text.trim());
    crate::util::truncate_at_char_boundary(&clean, cap).to_string()
}

/// Класс отказа Claude по тексту `result`, `api_error_status` и отказу в `rate_limit_event`.
/// Порядок: образец, структурный отказ лимита, HTTP-статус, иначе `Server`.
pub fn classify_claude(
    text: &str,
    api_status: Option<u16>,
    rejected: Option<Option<SystemTime>>,
) -> (FailureClass, bool) {
    if let Some(rule) = match_sample(CLAUDE_SAMPLES, text) {
        return (rule.class(None), false);
    }
    if let Some(resets_at) = rejected {
        return (Rule::Quota.class(resets_at), false);
    }
    match api_status {
        Some(status) => (classify_http(status, None, "", text), false),
        None => (FailureClass::Server, true),
    }
}

/// Класс отказа OpenCode по событию `error` (тип и сообщение).
pub fn classify_opencode(kind: &str, message: &str) -> (FailureClass, bool) {
    match match_sample(OPENCODE_SAMPLES, &format!("{kind} {message}")) {
        Some(rule) => (rule.class(None), false),
        None => (FailureClass::Server, true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_verified_sample_gives_auth_for_both_cli() {
        assert_eq!(
            classify_claude("Not logged in · Please run /login", None, None).0,
            FailureClass::Auth
        );
        assert_eq!(
            classify_opencode("provider.no-route", "Model unavailable: x").0,
            FailureClass::Auth
        );
    }

    #[test]
    fn an_unknown_text_is_a_server_error_marked_unrecognized() {
        let (class, unrecognized) = classify_claude("something nobody has seen", None, None);
        assert_eq!(class, FailureClass::Server);
        assert!(unrecognized);
        assert!(classify_opencode("weird", "text").1);
    }

    #[test]
    fn a_rejected_limit_event_means_exhausted_with_the_reset_time() {
        let reset = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000);
        let (class, unrecognized) = classify_claude("anything", None, Some(Some(reset)));
        assert_eq!(
            class,
            FailureClass::QuotaExhausted {
                resets_at: Some(reset)
            }
        );
        assert!(!unrecognized);
    }

    #[test]
    fn an_http_status_from_the_result_goes_through_the_shared_classifier() {
        assert!(matches!(
            classify_claude("x", Some(429), None).0,
            FailureClass::RateLimit { .. }
        ));
        assert_eq!(
            classify_claude("x", Some(529), None).0,
            FailureClass::Overloaded
        );
        assert_eq!(classify_claude("x", Some(401), None).0, FailureClass::Auth);
    }

    #[test]
    fn a_new_sample_is_one_table_line() {
        const TABLE: &[Sample] = &[Sample::new("limit reached", Rule::Quota)];
        assert_eq!(
            match_sample(TABLE, "Usage LIMIT REACHED, try later"),
            Some(Rule::Quota)
        );
        assert_eq!(match_sample(TABLE, "fine"), None);
    }

    const LEAKY: &str =
        "boom sk-0123456789abcdef0123456789 and Bearer abcdefghijklmnopqrstuvwx1234 done";

    #[test]
    fn raw_cli_output_loses_secrets_before_it_reaches_a_message_or_the_log() {
        for shown in [cap_message(LEAKY), safe_text(LEAKY, 8000)] {
            assert!(shown.contains("boom") && shown.contains("done"), "{shown}");
            assert!(!shown.contains("0123456789abcdef"), "{shown}");
            assert!(!shown.contains("abcdefghijklmnopqrstuvwx"), "{shown}");
        }
    }

    #[test]
    fn a_secret_that_straddles_the_cut_is_redacted_first_and_ordinary_text_is_untouched() {
        // Ключ начинается до границы и кончился бы после неё: режем уже вычищенный текст.
        let text = format!("{} sk-0123456789abcdef0123456789", "a".repeat(30));
        let cut = safe_text(&text, 40);
        assert!(!cut.contains("sk-0123"), "{cut}");
        assert_eq!(safe_text("  plain text  ", 100), "plain text");
        let wide = safe_text(&"я".repeat(50), 11);
        assert!(wide.chars().all(|c| c == 'я') && wide.len() <= 11);
    }

    #[test]
    fn long_messages_are_cut_on_a_char_boundary() {
        let long = "я".repeat(MESSAGE_CAP);
        let cut = cap_message(&long);
        assert!(cut.len() <= MESSAGE_CAP && cut.chars().all(|c| c == 'я'));
    }
}
