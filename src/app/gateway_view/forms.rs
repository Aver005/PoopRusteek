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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustChoice {
    Deny,
    Trust,
}

/// Вид строки документа диалога.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustLineKind {
    /// Сводка: хосты и число записей. Идёт первой и показывается целиком.
    Summary,
    Heading,
    Item,
    Plain,
    Blank,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustLine {
    pub kind: TrustLineKind,
    pub text: String,
}

/// Размеры окна диалога: одни и те же для кадра и для клавиш, чтобы «прочитано до конца»
/// значило то, что человек видел на экране.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustView {
    pub popup_w: u16,
    /// Ширина текста внутри окна.
    pub text_w: usize,
    /// Сколько строк документа помещается над кнопками.
    pub max_room: usize,
}

/// Служебные строки окна: рамка, разделитель, кнопки, подсказка.
const TRUST_CHROME: usize = 6;

pub fn trust_view(cols: u16, rows: u16) -> TrustView {
    let (cols, rows) = if cols == 0 || rows == 0 {
        (80, 24)
    } else {
        (cols, rows)
    };
    let popup_w = cols.clamp(48, 92);
    TrustView {
        popup_w,
        text_w: usize::from(popup_w).saturating_sub(8).max(20),
        // Ровно столько строк окно показывает на деле; на очень низком терминале их нет,
        // и тогда прочитать список нельзя.
        max_room: usize::from(rows.saturating_sub(2)).saturating_sub(TRUST_CHROME),
    }
}

#[derive(Debug, Clone)]
pub struct TrustDialog {
    pub request: TrustRequest,
    pub choice: TrustChoice,
    /// Первая видимая строка документа.
    pub offset: usize,
    /// Какие строки документа хоть раз были на экране. Порядок обхода не важен: `End`, потом
    /// `Home` и постраничное чтение дают то же, что чтение сверху вниз.
    seen: Vec<bool>,
    /// Экран запущен из инструмента агента: согласие недоступно.
    pub restricted: bool,
    /// Почему «Trust» ещё недоступен.
    pub notice: Option<String>,
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

impl TrustDialog {
    pub fn new(request: TrustRequest) -> Self {
        Self {
            request,
            choice: TrustChoice::Deny,
            offset: 0,
            seen: Vec::new(),
            restricted: false,
            notice: None,
        }
    }

    /// Документ диалога: сводка (хосты и число записей) вперёд, потом полный список.
    pub fn lines(&self, text_w: usize) -> Vec<TrustLine> {
        let mut out = Vec::new();
        let summary = &self.request.summary;
        let counts = format!(
            "{}, {}, {}",
            plural(summary.accounts, "account", "accounts"),
            plural(summary.routes, "route", "routes"),
            plural(summary.roles, "role", "roles"),
        );
        // Счёт идёт вперёд хостов: длинный список хостов переносится, а число остаётся целым.
        let head = if summary.hosts.is_empty() {
            format!("Proposed: {counts}. No new request address.")
        } else {
            format!(
                "Proposed: {counts}. Requests would go to: {}.",
                summary.hosts.join(", ")
            )
        };
        push_wrapped(&mut out, text_w, TrustLineKind::Summary, &head, "  ", "");
        out.push(blank());
        push_wrapped(
            &mut out,
            text_w,
            TrustLineKind::Plain,
            "Nothing is applied until you trust this file. If it changes, you are asked again.",
            "  ",
            "",
        );
        push_wrapped(
            &mut out,
            text_w,
            TrustLineKind::Plain,
            &format!("File: {}", self.request.config_path.display()),
            "  ",
            "",
        );
        out.push(blank());
        push_wrapped(
            &mut out,
            text_w,
            TrustLineKind::Heading,
            &format!(
                "It would add or change ({}):",
                plural(self.request.held_back.len(), "item", "items")
            ),
            "  ",
            "",
        );
        for item in &self.request.held_back {
            push_wrapped(
                &mut out,
                text_w,
                TrustLineKind::Item,
                item,
                "  ",
                "\u{2022}",
            );
        }
        out
    }

    /// Строк документа на экране сейчас.
    fn room(&self, view: &TrustView) -> (usize, usize) {
        let total = self.lines(view.text_w).len();
        (total, total.min(view.max_room))
    }

    /// Учесть показанное окно: вызывается на каждой клавише, до разбора самой клавиши.
    pub fn reveal(&mut self, view: &TrustView) {
        let (total, room) = self.room(view);
        self.offset = self.offset.min(total.saturating_sub(room));
        // Другая ширина — другой перенос строк: прежние отметки к новому документу не относятся.
        if self.seen.len() != total {
            self.seen = vec![false; total];
        }
        for line in self.seen.iter_mut().skip(self.offset).take(room) {
            *line = true;
        }
    }

    pub fn scroll(&mut self, delta: isize, view: &TrustView) {
        let (total, room) = self.room(view);
        let last = total.saturating_sub(room);
        self.offset = self.offset.saturating_add_signed(delta).min(last);
        self.reveal(view);
    }

    pub fn scroll_to(&mut self, end: bool, view: &TrustView) {
        let (total, room) = self.room(view);
        self.offset = if end { total.saturating_sub(room) } else { 0 };
        self.reveal(view);
    }

    /// Каждая строка документа была на экране; окно, что видно сейчас, в счёт.
    pub fn fully_read(&self, view: &TrustView) -> bool {
        let (total, room) = self.room(view);
        if room == 0 {
            return false;
        }
        let offset = self.offset.min(total.saturating_sub(room));
        (0..total).all(|line| {
            (offset..offset + room).contains(&line) || (self.seen.len() == total && self.seen[line])
        })
    }

    /// «Trust» выбирается только после того, как весь список был на экране.
    pub fn toggle(&mut self, view: &TrustView) {
        match self.choice {
            TrustChoice::Trust => {
                self.choice = TrustChoice::Deny;
                self.notice = None;
            }
            TrustChoice::Deny if self.restricted => {
                self.notice = Some(crate::app::gateway_view::HUMAN_ONLY.into());
            }
            TrustChoice::Deny if self.fully_read(view) => {
                self.choice = TrustChoice::Trust;
                self.notice = None;
            }
            TrustChoice::Deny => {
                self.notice = Some(
                    "Show every line first: scroll with Up/Down and PageUp/PageDown, in any order."
                        .into(),
                );
            }
        }
    }

    /// Только явный выбор «Trust» подтверждает; Enter на отказе оставляет заблокированным.
    pub fn confirmed(&self) -> bool {
        self.choice == TrustChoice::Trust
    }
}

fn push_wrapped(
    out: &mut Vec<TrustLine>,
    text_w: usize,
    kind: TrustLineKind,
    text: &str,
    indent: &str,
    bullet: &str,
) {
    for (n, row) in crate::tui::wrap::wrap_words(text, text_w.saturating_sub(4))
        .into_iter()
        .enumerate()
    {
        let mark = match (bullet.is_empty(), n) {
            (true, _) => "",
            (false, 0) => bullet,
            (false, _) => " ",
        };
        let gap = if bullet.is_empty() { "" } else { " " };
        out.push(TrustLine {
            kind,
            text: format!("{indent}{mark}{gap}{row}"),
        });
    }
}

fn blank() -> TrustLine {
    TrustLine {
        kind: TrustLineKind::Blank,
        text: String::new(),
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

    fn trust_request(accounts: &[(&str, &str)]) -> TrustRequest {
        let held_back: Vec<String> = accounts
            .iter()
            .map(|(id, host)| format!("account '{id}' sends requests to https://{host}/v1"))
            .collect();
        let mut hosts: Vec<String> = Vec::new();
        for (_, host) in accounts {
            if !hosts.iter().any(|h| h == host) {
                hosts.push((*host).to_string());
            }
        }
        let mut request = TrustRequest {
            config_path: "x/.pooprusteek/config.toml".into(),
            key: "k".into(),
            hash: "h".into(),
            held_back,
            summary: Default::default(),
        };
        request.summary.hosts = hosts;
        request.summary.accounts = accounts.len();
        request
    }

    const SMALL: TrustView = TrustView {
        popup_w: 80,
        text_w: 72,
        max_room: 16,
    };

    #[test]
    fn the_trust_dialog_refuses_by_default() {
        let mut dialog =
            TrustDialog::new(trust_request(&[("a", "one.example"), ("b", "two.example")]));
        assert!(
            !dialog.confirmed(),
            "Enter on a fresh dialog must not trust"
        );
        // Короткий файл целиком на экране: «Trust» доступен сразу.
        dialog.reveal(&SMALL);
        assert!(dialog.fully_read(&SMALL));
        dialog.toggle(&SMALL);
        assert!(dialog.confirmed());
        dialog.toggle(&SMALL);
        assert!(!dialog.confirmed());
    }

    #[test]
    fn the_summary_comes_first_and_names_every_host_and_the_count() {
        let mut entries: Vec<(String, String)> = (0..29)
            .map(|n| (format!("filler-{n}"), format!("host{n}.example")))
            .collect();
        entries.push(("evil".into(), "attacker.example".into()));
        let borrowed: Vec<(&str, &str)> = entries
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let dialog = TrustDialog::new(trust_request(&borrowed));
        let lines = dialog.lines(SMALL.text_w);
        let head: String = lines
            .iter()
            .take_while(|l| l.kind == TrustLineKind::Summary)
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(head.contains("attacker.example"), "{head}");
        assert!(
            head.contains("host0.example") && head.contains("host28.example"),
            "{head}"
        );
        assert!(head.contains("30 accounts"), "{head}");
        assert_eq!(
            lines[0].kind,
            TrustLineKind::Summary,
            "the summary is the first thing"
        );
    }

    #[test]
    fn a_long_list_must_be_read_to_the_end_before_trust_opens() {
        let entries: Vec<(String, String)> = (0..40)
            .map(|n| (format!("a{n}"), format!("h{n}.example")))
            .collect();
        let borrowed: Vec<(&str, &str)> = entries
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let mut dialog = TrustDialog::new(trust_request(&borrowed));
        dialog.reveal(&SMALL);
        assert!(!dialog.fully_read(&SMALL), "40 items do not fit");
        dialog.toggle(&SMALL);
        assert!(!dialog.confirmed(), "refused before the list was read");
        assert!(dialog.notice.is_some(), "and the reason is said");

        // Прыжок в конец пропускает середину и доверия не открывает.
        dialog.scroll_to(true, &SMALL);
        assert!(!dialog.fully_read(&SMALL), "a jump skips the middle");
        dialog.toggle(&SMALL);
        assert!(!dialog.confirmed());

        // Постраничное чтение подряд открывает.
        dialog.scroll_to(false, &SMALL);
        for _ in 0..10 {
            dialog.scroll(15, &SMALL);
        }
        assert!(dialog.fully_read(&SMALL));
        dialog.toggle(&SMALL);
        assert!(dialog.confirmed());
        // Назад на отказ можно всегда.
        dialog.toggle(&SMALL);
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

#[cfg(test)]
mod trust_view_tests {
    use super::*;

    fn request(items: usize) -> TrustRequest {
        TrustRequest {
            config_path: "x".into(),
            key: "k".into(),
            hash: "h".into(),
            held_back: (0..items).map(|n| format!("item number {n}")).collect(),
            summary: Default::default(),
        }
    }

    /// `End` не ловушка: после прыжка список дочитывается в любом порядке.
    #[test]
    fn after_a_jump_to_the_end_the_rest_can_still_be_read_in_any_order() {
        let view = trust_view(80, 24);
        let mut dialog = TrustDialog::new(request(60));
        dialog.reveal(&view);
        dialog.scroll_to(true, &view);
        assert!(!dialog.fully_read(&view), "the middle was skipped");
        dialog.toggle(&view);
        assert!(!dialog.confirmed());
        // Назад страницами к началу: всё, что мелькнуло по пути, засчитывается.
        for _ in 0..12 {
            dialog.scroll(-(view.max_room as isize - 1), &view);
        }
        assert!(
            dialog.fully_read(&view),
            "every line has been on screen by now"
        );
        dialog.toggle(&view);
        assert!(dialog.confirmed(), "the consent button is reachable");
        // Отказ работает всегда.
        dialog.toggle(&view);
        assert!(!dialog.confirmed());
    }

    /// Окно, в котором строк нет, ничего «прочитанным» не делает.
    #[test]
    fn a_window_too_low_to_show_the_list_cannot_be_read() {
        let tiny = trust_view(80, 8);
        assert_eq!(tiny.max_room, 0);
        let mut dialog = TrustDialog::new(request(1));
        dialog.reveal(&tiny);
        assert!(!dialog.fully_read(&tiny));
        dialog.toggle(&tiny);
        assert!(!dialog.confirmed());
    }
}
