//! In-memory registry of periodic jobs.
//!
//! The [`JobRegistry`] owns a map `name → JobEntry`. Native services
//! `register()` themselves during DI; the [`SchedulerEngine`](super::engine::SchedulerEngine)
//! iterates this map on every tick to pick the next-due job.
//!
//! Per-job state (in-flight semaphore, last outcome, next-run time)
//! lives inside each [`JobEntry`] behind a short-lived `std::sync::Mutex`.
//! The outer map uses a `tokio::sync::RwLock` so `pick_next` and
//! `snapshot` don't block one another and so future dynamic
//! registration (plugin manifests, admin UI) can acquire a write
//! lock without racing readers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use tokio::sync::{RwLock, Semaphore};

use super::handler::JobHandler;
use super::types::JobOutcome;

/// A registered job plus its runtime state. Held as `Arc<JobEntry>`
/// inside the registry so the engine can hold a snapshot across an
/// `await` without pinning the registry's outer lock.
pub struct JobEntry {
    pub(super) handler: Arc<dyn JobHandler>,
    /// `None` = on-demand only; the supervisor never fires this job
    /// (`pick_next` skips it). Admin/programmatic callers reach it
    /// via [`JobRegistry::trigger`].
    /// `Some(dur)` = periodic; supervisor dispatches every `dur`.
    pub(super) interval: Option<Duration>,
    pub(super) timeout: Option<Duration>,
    /// Single-permit gate enforcing the "one in-flight run per
    /// `job_name`" invariant. A tick that finds the permit taken
    /// emits `job.tick_skipped` and does not spawn.
    pub(super) in_flight: Semaphore,
    /// Mutable state — protected by `std::sync::Mutex` because guards
    /// are only held for a few statements at a time, never across an
    /// `await`. `tokio::sync::Mutex` would add overhead for no benefit.
    pub(super) state: Mutex<JobState>,
}

pub(super) struct JobState {
    /// Set when a run starts, cleared when it ends. Used to include
    /// `running_for_ms` in the `job.tick_skipped` warning.
    pub current_run_start: Option<Instant>,
    /// Wall-clock time + outcome of the most recent completed run.
    /// `None` until the first run finishes.
    pub last_outcome: Option<(DateTime<Utc>, JobOutcome)>,
    /// Wall-clock time of the next scheduled dispatch. `None` for
    /// on-demand jobs (never fires periodically); `Some(...)` for
    /// scheduled jobs, advanced by one interval after every tick.
    pub next_run_at: Option<DateTime<Utc>>,
}

/// In-memory job registry. `Arc<JobRegistry>` lives on `AppState`;
/// native services `register()` during DI wiring.
pub struct JobRegistry {
    entries: RwLock<HashMap<String, Arc<JobEntry>>>,
}

impl JobRegistry {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
        }
    }

    /// Register a job. Returns an error if a job with the same name
    /// is already registered — names are the primary identifier
    /// everywhere (logs, admin URLs, env vars) and collisions would
    /// hide bugs.
    ///
    /// - `interval = Some(dur)` → **scheduled**. The supervisor fires
    ///   the job every `dur`, starting `now + dur`. Registration does
    ///   NOT fire the job immediately — callers that want an at-startup
    ///   run should invoke the service's own initialiser once before
    ///   registering.
    /// - `interval = None` → **on-demand only**. The supervisor never
    ///   fires this job. Admin endpoint (or programmatic callers) can
    ///   still invoke it via [`JobRegistry::trigger`] — the dispatch
    ///   goes through the same panic/timeout/exclusivity gates.
    pub async fn register(
        &self,
        handler: Arc<dyn JobHandler>,
        interval: Option<Duration>,
        timeout: Option<Duration>,
    ) -> Result<(), RegisterError> {
        let name = handler.name().to_string();
        let mut guard = self.entries.write().await;
        if guard.contains_key(&name) {
            return Err(RegisterError::DuplicateName(name));
        }
        let next_run_at = interval.map(|dur| {
            Utc::now()
                + chrono::Duration::from_std(dur).unwrap_or_else(|_| chrono::Duration::seconds(0))
        });
        let entry = Arc::new(JobEntry {
            handler,
            interval,
            timeout,
            in_flight: Semaphore::new(1),
            state: Mutex::new(JobState {
                current_run_start: None,
                last_outcome: None,
                next_run_at,
            }),
        });
        guard.insert(name, entry);
        Ok(())
    }

    /// Return the name and next-due timestamp of the earliest-firing
    /// **scheduled** job, or `None` if no scheduled jobs are registered.
    /// On-demand jobs (registered with `interval = None`) are invisible
    /// to `pick_next` — they only run when reached via
    /// [`Self::trigger`]. Read-lock only — safe to call frequently from
    /// the supervisor loop.
    pub async fn pick_next(&self) -> Option<(String, DateTime<Utc>)> {
        let guard = self.entries.read().await;
        let mut earliest: Option<(String, DateTime<Utc>)> = None;
        for (name, entry) in guard.iter() {
            let Some(next_at) = entry
                .state
                .lock()
                .expect("JobState mutex poisoned")
                .next_run_at
            else {
                continue; // on-demand only — never picked
            };
            match &earliest {
                None => earliest = Some((name.clone(), next_at)),
                Some((_, current)) if next_at < *current => {
                    earliest = Some((name.clone(), next_at))
                }
                _ => {}
            }
        }
        earliest
    }

    /// Snapshot handle to a single job. Returns `Arc<JobEntry>` so
    /// callers can hold across `await` points without pinning the
    /// outer read lock.
    pub async fn get(&self, name: &str) -> Option<Arc<JobEntry>> {
        let guard = self.entries.read().await;
        guard.get(name).cloned()
    }

    /// Snapshot every registered job (used by the admin listing
    /// endpoint). Returns owned `(name, Arc<JobEntry>)` pairs to
    /// avoid pinning the outer lock through the HTTP response
    /// serialisation.
    pub async fn snapshot_all(&self) -> Vec<(String, Arc<JobEntry>)> {
        let guard = self.entries.read().await;
        guard.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    /// Count of registered jobs — used for the startup log line.
    pub async fn len(&self) -> usize {
        self.entries.read().await.len()
    }

    #[allow(dead_code)]
    pub async fn is_empty(&self) -> bool {
        self.entries.read().await.is_empty()
    }

    /// Manual dispatch — the single entry point for running a
    /// registered job outside the scheduler's tick loop. Called by:
    ///
    /// - The admin endpoint `POST /api/admin/internal/trigger-job/{name}`.
    /// - Any service that wants a scheduler-uniform dispatch of a
    ///   peer job (uniform log line, exclusivity, panic containment,
    ///   timeout enforcement).
    ///
    /// Returns `None` when the name isn't registered. Returns
    /// `Some(JobOutcome)` when it is — including the case where
    /// exclusivity denied the trigger (previous run still in flight),
    /// which surfaces as
    /// `Ok { count: 0, extra: { "skipped": "already_running" } }` per
    /// the engine's dispatch protocol.
    ///
    /// Works for BOTH scheduled and on-demand jobs — for on-demand
    /// jobs this is the only way they ever run.
    pub async fn trigger(self: &Arc<Self>, name: &str) -> Option<JobOutcome> {
        let entry = self.get(name).await?;
        Some(super::engine::dispatch(name, entry).await)
    }
}

impl Default for JobRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RegisterError {
    #[error("job name already registered: {0}")]
    DuplicateName(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct DummyHandler {
        name: String,
    }

    #[async_trait]
    impl JobHandler for DummyHandler {
        fn name(&self) -> &str {
            &self.name
        }
        async fn run(&self) -> JobOutcome {
            JobOutcome::ok(0)
        }
    }

    fn handler(name: &str) -> Arc<dyn JobHandler> {
        Arc::new(DummyHandler {
            name: name.to_string(),
        })
    }

    #[tokio::test]
    async fn register_and_pick_next() {
        let reg = JobRegistry::new();
        reg.register(handler("job_a"), Some(Duration::from_secs(60)), None)
            .await
            .unwrap();
        reg.register(handler("job_b"), Some(Duration::from_secs(10)), None)
            .await
            .unwrap();

        let (next_name, _) = reg.pick_next().await.expect("expected a due job");
        // job_b has the shorter interval → earlier next_run_at.
        assert_eq!(next_name, "job_b");
    }

    #[tokio::test]
    async fn duplicate_registration_rejected() {
        let reg = JobRegistry::new();
        reg.register(handler("job_x"), Some(Duration::from_secs(60)), None)
            .await
            .unwrap();
        let err = reg
            .register(handler("job_x"), Some(Duration::from_secs(60)), None)
            .await
            .expect_err("duplicate name must be rejected");
        assert!(matches!(err, RegisterError::DuplicateName(_)));
    }

    #[tokio::test]
    async fn empty_registry_picks_nothing() {
        let reg = JobRegistry::new();
        assert!(reg.pick_next().await.is_none());
    }

    #[tokio::test]
    async fn snapshot_all_returns_every_entry() {
        let reg = JobRegistry::new();
        reg.register(handler("a"), Some(Duration::from_secs(1)), None)
            .await
            .unwrap();
        reg.register(handler("b"), Some(Duration::from_secs(1)), None)
            .await
            .unwrap();
        let all = reg.snapshot_all().await;
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn on_demand_job_invisible_to_pick_next() {
        let reg = JobRegistry::new();
        // Scheduled job with a long interval.
        reg.register(handler("scheduled"), Some(Duration::from_secs(3600)), None)
            .await
            .unwrap();
        // On-demand job — supervisor must never pick it.
        reg.register(handler("on_demand"), None, None)
            .await
            .unwrap();

        let (next_name, _) = reg.pick_next().await.expect("scheduled job due");
        assert_eq!(
            next_name, "scheduled",
            "pick_next must ignore on-demand jobs"
        );
    }

    #[tokio::test]
    async fn trigger_dispatches_on_demand_job() {
        let reg = Arc::new(JobRegistry::new());
        reg.register(handler("gc"), None, None).await.unwrap();

        let outcome = reg.trigger("gc").await.expect("job exists");
        assert!(outcome.is_ok());
    }

    #[tokio::test]
    async fn trigger_returns_none_for_unknown_job() {
        let reg = Arc::new(JobRegistry::new());
        assert!(reg.trigger("nope").await.is_none());
    }
}
