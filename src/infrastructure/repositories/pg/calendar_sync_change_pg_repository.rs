//! PostgreSQL-backed change-log repository for CalDAV `sync-collection`.
//!
//! Mirrors `folder_sync_change_pg_repository.rs`. Reads
//! `caldav.calendar_sync_changes` (populated by triggers — see
//! `migrations/20260911000001_calendar_sync_changes.sql`) and the
//! `caldav.calendar_sync_watermark` singleton row.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::errors::DomainError;
use crate::domain::repositories::calendar_sync_change_repository::{
    CalendarSyncChangeRepository, CalendarSyncChangeRow, SyncChangeKind,
};

pub struct CalendarSyncChangePgRepository {
    pool: Arc<PgPool>,
}

impl CalendarSyncChangePgRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }
}

impl CalendarSyncChangeRepository for CalendarSyncChangePgRepository {
    async fn changes_since(
        &self,
        collection_calendar_id: Uuid,
        since_seq: Option<u64>,
    ) -> Result<(Vec<CalendarSyncChangeRow>, u64), DomainError> {
        let since = since_seq.map(|s| s as i64).unwrap_or(0);

        let rows = sqlx::query_as::<_, (Uuid, String, String)>(
            r#"
            SELECT DISTINCT ON (member_id)
                   member_id, member_ical_uid, change_kind
              FROM caldav.calendar_sync_changes
             WHERE collection_calendar_id = $1
               AND seq > $2
             ORDER BY member_id, seq DESC
            "#,
        )
        .bind(collection_calendar_id)
        .bind(since)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("calendar_sync_changes changes_since: {e}"))
        })?;

        let max_seq: Option<i64> = sqlx::query_scalar(
            "SELECT MAX(seq) FROM caldav.calendar_sync_changes WHERE collection_calendar_id = $1",
        )
        .bind(collection_calendar_id)
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("calendar_sync_changes max_seq: {e}")))?;

        let new_token_seq = max_seq.unwrap_or(since).max(since) as u64;

        let changes = rows
            .into_iter()
            .map(|(member_id, ical_uid, change_kind)| CalendarSyncChangeRow {
                member_id,
                ical_uid,
                kind: match change_kind.as_str() {
                    "created" => SyncChangeKind::Created,
                    "deleted" => SyncChangeKind::Deleted,
                    _ => SyncChangeKind::Updated,
                },
            })
            .collect();

        Ok((changes, new_token_seq))
    }

    async fn current_seq(&self, collection_calendar_id: Uuid) -> Result<u64, DomainError> {
        let max_seq: Option<i64> = sqlx::query_scalar(
            "SELECT MAX(seq) FROM caldav.calendar_sync_changes WHERE collection_calendar_id = $1",
        )
        .bind(collection_calendar_id)
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("calendar_sync_changes current_seq: {e}"))
        })?;

        Ok(max_seq.unwrap_or(0) as u64)
    }

    async fn is_seq_expired(&self, seq: u64) -> Result<bool, DomainError> {
        let low_water_seq: i64 = sqlx::query_scalar(
            "SELECT low_water_seq FROM caldav.calendar_sync_watermark WHERE singleton = TRUE",
        )
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("calendar_sync_watermark read: {e}")))?;

        Ok((seq as i64) < low_water_seq)
    }

    async fn delete_expired_before(&self, cutoff: DateTime<Utc>) -> Result<u64, DomainError> {
        let mut tx = self.pool.begin().await.map_err(|e| {
            DomainError::database_error(format!("calendar_sync_changes retention begin: {e}"))
        })?;

        let deleted_seqs: Vec<i64> = sqlx::query_scalar(
            "DELETE FROM caldav.calendar_sync_changes WHERE changed_at < $1 RETURNING seq",
        )
        .bind(cutoff)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("calendar_sync_changes retention delete: {e}"))
        })?;

        let deleted_count = deleted_seqs.len() as u64;

        if let Some(max_seq) = deleted_seqs.into_iter().max() {
            sqlx::query(
                "UPDATE caldav.calendar_sync_watermark
                    SET low_water_seq = GREATEST(low_water_seq, $1)
                  WHERE singleton = TRUE",
            )
            .bind(max_seq)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                DomainError::database_error(format!(
                    "calendar_sync_watermark retention advance: {e}"
                ))
            })?;
        }

        tx.commit().await.map_err(|e| {
            DomainError::database_error(format!("calendar_sync_changes retention commit: {e}"))
        })?;

        Ok(deleted_count)
    }
}
