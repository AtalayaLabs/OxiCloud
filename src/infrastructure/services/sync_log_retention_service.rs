use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use tracing::{debug, error, info};

use crate::domain::repositories::folder_sync_change_repository::FolderSyncChangeRepository;
use crate::domain::repositories::sync_change_log_repository::SyncChangeLogRepository;
use crate::infrastructure::repositories::pg::calendar_sync_change_pg_repository::CalendarSyncChangePgRepository;
use crate::infrastructure::repositories::pg::contact_sync_change_pg_repository::ContactSyncChangePgRepository;
use crate::infrastructure::repositories::pg::folder_sync_change_pg_repository::FolderSyncChangePgRepository;
use crate::infrastructure::scheduler::{JobHandler, JobOutcome, JobRegistry, JobRunArgs};

pub const SYNC_LOG_RETENTION_JOB_NAME: &str = "sync_log_retention";

/// Periodic retention sweep for the RFC 6578 `sync-collection` change
/// logs (`storage.folder_sync_changes`, `caldav.calendar_sync_changes`,
/// `carddav.contact_sync_changes`). Without it, the logs grow unbounded;
/// with it, two independent caps keep them bounded:
/// - `retention_days`: rows older than this are deleted (time-based).
/// - `max_rows_per_collection`: for any single collection with more rows
///   than this, the oldest excess rows are deleted regardless of age
///   (guards against one pathologically churny collection ballooning the
///   table within the retention window).
///
/// Each domain's watermark (`*_sync_watermark`, per-collection —
/// see the migration header) is advanced accordingly so `is_seq_expired`
/// can still correctly answer "your token predates what we kept"
/// (RFC 6578 §3.6 → HTTP 507) after the rows themselves are gone.
///
/// Registered with the periodic-job scheduler
/// (`docs/plan/job-registry.md` Part 1) — mirrors `GrantCleanupService`.
pub struct SyncLogRetentionService {
    folder_change_log: Arc<FolderSyncChangePgRepository>,
    calendar_change_log: Arc<CalendarSyncChangePgRepository>,
    contact_change_log: Arc<ContactSyncChangePgRepository>,
    retention_days: i64,
    sweep_interval_hours: u64,
    max_rows_per_collection: u32,
}

/// Per-domain outcome of one sweep. Kept separate from `JobOutcome` so the
/// "does the whole run count as failed" decision lives in one place
/// (`run()`), not scattered across `sweep()`.
struct SweepStats {
    folder_deleted: u64,
    calendar_deleted: u64,
    contact_deleted: u64,
    errors: Vec<String>,
}

impl SyncLogRetentionService {
    pub fn new(
        folder_change_log: Arc<FolderSyncChangePgRepository>,
        calendar_change_log: Arc<CalendarSyncChangePgRepository>,
        contact_change_log: Arc<ContactSyncChangePgRepository>,
        retention_days: u32,
        sweep_interval_hours: u64,
        max_rows_per_collection: u32,
    ) -> Self {
        Self {
            folder_change_log,
            calendar_change_log,
            contact_change_log,
            retention_days: retention_days as i64,
            sweep_interval_hours: sweep_interval_hours.max(1),
            max_rows_per_collection,
        }
    }

    /// Cadence exposed as `Duration`, for [`Self::register`].
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.sweep_interval_hours * 3600)
    }

    /// Register self with the periodic-job scheduler and return the same
    /// `Arc<Self>` for DI-style chaining. See
    /// `docs/plan/job-registry.md` Part 1.
    ///
    /// Note: registration does not fire an immediate sweep — the first
    /// run happens one `sweep_interval_hours` after boot. Retention is
    /// long-tail housekeeping, not urgent, so this is an accepted
    /// trade-off rather than special-cased with an extra boot-time call.
    pub async fn register(self: Arc<Self>, registry: &JobRegistry) -> Arc<Self> {
        let interval = self.interval();
        registry.register(self.clone(), Some(interval), None).await;
        self
    }

    /// One sweep across all three domains. Each domain's failure is
    /// logged and does not stop the others — same "a secondary failure
    /// doesn't block the rest of the run" shape as
    /// `TrashCleanupService::run_once`'s GC step.
    async fn sweep(&self) -> SweepStats {
        let cutoff = Utc::now() - chrono::Duration::days(self.retention_days);
        let mut stats = SweepStats {
            folder_deleted: 0,
            calendar_deleted: 0,
            contact_deleted: 0,
            errors: Vec::new(),
        };

        match self.folder_change_log.delete_expired_before(cutoff).await {
            Ok(n) => stats.folder_deleted += n,
            Err(e) => {
                error!("Folder sync-log retention sweep failed: {:?}", e);
                stats.errors.push(format!("folder retention: {e}"));
            }
        }
        match self
            .folder_change_log
            .enforce_row_cap(self.max_rows_per_collection)
            .await
        {
            Ok(n) => stats.folder_deleted += n,
            Err(e) => {
                error!("Folder sync-log row-cap sweep failed: {:?}", e);
                stats.errors.push(format!("folder row cap: {e}"));
            }
        }

        match self.calendar_change_log.delete_expired_before(cutoff).await {
            Ok(n) => stats.calendar_deleted += n,
            Err(e) => {
                error!("Calendar sync-log retention sweep failed: {:?}", e);
                stats.errors.push(format!("calendar retention: {e}"));
            }
        }
        match self
            .calendar_change_log
            .enforce_row_cap(self.max_rows_per_collection)
            .await
        {
            Ok(n) => stats.calendar_deleted += n,
            Err(e) => {
                error!("Calendar sync-log row-cap sweep failed: {:?}", e);
                stats.errors.push(format!("calendar row cap: {e}"));
            }
        }

        match self.contact_change_log.delete_expired_before(cutoff).await {
            Ok(n) => stats.contact_deleted += n,
            Err(e) => {
                error!("Contact sync-log retention sweep failed: {:?}", e);
                stats.errors.push(format!("contact retention: {e}"));
            }
        }
        match self
            .contact_change_log
            .enforce_row_cap(self.max_rows_per_collection)
            .await
        {
            Ok(n) => stats.contact_deleted += n,
            Err(e) => {
                error!("Contact sync-log row-cap sweep failed: {:?}", e);
                stats.errors.push(format!("contact row cap: {e}"));
            }
        }

        let total = stats.folder_deleted + stats.calendar_deleted + stats.contact_deleted;
        if total == 0 && stats.errors.is_empty() {
            debug!("Sync-log retention: nothing to purge");
        } else if !stats.errors.is_empty() {
            info!(
                "Sync-log retention: purged {total} row(s) with {} error(s): {:?}",
                stats.errors.len(),
                stats.errors
            );
        } else {
            info!("Sync-log retention: purged {total} row(s)");
        }

        stats
    }
}

#[async_trait]
impl JobHandler for SyncLogRetentionService {
    fn name(&self) -> &str {
        SYNC_LOG_RETENTION_JOB_NAME
    }

    /// `args.force` is ignored — retention days and the row cap are fixed
    /// policy, not a per-run override (same as `TrashCleanupService::run`).
    ///
    /// The whole run is `JobOutcome::Err` only if every one of the six
    /// underlying sweep calls (2 per domain) failed — i.e. nothing at all
    /// happened this sweep. Any partial success still reports `Ok` with
    /// the summed count and the per-domain breakdown (including any
    /// errors) in `extra`, so admins keep full visibility through
    /// `GET /api/admin/jobs/sync_log_retention/runs` instead of logs-only.
    async fn run(&self, _args: &JobRunArgs) -> JobOutcome {
        let stats = self.sweep().await;
        let total = stats.folder_deleted + stats.calendar_deleted + stats.contact_deleted;

        if total == 0 && stats.errors.len() >= 6 {
            return JobOutcome::err(format!(
                "sync-log retention: all sweeps failed: {:?}",
                stats.errors
            ));
        }

        JobOutcome::ok_with(
            total,
            serde_json::json!({
                "folder_deleted": stats.folder_deleted,
                "calendar_deleted": stats.calendar_deleted,
                "contact_deleted": stats.contact_deleted,
                "errors": stats.errors,
            }),
        )
    }
}
