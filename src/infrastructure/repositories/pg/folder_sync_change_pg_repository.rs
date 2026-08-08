//! PostgreSQL-backed change-log repository for WebDAV `sync-collection`.
//!
//! Reads `storage.folder_sync_changes` (populated by triggers — see
//! `migrations/20261001000000_folder_sync_changes.sql`) and the
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

        // Capture the upper bound FIRST, then use it to bound the delta
        // query below (`seq <= $3`). Reading MAX(seq) after the delta
        // query would race a concurrent insert: it could land in a value
        // this call mints as `new_token_seq` while being invisible to the
        // already-fetched DISTINCT ON rows — silently skipped forever, since
        // the next poll asks for `seq > new_token_seq`. Bounding both
        // queries to the same captured max keeps the delivered rows and
        // the minted token describing the same snapshot.
        let max_seq: Option<i64> = sqlx::query_scalar(
            "SELECT MAX(seq) FROM storage.folder_sync_changes WHERE collection_folder_id = $1",
        )
        .bind(collection_folder_id)
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("folder_sync_changes max_seq: {e}")))?;

        let new_token_seq = max_seq.unwrap_or(since).max(since) as u64;

        // DISTINCT ON collapses churn within the window to the latest row
        // per member (e.g. trash-then-restore nets to the correct single
        // outcome instead of contradictory duplicate entries). Row order
        // here doesn't matter to the client: the handler buckets upserts
        // and deletions into separate lists before rendering (see
        // `webdav_handler.rs`'s manual split / `split_homogeneous`), so
        // no response ever preserves this query's row order anyway — the
        // real hazard that shape creates (a stale tombstone can outlive
        // a same-href recreation within one poll window) is handled by
        // dropping such tombstones before rendering, not by row order
        // here. See `webdav_sync_collection_service.rs`'s comment.
        let rows = sqlx::query_as::<_, (i64, String, Uuid, String, String)>(
            r#"
            SELECT DISTINCT ON (member_id)
                   seq, member_type, member_id, member_href_name, change_kind
              FROM storage.folder_sync_changes
             WHERE collection_folder_id = $1
               AND seq > $2
               AND seq <= $3
             ORDER BY member_id, seq DESC
            "#,
        )
        .bind(collection_folder_id)
        .bind(since)
        .bind(new_token_seq as i64)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("folder_sync_changes changes_since: {e}"))
        })?;

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

    async fn is_seq_expired(
        &self,
        collection_folder_id: Uuid,
        seq: u64,
    ) -> Result<bool, DomainError> {
        let low_water_seq: Option<i64> = sqlx::query_scalar(
            "SELECT low_water_seq FROM storage.folder_sync_watermark
              WHERE collection_folder_id = $1",
        )
        .bind(collection_folder_id)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("folder_sync_watermark read: {e}")))?;

        // No row means this collection has never had rows purged by
        // retention — never expired, regardless of `seq`.
        Ok(low_water_seq.is_some_and(|low_water| (seq as i64) < low_water))
    }

    async fn delete_expired_before(&self, cutoff: DateTime<Utc>) -> Result<u64, DomainError> {
        sqlx::query_scalar::<_, i64>(
            r#"
            WITH deleted AS (
                DELETE FROM storage.folder_sync_changes
                 WHERE changed_at < $1
             RETURNING seq, collection_folder_id
            ), per_collection AS (
                SELECT collection_folder_id, MAX(seq) AS max_seq, COUNT(*) AS n
                  FROM deleted
                 GROUP BY collection_folder_id
            ), upserted AS (
                INSERT INTO storage.folder_sync_watermark (collection_folder_id, low_water_seq)
                SELECT collection_folder_id, max_seq FROM per_collection
                ON CONFLICT (collection_folder_id) DO UPDATE
                    SET low_water_seq = GREATEST(
                        storage.folder_sync_watermark.low_water_seq,
                        EXCLUDED.low_water_seq
                    )
            )
            SELECT COALESCE(SUM(n), 0)::bigint FROM per_collection
            "#,
        )
        .bind(cutoff)
        .fetch_one(&*self.pool)
        .await
        .map(|n| n as u64)
        .map_err(|e| DomainError::database_error(format!("folder_sync_changes retention: {e}")))
    }

    async fn enforce_row_cap(&self, max_rows: u32) -> Result<u64, DomainError> {
        sqlx::query_scalar::<_, i64>(
            r#"
            WITH ranked AS (
                SELECT seq, collection_folder_id,
                       row_number() OVER (
                           PARTITION BY collection_folder_id ORDER BY seq DESC
                       ) AS rn
                  FROM storage.folder_sync_changes
            ), to_delete AS (
                DELETE FROM storage.folder_sync_changes f
                 USING ranked r
                 WHERE f.seq = r.seq AND r.rn > $1
             RETURNING f.seq, f.collection_folder_id
            ), per_collection AS (
                SELECT collection_folder_id, MAX(seq) AS max_seq, COUNT(*) AS n
                  FROM to_delete
                 GROUP BY collection_folder_id
            ), upserted AS (
                INSERT INTO storage.folder_sync_watermark (collection_folder_id, low_water_seq)
                SELECT collection_folder_id, max_seq FROM per_collection
                ON CONFLICT (collection_folder_id) DO UPDATE
                    SET low_water_seq = GREATEST(
                        storage.folder_sync_watermark.low_water_seq,
                        EXCLUDED.low_water_seq
                    )
            )
            SELECT COALESCE(SUM(n), 0)::bigint FROM per_collection
            "#,
        )
        .bind(max_rows as i64)
        .fetch_one(&*self.pool)
        .await
        .map(|n| n as u64)
        .map_err(|e| DomainError::database_error(format!("folder_sync_changes row cap: {e}")))
    }
}
