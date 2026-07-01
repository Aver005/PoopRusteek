//! Counts of running background processes, kept in sync with the
//! [`tools::background`](crate::tools::background) registry.
//!
//! The three counts were a data clump on the `AppState` god-object — always set
//! together, always read together — and the helpers that maintained them took
//! `&mut self` (all of `App`) just to write those fields. Grouping the counts
//! here, with refresh / shutdown / kill / prune as methods, lets that logic
//! operate on just this state.

/// Cached counts of running background processes, mirrored from the registry.
#[derive(Default)]
pub struct BackgroundCounters {
    pub total: usize,
    pub interactive: usize,
    pub persistent: usize,
    last_refresh: Option<std::time::Instant>,
}

impl BackgroundCounters {
    /// Event-loop poll throttle. TTL expiry doesn't need sub-second precision,
    /// and the unthrottled version used to run three registry lock round-trips
    /// per keystroke and per streamed token.
    const REFRESH_INTERVAL_MS: u128 = 1000;

    /// Throttled poll for the event loop: expire idle / finished jobs and
    /// resync counts at most once per interval. Returns whether the visible
    /// counts changed (drives the dirty flag).
    pub async fn refresh(&mut self) -> bool {
        if let Some(last) = self.last_refresh
            && last.elapsed().as_millis() < Self::REFRESH_INTERVAL_MS {
                return false;
            }
        self.refresh_now().await
    }

    /// Unthrottled resync — call right after an operation that changed jobs.
    pub async fn refresh_now(&mut self) -> bool {
        self.last_refresh = Some(std::time::Instant::now());
        let _ = crate::tools::background::expire_persistent_idle_processes().await;
        let _ = crate::tools::background::prune_finished_processes().await;
        let (total, interactive, persistent) =
            crate::tools::background::running_process_counts().await;
        let changed = total != self.total
            || interactive != self.interactive
            || persistent != self.persistent;
        self.total = total;
        self.interactive = interactive;
        self.persistent = persistent;
        changed
    }

    fn clear(&mut self) {
        self.total = 0;
        self.interactive = 0;
        self.persistent = 0;
    }

    /// Stop every background process and zero the counts; returns how many died.
    pub async fn shutdown_all(&mut self) -> usize {
        let killed = crate::tools::background::shutdown_all().await;
        self.clear();
        killed
    }

    /// Before a new user turn, stop non-persistent processes; returns the count.
    pub async fn cleanup_before_user_turn(&mut self) -> usize {
        if self.total == 0 {
            return 0;
        }
        let killed = crate::tools::background::shutdown_nonpersistent().await;
        let _ = self.refresh_now().await;
        killed
    }

    /// Kill a single job by id, returning a user-facing status line.
    pub async fn kill_job(&mut self, id: u64) -> String {
        match crate::tools::background::kill_process(id).await {
            Some(Ok(())) => {
                let _ = crate::tools::background::remove_process(id).await;
                let _ = self.refresh_now().await;
                format!("Stopped job #{id}.")
            }
            Some(Err(error)) => format!("Failed to stop job #{id}: {error}"),
            None => format!("No job with id={id}."),
        }
    }

    /// Prune finished/expired jobs, returning a user-facing status line.
    pub async fn prune_jobs(&mut self) -> String {
        let (finished, expired) = crate::tools::background::prune_jobs().await;
        let _ = self.refresh_now().await;
        if finished == 0 && expired == 0 {
            "No jobs pruned.".to_string()
        } else {
            format!("Pruned jobs: finished={finished}, expired={expired}.")
        }
    }
}
