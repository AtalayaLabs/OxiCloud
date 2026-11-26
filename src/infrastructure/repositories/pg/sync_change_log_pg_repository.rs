//! Generic PostgreSQL-backed change-log repository for RFC 6578
//! `sync-collection` — parameterized over table/column names via
//! `SyncChangeLogSchema` so the folder, calendar and contact logs (see the
//! three schema files next to this one) are each a schema struct + type
//! alias instead of a hand-copied impl.
//!
//! SQL safety note: every `{table}`/`{column}` substitution below comes
//! from a `SyncChangeLogSchema::CONST` — a compiler-controlled
//! `&'static str` fixed at the `impl` sites in this crate, never from
//! request/user input — so the runtime `format!()`-templated SQL carries
//! no injection risk despite not going through the compile-time-checked
//! `sqlx::query!` macro (this crate uses runtime query strings throughout).

use std::marker::PhantomData;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::errors::DomainError;
use crate::domain::entities::sync_token::SyncCursor;
use crate::domain::repositories::sync_change_log_repository::{
    SyncChangeKind, SyncChangeLogRepository, SyncChangeRow,
};

/// Compile-time-fixed table/column names for one sync-collection change
/// log. Implementors are zero-sized marker structs (never instantiated);
/// only their associated consts are read, at query-build time.
pub trait SyncChangeLogSchema: Send + Sync + 'static {
    /// Fully-qualified change-log table, e.g. `"caldav.calendar_sync_changes"`.
    const TABLE: &'static str;
    /// Fully-qualified per-collection watermark table, e.g.
    /// `"caldav.calendar_sync_watermark"`. Keyed by the same column name as
    /// `COLLECTION_ID_COLUMN` below (one watermark row per collection, no
    /// FK — see the migration header for why).
    const WATERMARK_TABLE: &'static str;
    /// The column scoping rows to one collection (`"collection_calendar_id"`
    /// / `"collection_address_book_id"`) — also the watermark table's
    /// primary key column.
    const COLLECTION_ID_COLUMN: &'static str;
    /// The row's identifying-label column (`"member_href_name"` /
    /// `"member_ical_uid"` / `"member_uid"`) — becomes `SyncChangeRow::label`.
    const LABEL_COLUMN: &'static str;
    /// Boolean SQL expression over the log's columns that becomes
    /// `SyncChangeRow::is_collection` — `"member_type = 'folder'"` for the
    /// WebDAV log, `"false"` for logs whose members are never containers.
    const IS_COLLECTION_EXPR: &'static str;
    /// Short human tag for error messages (`"calendar_sync_changes"` /
    /// `"contact_sync_changes"`) — cosmetic only, never interpolated into SQL.
    const LOG_NAME: &'static str;
}

pub struct SyncChangeLogPgRepository<S: SyncChangeLogSchema> {
    pool: Arc<PgPool>,
    _schema: PhantomData<S>,
}

impl<S: SyncChangeLogSchema> SyncChangeLogPgRepository<S> {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self {
            pool,
            _schema: PhantomData,
        }
    }
}

impl<S: SyncChangeLogSchema> SyncChangeLogRepository for SyncChangeLogPgRepository<S> {
    async fn changes_since(
        &self,
        collection_id: Uuid,
        since: Option<&SyncCursor>,
    ) -> Result<(Vec<SyncChangeRow>, SyncCursor), DomainError> {
        // Take the new cursor FIRST and bound the delta to what its snapshot
        // saw (`xid < $4 AND xid <> ALL($5)`). A row committed after this
        // point is invisible to that snapshot, so the next poll — and only
        // the next poll — picks it up. Reading the cursor after the delta
        // would let a concurrent commit land in the token but not in the
        // delta.
        let current = self.current_cursor(collection_id).await?;
        let new_cursor = SyncCursor {
            seq: since.map_or(current.seq, |s| current.seq.max(s.seq)),
            ..current
        };
        // Rows `since`'s snapshot did not see: written by a transaction at
        // or above its xmax, or in flight then. `None` = since the
        // beginning: every row qualifies (xmax 0).
        let (since_xmax, since_xip) = since.map_or((0, Vec::new()), |s| {
            (s.xmax as i64, s.xip.iter().map(|x| *x as i64).collect())
        });
        let cur_xip: Vec<i64> = new_cursor.xip.iter().map(|x| *x as i64).collect();

        // DISTINCT ON collapses churn to the latest row per (member,
        // label) — per label, not per member, because a rename logs a
        // tombstone under the old label and a creation under the new one,
        // and the client needs both (collapsing per member kept only the
        // creation, and the old href lived on as a ghost). Row order
        // doesn't matter to the client: the response buckets upserts and
        // deletions separately, so the one ordering hazard (a stale
        // tombstone outliving a same-href recreation in one poll window)
        // is handled by dropping such tombstones in
        // `sync_collection_engine.rs`, not by row order here.
        let sql = format!(
            r#"
            SELECT DISTINCT ON (member_id, {label})
                   member_id, {label} AS label, change_kind,
                   ({is_collection}) AS is_collection
              FROM {table}
             WHERE {collection_col} = $1
               AND (xid >= $2 OR xid = ANY($3))
               AND xid < $4 AND xid <> ALL($5)
             ORDER BY member_id, {label}, seq DESC
            "#,
            label = S::LABEL_COLUMN,
            is_collection = S::IS_COLLECTION_EXPR,
            table = S::TABLE,
            collection_col = S::COLLECTION_ID_COLUMN,
        );

        let rows = sqlx::query_as::<_, (Uuid, String, String, bool)>(&sql)
            .bind(collection_id)
            .bind(since_xmax)
            .bind(&since_xip)
            .bind(new_cursor.xmax as i64)
            .bind(&cur_xip)
            .fetch_all(&*self.pool)
            .await
            .map_err(|e| {
                DomainError::database_error(format!("{}: changes_since: {e}", S::LOG_NAME))
            })?;

        let changes = rows
            .into_iter()
            .map(
                |(member_id, label, change_kind, is_collection)| SyncChangeRow {
                    member_id,
                    label,
                    kind: match change_kind.as_str() {
                        "created" => SyncChangeKind::Created,
                        "deleted" => SyncChangeKind::Deleted,
                        _ => SyncChangeKind::Updated,
                    },
                    is_collection,
                },
            )
            .collect();

        Ok((changes, new_cursor))
    }

    async fn current_cursor(&self, collection_id: Uuid) -> Result<SyncCursor, DomainError> {
        // One statement, so `seq` and the snapshot agree: every row this
        // snapshot can't see is one the cursor does not cover.
        //
        // The watermark is part of "current": once retention has purged
        // every row of an idle collection, MAX(seq) over the rows is NULL,
        // and a token minted at 0 would sit below `low_water_seq` — expired
        // on arrival, so every resync minted another expired token and the
        // client looped forever. GREATEST ignores NULLs.
        let sql = format!(
            "SELECT COALESCE(GREATEST(\
                (SELECT MAX(seq) FROM {table} WHERE {col} = $1), \
                (SELECT low_water_seq FROM {watermark} WHERE {col} = $1)), 0), \
                pg_snapshot_xmin(snap)::text::bigint, \
                pg_snapshot_xmax(snap)::text::bigint, \
                ARRAY(SELECT x::text::bigint FROM pg_snapshot_xip(snap) x ORDER BY 1) \
             FROM (SELECT pg_current_snapshot() AS snap) s",
            table = S::TABLE,
            watermark = S::WATERMARK_TABLE,
            col = S::COLLECTION_ID_COLUMN,
        );
        let (seq, xmin, xmax, xip): (i64, i64, i64, Vec<i64>) = sqlx::query_as(&sql)
            .bind(collection_id)
            .fetch_one(&*self.pool)
            .await
            .map_err(|e| {
                DomainError::database_error(format!("{}: current_cursor: {e}", S::LOG_NAME))
            })?;
        Ok(SyncCursor {
            seq: seq as u64,
            xmin: xmin as u64,
            xmax: xmax as u64,
            xip: xip.into_iter().map(|x| x as u64).collect(),
        })
    }

    async fn is_cursor_expired(
        &self,
        collection_id: Uuid,
        cursor: &SyncCursor,
    ) -> Result<bool, DomainError> {
        let sql = format!(
            "SELECT low_water_seq, low_water_xid, purged_by_xid FROM {} WHERE {} = $1",
            S::WATERMARK_TABLE,
            S::COLLECTION_ID_COLUMN
        );
        let watermark: Option<(i64, Option<i64>, Option<i64>)> = sqlx::query_as(&sql)
            .bind(collection_id)
            .fetch_optional(&*self.pool)
            .await
            .map_err(|e| {
                DomainError::database_error(format!("{}: watermark read: {e}", S::LOG_NAME))
            })?;
        // No row means this collection has never had rows purged by
        // retention — never expired, regardless of the cursor. Otherwise a
        // purge may have reached rows the cursor doesn't cover: above its
        // `seq`, or written by a transaction its snapshot did not see.
        //
        // The second can only happen to a token minted BEFORE the purge: a
        // purged row was committed before the purge, so a snapshot that saw
        // the purging transaction saw every row it removed. For an older
        // token, only the highest purged xid is kept, so the check is
        // conservative: any purged xid at or above the snapshot's xmin
        // expires it.
        Ok(watermark.is_some_and(|(low_seq, low_xid, purged_by)| {
            let saw_purge = purged_by.is_some_and(|p| cursor.saw(p as u64));
            (cursor.seq as i64) < low_seq
                || (!saw_purge && low_xid.is_some_and(|x| x >= cursor.xmin as i64))
        }))
    }

    async fn delete_expired_before(&self, cutoff: DateTime<Utc>) -> Result<u64, DomainError> {
        let sql = format!(
            r#"
            WITH deleted AS (
                DELETE FROM {table} WHERE changed_at < $1
             RETURNING seq, xid, {collection_col}
            ), per_collection AS (
                SELECT {collection_col}, MAX(seq) AS max_seq, MAX(xid) AS max_xid,
                       COUNT(*) AS n
                  FROM deleted
                 GROUP BY {collection_col}
            ), upserted AS (
                INSERT INTO {watermark}
                    ({collection_col}, low_water_seq, low_water_xid, purged_by_xid)
                SELECT {collection_col}, max_seq, max_xid,
                       pg_current_xact_id()::text::bigint
                  FROM per_collection
                ON CONFLICT ({collection_col}) DO UPDATE
                    SET low_water_seq = GREATEST({watermark}.low_water_seq, EXCLUDED.low_water_seq),
                        low_water_xid = GREATEST({watermark}.low_water_xid, EXCLUDED.low_water_xid),
                        purged_by_xid = GREATEST({watermark}.purged_by_xid, EXCLUDED.purged_by_xid)
            )
            SELECT COALESCE(SUM(n), 0)::bigint FROM per_collection
            "#,
            table = S::TABLE,
            collection_col = S::COLLECTION_ID_COLUMN,
            watermark = S::WATERMARK_TABLE,
        );

        sqlx::query_scalar::<_, i64>(&sql)
            .bind(cutoff)
            .fetch_one(&*self.pool)
            .await
            .map(|n| n as u64)
            .map_err(|e| DomainError::database_error(format!("{}: retention: {e}", S::LOG_NAME)))
    }

    async fn enforce_row_cap(&self, max_rows: u32) -> Result<u64, DomainError> {
        let sql = format!(
            r#"
            WITH ranked AS (
                SELECT seq, {collection_col},
                       row_number() OVER (
                           PARTITION BY {collection_col} ORDER BY seq DESC
                       ) AS rn
                  FROM {table}
            ), to_delete AS (
                DELETE FROM {table} f
                 USING ranked r
                 WHERE f.seq = r.seq AND r.rn > $1
             RETURNING f.seq, f.xid, f.{collection_col}
            ), per_collection AS (
                SELECT {collection_col}, MAX(seq) AS max_seq, MAX(xid) AS max_xid,
                       COUNT(*) AS n
                  FROM to_delete
                 GROUP BY {collection_col}
            ), upserted AS (
                INSERT INTO {watermark}
                    ({collection_col}, low_water_seq, low_water_xid, purged_by_xid)
                SELECT {collection_col}, max_seq, max_xid,
                       pg_current_xact_id()::text::bigint
                  FROM per_collection
                ON CONFLICT ({collection_col}) DO UPDATE
                    SET low_water_seq = GREATEST({watermark}.low_water_seq, EXCLUDED.low_water_seq),
                        low_water_xid = GREATEST({watermark}.low_water_xid, EXCLUDED.low_water_xid),
                        purged_by_xid = GREATEST({watermark}.purged_by_xid, EXCLUDED.purged_by_xid)
            )
            SELECT COALESCE(SUM(n), 0)::bigint FROM per_collection
            "#,
            table = S::TABLE,
            collection_col = S::COLLECTION_ID_COLUMN,
            watermark = S::WATERMARK_TABLE,
        );

        sqlx::query_scalar::<_, i64>(&sql)
            .bind(max_rows as i64)
            .fetch_one(&*self.pool)
            .await
            .map(|n| n as u64)
            .map_err(|e| DomainError::database_error(format!("{}: row cap: {e}", S::LOG_NAME)))
    }
}

#[cfg(feature = "integration_tests")]
// See `session_pg_repository.rs`: CI builds the lib as a plain dependency
// of the `tests/` binaries, where these helpers look unused.
#[allow(dead_code)]
mod integration_tests {
    //! Fixtures are scoped per test (own collection id, rows dated in the
    //! year 2000 so the retention sweep below only reaches them) because
    //! all integration modules share one Postgres (AGENTS.md § DB
    //! integration tests share one database).
    use super::*;
    use crate::integration_test_support::test_db_url;
    use sqlx::postgres::PgPoolOptions;

    async fn pool() -> Arc<PgPool> {
        Arc::new(
            PgPoolOptions::new()
                .max_connections(2)
                .connect(&test_db_url())
                .await
                .expect("connect to integration-test PostgreSQL"),
        )
    }

    async fn cleanup(pool: &PgPool, collection: Uuid) {
        for table in [
            "storage.folder_sync_changes",
            "storage.folder_sync_watermark",
        ] {
            sqlx::query(&format!(
                "DELETE FROM {table} WHERE collection_folder_id = $1"
            ))
            .bind(collection)
            .execute(pool)
            .await
            .expect("cleanup");
        }
    }

    /// Regression: retention purging EVERY row of an idle collection left
    /// the current seq at 0 while the watermark sat at the last purged seq.
    /// The initial-sync token (seq 0) was then expired on its very next
    /// use, the client resynced, got seq 0 again, and looped forever.
    #[tokio::test]
    async fn a_token_minted_after_a_full_purge_is_not_expired() {
        let pool = pool().await;
        let repo =
            crate::infrastructure::repositories::pg::folder_sync_change_pg_repository::FolderSyncChangePgRepository::new(pool.clone());
        let collection = Uuid::new_v4();
        cleanup(&pool, collection).await;

        // An unrelated transaction older than every purged row, still open
        // when the token is minted, holds the snapshot's xmin below them.
        // That must not expire a token minted after the purge: it has seen
        // every purged row. (It did: the client resynced in a loop for as
        // long as any long-running transaction was open.)
        let mut bystander = pool.begin().await.expect("begin bystander");
        sqlx::query("SELECT pg_current_xact_id()")
            .execute(&mut *bystander)
            .await
            .expect("bystander takes an xid");

        for i in 0..3 {
            sqlx::query(
                "INSERT INTO storage.folder_sync_changes \
                   (collection_folder_id, member_type, member_id, member_href_name, \
                    change_kind, changed_at) \
                 VALUES ($1, 'file', $2, $3, 'created', '2000-01-01T00:00:00Z')",
            )
            .bind(collection)
            .bind(Uuid::new_v4())
            .bind(format!("f{i}.txt"))
            .execute(&*pool)
            .await
            .expect("seed change row");
        }
        let last_seq = repo
            .current_cursor(collection)
            .await
            .expect("current_cursor")
            .seq;
        assert!(last_seq > 0);

        let cutoff = "2000-01-02T00:00:00Z".parse().expect("cutoff");
        repo.delete_expired_before(cutoff).await.expect("retention");

        let minted = repo
            .current_cursor(collection)
            .await
            .expect("current_cursor");
        assert_eq!(minted.seq, last_seq, "the watermark must count as current");
        assert!(
            !repo
                .is_cursor_expired(collection, &minted)
                .await
                .expect("is_cursor_expired"),
            "a freshly minted token must not be expired"
        );

        bystander.rollback().await.expect("rollback bystander");
        cleanup(&pool, collection).await;
    }

    async fn insert_change(executor: impl sqlx::PgExecutor<'_>, collection: Uuid, member: Uuid) {
        insert_change_at(executor, collection, member, Utc::now()).await;
    }

    async fn insert_change_at(
        executor: impl sqlx::PgExecutor<'_>,
        collection: Uuid,
        member: Uuid,
        changed_at: DateTime<Utc>,
    ) {
        sqlx::query(
            "INSERT INTO storage.folder_sync_changes \
               (collection_folder_id, member_type, member_id, member_href_name, \
                change_kind, changed_at) \
             VALUES ($1, 'file', $2, 'x.txt', 'created', $3)",
        )
        .bind(collection)
        .bind(member)
        .bind(changed_at)
        .execute(executor)
        .await
        .expect("insert change row");
    }

    /// Regression: `seq` is assigned at INSERT but becomes visible at
    /// COMMIT, so a slow writer's row can commit BELOW a token already
    /// handed out. The token was the max committed `seq`; the next poll
    /// asked for `seq > token` and the slow writer's change was never
    /// delivered — the client silently diverged from the server.
    #[tokio::test]
    async fn a_change_committed_below_a_handed_out_token_is_still_delivered() {
        let pool = pool().await;
        let repo =
            crate::infrastructure::repositories::pg::folder_sync_change_pg_repository::FolderSyncChangePgRepository::new(pool.clone());
        let collection = Uuid::new_v4();
        cleanup(&pool, collection).await;

        let (_, token) = repo.changes_since(collection, None).await.expect("initial");

        // Slow writer: takes its seq first, commits last.
        let slow_member = Uuid::new_v4();
        let mut slow = pool.begin().await.expect("begin slow writer");
        insert_change(&mut *slow, collection, slow_member).await;

        // Fast writer commits a higher seq; the client polls past it.
        let fast_member = Uuid::new_v4();
        insert_change(&*pool, collection, fast_member).await;
        let (rows, token) = repo
            .changes_since(collection, Some(&token))
            .await
            .expect("poll 1");
        assert!(rows.iter().any(|r| r.member_id == fast_member));

        slow.commit().await.expect("commit slow writer");

        let (rows, _) = repo
            .changes_since(collection, Some(&token))
            .await
            .expect("poll 2");
        assert!(
            rows.iter().any(|r| r.member_id == slow_member),
            "a change committed after the poll that skipped its seq must reach the next poll"
        );

        cleanup(&pool, collection).await;
    }

    /// Same hole on the initial sync: the token an initial listing hands
    /// back must not cover a change that was still in flight when it was
    /// minted (the listing could not see it either).
    #[tokio::test]
    async fn a_change_in_flight_at_initial_sync_is_delivered_by_the_first_poll() {
        let pool = pool().await;
        let repo =
            crate::infrastructure::repositories::pg::folder_sync_change_pg_repository::FolderSyncChangePgRepository::new(pool.clone());
        let collection = Uuid::new_v4();
        cleanup(&pool, collection).await;

        let slow_member = Uuid::new_v4();
        let mut slow = pool.begin().await.expect("begin slow writer");
        insert_change(&mut *slow, collection, slow_member).await;
        insert_change(&*pool, collection, Uuid::new_v4()).await;

        let token = repo
            .current_cursor(collection)
            .await
            .expect("initial token");
        slow.commit().await.expect("commit slow writer");

        let (rows, _) = repo
            .changes_since(collection, Some(&token))
            .await
            .expect("first poll");
        assert!(
            rows.iter().any(|r| r.member_id == slow_member),
            "a change in flight at initial sync must reach the first poll"
        );

        cleanup(&pool, collection).await;
    }

    /// The retention side of the commit-order gap: a change in flight when
    /// a token was minted is covered only by the token's `xmin`. If
    /// retention purges that row before the client polls, the purge sits
    /// BELOW the token's `seq`, so a seq-only watermark called the token
    /// valid and the change was silently lost. The token must be expired
    /// instead, so the client resyncs.
    #[tokio::test]
    async fn purging_a_change_a_token_saw_in_flight_expires_the_token() {
        let pool = pool().await;
        let repo =
            crate::infrastructure::repositories::pg::folder_sync_change_pg_repository::FolderSyncChangePgRepository::new(pool.clone());
        let collection = Uuid::new_v4();
        cleanup(&pool, collection).await;

        let old: DateTime<Utc> = "2000-01-01T00:00:00Z".parse().expect("old");
        let mut slow = pool.begin().await.expect("begin slow writer");
        insert_change_at(&mut *slow, collection, Uuid::new_v4(), old).await;
        insert_change(&*pool, collection, Uuid::new_v4()).await;

        let token = repo
            .current_cursor(collection)
            .await
            .expect("token minted while the slow write is in flight");
        slow.commit().await.expect("commit slow writer");

        let cutoff = "2000-01-02T00:00:00Z".parse().expect("cutoff");
        repo.delete_expired_before(cutoff).await.expect("retention");

        assert!(
            repo.is_cursor_expired(collection, &token)
                .await
                .expect("is_cursor_expired"),
            "a token must expire once retention purged a change it never delivered"
        );

        cleanup(&pool, collection).await;
    }

    /// The other half of exactness: a cursor must not re-report a change
    /// it already delivered just because some unrelated transaction was
    /// open when it was taken. An xmin-only cursor did exactly that — any
    /// long-running transaction anywhere (a maintenance job) made every
    /// poll re-send everything since it started, and a no-op poll came
    /// back non-empty.
    #[tokio::test]
    async fn an_unrelated_open_transaction_does_not_cause_re_reports() {
        let pool = pool().await;
        let repo =
            crate::infrastructure::repositories::pg::folder_sync_change_pg_repository::FolderSyncChangePgRepository::new(pool.clone());
        let (collection, elsewhere) = (Uuid::new_v4(), Uuid::new_v4());
        cleanup(&pool, collection).await;

        // Holds an xid, below everything written after it, until the end.
        let mut bystander = pool.begin().await.expect("begin bystander");
        insert_change(&mut *bystander, elsewhere, Uuid::new_v4()).await;

        let (_, token) = repo.changes_since(collection, None).await.expect("initial");
        insert_change(&*pool, collection, Uuid::new_v4()).await;
        let (rows, token) = repo
            .changes_since(collection, Some(&token))
            .await
            .expect("poll 1");
        assert_eq!(rows.len(), 1, "the new change is delivered once");

        let (rows, _) = repo
            .changes_since(collection, Some(&token))
            .await
            .expect("poll 2");
        assert!(rows.is_empty(), "a no-op poll is empty: {rows:?}");

        bystander.rollback().await.expect("rollback bystander");
        cleanup(&pool, collection).await;
    }
}
