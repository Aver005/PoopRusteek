//! Небольшие формы экрана аккаунтов: скрытый ввод секрета, подтверждение проектного
//! слоя, ограничения и избранные модели. Состояние чистое, клавиши — в `keys/accounts.rs`.

use super::wizard::HiddenInput;
use crate::app::input::InputState;
use crate::config::TrustRequest;
use crate::gateway::types::{Account, AccountId, AccountLimits};

// ── Скрытый ввод секрета ────────────────────────────────────────────────

/// Модалка с маской. Значение живёт только здесь: в поле ввода, истории, очереди,
/// журнале и снимках его нет.
#[derive(Debug, Clone)]
pub struct SecretPrompt {
    /// Имя записи: `/secrets set <name>`.
    pub name: String,
    pub value: HiddenInput,
    pub error: Option<String>,
}

impl SecretPrompt {
    pub fn named(name: &str) -> Self {
        Self {
            name: name.to_string(),
            value: HiddenInput::default(),
            error: None,
        }
    }

    pub fn title(&self) -> String {
        format!(" Secret: {} ", self.name)
    }
}

// ── Подтверждение проектного слоя ───────────────────────────────────────

/// Что выбрано в диалоге. По умолчанию — отказ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrustChoice {
    #[default]
    Deny,
    Trust,
}

#[derive(Debug, Clone)]
pub struct TrustDialog {
    pub request: TrustRequest,
    pub choice: TrustChoice,
}

impl TrustDialog {
    pub fn new(request: TrustRequest) -> Self {
        Self {
            request,
            choice: TrustChoice::Deny,
        }
    }

    pub fn toggle(&mut self) {
        self.choice = match self.choice {
            TrustChoice::Deny => TrustChoice::Trust,
            TrustChoice::Trust => TrustChoice::Deny,
        };
    }

    /// Только явный выбор «Trust» подтверждает; Enter на отказе оставляет заблокированным.
    pub fn confirmed(&self) -> bool {
        self.choice == TrustChoice::Trust
    }
}

// ── Ограничения и избранное ─────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitField {
    SoftPct,
    HardPct,
    RequestsPerMinute,
    MaxConcurrency,
    BudgetUsdMonth,
    Favorites,
}

pub const LIMIT_FIELDS: [LimitField; 6] = [
    LimitField::SoftPct,
    LimitField::HardPct,
    LimitField::RequestsPerMinute,
    LimitField::MaxConcurrency,
    LimitField::BudgetUsdMonth,
    LimitField::Favorites,
];

impl LimitField {
    pub fn label(self) -> &'static str {
        match self {
            Self::SoftPct => "Move down the list at, % of the window",
            Self::HardPct => "Never use above, % of the window",
            Self::RequestsPerMinute => "Requests per minute",
            Self::MaxConcurrency => "Calls at once",
            Self::BudgetUsdMonth => "Monthly budget, USD",
            Self::Favorites => "Favorite models (comma separated)",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LimitsForm {
    pub account: AccountId,
    pub name: String,
    pub focus: usize,
    pub fields: Vec<InputState>,
    pub error: Option<String>,
}

fn text_of<T: ToString>(value: Option<T>) -> InputState {
    let mut input = InputState::default();
    if let Some(value) = value {
        input.insert_str(&value.to_string());
    }
    input
}

impl LimitsForm {
    pub fn for_account(account: &Account) -> Self {
        let limits = &account.limits;
        Self {
            account: account.id.clone(),
            name: account.display_name().to_string(),
            focus: 0,
            fields: vec![
                text_of(limits.soft_pct),
                text_of(limits.hard_pct),
                text_of(limits.requests_per_minute),
                text_of(limits.max_concurrency),
                text_of(limits.budget_usd_month),
                text_of(Some(account.favorites.join(", ")).filter(|s| !s.is_empty())),
            ],
            error: None,
        }
    }

    pub fn current(&mut self) -> &mut InputState {
        let at = self.focus.min(self.fields.len() - 1);
        &mut self.fields[at]
    }

    pub fn step(&mut self, forward: bool) {
        let len = self.fields.len();
        self.focus = if forward {
            (self.focus + 1) % len
        } else {
            (self.focus + len - 1) % len
        };
    }

    fn number<T: std::str::FromStr>(&self, field: LimitField) -> Result<Option<T>, String> {
        let index = LIMIT_FIELDS.iter().position(|f| *f == field).unwrap_or(0);
        let raw = self.fields[index].buffer.trim();
        if raw.is_empty() {
            return Ok(None);
        }
        raw.parse::<T>()
            .map(Some)
            .map_err(|_| format!("'{raw}' is not a valid number for \"{}\"", field.label()))
    }

    /// Применить поля к копии аккаунта. Проверку границ делает `validate()` шлюза.
    pub fn apply_to(&self, account: &Account) -> Result<Account, String> {
        let mut next = account.clone();
        next.limits = AccountLimits {
            soft_pct: self.number(LimitField::SoftPct)?,
            hard_pct: self.number(LimitField::HardPct)?,
            requests_per_minute: self.number(LimitField::RequestsPerMinute)?,
            max_concurrency: self.number(LimitField::MaxConcurrency)?,
            budget_usd_month: self.number(LimitField::BudgetUsdMonth)?,
        };
        let favorites = &self.fields[LIMIT_FIELDS.len() - 1].buffer;
        next.favorites = favorites
            .split([',', '\n'])
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string)
            .collect();
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::types::AccountProvider;

    fn account() -> Account {
        let mut a = Account::new(AccountId::parse("a").unwrap(), AccountProvider::Openai);
        a.favorites = vec!["m1".into()];
        a.limits.soft_pct = Some(70);
        a
    }

    #[test]
    fn the_form_starts_from_the_current_values() {
        let form = LimitsForm::for_account(&account());
        assert_eq!(form.fields[0].buffer, "70");
        assert_eq!(form.fields[1].buffer, "");
        assert_eq!(form.fields[5].buffer, "m1");
    }

    #[test]
    fn edited_fields_become_limits_and_favorites() {
        let mut form = LimitsForm::for_account(&account());
        form.fields[1].insert_str("90");
        form.fields[3].insert_str("2");
        form.fields[5].buffer.clear();
        form.fields[5].cursor = 0;
        form.fields[5].insert_str("opus-5-5, sonnet-5-5 ,,");
        let next = form.apply_to(&account()).unwrap();
        assert_eq!(next.limits.hard_pct, Some(90));
        assert_eq!(next.limits.max_concurrency, Some(2));
        assert_eq!(next.favorites, vec!["opus-5-5", "sonnet-5-5"]);
    }

    #[test]
    fn a_bad_number_is_refused_with_the_field_name() {
        let mut form = LimitsForm::for_account(&account());
        form.fields[2].insert_str("fast");
        let error = form.apply_to(&account()).unwrap_err();
        assert!(error.contains("Requests per minute"), "{error}");
    }

    #[test]
    fn the_trust_dialog_refuses_by_default() {
        let request = TrustRequest {
            config_path: "x".into(),
            key: "k".into(),
            hash: "h".into(),
            held_back: vec!["account evil (https://evil.test)".into()],
        };
        let mut dialog = TrustDialog::new(request);
        assert!(
            !dialog.confirmed(),
            "Enter on a fresh dialog must not trust"
        );
        dialog.toggle();
        assert!(dialog.confirmed());
        dialog.toggle();
        assert!(!dialog.confirmed());
    }

    #[test]
    fn the_secret_prompt_never_prints_the_value() {
        let mut prompt = SecretPrompt::named("openai");
        prompt.value.push_str("sk-very-secret");
        assert!(!format!("{prompt:?}").contains("sk-very"));
        assert!(prompt.title().contains("openai"));
    }
}
