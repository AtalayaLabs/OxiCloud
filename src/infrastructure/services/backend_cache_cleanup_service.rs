//! `backend_cache_cleanup` — drop local cache entries whose content is gone.
//!
//! ## Why this is about privacy, not space
//!
//! The disk cache sits OUTSIDE the encryption wrapper — `di.rs` wraps the
//! backend with encryption and then wraps that with the cache — so the remote
//! holds ciphertext while the entry here is plaintext. An entry whose content has
//! been deleted is therefore readable cleartext of something the user deleted.
//!
//! Space is already handled and needs no job: the index is size-bounded by a byte
//! weigher with an eviction listener that unlinks, and `initialize` rebuilds it
//! from disk at boot, so nothing grows without limit or becomes invisible. What is
//! NOT handled is staleness — eviction is driven by size pressure and LRU order,
//! with no `time_to_live`, so on a cache that is not full a stale plaintext entry
//! can persist indefinitely.
//!
//! ## Why it repairs by default
//!
//! A deliberate exception to the house discovery-only rule, and worth arguing
//! rather than assuming. Discovery-only exists because repairing a storage
//! inconsistency can destroy the only copy of something. That cannot happen here:
//! a cache entry is BY CONSTRUCTION a copy, and the authoritative bytes live in
//! the backend. Evicting a stale entry removes plaintext that should already be
//! gone; evicting a live one costs a cache miss. There is no destructive arm to
//! gate.
//!
//! And for this particular finding the discovery-only default is actively wrong:
//! reporting "there are N plaintext copies of deleted files on disk" and then
//! waiting for someone to click *repair* is a decision to keep them.
//!
//! See `docs/plan/storage-consistency.md` §6.

use async_trait::async_trait;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::infrastructure::scheduler::{
    JobRegistry, JobRunArgs, JobStore, JobStoreProvider, Mutates, RecoverableJobHandler,
    RunOutcome, RunStatus,
};
use crate::infrastructure::services::cached_blob_backend::CachedBlobBackend;

pub const BACKEND_CACHE_CLEANUP_JOB_NAME: &str = "backend_cache_cleanup";

/// How often the sweep runs.
///
/// Cheap enough not to need tuning: a local directory walk plus one reference
/// query, with no network and no egress. Unlike `backend_consistency`, where a
/// bucket walk costs real money on S3, this can run often without anyone weighing
/// it.
pub const CACHE_CLEANUP_INTERVAL: Duration = Duration::from_secs(3600);

/// Entries younger than this are never considered stale.
///
/// An upload populates the cache BEFORE the backend put, and the PG row lands
/// later still, so a fresh entry legitimately has no reference yet. Without this
/// the job would churn against in-flight uploads — costing only a re-fetch, but
/// looking exactly like a bug.
const FRESH_GRACE: Duration = Duration::from_secs(3600);

pub struct BackendCacheCleanup {
    pool: Arc<PgPool>,
    cache: Arc<CachedBlobBackend>,
}

impl BackendCacheCleanup {
    pub fn new(pool: Arc<PgPool>, cache: Arc<CachedBlobBackend>) -> Self {
        Self { pool, cache }
    }

    pub async fn register_recoverable_job(
        self: Arc<Self>,
        registry: &JobRegistry,
        provider: &Arc<dyn JobStoreProvider>,
        interval: Option<Duration>,
    ) -> Arc<Self> {
        registry
            .register_recoverable_job(self.clone(), provider.clone(), interval)
            .await;
        self
    }
}

/// True when the entry is old enough to judge.
fn past_grace(mtime: Option<SystemTime>, now: SystemTime, grace: Duration) -> bool {
    match mtime {
        // No mtime means the file vanished or is unreadable between the walk and
        // the stat. Treat it as NOT past grace: the cost of skipping is one more
        // sweep, the cost of guessing wrong is deleting a live cache entry.
        None => false,
        Some(t) => now
            .duration_since(t)
            .map(|age| age >= grace)
            .unwrap_or(false),
    }
}

#[async_trait]
impl RecoverableJobHandler for BackendCacheCleanup {
    fn name(&self) -> &str {
        BACKEND_CACHE_CLEANUP_JOB_NAME
    }

    fn description(&self) -> &'static str {
        "Removes local disk-cache entries whose content no longer exists in the \
         registry. The cache sits outside the encryption wrapper, so its entries \
         are PLAINTEXT while the remote holds ciphertext — an entry for deleted \
         content is readable cleartext of something the user deleted, and cache \
         eviction is driven by size pressure alone, so on a cache that is not \
         full such an entry can persist indefinitely. Entries written recently \
         are left alone, because an upload populates the cache before its \
         database row exists. Capacity eviction is not touched: that is the \
         cache's own job and it does it better."
    }

    fn mutates(&self) -> Mutates {
        // Always, not OnRepairOnly. See the module docs: there is no destructive
        // arm to gate — a cache entry is a copy by construction — and leaving
        // plaintext of deleted content on disk pending a click is the wrong
        // default for this particular finding.
        Mutates::Always
    }

    fn repair_description(&self) -> Option<&'static str> {
        None
    }

    async fn count_total(&self) -> Option<u64> {
        Some(self.cache.cached_entries().await.len() as u64)
    }

    async fn run_resumable(
        &self,
        store: &dyn JobStore,
        _args: &JobRunArgs,
        _resume_cursor: Option<Vec<u8>>,
    ) -> RunOutcome {
        // No cursor, for the same reason as `backend_reclaim`: a settled entry is
        // deleted, so the set shrinks on its own and "resume" is "run again".
        let entries = self.cache.cached_entries().await;
        let now = SystemTime::now();

        let mut evicted = 0u64;
        let mut live = 0u64;
        let mut too_fresh = 0u64;
        let mut scanned = 0u64;

        for (hash, mtime) in entries {
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

            scanned += 1;

            if !past_grace(mtime, now, FRESH_GRACE) {
                too_fresh += 1;
                continue;
            }

            // The authority is the registry, exactly as it is for the deletion
            // queue. A pending deletion counts as NO reference, and that is
            // correct: the plaintext should go immediately even though the remote
            // unlink is still deferred — the local copy is the one that can be
            // read without credentials.
            let referenced: bool = match sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM storage.blobs WHERE hash = $1)
                     OR EXISTS (SELECT 1 FROM storage.chunk_manifests
                                 WHERE $1 = ANY(chunk_hashes))
                     OR EXISTS (SELECT 1 FROM storage.chunk_manifests WHERE file_hash = $1)",
            )
            .bind(&hash)
            .fetch_one(self.pool.as_ref())
            .await
            {
                Ok(v) => v,
                Err(e) => {
                    // Skip rather than guess. Deleting a live cache entry only
                    // costs a re-fetch, but doing it because a query failed would
                    // be deleting on no evidence at all.
                    tracing::warn!("backend_cache_cleanup: reference check failed for {hash}: {e}");
                    continue;
                }
            };

            if referenced {
                live += 1;
                continue;
            }

            self.cache.evict_cached(&hash).await;
            evicted += 1;
            tracing::info!(
                target: "audit",
                event = "storage.cache_plaintext_evicted",
                hash = %hash,
                "🧽 evicted cached plaintext for content no longer in the registry: {}",
                &hash[..hash.len().min(12)],
            );

            if evicted.is_multiple_of(64)
                && let Err(e) = store.checkpoint(Vec::new(), 64).await
            {
                return RunOutcome::Failed {
                    message: format!("checkpoint: {e}"),
                };
            }
        }

        if evicted > 0 {
            tracing::info!(
                evicted,
                live,
                too_fresh,
                scanned,
                "🧽 backend_cache_cleanup: evicted {evicted} stale plaintext entr(ies)"
            );
        }

        RunOutcome::completed_with(serde_json::json!({
            "evicted": evicted,
            "live": live,
            "too_fresh": too_fresh,
            "scanned": scanned,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_entries_are_never_judged_stale() {
        let now = SystemTime::now();
        let grace = Duration::from_secs(3600);

        // Just written — an upload populates the cache before its row exists.
        assert!(!past_grace(Some(now), now, grace));
        assert!(!past_grace(Some(now - Duration::from_secs(60)), now, grace));

        // Old enough to judge.
        assert!(past_grace(
            Some(now - Duration::from_secs(7200)),
            now,
            grace
        ));
    }

    #[test]
    fn an_unreadable_mtime_is_treated_as_fresh() {
        // The safe direction: skipping costs one more sweep, guessing wrong
        // deletes a live cache entry.
        assert!(!past_grace(None, SystemTime::now(), Duration::from_secs(1)));
    }

    #[test]
    fn a_clock_moving_backwards_does_not_evict() {
        // `duration_since` errors when the file is NEWER than `now` — which
        // happens with clock skew or a corrected system clock. Erring toward
        // "fresh" keeps that from becoming a mass eviction.
        let now = SystemTime::now();
        let future = now + Duration::from_secs(600);
        assert!(!past_grace(Some(future), now, Duration::from_secs(1)));
    }

    #[test]
    fn job_name_is_not_a_consistency_tenant() {
        assert_eq!(BACKEND_CACHE_CLEANUP_JOB_NAME, "backend_cache_cleanup");
        // The suffix is functional — `consistency_batch` auto-discovers children
        // by `ends_with("_consistency")` and is discovery-only; this job always
        // mutates. It also must not be `*_eviction`: in cache terminology that
        // means capacity-driven removal, which is moka's job, not this one.
        assert!(!BACKEND_CACHE_CLEANUP_JOB_NAME.ends_with("_consistency"));
        assert!(!BACKEND_CACHE_CLEANUP_JOB_NAME.ends_with("_eviction"));
        // Groups with the other backend_* jobs in the sorted admin panel.
        assert!(BACKEND_CACHE_CLEANUP_JOB_NAME.starts_with("backend_"));
    }
}
