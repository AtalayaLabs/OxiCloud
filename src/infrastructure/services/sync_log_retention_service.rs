use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use tracing::{debug, error, info};

use crate::common::errors::DomainError;
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
/// see the migration header) is advanced accordingly so `is_cursor_expired`
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
/// (`SweepStats::into_outcome`), not scattered across `sweep()`.
#[derive(Default)]
struct SweepStats {
    folder_deleted: u64,
    calendar_deleted: u64,
    contact_deleted: u64,
    errors: Vec<String>,
}

impl SweepStats {
    fn total(&self) -> u64 {
        self.folder_deleted + self.calendar_deleted + self.contact_deleted
    }

    /// Any failed sweep fails the run. A log that was not trimmed is work
    /// skipped, and a job that skips work must not report success
    /// (AGENTS.md § Resilience). Rows the other sweeps purged stay purged;
    /// the counts travel in the error message so the run history still
    /// shows them.
    fn into_outcome(self) -> JobOutcome {
        let extra = serde_json::json!({
            "folder_deleted": self.folder_deleted,
            "calendar_deleted": self.calendar_deleted,
            "contact_deleted": self.contact_deleted,
            "errors": self.errors,
        });
        if self.errors.is_empty() {
            JobOutcome::ok_with(self.total(), extra)
        } else {
            JobOutcome::err(format!(
                "sync-log retention: {} sweep(s) failed: {extra}",
                self.errors.len()
            ))
        }
    }
}

/// Folds one sweep call's result into the run: the purged count on
/// success, a logged and recorded error otherwise.
fn record(result: Result<u64, DomainError>, what: &str, errors: &mut Vec<String>) -> u64 {
    result.unwrap_or_else(|e| {
        error!("Sync-log {what} sweep failed: {e:?}");
        errors.push(format!("{what}: {e}"));
        0
    })
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
        let cap = self.max_rows_per_collection;
        let mut stats = SweepStats::default();

        // Every sweep runs even if an earlier one failed: one broken log
        // must not stop the other two from being trimmed.
        stats.folder_deleted += record(
            self.folder_change_log.delete_expired_before(cutoff).await,
            "folder retention",
            &mut stats.errors,
        );
        stats.folder_deleted += record(
            self.folder_change_log.enforce_row_cap(cap).await,
            "folder row cap",
            &mut stats.errors,
        );
        stats.calendar_deleted += record(
            self.calendar_change_log.delete_expired_before(cutoff).await,
            "calendar retention",
            &mut stats.errors,
        );
        stats.calendar_deleted += record(
            self.calendar_change_log.enforce_row_cap(cap).await,
            "calendar row cap",
            &mut stats.errors,
        );
        stats.contact_deleted += record(
            self.contact_change_log.delete_expired_before(cutoff).await,
            "contact retention",
            &mut stats.errors,
        );
        stats.contact_deleted += record(
            self.contact_change_log.enforce_row_cap(cap).await,
            "contact row cap",
            &mut stats.errors,
        );

        let total = stats.total();
        if !stats.errors.is_empty() {
            info!(
                "Sync-log retention: purged {total} row(s) with {} error(s): {:?}",
                stats.errors.len(),
                stats.errors
            );
        } else if total == 0 {
            debug!("Sync-log retention: nothing to purge");
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

    /// Takes no run arguments — retention days and the row cap are fixed
    /// policy, not a per-run override, so the trigger endpoint rejects
    /// `force` and friends for this job.
    /// See `SweepStats::into_outcome` for when a run counts as failed.
    async fn run(&self, _args: &JobRunArgs) -> JobOutcome {
        self.sweep().await.into_outcome()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: the run reported `Ok` unless all six sweeps failed, so
    /// a log that never got trimmed (five failures, one success) looked
    /// healthy in the admin job history. One failure must fail the run.
    #[test]
    fn one_failed_sweep_fails_the_run() {
        let stats = SweepStats {
            folder_deleted: 12,
            errors: vec!["calendar row cap: db down".to_owned()],
            ..SweepStats::default()
        };
        let outcome = stats.into_outcome();
        assert!(!outcome.is_ok());
        let JobOutcome::Err { message } = outcome else {
            unreachable!()
        };
        assert!(message.contains("calendar row cap"), "{message}");
        assert!(message.contains("\"folder_deleted\":12"), "{message}");
    }

    #[test]
    fn a_clean_run_reports_the_total() {
        let stats = SweepStats {
            folder_deleted: 1,
            calendar_deleted: 2,
            contact_deleted: 3,
            errors: Vec::new(),
        };
        let JobOutcome::Ok { count, .. } = stats.into_outcome() else {
            panic!("clean run must be Ok")
        };
        assert_eq!(count, 6);
    }

    #[test]
    fn record_keeps_the_count_or_the_error() {
        let mut errors = Vec::new();
        assert_eq!(record(Ok(4), "x", &mut errors), 4);
        let failed = record(
            Err(DomainError::database_error("boom")),
            "folder retention",
            &mut errors,
        );
        assert_eq!(failed, 0);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("folder retention: "));
    }
}
