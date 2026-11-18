//! `list_consistency` scheduled job — detector for orphans in
//! `auth.user_favorites` and `auth.user_recent_files` that the
//! `role_grants` DELETE trigger could not catch.
//!
//! The trigger from migration 20261118000000 enqueues every user
//! whose `role_grants` row goes away, and `ListCleanupService`
//! drains the queue. That pair closes the live-leak path for
//! issue #913 going forward. It does NOT cover:
//!
//! * Direct-SQL revokes that bypass the trigger (an operator
//!   mutating `storage.role_grants` with a transaction that
//!   suspends triggers, a future code path that deletes grants
//!   another way).
//! * Rows that existed BEFORE the trigger shipped — a user who
//!   favorited a shared resource and later lost access has a
//!   stale row that no event will ever re-emit.
//!
//! The detector scans the two source tables, re-checks each row
//! against the current ACL engine through the `ListCleanup`
//! port's `count_user_orphans`, and reports per-user orphan
//! counts. By default it is discovery-only — `?repair=true`
//! enqueues affected users into `auth.cleanup_queue`; the drain
//! job does the DELETE. That split matches the house rule: no
//! silent auto-repair on consistency checks, and the drain is
//! the single actor that writes.
//!
//! Non-recoverable: a one-shot scan of two bounded tables
//! (favorites + recent, both capped per user). Transient failures
//! reappear on the next scheduled tick.

use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;
use tracing::{info, warn};
use uuid::Uuid;

use crate::application::ports::list_cleanup::ListCleanup;
use crate::infrastructure::scheduler::{
    JobHandler, JobOutcome, JobParam, JobRegistry, JobRunArgs, Mutates,
};

pub struct ListConsistencyService {
    pool: Arc<PgPool>,
    lists: Vec<Arc<dyn ListCleanup>>,
}

impl ListConsistencyService {
    pub const JOB_NAME: &'static str = "list_consistency";

    pub fn new(pool: Arc<PgPool>, lists: Vec<Arc<dyn ListCleanup>>) -> Self {
        Self { pool, lists }
    }

    /// Chainable self-registration — one line in DI.
    ///
    /// Registered with `None` interval on purpose. The sibling
    /// `_consistency` detectors do the same (`drives_consistency`,
    /// `drive_policies_consistency`, …): the weekly `consistency_batch`
    /// auto-dispatches every job whose name matches `is_batch_child`
    /// (ends in `_consistency`). Giving this one its own schedule
    /// would double-run it AND drop the "via consistency_batch — every
    /// 7 d" label in the admin UI (the handler only sets
    /// `scheduled_via` when the job has no interval of its own).
    pub async fn register(self: Arc<Self>, registry: &JobRegistry) -> Arc<Self> {
        registry.register(self.clone(), None, None).await;
        self
    }

    /// Enumerate every distinct user_id that owns at least one
    /// favorite or recent row. Union is server-side so the scan
    /// cost stays in the DB. The scheduler only ever runs ONE
    /// instance of this job (exclusivity guard), so a plain
    /// SELECT is enough — no locking required.
    async fn users_with_lists(&self) -> Result<Vec<Uuid>, sqlx::Error> {
        let rows: Vec<(Uuid,)> = sqlx::query_as(
            r#"
            SELECT user_id FROM auth.user_favorites
            UNION
            SELECT user_id FROM auth.user_recent_files
            "#,
        )
        .fetch_all(&*self.pool)
        .await?;
        Ok(rows.into_iter().map(|(u,)| u).collect())
    }

    /// One detector pass. Exposed for the admin trigger + tests.
    ///
    /// `repair = true` enqueues every user with ≥1 orphan into
    /// `auth.cleanup_queue`; the drain job (`list_cleanup`) does
    /// the actual DELETE on its next tick.
    pub async fn scan(&self, repair: bool) -> Result<ScanOutcome, ScanError> {
        let users = self
            .users_with_lists()
            .await
            .map_err(ScanError::Enumerate)?;

        let mut users_scanned = 0u64;
        let mut users_with_orphans = 0u64;
        let mut total_orphans = 0u64;
        let mut per_list_orphans: Vec<(String, u64)> = self
            .lists
            .iter()
            .map(|l| (l.list_name().to_string(), 0))
            .collect();
        let mut enqueued = 0u64;

        for user_id in users {
            let mut user_total = 0u64;
            for (idx, list) in self.lists.iter().enumerate() {
                match list.count_user_orphans(user_id).await {
                    Ok(n) => {
                        user_total += n;
                        per_list_orphans[idx].1 += n;
                    }
                    Err(e) => warn!(
                        target: "audit",
                        event = "list_consistency.user_scan_failed",
                        reason = "count_error",
                        list = list.list_name(),
                        user_id = %user_id,
                        error = %e,
                        "👮🏻‍♂️ list consistency scan failed for user; row reported as 0 for this list",
                    ),
                }
            }
            users_scanned += 1;
            if user_total > 0 {
                users_with_orphans += 1;
                total_orphans += user_total;
                if repair {
                    // Enqueue → drain will DELETE on its next tick.
                    // DO UPDATE bumps `enqueued_at` so a mid-drain
                    // race (same pair the drain is already
                    // processing) survives for a second pass —
                    // matches the trigger's enqueue semantics.
                    let insert = sqlx::query(
                        r#"
                        INSERT INTO auth.cleanup_queue (user_id)
                        VALUES ($1)
                        ON CONFLICT (user_id) DO UPDATE
                            SET enqueued_at = EXCLUDED.enqueued_at
                        "#,
                    )
                    .bind(user_id)
                    .execute(&*self.pool)
                    .await;
                    match insert {
                        Ok(_) => enqueued += 1,
                        Err(e) => warn!(
                            target: "audit",
                            event = "list_consistency.enqueue_failed",
                            reason = "cleanup_queue_insert_error",
                            user_id = %user_id,
                            error = %e,
                            "👮🏻‍♂️ failed to enqueue user for repair; next scheduled scan will retry",
                        ),
                    }
                }
            }
        }

        Ok(ScanOutcome {
            users_scanned,
            users_with_orphans,
            total_orphans,
            per_list_orphans,
            repaired: enqueued,
            repair_mode: repair,
        })
    }
}

#[derive(Debug)]
pub enum ScanError {
    Enumerate(sqlx::Error),
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScanError::Enumerate(e) => write!(f, "list_consistency enumerate failed: {e}"),
        }
    }
}

impl std::error::Error for ScanError {}

#[derive(Debug, Clone)]
pub struct ScanOutcome {
    pub users_scanned: u64,
    pub users_with_orphans: u64,
    pub total_orphans: u64,
    pub per_list_orphans: Vec<(String, u64)>,
    pub repaired: u64,
    pub repair_mode: bool,
}

#[async_trait]
impl JobHandler for ListConsistencyService {
    fn name(&self) -> &str {
        Self::JOB_NAME
    }

    fn description(&self) -> &'static str {
        "Scans favorites + recent-items for rows whose target the \
         owner can no longer read. Discovery-only by default. \
         `?repair=true` enqueues affected users into `auth.cleanup_queue`; \
         the `list_cleanup` drain job does the actual DELETE. Covers \
         drift the `role_grants` DELETE trigger cannot see: direct-SQL \
         revokes, rows that outlived the pre-trigger era, bugs in \
         future code that deletes grants another way."
    }

    fn mutates(&self) -> Mutates {
        // Discovery is a read capability; `repair=true` writes to
        // `auth.cleanup_queue` (not to the lists themselves — the
        // drain does those DELETEs). `OnRepairOnly` tells the
        // scheduler + admin UI exactly that: mutating only when
        // the toggle is on. Matches the convention used by
        // `consistency_batch` itself.
        Mutates::OnRepairOnly
    }

    fn repair_description(&self) -> Option<&'static str> {
        Some(
            "Enqueues every user carrying at least one orphaned \
             favorite or recent row into `auth.cleanup_queue`. The \
             `list_cleanup` drain job then deletes the orphans on \
             its next tick; the consistency scan itself never \
             touches the list tables.",
        )
    }

    fn parameters(&self) -> &'static [JobParam] {
        const PARAMS: &[JobParam] = &[JobParam::boolean(
            "repair",
            false,
            "Enqueue every user carrying at least one orphan into \
             `auth.cleanup_queue`. The drain job (`list_cleanup`) \
             performs the DELETE on its next tick.",
        )];
        PARAMS
    }

    async fn run(&self, args: &JobRunArgs) -> JobOutcome {
        let repair = args.get_bool("repair");
        match self.scan(repair).await {
            Ok(outcome) => {
                let per_list = outcome
                    .per_list_orphans
                    .iter()
                    .map(|(n, c)| (n.clone(), serde_json::Value::from(*c)))
                    .collect::<serde_json::Map<_, _>>();
                info!(
                    target: "audit",
                    event = "list_consistency.scanned",
                    repair = outcome.repair_mode,
                    users_scanned = outcome.users_scanned,
                    users_with_orphans = outcome.users_with_orphans,
                    total_orphans = outcome.total_orphans,
                    repaired = outcome.repaired,
                    "🔍 list consistency scan complete",
                );
                JobOutcome::ok_with(
                    outcome.total_orphans,
                    serde_json::json!({
                        "repair_mode": outcome.repair_mode,
                        "users_scanned": outcome.users_scanned,
                        "users_with_orphans": outcome.users_with_orphans,
                        "repaired": outcome.repaired,
                        "per_list": per_list,
                    }),
                )
            }
            Err(e) => JobOutcome::err(e.to_string()),
        }
    }
}
