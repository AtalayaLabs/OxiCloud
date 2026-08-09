//! Periodic janitor that purges long-expired session rows from
//! `auth.sessions`. Naturally-expired sessions accumulate forever
//! otherwise — `SessionRepository::delete_expired_sessions()` and its
//! delayed sibling exist, but nothing was scheduling them (see
//! [[project_session_janitor_missing]] for the historical gap).
//!
//! Retention window: **3 months past `expires_at`**. Rows past the
//! natural refresh-token expiry (default 30 days) already can't
//! authenticate — expiry is checked independently at every auth path
//! (`session.is_expired()` in the refresh handler, JWT `exp` at the
//! middleware). The 3-month cushion buys ops a forensic window before
//! the row disappears entirely — a security-review after-the-fact can
//! still see "this session belonged to user X, from IP Y, minted via
//! origin Z". After that, the row is dead weight.
//!
//! Interval: 24 hours, same cadence as
//! [`super::trash_cleanup_service::TrashCleanupService`]. Session
//! cleanup is even cheaper (one SQL DELETE, no dedup GC pass), so this
//! could run more often — daily is chosen for consistency with the
//! other janitors and to keep operator noise predictable.
//!
//! Not gated behind a feature flag: expired sessions are always safe
//! to drop, and hoarding them creates a slow leak that shows up as
//! `auth.sessions` bloat months into a deployment. The one operator
//! surface is the retention window itself — hardcoded to 90 days for
//! now; if a tenant needs a different value, promote to
//! `OXICLOUD_SESSION_RETENTION_DAYS` and thread through here.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use tracing::{error, info};

use crate::domain::repositories::session_repository::SessionRepository;
use crate::infrastructure::repositories::SessionPgRepository;
use crate::infrastructure::scheduler::{JobHandler, JobOutcome, JobRegistry, JobRunArgs};

/// How long a session row survives past its `expires_at` before this
/// janitor deletes it. Enough time for a security review of a
/// suspicious session to still see the row; not so long that
/// `auth.sessions` bloats indefinitely.
const RETENTION_DAYS: i64 = 90;

/// How often the sweep runs. Hours, matches the trash-cleanup cadence.
const SWEEP_INTERVAL_HOURS: u64 = 24;

pub struct SessionCleanupService {
    session_repository: Arc<SessionPgRepository>,
}

impl SessionCleanupService {
    pub const JOB_NAME: &'static str = "session_cleanup";

    pub fn new(session_repository: Arc<SessionPgRepository>) -> Self {
        Self { session_repository }
    }

    /// Register with the scheduler and return `Arc<Self>` for the
    /// chained-constructor DI pattern (mirrors
    /// `TrashCleanupService::register`).
    pub async fn register(self: Arc<Self>, registry: &JobRegistry) -> Arc<Self> {
        let interval = Duration::from_secs(SWEEP_INTERVAL_HOURS * 3600);
        registry.register(self.clone(), Some(interval), None).await;
        self
    }

    /// One-shot execution — deletes rows where
    /// `expires_at < NOW() - RETENTION_DAYS`. Returns the row count so
    /// `JobHandler::run` can shape a `JobOutcome::Ok`.
    async fn run_once(&self) -> Result<u64, String> {
        let cutoff = Utc::now() - chrono::Duration::days(RETENTION_DAYS);
        self.session_repository
            .delete_sessions_expired_before(cutoff)
            .await
            .map_err(|e| format!("delete_sessions_expired_before: {e}"))
    }
}

#[async_trait]
impl JobHandler for SessionCleanupService {
    fn name(&self) -> &str {
        Self::JOB_NAME
    }

    /// Runs one bulk-delete of long-expired session rows. `count` on
    /// the returned `JobOutcome::Ok` is the number of rows dropped
    /// this tick; `extra` records the retention window operators can
    /// spot-check against `OXICLOUD_ACCESS_TOKEN_EXPIRY_SECS` /
    /// refresh TTL if they suspect the window is too tight.
    ///
    /// `args.force` is ignored — there's no acceleration knob (the
    /// retention window is a constant, not a runtime tunable).
    async fn run(&self, _args: &JobRunArgs) -> JobOutcome {
        match self.run_once().await {
            Ok(0) => JobOutcome::ok_with(
                0,
                serde_json::json!({
                    "retention_days": RETENTION_DAYS,
                    "note": "no rows past retention window",
                }),
            ),
            Ok(deleted) => {
                info!(
                    target: "audit",
                    event = "session_cleanup.purged",
                    rows_deleted = deleted,
                    retention_days = RETENTION_DAYS,
                    "🧹 Session janitor purged {deleted} rows past {RETENTION_DAYS}-day retention"
                );
                JobOutcome::ok_with(
                    deleted,
                    serde_json::json!({
                        "retention_days": RETENTION_DAYS,
                        "rows_deleted": deleted,
                    }),
                )
            }
            Err(e) => {
                error!("Session cleanup failed: {e}");
                JobOutcome::err(format!("session cleanup failed: {e}"))
            }
        }
    }
}
