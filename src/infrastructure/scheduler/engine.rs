//! The scheduler supervisor loop.
//!
//! One `tokio::spawn` at startup runs [`SchedulerEngine::run`]. The
//! loop iterates:
//!
//! 1. `pick_next()` — find the job with the earliest `next_run_at`.
//! 2. Sleep until that instant.
//! 3. Dispatch: try-acquire the job's in-flight permit; if held, warn
//!    and reschedule; otherwise spawn the handler, apply the timeout,
//!    catch panics, record the outcome.
//!
//! Sequential dispatch is intentional. Two jobs due at the same
//! instant run one-after-the-other — the second's `pick_next` fires
//! immediately after the first's dispatch returns, with a zero-length
//! sleep. See `docs/plan/job-registry.md` Part 1 §Runtime model.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use tokio::task::JoinHandle;

use super::registry::{JobEntry, JobRegistry};
use super::types::{ErrCause, JobOutcome};

/// Public handle to the running supervisor.
///
/// Dropping the handle does NOT cancel the loop (the spawned task
/// runs until the runtime dies). Explicit shutdown is deferred to
/// whenever graceful-shutdown lands globally — matches the shape
/// every other daemon in the codebase has today. See
/// `docs/plan/job-registry.md` Part 1 §Shutdown coordination.
pub struct SchedulerEngine {
    _handle: JoinHandle<()>,
}

impl SchedulerEngine {
    /// Spawn the supervisor loop and return a handle. Callers hold
    /// the returned `SchedulerEngine` on `AppState` so the task lives
    /// as long as the runtime.
    pub fn start(registry: Arc<JobRegistry>) -> Self {
        let handle = tokio::spawn(async move {
            run(registry).await;
        });
        Self { _handle: handle }
    }
}

/// If the registry is empty (no jobs registered yet), sleep this long
/// before rechecking. Registration happens once at boot in the current
/// design, so this only matters as a defensive fallback — in practice
/// the loop enters this branch at most once, right before the first
/// `register()` call completes.
const IDLE_POLL: Duration = Duration::from_secs(60);

async fn run(registry: Arc<JobRegistry>) {
    tracing::info!(
        target: "oxicloud::scheduler",
        event = "scheduler.started",
        "periodic scheduler supervisor started"
    );

    loop {
        // `pick_next` only returns scheduled jobs (interval = Some);
        // on-demand jobs never appear here and are only reachable
        // through `JobRegistry::trigger`.
        let Some((name, next_at)) = registry.pick_next().await else {
            tokio::time::sleep(IDLE_POLL).await;
            continue;
        };

        // Convert to `Duration`. If `next_at` is in the past (missed
        // tick, e.g. very short interval and the previous dispatch
        // took longer than the interval), sleep zero and dispatch
        // immediately.
        let now = Utc::now();
        let sleep_dur = (next_at - now)
            .to_std()
            .unwrap_or_else(|_| Duration::from_millis(0));
        if !sleep_dur.is_zero() {
            tokio::time::sleep(sleep_dur).await;
        }

        // The job's `next_run_at` might have changed since `pick_next`
        // returned if a concurrent trigger fired — that's fine; the
        // dispatch below re-reads via the `JobEntry` snapshot.
        let Some(entry) = registry.get(&name).await else {
            // Job was unregistered between pick_next and dispatch —
            // unreachable in the current design (no unregister), but
            // guard defensively.
            continue;
        };

        // Fire and forget from the supervisor's perspective — we
        // don't care about the outcome, `dispatch` records it on the
        // entry and emits the log line itself.
        let _ = dispatch(&name, entry).await;
    }
}

/// Dispatch a single run of `name`. Handles:
/// - exclusivity: try-acquire the in-flight permit; skip + warn if held,
/// - spawning under panic containment (via `tokio::spawn` + `JoinHandle`),
/// - timeout enforcement (if `ScheduledJob.timeout` is set),
/// - recording `last_outcome` + advancing `next_run_at` on completion,
/// - emitting the uniform `oxicloud::scheduler::job.run` log line.
///
/// Returns the [`JobOutcome`] the run produced. The scheduler loop
/// discards this (records-only-via-side-effect); admin/programmatic
/// callers via [`JobRegistry::trigger`](super::registry::JobRegistry::trigger)
/// surface it to the caller.
///
/// Non-panicking; every failure path resolves to a `JobOutcome::Err`
/// with a `cause` log field.
pub(super) async fn dispatch(name: &str, entry: Arc<JobEntry>) -> JobOutcome {
    // Try to acquire the single-permit gate. `try_acquire` is
    // non-blocking — if held, we know the previous run is still
    // executing and skip this tick.
    let permit = match entry.in_flight.try_acquire() {
        Ok(p) => p,
        Err(_) => {
            // Someone else holds the permit → previous run still in
            // flight. Emit the operator-signal warning and reschedule.
            let running_for_ms = {
                let state = entry.state.lock().expect("JobState mutex poisoned");
                state
                    .current_run_start
                    .map(|t| t.elapsed().as_millis())
                    .unwrap_or(0)
            };
            // On-demand jobs have `interval = None`; log 0 rather than
            // fabricate one. Operators reading this line for a scheduled
            // job compare `interval_ms` vs `running_for_ms`; the same
            // line for an on-demand job just tells them a concurrent
            // trigger raced an in-flight run.
            let interval_ms = entry.interval.map(|d| d.as_millis()).unwrap_or(0);
            tracing::warn!(
                target: "oxicloud::scheduler",
                event = "job.tick_skipped",
                job = %name,
                interval_ms = interval_ms,
                running_for_ms = running_for_ms,
                "{} still running past its interval — tick skipped",
                name,
            );
            advance_next_run(&entry);
            return JobOutcome::ok_with(0, serde_json::json!({ "skipped": "already_running" }));
        }
    };

    // We hold the permit. Record run-start, spawn, await, translate.
    {
        let mut state = entry.state.lock().expect("JobState mutex poisoned");
        state.current_run_start = Some(Instant::now());
    }
    let started_wall = Utc::now();
    let start_instant = Instant::now();

    // Spawn so panics land as `JoinError::is_panic()` instead of
    // unwinding into the supervisor loop.
    let handler = entry.handler.clone();
    let join = tokio::spawn(async move { handler.run().await });

    let (outcome, cause) = match entry.timeout {
        Some(dur) => match tokio::time::timeout(dur, join).await {
            Ok(res) => translate_join(res),
            Err(_elapsed) => {
                // Timeout fired. The JoinHandle is dropped, which
                // aborts the spawned task cooperatively — but abort
                // is best-effort in Rust; a handler that ignores
                // yield points may run to completion in the background.
                // We still record timeout and release the permit.
                (
                    JobOutcome::Err(format!("wall-clock timeout of {:?} exceeded", dur)),
                    Some(ErrCause::Timeout),
                )
            }
        },
        None => translate_join(join.await),
    };

    let elapsed_ms = start_instant.elapsed().as_millis();

    // Record outcome and advance the schedule. Permit drops naturally
    // when `permit` goes out of scope at the end of the function.
    {
        let mut state = entry.state.lock().expect("JobState mutex poisoned");
        state.current_run_start = None;
        state.last_outcome = Some((started_wall, outcome.clone()));
        // Only scheduled jobs advance next_run_at. On-demand jobs stay
        // at None so `pick_next` never returns them, even after a
        // trigger. Same rule as the skip branch — schedule advances
        // by one interval, no backlog queueing.
        state.next_run_at = entry.interval.map(|dur| {
            Utc::now()
                + chrono::Duration::from_std(dur).unwrap_or_else(|_| chrono::Duration::seconds(0))
        });
    }

    // Log line. `outcome=ok` runs are informational; `outcome=err` include
    // the diagnostic `cause` field.
    log_outcome(name, &outcome, cause, elapsed_ms);

    drop(permit);
    outcome
}

/// Advance `next_run_at` by one interval without touching outcome or
/// run-start (skip-path helper). No-op for on-demand jobs — `interval`
/// is `None`, so `next_run_at` stays `None` and `pick_next` continues
/// to skip them.
fn advance_next_run(entry: &JobEntry) {
    let mut state = entry.state.lock().expect("JobState mutex poisoned");
    state.next_run_at = entry.interval.map(|dur| {
        Utc::now()
            + chrono::Duration::from_std(dur).unwrap_or_else(|_| chrono::Duration::seconds(0))
    });
}

/// Convert the `Result<JobOutcome, JoinError>` returned by the spawned
/// handler into `(JobOutcome, Option<ErrCause>)`. `cause` is `None`
/// on Ok, `Some(_)` on Err.
fn translate_join(
    res: Result<JobOutcome, tokio::task::JoinError>,
) -> (JobOutcome, Option<ErrCause>) {
    match res {
        Ok(outcome) => {
            let cause = if outcome.is_ok() {
                None
            } else {
                Some(ErrCause::Handler)
            };
            (outcome, cause)
        }
        Err(join_err) if join_err.is_panic() => {
            let payload = join_err.into_panic();
            let msg = if let Some(s) = payload.downcast_ref::<&'static str>() {
                (*s).to_string()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic payload".to_string()
            };
            (
                JobOutcome::Err(format!("handler panicked: {msg}")),
                Some(ErrCause::Panicked),
            )
        }
        Err(join_err) => (
            JobOutcome::Err(format!("task cancelled: {join_err}")),
            Some(ErrCause::Handler),
        ),
    }
}

/// Emit the uniform `oxicloud::scheduler` log line for a completed run.
/// Distinct Ok/Err branches so the tracing macros pick up the fields at
/// compile time — `tracing` doesn't expand conditional field lists.
fn log_outcome(name: &str, outcome: &JobOutcome, cause: Option<ErrCause>, elapsed_ms: u128) {
    match outcome {
        JobOutcome::Ok { count, extra } => {
            tracing::info!(
                target: "oxicloud::scheduler",
                event = "job.run",
                job = %name,
                outcome = "ok",
                count = *count,
                elapsed_ms = elapsed_ms,
                extra = %extra,
                "job {} ran",
                name,
            );
        }
        JobOutcome::Err(msg) => {
            tracing::warn!(
                target: "oxicloud::scheduler",
                event = "job.run",
                job = %name,
                outcome = "err",
                cause = %cause.unwrap_or(ErrCause::Handler),
                elapsed_ms = elapsed_ms,
                error = %msg,
                "job {} failed",
                name,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::scheduler::handler::JobHandler;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct CountingHandler {
        name: String,
        calls: Arc<AtomicU64>,
        sleep: Duration,
    }

    #[async_trait]
    impl JobHandler for CountingHandler {
        fn name(&self) -> &str {
            &self.name
        }
        async fn run(&self) -> JobOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !self.sleep.is_zero() {
                tokio::time::sleep(self.sleep).await;
            }
            JobOutcome::ok(1)
        }
    }

    struct PanickingHandler;

    #[async_trait]
    impl JobHandler for PanickingHandler {
        fn name(&self) -> &str {
            "panicker"
        }
        async fn run(&self) -> JobOutcome {
            panic!("intentional test panic");
        }
    }

    #[tokio::test]
    async fn panic_containment_via_translate_join() {
        // Directly exercise translate_join with a spawned panic — the
        // supervisor loop's dispatch path uses this same helper.
        let handler = Arc::new(PanickingHandler);
        let join = tokio::spawn(async move { handler.run().await });
        let (outcome, cause) = translate_join(join.await);
        assert!(!outcome.is_ok());
        assert_eq!(cause, Some(ErrCause::Panicked));
        if let JobOutcome::Err(msg) = outcome {
            assert!(msg.contains("panicked"), "expected panic marker in: {msg}");
        }
    }

    #[tokio::test]
    async fn overrun_skips_second_tick() {
        // Handler that sleeps 200 ms; two dispatches fired back-to-back
        // should see the second skip with a `tick_skipped` warning.
        let calls = Arc::new(AtomicU64::new(0));
        let handler = Arc::new(CountingHandler {
            name: "overrun".to_string(),
            calls: calls.clone(),
            sleep: Duration::from_millis(200),
        });

        let registry = Arc::new(JobRegistry::new());
        registry
            .register(handler, Some(Duration::from_millis(100)), None)
            .await
            .unwrap();
        let entry = registry.get("overrun").await.unwrap();

        // Kick off dispatch 1 in the background — it holds the permit
        // for ~200 ms.
        let entry_bg = entry.clone();
        let bg = tokio::spawn(async move { dispatch("overrun", entry_bg).await });

        // Give dispatch 1 time to grab the permit.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Dispatch 2 should observe the permit taken and skip.
        dispatch("overrun", entry.clone()).await;

        // Only dispatch 1's handler should have actually run so far.
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        bg.await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn timeout_records_err_and_releases_permit() {
        let calls = Arc::new(AtomicU64::new(0));
        let handler = Arc::new(CountingHandler {
            name: "slow".to_string(),
            calls: calls.clone(),
            sleep: Duration::from_millis(500),
        });

        let registry = Arc::new(JobRegistry::new());
        registry
            .register(
                handler,
                Some(Duration::from_millis(100)),
                Some(Duration::from_millis(50)),
            )
            .await
            .unwrap();
        let entry = registry.get("slow").await.unwrap();

        dispatch("slow", entry.clone()).await;

        // The timeout fired; last_outcome must be Err.
        let state = entry.state.lock().unwrap();
        let (_, outcome) = state.last_outcome.as_ref().expect("outcome recorded");
        assert!(!outcome.is_ok(), "expected timeout-Err, got {outcome:?}");

        // Permit released — another dispatch could acquire it.
        assert_eq!(entry.in_flight.available_permits(), 1);
    }
}
