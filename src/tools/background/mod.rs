//! Background and interactive (PTY) process management.
//!
//! Split into three parts:
//! - [`types`] — the shared vocabulary: [`ProcessStatus`], the process handle,
//!   and output/kill helpers.
//! - [`registry`] — the global process store and the operations over it
//!   (output, input, kill, prune, idle-expiry, shutdown).
//! - [`spawn`] — spawning detached pipe-based and PTY-based processes.
//!
//! The public surface is re-exported here so callers keep using
//! `background::spawn_background`, `background::read_output`, etc. unchanged.

mod registry;
mod spawn;
mod types;

pub use registry::{
    expire_persistent_idle_processes, kill_process, process_snapshots, prune_finished_processes,
    prune_jobs, read_output, remove_process, running_process_counts, shutdown_all,
    shutdown_nonpersistent_of, shutdown_owned_by, write_input,
};
pub use spawn::{spawn_background, spawn_interactive};
pub use types::{DEFAULT_PERSISTENT_TTL_SECS, ProcessStatus, with_owner};
// Дерево процессов команды переднего плана — см. `shell.rs`.
pub(crate) use types::ProcessTree;
// `win_job`: фоновые дети (и MCP stdio) привязываются к задаче kill-on-close,
// иначе принудительное закрытие pooprusteek оставит их сиротами.
#[cfg(windows)]
pub(crate) use types::win_job;
