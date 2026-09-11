//! `notifications_cleanup` scheduled job — daily retention sweep.
//!
//! Deletes rows from `notif.notifications` where `read_at IS NOT NULL`
//! and older than the retention window. Unread rows are preserved
//! unconditionally (the whole point of the durable table is that a
//! user offline for a month still sees the share-granted notice on
//! next login).
//!
//! Retention window comes from `OXICLOUD_NOTIFICATIONS_RETENTION_DAYS`
//! (default 30), applied at job dispatch — one env var maps to one
//! `retention_days` parameter so an operator can override the default
//! at trigger time without a redeploy.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use tracing::info;

use crate::application::services::notification_application_service::NotificationApplicationService;
use crate::infrastructure::scheduler::{JobHandler, JobOutcome, JobRegistry, JobRunArgs, Mutates};

/// Parameter declaration table. Kept at module scope so
/// `JobHandler::parameters` can return a `'static` slice without
/// stack-allocating each call.
static PARAMETERS: [crate::infrastructure::scheduler::JobParam; 1] =
    [crate::infrastructure::scheduler::JobParam::number(
        "retention_days",
        30,
        "Delete read notifications older than this many days.",
    )];

pub struct NotificationsCleanupService {
    service: Arc<NotificationApplicationService>,
    /// Default retention window in days when the trigger call did NOT
    /// supply an explicit `retention_days` parameter. Read from
    /// `OXICLOUD_NOTIFICATIONS_RETENTION_DAYS` at boot; the constructor
    /// clamps to a minimum of 1 day (0 would purge every read row on
    /// every tick).
    default_retention_days: i64,
}

impl NotificationsCleanupService {
    pub const JOB_NAME: &'static str = "notifications_cleanup";

    pub fn new(service: Arc<NotificationApplicationService>, default_retention_days: u32) -> Self {
        Self {
            service,
            default_retention_days: default_retention_days.max(1) as i64,
        }
    }

    /// Interval — daily. Same tier as `trash_cleanup`; retention is a
    /// "days" concept, so a finer cadence buys nothing.
    fn interval() -> Duration {
        Duration::from_secs(24 * 3600)
    }

    /// Register self with the scheduler. Chained DI helper, same shape
    /// as [`TrashCleanupService::register`].
    pub async fn register(self: Arc<Self>, registry: &JobRegistry) -> Arc<Self> {
        registry
            .register(self.clone(), Some(Self::interval()), None)
            .await;
        self
    }
}

#[async_trait]
impl JobHandler for NotificationsCleanupService {
    fn name(&self) -> &str {
        Self::JOB_NAME
    }

    fn description(&self) -> &'static str {
        "Deletes read notifications older than the retention window \
         (default 30 days, override via `retention_days` parameter or \
         OXICLOUD_NOTIFICATIONS_RETENTION_DAYS). Unread rows are \
         preserved unconditionally."
    }

    fn mutates(&self) -> Mutates {
        Mutates::Always
    }

    fn parameters(&self) -> &'static [crate::infrastructure::scheduler::JobParam] {
        // Declared default of 30 days is the SAME literal the config
        // block's env fallback uses (`OXICLOUD_NOTIFICATIONS_RETENTION_DAYS`
        // default), so an operator who never sets the env sees 30
        // everywhere. The env-derived `default_retention_days` on
        // this struct only diverges from 30 when the operator DID
        // set the env — see the guard in `run()` below.
        &PARAMETERS
    }

    async fn run(&self, args: &JobRunArgs) -> JobOutcome {
        // `get_number` returns the fallback ONLY when the arg is
        // absent — but declared defaults are seeded by the engine
        // before `run` runs (see JobRunArgs::normalized_for), so the
        // param is always present with either the caller's value or
        // the declared 30. We treat "declared default AND env
        // override differs" as "use env override" to keep the
        // OXICLOUD_NOTIFICATIONS_RETENTION_DAYS knob effective
        // without teaching the engine per-instance defaults.
        let declared_default = 30_i64;
        let raw = args.get_number("retention_days", declared_default);
        let retention_days = if raw == declared_default {
            self.default_retention_days
        } else {
            raw
        }
        .max(1);
        let cutoff = Utc::now() - chrono::Duration::days(retention_days);

        match self.service.purge_read_before_cutoff(cutoff).await {
            Ok(removed) => {
                info!(
                    target: "audit",
                    event = "notifications.retention_sweep",
                    retention_days,
                    removed,
                    "🧹 notifications retention sweep: {removed} row(s) purged (retention {retention_days} d)"
                );
                JobOutcome::ok_with(
                    removed,
                    serde_json::json!({
                        "retention_days": retention_days,
                        "removed":        removed,
                    }),
                )
            }
            Err(e) => JobOutcome::err(format!("notifications cleanup failed: {e}")),
        }
    }
}
