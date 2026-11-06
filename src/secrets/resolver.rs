//! `SecretResolver`: ссылка на секрет → значение, в момент сборки адаптера.
//!
//! Правила доверия: аккаунт из проектного слоя недоверенный. Ему нельзя `cmd:`
//! и `file:`, а `keyring:`/`env:` выдаются только хосту, который человек
//! подтвердил (`secrets bind`). Чужой `base_url` чужого ключа не получает.

use super::error::{SecretError, SecretResult};
use super::redact;
use super::reference::RefKind;
use super::secret::Secret;
use super::store::SecretStore;
use crate::gateway::types::{AccountId, SecretRef};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Контракт SECRETS ↔ ENGINE (`.docs/gateway-contracts.md` §2).
pub trait SecretResolver: Send + Sync {
    /// Зовётся при сборке адаптера, не при разборе конфига. Ошибка называет имя, не значение.
    fn resolve(&self, reference: &SecretRef, account: &AccountId) -> Result<Secret, SecretError>;
}

/// Сколько ждём команду `cmd:` и сколько байт её вывода читаем.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const OUTPUT_CAP: usize = 64 * 1024;

/// Кто из аккаунтов откуда и какие хосты подтверждены для каких ссылок.
#[derive(Debug, Clone, Default)]
pub struct TrustContext {
    /// Аккаунты, пришедшие из проектного слоя.
    pub project_accounts: BTreeSet<AccountId>,
    /// Хост из `base_url` аккаунта.
    pub hosts: BTreeMap<AccountId, String>,
    /// Ссылка (`keyring:global/x`) → подтверждённые хосты.
    pub bindings: BTreeMap<String, Vec<String>>,
}

impl TrustContext {
    /// Хост из адреса: нижний регистр, без порта и пути.
    #[cfg(test)]
    pub fn host_of(base_url: &str) -> Option<String> {
        reqwest::Url::parse(base_url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
    }

    /// Происхождение запроса: схема и хост (`https://api.openai.com`), без порта и пути. Привязка
    /// держится за схему тоже: подтверждённый https не разрешает ключ по открытому http.
    pub fn origin_of(base_url: &str) -> Option<String> {
        let url = reqwest::Url::parse(base_url).ok()?;
        Some(format!(
            "{}://{}",
            url.scheme().to_ascii_lowercase(),
            url.host_str()?.to_ascii_lowercase()
        ))
    }

    /// Запись привязки или адрес аккаунта к единому виду: голый `host` значит `https://host`.
    fn normalize_origin(raw: &str) -> String {
        let raw = raw.trim().trim_end_matches('/').to_ascii_lowercase();
        if raw.contains("://") {
            raw
        } else {
            format!("https://{raw}")
        }
    }

    fn bound(&self, reference: &str, origin: &str) -> bool {
        let wanted = Self::normalize_origin(origin);
        self.bindings.get(reference).is_some_and(|allowed| {
            allowed
                .iter()
                .any(|entry| Self::normalize_origin(entry) == wanted)
        })
    }
}

pub struct StoreResolver {
    store: Arc<SecretStore>,
    trust: TrustContext,
    cache: Mutex<HashMap<String, Secret>>,
}

impl StoreResolver {
    pub fn new(store: Arc<SecretStore>, trust: TrustContext) -> Self {
        Self {
            store,
            trust,
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn check_trust(
        &self,
        kind: &RefKind,
        reference: &str,
        account: &AccountId,
    ) -> SecretResult<()> {
        let from_project = self.trust.project_accounts.contains(account);
        if from_project && !kind.allowed_in_project_layer() {
            return Err(SecretError::Refused {
                name: kind.display_name(),
                reason: format!(
                    "account '{account}' comes from the project config, which may not use {}: references",
                    kind.backend_name()
                ),
            });
        }
        let has_bindings = self.trust.bindings.contains_key(reference);
        if !from_project && !has_bindings {
            return Ok(());
        }
        let Some(host) = self.trust.hosts.get(account) else {
            // Аккаунт пользователя без адреса идёт на штатный хост провайдера. Проектный без
            // адреса подтвердить нечем: чужой репозиторий мог бы жечь квоту, поэтому отказ.
            return if from_project {
                Err(SecretError::Refused {
                    name: reference.to_string(),
                    reason: format!(
                        "account '{account}' comes from the project config and has no base_url, so there is no host to confirm"
                    ),
                })
            } else {
                Ok(())
            };
        };
        if self.trust.bound(reference, host) {
            Ok(())
        } else {
            Err(SecretError::HostNotTrusted {
                name: reference.to_string(),
                host: host.clone(),
            })
        }
    }

    fn fetch(&self, kind: &RefKind) -> SecretResult<Secret> {
        let name = kind.display_name();
        let missing = |backend: &str| SecretError::NotFound {
            name: name.clone(),
            backend: backend.to_string(),
        };
        match kind {
            RefKind::Keyring(id) => self
                .store
                .get(id)?
                .ok_or_else(|| missing(self.store.backend_name())),
            RefKind::Env(var) => match std::env::var(var) {
                Ok(value) if !value.trim().is_empty() => Ok(Secret::new(value.trim())),
                _ => Err(missing("env")),
            },
            RefKind::Command(argv) => run_command(argv, &name),
            RefKind::File(path) => read_secret_file(path, &name),
            RefKind::None => Err(SecretError::NotNeeded { name }),
        }
    }
}

impl SecretResolver for StoreResolver {
    fn resolve(&self, reference: &SecretRef, account: &AccountId) -> Result<Secret, SecretError> {
        let kind = RefKind::from_ref(reference).map_err(|reason| SecretError::Refused {
            name: reference.as_str().chars().take(24).collect(),
            reason,
        })?;
        if matches!(kind, RefKind::None) {
            return Err(SecretError::NotNeeded {
                name: account.to_string(),
            });
        }
        self.check_trust(&kind, reference.as_str(), account)?;
        if let Some(hit) = self
            .cache
            .lock()
            .ok()
            .and_then(|cache| cache.get(reference.as_str()).cloned())
        {
            return Ok(hit);
        }
        let secret = self.fetch(&kind)?;
        redact::register(&secret);
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(reference.as_str().to_string(), secret.clone());
        }
        Ok(secret)
    }
}

/// Запускает команду без оболочки, с таймаутом; первая строка вывода — секрет.
fn run_command(argv: &[String], name: &str) -> SecretResult<Secret> {
    let unavailable = |reason: String| SecretError::Unavailable {
        name: name.to_string(),
        backend: "cmd".to_string(),
        reason,
    };
    let mut command = std::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Без мигающего окна консоли.
        command.creation_flags(0x0800_0000);
    }
    let mut child = command
        .spawn()
        .map_err(|e| unavailable(format!("cannot start: {}", e.kind())))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| unavailable("no output".into()))?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = (&mut stdout)
            .take(OUTPUT_CAP as u64)
            .read_to_end(&mut buffer);
        let _ = tx.send(buffer);
    });
    let started = Instant::now();
    let bytes = loop {
        if let Ok(bytes) = rx.try_recv() {
            break bytes;
        }
        if started.elapsed() > COMMAND_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SecretError::Timeout {
                name: name.to_string(),
                backend: "cmd".to_string(),
            });
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let status = child
        .wait()
        .map_err(|e| unavailable(format!("wait failed: {}", e.kind())))?;
    if !status.success() {
        return Err(unavailable("the command exited with an error".to_string()));
    }
    let text = crate::util::decode_process_output(&bytes);
    let value = text.lines().next().unwrap_or("").trim();
    if value.is_empty() {
        return Err(SecretError::NotFound {
            name: name.to_string(),
            backend: "cmd".to_string(),
        });
    }
    Ok(Secret::new(value))
}

/// Читает секрет из файла. На unix файл не должен быть доступен другим.
fn read_secret_file(path: &Path, name: &str) -> SecretResult<Secret> {
    let refused = |reason: &str| SecretError::Refused {
        name: name.to_string(),
        reason: reason.to_string(),
    };
    let meta = std::fs::metadata(path).map_err(|e| SecretError::NotFound {
        name: name.to_string(),
        backend: format!("file ({})", e.kind()),
    })?;
    if !meta.is_file() {
        return Err(refused("not a regular file"));
    }
    if meta.len() > OUTPUT_CAP as u64 {
        return Err(refused("file is larger than 64 KiB"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(refused("file is readable by other users; run chmod 600"));
        }
    }
    let text = crate::util::read_text(path).map_err(|e| SecretError::NotFound {
        name: name.to_string(),
        backend: format!("file ({})", e.kind()),
    })?;
    let value = text.trim();
    if value.is_empty() {
        return Err(SecretError::NotFound {
            name: name.to_string(),
            backend: "file".to_string(),
        });
    }
    Ok(Secret::new(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::store::fake::store;

    fn account(id: &str) -> AccountId {
        AccountId::parse(id).unwrap()
    }

    fn reference(raw: &str) -> SecretRef {
        SecretRef::parse(raw).unwrap()
    }

    fn resolver(
        trust: TrustContext,
    ) -> (Arc<crate::secrets::store::fake::FakeBackend>, StoreResolver) {
        let (backend, store) = store();
        (backend, StoreResolver::new(Arc::new(store), trust))
    }

    #[test]
    fn a_keyring_reference_resolves_through_the_store() {
        let (backend, resolver) = resolver(TrustContext::default());
        backend
            .data
            .lock()
            .unwrap()
            .insert("global/x".into(), Secret::new("value"));
        let secret = resolver
            .resolve(&reference("keyring:global/x"), &account("a"))
            .unwrap();
        assert_eq!(secret.expose(), "value");
    }

    #[test]
    fn a_missing_secret_is_named_not_shown() {
        let (_, resolver) = resolver(TrustContext::default());
        let error = resolver
            .resolve(&reference("keyring:global/nope"), &account("a"))
            .unwrap_err();
        assert!(matches!(error, SecretError::NotFound { .. }));
        assert!(error.to_string().contains("global/nope"));
    }

    #[test]
    fn none_means_no_secret_is_needed() {
        let (_, resolver) = resolver(TrustContext::default());
        let error = resolver
            .resolve(&reference("none"), &account("a"))
            .unwrap_err();
        assert!(matches!(error, SecretError::NotNeeded { .. }));
    }

    #[test]
    fn env_references_read_the_variable() {
        let (_, resolver) = resolver(TrustContext::default());
        let name = format!("POOPRUSTEEK_TEST_ENV_{}", std::process::id());
        // SAFETY-free: переменная с уникальным именем, другие тесты её не читают.
        unsafe { std::env::set_var(&name, " env-value ") };
        let secret = resolver
            .resolve(&reference(&format!("env:{name}")), &account("a"))
            .unwrap();
        assert_eq!(secret.expose(), "env-value");
        unsafe { std::env::remove_var(&name) };
        let (_, fresh) = (
            0,
            StoreResolver::new(Arc::new(store().1), TrustContext::default()),
        );
        assert!(
            fresh
                .resolve(&reference(&format!("env:{name}")), &account("a"))
                .is_err()
        );
    }

    #[test]
    fn a_project_account_gets_no_commands_and_no_files() {
        let mut trust = TrustContext::default();
        trust.project_accounts.insert(account("proj"));
        let (_, resolver) = resolver(trust);
        for raw in [r#"cmd:["echo","x"]"#, "file:/etc/hostname"] {
            let error = resolver
                .resolve(&reference(raw), &account("proj"))
                .unwrap_err();
            assert!(
                matches!(error, SecretError::Refused { .. }),
                "{raw}: {error}"
            );
        }
    }

    #[test]
    fn a_project_account_with_a_foreign_base_url_does_not_get_a_global_key() {
        let mut trust = TrustContext::default();
        trust.project_accounts.insert(account("proj"));
        trust
            .hosts
            .insert(account("proj"), "evil.example".to_string());
        trust.bindings.insert(
            "keyring:global/openai".into(),
            vec!["api.openai.com".into()],
        );
        let (backend, resolver) = resolver(trust);
        backend
            .data
            .lock()
            .unwrap()
            .insert("global/openai".into(), Secret::new("sk-real"));
        let error = resolver
            .resolve(&reference("keyring:global/openai"), &account("proj"))
            .unwrap_err();
        assert!(
            matches!(error, SecretError::HostNotTrusted { .. }),
            "{error}"
        );
        assert!(!error.to_string().contains("sk-real"));
    }

    #[test]
    fn a_confirmed_host_receives_the_key() {
        let mut trust = TrustContext::default();
        trust.project_accounts.insert(account("proj"));
        trust
            .hosts
            .insert(account("proj"), "api.openai.com".to_string());
        trust.bindings.insert(
            "keyring:global/openai".into(),
            vec!["API.openai.com".into()],
        );
        let (backend, resolver) = resolver(trust);
        backend
            .data
            .lock()
            .unwrap()
            .insert("global/openai".into(), Secret::new("sk-real"));
        assert!(
            resolver
                .resolve(&reference("keyring:global/openai"), &account("proj"))
                .is_ok()
        );
    }

    #[test]
    fn a_project_account_with_no_binding_at_all_is_refused_until_confirmed() {
        let mut trust = TrustContext::default();
        trust.project_accounts.insert(account("proj"));
        trust.hosts.insert(account("proj"), "h.example".to_string());
        let (backend, resolver) = resolver(trust);
        backend
            .data
            .lock()
            .unwrap()
            .insert("global/k".into(), Secret::new("v"));
        assert!(matches!(
            resolver.resolve(&reference("keyring:global/k"), &account("proj")),
            Err(SecretError::HostNotTrusted { .. })
        ));
    }

    #[test]
    fn a_global_account_is_only_checked_when_the_user_bound_the_secret() {
        let mut trust = TrustContext::default();
        trust.hosts.insert(account("mine"), "other.example".into());
        let (backend, resolver) = resolver(trust.clone());
        backend
            .data
            .lock()
            .unwrap()
            .insert("global/k".into(), Secret::new("v"));
        assert!(
            resolver
                .resolve(&reference("keyring:global/k"), &account("mine"))
                .is_ok(),
            "no binding: the user's own config is trusted"
        );
        trust
            .bindings
            .insert("keyring:global/k".into(), vec!["api.good.example".into()]);
        let (backend, resolver) = self::resolver(trust);
        backend
            .data
            .lock()
            .unwrap()
            .insert("global/k".into(), Secret::new("v"));
        assert!(
            resolver
                .resolve(&reference("keyring:global/k"), &account("mine"))
                .is_err(),
            "a binding is enforced even for the user's own accounts"
        );
    }

    #[test]
    fn a_secret_file_is_read_trimmed_and_a_missing_one_is_named() {
        let dir = std::env::temp_dir().join(format!("pooprusteek-res-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key.txt");
        std::fs::write(&path, "  file-secret \r\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let (_, resolver) = resolver(TrustContext::default());
        let raw = format!("file:{}", path.display());
        assert_eq!(
            resolver
                .resolve(&reference(&raw), &account("a"))
                .unwrap()
                .expose(),
            "file-secret"
        );
        let gone = format!("file:{}", dir.join("missing").display());
        assert!(matches!(
            resolver.resolve(&reference(&gone), &account("a")),
            Err(SecretError::NotFound { .. })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_command_output_is_the_secret_and_a_failing_command_is_unavailable() {
        let (_, resolver) = resolver(TrustContext::default());
        let ok = if cfg!(windows) {
            r#"cmd:["cmd","/C","echo cmd-secret"]"#
        } else {
            r#"cmd:["sh","-c","echo cmd-secret"]"#
        };
        assert_eq!(
            resolver
                .resolve(&reference(ok), &account("a"))
                .unwrap()
                .expose(),
            "cmd-secret"
        );
        let bad = if cfg!(windows) {
            r#"cmd:["cmd","/C","exit 3"]"#
        } else {
            r#"cmd:["sh","-c","exit 3"]"#
        };
        let (_, fresh) = (
            0,
            StoreResolver::new(Arc::new(store().1), TrustContext::default()),
        );
        assert!(
            fresh
                .resolve(&reference(bad), &account("a"))
                .unwrap_err()
                .is_unavailable()
        );
    }

    #[test]
    fn host_of_ignores_port_case_and_path() {
        assert_eq!(
            TrustContext::host_of("https://API.Example.com:8443/v1"),
            Some("api.example.com".into())
        );
        assert_eq!(TrustContext::host_of("not a url"), None);
    }

    /// Проектный аккаунт без адреса подтвердить нечем: ключ ему не выдаётся.
    #[test]
    fn a_project_account_without_an_address_gets_no_key() {
        let mut trust = TrustContext::default();
        trust.project_accounts.insert(account("proj"));
        let (backend, resolver) = resolver(trust);
        backend
            .data
            .lock()
            .unwrap()
            .insert("global/k".into(), Secret::new("v"));
        let error = resolver
            .resolve(&reference("keyring:global/k"), &account("proj"))
            .unwrap_err();
        assert!(matches!(error, SecretError::Refused { .. }), "{error}");
        // Аккаунт пользователя без адреса работает как раньше.
        assert!(
            resolver
                .resolve(&reference("keyring:global/k"), &account("mine"))
                .is_ok()
        );
    }

    /// Подтверждённый https не разрешает тот же хост по открытому http; явный http — разрешает.
    #[test]
    fn a_binding_covers_the_scheme_not_just_the_host() {
        let make = |origin: &str, binding: &str| {
            let mut trust = TrustContext::default();
            trust.project_accounts.insert(account("proj"));
            trust.hosts.insert(account("proj"), origin.to_string());
            trust
                .bindings
                .insert("keyring:global/k".into(), vec![binding.to_string()]);
            let (backend, resolver) = resolver(trust);
            backend
                .data
                .lock()
                .unwrap()
                .insert("global/k".into(), Secret::new("v"));
            resolver.resolve(&reference("keyring:global/k"), &account("proj"))
        };
        assert!(make("https://api.example.com", "api.example.com").is_ok());
        assert!(make("https://api.example.com", "https://API.example.com/").is_ok());
        assert!(
            matches!(
                make("http://api.example.com", "api.example.com"),
                Err(SecretError::HostNotTrusted { .. })
            ),
            "a bare host means https only"
        );
        assert!(make("http://localhost", "http://localhost").is_ok());
        assert_eq!(
            TrustContext::origin_of("HTTP://Api.Example.com:8080/v1").as_deref(),
            Some("http://api.example.com")
        );
    }
}
