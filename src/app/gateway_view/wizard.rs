//! Мастер добавления аккаунта: тип → имя → адрес и модель → секрет.
//! Состояние и сборка `Account` — чистые; хранилище и шлюз трогает вызывающий.

use crate::app::input::InputState;
use crate::config::Config;
use crate::gateway::types::{
    Account, AccountId, AccountProvider, CliTool, DIRECT_TOKEN_WARNING, SecretRef,
};
use std::path::{Path, PathBuf};

/// Скрытое поле: значение не попадает ни в `Debug`, ни в поле ввода, ни в историю.
#[derive(Default, Clone)]
pub struct HiddenInput {
    text: String,
}

impl HiddenInput {
    pub fn push(&mut self, ch: char) {
        if !ch.is_control() {
            self.text.push(ch);
        }
    }

    pub fn push_str(&mut self, text: &str) {
        self.text.extend(text.chars().filter(|c| !c.is_control()));
    }

    pub fn pop(&mut self) {
        self.text.pop();
    }

    pub fn len(&self) -> usize {
        self.text.chars().count()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Забрать значение как секрет; поле очищается.
    pub fn take_secret(&mut self) -> crate::secrets::Secret {
        let secret = crate::secrets::Secret::new(self.text.trim());
        self.clear();
        secret
    }

    pub fn clear(&mut self) {
        // Перезапись до очистки: буфер не должен хранить значение после выхода.
        let len = self.text.len();
        self.text.replace_range(.., &"\0".repeat(len));
        self.text.clear();
    }

    /// Маска нужной длины для показа.
    pub fn mask(&self) -> String {
        crate::tui::glyphs::mask_text(self.len())
    }
}

impl std::fmt::Debug for HiddenInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HiddenInput(<{} chars>)", self.len())
    }
}

impl Drop for HiddenInput {
    fn drop(&mut self) {
        self.clear();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Api,
    ClaudeCode,
    OpenCode,
    DirectToken,
}

/// Порядок пунктов первого шага.
pub const KINDS: [Kind; 4] = [
    Kind::Api,
    Kind::ClaudeCode,
    Kind::OpenCode,
    Kind::DirectToken,
];

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Api => "API key (OpenAI-compatible, Anthropic, Gemini, local server)",
            Self::ClaudeCode => "Claude Code subscription (official CLI)",
            Self::OpenCode => "OpenCode subscription (official CLI)",
            Self::DirectToken => "Subscription token (not Claude; terms-of-use risk)",
        }
    }

    fn cli_tool(self) -> Option<CliTool> {
        match self {
            Self::ClaudeCode => Some(CliTool::Claude),
            Self::OpenCode => Some(CliTool::Opencode),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Kind,
    Label,
    Protocol,
    BaseUrl,
    Model,
    Login,
    Consent,
    Secret,
    Confirm,
}

impl Step {
    pub fn title(self) -> &'static str {
        match self {
            Self::Kind => "What are you connecting?",
            Self::Label => "Name (you will see it everywhere)",
            Self::Protocol => "Protocol",
            Self::BaseUrl => "Address (usually ends with /v1)",
            Self::Model => "Default model (blank = pick later)",
            Self::Login => "Login",
            Self::Consent => "Direct token warning",
            Self::Secret => "Secret (hidden; blank = none)",
            Self::Confirm => "Confirm",
        }
    }
}

/// Протоколы API-аккаунта.
pub const PROTOCOLS: [(AccountProvider, &str); 3] = [
    (
        AccountProvider::Openai,
        "OpenAI Chat Completions (LM Studio, Ollama /v1, vLLM, xAI, OpenRouter)",
    ),
    (AccountProvider::Anthropic, "Anthropic Messages API"),
    (AccountProvider::Gemini, "Google Gemini"),
];

pub const LOGIN_OWN: &str = "Own login folder (recommended): this account never shares a login";
pub const LOGIN_SHARED: &str = "Shared login: use the login the CLI already has; signing out there, or a parallel session, may log it out";

#[derive(Debug, Clone)]
pub struct AccountWizard {
    pub step: Step,
    pub kind: usize,
    pub label: InputState,
    pub protocol: usize,
    pub base_url: InputState,
    pub model: InputState,
    /// 0 — своя папка входа, 1 — общий вход.
    pub login: usize,
    /// Выбор на шаге согласия: 0 — отмена, 1 — продолжить.
    pub consent_choice: usize,
    pub secret: HiddenInput,
    pub error: Option<String>,
}

/// Что мастеру нужно знать о существующих аккаунтах.
pub struct Existing<'a> {
    pub ids: &'a [AccountId],
    pub labels: &'a [String],
    /// Корень папок входа: `~/.pooprusteek/accounts`.
    pub home_root: &'a Path,
}

/// Готовый к записи результат мастера.
#[derive(Debug, Clone)]
pub struct NewAccount {
    pub account: Account,
    /// Имя записи в хранилище; значение лежит отдельно (`take_secret`).
    pub secret_id: Option<String>,
    /// Что показать после добавления: команда входа в CLI.
    pub login_command: Option<String>,
    pub warnings: Vec<String>,
}

pub fn login_home_root() -> PathBuf {
    Config::global_path()
        .parent()
        .map_or_else(|| PathBuf::from("accounts"), |dir| dir.join("accounts"))
}

impl Default for AccountWizard {
    fn default() -> Self {
        Self::new()
    }
}

impl AccountWizard {
    pub fn new() -> Self {
        Self {
            step: Step::Kind,
            kind: 0,
            label: InputState::default(),
            protocol: 0,
            base_url: InputState::default(),
            model: InputState::default(),
            login: 0,
            consent_choice: 0,
            secret: HiddenInput::default(),
            error: None,
        }
    }

    /// Быстрая форма `/providers add <имя> <адрес> [модель] [ключ]`: поля заполнены, шаг подтверждения.
    pub fn from_quick(entry: &crate::config::ProviderEntry) -> Self {
        let mut wizard = Self::new();
        wizard.label.insert_str(&entry.name);
        wizard.protocol = PROTOCOLS
            .iter()
            .position(|(provider, _)| provider.protocol() == Some(entry.protocol))
            .unwrap_or(0);
        wizard.base_url.insert_str(&entry.base_url);
        wizard.model.insert_str(&entry.model);
        if let Some(key) = &entry.api_key {
            wizard.secret.push_str(key);
        }
        wizard.step = Step::Confirm;
        wizard
    }

    pub fn kind(&self) -> Kind {
        KINDS[self.kind.min(KINDS.len() - 1)]
    }

    /// Шаги по порядку для выбранного типа.
    pub fn steps(&self) -> Vec<Step> {
        match self.kind() {
            Kind::Api => vec![
                Step::Kind,
                Step::Label,
                Step::Protocol,
                Step::BaseUrl,
                Step::Model,
                Step::Secret,
                Step::Confirm,
            ],
            Kind::ClaudeCode => vec![Step::Kind, Step::Label, Step::Login, Step::Confirm],
            Kind::OpenCode => vec![
                Step::Kind,
                Step::Label,
                Step::Login,
                Step::Model,
                Step::Confirm,
            ],
            Kind::DirectToken => vec![
                Step::Kind,
                Step::Label,
                Step::BaseUrl,
                Step::Model,
                Step::Consent,
                Step::Secret,
                Step::Confirm,
            ],
        }
    }

    /// Номер шага для заголовка: «Step 2/5».
    pub fn position(&self) -> (usize, usize) {
        let steps = self.steps();
        let at = steps.iter().position(|s| *s == self.step).unwrap_or(0);
        (at + 1, steps.len())
    }

    fn neighbour(&self, forward: bool) -> Option<Step> {
        let steps = self.steps();
        let at = steps.iter().position(|s| *s == self.step)?;
        if forward {
            steps.get(at + 1).copied()
        } else {
            at.checked_sub(1).map(|i| steps[i])
        }
    }

    /// Назад на шаг; `false` — это был первый, мастер закрывается.
    pub fn back(&mut self) -> bool {
        self.error = None;
        match self.neighbour(false) {
            Some(step) => {
                self.step = step;
                true
            }
            None => false,
        }
    }

    /// Проверить текущий шаг и перейти к следующему. На последнем шаге `Ok(true)`.
    pub fn advance(&mut self, existing: &Existing<'_>) -> Result<bool, String> {
        self.error = None;
        let outcome = self.validate_step(existing);
        if let Err(message) = &outcome {
            self.error = Some(message.clone());
            return outcome.map(|_| false);
        }
        match self.neighbour(true) {
            Some(step) => {
                self.step = step;
                Ok(false)
            }
            None => Ok(true),
        }
    }

    fn validate_step(&self, existing: &Existing<'_>) -> Result<(), String> {
        match self.step {
            Step::Label => {
                let label = self.label.buffer.trim();
                if label.is_empty() {
                    return Err("the name can't be empty".into());
                }
                if existing
                    .labels
                    .iter()
                    .any(|l| l.eq_ignore_ascii_case(label))
                {
                    return Err(format!("'{label}' is already used by another account"));
                }
                Ok(())
            }
            Step::BaseUrl => crate::app::providers::validate_base_url(self.base_url.buffer.trim())
                .map(|_| ()),
            Step::Consent if self.consent_choice != 1 => Err(
                "direct tokens are off unless you explicitly accept the warning (choose the second option)"
                    .into(),
            ),
            _ => Ok(()),
        }
    }

    /// Стабильный id из имени: `a-z`, цифры, `-`; при совпадении `-2`, `-3`.
    pub fn make_id(label: &str, taken: &[AccountId], fallback: &str) -> AccountId {
        let mut slug = String::new();
        for ch in label.trim().chars() {
            if ch.is_ascii_alphanumeric() {
                slug.push(ch.to_ascii_lowercase());
            } else if !slug.ends_with('-') && !slug.is_empty() {
                slug.push('-');
            }
        }
        let slug = slug.trim_matches('-').to_string();
        let base = if slug.is_empty() || !slug.starts_with(|c: char| c.is_ascii_lowercase()) {
            format!("{fallback}-{slug}")
                .trim_end_matches('-')
                .to_string()
        } else {
            slug
        };
        let free = |candidate: &str| !taken.iter().any(|t| t.as_str() == candidate);
        if free(&base) {
            return AccountId::parse(&base).expect("slug is a valid id");
        }
        (2..)
            .map(|n| format!("{base}-{n}"))
            .find(|c| free(c))
            .and_then(|c| AccountId::parse(&c).ok())
            .expect("a free id exists")
    }

    /// Собрать аккаунт. Секрет остаётся в мастере: его забирает `take_secret`.
    pub fn build(&self, existing: &Existing<'_>) -> Result<NewAccount, String> {
        let label = self.label.buffer.trim().to_string();
        if label.is_empty() {
            return Err("the name can't be empty".into());
        }
        let kind = self.kind();
        let fallback = match kind {
            Kind::Api => "api",
            Kind::ClaudeCode => "claude",
            Kind::OpenCode => "opencode",
            Kind::DirectToken => "token",
        };
        let id = Self::make_id(&label, existing.ids, fallback);
        let model = self.model.buffer.trim();
        let mut warnings = Vec::new();
        let mut login_command = None;
        let mut secret_id = None;
        let provider = match kind {
            Kind::Api => PROTOCOLS[self.protocol.min(PROTOCOLS.len() - 1)].0,
            Kind::ClaudeCode => AccountProvider::CliClaude,
            Kind::OpenCode => AccountProvider::CliOpencode,
            Kind::DirectToken => AccountProvider::Custom,
        };
        let mut account = Account::new(id.clone(), provider);
        account.label = Some(label);
        account.model = (!model.is_empty()).then(|| model.to_string());
        if matches!(kind, Kind::Api | Kind::DirectToken) {
            account.base_url = Some(crate::app::providers::validate_base_url(
                self.base_url.buffer.trim(),
            )?);
            if !self.secret.is_empty() {
                let entry = crate::secrets::account_secret_id(id.as_str());
                let reference = SecretRef::parse(&crate::secrets::keyring_ref(&entry))?;
                if kind == Kind::DirectToken {
                    account.direct_token = Some(reference);
                    account.direct_token_ack = self.consent_choice == 1;
                } else {
                    account.secret = Some(reference);
                }
                secret_id = Some(entry);
            } else if kind == Kind::Api {
                account.secret = Some(SecretRef::parse("none")?);
            }
            if kind == Kind::DirectToken {
                if self.consent_choice != 1 {
                    return Err("accept the direct token warning first".into());
                }
                if secret_id.is_none() {
                    return Err("a direct token needs the token".into());
                }
                warnings.push(DIRECT_TOKEN_WARNING.to_string());
            }
        }
        if let Some(tool) = kind.cli_tool() {
            if self.login == 0 {
                let home = existing.home_root.join(id.as_str());
                login_command = Some(login_command_for(tool, &home));
                account.home = Some(home);
            } else {
                warnings.push(LOGIN_SHARED.to_string());
            }
        }
        Ok(NewAccount {
            account,
            secret_id,
            login_command,
            warnings,
        })
    }
}

/// Команда входа в CLI в своей папке. Показываем её человеку; сами токены не читаем.
pub fn login_command_for(tool: CliTool, home: &Path) -> String {
    let (var, command) = match tool {
        CliTool::Claude => ("CLAUDE_CONFIG_DIR", "claude auth login"),
        CliTool::Opencode => ("XDG_DATA_HOME", "opencode auth login"),
        _ => ("CODEX_HOME", "codex login"),
    };
    let home = home.display();
    if cfg!(windows) {
        format!("$env:{var} = '{home}'; {command}")
    } else {
        format!("{var}='{home}' {command}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn existing<'a>(ids: &'a [AccountId], labels: &'a [String], root: &'a Path) -> Existing<'a> {
        Existing {
            ids,
            labels,
            home_root: root,
        }
    }

    fn type_into(input: &mut InputState, text: &str) {
        input.insert_str(text);
    }

    fn claude(label: &str, login: usize) -> AccountWizard {
        let mut wizard = AccountWizard::new();
        wizard.kind = 1;
        type_into(&mut wizard.label, label);
        wizard.login = login;
        wizard
    }

    #[test]
    fn ids_come_from_the_name_and_stay_unique() {
        let taken = vec![AccountId::parse("claude-max").unwrap()];
        assert_eq!(
            AccountWizard::make_id("Claude Max", &taken, "claude").as_str(),
            "claude-max-2"
        );
        assert_eq!(
            AccountWizard::make_id("Рабочий", &[], "claude").as_str(),
            "claude",
            "a name with no Latin letters falls back to the kind"
        );
        assert!(AccountWizard::make_id("  !!  ", &[], "api").is_slug());
    }

    #[test]
    fn two_subscriptions_of_one_kind_get_their_own_login_folders() {
        let root = PathBuf::from("/accounts");
        let first = claude("claude-max", 0)
            .build(&existing(&[], &[], &root))
            .unwrap();
        let ids = vec![first.account.id.clone()];
        let labels = vec!["claude-max".to_string()];
        let second = claude("claude-work", 0)
            .build(&existing(&ids, &labels, &root))
            .unwrap();
        assert_ne!(first.account.home, second.account.home);
        assert_eq!(first.account.provider, AccountProvider::CliClaude);
        assert!(
            first
                .login_command
                .as_deref()
                .unwrap()
                .contains("claude auth login")
        );
        assert!(
            second
                .login_command
                .as_deref()
                .unwrap()
                .contains("claude-work")
        );
        // Вместе они проходят проверку конфига без ошибок.
        let config = crate::gateway::types::GatewayConfig {
            accounts: vec![first.account, second.account],
            ..Default::default()
        };
        let errors: Vec<_> = config
            .validate()
            .into_iter()
            .filter(|p| p.severity == crate::gateway::types::Severity::Error)
            .collect();
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn a_shared_login_is_allowed_but_warned_about() {
        let built = claude("shared", 1)
            .build(&existing(&[], &[], Path::new("/a")))
            .unwrap();
        assert!(built.account.home.is_none());
        assert!(
            built.warnings[0].contains("log it out"),
            "{:?}",
            built.warnings
        );
    }

    #[test]
    fn a_duplicate_name_stops_the_label_step() {
        let labels = vec!["Claude".to_string()];
        let mut wizard = claude("claude", 0);
        wizard.step = Step::Label;
        let error = wizard
            .advance(&existing(&[], &labels, Path::new("/a")))
            .unwrap_err();
        assert!(error.contains("already used"), "{error}");
        assert_eq!(wizard.step, Step::Label, "the wizard stays where it was");
    }

    #[test]
    fn an_api_account_keeps_its_key_out_of_the_account_and_in_the_keyring() {
        let mut wizard = AccountWizard::new();
        type_into(&mut wizard.label, "Local LM");
        type_into(&mut wizard.base_url, "http://localhost:1234/v1/");
        type_into(&mut wizard.model, "qwen");
        wizard.secret.push_str("sk-test-key-123456");
        let built = wizard.build(&existing(&[], &[], Path::new("/a"))).unwrap();
        assert_eq!(
            built.account.base_url.as_deref(),
            Some("http://localhost:1234/v1")
        );
        let reference = built.account.secret.as_ref().unwrap().as_str().to_string();
        assert!(reference.starts_with("keyring:"), "{reference}");
        assert!(built.secret_id.is_some());
        assert!(
            built.account.api_key.is_none(),
            "no plain key in the config"
        );
        let printed = format!("{built:?} {wizard:?}");
        assert!(!printed.contains("sk-test-key"), "{printed}");
    }

    #[test]
    fn without_a_key_an_api_account_says_none_explicitly() {
        let mut wizard = AccountWizard::new();
        type_into(&mut wizard.label, "Ollama");
        type_into(&mut wizard.base_url, "http://localhost:11434/v1");
        type_into(&mut wizard.model, "llama3");
        let built = wizard.build(&existing(&[], &[], Path::new("/a"))).unwrap();
        assert!(built.account.secret.as_ref().unwrap().is_none());
        assert!(built.secret_id.is_none());
    }

    #[test]
    fn a_direct_token_needs_the_warning_accepted_and_the_token() {
        let mut wizard = AccountWizard::new();
        wizard.kind = 3;
        type_into(&mut wizard.label, "Codex token");
        type_into(&mut wizard.base_url, "https://example.test/v1");
        type_into(&mut wizard.model, "gpt");
        wizard.secret.push_str("tok-abcdefghijkl");
        let denied = wizard.build(&existing(&[], &[], Path::new("/a")));
        assert!(denied.is_err(), "no consent, no account");
        wizard.consent_choice = 1;
        let built = wizard.build(&existing(&[], &[], Path::new("/a"))).unwrap();
        assert!(built.account.direct_token_ack);
        assert!(built.account.direct_token.is_some());
        assert_eq!(built.warnings, vec![DIRECT_TOKEN_WARNING.to_string()]);
        let config = crate::gateway::types::GatewayConfig {
            accounts: vec![built.account],
            ..Default::default()
        };
        assert!(
            config
                .validate()
                .iter()
                .all(|p| p.severity != crate::gateway::types::Severity::Error)
        );
    }

    #[test]
    fn the_consent_step_defaults_to_no() {
        let mut wizard = AccountWizard::new();
        wizard.kind = 3;
        wizard.step = Step::Consent;
        let error = wizard
            .advance(&existing(&[], &[], Path::new("/a")))
            .unwrap_err();
        assert!(error.contains("explicitly"), "{error}");
        wizard.consent_choice = 1;
        assert_eq!(
            wizard.advance(&existing(&[], &[], Path::new("/a"))),
            Ok(false)
        );
        assert_eq!(wizard.step, Step::Secret);
    }

    #[test]
    fn there_is_no_direct_token_path_for_claude() {
        // Пункты мастера: прямой токен — отдельный тип, и он собирается как Custom, не Claude.
        let mut wizard = AccountWizard::new();
        wizard.kind = 3;
        type_into(&mut wizard.label, "x");
        type_into(&mut wizard.base_url, "https://api.anthropic.com/v1");
        wizard.consent_choice = 1;
        wizard.secret.push_str("tok-abcdefghijkl");
        let built = wizard.build(&existing(&[], &[], Path::new("/a"))).unwrap();
        let config = crate::gateway::types::GatewayConfig {
            accounts: vec![built.account],
            ..Default::default()
        };
        assert!(
            config
                .validate()
                .iter()
                .any(|p| p.severity == crate::gateway::types::Severity::Error
                    && p.message.contains("not allowed for Claude")),
            "a Claude address with a direct token is refused by validate()"
        );
    }

    #[test]
    fn the_hidden_field_never_prints_its_value() {
        let mut field = HiddenInput::default();
        field.push_str("secret-value");
        assert_eq!(format!("{field:?}"), "HiddenInput(<12 chars>)");
        assert!(!field.mask().contains("secret"));
        let taken = field.take_secret();
        assert!(!taken.is_empty());
        assert!(field.is_empty());
    }
}
