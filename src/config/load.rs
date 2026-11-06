//! Загрузка слоёв, сохранение разницы, разрешение ссылок на секреты.

use super::describe::describe;
use super::fields;
use super::layers::{self, Layer, LayerInfo, LayerStatus, Origins};
use super::{Config, is_isolated, migrate, paths, startup, write};
use crate::error::{AppError, AppResult};
use crate::gateway::types::{AccountId, GatewaySettings, Roles};
use crate::secrets::{
    RefKind, Secret, SecretResolver, SecretStore, StoreResolver, TrustContext, process_store,
    redact,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::GATEWAY_SCHEMA as SUPPORTED_SCHEMA;

pub(super) fn gateway_is_default(settings: &GatewaySettings) -> bool {
    *settings == GatewaySettings::default()
}

pub(super) fn roles_empty(roles: &Roles) -> bool {
    roles.0.is_empty()
}

/// `[secrets]` — подтверждённые человеком хосты для ссылок на секреты.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretsConfig {
    /// `keyring:global/openai` → `["api.openai.com"]`.
    #[serde(default)]
    pub bindings: BTreeMap<String, Vec<String>>,
}

impl SecretsConfig {
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }
}

/// Ссылка на секрет, которую при загрузке не удалось прочитать.
#[derive(Debug, Clone)]
pub struct UnresolvedSecret {
    /// Поле конфига: `provider.token`, `providers[x].api_key`.
    pub label: String,
    pub reference: String,
    /// Причина без значения.
    pub reason: String,
}

/// Откуда загружен конфиг: слои, источники значений и снимок для точечного сохранения.
#[derive(Clone, Default)]
pub struct ConfigSource {
    /// Куда писать при сохранении; `None` — общий файл.
    pub write_path: Option<PathBuf>,
    pub layers: Vec<LayerInfo>,
    pub origins: Origins,
    /// Конфиг таким, каким он был при загрузке (или при последнем сохранении).
    pub baseline: Arc<Mutex<Option<toml::Table>>>,
    /// Аккаунты, которых нет в общем слое: их привёл проект.
    pub project_accounts: BTreeSet<String>,
    pub unresolved: Vec<UnresolvedSecret>,
}

impl std::fmt::Debug for ConfigSource {
    /// Снимок несёт значения секретов, поэтому в `Debug` его нет.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigSource")
            .field("write_path", &self.write_path)
            .field("layers", &self.layers)
            .field("origins", &self.origins.len())
            .field("project_accounts", &self.project_accounts)
            .field("unresolved", &self.unresolved)
            .finish_non_exhaustive()
    }
}

fn io_error(path: &Path, error: &std::io::Error) -> AppError {
    AppError::Config(format!("{}: {}", path.display(), error.kind()))
}

fn read_layer(path: &Path) -> AppResult<Option<String>> {
    match super::lock::retry_transient(|| crate::util::read_text(path)) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error(path, &e)),
    }
}

fn parse_table(path: &Path, text: &str) -> AppResult<toml::Table> {
    toml::from_str::<toml::Table>(text).map_err(|e| AppError::Config(describe(path, text, &e)))
}

/// Всё, что нужно знать загрузчику о процессе. Выделено, чтобы тесты не трогали настоящее.
pub(super) struct Plan<'a> {
    pub primary: &'a Path,
    pub primary_layer: Layer,
    pub project_dir: Option<PathBuf>,
    pub write_path: Option<PathBuf>,
    pub store: &'a Arc<SecretStore>,
}

pub(super) fn load_layers(plan: Plan<'_>) -> AppResult<Config> {
    let mut layer_infos = Vec::new();
    let mut notices = Vec::new();

    let primary_text = read_layer(plan.primary)?;
    let primary_table = match &primary_text {
        Some(text) => parse_table(plan.primary, text)?,
        None => toml::Table::new(),
    };
    // Типовая проверка по самому файлу: ошибка называет файл и строку, но не значение.
    let primary_config = match &primary_text {
        Some(text) => {
            super::parse(text).map_err(|e| AppError::Config(describe(plan.primary, text, &e)))?
        }
        None => Config::default(),
    };
    layer_infos.push(LayerInfo {
        layer: plan.primary_layer,
        path: plan.primary.to_path_buf(),
        status: if primary_text.is_some() {
            LayerStatus::Applied
        } else {
            LayerStatus::Missing
        },
    });

    let mut origins = Origins::new();
    layers::mark_origins(&primary_table, "", plan.primary_layer, &mut origins);

    let mut merged = primary_table.clone();
    let mut project_accounts = BTreeSet::new();
    let mut project_applied = false;
    if let Some(dir) = &plan.project_dir {
        let path = dir.join("config.toml");
        let status = load_project_layer(
            &path,
            &primary_table,
            &mut merged,
            &mut origins,
            &mut project_accounts,
            &mut notices,
        );
        project_applied = status == LayerStatus::Applied;
        layer_infos.push(LayerInfo {
            layer: Layer::Project,
            path,
            status,
        });
    }

    let mut config = if project_applied {
        let mut config: Config = merged.try_into().map_err(|e: toml::de::Error| {
            AppError::Config(format!("merged config: {}", e.message()))
        })?;
        if let Some(text) = &primary_text {
            super::apply_migrations(text, &mut config);
        }
        config
    } else {
        primary_config
    };

    if config.gateway.schema > SUPPORTED_SCHEMA {
        return Err(AppError::Config(format!(
            "{}: [gateway] schema = {} is newer than this build understands ({}); update Pooprusteek",
            plan.primary.display(),
            config.gateway.schema,
            SUPPORTED_SCHEMA
        )));
    }

    let unresolved = resolve_legacy_references(&mut config, plan.store);
    for item in &unresolved {
        notices.push(format!(
            "{} ({}) could not be read: {}. Run `pooprusteek doctor`.",
            item.label, item.reference, item.reason
        ));
    }
    register_secret_files(&config);

    let baseline = toml::Table::try_from(&config)
        .map_err(|e| AppError::Config(format!("config snapshot: {e}")))?;
    config.source = ConfigSource {
        write_path: plan.write_path,
        layers: layer_infos,
        origins,
        baseline: Arc::new(Mutex::new(Some(baseline))),
        project_accounts,
        unresolved,
    };
    config.migration_notices.extend(notices);
    Ok(config)
}

/// Применяет проектный слой, если он безопасен. Возвращает итог по слою.
fn load_project_layer(
    path: &Path,
    primary: &toml::Table,
    merged: &mut toml::Table,
    origins: &mut Origins,
    project_accounts: &mut BTreeSet<String>,
    notices: &mut Vec<String>,
) -> LayerStatus {
    let text = match read_layer(path) {
        Ok(Some(text)) => text,
        Ok(None) => return LayerStatus::Missing,
        Err(error) => {
            notices.push(format!("Project config skipped: {error}"));
            return LayerStatus::Unreadable(error.to_string());
        }
    };
    let table = match parse_table(path, &text) {
        Ok(table) => table,
        Err(error) => {
            notices.push(format!("Project config skipped: {error}"));
            return LayerStatus::Unreadable(error.to_string());
        }
    };
    let reasons = layers::check_project_layer(&table, primary);
    if !reasons.is_empty() {
        notices.push(format!(
            "Project config {} was not applied: {}",
            path.display(),
            reasons.join("; ")
        ));
        return LayerStatus::Rejected(reasons);
    }
    // Слой должен читаться как конфиг; иначе одна опечатка ломала бы весь запуск.
    if let Err(error) = super::parse(&text) {
        let shown = describe(path, &text, &error);
        notices.push(format!("Project config skipped: {shown}"));
        return LayerStatus::Unreadable(shown);
    }
    project_accounts.extend(layers::new_project_accounts(&table, primary));
    layers::merge(merged, &table);
    layers::mark_origins(&table, "", Layer::Project, origins);
    LayerStatus::Applied
}

// ── секреты в памяти ────────────────────────────────────────────────────

fn resolve_one(
    resolver: &StoreResolver,
    label: &str,
    raw: &str,
    unresolved: &mut Vec<UnresolvedSecret>,
) -> Option<String> {
    if !fields::is_reference(raw) {
        // Открытое значение (ещё не перенесено или хранилища нет): помним, чтобы не печатать.
        redact::register(&Secret::new(raw));
        return Some(raw.to_string());
    }
    let owner = AccountId::parse("config").expect("valid id");
    let outcome = crate::gateway::types::SecretRef::parse(raw)
        .map_err(|reason| crate::secrets::SecretError::Refused {
            name: label.to_string(),
            reason,
        })
        .and_then(|reference| resolver.resolve(&reference, &owner));
    match outcome {
        Ok(secret) => Some(secret.expose().to_string()),
        Err(error) => {
            unresolved.push(UnresolvedSecret {
                label: label.to_string(),
                reference: RefKind::parse(raw)
                    .map_or_else(|_| "?".to_string(), |k| k.display_name()),
                reason: error.to_string(),
            });
            None
        }
    }
}

/// Старые поля (`provider.token`, `providers[].api_key`, `server.api_key`) читает остальной
/// код как есть, поэтому ссылки разворачиваются в память при загрузке. В файл оно не вернётся:
/// сохранение пишет только разницу.
fn resolve_legacy_references(
    config: &mut Config,
    store: &Arc<SecretStore>,
) -> Vec<UnresolvedSecret> {
    let resolver = StoreResolver::new(Arc::clone(store), TrustContext::default());
    let mut unresolved = Vec::new();
    if !config.provider.token.is_empty() {
        config.provider.token = resolve_one(
            &resolver,
            "provider.token",
            &config.provider.token.clone(),
            &mut unresolved,
        )
        .unwrap_or_default();
    }
    if let Some(key) = config.server.api_key.clone().filter(|k| !k.is_empty()) {
        config.server.api_key = resolve_one(&resolver, "server.api_key", &key, &mut unresolved);
    }
    for entry in &mut config.providers {
        if let Some(key) = entry.api_key.clone().filter(|k| !k.is_empty()) {
            let label = format!("providers[{}].api_key", entry.name);
            entry.api_key = resolve_one(&resolver, &label, &key, &mut unresolved);
        }
    }
    for account in &config.accounts {
        if let Some(key) = &account.api_key {
            redact::register(&Secret::new(key.as_str()));
        }
    }
    unresolved
}

/// Файлы из `file:` читать модели нельзя.
fn register_secret_files(config: &Config) {
    for account in &config.accounts {
        for reference in [&account.secret, &account.direct_token]
            .into_iter()
            .flatten()
        {
            if let Ok(RefKind::File(path)) = RefKind::from_ref(reference) {
                super::protect::register_secret_file(path);
            }
        }
    }
}

impl Config {
    /// Кто откуда и какие хосты подтверждены: вход для `SecretResolver`.
    pub fn trust_context(&self) -> TrustContext {
        let mut trust = TrustContext::default();
        for account in &self.accounts {
            if let Some(host) = account.base_url.as_deref().and_then(TrustContext::host_of) {
                trust.hosts.insert(account.id.clone(), host);
            }
            if self.source.project_accounts.contains(account.id.as_str()) {
                trust.project_accounts.insert(account.id.clone());
            }
        }
        trust.bindings.clone_from(&self.secrets.bindings);
        trust
    }

    /// Резолвер ссылок аккаунтов на системное хранилище процесса.
    pub fn secret_resolver(&self) -> StoreResolver {
        StoreResolver::new(process_store(), self.trust_context())
    }
}

// ── входы ───────────────────────────────────────────────────────────────

/// `--config <file>` when given, the user's real config otherwise.
pub fn load_from_optional(explicit: Option<&Path>) -> AppResult<Config> {
    match explicit {
        Some(path) => {
            paths::set_explicit_config(path);
            load_from(path)
        }
        None => load(),
    }
}

/// Load from an explicit file instead of the user's real config. Used by
/// `--config`: one layer, no project layer, writes go back to this file.
pub fn load_from(path: &Path) -> AppResult<Config> {
    if read_layer(path)?.is_none() {
        return Err(AppError::Config(format!(
            "{}: file not found",
            path.display()
        )));
    }
    let store = process_store();
    load_layers(Plan {
        primary: path,
        primary_layer: Layer::Explicit,
        project_dir: None,
        write_path: Some(path.to_path_buf()),
        store: &store,
    })
}

pub fn load() -> AppResult<Config> {
    let global = paths::global_config_path();
    let store = process_store();
    let notices = startup::prepare_config(&global, &store);
    let project_dir = std::env::current_dir()
        .ok()
        .and_then(|cwd| paths::find_project_layer(&cwd));
    let mut config = load_layers(Plan {
        primary: &global,
        primary_layer: Layer::Global,
        project_dir,
        write_path: None,
        store: &store,
    })?;
    config.migration_notices.splice(0..0, notices);
    Ok(config)
}

// ── сохранение ──────────────────────────────────────────────────────────

/// Сохранить настройки, а отказ вернуть готовым для показа текстом.
/// Одна формулировка на все панели: копий было шестнадцать.
pub fn save_or_message(config: &Config) -> Result<(), String> {
    save(config).map_err(|error| format!("Failed to save config: {error}"))
}

/// Записывает в файл только то, что изменилось с загрузки. Комментарии, порядок и
/// незнакомые ключи остаются; повреждённый вручную файл не затирается.
pub fn save(config: &Config) -> AppResult<()> {
    let path = config
        .source
        .write_path
        .clone()
        .unwrap_or_else(Config::path);
    let current = toml::Table::try_from(config)
        .map_err(|e| AppError::Config(format!("config serialisation: {e}")))?;
    let baseline = config.source.baseline.lock().ok().and_then(|b| b.clone());
    let report = write::save_diff(&path, baseline.as_ref(), &current)
        .map_err(|e| AppError::Config(e.to_string()))?;
    for skipped in &report.skipped {
        tracing::warn!(
            "config change not saved (not in {}): {skipped}",
            path.display()
        );
    }
    if let Ok(mut slot) = config.source.baseline.lock() {
        *slot = Some(current);
    }
    if report.wrote {
        startup::kick_background_migration(&path);
        startup::delete_orphans(report.removed_refs, report.kept_refs);
    }
    Ok(())
}

/// Подтвердить хост для ссылки на секрет: `[secrets.bindings]` общего файла.
pub fn bind_secret_host(reference: &str, host: &str) -> AppResult<()> {
    use super::write::{Op, Seg};
    RefKind::parse(reference).map_err(AppError::Config)?;
    let path = Config::path();
    write::with_document(&path, |doc| {
        let current = super::parse(&doc.to_string())
            .map_err(|e| e.message().to_string())?
            .secrets
            .bindings
            .get(reference)
            .cloned()
            .unwrap_or_default();
        let host = host.to_ascii_lowercase();
        if current.contains(&host) {
            return Ok(());
        }
        let hosts: Vec<toml::Value> = current
            .into_iter()
            .chain(std::iter::once(host))
            .map(toml::Value::String)
            .collect();
        let op = Op::Set(
            vec![
                Seg::Key("secrets".into()),
                Seg::Key("bindings".into()),
                Seg::Key(reference.to_string()),
            ],
            toml::Value::Array(hosts),
        );
        write::apply(doc, &[op]);
        Ok(())
    })
    .map(|_| ())
    .map_err(|e| AppError::Config(e.to_string()))
}

impl Config {
    /// Модель данных шлюза одним значением: для `validate()` и движка.
    pub fn gateway_config(&self) -> crate::gateway::types::GatewayConfig {
        crate::gateway::types::GatewayConfig {
            gateway: self.gateway.clone(),
            accounts: self.accounts.clone(),
            routes: self.routes.clone(),
            roles: self.roles.clone(),
        }
    }
}

/// Ссылка на секрет в файле конфига: поле, сама ссылка (не значение).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileReference {
    pub label: String,
    pub reference: String,
}

/// Ссылки на секреты, записанные в файле, куда пишет этот процесс.
pub fn file_references() -> Vec<FileReference> {
    let path = Config::path();
    let Ok(text) = crate::util::read_text(&path) else {
        return Vec::new();
    };
    let Ok(mut doc) = text.parse::<toml_edit::DocumentMut>() else {
        return Vec::new();
    };
    fields::reference_slots(&mut doc)
        .into_iter()
        .map(|(slot, reference)| FileReference {
            label: slot.label(),
            reference,
        })
        .collect()
}

/// Открытые секреты, оставшиеся в файле: только названия полей.
pub fn inline_secret_labels() -> Vec<String> {
    let path = Config::path();
    let Ok(text) = crate::util::read_text(&path) else {
        return Vec::new();
    };
    let Ok(mut doc) = text.parse::<toml_edit::DocumentMut>() else {
        return Vec::new();
    };
    fields::plaintext_slots(&mut doc)
        .into_iter()
        .map(|(slot, _)| slot.label())
        .collect()
}

/// Привязывает записанный в хранилище секрет к аккаунту или записи `[[providers]]`
/// с таким именем, если у них ещё нет ключа. Возвращает поле, которое изменено.
pub fn link_secret_reference(name: &str, reference: &str) -> AppResult<Option<String>> {
    use super::write::{Op, Seg};
    let path = Config::path();
    let (label, _) = write::with_document(&path, |doc| {
        let typed = super::parse(&doc.to_string()).map_err(|e| e.message().to_string())?;
        let account = typed
            .accounts
            .iter()
            .find(|a| a.id.as_str() == name)
            .filter(|a| a.secret.is_none() && a.api_key.is_none() && a.home.is_none());
        let provider = typed
            .providers
            .iter()
            .find(|p| p.name == name)
            .filter(|p| p.api_key.as_deref().is_none_or(str::is_empty));
        let (path, label) = if account.is_some() {
            (
                vec![
                    Seg::Key("accounts".into()),
                    Seg::Elem(name.to_string()),
                    Seg::Key("secret".into()),
                ],
                format!("accounts[{name}].secret"),
            )
        } else if provider.is_some() {
            (
                vec![
                    Seg::Key("providers".into()),
                    Seg::Elem(name.to_string()),
                    Seg::Key("api_key".into()),
                ],
                format!("providers[{name}].api_key"),
            )
        } else {
            return Ok(None);
        };
        let op = Op::Set(path, toml::Value::String(reference.to_string()));
        write::apply(doc, &[op]);
        Ok(Some(label))
    })
    .map_err(|e| AppError::Config(e.to_string()))?;
    Ok(label)
}

/// Полный сброс: записи в хранилище, на которые ссылается конфиг, токены MCP и сами
/// файлы конфига. Блокирующий; зовётся из `spawn_blocking`. Возвращает, что не вышло.
pub fn wipe_secrets_and_files(config: &Config, mcp_servers: &[String]) -> Vec<String> {
    use crate::secrets::keyring_slots::KeyringSlots;
    use crate::secrets::{DEEPSEEK_TOKEN_ID, SERVER_KEY_ID, account_secret_id};
    let mut failed = Vec::new();
    if is_isolated() {
        return failed;
    }
    let mut ids: BTreeSet<String> =
        BTreeSet::from([DEEPSEEK_TOKEN_ID.to_string(), SERVER_KEY_ID.to_string()]);
    for name in config
        .providers
        .iter()
        .map(|p| p.name.as_str())
        .chain(config.accounts.iter().map(|a| a.id.as_str()))
    {
        ids.insert(account_secret_id(name));
    }
    for reference in file_references() {
        if let Ok(RefKind::Keyring(id)) = RefKind::parse(&reference.reference) {
            ids.insert(id);
        }
    }
    let store = process_store();
    for id in &ids {
        if let Err(error) = store.delete(id) {
            failed.push(error.to_string());
        }
    }
    use crate::secrets::chunked::Slots;
    let oauth = KeyringSlots::new("pooprusteek-mcp");
    for server in mcp_servers {
        if let Err(error) = oauth.remove(server) {
            failed.push(format!("mcp token '{server}': {error:?}"));
        }
    }
    let global = paths::global_config_path();
    let mut files = vec![
        global.clone(),
        migrate::backup_path(&global),
        super::lock::lock_path(&global),
    ];
    if let Some(legacy) = paths::legacy_config_path() {
        files.push(migrate::backup_path(&legacy));
        files.push(legacy);
    }
    for file in files {
        match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => failed.push(format!("{}: {}", file.display(), e.kind())),
        }
    }
    failed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::store::fake::store;

    fn dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pooprusteek-load-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn plan<'a>(
        primary: &'a Path,
        project: Option<PathBuf>,
        store: &'a Arc<SecretStore>,
    ) -> Plan<'a> {
        Plan {
            primary,
            primary_layer: Layer::Global,
            project_dir: project,
            write_path: Some(primary.to_path_buf()),
            store,
        }
    }

    fn fake_store() -> (
        Arc<crate::secrets::store::fake::FakeBackend>,
        Arc<SecretStore>,
    ) {
        let (backend, store) = store();
        (backend, Arc::new(store))
    }

    #[test]
    fn the_project_layer_overrides_the_global_one_and_names_its_source() {
        let root = dir("layers");
        let global = root.join("config.toml");
        std::fs::write(
            &global,
            "[ui]\ntheme = \"nord\"\n[agent]\nmax_retries = 2\n",
        )
        .unwrap();
        let project = root.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("config.toml"), "[ui]\ntheme = \"dark\"\n").unwrap();
        let (_, store) = fake_store();
        let config = load_layers(plan(&global, Some(project), &store)).unwrap();
        assert_eq!(config.ui.theme, "dark");
        assert_eq!(config.agent.max_retries, 2);
        assert_eq!(config.source.origins["ui.theme"], Layer::Project);
        assert_eq!(config.source.origins["agent.max_retries"], Layer::Global);
        assert!(
            config
                .source
                .layers
                .iter()
                .any(|l| l.layer == Layer::Project && l.status == LayerStatus::Applied)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_project_layer_with_a_forbidden_key_is_ignored_with_a_notice() {
        let root = dir("reject");
        let global = root.join("config.toml");
        std::fs::write(&global, "[ui]\ntheme = \"nord\"\n").unwrap();
        let project = root.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("config.toml"),
            "[ui]\ntheme = \"dark\"\n[server]\nport = 1\n",
        )
        .unwrap();
        let (_, store) = fake_store();
        let config = load_layers(plan(&global, Some(project), &store)).unwrap();
        assert_eq!(
            config.ui.theme, "nord",
            "nothing from a rejected layer applies"
        );
        assert!(
            config
                .migration_notices
                .iter()
                .any(|n| n.contains("not applied")),
            "{:?}",
            config.migration_notices
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_broken_project_layer_never_breaks_startup() {
        let root = dir("brokenproj");
        let global = root.join("config.toml");
        std::fs::write(&global, "").unwrap();
        let project = root.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("config.toml"), "[ui\n").unwrap();
        let (_, store) = fake_store();
        assert!(load_layers(plan(&global, Some(project), &store)).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_broken_global_file_is_a_clear_error_that_does_not_echo_values() {
        let root = dir("brokenglobal");
        let global = root.join("config.toml");
        std::fs::write(
            &global,
            "[provider]\ntoken = \"sk-canary-9999999999\"\nmodel = \n",
        )
        .unwrap();
        let (_, store) = fake_store();
        let error = load_layers(plan(&global, None, &store))
            .unwrap_err()
            .to_string();
        assert!(error.contains("line"), "{error}");
        assert!(!error.contains("canary"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_typo_in_a_gateway_table_is_an_error_naming_the_key() {
        let root = dir("typo");
        let global = root.join("config.toml");
        std::fs::write(
            &global,
            "[[accounts]]\nid = \"a\"\nprovider = \"openai\"\nbase_urll = \"http://x\"\n",
        )
        .unwrap();
        let (_, store) = fake_store();
        let error = load_layers(plan(&global, None, &store))
            .unwrap_err()
            .to_string();
        assert!(error.contains("base_urll"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_old_config_keeps_ignoring_unknown_legacy_fields() {
        let root = dir("legacyfields");
        let global = root.join("config.toml");
        std::fs::write(
            &global,
            "[agent]\nsome_future_field = 1\n[provider]\nflavour = \"x\"\n",
        )
        .unwrap();
        let (_, store) = fake_store();
        assert!(load_layers(plan(&global, None, &store)).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_newer_gateway_schema_is_refused_by_name() {
        let root = dir("schema");
        let global = root.join("config.toml");
        std::fs::write(&global, "[gateway]\nschema = 9\n").unwrap();
        let (_, store) = fake_store();
        let error = load_layers(plan(&global, None, &store))
            .unwrap_err()
            .to_string();
        assert!(error.contains("schema = 9"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn references_in_the_old_fields_become_values_in_memory_only() {
        let root = dir("resolve");
        let global = root.join("config.toml");
        std::fs::write(
            &global,
            "# мой\n[provider]\ntoken = \"keyring:global/deepseek-token\"\n[[providers]]\nname = \"lm\"\nbase_url = \"http://x/v1\"\napi_key = \"keyring:global/lm\"\n",
        )
        .unwrap();
        let (backend, store) = fake_store();
        {
            let mut data = backend.data.lock().unwrap();
            data.insert(
                "global/deepseek-token".into(),
                Secret::new("real-ds-token-123"),
            );
            data.insert("global/lm".into(), Secret::new("real-lm-key-456"));
        }
        let config = load_layers(plan(&global, None, &store)).unwrap();
        assert_eq!(config.provider.token, "real-ds-token-123");
        assert_eq!(
            config.providers[0].api_key.as_deref(),
            Some("real-lm-key-456")
        );
        assert!(config.source.unresolved.is_empty());
        // Сохранение, которое ничего не менялось, не возвращает значения в файл.
        save(&config).unwrap();
        let text = std::fs::read_to_string(&global).unwrap();
        assert!(
            !text.contains("real-ds-token") && !text.contains("real-lm-key"),
            "{text}"
        );
        assert!(
            text.contains("# мой") && text.contains("keyring:global/lm"),
            "{text}"
        );
        // А Debug конфига значений не печатает.
        let shown = format!("{config:?}");
        assert!(
            !shown.contains("real-ds-token") && !shown.contains("real-lm-key"),
            "{shown}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Загрузка — чтение: ни файл, ни хранилище она не меняет, даже если в файле открытый ключ.
    #[test]
    fn loading_changes_neither_the_file_nor_the_store() {
        let root = dir("readonly");
        let global = root.join("config.toml");
        let text = "[provider]\ntoken = \"plain-token-0123456789\"\n";
        std::fs::write(&global, text).unwrap();
        let (backend, store) = fake_store();
        let config = load_layers(plan(&global, None, &store)).unwrap();
        assert_eq!(config.provider.token, "plain-token-0123456789");
        assert_eq!(std::fs::read_to_string(&global).unwrap(), text);
        assert!(backend.data.lock().unwrap().is_empty());
        assert!(!migrate::backup_path(&global).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unreadable_reference_leaves_the_field_empty_and_the_file_alone() {
        let root = dir("unresolved");
        let global = root.join("config.toml");
        let text = "[provider]\ntoken = \"keyring:global/deepseek-token\"\n";
        std::fs::write(&global, text).unwrap();
        let (backend, store) = fake_store();
        backend.mode(crate::secrets::store::fake::Mode::Unavailable);
        let config = load_layers(plan(&global, None, &store)).unwrap();
        assert!(config.provider.token.is_empty());
        assert_eq!(config.source.unresolved.len(), 1);
        assert!(
            config
                .migration_notices
                .iter()
                .any(|n| n.contains("provider.token")),
            "{:?}",
            config.migration_notices
        );
        // Несвязанная правка не стирает ссылку из файла.
        let mut changed = config;
        changed.ui.theme = "nord".into();
        save(&changed).unwrap();
        let saved = std::fs::read_to_string(&global).unwrap();
        assert!(saved.contains("keyring:global/deepseek-token"), "{saved}");
        assert!(saved.contains("theme = \"nord\""), "{saved}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn saving_through_a_project_layer_does_not_copy_its_values_into_the_global_file() {
        let root = dir("noleak");
        let global = root.join("config.toml");
        std::fs::write(&global, "# общий\n[ui]\ntheme = \"nord\"\n").unwrap();
        let project = root.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("config.toml"), "[agent]\nmax_retries = 7\n").unwrap();
        let (_, store) = fake_store();
        let mut config = load_layers(plan(&global, Some(project), &store)).unwrap();
        assert_eq!(config.agent.max_retries, 7);
        config.ui.theme = "dark".into();
        save(&config).unwrap();
        let saved = std::fs::read_to_string(&global).unwrap();
        assert!(!saved.contains("max_retries"), "{saved}");
        assert!(
            saved.contains("theme = \"dark\"") && saved.contains("# общий"),
            "{saved}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn two_saves_in_a_row_do_not_duplicate_added_providers() {
        let root = dir("twice");
        let global = root.join("config.toml");
        std::fs::write(&global, "[ui]\ntheme = \"x\"\n").unwrap();
        let (_, store) = fake_store();
        let mut config = load_layers(plan(&global, None, &store)).unwrap();
        config.providers.push(crate::config::ProviderEntry {
            name: "lm".into(),
            base_url: "http://x/v1".into(),
            api_key: None,
            model: "m".into(),
            protocol: Default::default(),
            tools: Default::default(),
        });
        save(&config).unwrap();
        save(&config).unwrap();
        let saved = std::fs::read_to_string(&global).unwrap();
        assert_eq!(saved.matches("[[providers]]").count(), 1, "{saved}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn saving_a_new_key_logs_nothing_and_a_later_logout_drops_the_reference() {
        let root = dir("logout");
        let global = root.join("config.toml");
        std::fs::write(
            &global,
            "[provider]\ntoken = \"keyring:global/deepseek-token\"\n",
        )
        .unwrap();
        let (backend, store) = fake_store();
        backend.data.lock().unwrap().insert(
            "global/deepseek-token".into(),
            Secret::new("tok-real-value-1"),
        );
        let mut config = load_layers(plan(&global, None, &store)).unwrap();
        config.provider.token = String::new();
        save(&config).unwrap();
        let saved = std::fs::read_to_string(&global).unwrap();
        assert!(!saved.contains("keyring:"), "{saved}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_damaged_file_blocks_saving_instead_of_being_overwritten() {
        let root = dir("damagedsave");
        let global = root.join("config.toml");
        let broken = "[ui\ntheme = \"nord\"\n";
        std::fs::write(&global, broken).unwrap();
        let mut config = Config::default();
        config.source.write_path = Some(global.clone());
        config.ui.theme = "dark".into();
        let error = save(&config).unwrap_err().to_string();
        assert!(error.contains("untouched"), "{error}");
        assert_eq!(std::fs::read_to_string(&global).unwrap(), broken);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_trust_context_sees_hosts_projects_and_bindings() {
        let root = dir("trust");
        let global = root.join("config.toml");
        std::fs::write(
            &global,
            "[[accounts]]\nid = \"mine\"\nprovider = \"openai\"\nbase_url = \"https://api.openai.com/v1\"\n[secrets.bindings]\n\"keyring:global/mine\" = [\"api.openai.com\"]\n",
        )
        .unwrap();
        let project = root.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("config.toml"),
            "[[accounts]]\nid = \"team\"\nprovider = \"openai\"\nbase_url = \"https://llm.team.example/v1\"\nsecret = \"keyring:global/mine\"\n",
        )
        .unwrap();
        let (_, store) = fake_store();
        let config = load_layers(plan(&global, Some(project), &store)).unwrap();
        let trust = config.trust_context();
        let team = AccountId::parse("team").unwrap();
        assert!(trust.project_accounts.contains(&team));
        assert!(
            !trust
                .project_accounts
                .contains(&AccountId::parse("mine").unwrap())
        );
        assert_eq!(trust.hosts[&team], "llm.team.example");
        assert!(trust.bindings.contains_key("keyring:global/mine"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
