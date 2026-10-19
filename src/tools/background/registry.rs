//! The global process registry and the operations that read or mutate it:
//! output draining, input, kill, pruning, idle-expiry, and shutdown.

use super::types::{
    ActivitySlot, BackgroundHandle, BgCmd, OutputBuffer, ProcessSnapshot, ProcessStatus,
    StatusSlot, StdinWriter, is_running_status,
};
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

struct BackgroundRegistry {
    next_id: u64,
    procs: HashMap<u64, Arc<BackgroundHandle>>,
}

impl BackgroundRegistry {
    fn new() -> Self {
        Self {
            next_id: 1,
            procs: HashMap::new(),
        }
    }
}

static REGISTRY: std::sync::OnceLock<Mutex<BackgroundRegistry>> = std::sync::OnceLock::new();

fn registry() -> &'static Mutex<BackgroundRegistry> {
    REGISTRY.get_or_init(|| Mutex::new(BackgroundRegistry::new()))
}

/// Register a freshly-spawned process and return its assigned id.
#[expect(clippy::too_many_arguments)]
pub(crate) async fn insert_handle(
    pid: Option<u32>,
    command_str: String,
    shell: String,
    started_at: chrono::DateTime<chrono::Utc>,
    last_activity_at: ActivitySlot,
    buffer: OutputBuffer,
    overflow: Arc<std::sync::atomic::AtomicBool>,
    status: StatusSlot,
    cmd_tx: mpsc::UnboundedSender<BgCmd>,
    writer: Option<StdinWriter>,
    interactive: bool,
    persistent: bool,
    ttl_secs: Option<u64>,
) -> u64 {
    let mut reg = registry().lock().await;
    let id = reg.next_id;
    reg.next_id += 1;
    reg.procs.insert(
        id,
        Arc::new(BackgroundHandle {
            pid,
            command: command_str,
            shell,
            started_at,
            last_activity_at,
            buffer,
            overflow,
            status,
            cmd_tx,
            writer,
            interactive,
            persistent,
            ttl_secs,
            owner: super::types::current_owner(),
        }),
    );
    id
}

pub async fn read_output(id: u64) -> Option<(String, ProcessStatus)> {
    let handle = {
        let reg = registry().lock().await;
        reg.procs.get(&id).cloned()
    }?;
    let status = handle.status().await;
    let out = handle.drain_output().await;
    Some((out, status))
}

pub async fn prune_finished_processes() -> usize {
    let finished_ids: Vec<u64> = {
        let reg = registry().lock().await;
        reg.procs
            .iter()
            .filter_map(|(id, handle)| {
                let status = handle.status.lock().unwrap().clone();
                (!is_running_status(&status)).then_some(*id)
            })
            .collect()
    };
    if finished_ids.is_empty() {
        return 0;
    }
    let mut reg = registry().lock().await;
    for id in &finished_ids {
        reg.procs.remove(id);
    }
    finished_ids.len()
}

pub async fn remove_process(id: u64) -> bool {
    let mut reg = registry().lock().await;
    reg.procs.remove(&id).is_some()
}

/// Срок записи во ввод интерактивного процесса.
const INPUT_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub async fn write_input(id: u64, bytes: &[u8]) -> Result<(), String> {
    let handle = {
        let reg = registry().lock().await;
        reg.procs.get(&id).cloned()
    };
    let handle = handle.ok_or_else(|| format!("No background process with id={id}"))?;
    if !handle.interactive {
        return Err(
            "Process is not interactive; start it with interactive=true to send input.".to_string(),
        );
    }
    let writer = handle.writer.clone().ok_or_else(|| {
        "Process has no stdin writer (this should not happen for interactive processes)."
            .to_string()
    })?;
    // Запись в PTY блокирующая: процесс, который не читает ввод, держал бы
    // воркер tokio вечно. `try_lock` — прошлый ввод ещё в пути, не копим очередь.
    let bytes = bytes.to_vec();
    let write = tokio::task::spawn_blocking(move || {
        let mut guard = match writer.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return Err(InputError::Busy),
        };
        guard
            .write_all(&bytes)
            .and_then(|()| guard.flush())
            .map_err(|e| InputError::Failed(e.to_string()))
    });
    let result = match tokio::time::timeout(INPUT_WRITE_TIMEOUT, write).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(InputError::Busy))) => Err(
            "The previous input to this process is still being delivered; wait and read its output first."
                .to_string(),
        ),
        Ok(Ok(Err(InputError::Failed(e)))) => Err(format!("Failed to write to process stdin: {e}")),
        Ok(Err(e)) => Err(format!("Failed to write to process stdin: {e}")),
        Err(_) => Err(format!(
            "The process has not taken this input within {}s (it is not reading). It may still be delivered later — do not resend it.",
            INPUT_WRITE_TIMEOUT.as_secs()
        )),
    };
    if result.is_ok() {
        handle.touch();
    }
    result
}

/// Почему ввод не дошёл до процесса.
enum InputError {
    Busy,
    Failed(String),
}

pub async fn kill_process(id: u64) -> Option<std::io::Result<()>> {
    let handle = {
        let reg = registry().lock().await;
        reg.procs.get(&id).cloned()
    }?;
    Some(handle.kill().await)
}

pub async fn process_snapshots() -> Vec<ProcessSnapshot> {
    let reg = registry().lock().await;
    let mut out = Vec::new();
    for (id, h) in reg.procs.iter() {
        out.push(ProcessSnapshot {
            id: *id,
            pid: h.pid,
            shell: h.shell.clone(),
            command: h.command.clone(),
            started_at: h.started_at,
            last_activity_at: *h.last_activity_at.lock().unwrap(),
            status: h.status.lock().unwrap().clone(),
            interactive: h.interactive,
            persistent: h.persistent,
            ttl_secs: h.ttl_secs,
        });
    }
    out.sort_by_key(|proc| proc.id);
    out
}

pub async fn running_process_counts() -> (usize, usize, usize) {
    let reg = registry().lock().await;
    let mut total = 0usize;
    let mut interactive = 0usize;
    let mut persistent = 0usize;
    for handle in reg.procs.values() {
        let status = handle.status.lock().unwrap().clone();
        if is_running_status(&status) {
            total += 1;
            if handle.interactive {
                interactive += 1;
            }
            if handle.persistent {
                persistent += 1;
            }
        }
    }
    (total, interactive, persistent)
}

pub async fn expire_persistent_idle_processes() -> usize {
    let now = chrono::Utc::now();
    let expired: Vec<(u64, Arc<BackgroundHandle>)> = {
        let mut reg = registry().lock().await;
        let expired_ids: Vec<u64> = reg
            .procs
            .iter()
            .filter_map(|(id, handle)| {
                let ttl_secs = handle.ttl_secs?;
                if ttl_secs == 0 {
                    return None;
                }
                let last_activity_at = *handle.last_activity_at.lock().unwrap();
                let status = handle.status.lock().unwrap().clone();
                let idle_secs = now
                    .signed_duration_since(last_activity_at)
                    .num_seconds()
                    .max(0) as u64;
                (handle.persistent && is_running_status(&status) && idle_secs >= ttl_secs)
                    .then_some(*id)
            })
            .collect();
        expired_ids
            .into_iter()
            .filter_map(|id| reg.procs.remove(&id).map(|handle| (id, handle)))
            .collect()
    };

    let count = expired.len();
    // Убийство — в фоне: его зовёт и главный цикл, а `taskkill` идёт сотни мс.
    kill_in_background(expired.into_iter().map(|(_, handle)| handle).collect());
    count
}

/// Погасить процессы, не дожидаясь их смерти на вызывающей стороне.
fn kill_in_background(handles: Vec<Arc<BackgroundHandle>>) {
    if handles.is_empty() {
        return;
    }
    tokio::spawn(async move {
        for handle in &handles {
            let _ = handle.kill().await;
        }
    });
}

pub async fn prune_jobs() -> (usize, usize) {
    let expired = expire_persistent_idle_processes().await;
    let finished = prune_finished_processes().await;
    (finished, expired)
}

/// Временные (не `persistent`) процессы чата `owner` — перед его новой
/// репликой и по Esc. Чужие чаты и постоянные серверы не трогаются.
pub async fn shutdown_nonpersistent_of(owner: u64) -> usize {
    let handles: Vec<Arc<BackgroundHandle>> = {
        let mut reg = registry().lock().await;
        let ids: Vec<u64> = reg
            .procs
            .iter()
            .filter(|(_, handle)| !handle.persistent && handle.owner == Some(owner))
            .map(|(id, _)| *id)
            .collect();
        ids.iter().filter_map(|id| reg.procs.remove(id)).collect()
    };
    let count = handles.len();
    kill_in_background(handles);
    count
}

/// Все процессы чата `owner` — когда сам чат закрыт или остановлен.
pub async fn shutdown_owned_by(owner: u64) -> usize {
    let handles: Vec<Arc<BackgroundHandle>> = {
        let mut reg = registry().lock().await;
        let ids: Vec<u64> = reg
            .procs
            .iter()
            .filter(|(_, handle)| handle.owner == Some(owner))
            .map(|(id, _)| *id)
            .collect();
        ids.iter().filter_map(|id| reg.procs.remove(id)).collect()
    };
    let count = handles.len();
    kill_in_background(handles);
    count
}

/// Kill every background/PTY process and clear the registry. Called on app
/// shutdown so spawn_blocking waiters unblock and the tokio runtime can exit.
pub async fn shutdown_all() -> usize {
    let handles: Vec<Arc<BackgroundHandle>> = {
        let mut reg = registry().lock().await;
        reg.procs.drain().map(|(_, h)| h).collect()
    };
    for handle in &handles {
        let _ = handle.kill().await;
    }
    // Give the OS a moment to actually terminate the children so the
    // blocking wait() calls in the spawned tasks return before we drop
    // the runtime.
    if !handles.is_empty() {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    handles.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::background::with_owner;

    /// Процесс-пустышка в общем реестре; владелец — из `with_owner` вокруг.
    async fn fake(persistent: bool) -> u64 {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<BgCmd>();
        insert_handle(
            None,
            "test".to_string(),
            "bash".to_string(),
            chrono::Utc::now(),
            Arc::new(std::sync::Mutex::new(chrono::Utc::now())),
            Arc::new(std::sync::Mutex::new(Vec::new())),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Arc::new(std::sync::Mutex::new(ProcessStatus::Running)),
            cmd_tx,
            None,
            false,
            persistent,
            None,
        )
        .await
    }

    async fn alive(id: u64) -> bool {
        registry().lock().await.procs.contains_key(&id)
    }

    /// Esc в одном чате гасит только его временные процессы.
    #[tokio::test]
    async fn turn_cleanup_spares_other_chats_and_persistent_servers() {
        // Владельцы с запасом от реальных id: реестр общий для всех тестов.
        let (mine, other) = (u64::MAX - 101, u64::MAX - 102);
        let temp = with_owner(mine, fake(false)).await;
        let server = with_owner(mine, fake(true)).await;
        let foreign = with_owner(other, fake(false)).await;

        assert_eq!(shutdown_nonpersistent_of(mine).await, 1);
        assert!(!alive(temp).await);
        assert!(alive(server).await && alive(foreign).await);

        assert_eq!(shutdown_owned_by(mine).await, 1);
        assert!(!alive(server).await);
        assert_eq!(shutdown_owned_by(other).await, 1);
    }

    /// Суб-агент внутри хода родителя — процессы родителя, не свои.
    #[tokio::test]
    async fn nested_owner_is_inherited_from_the_outer_turn() {
        let (parent, child) = (u64::MAX - 201, u64::MAX - 202);
        let id = with_owner(parent, with_owner(child, fake(false))).await;
        assert_eq!(
            registry().lock().await.procs.get(&id).and_then(|h| h.owner),
            Some(parent)
        );
        assert_eq!(shutdown_owned_by(parent).await, 1);
    }
}
