//! `pooprusteek doctor` и `/doctor` — что откуда взялось и что не так. Без значений секретов.

use crate::config::{self, Config, LayerStatus};
use crate::gateway::types::Severity;
use crate::secrets::{SecretResolver, SecretStore, ops, process_store};
use clap::Args;
use serde_json::json;

#[derive(Args, Debug)]
pub struct DoctorArgs {
    /// Machine-readable output for CI.
    #[arg(long)]
    pub json: bool,
    /// Also list where every configured value came from.
    #[arg(long)]
    pub origins: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    fn mark(self) -> &'static str {
        match self {
            Self::Info => "  ",
            Self::Warn => "! ",
            Self::Error => "x ",
        }
    }

    fn word(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warning",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub level: Level,
    pub area: &'static str,
    pub text: String,
}

#[derive(Debug, Default)]
pub struct Report {
    pub layers: Vec<String>,
    pub secrets: Vec<ops::Row>,
    pub findings: Vec<Finding>,
    pub origins: Vec<(String, &'static str)>,
}

impl Report {
    fn add(&mut self, level: Level, area: &'static str, text: impl Into<String>) {
        self.findings.push(Finding {
            level,
            area,
            text: text.into(),
        });
    }

    pub fn worst(&self) -> Level {
        self.findings
            .iter()
            .map(|f| f.level)
            .max()
            .unwrap_or(Level::Info)
    }
}

fn layer_line(info: &config::LayerInfo) -> String {
    let status = match &info.status {
        LayerStatus::Applied => "applied".to_string(),
        LayerStatus::Missing => "not found".to_string(),
        LayerStatus::Rejected(reasons) => format!("IGNORED: {}", reasons.join("; ")),
        LayerStatus::Unreadable(why) => format!("skipped: {why}"),
    };
    format!(
        "{:<9} {}  [{status}]",
        info.layer.label(),
        info.path.display()
    )
}

fn http_to_public_host(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "http" {
        return false;
    }
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    let private = host == "localhost"
        || host.ends_with(".local")
        || host.starts_with("127.")
        || host.starts_with("10.")
        || host.starts_with("192.168.")
        || host == "[::1]"
        || host.split('.').next() == Some("172");
    !private
}

pub fn build(config: &Config, store: &SecretStore) -> Report {
    let mut report = Report {
        layers: config.source.layers.iter().map(layer_line).collect(),
        ..Report::default()
    };
    if config.source.layers.is_empty() {
        report
            .layers
            .push("(no config file loaded: built-in defaults)".to_string());
    }
    report.origins = config
        .source
        .origins
        .iter()
        .map(|(key, layer)| (key.clone(), layer.label()))
        .collect();

    if config::is_isolated() {
        report.add(
            Level::Info,
            "isolation",
            "this run uses --config/--data-dir: the system keyring is not touched, and nothing is migrated",
        );
    }
    for layer in &config.source.layers {
        if let LayerStatus::Rejected(reasons) = &layer.status {
            report.add(
                Level::Error,
                "project config",
                format!(
                    "{} was not applied: {}",
                    layer.path.display(),
                    reasons.join("; ")
                ),
            );
        }
    }

    // Хранилище.
    if config::is_isolated() {
        // выше уже сказано
    } else {
        match store.probe() {
            Ok(()) => report.add(Level::Info, "keyring", format!("{} works (write, read back, delete)", store.backend_name())),
            Err(error) => report.add(
                Level::Warn,
                "keyring",
                format!("the system keyring is not available: {error}. Secrets stay in the config file (mode 0600 on unix) and move later"),
            ),
        }
    }

    // Секреты.
    report.secrets = ops::list(store);
    for row in report.secrets.clone() {
        if row.state.starts_with("missing") || row.state.starts_with("invalid") {
            report.add(
                Level::Error,
                "secrets",
                format!("{} -> {}: {}", row.label, row.reference, row.state),
            );
        } else if row.reference.starts_with("(plain text") {
            report.add(
                Level::Warn,
                "secrets",
                format!("{} is stored in plain text in the config file", row.label),
            );
        } else if row.reference.starts_with("env:") {
            report.add(
                Level::Info,
                "secrets",
                format!(
                    "{} uses {}: environment variables are inherited by child processes",
                    row.label, row.reference
                ),
            );
        }
    }
    for item in &config.source.unresolved {
        report.add(
            Level::Error,
            "secrets",
            format!(
                "{} ({}) could not be read: {}",
                item.label, item.reference, item.reason
            ),
        );
    }

    // Аккаунты из проекта: ссылки и хосты, ждущие подтверждения.
    let resolver = config.secret_resolver();
    for account in &config.accounts {
        let Some(reference) = &account.secret else {
            continue;
        };
        if reference.is_none() {
            continue;
        }
        let from_project = config.source.project_accounts.contains(account.id.as_str());
        match resolver.resolve(reference, &account.id) {
            Ok(secret) => {
                if from_project {
                    report.secrets.push(ops::Row {
                        label: format!("accounts[{}].secret (project)", account.id),
                        reference: reference.as_str().to_string(),
                        state: format!("ok (…{})", secret.fingerprint()),
                    });
                }
            }
            Err(error @ crate::secrets::SecretError::HostNotTrusted { .. }) => report.add(
                Level::Warn,
                "trust",
                format!(
                    "account '{}' comes from the project config and wants {}: {error}",
                    account.id,
                    reference.as_str()
                ),
            ),
            Err(error) => report.add(
                if from_project {
                    Level::Warn
                } else {
                    Level::Error
                },
                "secrets",
                format!("accounts[{}].secret: {error}", account.id),
            ),
        }
    }

    // Шлюз.
    for problem in config.gateway_config().validate() {
        report.add(
            match problem.severity {
                Severity::Error => Level::Error,
                Severity::Warning => Level::Warn,
            },
            "gateway",
            format!("{}: {}", problem.path, problem.message),
        );
    }
    let mirrored: Vec<&str> = config.accounts.iter().map(|a| a.id.as_str()).collect();
    let drift: Vec<&str> = config
        .providers
        .iter()
        .map(|p| p.name.as_str())
        .filter(|name| !mirrored.contains(name))
        .collect();
    if !config.accounts.is_empty() && !drift.is_empty() {
        report.add(
            Level::Warn,
            "gateway",
            format!(
                "providers not mirrored in [[accounts]]: {}",
                drift.join(", ")
            ),
        );
    }

    // Файлы и логи.
    let path = Config::path();
    #[cfg(unix)]
    if let Ok(meta) = std::fs::metadata(&path) {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            report.add(
                Level::Warn,
                "files",
                format!(
                    "{} is readable by other users; run chmod 600",
                    path.display()
                ),
            );
        }
    }
    let backup = {
        let mut name = path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        name.push(".pre-gateway");
        path.with_file_name(name)
    };
    if let Ok(text) = crate::util::read_text(&backup)
        && config::fields::contains_inline_secrets(&text)
    {
        report.add(
            Level::Warn,
            "files",
            format!(
                "{} still holds the old keys in plain text; it is cleaned after a start where every reference resolves",
                backup.display()
            ),
        );
    }
    // Настоящие папки изолированный прогон не трогает, даже чтением метаданных.
    if !config::is_isolated()
        && path.is_file()
        && let Some(legacy) = config::legacy_config_file().filter(|l| l.is_file() && *l != path)
    {
        report.add(
            Level::Info,
            "files",
            format!(
                "old config {} is no longer read; delete it when you are sure",
                legacy.display()
            ),
        );
    }
    if crate::debug_log::is_enabled() {
        report.add(
            Level::Warn,
            "logs",
            "the debug log is on: keys are masked, but prompts and replies are written in full",
        );
    }
    for entry in &config.providers {
        if http_to_public_host(&entry.base_url) {
            report.add(
                Level::Warn,
                "network",
                format!(
                    "provider '{}' sends its key over plain http to a public host",
                    entry.name
                ),
            );
        }
    }
    for account in &config.accounts {
        if account.base_url.as_deref().is_some_and(http_to_public_host) {
            report.add(
                Level::Warn,
                "network",
                format!(
                    "account '{}' sends its key over plain http to a public host",
                    account.id
                ),
            );
        }
    }
    if config.server.api_key.as_deref().is_none_or(str::is_empty)
        && config.server.host != "127.0.0.1"
        && config.server.host != "localhost"
    {
        report.add(
            Level::Warn,
            "network",
            "the API server listens beyond loopback without an api_key",
        );
    }

    for notice in &config.migration_notices {
        report.add(Level::Info, "startup", notice.clone());
    }
    report.add(
        Level::Info,
        "limits",
        "this keeps keys out of files, logs and commits; it cannot stop an agent that runs shell commands as your user",
    );
    report
}

pub fn render_text(report: &Report, with_origins: bool) -> String {
    let mut out = String::new();
    out.push_str("Config layers (later wins):\n");
    for line in &report.layers {
        out.push_str(&format!("  {line}\n"));
    }
    out.push_str("\nSecrets (names only):\n");
    if report.secrets.is_empty() {
        out.push_str("  none referenced\n");
    }
    for row in &report.secrets {
        out.push_str(&format!("  {}\n", row.line()));
    }
    out.push_str("\nFindings:\n");
    for finding in &report.findings {
        out.push_str(&format!(
            "  {}[{}] {}\n",
            finding.level.mark(),
            finding.area,
            finding.text
        ));
    }
    if with_origins {
        out.push_str("\nWhere values came from:\n");
        for (key, layer) in &report.origins {
            out.push_str(&format!("  {key:<44} {layer}\n"));
        }
    } else if !report.origins.is_empty() {
        let mut counts = std::collections::BTreeMap::new();
        for (_, layer) in &report.origins {
            *counts.entry(*layer).or_insert(0usize) += 1;
        }
        let summary: Vec<String> = counts
            .iter()
            .map(|(l, n)| format!("{n} from {l}"))
            .collect();
        out.push_str(&format!(
            "\nValues set in files: {} (run with --origins for the list)\n",
            summary.join(", ")
        ));
    }
    out
}

pub fn to_json(report: &Report) -> serde_json::Value {
    json!({
        "layers": report.layers,
        "secrets": report.secrets.iter().map(|r| json!({
            "label": r.label, "reference": r.reference, "state": r.state,
        })).collect::<Vec<_>>(),
        "findings": report.findings.iter().map(|f| json!({
            "level": f.level.word(), "area": f.area, "text": f.text,
        })).collect::<Vec<_>>(),
        "origins": report.origins.iter().map(|(k, l)| json!({"key": k, "layer": l})).collect::<Vec<_>>(),
        "worst": report.worst().word(),
    })
}

/// Отчёт для TUI: блокирующий, зовётся из `spawn_blocking`.
pub fn text_for_tui(config: &Config) -> String {
    render_text(&build(config, &process_store()), false)
}

/// Код выхода: 0 — всё в порядке или есть замечания, 1 — есть ошибки.
pub fn run(args: DoctorArgs, explicit_config: Option<&std::path::Path>) -> i32 {
    let config = match config::load_from_optional(explicit_config) {
        Ok(config) => config,
        Err(error) => {
            if args.json {
                println!(
                    "{}",
                    json!({"worst": "error", "findings": [{"level": "error", "area": "config", "text": error.to_string()}]})
                );
            } else {
                println!("x [config] the config could not be loaded:\n    {error}");
            }
            return 1;
        }
    };
    let report = build(&config, &process_store());
    if args.json {
        match serde_json::to_string_pretty(&to_json(&report)) {
            Ok(text) => println!("{text}"),
            Err(error) => eprintln!("Error: {error}"),
        }
    } else {
        print!("{}", render_text(&report, args.origins));
    }
    i32::from(report.worst() == Level::Error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::store::fake::store;

    fn report_for(config: &Config) -> Report {
        let (_, store) = store();
        build(config, &store)
    }

    #[test]
    fn a_clean_config_has_no_warnings_besides_the_honest_limits_note() {
        let report = report_for(&Config::default());
        assert!(report.worst() <= Level::Warn, "{:?}", report.findings);
        assert!(report.findings.iter().any(|f| f.area == "limits"));
    }

    #[test]
    fn validation_problems_of_the_gateway_are_reported() {
        let mut config = Config::default();
        config.accounts.push(
            toml::from_str(
                "id = \"a\"\nprovider = \"cli:claude\"\nhome = \"/h\"\nsecret = \"keyring:x\"\n",
            )
            .unwrap(),
        );
        let report = report_for(&config);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.area == "gateway" && f.level == Level::Error),
            "{:?}",
            report.findings
        );
    }

    #[test]
    fn no_value_ever_appears_in_the_report() {
        let mut config = Config::default();
        config.provider.token = "canary-doctor-token-1234".into();
        config.providers.push(
            toml::from_str(
                "name = \"lm\"\nbase_url = \"http://example.com/v1\"\napi_key = \"canary-doctor-key-5678\"\n",
            )
            .unwrap(),
        );
        let report = report_for(&config);
        let text = render_text(&report, true);
        let json = to_json(&report).to_string();
        for shown in [text, json] {
            assert!(!shown.contains("canary-doctor"), "{shown}");
        }
        assert!(
            report.findings.iter().any(|f| f.area == "network"),
            "plain http to a public host is flagged"
        );
    }

    #[test]
    fn a_public_http_endpoint_is_flagged_and_a_local_one_is_not() {
        assert!(http_to_public_host("http://api.example.com/v1"));
        assert!(!http_to_public_host("http://localhost:1234/v1"));
        assert!(!http_to_public_host("http://192.168.1.5:8080"));
        assert!(!http_to_public_host("https://api.example.com"));
    }

    #[test]
    fn the_json_report_has_a_stable_shape() {
        let value = to_json(&report_for(&Config::default()));
        for key in ["layers", "secrets", "findings", "origins", "worst"] {
            assert!(value.get(key).is_some(), "{key}");
        }
    }
}
