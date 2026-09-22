//! `collab_idle_gc` scheduled job — periodic sweep of stale
//! collaborative-doc sessions.
//!
//! Rows in `collab.doc_sessions` whose `last_activity_at` is older
//! than `idle_ttl_minutes` (default 30, per `docs/plan/markdown-collab.md
//! § Backend step 5`) are eligible for reaping. For each stale row
//! the service:
//!
//!   1. Force-flushes any pending CRDT text back to the blob
//!      (belt-and-braces vs the periodic flush; the debouncer
//!      normally has caught everything by 30 min but the extra
//!      guarantee costs nothing).
//!   2. Shuts down the in-memory actor.
//!   3. Deletes the row from `collab.doc_sessions` — a subsequent
//!      attach re-seeds fresh from the (now up-to-date) blob.
//!
//! `idle_ttl` comes from `OXICLOUD_COLLAB_IDLE_TTL_MINUTES` (default
//! 30) via `default_idle_ttl_minutes`; override at trigger time via
//! the `idle_ttl_minutes` param. `batch_limit` caps the per-tick
//! work so a large backlog doesn't monopolise the maintenance pool
//! — the next tick picks up whatever this one leaves behind.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use tracing::info;

use crate::application::services::collab_session_service::CollabSessionService;
use crate::infrastructure::scheduler::{JobHandler, JobOutcome, JobRegistry, JobRunArgs, Mutates};

/// Parameter declaration. Kept module-level so `parameters()` can
/// return a `'static` slice.
static PARAMETERS: [crate::infrastructure::scheduler::JobParam; 2] = [
    crate::infrastructure::scheduler::JobParam::number(
        "idle_ttl_minutes",
        30,
        "Reap sessions with no activity for this many minutes.",
    ),
    crate::infrastructure::scheduler::JobParam::number(
        "batch_limit",
        200,
        "Maximum sessions swept in one tick (backlog carries to the next tick).",
    ),
];

pub struct CollabIdleGcService {
    service: Arc<CollabSessionService>,
    /// Effective TTL for stale rows, stored as `Duration` so the
    /// prod path (minutes-granular, from
    /// `OXICLOUD_COLLAB_IDLE_TTL_MINUTES`) and the test escape
    /// hatch (sub-minute, from
    /// `OXICLOUD_COLLAB_IDLE_TTL_SECONDS`) both fit. Clamped to
    /// a minimum of 1 second so a misconfigured env can't reap
    /// everything on every tick.
    default_idle_ttl: Duration,
    default_batch_limit: i64,
    interval: Duration,
}

impl CollabIdleGcService {
    pub const JOB_NAME: &'static str = "collab_idle_gc";

    /// `default_idle_ttl` is the fallback used when the trigger call
    /// doesn't supply an explicit `idle_ttl_minutes` parameter. DI
    /// composes it from either `OXICLOUD_COLLAB_IDLE_TTL_MINUTES`
    /// (production granularity) or `OXICLOUD_COLLAB_IDLE_TTL_SECONDS`
    /// (test escape hatch, seconds wins if both set) and clamps to
    /// a minimum of 1 second.
    ///
    /// `interval` is how often the job fires — plan default is 5
    /// minutes (jobs at this cadence don't need to fire more often
    /// than the eviction cadence). Overridable via
    /// `OXICLOUD_COLLAB_IDLE_SCAN_INTERVAL_SECS`.
    pub fn new(
        service: Arc<CollabSessionService>,
        default_idle_ttl: Duration,
        default_batch_limit: u32,
        interval: Duration,
    ) -> Self {
        Self {
            service,
            default_idle_ttl: default_idle_ttl.max(Duration::from_secs(1)),
            default_batch_limit: default_batch_limit.max(1) as i64,
            interval,
        }
    }

    /// Register with the scheduler. Same chain shape as
    /// `NotificationsCleanupService::register` — keeps DI wiring
    /// terse.
    pub async fn register(self: Arc<Self>, registry: &JobRegistry) -> Arc<Self> {
        registry
            .register(self.clone(), Some(self.interval), None)
            .await;
        self
    }
}

#[async_trait]
impl JobHandler for CollabIdleGcService {
    fn name(&self) -> &str {
        Self::JOB_NAME
    }

    fn description(&self) -> &'static str {
        "Reaps stale collab.doc_sessions rows: force-flush → actor shutdown → row delete. \
         Idle TTL default 30 min (override via `idle_ttl_minutes` param or \
         OXICLOUD_COLLAB_IDLE_TTL_MINUTES). Batch limit caps per-tick sweep size."
    }

    fn mutates(&self) -> Mutates {
        Mutates::Always
    }

    fn parameters(&self) -> &'static [crate::infrastructure::scheduler::JobParam] {
        &PARAMETERS
    }

    async fn run(&self, args: &JobRunArgs) -> JobOutcome {
        // Trigger-time TTL resolution:
        //   * Absent / declared-default value → `default_idle_ttl`
        //     (already carries the env override in Duration form,
        //     so seconds-granular test overrides win here too).
        //   * Explicit non-default value → honour it as minutes.
        //
        // Same "declared default vs env override" pattern as
        // NotificationsCleanupService, but the storage type is
        // Duration to let the env carry sub-minute values for tests.
        let declared_ttl_default = 30_i64;
        let raw_ttl = args.get_number("idle_ttl_minutes", declared_ttl_default);
        let idle_ttl = if raw_ttl == declared_ttl_default {
            self.default_idle_ttl
        } else {
            Duration::from_secs((raw_ttl.max(1) as u64).saturating_mul(60))
        };

        let declared_batch_default = 200_i64;
        let raw_batch = args.get_number("batch_limit", declared_batch_default);
        let batch_limit = if raw_batch == declared_batch_default {
            self.default_batch_limit
        } else {
            raw_batch
        }
        .max(1);

        let cutoff = Utc::now()
            - chrono::Duration::from_std(idle_ttl).unwrap_or(chrono::Duration::minutes(30));

        match self.service.gc_stale(cutoff, batch_limit).await {
            Ok(swept) => {
                let idle_ttl_secs = idle_ttl.as_secs();
                info!(
                    target: "audit",
                    event = "collab.idle_gc_sweep",
                    idle_ttl_secs,
                    swept,
                    "🧹 collab idle-GC: {swept} session(s) reaped (TTL {idle_ttl_secs} s)",
                );
                JobOutcome::ok_with(
                    swept as u64,
                    serde_json::json!({
                        "idle_ttl_secs": idle_ttl_secs,
                        "batch_limit":   batch_limit,
                        "swept":         swept,
                    }),
                )
            }
            Err(e) => JobOutcome::err(format!("collab idle-GC failed: {e}")),
        }
    }
}
