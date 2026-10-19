//! Owns every configured MCP server: connecting, reconnecting, caching tool
//! lists, routing tool calls, and persisting user overrides.
//!
//! Two structural things worth knowing before reading the rest of this file:
//!
//! - Every server's client is `Arc<MCPClient>`. That's what lets
//!   `client_for` hand a caller (the agent loop) a handle it can run a tool
//!   call through *without* holding `MCPManager`'s own lock — previously the
//!   whole `Mutex<MCPManager>` was held across the network await, which
//!   froze the UI event loop for the duration of every MCP tool call.
//!   Сбои транспорта возвращаются через `mark_broken`.
//! - Each entry remembers a [`ServerSource`]: `Own` (from our `mcp.json`) or
//!   `Foreign` (discovered from another tool's config). `persist_config`
//!   uses this to avoid writing foreign servers' full config — including
//!   their `env` secrets — into our own file; see its doc comment.
//! - Методы `MCPManager` — только состояние под замком, без сетевых `await`.
//!   Операции `/mcp` с I/O — свободные функции внизу файла (`reload`, `add`…).

use super::client::MCPClient;
use super::config::{load_mcp_config, load_overrides, load_own_enabled_map, save_mcp_config};
use super::types::*;
use crate::error::AppResult;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

/// Куда идёт вызов MCP-инструмента.
pub struct McpTarget {
    pub client: Arc<MCPClient>,
    pub server: String,
    pub tool: String,
}

/// Почему вызов некуда отправить. Раньше обе причины звучали как «server not
/// connected», и опечатку в имени модель чинила переподключением.
#[derive(Debug, Clone, PartialEq)]
pub enum McpUnavailable {
    UnknownTool,
    NotConnected {
        server: String,
        status: MCPServerStatus,
    },
}

/// Сколько сбоев транспорта подряд, чтобы считать сервер сломанным.
pub const BROKEN_AFTER_FAILURES: u32 = 3;

impl McpUnavailable {
    /// Объяснение для модели: что случилось и что делать дальше.
    pub fn explain(&self, tool: &str) -> String {
        match self {
            Self::UnknownTool => {
                format!("Unknown MCP tool '{tool}'. Use tool_search to find the right name.")
            }
            Self::NotConnected { server, status } => {
                let state = match status {
                    MCPServerStatus::Disabled => "is disabled by the user".to_string(),
                    MCPServerStatus::AuthRequired(_) => {
                        "needs authorization — ask the user to run /mcp auth".to_string()
                    }
                    MCPServerStatus::Error(error) => {
                        format!("is broken ({error}) — ask the user to reconnect it with /mcp")
                    }
                    MCPServerStatus::Pending | MCPServerStatus::Connecting => {
                        "is still connecting — try again shortly".to_string()
                    }
                    MCPServerStatus::Connected => "is connected".to_string(),
                };
                format!("MCP tool '{tool}' is not available: server '{server}' {state}.")
            }
        }
    }
}

/// Determine a discovered server's enabled state and [`ServerSource`].
///
/// `own_enabled` is the key set of our own `mcpServers` section
/// (`load_own_enabled_map`) — a name present there is `ServerSource::Own`,
/// full stop, regardless of what other tool might coincidentally also
/// define a server with that name (our own config always wins, matching
/// `load_mcp_config`'s discovery order). Everything else is
/// `ServerSource::Foreign`; its enabled state comes from `foreign_overrides`
/// (`load_overrides`) if the user has toggled it in-app before, defaulting
/// to enabled otherwise (matches prior behavior, where every discovered
/// server defaulted to enabled).
fn resolve_enabled_and_source(
    name: &str,
    own_enabled: &HashMap<String, bool>,
    foreign_overrides: &HashMap<String, bool>,
) -> (bool, ServerSource) {
    if let Some(&enabled) = own_enabled.get(name) {
        (enabled, ServerSource::Own)
    } else {
        let enabled = foreign_overrides.get(name).copied().unwrap_or(true);
        (enabled, ServerSource::Foreign)
    }
}

struct ServerCache {
    tools: Vec<MCPTool>,
    resources: Vec<MCPResource>,
    tool_entries: Vec<(String, String, String)>, // (full_name, server_name, tool_name)
    cached_at: Instant,
}

pub struct MCPManager {
    servers: HashMap<String, MCPServerEntry>,
    tool_name_map: HashMap<String, (String, String)>,
    cache: HashMap<String, ServerCache>,
    cache_ttl: u64,
}

struct MCPServerEntry {
    client: Arc<MCPClient>,
    config: MCPServerConfig,
    status: MCPServerStatus,
    tools: Vec<MCPTool>,
    resources: Vec<MCPResource>,
    enabled: bool,
    source: ServerSource,
}

impl MCPServerEntry {
    fn new(
        client: Arc<MCPClient>,
        config: MCPServerConfig,
        source: ServerSource,
        status: MCPServerStatus,
    ) -> Self {
        Self {
            client,
            config,
            enabled: !matches!(status, MCPServerStatus::Disabled),
            status,
            tools: Vec::new(),
            resources: Vec::new(),
            source,
        }
    }

    /// Включённый сервер с готовым клиентом, ждёт подключения.
    fn pending(client: Arc<MCPClient>, config: MCPServerConfig, source: ServerSource) -> Self {
        Self::new(client, config, source, MCPServerStatus::Pending)
    }

    /// Выключенный сервер: вместо клиента заглушка.
    fn disabled(name: &str, config: MCPServerConfig, source: ServerSource) -> Self {
        let client = Arc::new(MCPClient::dummy(name));
        Self::new(client, config, source, MCPServerStatus::Disabled)
    }

    /// Включённый сервер, клиент которого не собрался: видно в `/mcp`, можно `r`.
    fn failed(name: &str, config: MCPServerConfig, source: ServerSource, error: String) -> Self {
        let client = Arc::new(MCPClient::dummy(name));
        Self::new(client, config, source, MCPServerStatus::Error(error))
    }
}

/// Снимок кэша сервера, взятый под замком: `connect_one` идёт без менеджера.
struct CachedSnapshot {
    tools: Vec<MCPTool>,
    resources: Vec<MCPResource>,
    tool_entries: Vec<(String, String, String)>,
    age_secs: u64,
}

/// Итог `connect_one` для одного сервера; сливается `apply_connect_outcome`.
struct ConnectOutcome {
    name: String,
    /// Клиент, который подключали: итог чужого клиента не сливается.
    client: Arc<MCPClient>,
    status: MCPServerStatus,
    tools: Vec<MCPTool>,
    resources: Vec<MCPResource>,
    tool_entries: Vec<(String, String, String)>,
    /// Инструменты взяты из кэша, а не свежим `list_tools` — кэш не обновлять.
    used_cache: bool,
}

/// Handshake и список инструментов (или кэш) для одного сервера. Не метод:
/// идёт без замка менеджера, и `join_all` гоняет много таких сразу.
async fn connect_one(
    name: String,
    client: Arc<MCPClient>,
    cached: Option<CachedSnapshot>,
    cache_ttl: u64,
) -> ConnectOutcome {
    let empty = |status: MCPServerStatus| ConnectOutcome {
        name: name.clone(),
        client: Arc::clone(&client),
        status,
        tools: Vec::new(),
        resources: Vec::new(),
        tool_entries: Vec::new(),
        used_cache: false,
    };

    if let Err(e) = client.initialize().await {
        if let crate::error::AppError::McpAuthRequired { www_authenticate } = &e {
            let hint = www_authenticate
                .as_deref()
                .and_then(super::oauth::extract_resource_metadata);
            tracing::info!("MCP '{name}' requires authorization");
            return empty(MCPServerStatus::AuthRequired(hint));
        }
        tracing::warn!("MCP '{name}' initialize failed: {e}");
        return empty(MCPServerStatus::Error(e.to_string()));
    }
    if let Err(e) = client.send_initialized().await {
        tracing::warn!("MCP '{name}' initialized but send_initialized failed: {e}");
        return empty(MCPServerStatus::Error(e.to_string()));
    }

    if let Some(cache) = cached {
        if cache.age_secs < cache_ttl {
            tracing::info!(
                "MCP '{name}' using cache ({}s old, TTL {cache_ttl})",
                cache.age_secs
            );
            return ConnectOutcome {
                name,
                client,
                status: MCPServerStatus::Connected,
                tools: cache.tools,
                resources: cache.resources,
                tool_entries: cache.tool_entries,
                used_cache: true,
            };
        }
        tracing::info!(
            "MCP '{name}' cache expired ({}s >= TTL {cache_ttl}s)",
            cache.age_secs
        );
    }

    // Fetch from server. A failed `list_tools` used to still fall through to
    // `Connected` below — silently reporting a broken server as healthy with
    // zero tools. Treat it as a connection failure instead; `list_resources`
    // is best-effort/optional either way (many servers don't implement it).
    let tools = match client.list_tools().await {
        Ok(tools) => tools,
        Err(e) => {
            tracing::warn!("MCP '{name}' list_tools failed: {e}");
            return empty(MCPServerStatus::Error(format!("list_tools failed: {e}")));
        }
    };

    let mut tool_entries = Vec::new();
    for tool in &tools {
        let full_name = format!("{}{}__{}", crate::mcp::MCP_TOOL_PREFIX, name, tool.name);
        tool_entries.push((full_name, name.clone(), tool.name.clone()));
    }

    let resources = match client.list_resources().await {
        Ok(r) => {
            tracing::info!("MCP '{name}' fetched {} resources", r.len());
            r
        }
        Err(e) => {
            tracing::info!("MCP '{name}' list_resources (optional): {e}");
            Vec::new()
        }
    };

    tracing::info!("MCP '{name}' connected with {} tools", tools.len());
    ConnectOutcome {
        name,
        client,
        status: MCPServerStatus::Connected,
        tools,
        resources,
        tool_entries,
        used_cache: false,
    }
}

impl MCPManager {
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
            tool_name_map: HashMap::new(),
            cache: HashMap::new(),
            cache_ttl: 300,
        }
    }

    pub fn set_cache_ttl(&mut self, ttl: u64) {
        self.cache_ttl = ttl;
    }

    /// Закрыть транспорты всех серверов (у stdio — убить процесс).
    async fn close_all_clients(&self) {
        for (name, entry) in &self.servers {
            if let Err(e) = entry.client.close().await {
                tracing::debug!("MCP '{name}' close failed (best-effort): {e}");
            }
        }
    }

    /// Погасить все серверы — на выходе из приложения и в конце `exec`.
    pub async fn shutdown_all(&mut self) {
        self.close_all_clients().await;
    }

    /// Clone a read-only snapshot of `name`'s cache entry, if any and not
    /// yet checked for TTL — the age is computed here so the snapshot is
    /// self-contained (no `Instant` comparison needed later).
    fn cache_snapshot(&self, name: &str) -> Option<CachedSnapshot> {
        self.cache.get(name).map(|c| CachedSnapshot {
            tools: c.tools.clone(),
            resources: c.resources.clone(),
            tool_entries: c.tool_entries.clone(),
            age_secs: c.cached_at.elapsed().as_secs(),
        })
    }

    /// Слить итог `connect_one`: карта инструментов, кэш (только свежий удачный
    /// список) и состояние сервера. Чужой итог отбрасывается — см. `owns`.
    fn apply_connect_outcome(&mut self, outcome: ConnectOutcome) {
        // Пока шло подключение без замка, сервер могли удалить, выключить или
        // переподключить: слить такой итог значило бы воскресить старый клиент.
        if !self.owns(&outcome.name, &outcome.client) {
            return;
        }
        if outcome.used_cache || matches!(outcome.status, MCPServerStatus::Connected) {
            for (full_name, server_name, tool_name) in &outcome.tool_entries {
                self.tool_name_map
                    .insert(full_name.clone(), (server_name.clone(), tool_name.clone()));
            }
            if !outcome.used_cache {
                self.cache.insert(
                    outcome.name.clone(),
                    ServerCache {
                        tools: outcome.tools.clone(),
                        resources: outcome.resources.clone(),
                        tool_entries: outcome.tool_entries.clone(),
                        cached_at: Instant::now(),
                    },
                );
            }
        }

        if let Some(entry) = self.servers.get_mut(&outcome.name) {
            entry.status = outcome.status;
            entry.tools = outcome.tools;
            entry.resources = outcome.resources;
        }
    }

    /// Сервер `name` включён и стоит на клиенте `client`, а не на чужом.
    fn owns(&self, name: &str, client: &Arc<MCPClient>) -> bool {
        self.servers
            .get(name)
            .is_some_and(|entry| entry.enabled && Arc::ptr_eq(&entry.client, client))
    }

    /// Убрать инструменты сервера из карты имён.
    fn forget_tools(&mut self, name: &str) {
        self.tool_name_map.retain(|_, (server, _)| server != name);
    }

    /// Включить сервер и пометить `Pending`; вернуть конфиг и старый клиент,
    /// чтобы собрать новый без замка.
    fn begin_connect(&mut self, name: &str) -> AppResult<(MCPServerConfig, Arc<MCPClient>)> {
        let entry = self.servers.get_mut(name).ok_or_else(|| not_found(name))?;
        entry.enabled = true;
        entry.status = MCPServerStatus::Pending;
        entry.tools.clear();
        entry.resources.clear();
        let taken = (entry.config.clone(), Arc::clone(&entry.client));
        self.forget_tools(name);
        Ok(taken)
    }

    /// Выключить сервер: заглушка вместо клиента; вернуть старый клиент.
    fn disable(&mut self, name: &str) -> AppResult<Arc<MCPClient>> {
        let entry = self.servers.get_mut(name).ok_or_else(|| not_found(name))?;
        entry.enabled = false;
        entry.status = MCPServerStatus::Disabled;
        entry.tools.clear();
        entry.resources.clear();
        let old = std::mem::replace(&mut entry.client, Arc::new(MCPClient::dummy(name)));
        self.forget_tools(name);
        Ok(old)
    }

    /// Переключить сервер одним шагом под замком: два быстрых нажатия дают
    /// «выкл, вкл», а не два включения.
    fn flip(&mut self, name: &str) -> AppResult<Flip> {
        let enabled = self
            .servers
            .get(name)
            .ok_or_else(|| not_found(name))?
            .enabled;
        if enabled {
            self.disable(name).map(Flip::Disabled)
        } else {
            let (config, old) = self.begin_connect(name)?;
            Ok(Flip::Enabling(config, old))
        }
    }

    /// Забрать запись сервера целиком; клиент закрывает вызывающий.
    fn take_server(&mut self, name: &str) -> AppResult<Arc<MCPClient>> {
        let entry = self.servers.remove(name).ok_or_else(|| not_found(name))?;
        self.forget_tools(name);
        self.cache.remove(name);
        Ok(entry.client)
    }

    /// Хэндл клиента для вызова инструмента без замка менеджера (замок через
    /// сетевой `await` однажды заморозил весь UI). Отказ называет причину.
    pub fn client_for(&self, full_tool_name: &str) -> Result<McpTarget, McpUnavailable> {
        let (server_name, tool_name) = self
            .tool_name_map
            .get(full_tool_name)
            .ok_or(McpUnavailable::UnknownTool)?;
        let entry = self
            .servers
            .get(server_name)
            .ok_or(McpUnavailable::UnknownTool)?;
        if !matches!(entry.status, MCPServerStatus::Connected) {
            return Err(McpUnavailable::NotConnected {
                server: server_name.clone(),
                status: entry.status.clone(),
            });
        }
        Ok(McpTarget {
            client: Arc::clone(&entry.client),
            server: server_name.clone(),
            tool: tool_name.clone(),
        })
    }

    /// Сервер, чей вызов падал [`BROKEN_AFTER_FAILURES`] раз подряд, больше не
    /// «подключён». Только тот же клиент: переподключённый новый не трогаем.
    pub fn mark_broken(&mut self, target: &McpTarget, reason: &str) {
        let Some(entry) = self.servers.get_mut(&target.server) else {
            return;
        };
        if Arc::ptr_eq(&entry.client, &target.client)
            && matches!(entry.status, MCPServerStatus::Connected)
        {
            entry.status = MCPServerStatus::Error(reason.to_string());
            tracing::warn!("MCP '{}' marked broken: {reason}", target.server);
        }
    }

    pub fn get_all_resources(&self) -> Vec<MCPResource> {
        let mut result = Vec::new();
        for entry in self.servers.values() {
            result.extend(entry.resources.clone());
        }
        // Порядок `HashMap` меняется при переподключении, а промпт из этого
        // списка обязан быть стабильным: DeepSeek досылает его при любом отличии.
        result.sort_by(|a, b| a.uri.cmp(&b.uri));
        result
    }

    pub fn get_all_tools(&self) -> Vec<FullMCPTool> {
        let mut result = Vec::new();
        for (full_name, (server_name, tool_name)) in &self.tool_name_map {
            if let Some(entry) = self.servers.get(server_name)
                && let Some(tool) = entry.tools.iter().find(|t| t.name == *tool_name)
            {
                result.push(FullMCPTool {
                    full_name: full_name.clone(),
                    tool: tool.clone(),
                });
            }
        }
        // Та же причина, что у ресурсов: стабильный промпт.
        result.sort_by(|a, b| a.full_name.cmp(&b.full_name));
        result
    }

    pub fn get_servers_info(&self) -> Vec<ServerDisplayInfo> {
        let mut list: Vec<ServerDisplayInfo> = self
            .servers
            .iter()
            .map(|(name, entry)| {
                let transport = match &entry.config {
                    MCPServerConfig::Stdio { .. } => "stdio",
                    MCPServerConfig::Http { .. } => "http",
                    MCPServerConfig::Sse { .. } => "sse",
                }
                .to_string();
                ServerDisplayInfo {
                    name: name.clone(),
                    transport,
                    status: entry.status.label(),
                    tool_count: entry.tools.len(),
                    resource_count: entry.resources.len(),
                    enabled: entry.enabled,
                    needs_auth: matches!(entry.status, MCPServerStatus::AuthRequired(_)),
                }
            })
            .collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }

    /// The config + discovery hint needed to start an OAuth flow for `name`
    /// (`/mcp auth`'s Enter handler) — `None` if the server isn't known or
    /// isn't currently `AuthRequired`.
    pub fn oauth_context(&self, name: &str) -> Option<(MCPServerConfig, Option<String>)> {
        let entry = self.servers.get(name)?;
        match &entry.status {
            MCPServerStatus::AuthRequired(hint) => Some((entry.config.clone(), hint.clone())),
            _ => None,
        }
    }

    /// Whether a server named `name` is already configured — used by
    /// `/mcp add` to reject a duplicate before even attempting to connect.
    pub fn has_server(&self, name: &str) -> bool {
        self.servers.contains_key(name)
    }

    /// Persist enabled/disabled state (and, for our own servers, full
    /// config) to `mcp.json`.
    ///
    /// Splits servers by `ServerSource` before handing off to
    /// `save_mcp_config`: `Own` servers go into `mcpServers` in full (same
    /// as before); `Foreign` servers — discovered from Claude Desktop, VS
    /// Code, Cursor, opencode, or the Claude CLI's own config files —
    /// contribute only their enabled bool to the `overrides` section.
    ///
    /// Previously every discovered server, foreign or not, was written into
    /// `mcpServers` in full on every single toggle — meaning any server
    /// from e.g. Claude Desktop's config (command, args, and any secrets in
    /// its `env` map) ended up duplicated into our own file the moment the
    /// user toggled *any* server in the UI, foreign or not.
    fn persist_config(&self) {
        let mut own_servers = HashMap::new();
        let mut foreign_overrides = HashMap::new();
        for (name, entry) in &self.servers {
            match entry.source {
                ServerSource::Own => {
                    own_servers.insert(name.clone(), (entry.config.clone(), entry.enabled));
                }
                ServerSource::Foreign => {
                    foreign_overrides.insert(name.clone(), entry.enabled);
                }
            }
        }
        if let Err(e) = save_mcp_config(&own_servers, &foreign_overrides) {
            tracing::warn!("Failed to save MCP config: {e}");
        }
    }
}

pub struct FullMCPTool {
    pub full_name: String,
    pub tool: MCPTool,
}

/// Итог `MCPManager::flip`: что доделать уже без замка.
enum Flip {
    /// Сервер выключен; старый клиент надо закрыть.
    Disabled(Arc<MCPClient>),
    /// Сервер включён; собрать клиент по конфигу вместо старого.
    Enabling(MCPServerConfig, Arc<MCPClient>),
}

fn not_found(name: &str) -> crate::error::AppError {
    crate::error::AppError::Mcp(format!("Server '{name}' not found"))
}

/// Закрыть клиент, ошибку только записать в лог: закрытие — по мере сил.
async fn close_quietly(client: &MCPClient, name: &str) {
    if let Err(e) = client.close().await {
        tracing::debug!("MCP '{name}' close failed (best-effort): {e}");
    }
}

// ─── Операции `/mcp`: короткий замок, сеть без замка, короткий замок ──────
// Замок менеджера ждёт и цикл событий, поэтому закрытие и подключение — вне его.

/// Найти серверы в конфигах и подключить в четыре фазы: сборка и подключение
/// без замка, вставка записей и слияние итогов — короткими замками без `await`.
pub async fn startup_initialize(mcp: &tokio::sync::Mutex<MCPManager>) {
    let configs = load_mcp_config();
    let own_enabled = load_own_enabled_map();
    let foreign_overrides = load_overrides();

    // Фаза 1: клиенты собираются параллельно, без замка.
    let jobs: Vec<_> = configs
        .into_iter()
        .map(|(name, config)| {
            let (enabled, source) =
                resolve_enabled_and_source(&name, &own_enabled, &foreign_overrides);
            async move {
                let client = if enabled {
                    Some(build_client(&name, &config).await)
                } else {
                    None
                };
                (name, config, source, client)
            }
        })
        .collect();
    let built = futures::future::join_all(jobs).await;

    // Фаза 2: записи (`/mcp` видит их сразу) и входы для подключения.
    let (inputs, cache_ttl, unused) = {
        let mut manager = mcp.lock().await;
        let mut inputs = Vec::new();
        let mut unused = Vec::new();
        for (name, config, source, client) in built {
            // Сервер успел добавить `/mcp add` или другой `reload`: его запись
            // свежее, наш клиент лишний.
            if manager.has_server(&name) {
                unused.extend(client.and_then(Result::ok).map(|c| (name, Arc::new(c))));
                continue;
            }
            let entry = match client {
                None => MCPServerEntry::disabled(&name, config, source),
                Some(Err(error)) => {
                    tracing::warn!("Failed to create MCP client for '{name}': {error}");
                    MCPServerEntry::failed(&name, config, source, error.to_string())
                }
                Some(Ok(client)) => {
                    let client = Arc::new(client);
                    let cached = manager.cache_snapshot(&name);
                    inputs.push((name.clone(), Arc::clone(&client), cached));
                    MCPServerEntry::pending(client, config, source)
                }
            };
            manager.servers.insert(name, entry);
        }
        (inputs, manager.cache_ttl, unused)
    };
    futures::future::join_all(unused.iter().map(|(name, c)| close_quietly(c, name))).await;

    // Фаза 3: все подключения параллельно, без замка.
    let jobs = inputs
        .into_iter()
        .map(|(name, client, cached)| connect_one(name, client, cached, cache_ttl));
    let outcomes = futures::future::join_all(jobs).await;

    // Фаза 4: слияние; удалённые и переподключённые за это время пропускаются.
    let mut manager = mcp.lock().await;
    for outcome in outcomes {
        manager.apply_connect_outcome(outcome);
    }
}

/// Перечитать конфиг и переподключить все серверы.
pub async fn reload(mcp: &tokio::sync::Mutex<MCPManager>) {
    let old: Vec<(String, Arc<MCPClient>)> = {
        let mut manager = mcp.lock().await;
        manager.cache.clear();
        manager.tool_name_map.clear();
        manager
            .servers
            .drain()
            .map(|(name, entry)| (name, entry.client))
            .collect()
    };
    futures::future::join_all(old.iter().map(|(name, c)| close_quietly(c, name))).await;
    startup_initialize(mcp).await;
}

/// Переподключить один сервер (и включить, если был выключен).
pub async fn reconnect(mcp: &tokio::sync::Mutex<MCPManager>, name: &str) -> AppResult<()> {
    let (config, old) = {
        let mut manager = mcp.lock().await;
        let taken = manager.begin_connect(name)?;
        manager.persist_config();
        taken
    };
    rebuild(mcp, name, config, old).await
}

/// Включить или выключить сервер; вернуть, включён ли он теперь.
pub async fn toggle(mcp: &tokio::sync::Mutex<MCPManager>, name: &str) -> AppResult<bool> {
    let flip = {
        let mut manager = mcp.lock().await;
        let flip = manager.flip(name)?;
        manager.persist_config();
        flip
    };
    match flip {
        Flip::Disabled(old) => {
            close_quietly(&old, name).await;
            Ok(false)
        }
        Flip::Enabling(config, old) => rebuild(mcp, name, config, old).await.map(|()| true),
    }
}

/// Удалить сервер из менеджера и конфига; клиент закрывается уже без замка.
pub async fn remove(mcp: &tokio::sync::Mutex<MCPManager>, name: &str) -> AppResult<()> {
    let client = {
        let mut manager = mcp.lock().await;
        let client = manager.take_server(name)?;
        manager.persist_config();
        client
    };
    close_quietly(&client, name).await;
    Ok(())
}

/// Добавить свой сервер (из `/mcp add`) и подключить; вернуть его состояние.
pub async fn add(
    mcp: &tokio::sync::Mutex<MCPManager>,
    name: String,
    config: MCPServerConfig,
) -> AppResult<MCPServerStatus> {
    let duplicate = || crate::error::AppError::Mcp(format!("Server '{name}' already exists"));
    if mcp.lock().await.has_server(&name) {
        return Err(duplicate());
    }
    let client = Arc::new(build_client(&name, &config).await.map_err(|error| {
        crate::error::AppError::Mcp(format!("Failed to create a client for '{name}': {error}"))
    })?);
    let inserted = {
        let mut manager = mcp.lock().await;
        let free = !manager.has_server(&name);
        if free {
            let entry = MCPServerEntry::pending(Arc::clone(&client), config, ServerSource::Own);
            manager.servers.insert(name.clone(), entry);
            // Сразу на диск: параллельный `reload` перечитает конфиг и не потеряет сервер.
            manager.persist_config();
        }
        free
    };
    if !inserted {
        close_quietly(&client, &name).await;
        return Err(duplicate());
    }
    connect_installed(mcp, &name, Arc::clone(&client), &client).await;
    Ok(mcp
        .lock()
        .await
        .servers
        .get(&name)
        .map(|entry| entry.status.clone())
        .unwrap_or(MCPServerStatus::Pending))
}

/// Закрыть старый клиент, собрать новый по `config` и подключить без замка.
/// Не собрался — `Error` в записи, если её не успел перехватить другой вызов.
async fn rebuild(
    mcp: &tokio::sync::Mutex<MCPManager>,
    name: &str,
    config: MCPServerConfig,
    old: Arc<MCPClient>,
) -> AppResult<()> {
    close_quietly(&old, name).await;
    match build_client(name, &config).await {
        Ok(client) => {
            connect_installed(mcp, name, Arc::new(client), &old).await;
            Ok(())
        }
        Err(error) => {
            let mut manager = mcp.lock().await;
            if manager.owns(name, &old)
                && let Some(entry) = manager.servers.get_mut(name)
            {
                entry.status = MCPServerStatus::Error(error.to_string());
            }
            Err(error)
        }
    }
}

/// Поставить новый клиент вместо `previous`, подключить без замка и слить итог.
/// Сервер успели удалить, выключить или переподключить — наш клиент закрывается.
async fn connect_installed(
    mcp: &tokio::sync::Mutex<MCPManager>,
    name: &str,
    client: Arc<MCPClient>,
    previous: &Arc<MCPClient>,
) {
    let cache_ttl = {
        let mut manager = mcp.lock().await;
        let ttl = manager.cache_ttl;
        match manager.servers.get_mut(name) {
            Some(entry) if entry.enabled && Arc::ptr_eq(&entry.client, previous) => {
                entry.client = Arc::clone(&client);
                Some(ttl)
            }
            _ => None,
        }
    };
    let Some(cache_ttl) = cache_ttl else {
        close_quietly(&client, name).await;
        return;
    };
    // Кэш не берём: переподключают, когда хотят свежий список инструментов.
    let outcome = connect_one(name.to_string(), client, None, cache_ttl).await;
    mcp.lock().await.apply_connect_outcome(outcome);
}

/// Собрать `MCPClient` под транспорт из `config`. Для `Http`/`Sse` подмешать
/// сохранённый `/mcp auth` заголовок `Authorization` (нет токена — без него).
async fn build_client(name: &str, config: &MCPServerConfig) -> AppResult<MCPClient> {
    match config {
        MCPServerConfig::Stdio {
            command,
            args,
            env,
            cwd,
        } => MCPClient::from_stdio(name, command, args, env.as_ref(), cwd.as_deref()).await,
        MCPServerConfig::Http { url, headers } => {
            let headers = super::oauth_store::with_bearer_header(name, headers.clone()).await;
            MCPClient::from_http(name, url, headers).await
        }
        MCPServerConfig::Sse { url, headers } => {
            let headers = super::oauth_store::with_bearer_header(name, headers.clone()).await;
            MCPClient::from_sse(name, url, headers).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Незнакомое имя — не «сервер не подключён»: модель чинила бы не то.
    #[test]
    fn an_unknown_mcp_tool_is_not_blamed_on_the_connection() {
        let manager = MCPManager::new();
        assert!(matches!(
            manager.client_for("mcp__nope__tool"),
            Err(McpUnavailable::UnknownTool)
        ));
    }

    #[test]
    fn resolve_enabled_and_source_own_server_uses_own_map() {
        let mut own = HashMap::new();
        own.insert("our-server".to_string(), false);
        let foreign = HashMap::new();

        let (enabled, source) = resolve_enabled_and_source("our-server", &own, &foreign);
        assert!(!enabled);
        assert_eq!(source, ServerSource::Own);
    }

    #[test]
    fn resolve_enabled_and_source_foreign_server_uses_override_map() {
        let own = HashMap::new();
        let mut foreign = HashMap::new();
        foreign.insert("claude-desktop-server".to_string(), false);

        let (enabled, source) = resolve_enabled_and_source("claude-desktop-server", &own, &foreign);
        assert!(!enabled);
        assert_eq!(source, ServerSource::Foreign);
    }

    #[test]
    fn resolve_enabled_and_source_foreign_server_defaults_enabled_without_override() {
        let own = HashMap::new();
        let foreign = HashMap::new();

        let (enabled, source) = resolve_enabled_and_source("never-toggled-server", &own, &foreign);
        assert!(
            enabled,
            "undiscovered foreign servers should default to enabled, matching prior behavior"
        );
        assert_eq!(source, ServerSource::Foreign);
    }

    #[test]
    fn resolve_enabled_and_source_own_takes_priority_over_foreign_override() {
        // A name present in both maps (e.g. the user's own mcp.json happens
        // to reuse a name Claude Desktop also uses) must resolve as Own —
        // matching `load_mcp_config`'s discovery order, where our own
        // config is loaded first and wins via `entry().or_insert()`.
        let mut own = HashMap::new();
        own.insert("shared-name".to_string(), true);
        let mut foreign = HashMap::new();
        foreign.insert("shared-name".to_string(), false);

        let (enabled, source) = resolve_enabled_and_source("shared-name", &own, &foreign);
        assert!(enabled);
        assert_eq!(source, ServerSource::Own);
    }

    /// Транспорт без сервера: запросы падают, `close` отмечается во флаге.
    struct Offline(Arc<std::sync::atomic::AtomicBool>);

    #[async_trait::async_trait]
    impl crate::mcp::transport::Transport for Offline {
        async fn send_request(
            &mut self,
            _request: &crate::mcp::jsonrpc::JsonRpcRequest,
        ) -> AppResult<crate::mcp::jsonrpc::JsonRpcResponse> {
            Err(crate::error::AppError::Mcp("offline".to_string()))
        }
        async fn send_raw(&mut self, _data: &[u8]) -> AppResult<()> {
            Err(crate::error::AppError::Mcp("offline".to_string()))
        }
        async fn close(&mut self) -> AppResult<()> {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    /// Клиент и флаг «его закрыли».
    fn offline_client() -> (Arc<MCPClient>, Arc<std::sync::atomic::AtomicBool>) {
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let client = MCPClient::from_transport("srv", Offline(Arc::clone(&closed)));
        (Arc::new(client), closed)
    }

    fn config() -> MCPServerConfig {
        MCPServerConfig::Stdio {
            command: "never-run".to_string(),
            args: Vec::new(),
            env: None,
            cwd: None,
        }
    }

    fn manager_with(entry: MCPServerEntry) -> tokio::sync::Mutex<MCPManager> {
        let mut manager = MCPManager::new();
        manager.servers.insert("srv".to_string(), entry);
        tokio::sync::Mutex::new(manager)
    }

    fn is_closed(flag: &std::sync::atomic::AtomicBool) -> bool {
        flag.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Запись всё ещё на `previous` — наш клиент встаёт и его итог сливается.
    #[tokio::test]
    async fn connect_installed_installs_and_merges_its_own_client() {
        let previous = Arc::new(MCPClient::dummy("srv"));
        let mcp = manager_with(MCPServerEntry::pending(
            Arc::clone(&previous),
            config(),
            ServerSource::Own,
        ));
        let (ours, closed) = offline_client();

        connect_installed(&mcp, "srv", Arc::clone(&ours), &previous).await;

        let manager = mcp.lock().await;
        let entry = &manager.servers["srv"];
        assert!(Arc::ptr_eq(&entry.client, &ours));
        assert!(matches!(entry.status, MCPServerStatus::Error(_)));
        assert!(!is_closed(&closed));
    }

    /// Другой вызов уже поставил свой клиент: наш закрыт, чужое не тронуто.
    #[tokio::test]
    async fn connect_installed_closes_its_client_when_the_entry_was_replaced() {
        let newer = Arc::new(MCPClient::dummy("srv"));
        let mut entry = MCPServerEntry::pending(Arc::clone(&newer), config(), ServerSource::Own);
        entry.status = MCPServerStatus::Connected;
        let mcp = manager_with(entry);
        let (ours, closed) = offline_client();
        let previous = Arc::new(MCPClient::dummy("srv"));

        connect_installed(&mcp, "srv", ours, &previous).await;

        assert!(is_closed(&closed));
        let manager = mcp.lock().await;
        let entry = &manager.servers["srv"];
        assert!(Arc::ptr_eq(&entry.client, &newer));
        assert_eq!(entry.status, MCPServerStatus::Connected);
    }

    #[tokio::test]
    async fn connect_installed_closes_its_client_when_the_server_was_disabled() {
        let mcp = manager_with(MCPServerEntry::disabled("srv", config(), ServerSource::Own));
        let previous = Arc::clone(&mcp.lock().await.servers["srv"].client);
        let (ours, closed) = offline_client();

        connect_installed(&mcp, "srv", ours, &previous).await;

        assert!(is_closed(&closed));
        let manager = mcp.lock().await;
        let entry = &manager.servers["srv"];
        assert!(Arc::ptr_eq(&entry.client, &previous));
        assert_eq!(entry.status, MCPServerStatus::Disabled);
    }

    #[tokio::test]
    async fn connect_installed_closes_its_client_when_the_server_was_removed() {
        let mcp = tokio::sync::Mutex::new(MCPManager::new());
        let (ours, closed) = offline_client();
        let previous = Arc::new(MCPClient::dummy("srv"));

        connect_installed(&mcp, "srv", ours, &previous).await;

        assert!(is_closed(&closed));
        assert!(!mcp.lock().await.has_server("srv"));
    }

    /// Итог клиента, которого в записи уже нет, не перетирает свежее состояние.
    #[test]
    fn an_outcome_of_a_replaced_client_is_not_merged() {
        let installed = Arc::new(MCPClient::dummy("srv"));
        let mut manager = MCPManager::new();
        manager.servers.insert(
            "srv".to_string(),
            MCPServerEntry::pending(installed, config(), ServerSource::Own),
        );

        manager.apply_connect_outcome(ConnectOutcome {
            name: "srv".to_string(),
            client: Arc::new(MCPClient::dummy("srv")),
            status: MCPServerStatus::Connected,
            tools: Vec::new(),
            resources: Vec::new(),
            tool_entries: vec![("mcp__srv__t".into(), "srv".into(), "t".into())],
            used_cache: false,
        });

        assert_eq!(manager.servers["srv"].status, MCPServerStatus::Pending);
        assert!(manager.tool_name_map.is_empty());
        assert!(manager.cache.is_empty());
    }

    /// Два переключения подряд — «выкл, вкл», а не два включения.
    #[test]
    fn flip_decides_and_changes_in_one_step() {
        let mut manager = MCPManager::new();
        let (client, _) = offline_client();
        manager.servers.insert(
            "srv".to_string(),
            MCPServerEntry::pending(Arc::clone(&client), config(), ServerSource::Own),
        );

        let Ok(Flip::Disabled(old)) = manager.flip("srv") else {
            panic!("включённый сервер должен выключиться");
        };
        assert!(Arc::ptr_eq(&old, &client));
        assert_eq!(manager.servers["srv"].status, MCPServerStatus::Disabled);

        assert!(matches!(manager.flip("srv"), Ok(Flip::Enabling(..))));
        let entry = &manager.servers["srv"];
        assert!(entry.enabled);
        assert_eq!(entry.status, MCPServerStatus::Pending);

        assert!(manager.flip("nope").is_err());
    }

    #[test]
    fn take_server_drops_the_entry_and_its_tools() {
        let mut manager = MCPManager::new();
        let (client, _) = offline_client();
        manager.servers.insert(
            "srv".to_string(),
            MCPServerEntry::pending(Arc::clone(&client), config(), ServerSource::Own),
        );
        manager
            .tool_name_map
            .insert("mcp__srv__t".into(), ("srv".into(), "t".into()));

        let taken = manager.take_server("srv").expect("сервер есть");

        assert!(Arc::ptr_eq(&taken, &client));
        assert!(!manager.has_server("srv"));
        assert!(manager.tool_name_map.is_empty());
        assert!(manager.take_server("srv").is_err());
    }
}
