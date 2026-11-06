//! Фабрика адаптеров `cli:*` для движка. Отсутствующий бинарь или старая версия не роняют
//! запуск: аккаунт становится `Unavailable` с причиной.

use super::adapter::{CliBackend, CliSetup};
use super::claude::{ClaudeDialect, MIN_VERSION};
use super::codex::CodexBackend;
use super::dialect::check_model;
use super::discover::{Lookup, find, read_version};
use super::opencode::OpencodeDialect;
use crate::gateway::backend::Backend;
use crate::gateway::engine::{BackendFactory, SecretResolver};
use crate::gateway::types::{Account, CliTool, Credential};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Забытые папки вызовов (аварийный выход) старше этого убираются при старте.
const STALE_SCRATCH: Duration = Duration::from_secs(24 * 3600);

pub struct CliFactory {
    /// Подмена поиска (тесты); `None` — окружение процесса.
    lookup: Option<Lookup>,
    scratch_root: PathBuf,
    keep_scratch: bool,
}

impl Default for CliFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl CliFactory {
    pub fn new() -> Self {
        Self {
            scratch_root: std::env::temp_dir().join("pooprusteek-cli"),
            lookup: None,
            keep_scratch: false,
        }
    }

    pub fn with_lookup(lookup: Lookup, scratch_root: PathBuf) -> Self {
        Self {
            lookup: Some(lookup),
            scratch_root,
            keep_scratch: false,
        }
    }

    /// Оставлять папки вызовов на диске (проверка приманки).
    #[cfg(test)]
    pub fn keeping_scratch(mut self) -> Self {
        self.keep_scratch = true;
        self
    }

    pub fn build_for(&self, account: &Account) -> Result<Arc<dyn Backend>, String> {
        let tool = account
            .provider
            .cli_tool()
            .ok_or_else(|| "not a cli account".to_string())?;
        let home = match account.credential()? {
            Credential::None => None,
            Credential::CliHome(path) => Some(expand_home(&path)),
            _ => return Err("a cli account takes `home`, not a secret or a key".to_string()),
        };
        if tool == CliTool::Codex {
            return Ok(Arc::new(CodexBackend));
        }
        if let Some(model) = &account.model {
            check_model(model).map_err(|reason| format!("account model: {reason}"))?;
        }
        // Забытые папки аварийных выходов убираем один раз за процесс, когда CLI реально нужен.
        static SWEPT: std::sync::Once = std::sync::Once::new();
        SWEPT.call_once(|| sweep_stale(&self.scratch_root));
        let lookup = self
            .lookup
            .clone()
            .unwrap_or_else(|| Lookup::from_env(tool));
        let program = find(tool, &lookup)?;
        let version = read_version(&program)?;
        let dialect: Arc<dyn super::dialect::Dialect> = match tool {
            CliTool::Claude => {
                if version < MIN_VERSION {
                    return Err(format!(
                        "Claude Code {version} is too old: {MIN_VERSION} or newer is needed (it adds --restricted); update it"
                    ));
                }
                Arc::new(ClaudeDialect)
            }
            _ => Arc::new(OpencodeDialect),
        };
        Ok(Arc::new(CliBackend::new(CliSetup {
            dialect,
            account: account.id.clone(),
            program,
            version: Some(version),
            home,
            default_model: account.model.clone(),
            scratch_root: self.scratch_root.clone(),
            keep_scratch: self.keep_scratch,
        })))
    }
}

impl BackendFactory for CliFactory {
    fn build(
        &self,
        account: &Account,
        _secrets: &dyn SecretResolver,
    ) -> Option<Result<Arc<dyn Backend>, String>> {
        account.provider.cli_tool()?;
        Some(self.build_for(account))
    }
}

/// `~` и `~/…` в пути папки входа.
fn expand_home(path: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => dirs::home_dir().map_or_else(|| path.to_path_buf(), |home| home.join(rest)),
        Err(_) => path.to_path_buf(),
    }
}

fn sweep_stale(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|at| SystemTime::now().duration_since(at).ok())
            .is_some_and(|age| age > STALE_SCRATCH);
        if old {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tilde_home_is_expanded_and_other_paths_are_kept() {
        let expanded = expand_home(Path::new("~/.claude-work"));
        if let Some(home) = dirs::home_dir() {
            assert_eq!(expanded, home.join(".claude-work"));
        }
        assert_eq!(expand_home(Path::new("D:/x/y")), PathBuf::from("D:/x/y"));
    }
}
