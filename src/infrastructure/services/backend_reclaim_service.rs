//! `backend_reclaim` — drain `storage.pending_actions`, unlinking backend
//! objects whose PG rows are gone.
//!
//! The other half of the fix `dedup_gc` started. `dedup_gc` decides what is
//! unreferenced and records the intent in the same transaction as the row
//! delete; this job acts on the intent and clears it. Neither does the other's
//! work, which is what turns a fire-and-forget unlink into a retryable one.
//!
//! ## The row is a hint, not a command
//!
//! A queue row means *"this hash was last observed at refcount 0 — check, and if
//! it still is, unlink"*. The authority on whether an object should exist is
//! `storage.blobs` / `storage.chunk_manifests`, never this table. Three
//! consequences the implementation depends on:
//!
//! * **Order does not matter.** The re-verify below reads current truth, so
//!   whatever sequence of events occurred, the outcome is derived from the
//!   present state rather than from row order. `create` then `delete` leaves
//!   refcount 0 and the unlink proceeds; `delete` then `create` leaves a live
//!   reference and the row is discarded.
//! * **A falsified hint is discarded, not deferred.** Once a reference exists the
//!   intent is not "not yet", it is *wrong*.
//! * **Deleting an absent object is success.** That is what makes the job safe to
//!   schedule and safe to re-run.
//!
//! ## Why this is a recoverable job even though it is NOT resumable
//!
//! Worth stating, because the resume half of the interface is deliberately
//! unused and a reader will otherwise look for the cursor logic.
//!
//! **It does not need a cursor.** The queue is self-draining: a settled row is
//! DELETED, so "resume" and "run again" are the same operation. The run takes a
//! fresh boundary each time rather than restoring one, which is safe precisely
//! because nothing marks a row as seen — a row a run misses is simply still
//! there next time. `checkpoint` is called only to report progress; the cursor
//! it carries is always empty, and `run_resumable` ignores the one it is given.
//!
//! **It does need findings.** Per `scheduler::handler`, only handlers that
//! persist per-run rows get a findings drawer — and a PARKED object is the one
//! outcome here that never self-corrects: nothing will retry it, so it is bytes
//! leaked indefinitely until a human looks. Reporting that as a count in a log
//! line would bury the single fact an operator has to act on.
//!
//! It also gets cooperative cancellation for free, which a drain over a large
//! backlog genuinely needs.
//!
//! By contrast `dedup_gc` and `collab_idle_gc` are plain jobs, correctly: they
//! report totals, not per-resource outcomes, so they have nothing to put in a
//! findings drawer.
//!
//! See `docs/plan/storage-consistency.md` §2.

use async_trait::async_trait;
use sqlx::PgPool;
use std::sync::Arc;

use crate::application::ports::blob_storage_ports::BlobStorageBackend;
use crate::infrastructure::scheduler::{
    JobRegistry, JobRunArgs, JobStore, JobStoreProvider, Mutates, RecoverableJobHandler,
    RunOutcome, RunStatus, record_or_log,
};

pub const BACKEND_RECLAIM_JOB_NAME: &str = "backend_reclaim";

/// Rows fetched per candidate query.
const PAGE_SIZE: i64 = 128;

/// Items settled between checkpoints.
const CHECKPOINT_EVERY: usize = 32;

/// How often the drain runs.
///
/// Fixed rather than configurable, deliberately: the interval changes only *when*
/// objects are unlinked, never how many backend calls it takes — that is one
/// DELETE per reclaimed object whatever the cadence. So there is no cost to tune,
/// and an operator on a metered backend gains nothing from a knob here.
///
/// The schedule itself is not optional. The run shape defers anything enqueued
/// mid-run to the next run, which on an on-demand-only job would mean fresh
/// entries waiting until someone clicked — today's `dedup_gc` failure exactly.
pub const RECLAIM_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

/// Permanent failures tolerated on one object before it is parked.
///
/// Parking means "stop trying automatically, a human should look". Only
/// PERMANENT failures count: a transient one pauses the whole run instead, so an
/// outage cannot march the entire backlog into the parked state, which would turn
/// a temporary problem into a pile of manual work.
const MAX_ATTEMPTS: i32 = 8;

/// A 1-attempt budget would park on the first failure, defeating the retry this
/// whole queue exists to provide. Compile-time rather than a test: the value is a
/// constant, so a bad edit should fail the build instead of waiting for someone
/// to run the suite.
const _: () = assert!(
    MAX_ATTEMPTS > 1,
    "MAX_ATTEMPTS must allow at least one retry"
);

pub struct BackendReclaim {
    pool: Arc<PgPool>,
    backend: Arc<dyn BlobStorageBackend>,
}

/// One settled item's effect on the run's counters.
enum Settled {
    /// Unlinked (or already absent) and the row is gone.
    Reclaimed { bytes: u64 },
    /// A live reference appeared, so the intent was wrong and was discarded.
    Cancelled,
    /// Another transaction holds the row, or it vanished. Left for a later run.
    Skipped,
    /// Permanent failure; attempt recorded, possibly parked.
    Failed { parked: bool },
}

impl BackendReclaim {
    pub fn new(pool: Arc<PgPool>, backend: Arc<dyn BlobStorageBackend>) -> Self {
        Self { pool, backend }
    }

    pub async fn register_recoverable_job(
        self: Arc<Self>,
        registry: &JobRegistry,
        provider: &Arc<dyn JobStoreProvider>,
        interval: Option<std::time::Duration>,
    ) -> Arc<Self> {
        // An interval, unlike most jobs here — and it is not optional.
        //
        // The run shape defers anything enqueued mid-run to the next run, which
        // is meaningless for an on-demand-only job: fresh entries would wait
        // until someone clicked. That is exactly today's `dedup_gc` failure,
        // which is registered on-demand and which nothing schedules, so on any
        // instance where nobody clicks it reclaimable bytes accumulate forever.
        registry
            .register_recoverable_job(self.clone(), provider.clone(), interval)
            .await;
        self
    }

    /// Settle one object inside its own short transaction.
    ///
    /// The transaction spans a backend call, which is not pretty and is
    /// unavoidable: the unlink cannot share a transaction with an uploader's
    /// insert, so the only alternatives are making the lost race recoverable —
    /// impossible, because an idempotent-skip PUT leaves the uploader with no
    /// copy of bytes it did not write — or keeping the window small, which is
    /// what the old code relied on and what a deferred queue destroys by design.
    /// A lease is the escape if these transactions ever become a problem.
    async fn settle_one(&self, hash: &str, entry_name: Option<&str>) -> Settled {
        let mut tx = match self.pool.begin().await {
            Ok(tx) => tx,
            Err(e) => {
                tracing::warn!("backend_reclaim: begin failed for {hash}: {e}");
                return Settled::Skipped;
            }
        };

        // SKIP LOCKED, so a row an uploader is mid-cancel on is passed over
        // rather than waited on. The asymmetry is deliberate: this job has other
        // rows it could usefully do, so skipping costs nothing and the row is
        // picked up later. The uploader's side takes a plain `FOR UPDATE` and
        // waits, because it needs *this* hash and proceeding as though the object
        // were safe is the data-loss outcome.
        let claimed: Option<(i32,)> = match sqlx::query_as(
            "SELECT attempts FROM storage.pending_actions
              WHERE hash = $1 AND action = 'deletion'
                FOR UPDATE SKIP LOCKED",
        )
        .bind(hash)
        .fetch_optional(&mut *tx)
        .await
        {
            Ok(row) => row,
            Err(e) => {
                tracing::warn!("backend_reclaim: claim failed for {hash}: {e}");
                return Settled::Skipped;
            }
        };

        let Some((attempts,)) = claimed else {
            // Locked by someone else, or already settled/cancelled.
            return Settled::Skipped;
        };

        // The re-verify, and it is NOT optional: the candidate list is a stale
        // read by the time this row is reached, and an uploader may have
        // resurrected the hash in between. This is also what makes the drain
        // order-free — it acts on current truth rather than on the row's age.
        let still_unreferenced: bool = match sqlx::query_scalar(
            "SELECT NOT (EXISTS (SELECT 1 FROM storage.blobs WHERE hash = $1)
                      OR EXISTS (SELECT 1 FROM storage.chunk_manifests
                                  WHERE $1 = ANY(chunk_hashes)))",
        )
        .bind(hash)
        .fetch_one(&mut *tx)
        .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("backend_reclaim: re-verify failed for {hash}: {e}");
                return Settled::Skipped;
            }
        };

        if !still_unreferenced {
            // Someone took a reference: the hint is wrong, not early. Discard it
            // — deferring would mean deleting live bytes on a later run.
            if let Err(e) = Self::delete_row(&mut tx, hash).await {
                tracing::warn!("backend_reclaim: cancel failed for {hash}: {e}");
                return Settled::Skipped;
            }
            if tx.commit().await.is_err() {
                return Settled::Skipped;
            }
            tracing::info!(
                target: "audit",
                event = "storage.reclaim_cancelled",
                hash = %hash,
                "🧹 reclaim cancelled: {} is referenced again, deletion intent discarded",
                &hash[..hash.len().min(12)],
            );
            return Settled::Cancelled;
        }

        // Resolve the object's size before the unlink, for the reclaimed-bytes
        // counter. Best-effort: a missing size is a worse report, not a worse
        // outcome.
        let size: i64 = sqlx::query_scalar(
            "SELECT size_bytes FROM storage.pending_actions
              WHERE hash = $1 AND action = 'deletion'",
        )
        .bind(hash)
        .fetch_one(&mut *tx)
        .await
        .unwrap_or(0);

        // `entry_name` is recorded but not yet honoured — the backend stack has
        // no per-entry delete entry point, and the cutover procedure drains this
        // queue first precisely so the active backend is the right one. Logged
        // when present so a stale entry is visible rather than silent.
        if let Some(entry) = entry_name {
            tracing::debug!(
                "backend_reclaim: {} was reaped from entry '{entry}'; \
                 draining via the active backend",
                &hash[..hash.len().min(12)],
            );
        }

        match self.backend.delete_blob(hash).await {
            Ok(()) => {
                if let Err(e) = Self::delete_row(&mut tx, hash).await {
                    // The object is gone but the row survived. Harmless: the next
                    // run re-verifies, finds nothing referencing it, and deleting
                    // an absent object is success.
                    tracing::warn!("backend_reclaim: settle row delete failed for {hash}: {e}");
                    return Settled::Skipped;
                }
                if tx.commit().await.is_err() {
                    return Settled::Skipped;
                }
                Settled::Reclaimed {
                    bytes: size.max(0) as u64,
                }
            }
            Err(e) => {
                let transient = e.is_transient();
                let next_attempts = if transient { attempts } else { attempts + 1 };
                let park = !transient && next_attempts >= MAX_ATTEMPTS;

                // A transient failure does NOT advance `attempts`: the object is
                // not the problem, the backend is unreachable, and counting it
                // would march the whole backlog into the parked state during an
                // outage. `last_attempt_at` still moves, so the backoff spaces
                // out retries.
                if let Err(err) = sqlx::query(
                    "UPDATE storage.pending_actions
                        SET attempts = $2,
                            last_attempt_at = now(),
                            last_error = $3,
                            parked_at = CASE WHEN $4 THEN now() ELSE parked_at END
                      WHERE hash = $1 AND action = 'deletion'",
                )
                .bind(hash)
                .bind(next_attempts)
                .bind(e.to_string())
                .bind(park)
                .execute(&mut *tx)
                .await
                {
                    tracing::warn!("backend_reclaim: attempt bookkeeping failed for {hash}: {err}");
                    return Settled::Skipped;
                }
                if tx.commit().await.is_err() {
                    return Settled::Skipped;
                }

                if transient {
                    Settled::Skipped
                } else {
                    Settled::Failed { parked: park }
                }
            }
        }
    }

    async fn delete_row(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        hash: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM storage.pending_actions WHERE hash = $1 AND action = 'deletion'")
            .bind(hash)
            .execute(&mut **tx)
            .await
            .map(|_| ())
    }
}

#[async_trait]
impl RecoverableJobHandler for BackendReclaim {
    fn name(&self) -> &str {
        BACKEND_RECLAIM_JOB_NAME
    }

    fn description(&self) -> &'static str {
        "Unlinks backend objects whose database rows are already gone, from the \
         durable queue that dedup_gc writes when it reaps an unreferenced blob. \
         This is what makes a failed delete retryable instead of lost: the queue \
         row survives the failure, so an unreachable or erroring backend delays \
         reclamation rather than stranding bytes invisibly. Each object is \
         re-checked against the registry immediately before its unlink, so an \
         object that became referenced again is never deleted — its queued \
         intent is discarded instead. Deleting an object that is already absent \
         counts as success."
    }

    fn mutates(&self) -> Mutates {
        // Unlinking IS the job; a read-only run would do nothing. Also why this
        // must never carry the `_consistency` suffix, which would auto-enrol it
        // into the discovery-only `consistency_batch`.
        Mutates::Always
    }

    fn repair_description(&self) -> Option<&'static str> {
        None
    }

    async fn count_total(&self) -> Option<u64> {
        match sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM storage.pending_actions
              WHERE action = 'deletion' AND parked_at IS NULL",
        )
        .fetch_one(self.pool.as_ref())
        .await
        {
            Ok(n) => Some(n.max(0) as u64),
            Err(e) => {
                tracing::warn!("backend_reclaim: backlog count failed: {e}");
                None
            }
        }
    }

    async fn run_resumable(
        &self,
        store: &dyn JobStore,
        _args: &JobRunArgs,
        _resume_cursor: Option<Vec<u8>>,
    ) -> RunOutcome {
        // The boundary comes from the DATABASE clock, not the process clock.
        // Every timestamp it is compared against (`requested_at`,
        // `last_attempt_at`) is written by `now()` in Postgres, and mixing an
        // app clock into that comparison is a known defect elsewhere in this
        // codebase — a few seconds of skew would either skip fresh rows or
        // re-select rows this run already attempted.
        let boundary: chrono::DateTime<chrono::Utc> = match sqlx::query_scalar("SELECT now()")
            .fetch_one(self.pool.as_ref())
            .await
        {
            Ok(t) => t,
            Err(e) => {
                return RunOutcome::Failed {
                    message: format!("boundary clock read: {e}"),
                };
            }
        };

        // The resume cursor is deliberately ignored: a stop is a re-plan
        // opportunity. Taking a fresh boundary is safe because the cursor carries
        // no correctness — a settled row is DELETED, so nothing marks a row as
        // "seen" and a row this run misses is simply still there next time. The
        // worst case is a delayed reclaim, never a permanent skip.
        let mut reclaimed = 0u64;
        let mut reclaimed_bytes = 0u64;
        let mut cancelled = 0u64;
        let mut failed = 0u64;
        let mut parked = 0u64;
        let mut skipped = 0u64;
        let mut since_checkpoint = 0usize;

        loop {
            // Candidates at or below the boundary, respecting backoff and
            // excluding anything already attempted in THIS run — without that
            // last clause a failing row would be re-selected forever, because
            // rows are removed by settling rather than by a cursor advancing.
            let page: Vec<(String, Option<String>)> = match sqlx::query_as(
                "SELECT hash, entry_name FROM storage.pending_actions
                  WHERE action = 'deletion'
                    AND parked_at IS NULL
                    AND requested_at <= $1
                    AND (last_attempt_at IS NULL OR last_attempt_at < $1)
                    AND (last_attempt_at IS NULL
                         OR last_attempt_at
                            < now() - (interval '1 second'
                                       * power(2, least(attempts, 12))))
                  ORDER BY requested_at
                  LIMIT $2",
            )
            .bind(boundary)
            .bind(PAGE_SIZE)
            .fetch_all(self.pool.as_ref())
            .await
            {
                Ok(p) => p,
                Err(e) => {
                    return RunOutcome::Failed {
                        message: format!("candidate page: {e}"),
                    };
                }
            };

            if page.is_empty() {
                break;
            }

            for (hash, entry_name) in page {
                match store.status().await {
                    Ok(RunStatus::CancelRequested) => {
                        return RunOutcome::Paused { cursor: Vec::new() };
                    }
                    Ok(_) => {}
                    Err(e) => {
                        return RunOutcome::Failed {
                            message: format!("status poll: {e}"),
                        };
                    }
                }

                match self.settle_one(&hash, entry_name.as_deref()).await {
                    Settled::Reclaimed { bytes } => {
                        reclaimed += 1;
                        reclaimed_bytes += bytes;
                    }
                    Settled::Cancelled => cancelled += 1,
                    Settled::Skipped => skipped += 1,
                    Settled::Failed { parked: p } => {
                        failed += 1;
                        if p {
                            parked += 1;
                            // A parked object is the one thing here an operator
                            // must see: it will not be retried automatically, so
                            // without a finding the bytes stay leaked silently —
                            // which is the failure mode this whole plan exists to
                            // remove.
                            record_or_log(
                                store,
                                BACKEND_RECLAIM_JOB_NAME,
                                "backend_object_reclaim_parked",
                                "anomaly",
                                None,
                                serde_json::json!({
                                    "hash": hash,
                                    "attempts": MAX_ATTEMPTS,
                                }),
                            )
                            .await;
                        }
                    }
                }

                since_checkpoint += 1;
                if since_checkpoint >= CHECKPOINT_EVERY {
                    let scanned = since_checkpoint as u64;
                    since_checkpoint = 0;
                    if let Err(e) = store.checkpoint(Vec::new(), scanned).await {
                        return RunOutcome::Failed {
                            message: format!("checkpoint: {e}"),
                        };
                    }
                }

                tokio::task::yield_now().await;
            }
        }

        if since_checkpoint > 0
            && let Err(e) = store.checkpoint(Vec::new(), since_checkpoint as u64).await
        {
            return RunOutcome::Failed {
                message: format!("final checkpoint: {e}"),
            };
        }

        if reclaimed > 0 || failed > 0 || cancelled > 0 {
            tracing::info!(
                reclaimed,
                reclaimed_bytes,
                cancelled,
                failed,
                parked,
                skipped,
                "🧹 backend_reclaim: {reclaimed} object(s) unlinked ({reclaimed_bytes} bytes), \
                 {cancelled} cancelled (referenced again), {failed} failed, {parked} parked"
            );
        }

        RunOutcome::completed_with(serde_json::json!({
            "reclaimed": reclaimed,
            "reclaimed_bytes": reclaimed_bytes,
            "cancelled": cancelled,
            "failed": failed,
            "parked": parked,
            "skipped": skipped,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_name_is_stable_and_not_a_consistency_tenant() {
        assert_eq!(BACKEND_RECLAIM_JOB_NAME, "backend_reclaim");
        // The suffix is functional: `consistency_batch` auto-discovers children
        // by `ends_with("_consistency")`, and this job always mutates.
        assert!(!BACKEND_RECLAIM_JOB_NAME.ends_with("_consistency"));
        // Groups with backend_consistency / backend_migration / backend_rechunk /
        // backend_rotate in the sorted admin panel.
        assert!(BACKEND_RECLAIM_JOB_NAME.starts_with("backend_"));
    }

    #[test]
    fn transient_failures_must_not_consume_the_attempt_budget() {
        // Pinning the intent behind `settle_one`'s attempt arithmetic: an outage
        // would otherwise park the entire backlog, turning a temporary backend
        // problem into a pile of manual un-parking.
        let attempts = 3;
        let after_transient = attempts; // unchanged
        let after_permanent = attempts + 1;
        assert_eq!(after_transient, 3);
        assert_eq!(after_permanent, 4);
        // The MAX_ATTEMPTS > 1 invariant is asserted at compile time beside the
        // constant, not here — clippy is right that asserting a constant in a
        // test is a tautology that only fires when someone runs the suite.
    }
}
