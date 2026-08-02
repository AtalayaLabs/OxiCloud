//! Generic PostgreSQL-backed change-log repository for RFC 6578
//! `sync-collection` — parameterized over table/column names via
//! `SyncChangeLogSchema` so `CalendarSyncChangePgRepository` and
//! `ContactSyncChangePgRepository` (see the two schema files next to this
//! one) collapse to a schema struct + type alias instead of a hand-copied
//! impl each. WebDAV's `FolderSyncChangePgRepository` stays hand-rolled
//! (5-column row incl. member_type, two source tables) — not a fit here.
//!
//! SQL safety note: every `{table}`/`{column}` substitution below comes
//! from a `SyncChangeLogSchema::CONST` — a compiler-controlled
//! `&'static str` fixed at the two `impl` sites in this crate, never from
//! request/user input — so the runtime `format!()`-templated SQL carries
//! no injection risk despite not going through the compile-time-checked
//! `sqlx::query!` macro (this crate uses runtime query strings throughout).

use std::marker::PhantomData;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::errors::DomainError;
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
    /// The row's identifying-label column (`"member_ical_uid"` /
    /// `"member_uid"`) — becomes `SyncChangeRow::label`.
    const LABEL_COLUMN: &'static str;
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
        since_seq: Option<u64>,
    ) -> Result<(Vec<SyncChangeRow>, u64), DomainError> {
        let since = since_seq.map(|s| s as i64).unwrap_or(0);

        // Capture the upper bound FIRST — see the WebDAV repo's
        // `changes_since` comment for why a query issued after the delta
        // fetch would race a concurrent insert and silently drop it.
        let max_sql = format!(
            "SELECT MAX(seq) FROM {} WHERE {} = $1",
            S::TABLE,
            S::COLLECTION_ID_COLUMN
        );
        let max_seq: Option<i64> = sqlx::query_scalar(&max_sql)
            .bind(collection_id)
            .fetch_one(&*self.pool)
            .await
            .map_err(|e| DomainError::database_error(format!("{}: max_seq: {e}", S::LOG_NAME)))?;

        let new_token_seq = max_seq.unwrap_or(since).max(since) as u64;

        let sql = format!(
            r#"
            SELECT DISTINCT ON (member_id)
                   member_id, {label} AS label, change_kind
              FROM {table}
             WHERE {collection_col} = $1
               AND seq > $2
               AND seq <= $3
             ORDER BY member_id, seq DESC
            "#,
            label = S::LABEL_COLUMN,
            table = S::TABLE,
            collection_col = S::COLLECTION_ID_COLUMN,
        );

        let rows = sqlx::query_as::<_, (Uuid, String, String)>(&sql)
            .bind(collection_id)
            .bind(since)
            .bind(new_token_seq as i64)
            .fetch_all(&*self.pool)
            .await
            .map_err(|e| {
                DomainError::database_error(format!("{}: changes_since: {e}", S::LOG_NAME))
            })?;

        let changes = rows
            .into_iter()
            .map(|(member_id, label, change_kind)| SyncChangeRow {
                member_id,
                label,
                kind: match change_kind.as_str() {
                    "created" => SyncChangeKind::Created,
                    "deleted" => SyncChangeKind::Deleted,
                    _ => SyncChangeKind::Updated,
                },
            })
            .collect();

        Ok((changes, new_token_seq))
    }

    async fn current_seq(&self, collection_id: Uuid) -> Result<u64, DomainError> {
        let sql = format!(
            "SELECT MAX(seq) FROM {} WHERE {} = $1",
            S::TABLE,
            S::COLLECTION_ID_COLUMN
        );
        let max_seq: Option<i64> = sqlx::query_scalar(&sql)
            .bind(collection_id)
            .fetch_one(&*self.pool)
            .await
            .map_err(|e| {
                DomainError::database_error(format!("{}: current_seq: {e}", S::LOG_NAME))
            })?;
        Ok(max_seq.unwrap_or(0) as u64)
    }

    async fn is_seq_expired(&self, collection_id: Uuid, seq: u64) -> Result<bool, DomainError> {
        let sql = format!(
            "SELECT low_water_seq FROM {} WHERE {} = $1",
            S::WATERMARK_TABLE,
            S::COLLECTION_ID_COLUMN
        );
        let low_water_seq: Option<i64> = sqlx::query_scalar(&sql)
            .bind(collection_id)
            .fetch_optional(&*self.pool)
            .await
            .map_err(|e| {
                DomainError::database_error(format!("{}: watermark read: {e}", S::LOG_NAME))
            })?;
        // No row means this collection has never had rows purged by
        // retention — never expired, regardless of `seq`.
        Ok(low_water_seq.is_some_and(|low_water| (seq as i64) < low_water))
    }

    async fn delete_expired_before(&self, cutoff: DateTime<Utc>) -> Result<u64, DomainError> {
        let sql = format!(
            r#"
            WITH deleted AS (
                DELETE FROM {table} WHERE changed_at < $1
             RETURNING seq, {collection_col}
            ), per_collection AS (
                SELECT {collection_col}, MAX(seq) AS max_seq, COUNT(*) AS n
                  FROM deleted
                 GROUP BY {collection_col}
            ), upserted AS (
                INSERT INTO {watermark} ({collection_col}, low_water_seq)
                SELECT {collection_col}, max_seq FROM per_collection
                ON CONFLICT ({collection_col}) DO UPDATE
                    SET low_water_seq = GREATEST({watermark}.low_water_seq, EXCLUDED.low_water_seq)
            )
            SELECT COALESCE(SUM(n), 0) FROM per_collection
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
             RETURNING f.seq, f.{collection_col}
            ), per_collection AS (
                SELECT {collection_col}, MAX(seq) AS max_seq, COUNT(*) AS n
                  FROM to_delete
                 GROUP BY {collection_col}
            ), upserted AS (
                INSERT INTO {watermark} ({collection_col}, low_water_seq)
                SELECT {collection_col}, max_seq FROM per_collection
                ON CONFLICT ({collection_col}) DO UPDATE
                    SET low_water_seq = GREATEST({watermark}.low_water_seq, EXCLUDED.low_water_seq)
            )
            SELECT COALESCE(SUM(n), 0) FROM per_collection
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
