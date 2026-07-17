//! PostgreSQL-backed change-log repository for WebDAV `sync-collection`.
//!
//! Reads `storage.folder_sync_changes` (populated by triggers — see
//! `migrations/20260911000000_folder_sync_changes.sql`) and the
//! `storage.folder_sync_watermark` singleton row maintained by
//! `SyncLogRetentionService`.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::errors::DomainError;
use crate::domain::repositories::folder_sync_change_repository::{
    FolderSyncChangeRepository, FolderSyncChangeRow, SyncChangeKind, SyncMemberType,
};

pub struct FolderSyncChangePgRepository {
    pool: Arc<PgPool>,
}

impl FolderSyncChangePgRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }
}

impl FolderSyncChangeRepository for FolderSyncChangePgRepository {
    async fn changes_since(
        &self,
        collection_folder_id: Uuid,
        since_seq: Option<u64>,
    ) -> Result<(Vec<FolderSyncChangeRow>, u64), DomainError> {
        let since = since_seq.map(|s| s as i64).unwrap_or(0);

        // DISTINCT ON collapses churn within the window to the latest row
        // per member (e.g. trash-then-restore nets to the correct single
        // outcome instead of contradictory duplicate entries).
        let rows = sqlx::query_as::<_, (i64, String, Uuid, String, String)>(
            r#"
            SELECT DISTINCT ON (member_id)
                   seq, member_type, member_id, member_href_name, change_kind
              FROM storage.folder_sync_changes
             WHERE collection_folder_id = $1
               AND seq > $2
             ORDER BY member_id, seq DESC
            "#,
        )
        .bind(collection_folder_id)
        .bind(since)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("folder_sync_changes changes_since: {e}"))
        })?;

        let max_seq: Option<i64> = sqlx::query_scalar(
            "SELECT MAX(seq) FROM storage.folder_sync_changes WHERE collection_folder_id = $1",
        )
        .bind(collection_folder_id)
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("folder_sync_changes max_seq: {e}")))?;

        let new_token_seq = max_seq.unwrap_or(since).max(since) as u64;

        let changes = rows
            .into_iter()
            .map(
                |(seq, member_type, member_id, href_name, change_kind)| FolderSyncChangeRow {
                    seq: seq as u64,
                    member_type: match member_type.as_str() {
                        "folder" => SyncMemberType::Folder,
                        _ => SyncMemberType::File,
                    },
                    member_id,
                    href_name,
                    kind: match change_kind.as_str() {
                        "created" => SyncChangeKind::Created,
                        "deleted" => SyncChangeKind::Deleted,
                        _ => SyncChangeKind::Updated,
                    },
                },
            )
            .collect();

        Ok((changes, new_token_seq))
    }

    async fn current_seq(&self, collection_folder_id: Uuid) -> Result<u64, DomainError> {
        let max_seq: Option<i64> = sqlx::query_scalar(
            "SELECT MAX(seq) FROM storage.folder_sync_changes WHERE collection_folder_id = $1",
        )
        .bind(collection_folder_id)
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("folder_sync_changes current_seq: {e}"))
        })?;

        Ok(max_seq.unwrap_or(0) as u64)
    }

    async fn is_seq_expired(&self, seq: u64) -> Result<bool, DomainError> {
        let low_water_seq: i64 = sqlx::query_scalar(
            "SELECT low_water_seq FROM storage.folder_sync_watermark WHERE singleton = TRUE",
        )
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("folder_sync_watermark read: {e}")))?;

        Ok((seq as i64) < low_water_seq)
    }

    async fn delete_expired_before(&self, cutoff: DateTime<Utc>) -> Result<u64, DomainError> {
        let mut tx = self.pool.begin().await.map_err(|e| {
            DomainError::database_error(format!("folder_sync_changes retention begin: {e}"))
        })?;

        let deleted_seqs: Vec<i64> = sqlx::query_scalar(
            "DELETE FROM storage.folder_sync_changes WHERE changed_at < $1 RETURNING seq",
        )
        .bind(cutoff)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("folder_sync_changes retention delete: {e}"))
        })?;

        let deleted_count = deleted_seqs.len() as u64;

        if let Some(max_seq) = deleted_seqs.into_iter().max() {
            sqlx::query(
                "UPDATE storage.folder_sync_watermark
                    SET low_water_seq = GREATEST(low_water_seq, $1)
                  WHERE singleton = TRUE",
            )
            .bind(max_seq)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                DomainError::database_error(format!("folder_sync_watermark retention advance: {e}"))
            })?;
        }

        tx.commit().await.map_err(|e| {
            DomainError::database_error(format!("folder_sync_changes retention commit: {e}"))
        })?;

        Ok(deleted_count)
    }
}
