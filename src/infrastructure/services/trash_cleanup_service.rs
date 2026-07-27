use std::sync::Arc;
use std::time::Duration;
use tokio::time;
use tracing::{debug, error, info, instrument};

use crate::common::errors::Result;
use crate::domain::repositories::trash_repository::TrashRepository;
use crate::infrastructure::repositories::pg::trash_db_repository::TrashDbRepository;
use crate::infrastructure::scheduler::{JobHandler, JobOutcome, JobRunArgs};
use crate::infrastructure::services::dedup_service::DedupService;
use async_trait::async_trait;

/// Service for automatic cleanup of expired items in the trash.
///
/// Uses `TrashRepository::delete_expired_bulk` to purge all expired items
/// in **2 SQL statements inside a single transaction**, instead of the
/// previous N+1 pattern that issued 3 queries per expired item.
///
/// Each sweep ends with a dedup `garbage_collect()` pass: it reclaims the
/// blobs the expiry just dereferenced AND any other zero-reference rows —
/// notably chunks left behind by aborted streaming uploads, whose rollback
/// registers them at ref_count 0 precisely so this sweep can find them.
/// Without it, orphans would only be collected when a user happens to
/// empty their trash by hand.
pub struct TrashCleanupService {
    trash_repository: Arc<TrashDbRepository>,
    dedup_service: Arc<DedupService>,
    cleanup_interval_hours: u64,
}

impl TrashCleanupService {
    pub const JOB_NAME: &'static str = "trash_cleanup";

    pub fn new(
        trash_repository: Arc<TrashDbRepository>,
        dedup_service: Arc<DedupService>,
        cleanup_interval_hours: u64,
    ) -> Self {
        Self {
            trash_repository,
            dedup_service,
            cleanup_interval_hours: cleanup_interval_hours.max(1), // Minimum 1 hour
        }
    }

    /// Registered interval as a `Duration` — helper for DI wiring so
    /// the composition root doesn't reinvent the `hours × 3600` cast.
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.cleanup_interval_hours * 3600)
    }

    /// Starts the periodic cleanup job
    #[instrument(skip(self))]
    pub async fn start_cleanup_job(&self) {
        let trash_repository = self.trash_repository.clone();
        let dedup_service = self.dedup_service.clone();
        let interval_hours = self.cleanup_interval_hours;

        info!(
            "Starting trash cleanup job with interval of {} hours",
            interval_hours
        );

        tokio::spawn(async move {
            let interval_duration = Duration::from_secs(interval_hours * 60 * 60);
            let mut interval = time::interval(interval_duration);

            // First immediate execution
            Self::cleanup_expired_items(trash_repository.clone(), dedup_service.clone())
                .await
                .unwrap_or_else(|e| error!("Error in initial trash cleanup: {:?}", e));

            loop {
                interval.tick().await;
                debug!("Running scheduled trash cleanup task");

                if let Err(e) =
                    Self::cleanup_expired_items(trash_repository.clone(), dedup_service.clone())
                        .await
                {
                    error!("Error in scheduled trash cleanup: {:?}", e);
                }
            }
        });
    }

    /// Bulk-delete all expired trash items in a single transaction, then
    /// garbage-collect every zero-reference manifest/blob (expired content
    /// plus aborted-upload orphans).
    #[instrument(skip(trash_repository, dedup_service))]
    async fn cleanup_expired_items(
        trash_repository: Arc<TrashDbRepository>,
        dedup_service: Arc<DedupService>,
    ) -> Result<()> {
        debug!("Starting bulk cleanup of expired trash items");

        let (files, folders) = trash_repository.delete_expired_bulk().await?;

        if files == 0 && folders == 0 {
            debug!("No expired items to clean up");
        } else {
            info!(
                "Trash cleanup completed: {} files + {} folders purged",
                files, folders
            );
        }

        // Runs on the maintenance pool; batched (500 rows/iteration) with
        // yield points, so it never starves request-path queries.
        match dedup_service.garbage_collect().await {
            Ok((0, _)) => debug!("Trash cleanup GC: nothing to collect"),
            Ok((items, bytes)) => {
                info!("Trash cleanup GC: reclaimed {items} orphaned blobs ({bytes} bytes)");
            }
            Err(e) => error!("Trash cleanup GC failed: {:?}", e),
        }

        Ok(())
    }

    /// One-shot execution used by both the legacy `start_cleanup_job`
    /// timer AND the new `JobHandler::run` path. Returns the counts a
    /// caller can turn into either a log line (legacy) or a `JobOutcome`
    /// (scheduler).
    async fn run_once(&self) -> Result<TrashCleanupStats> {
        let (files, folders) = self.trash_repository.delete_expired_bulk().await?;
        // GC failure is non-fatal — the expiry itself succeeded. Report
        // reclaimed bytes when possible; log + swallow otherwise.
        let (gc_items, gc_bytes) = match self.dedup_service.garbage_collect().await {
            Ok(pair) => pair,
            Err(e) => {
                error!("Trash cleanup GC failed: {:?}", e);
                (0, 0)
            }
        };
        Ok(TrashCleanupStats {
            files_purged: files,
            folders_purged: folders,
            gc_items,
            gc_bytes,
        })
    }
}

/// Structured counters for one trash-cleanup sweep. Consumed by the
/// scheduler's `JobHandler::run` to shape `JobOutcome::Ok.extra`.
#[derive(Debug, Clone, Copy)]
struct TrashCleanupStats {
    files_purged: u64,
    folders_purged: u64,
    gc_items: u64,
    gc_bytes: u64,
}

#[async_trait]
impl JobHandler for TrashCleanupService {
    fn name(&self) -> &str {
        Self::JOB_NAME
    }

    /// Runs one bulk-delete-expired + GC sweep. `count` on the returned
    /// `JobOutcome::Ok` is the total number of rows this tick removed
    /// from the trash (files + folders); `extra` carries GC reclaim
    /// counts so operators can see "how much did this actually free."
    ///
    /// Failure of the trash sweep itself → `Err`. GC failure alone is
    /// non-fatal and stays logged only.
    ///
    /// `args.force` is ignored — trash cleanup has no acceleration
    /// concept (retention windows are per-item metadata, not a runtime
    /// knob).
    async fn run(&self, _args: &JobRunArgs) -> JobOutcome {
        match self.run_once().await {
            Ok(stats) => {
                let removed = stats.files_purged + stats.folders_purged;
                JobOutcome::ok_with(
                    removed,
                    serde_json::json!({
                        "files_purged": stats.files_purged,
                        "folders_purged": stats.folders_purged,
                        "gc_items": stats.gc_items,
                        "gc_bytes": stats.gc_bytes,
                    }),
                )
            }
            Err(e) => JobOutcome::Err(format!("trash cleanup failed: {e}")),
        }
    }
}
