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
    shutdown_nonpersistent, write_input,
};
pub use spawn::{spawn_background, spawn_interactive};
pub use types::{DEFAULT_PERSISTENT_TTL_SECS, ProcessStatus};
// `force_kill_pid` is `pub(crate)` on the item itself (see types.rs); this
// re-export just makes it reachable as `background::force_kill_pid` from
// sibling tool modules (e.g. shell.rs's foreground timeout path) without
// widening its actual visibility past the crate.
pub(crate) use types::force_kill_pid;
