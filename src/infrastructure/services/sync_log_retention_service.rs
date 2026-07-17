use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::time;
use tracing::{debug, error, info, instrument};

use crate::domain::repositories::calendar_sync_change_repository::CalendarSyncChangeRepository;
use crate::domain::repositories::contact_sync_change_repository::ContactSyncChangeRepository;
use crate::domain::repositories::folder_sync_change_repository::FolderSyncChangeRepository;
use crate::infrastructure::repositories::pg::calendar_sync_change_pg_repository::CalendarSyncChangePgRepository;
use crate::infrastructure::repositories::pg::contact_sync_change_pg_repository::ContactSyncChangePgRepository;
use crate::infrastructure::repositories::pg::folder_sync_change_pg_repository::FolderSyncChangePgRepository;

/// Periodic retention sweep for the RFC 6578 `sync-collection` change
/// logs (`storage.folder_sync_changes`, `caldav.calendar_sync_changes`,
/// `carddav.contact_sync_changes`) — structured the same way as
/// `TrashCleanupService`. Without this, the logs grow unbounded; with
/// it, rows older than `retention_days` are deleted and each domain's
/// watermark is advanced so `is_seq_expired` can still correctly answer
/// "your token predates what we kept" (RFC 6578 §3.6 → HTTP 507) after
/// the rows themselves are gone.
pub struct SyncLogRetentionService {
    folder_change_log: Arc<FolderSyncChangePgRepository>,
    calendar_change_log: Arc<CalendarSyncChangePgRepository>,
    contact_change_log: Arc<ContactSyncChangePgRepository>,
    retention_days: i64,
    sweep_interval_hours: u64,
}

impl SyncLogRetentionService {
    pub fn new(
        folder_change_log: Arc<FolderSyncChangePgRepository>,
        calendar_change_log: Arc<CalendarSyncChangePgRepository>,
        contact_change_log: Arc<ContactSyncChangePgRepository>,
        retention_days: u32,
        sweep_interval_hours: u64,
    ) -> Self {
        Self {
            folder_change_log,
            calendar_change_log,
            contact_change_log,
            retention_days: retention_days as i64,
            sweep_interval_hours: sweep_interval_hours.max(1),
        }
    }

    #[instrument(skip(self))]
    pub fn start_retention_job(&self) {
        let folder_change_log = self.folder_change_log.clone();
        let calendar_change_log = self.calendar_change_log.clone();
        let contact_change_log = self.contact_change_log.clone();
        let retention_days = self.retention_days;
        let interval_hours = self.sweep_interval_hours;

        info!(
            "Starting sync-log retention job: {} day(s) retention, {} hour interval",
            retention_days, interval_hours
        );

        tokio::spawn(async move {
            let interval_duration = Duration::from_secs(interval_hours * 60 * 60);
            let mut interval = time::interval(interval_duration);

            Self::sweep(
                folder_change_log.clone(),
                calendar_change_log.clone(),
                contact_change_log.clone(),
                retention_days,
            )
            .await;

            loop {
                interval.tick().await;
                debug!("Running scheduled sync-log retention sweep");
                Self::sweep(
                    folder_change_log.clone(),
                    calendar_change_log.clone(),
                    contact_change_log.clone(),
                    retention_days,
                )
                .await;
            }
        });
    }

    #[instrument(skip(folder_change_log, calendar_change_log, contact_change_log))]
    async fn sweep(
        folder_change_log: Arc<FolderSyncChangePgRepository>,
        calendar_change_log: Arc<CalendarSyncChangePgRepository>,
        contact_change_log: Arc<ContactSyncChangePgRepository>,
        retention_days: i64,
    ) {
        let cutoff = Utc::now() - chrono::Duration::days(retention_days);

        match folder_change_log.delete_expired_before(cutoff).await {
            Ok(0) => debug!("Folder sync-log retention: nothing to purge"),
            Ok(deleted) => info!("Folder sync-log retention: purged {deleted} expired row(s)"),
            Err(e) => error!("Folder sync-log retention sweep failed: {:?}", e),
        }

        match calendar_change_log.delete_expired_before(cutoff).await {
            Ok(0) => debug!("Calendar sync-log retention: nothing to purge"),
            Ok(deleted) => info!("Calendar sync-log retention: purged {deleted} expired row(s)"),
            Err(e) => error!("Calendar sync-log retention sweep failed: {:?}", e),
        }

        match contact_change_log.delete_expired_before(cutoff).await {
            Ok(0) => debug!("Contact sync-log retention: nothing to purge"),
            Ok(deleted) => info!("Contact sync-log retention: purged {deleted} expired row(s)"),
            Err(e) => error!("Contact sync-log retention sweep failed: {:?}", e),
        }
    }
}
