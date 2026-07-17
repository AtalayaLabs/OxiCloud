use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::time;
use tracing::{debug, error, info, instrument};

use crate::common::errors::Result;
use crate::domain::repositories::folder_sync_change_repository::FolderSyncChangeRepository;
use crate::infrastructure::repositories::pg::folder_sync_change_pg_repository::FolderSyncChangePgRepository;

/// Periodic retention sweep for the RFC 6578 `sync-collection` change log
/// (`storage.folder_sync_changes`) — structured the same way as
/// `TrashCleanupService`. Without this, the log grows unbounded; with it,
/// rows older than `retention_days` are deleted and
/// `storage.folder_sync_watermark` is advanced so `is_seq_expired` can
/// still correctly answer "your token predates what we kept" (RFC 6578
/// §3.6 → HTTP 507) after the rows themselves are gone.
pub struct SyncLogRetentionService {
    change_log: Arc<FolderSyncChangePgRepository>,
    retention_days: i64,
    sweep_interval_hours: u64,
}

impl SyncLogRetentionService {
    pub fn new(
        change_log: Arc<FolderSyncChangePgRepository>,
        retention_days: u32,
        sweep_interval_hours: u64,
    ) -> Self {
        Self {
            change_log,
            retention_days: retention_days as i64,
            sweep_interval_hours: sweep_interval_hours.max(1),
        }
    }

    #[instrument(skip(self))]
    pub fn start_retention_job(&self) {
        let change_log = self.change_log.clone();
        let retention_days = self.retention_days;
        let interval_hours = self.sweep_interval_hours;

        info!(
            "Starting sync-log retention job: {} day(s) retention, {} hour interval",
            retention_days, interval_hours
        );

        tokio::spawn(async move {
            let interval_duration = Duration::from_secs(interval_hours * 60 * 60);
            let mut interval = time::interval(interval_duration);

            Self::sweep(change_log.clone(), retention_days)
                .await
                .unwrap_or_else(|e| error!("Error in initial sync-log retention sweep: {:?}", e));

            loop {
                interval.tick().await;
                debug!("Running scheduled sync-log retention sweep");

                if let Err(e) = Self::sweep(change_log.clone(), retention_days).await {
                    error!("Error in scheduled sync-log retention sweep: {:?}", e);
                }
            }
        });
    }

    #[instrument(skip(change_log))]
    async fn sweep(
        change_log: Arc<FolderSyncChangePgRepository>,
        retention_days: i64,
    ) -> Result<()> {
        let cutoff = Utc::now() - chrono::Duration::days(retention_days);
        let deleted = change_log.delete_expired_before(cutoff).await?;

        if deleted == 0 {
            debug!("Sync-log retention: nothing to purge");
        } else {
            info!("Sync-log retention: purged {deleted} expired change-log row(s)");
        }

        Ok(())
    }
}
