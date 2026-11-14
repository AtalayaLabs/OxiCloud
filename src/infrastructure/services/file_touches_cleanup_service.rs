//! `file_touches_cleanup` scheduled job — daily retention sweep.
//!
//! Deletes rows from `storage.file_content_touches` older than the retention
//! window. That table holds one row per (file, person) — the latest content
//! change that person made to that file — and powers
//! `GET /api/files/recent-changes`. See
//! `docs/plan/recently-modified.md` §1b.
//!
//! **Why the sweep is mandatory rather than hygiene.** The table is bounded by
//! files × distinct editors, not by edit count, so activity cannot inflate it.
//! What it does accumulate is *history*: without a sweep it keeps a row for
//! every file anyone has ever touched, which is the one way this design
//! degrades over years instead of under load.
//!
//! Retention comes from `OXICLOUD_FILE_TOUCHES_RETENTION_DAYS` (default 120),
//! applied at dispatch, and is deliberately **wider than the 90-day maximum
//! the query accepts**: the window bounds what a caller may ask for, retention
//! bounds what is kept, and equal values would let a boundary request read a
//! half-pruned window.
//!
//! It runs whether or not `enable_recent_changes` is on. Touches are written
//! regardless — operators may want the record for auditability, and enabling
//! the view later then has history from day one — so a deployment that never
//! enables the view must still not grow the table forever.
//!
//! A deleted touch is never data loss: the file, its `content_modified_at` and
//! its `content_modified_by` are untouched. This table is a cache of recency
//! and the retention window is its ceiling.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::PgPool;
use tracing::info;

use crate::infrastructure::scheduler::{JobHandler, JobOutcome, JobRegistry, JobRunArgs, Mutates};

/// Parameter declaration table. Module scope so `JobHandler::parameters`
/// returns a `'static` slice without stack-allocating per call.
static PARAMETERS: [crate::infrastructure::scheduler::JobParam; 1] =
    [crate::infrastructure::scheduler::JobParam::number(
        "retention_days",
        120,
        "Delete content-touch rows older than this many days.",
    )];

/// The declared default, kept as a named constant because `run()` compares
/// against it to decide whether the operator's env override applies — see the
/// comment there.
const DECLARED_DEFAULT_DAYS: i64 = 120;

pub struct FileTouchesCleanupService {
    /// Maintenance pool: a retention sweep must not compete with user
    /// requests for the user-facing pool (`infrastructure::db::DbPools`).
    pool: Arc<PgPool>,
    /// Default retention when the trigger call supplied no explicit
    /// `retention_days`. Read from the env at boot; clamped to at least 1 day
    /// because 0 would purge every row on every tick.
    default_retention_days: i64,
}

impl FileTouchesCleanupService {
    pub const JOB_NAME: &'static str = "file_touches_cleanup";

    pub fn new(pool: Arc<PgPool>, default_retention_days: u32) -> Self {
        Self {
            pool,
            default_retention_days: default_retention_days.max(1) as i64,
        }
    }

    /// Daily, the same tier as `notifications_cleanup` and `trash_cleanup`:
    /// retention is a "days" concept, so a finer cadence buys nothing.
    fn interval() -> Duration {
        Duration::from_secs(24 * 3600)
    }

    /// Chained DI helper, same shape as the other cleanup services.
    pub async fn register(self: Arc<Self>, registry: &JobRegistry) -> Arc<Self> {
        registry
            .register(self.clone(), Some(Self::interval()), None)
            .await;
        self
    }
}

#[async_trait]
impl JobHandler for FileTouchesCleanupService {
    fn name(&self) -> &str {
        Self::JOB_NAME
    }

    fn description(&self) -> &'static str {
        "Deletes content-touch rows (storage.file_content_touches) older \
         than the retention window (default 120 days, override via \
         `retention_days` or OXICLOUD_FILE_TOUCHES_RETENTION_DAYS). The \
         files themselves and both their content clocks are untouched."
    }

    fn mutates(&self) -> Mutates {
        Mutates::Always
    }

    fn parameters(&self) -> &'static [crate::infrastructure::scheduler::JobParam] {
        &PARAMETERS
    }

    async fn run(&self, args: &JobRunArgs) -> JobOutcome {
        // Declared defaults are seeded by the engine before `run`, so the
        // parameter is always present — either with the caller's value or with
        // the declared 120. "Declared default AND env override differs" is
        // treated as "use the env override", which keeps
        // OXICLOUD_FILE_TOUCHES_RETENTION_DAYS effective without teaching the
        // engine per-instance defaults. Same reasoning as
        // `notifications_cleanup_service`.
        let raw = args.get_number("retention_days", DECLARED_DEFAULT_DAYS);
        let retention_days = if raw == DECLARED_DEFAULT_DAYS {
            self.default_retention_days
        } else {
            raw
        }
        .max(1);

        // The cutoff is computed by Postgres, not by the app: the sweep
        // compares against `touched_at`, which the write path sets from the
        // database clock (or a client-asserted mtime), and mixing an app clock
        // into that comparison is how a sweep deletes a row it should have
        // kept. See `bug_job_run_timestamps_two_clocks`.
        let deleted = sqlx::query_scalar::<_, i64>(
            "WITH gone AS (
                 DELETE FROM storage.file_content_touches
                  WHERE touched_at < NOW() - ($1 || ' days')::interval
                 RETURNING 1
             )
             SELECT COUNT(*)::bigint FROM gone",
        )
        .bind(retention_days.to_string())
        .fetch_one(self.pool.as_ref())
        .await;

        match deleted {
            Ok(removed) => {
                info!(
                    target: "audit",
                    event = "file_touches.retention_sweep",
                    retention_days,
                    removed,
                    "🧹 content-touch retention sweep: {removed} row(s) purged (retention {retention_days} d)"
                );
                JobOutcome::ok_with(
                    removed as u64,
                    serde_json::json!({
                        "retention_days": retention_days,
                        "removed":        removed,
                    }),
                )
            }
            Err(e) => JobOutcome::err(format!("file touches cleanup failed: {e}")),
        }
    }
}
