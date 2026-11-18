//! `list_cleanup` scheduled job — drains `auth.cleanup_queue`.
//!
//! See `migrations/20261118000000_favorites_recent_cleanup_queue.sql`
//! for the end-to-end design (issue #913). Short version:
//!
//! * A trigger on `storage.role_grants` DELETE enqueues every
//!   affected user (user subjects directly, group subjects fanned
//!   out through transitive membership) into `auth.cleanup_queue`.
//! * This job drains the queue in batches. Per queued user, it
//!   runs every registered `ListCleanup` implementor (today:
//!   `FavoritesService` and `RecentService`), each of which
//!   iterates its own user-scoped rows and deletes targets the
//!   user can no longer read via SOME grant path.
//! * Processed rows are then deleted from the queue. The DELETE
//!   matches both `user_id` AND `enqueued_at`; a revoke that
//!   fired between our SELECT and our DELETE bumps the timestamp
//!   (the trigger uses `ON CONFLICT DO UPDATE SET enqueued_at`),
//!   so the stale DELETE no-ops and the row survives for the
//!   next tick.
//!
//! **Adding a new list.** Implement the
//! [`crate::application::ports::list_cleanup::ListCleanup`] trait
//! on the new service and push an `Arc` of it into `lists` at DI
//! wiring time. The drain code needs no changes.
//!
//! Non-recoverable on purpose: `ListCleanup::cleanup_user_orphans`
//! is idempotent (re-running against the current ACL state
//! produces the same result), the whole operation is DB-only,
//! and a failed user is simply left in the queue for the next
//! tick. No cursor to persist.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::PgPool;
use tracing::{info, warn};
use uuid::Uuid;

use crate::application::ports::list_cleanup::ListCleanup;
use crate::infrastructure::scheduler::{JobHandler, JobOutcome, JobRegistry, JobRunArgs, Mutates};

/// How many queue rows to drain per tick.
///
/// Bounded so a huge mass revoke (e.g. group-admin removes 10k
/// users from a group with many grants) doesn't stall other
/// scheduled jobs sharing the runtime. Remaining rows carry
/// over to the next tick with no additional pressure — queue PK
/// gives natural dedup.
const DRAIN_BATCH: i64 = 500;

pub struct ListCleanupService {
    pool: Arc<PgPool>,
    lists: Vec<Arc<dyn ListCleanup>>,
    interval: Duration,
}

impl ListCleanupService {
    pub const JOB_NAME: &'static str = "list_cleanup";

    pub fn new(pool: Arc<PgPool>, lists: Vec<Arc<dyn ListCleanup>>, interval_secs: u64) -> Self {
        Self {
            pool,
            lists,
            // Floor at 10s — a mis-set `0` would spin the scheduler;
            // and nothing below 10s is observably better for the
            // security-fix UX (stale window post-revoke).
            interval: Duration::from_secs(interval_secs.max(10)),
        }
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Chainable self-registration — one line in DI.
    pub async fn register(self: Arc<Self>, registry: &JobRegistry) -> Arc<Self> {
        let interval = self.interval;
        registry.register(self.clone(), Some(interval), None).await;
        self
    }

    /// One drain pass. Exposed for the admin trigger + tests; the
    /// scheduled run calls this through `JobHandler::run`.
    pub async fn drain(&self) -> Result<DrainOutcome, sqlx::Error> {
        // Pick up to DRAIN_BATCH queued users, FOR UPDATE SKIP LOCKED
        // so two concurrent runners (shouldn't happen under the
        // scheduler's exclusivity guard, but survives it) don't
        // process the same row twice.
        let picked: Vec<(Uuid, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
            r#"
            SELECT user_id, enqueued_at
            FROM auth.cleanup_queue
            ORDER BY enqueued_at
            LIMIT $1
            FOR UPDATE SKIP LOCKED
            "#,
        )
        .bind(DRAIN_BATCH)
        .fetch_all(&*self.pool)
        .await?;

        let mut processed = 0u64;
        let mut failed = 0u64;
        let mut total_removed = 0u64;
        let mut delete_targets: Vec<(Uuid, chrono::DateTime<chrono::Utc>)> =
            Vec::with_capacity(picked.len());

        for (user_id, enqueued_at) in picked {
            let mut any_failed = false;
            let mut user_removed = 0u64;
            for list in &self.lists {
                match list.cleanup_user_orphans(user_id).await {
                    Ok(n) => user_removed += n,
                    Err(e) => {
                        any_failed = true;
                        warn!(
                            target: "audit",
                            event = "list_cleanup.user_failed",
                            reason = "sweep_error",
                            list = list.list_name(),
                            user_id = %user_id,
                            error = %e,
                            "👮🏻‍♂️ list sweep failed for queued user; row left for next tick",
                        );
                    }
                }
            }
            if any_failed {
                // At least one list sweep failed — leave the queue
                // row for the next tick so no signal is lost. Lists
                // that succeeded already deleted their orphans;
                // that's harmless on retry (idempotent sweep).
                failed += 1;
            } else {
                processed += 1;
                total_removed += user_removed;
                delete_targets.push((user_id, enqueued_at));
            }
        }

        // DELETE only (user_id, enqueued_at) pairs we actually swept,
        // so a revoke that fired mid-tick (bumping `enqueued_at`
        // through the trigger's `DO UPDATE`) leaves its row for the
        // next tick instead of being silently absorbed by this one.
        if !delete_targets.is_empty() {
            let user_ids: Vec<Uuid> = delete_targets.iter().map(|(u, _)| *u).collect();
            let enqueued_ats: Vec<chrono::DateTime<chrono::Utc>> =
                delete_targets.iter().map(|(_, t)| *t).collect();
            sqlx::query(
                r#"
                DELETE FROM auth.cleanup_queue cq
                USING UNNEST($1::uuid[], $2::timestamptz[]) AS d(user_id, enqueued_at)
                WHERE cq.user_id = d.user_id
                  AND cq.enqueued_at = d.enqueued_at
                "#,
            )
            .bind(&user_ids)
            .bind(&enqueued_ats)
            .execute(&*self.pool)
            .await?;
        }

        Ok(DrainOutcome {
            processed,
            failed,
            rows_removed: total_removed,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DrainOutcome {
    pub processed: u64,
    pub failed: u64,
    pub rows_removed: u64,
}

#[async_trait]
impl JobHandler for ListCleanupService {
    fn name(&self) -> &str {
        Self::JOB_NAME
    }

    fn description(&self) -> &'static str {
        "Drains `auth.cleanup_queue` — the per-user queue populated \
         by the `role_grants` DELETE trigger. For each queued user, \
         runs every registered list-cleanup port (favorites + recent \
         today) so any row whose target the caller can no longer \
         read gets dropped. Closes the metadata leak described in \
         issue #913."
    }

    fn mutates(&self) -> Mutates {
        // Even an empty queue tick is a mutating capability — the
        // drain exists to DELETE rows the sweep deems orphaned.
        Mutates::Always
    }

    async fn run(&self, _args: &JobRunArgs) -> JobOutcome {
        match self.drain().await {
            Ok(outcome) => {
                if outcome.processed > 0 || outcome.failed > 0 {
                    info!(
                        target: "audit",
                        event = "list_cleanup.drained",
                        processed = outcome.processed,
                        failed = outcome.failed,
                        rows_removed = outcome.rows_removed,
                        "🧹 list-cleanup drain tick",
                    );
                }
                JobOutcome::ok_with(
                    outcome.processed,
                    serde_json::json!({
                        "failed": outcome.failed,
                        "rows_removed": outcome.rows_removed,
                    }),
                )
            }
            Err(e) => JobOutcome::err(format!("list_cleanup drain failed: {e}")),
        }
    }
}
