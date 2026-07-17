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
    /// Fully-qualified watermark singleton table, e.g.
    /// `"caldav.calendar_sync_watermark"`.
    const WATERMARK_TABLE: &'static str;
    /// The column scoping rows to one collection (`"collection_calendar_id"`
    /// / `"collection_address_book_id"`).
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

        let sql = format!(
            r#"
            SELECT DISTINCT ON (member_id)
                   member_id, {label} AS label, change_kind
              FROM {table}
             WHERE {collection_col} = $1
               AND seq > $2
             ORDER BY member_id, seq DESC
            "#,
            label = S::LABEL_COLUMN,
            table = S::TABLE,
            collection_col = S::COLLECTION_ID_COLUMN,
        );

        let rows = sqlx::query_as::<_, (Uuid, String, String)>(&sql)
            .bind(collection_id)
            .bind(since)
            .fetch_all(&*self.pool)
            .await
            .map_err(|e| {
                DomainError::database_error(format!("{}: changes_since: {e}", S::LOG_NAME))
            })?;

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

    async fn is_seq_expired(&self, seq: u64) -> Result<bool, DomainError> {
        let sql = format!(
            "SELECT low_water_seq FROM {} WHERE singleton = TRUE",
            S::WATERMARK_TABLE
        );
        let low_water_seq: i64 = sqlx::query_scalar(&sql)
            .fetch_one(&*self.pool)
            .await
            .map_err(|e| {
                DomainError::database_error(format!("{}: watermark read: {e}", S::LOG_NAME))
            })?;
        Ok((seq as i64) < low_water_seq)
    }

    async fn delete_expired_before(&self, cutoff: DateTime<Utc>) -> Result<u64, DomainError> {
        let mut tx = self.pool.begin().await.map_err(|e| {
            DomainError::database_error(format!("{}: retention begin: {e}", S::LOG_NAME))
        })?;

        let delete_sql = format!(
            "DELETE FROM {} WHERE changed_at < $1 RETURNING seq",
            S::TABLE
        );
        let deleted_seqs: Vec<i64> = sqlx::query_scalar(&delete_sql)
            .bind(cutoff)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| {
                DomainError::database_error(format!("{}: retention delete: {e}", S::LOG_NAME))
            })?;

        let deleted_count = deleted_seqs.len() as u64;

        if let Some(max_seq) = deleted_seqs.into_iter().max() {
            let update_sql = format!(
                "UPDATE {} SET low_water_seq = GREATEST(low_water_seq, $1) WHERE singleton = TRUE",
                S::WATERMARK_TABLE
            );
            sqlx::query(&update_sql)
                .bind(max_seq)
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    DomainError::database_error(format!("{}: watermark advance: {e}", S::LOG_NAME))
                })?;
        }

        tx.commit().await.map_err(|e| {
            DomainError::database_error(format!("{}: retention commit: {e}", S::LOG_NAME))
        })?;

        Ok(deleted_count)
    }
}
