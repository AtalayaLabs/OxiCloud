use sqlx::PgPool;
use std::sync::Arc;
use tracing::{debug, error, info, instrument};

use crate::infrastructure::repositories::pg::transaction_utils::retry_on_deadlock;

/// Background drainer for `storage.tree_etag_dirty` — the asynchronous half
/// of the tree-ETag bump (see migration `20260627000000_async_tree_etag_queue`).
///
/// The statement triggers on `storage.files` / `storage.folders` only append
/// "bump request" lpaths to the queue, taking zero folder-row locks, so any
/// number of concurrent uploads never conflict with each other. This service
/// turns those requests into actual `tree_modified_at` updates: every
/// `interval_ms` it drains the queue and bumps the deduplicated ancestor
/// closure in ONE batched UPDATE with deterministic id-order locking.
///
/// Correctness invariants this relies on:
///   * The drain (DELETE) and the bump (UPDATE) are one statement — one
///     transaction. A crash mid-flush rolls both back and the queue rows
///     survive; the first flush after startup drains any leftovers, so no
///     bump is ever lost (a lost bump means sync clients never discover
///     the change).
///   * Queue rows are dual-keyed (`lpath` captured at mutation time,
///     `folder_id` re-resolved at flush time) and the target set is the
///     UNION of both — so neither a folder deleted/moved away (captured
///     lpath wins) nor a folder re-rooted by a move inside the flush
///     window (resolved lpath wins) can lose its pending bump.
///   * The bump is monotonic per folder:
///     `GREATEST(NOW(), tree_modified_at + interval '1 second')`. Folder
///     ETags have whole-second granularity (`{id}-{epoch_seconds}`, see
///     `Folder::compute_etag`); two flushes inside the same wall-clock
///     second must still produce two distinct ETags or a client polling
///     between them would permanently miss the second change.
///   * The flusher locks victims in id order, so it cannot deadlock
///     against itself. It can still lose a race against a folder move's
///     descendant lpath cascade, which is why the statement runs under
///     [`retry_on_deadlock`]; a retried flush re-drains the intact queue,
///     invisible to users.
///   * The flusher is not the only writer of `tree_modified_at`: structural
///     folder changes bump their ancestor chain in the writing statement
///     (`storage.tree_etag_bump_or_enqueue`, migration
///     `20261104000000_tree_etag_immediate_folder_bumps`). That bump uses
///     `SKIP LOCKED` and never waits, so the flusher may wait on it but
///     not the reverse; anything it skips is enqueued and lands here.
pub struct TreeEtagFlushService {
    pool: Arc<PgPool>,
    interval_ms: u64,
}

/// Rows drained from the queue per statement. Bounds the lock footprint of
/// one flush under burst load (bulk trash purge, recursive copy); leftovers
/// are picked up by the in-tick drain loop or the next tick.
const DRAIN_BATCH: i64 = 5_000;

/// Max drain statements per tick, so a pathological backlog cannot
/// monopolise the maintenance connection forever within one tick.
const MAX_BATCHES_PER_TICK: u32 = 8;

impl TreeEtagFlushService {
    pub fn new(pool: Arc<PgPool>, interval_ms: u64) -> Self {
        Self {
            pool,
            // Floor the cadence so a misconfiguration can't busy-loop the
            // maintenance pool.
            interval_ms: interval_ms.max(100),
        }
    }

    /// Spawn the flush loop. Fire-and-forget: the loop logs and survives
    /// every error (an exited loop would silently freeze all folder ETags),
    /// and the first flush runs immediately to drain rows left over from a
    /// previous run.
    #[instrument(skip(self))]
    pub fn start_flush_job(&self) {
        let pool = self.pool.clone();
        let interval_ms = self.interval_ms;
        info!(
            "Starting tree-ETag flush job (every {}ms, batch {})",
            interval_ms, DRAIN_BATCH
        );
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(interval_ms));
            // If a flush overruns the interval, fire the next one a full
            // interval later instead of bursting to catch up.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                match Self::flush(&pool).await {
                    Ok((0, _)) => {}
                    Ok((drained, bumped)) => {
                        debug!(
                            "Tree-ETag flush: drained {} queue row(s), bumped {} folder(s)",
                            drained, bumped
                        );
                    }
                    Err(e) => {
                        error!("Tree-ETag flush failed (queue preserved, will retry): {e}");
                    }
                }
            }
        });
    }

    /// Drain the dirty queue and bump the ancestor closure. Returns
    /// `(queue rows drained, folder rows bumped)` summed over the batches
    /// processed this tick.
    async fn flush(pool: &PgPool) -> Result<(i64, i64), sqlx::Error> {
        let mut total_drained = 0i64;
        let mut total_bumped = 0i64;

        for _ in 0..MAX_BATCHES_PER_TICK {
            let (drained, bumped) = retry_on_deadlock("tree_etag_flush", || {
                sqlx::query_as::<_, (i64, i64)>(
                    r#"
                    WITH drained AS (
                        DELETE FROM storage.tree_etag_dirty
                         WHERE id IN (SELECT id
                                        FROM storage.tree_etag_dirty
                                       ORDER BY id
                                       LIMIT $1)
                        RETURNING lpath, folder_id, drive_id
                    ),
                    targets AS (
                        -- Captured chain: covers target folders deleted or
                        -- moved away since enqueue (the old location's
                        -- surviving ancestors still get their bump). drive_id
                        -- comes along so the victims walk can enforce
                        -- cross-drive isolation (D0-13).
                        SELECT lpath, drive_id FROM drained
                        UNION
                        -- Flush-time resolution: a folder MOVED since
                        -- enqueue had its subtree's lpaths rewritten, so
                        -- the captured lpath no longer matches it — its id
                        -- resolves to the CURRENT chain instead. Without
                        -- this, a bump queued just before a move would be
                        -- silently lost and sync clients would never
                        -- discover the change.
                        SELECT fo.lpath, fo.drive_id
                          FROM storage.folders fo
                          JOIN drained d ON fo.id = d.folder_id
                    ),
                    victims AS (
                        -- `lpath @> target` = the target folder itself plus
                        -- every ancestor (GiST-indexed). The `drive_id`
                        -- predicate prevents a numerically-overlapping
                        -- lpath in a SIBLING drive from spuriously matching
                        -- (D0-13). Rows from old queue entries (pre-M4) have
                        -- NULL `drive_id` — `IS NOT DISTINCT FROM` falls
                        -- back to pure lpath matching for those, preserving
                        -- the rollover semantics for any rows enqueued
                        -- between this migration committing and the
                        -- service restart.
                        SELECT f.id
                          FROM storage.folders f
                         WHERE EXISTS (
                             SELECT 1 FROM targets t
                              WHERE f.lpath @> t.lpath
                                AND (t.drive_id IS NULL
                                     OR f.drive_id = t.drive_id)
                         )
                         ORDER BY f.id
                           FOR NO KEY UPDATE
                    ),
                    bumped AS (
                        UPDATE storage.folders f
                           SET tree_modified_at =
                                   GREATEST(NOW(), f.tree_modified_at + interval '1 second')
                          FROM victims v
                         WHERE f.id = v.id
                        RETURNING f.id
                    )
                    SELECT (SELECT COUNT(*) FROM drained)::bigint,
                           (SELECT COUNT(*) FROM bumped)::bigint
                    "#,
                )
                .bind(DRAIN_BATCH)
                .fetch_one(pool)
            })
            .await?;

            total_drained += drained;
            total_bumped += bumped;
            if drained < DRAIN_BATCH {
                break;
            }
        }

        Ok((total_drained, total_bumped))
    }
}
