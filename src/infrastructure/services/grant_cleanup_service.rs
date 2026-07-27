//! Service that purges expired `storage.role_grants` rows.
//!
//! The AuthZ engine already filters expired grants out of every
//! permission check at read time (`expires_at IS NULL OR
//! expires_at > NOW()` on every `check` / `list_grants_*` path in
//! `PgAclEngine`), so expired rows never leak permission. They just
//! accumulate. This service garbage-collects them, with a grace window
//! past `expires_at` that preserves the audit / support answer to
//! "what happened to my access?" for a few weeks.
//!
//! **Scheduling.** Registered with the periodic-job scheduler
//! (`docs/plan/job-registry.md` Part 1). The retired `start_cleanup_job`
//! used to spawn its own `tokio::interval` loop; the scheduler now
//! dispatches [`GrantCleanupService::purge`] on the configured cadence
//! and handles panic containment + exclusivity + admin trigger routing.
//! Admin trigger with `?force=true` still bypasses the registered
//! job and calls `purge(Some(0))` directly so the grace override reaches
//! the underlying SQL.

use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{error, info};

use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::common::errors::DomainError;
use crate::infrastructure::scheduler::{JobHandler, JobOutcome, JobRunArgs};
use crate::infrastructure::services::pg_acl_engine::PgAclEngine;
use async_trait::async_trait;

pub const GRANT_CLEANUP_JOB_NAME: &str = "grant_cleanup";

/// Service that deletes expired grants.
///
/// Owns an `Arc<PgAclEngine>` (not a `dyn AuthorizationEngine`) to avoid
/// the wrapper allocation on every SQL call — the caller set is small
/// (scheduler tick + admin trigger endpoint), both statically dispatched.
pub struct GrantCleanupService {
    authz: Arc<PgAclEngine>,
    grace_days: u32,
    interval_hours: u64,
}

impl GrantCleanupService {
    pub fn new(authz: Arc<PgAclEngine>, grace_days: u32, interval_hours: u64) -> Self {
        Self {
            authz,
            grace_days,
            // Minimum 1 hour — matches TrashCleanupService's clamp so
            // a mis-set `0` doesn't spin a hot loop.
            interval_hours: interval_hours.max(1),
        }
    }

    /// Grace period the service uses on its scheduled runs. Exposed
    /// for the admin trigger's default-response field.
    pub fn grace_days(&self) -> u32 {
        self.grace_days
    }

    /// Cadence exposed as `Duration` so DI passes a sanitised value
    /// (post-`.max(1)`) to `JobRegistry::register`.
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_hours * 3600)
    }

    /// Run one purge pass.
    ///
    /// `grace_override`:
    /// - `None` → use the configured grace (`self.grace_days`).
    /// - `Some(n)` → override with `n`. The admin `?force=true` trigger
    ///   passes `Some(0)` so Hurl regressions can hit expired grants
    ///   without waiting the configured grace out.
    ///
    /// Returns `Ok(count)` on success, `Err(_)` on DB error. Audit-log
    /// lines fire on both paths (success + failure) — bulk deletion of
    /// authorization rows is security-relevant enough to log even a
    /// zero-count run, and failures MUST reach the audit channel.
    pub async fn purge(&self, grace_override: Option<u32>) -> Result<u64, DomainError> {
        let grace = grace_override.unwrap_or(self.grace_days);
        let start = Instant::now();
        match self.authz.purge_expired_grants(grace).await {
            Ok(count) => {
                info!(
                    target: "audit",
                    event = "grant_cleanup.purged",
                    count = count,
                    grace_days = grace,
                    elapsed_ms = start.elapsed().as_millis() as u64,
                    "👮🏻‍♂️ Purged {} expired grant(s) older than {} days",
                    count,
                    grace,
                );
                Ok(count)
            }
            Err(e) => {
                error!(
                    target: "audit",
                    event = "grant_cleanup.failed",
                    grace_days = grace,
                    error = %e,
                    "Grant cleanup failed"
                );
                Err(e)
            }
        }
    }
}

#[async_trait]
impl JobHandler for GrantCleanupService {
    fn name(&self) -> &str {
        GRANT_CLEANUP_JOB_NAME
    }

    /// Runs one purge. `count` on the returned `JobOutcome::Ok` is
    /// the number of `role_grants` rows physically deleted;
    /// `extra.grace_days` records which grace was applied so admin
    /// listings can see it without a second lookup.
    ///
    /// `args.force = true` collapses the grace window to zero for
    /// this run only — matches the legacy
    /// `POST /admin/internal/trigger-grant-cleanup?force=true` shape.
    /// The configured `self.grace_days` is not mutated.
    async fn run(&self, args: &JobRunArgs) -> JobOutcome {
        let grace_override = if args.force { Some(0) } else { None };
        let effective_grace = grace_override.unwrap_or(self.grace_days);
        match self.purge(grace_override).await {
            Ok(count) => JobOutcome::ok_with(
                count,
                serde_json::json!({
                    "grace_days": effective_grace,
                    "forced": args.force,
                }),
            ),
            Err(e) => JobOutcome::Err(format!("grant cleanup failed: {e}")),
        }
    }
}
