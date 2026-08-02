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
use serde::Serialize;
use tokio::sync::{RwLock, Semaphore};

use super::handler::JobHandler;
use super::types::{JobOutcome, JobRunArgs};

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

    /// Register a job — production wiring path.
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
    ///
    /// **Panics on error.** Registration failure (duplicate name today)
    /// is a DI-wiring bug — the server must not start with a mis-wired
    /// scheduler. Emits a uniform `job.registered` log line on success
    /// so callers don't reinvent the log message at every site.
    ///
    /// For unit tests that need to assert the error path use
    /// [`Self::try_register`] instead.
    pub async fn register(
        &self,
        handler: Arc<dyn JobHandler>,
        interval: Option<Duration>,
        timeout: Option<Duration>,
    ) {
        let name = handler.name().to_string();
        match self.try_register(handler, interval, timeout).await {
            Ok(()) => {
                let cadence = match interval {
                    Some(dur) => {
                        let secs = dur.as_secs();
                        if secs % 3600 == 0 {
                            format!("every {} h", secs / 3600)
                        } else if secs % 60 == 0 {
                            format!("every {} min", secs / 60)
                        } else {
                            format!("every {} s", secs)
                        }
                    }
                    None => "on-demand".to_string(),
                };
                tracing::info!(
                    target: "oxicloud::scheduler",
                    event = "job.registered",
                    job = %name,
                    cadence = %cadence,
                    "job {} registered ({})",
                    name,
                    cadence,
                );
            }
            Err(e) => panic!("JobRegistry::register({name}) failed — DI wiring bug: {e}"),
        }
    }

    /// Fallible sibling of [`Self::register`]. Returns `Err` on
    /// duplicate-name instead of panicking, and does NOT emit the
    /// `job.registered` log line — for unit tests that need to
    /// assert failure without triggering the boot panic path.
    pub async fn try_register(
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

    /// Serialisable snapshot for `GET /api/admin/jobs`. Each entry
    /// captures the operator-visible state: interval (null for on-
    /// demand), next scheduled dispatch (null for on-demand), when
    /// the last run started, and its outcome.
    pub async fn snapshot(&self) -> Vec<JobSummary> {
        let entries = self.snapshot_all().await;
        entries
            .into_iter()
            .map(|(name, entry)| {
                let state = entry.state.lock().expect("JobState mutex poisoned");
                let (last_run_at, last_outcome) = match &state.last_outcome {
                    Some((at, outcome)) => (Some(*at), Some(outcome.clone())),
                    None => (None, None),
                };
                JobSummary {
                    name,
                    interval_ms: entry.interval.map(|d| d.as_millis() as u64),
                    next_run_at: state.next_run_at,
                    last_run_at,
                    last_outcome,
                    running: state.current_run_start.is_some(),
                    recoverable: entry.handler.is_recoverable(),
                    // Populated in `list_jobs` handler via a single
                    // DB round-trip — kept out of the registry
                    // snapshot to avoid pulling a DB dependency into
                    // the in-memory scheduler state.
                    paused_run: None,
                }
            })
            .collect()
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
    /// - The admin endpoint `POST /api/admin/jobs/{name}/trigger`.
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
    ///
    /// `args` is forwarded to `JobHandler::run`. Admin trigger routes
    /// use `JobRunArgs { force: query.force }`; programmatic callers
    /// that just want a plain run pass `JobRunArgs::default()`.
    pub async fn trigger(self: &Arc<Self>, name: &str, args: &JobRunArgs) -> Option<JobOutcome> {
        let entry = self.get(name).await?;
        Some(super::engine::dispatch(name, entry, args).await)
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

/// Per-job row in the `GET /api/admin/jobs` response.
///
/// - `interval_ms` — periodic cadence; `null` for on-demand jobs.
/// - `next_run_at` — next scheduled dispatch; `null` for on-demand.
/// - `last_run_at` / `last_outcome` — most recent completed run;
///   `null` until the first run finishes.
/// - `running` — true iff the in-flight permit is currently held
///   (either the supervisor tick is in progress or an admin trigger
///   raced in).
/// - `recoverable` — true iff the job persists runs + findings to
///   `jobs.recoverable_runs`. Consumed by the admin UI to decide
///   whether the row is expandable (drawer with run history +
///   findings) and to gate the retention/purge action.
/// - `paused_run` — populated iff a `Paused` row exists in
///   `jobs.recoverable_runs` for this job. The UI uses it to render
///   "Resume (scanned/total)" instead of "Run".
#[derive(Debug, Clone, Serialize)]
pub struct JobSummary {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_outcome: Option<JobOutcome>,
    pub running: bool,
    pub recoverable: bool,
    /// Populated iff a `Paused` row exists in `jobs.recoverable_runs`
    /// for this job. Distinct from `running` — a paused run is
    /// resumable via the same trigger endpoint (`run_or_resume`
    /// picks Resume when the latest row is Paused).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused_run: Option<PausedRunBrief>,
}

/// Enough info about a paused recoverable run for the admin panel to
/// render "Resume (scanned/total)" on the job row without opening the
/// drawer. Populated by `list_jobs` in the admin handler from a
/// single `SELECT job_name, id, stats->>'scanned_count',
/// params->>'total_rows' FROM jobs.recoverable_runs WHERE status =
/// 'Paused'` — indexed by the `one_active_run_per_job` partial UNIQUE.
///
/// `total` is `None` when the tenant doesn't seed a countable subject
/// (`RecoverableJobHandler::count_total`); the UI then shows just
/// "Resume" without progress.
#[derive(Debug, Clone, Serialize)]
pub struct PausedRunBrief {
    pub id: uuid::Uuid,
    pub scanned: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
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
        async fn run(&self, _args: &JobRunArgs) -> JobOutcome {
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
            .await;
        reg.register(handler("job_b"), Some(Duration::from_secs(10)), None)
            .await;

        let (next_name, _) = reg.pick_next().await.expect("expected a due job");
        // job_b has the shorter interval → earlier next_run_at.
        assert_eq!(next_name, "job_b");
    }

    #[tokio::test]
    async fn duplicate_registration_rejected() {
        let reg = JobRegistry::new();
        // Use the fallible `try_register` here so we can assert the
        // Err path without triggering `register`'s boot-time panic.
        reg.try_register(handler("job_x"), Some(Duration::from_secs(60)), None)
            .await
            .unwrap();
        let err = reg
            .try_register(handler("job_x"), Some(Duration::from_secs(60)), None)
            .await
            .expect_err("duplicate name must be rejected");
        assert!(matches!(err, RegisterError::DuplicateName(_)));
    }

    #[tokio::test]
    #[should_panic(expected = "DI wiring bug")]
    async fn register_panics_on_duplicate() {
        let reg = JobRegistry::new();
        reg.register(handler("job_dup"), Some(Duration::from_secs(60)), None)
            .await;
        // Second register with same name — boot panic. Anything doing
        // this outside a #[should_panic] test is a mis-wired DI.
        reg.register(handler("job_dup"), Some(Duration::from_secs(60)), None)
            .await;
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
            .await;
        reg.register(handler("b"), Some(Duration::from_secs(1)), None)
            .await;
        let all = reg.snapshot_all().await;
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn on_demand_job_invisible_to_pick_next() {
        let reg = JobRegistry::new();
        // Scheduled job with a long interval.
        reg.register(handler("scheduled"), Some(Duration::from_secs(3600)), None)
            .await;
        // On-demand job — supervisor must never pick it.
        reg.register(handler("on_demand"), None, None).await;

        let (next_name, _) = reg.pick_next().await.expect("scheduled job due");
        assert_eq!(
            next_name, "scheduled",
            "pick_next must ignore on-demand jobs"
        );
    }

    #[tokio::test]
    async fn trigger_dispatches_on_demand_job() {
        let reg = Arc::new(JobRegistry::new());
        reg.register(handler("gc"), None, None).await;

        let outcome = reg
            .trigger("gc", &JobRunArgs::default())
            .await
            .expect("job exists");
        assert!(outcome.is_ok());
    }

    #[tokio::test]
    async fn trigger_returns_none_for_unknown_job() {
        let reg = Arc::new(JobRegistry::new());
        assert!(reg.trigger("nope", &JobRunArgs::default()).await.is_none());
    }
}
