//! The global process registry and the operations that read or mutate it:
//! output draining, input, kill, pruning, idle-expiry, and shutdown.

use super::types::{
    is_running_status, ActivitySlot, BackgroundHandle, BgCmd, OutputBuffer, ProcessSnapshot,
    ProcessStatus, StatusSlot, StdinWriter,
};
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

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
#[allow(clippy::too_many_arguments)]
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

pub async fn write_input(id: u64, bytes: &[u8]) -> Result<(), String> {
    let handle = {
        let reg = registry().lock().await;
        reg.procs.get(&id).cloned()
    };
    let handle = handle.ok_or_else(|| format!("No background process with id={id}"))?;
    if !handle.interactive {
        return Err("Process is not interactive; start it with interactive=true to send input.".to_string());
    }
    let writer = handle.writer.as_ref().ok_or_else(|| {
        "Process has no stdin writer (this should not happen for interactive processes).".to_string()
    })?;
    let mut guard = writer.lock().unwrap();
    let result = guard
        .write_all(bytes)
        .map_err(|e| format!("Failed to write to process stdin: {e}"));
    if result.is_ok() {
        handle.touch();
    }
    result
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
                let idle_secs = now.signed_duration_since(last_activity_at).num_seconds().max(0) as u64;
                (handle.persistent && is_running_status(&status) && idle_secs >= ttl_secs).then_some(*id)
            })
            .collect();
        expired_ids
            .into_iter()
            .filter_map(|id| reg.procs.remove(&id).map(|handle| (id, handle)))
            .collect()
    };

    for (_, handle) in &expired {
        let _ = handle.kill().await;
    }
    if !expired.is_empty() {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    expired.len()
}

pub async fn prune_jobs() -> (usize, usize) {
    let expired = expire_persistent_idle_processes().await;
    let finished = prune_finished_processes().await;
    (finished, expired)
}

pub async fn shutdown_nonpersistent() -> usize {
    let handles: Vec<Arc<BackgroundHandle>> = {
        let mut reg = registry().lock().await;
        let ids: Vec<u64> = reg
            .procs
            .iter()
            .filter_map(|(id, handle)| handle.persistent.then_some(*id))
            .collect();
        let persistent: HashMap<u64, Arc<BackgroundHandle>> = ids
            .iter()
            .filter_map(|id| reg.procs.get(id).cloned().map(|handle| (*id, handle)))
            .collect();
        let drained: Vec<Arc<BackgroundHandle>> = reg.procs.drain().map(|(_, h)| h).collect();
        reg.procs = persistent;
        drained
            .into_iter()
            .filter(|handle| !handle.persistent)
            .collect()
    };
    for handle in &handles {
        let _ = handle.kill().await;
    }
    if !handles.is_empty() {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    handles.len()
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
