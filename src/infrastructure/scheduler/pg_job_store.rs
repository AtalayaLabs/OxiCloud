//! PostgreSQL adapter for the recoverable-run engine
//! ([`super::recoverable`]). Concrete impls of [`JobStore`] and
//! [`JobStoreProvider`] backed by `jobs.recoverable_runs`.
//!
//! Both types are cheap to construct (just an `Arc<PgPool>` plus, for
//! `PgJobStore`, the bound run's id + started_at). One `PgJobStoreProvider`
//! lives on `AppState.core.job_store_provider`; per-run `PgJobStore`
//! instances are built by `open_or_start`.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::errors::DomainError;

use super::recoverable::{
    Finding, JobStore, JobStoreProvider, OpenedRun, ProgressKind, RunStatus, RunSummary,
    derive_progress,
};

// ─── PgJobStore — bound to one run ──────────────────────────────────────────

/// A `JobStore` bound to a specific `jobs.recoverable_runs.id`. Every
/// method issues one small UPDATE / SELECT against that row.
pub struct PgJobStore {
    pool: Arc<PgPool>,
    run_id: Uuid,
    started_at: DateTime<Utc>,
}

impl PgJobStore {
    /// Called only from [`PgJobStoreProvider::open_or_start`] and its
    /// test helpers — implementors never construct one directly.
    pub(super) fn new(pool: Arc<PgPool>, run_id: Uuid, started_at: DateTime<Utc>) -> Self {
        Self {
            pool,
            run_id,
            started_at,
        }
    }
}

fn map_sqlx_err(op: &'static str, e: sqlx::Error) -> DomainError {
    DomainError::internal_error("JobStore", format!("{op}: {e}"))
}

#[async_trait]
impl JobStore for PgJobStore {
    fn run_id(&self) -> Uuid {
        self.run_id
    }

    fn started_at(&self) -> DateTime<Utc> {
        self.started_at
    }

    async fn status(&self) -> Result<RunStatus, DomainError> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT status FROM jobs.recoverable_runs WHERE id = $1")
                .bind(self.run_id)
                .fetch_optional(self.pool.as_ref())
                .await
                .map_err(|e| map_sqlx_err("status", e))?;
        let raw = row
            .ok_or_else(|| {
                DomainError::internal_error("JobStore", format!("run vanished: {}", self.run_id))
            })?
            .0;
        RunStatus::parse(&raw).ok_or_else(|| {
            DomainError::internal_error("JobStore", format!("unknown status value: {raw}"))
        })
    }

    async fn checkpoint(&self, cursor: Vec<u8>, delta_count: u64) -> Result<(), DomainError> {
        // stats.scanned_count += delta_count. jsonb_set expects the new
        // value serialised as jsonb; the cast chain from bigint → text
        // → jsonb is the standard way to bump a numeric counter without
        // pulling the whole JSONB into Rust.
        let delta = delta_count as i64;
        sqlx::query(
            r#"
            UPDATE jobs.recoverable_runs
               SET cursor           = $2,
                   stats            = jsonb_set(
                                          stats,
                                          '{scanned_count}',
                                          ((COALESCE(stats->>'scanned_count', '0')::bigint + $3)::text)::jsonb
                                      ),
                   last_progress_at = NOW()
             WHERE id = $1
            "#,
        )
        .bind(self.run_id)
        .bind(&cursor[..])
        .bind(delta)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| map_sqlx_err("checkpoint", e))?;
        Ok(())
    }

    async fn record_finding(
        &self,
        kind: &str,
        severity: &str,
        resource_id: Option<Uuid>,
        detail: serde_json::Value,
    ) -> Result<(), DomainError> {
        // Two writes in one round-trip via CTE: INSERT the finding
        // row + UPDATE stats.finding_count on the parent run. The
        // counter stays a coarse UI hint — the source of truth is
        // the `jobs.run_findings` table itself. Even if the counter
        // drifts (crash mid-statement, hand-edited row), aggregations
        // stay accurate. Bumping in the same statement avoids two
        // round trips per finding on a hot scan; on a normal
        // consistency run the ratio of findings-to-batches is low
        // enough that a round trip either way is fine, but this shape
        // scales to a bulk-finding tenant without change.
        //
        // `resource_id` is nullable in the schema; when None here we
        // bind `Option::<Uuid>::None` and sqlx encodes it as SQL NULL.
        let bumped = sqlx::query(
            r#"
            WITH inserted AS (
                INSERT INTO jobs.run_findings
                    (run_id, kind, severity, resource_id, detail)
                VALUES ($1, $2, $3, $4, $5)
                RETURNING run_id
            )
            UPDATE jobs.recoverable_runs
               SET stats = jsonb_set(
                              stats,
                              '{finding_count}',
                              ((COALESCE(stats->>'finding_count', '0')::bigint + 1)::text)::jsonb
                           )
             WHERE id = (SELECT run_id FROM inserted)
            "#,
        )
        .bind(self.run_id)
        .bind(kind)
        .bind(severity)
        .bind(resource_id)
        .bind(&detail)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| map_sqlx_err("record_finding", e))?;

        // A rows_affected == 0 on the UPDATE would mean the parent run
        // row vanished between INSERT and UPDATE — theoretically
        // impossible under our CASCADE FK (deleting the run drops the
        // finding), so we don't error, but a debug log covers the
        // defensive path.
        if bumped.rows_affected() == 0 {
            tracing::debug!(
                target: "oxicloud::scheduler",
                event = "record_finding.counter_bump_noop",
                run_id = %self.run_id,
                "record_finding: parent run row missing during counter bump"
            );
        }
        Ok(())
    }

    async fn seed_progress_params(
        &self,
        total: u64,
        kind: ProgressKind,
    ) -> Result<(), DomainError> {
        // Stamp `params.total_rows` + `params.progress_kind` in one
        // UPDATE. Two `jsonb_set` calls compose left-to-right so both
        // keys land atomically. `bigint` cast handles the (theoretical)
        // > 2^31 subject-row case.
        let total_i64 = total as i64;
        sqlx::query(
            r#"
            UPDATE jobs.recoverable_runs
               SET params = jsonb_set(
                                jsonb_set(
                                    params,
                                    '{total_rows}',
                                    to_jsonb($2::bigint)
                                ),
                                '{progress_kind}',
                                to_jsonb($3::text)
                            )
             WHERE id = $1
            "#,
        )
        .bind(self.run_id)
        .bind(total_i64)
        .bind(kind.as_str())
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| map_sqlx_err("seed_progress_params", e))?;
        Ok(())
    }

    async fn mark_completed(&self) -> Result<(), DomainError> {
        sqlx::query(
            r#"
            UPDATE jobs.recoverable_runs
               SET status           = 'Completed',
                   completed_at     = NOW(),
                   last_progress_at = NOW()
             WHERE id = $1
            "#,
        )
        .bind(self.run_id)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| map_sqlx_err("mark_completed", e))?;
        Ok(())
    }

    async fn mark_paused(&self, cursor: Option<Vec<u8>>) -> Result<(), DomainError> {
        // Two-query variant would be simpler but this preserves the
        // final cursor value in one statement whether or not the
        // handler advanced it since the last checkpoint.
        if let Some(c) = cursor {
            sqlx::query(
                r#"
                UPDATE jobs.recoverable_runs
                   SET status           = 'Paused',
                       cursor           = $2,
                       last_progress_at = NOW()
                 WHERE id = $1
                "#,
            )
            .bind(self.run_id)
            .bind(&c[..])
            .execute(self.pool.as_ref())
            .await
            .map_err(|e| map_sqlx_err("mark_paused", e))?;
        } else {
            sqlx::query(
                r#"
                UPDATE jobs.recoverable_runs
                   SET status           = 'Paused',
                       last_progress_at = NOW()
                 WHERE id = $1
                "#,
            )
            .bind(self.run_id)
            .execute(self.pool.as_ref())
            .await
            .map_err(|e| map_sqlx_err("mark_paused", e))?;
        }
        Ok(())
    }

    async fn mark_failed(&self, message: &str) -> Result<(), DomainError> {
        sqlx::query(
            r#"
            UPDATE jobs.recoverable_runs
               SET status           = 'Failed',
                   completed_at     = NOW(),
                   last_progress_at = NOW(),
                   error_message    = $2
             WHERE id = $1
            "#,
        )
        .bind(self.run_id)
        .bind(message)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| map_sqlx_err("mark_failed", e))?;
        Ok(())
    }
}

// ─── PgJobStoreProvider — registry-level ops ────────────────────────────────

/// The `JobStoreProvider` PG-backed implementation. Constructs
/// `PgJobStore` handles via `open_or_start`, and drives the boot-time
/// crash-recovery sweep.
pub struct PgJobStoreProvider {
    pool: Arc<PgPool>,
}

impl PgJobStoreProvider {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl JobStoreProvider for PgJobStoreProvider {
    async fn open_or_start(&self, job_name: &str) -> Result<OpenedRun, DomainError> {
        // Two-shot: look up the latest non-terminal row; if found,
        // dispatch on its status; if not, INSERT a fresh Running row.
        //
        // Concurrent-insert race is caught by the partial unique index
        // `one_active_run_per_job` — the losing INSERT falls back to
        // re-querying and dispatching on whatever the winner wrote.
        // We retry once because after the first losing insert, the
        // winning row is guaranteed to exist and no third caller can
        // race in ahead of us (they'd hit the same unique index).
        for attempt in 0..2 {
            match self.try_open_or_start(job_name).await {
                Ok(opened) => return Ok(opened),
                Err(OpenErr::Retry) => {
                    tracing::debug!(
                        target: "oxicloud::scheduler",
                        event = "recoverable.open_or_start.race",
                        job = job_name,
                        attempt = attempt,
                        "open_or_start lost to a concurrent INSERT; retrying"
                    );
                    continue;
                }
                Err(OpenErr::Fatal(e)) => return Err(e),
            }
        }
        Err(DomainError::internal_error(
            "JobStore",
            format!("open_or_start({job_name}): retry budget exhausted"),
        ))
    }

    async fn boot_recovery_sweep(&self) -> Result<u64, DomainError> {
        // Every row abandoned in Running / CancelRequested by the
        // previous process flips to Paused with a synthetic
        // error_message. We DO NOT auto-resume — operators trigger
        // the resume explicitly per the trait doc.
        let result = sqlx::query(
            r#"
            UPDATE jobs.recoverable_runs
               SET status        = 'Paused',
                   error_message = COALESCE(error_message, 'server restart mid-run')
             WHERE status IN ('Running', 'CancelRequested')
            "#,
        )
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| map_sqlx_err("boot_recovery_sweep", e))?;
        Ok(result.rows_affected())
    }

    async fn list_runs(&self, job_name: &str, limit: u32) -> Result<Vec<RunSummary>, DomainError> {
        // Cap the limit at 100 defensively — the API layer should
        // also clamp, but a broken caller shouldn't tank the DB.
        let capped = limit.min(100) as i64;
        let rows: Vec<RunSummaryRow> = sqlx::query_as(RUN_SUMMARY_SELECT_LIST)
            .bind(job_name)
            .bind(capped)
            .fetch_all(self.pool.as_ref())
            .await
            .map_err(|e| map_sqlx_err("list_runs", e))?;
        rows.into_iter().map(row_to_summary).collect()
    }

    async fn get_run_by_id(&self, run_id: Uuid) -> Result<Option<RunSummary>, DomainError> {
        let row: Option<RunSummaryRow> = sqlx::query_as(RUN_SUMMARY_SELECT_BY_ID)
            .bind(run_id)
            .fetch_optional(self.pool.as_ref())
            .await
            .map_err(|e| map_sqlx_err("get_run_by_id", e))?;
        row.map(row_to_summary).transpose()
    }

    async fn list_findings(
        &self,
        run_id: Uuid,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<Finding>, DomainError> {
        let capped = limit.min(500) as i64;
        let off = offset as i64;
        let rows: Vec<(
            Uuid,
            Uuid,
            String,
            String,
            Option<Uuid>,
            serde_json::Value,
            DateTime<Utc>,
        )> = sqlx::query_as(
            r#"
            SELECT id, run_id, kind, severity, resource_id, detail, created_at
              FROM jobs.run_findings
             WHERE run_id = $1
             ORDER BY created_at, id
             LIMIT $2 OFFSET $3
            "#,
        )
        .bind(run_id)
        .bind(capped)
        .bind(off)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| map_sqlx_err("list_findings", e))?;

        Ok(rows
            .into_iter()
            .map(
                |(id, run_id, kind, severity, resource_id, detail, created_at)| Finding {
                    id,
                    run_id,
                    kind,
                    severity,
                    resource_id,
                    detail,
                    created_at,
                },
            )
            .collect())
    }

    async fn finding_severity_counts(
        &self,
        run_id: Uuid,
    ) -> Result<Vec<(String, u64)>, DomainError> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            r#"
            SELECT severity, COUNT(*)::bigint
              FROM jobs.run_findings
             WHERE run_id = $1
             GROUP BY severity
            "#,
        )
        .bind(run_id)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| map_sqlx_err("finding_severity_counts", e))?;
        Ok(rows
            .into_iter()
            .map(|(sev, count)| (sev, count.max(0) as u64))
            .collect())
    }

    async fn request_cancel(&self, job_name: &str) -> Result<Option<Uuid>, DomainError> {
        // Only Running → CancelRequested flips. `Paused` can be
        // cancelled by not resuming — no need for a state change.
        // `CancelRequested` already is what it is.
        // Multiple Running rows shouldn't exist (partial unique index),
        // but LIMIT 1 is defensive.
        let flipped: Option<(Uuid,)> = sqlx::query_as(
            r#"
            UPDATE jobs.recoverable_runs
               SET status           = 'CancelRequested',
                   last_progress_at = NOW()
             WHERE id = (
                    SELECT id FROM jobs.recoverable_runs
                     WHERE job_name = $1
                       AND status = 'Running'
                     ORDER BY started_at DESC
                     LIMIT 1
                   )
            RETURNING id
            "#,
        )
        .bind(job_name)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| map_sqlx_err("request_cancel", e))?;
        Ok(flipped.map(|(id,)| id))
    }
}

// ─── Shared row → RunSummary decoder ────────────────────────────────────────

/// Row shape returned by the run-summary SELECTs. Kept as a distinct
/// type so both `list_runs` and `get_run_by_id` share the projection
/// (SQL column list + decoder). Order matches the SELECT below.
type RunSummaryRow = (
    Uuid,                  // id
    String,                // job_name
    String,                // status
    DateTime<Utc>,         // started_at
    DateTime<Utc>,         // last_progress_at
    Option<DateTime<Utc>>, // completed_at
    Option<Vec<u8>>,       // cursor
    serde_json::Value,     // stats
    serde_json::Value,     // params
    Option<String>,        // error_message
);

const RUN_SUMMARY_COLUMNS: &str = "id, job_name, status, started_at, last_progress_at, completed_at, cursor, stats, params, error_message";

// `format!` isn't const, but `concat!` gives us a &'static str at compile
// time — worth it so the SELECT strings show up in tracing / SQL logs
// as one contiguous line instead of a runtime string build.
const RUN_SUMMARY_SELECT_LIST: &str = concat!(
    "SELECT id, job_name, status, started_at, last_progress_at, completed_at, cursor, stats, params, error_message ",
    "FROM jobs.recoverable_runs ",
    "WHERE job_name = $1 ",
    "ORDER BY started_at DESC ",
    "LIMIT $2"
);

const RUN_SUMMARY_SELECT_BY_ID: &str = concat!(
    "SELECT id, job_name, status, started_at, last_progress_at, completed_at, cursor, stats, params, error_message ",
    "FROM jobs.recoverable_runs ",
    "WHERE id = $1"
);

fn row_to_summary(row: RunSummaryRow) -> Result<RunSummary, DomainError> {
    let (
        id,
        job_name,
        status_str,
        started_at,
        last_progress_at,
        completed_at,
        cursor,
        stats,
        params,
        error_message,
    ) = row;
    let status = RunStatus::parse(&status_str).ok_or_else(|| {
        DomainError::internal_error("JobStore", format!("unknown status: {status_str}"))
    })?;

    // Derive progress from stats.scanned_count + params.total_rows +
    // params.progress_kind. All three are optional — if the tenant
    // didn't seed a total (no count_total override) the block is None
    // and the UI hides the bar. `derive_progress` also guards against
    // total = 0 (empty-subject run — bar would be meaningless).
    let scanned = stats
        .get("scanned_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let total = params.get("total_rows").and_then(|v| v.as_u64());
    let kind = params
        .get("progress_kind")
        .and_then(|v| v.as_str())
        .and_then(ProgressKind::parse);
    let progress = derive_progress(scanned, total, kind);

    Ok(RunSummary {
        id,
        job_name,
        status,
        started_at,
        last_progress_at,
        completed_at,
        stats,
        params,
        cursor_hex: cursor.map(hex::encode),
        error_message,
        progress,
    })
}

// Suppress the dead-code lint on the column list — kept as a
// human-readable constant even though the actual SELECTs currently
// inline it. Future rewrites of the SELECTs (e.g. adding stats
// projection) will use it.
#[allow(dead_code)]
const _RUN_SUMMARY_COLUMNS_UNUSED: &str = RUN_SUMMARY_COLUMNS;

/// Row shape returned by `open_or_start`'s SELECT — factored out
/// so clippy's `type_complexity` lint doesn't yell at the query.
type ExistingRun = (Uuid, String, DateTime<Utc>, Option<Vec<u8>>);

/// Internal error surface for the two-shot open_or_start retry loop.
enum OpenErr {
    /// Lost to a concurrent INSERT — caller retries.
    Retry,
    /// Any other DB error — surfaces to caller unchanged.
    Fatal(DomainError),
}

impl PgJobStoreProvider {
    /// One attempt of open_or_start. Returns `Err(Retry)` on the
    /// unique-index-conflict path so the outer loop re-queries.
    async fn try_open_or_start(&self, job_name: &str) -> Result<OpenedRun, OpenErr> {
        // Latest non-terminal row for this job_name, if any.
        let existing: Option<ExistingRun> = sqlx::query_as(
            r#"
            SELECT id, status, started_at, cursor
              FROM jobs.recoverable_runs
             WHERE job_name = $1
               AND status IN ('Running', 'Paused', 'CancelRequested')
             ORDER BY started_at DESC
             LIMIT 1
            "#,
        )
        .bind(job_name)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| OpenErr::Fatal(map_sqlx_err("open_or_start.select", e)))?;

        match existing {
            Some((id, raw_status, _started_at, _cursor)) => {
                let status = RunStatus::parse(&raw_status).ok_or_else(|| {
                    OpenErr::Fatal(DomainError::internal_error(
                        "JobStore",
                        format!("unknown status: {raw_status}"),
                    ))
                })?;
                match status {
                    RunStatus::Running | RunStatus::CancelRequested => {
                        Ok(OpenedRun::AlreadyActive { run_id: id, status })
                    }
                    RunStatus::Paused => {
                        // Flip to Running and hand back the cursor.
                        // Race note: another concurrent caller could
                        // race the same UPDATE. Both would succeed
                        // (Paused → Running is idempotent), but only
                        // one caller's dispatch would then race the
                        // partial unique index on subsequent
                        // operations. Acceptable — the loser's
                        // handler will observe `status = Running`
                        // (via `store.status()`) and can early-exit.
                        // In practice this is a rare edge case that
                        // ONLY hits if two admin triggers land in
                        // the same microsecond.
                        let row: Option<(DateTime<Utc>, Option<Vec<u8>>)> = sqlx::query_as(
                            r#"
                            UPDATE jobs.recoverable_runs
                               SET status           = 'Running',
                                   last_progress_at = NOW()
                             WHERE id = $1
                            RETURNING started_at, cursor
                            "#,
                        )
                        .bind(id)
                        .fetch_optional(self.pool.as_ref())
                        .await
                        .map_err(|e| OpenErr::Fatal(map_sqlx_err("open_or_start.resume", e)))?;
                        let (started_at, cursor_bytes) = row.ok_or_else(|| {
                            OpenErr::Fatal(DomainError::internal_error(
                                "JobStore",
                                format!("run vanished during resume: {id}"),
                            ))
                        })?;
                        let store: Arc<dyn JobStore> =
                            Arc::new(PgJobStore::new(self.pool.clone(), id, started_at));
                        Ok(OpenedRun::Resumed {
                            store,
                            cursor: cursor_bytes.unwrap_or_default(),
                        })
                    }
                    // Terminal states shouldn't appear here (WHERE
                    // clause filters them). Defensive branch.
                    _ => Err(OpenErr::Fatal(DomainError::internal_error(
                        "JobStore",
                        format!("terminal status leaked into open_or_start: {status:?}"),
                    ))),
                }
            }
            None => {
                // No non-terminal row → INSERT a fresh one. The
                // partial unique index protects against a concurrent
                // second INSERT; on conflict we retry.
                let run_id = Uuid::new_v4();
                let now = Utc::now();
                // ON CONFLICT here infers the partial unique index by
                // matching `(job_name)` + the WHERE predicate that
                // matches `one_active_run_per_job`. We cannot use
                // `ON CONFLICT ON CONSTRAINT one_active_run_per_job`
                // because `CREATE UNIQUE INDEX` produces an index, not
                // a named constraint from PG's perspective; that
                // syntax is reserved for `ALTER TABLE ... ADD CONSTRAINT
                // UNIQUE`. Inference form is equivalent and works with
                // partial indexes.
                let result = sqlx::query(
                    r#"
                    INSERT INTO jobs.recoverable_runs
                        (id, job_name, status, started_at, last_progress_at)
                    VALUES ($1, $2, 'Running', $3, $3)
                    ON CONFLICT (job_name)
                        WHERE status IN ('Running', 'Paused', 'CancelRequested')
                        DO NOTHING
                    "#,
                )
                .bind(run_id)
                .bind(job_name)
                .bind(now)
                .execute(self.pool.as_ref())
                .await
                .map_err(|e| OpenErr::Fatal(map_sqlx_err("open_or_start.insert", e)))?;

                if result.rows_affected() == 1 {
                    let store: Arc<dyn JobStore> =
                        Arc::new(PgJobStore::new(self.pool.clone(), run_id, now));
                    Ok(OpenedRun::Fresh { store })
                } else {
                    // Someone raced us. Retry to pick up their row.
                    Err(OpenErr::Retry)
                }
            }
        }
    }
}
