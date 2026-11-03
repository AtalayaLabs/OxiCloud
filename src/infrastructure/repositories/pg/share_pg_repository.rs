use sqlx::{PgPool, Row};
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    application::ports::share_ports::ShareStoragePort,
    common::errors::DomainError,
    domain::entities::share::{Share, ShareItemType},
};

/// PostgreSQL implementation of [`ShareStoragePort`].
///
/// Replaces the legacy file-based `ShareFsRepository` that read/wrote the
/// entire `shares.json` on every operation. Each method now issues a single
/// indexed SQL statement — O(1) lookups, ACID transactions, and no data-race
/// risk.
pub struct SharePgRepository {
    db_pool: Arc<PgPool>,
}

impl SharePgRepository {
    pub fn new(db_pool: Arc<PgPool>) -> Self {
        Self { db_pool }
    }

    /// Creates a stub instance for testing — never hits PG.
    #[cfg(test)]
    pub fn new_stub() -> Self {
        Self {
            db_pool: Arc::new(
                sqlx::pool::PoolOptions::<sqlx::Postgres>::new()
                    .max_connections(1)
                    .connect_lazy("postgres://invalid:5432/none")
                    .unwrap(),
            ),
        }
    }

    /// Maps a [`sqlx::postgres::PgRow`] to the domain [`Share`] entity.
    /// Expects columns: id, item_id, item_name, item_type, token, password_hash,
    /// expires_at (derived from role_grants subquery), created_at, created_by, access_count.
    fn row_to_entity(row: &sqlx::postgres::PgRow) -> Result<Share, DomainError> {
        let id: Uuid = row
            .try_get("id")
            .map_err(|e| DomainError::internal_error("Share", format!("Failed to read id: {e}")))?;
        let item_id: String = row.try_get("item_id").map_err(|e| {
            DomainError::internal_error("Share", format!("Failed to read item_id: {e}"))
        })?;
        let item_name: Option<String> = row.try_get("item_name").unwrap_or(None);
        let item_type_str: String = row.try_get("item_type").map_err(|e| {
            DomainError::internal_error("Share", format!("Failed to read item_type: {e}"))
        })?;
        let token: String = row.try_get("token").map_err(|e| {
            DomainError::internal_error("Share", format!("Failed to read token: {e}"))
        })?;
        let password_hash: Option<String> = row.try_get("password_hash").unwrap_or(None);
        // expires_at derived from role_grants subquery (unix seconds as i64)
        let expires_at: Option<i64> = row.try_get("expires_at").unwrap_or(None);
        let created_at: i64 = row.try_get("created_at").map_err(|e| {
            DomainError::internal_error("Share", format!("Failed to read created_at: {e}"))
        })?;
        let created_by: Uuid = row.try_get("created_by").map_err(|e| {
            DomainError::internal_error("Share", format!("Failed to read created_by: {e}"))
        })?;
        let access_count: i64 = row.try_get("access_count").unwrap_or(0);
        let max_downloads: Option<i64> = row.try_get("max_downloads").unwrap_or(None);
        let download_count: i64 = row.try_get("download_count").unwrap_or(0);

        let item_type =
            ShareItemType::try_from(item_type_str.as_str()).unwrap_or(ShareItemType::File);

        Ok(Share::from_raw(
            id,
            item_id,
            item_name,
            item_type,
            token,
            password_hash,
            expires_at.map(|v| v as u64),
            created_at as u64,
            created_by,
            access_count as u64,
            max_downloads.map(|v| v as u32),
            download_count as u64,
        ))
    }
}

impl ShareStoragePort for SharePgRepository {
    async fn save_share(&self, share: &Share) -> Result<Share, DomainError> {
        let row = sqlx::query(
            r#"
            INSERT INTO storage.shares
                (id, item_id, item_name, item_type, token, password_hash,
                 created_at, created_by, access_count, max_downloads, download_count)
            VALUES
                ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            ON CONFLICT (id) DO UPDATE SET
                item_name     = EXCLUDED.item_name,
                password_hash = EXCLUDED.password_hash,
                access_count  = EXCLUDED.access_count,
                max_downloads = EXCLUDED.max_downloads
            RETURNING
                id, item_id, item_name, item_type, token, password_hash,
                (SELECT MIN(EXTRACT(EPOCH FROM ag.expires_at)::BIGINT)
                 FROM storage.role_grants ag
                 WHERE ag.subject_type = 'token' AND ag.subject_id = id) AS expires_at,
                created_at, created_by, access_count, max_downloads, download_count
            "#,
        )
        .bind(share.id())
        .bind(share.item_id())
        .bind(share.item_name())
        .bind(share.item_type().to_string())
        .bind(share.token())
        .bind(share.password_hash())
        .bind(share.created_at() as i64)
        .bind(share.created_by())
        .bind(share.access_count() as i64)
        .bind(share.max_downloads().map(i64::from))
        .bind(share.download_count() as i64)
        .fetch_one(&*self.db_pool)
        .await
        .map_err(|e| {
            tracing::error!("Database error saving share: {}", e);
            DomainError::internal_error("Share", format!("Failed to save share: {e}"))
        })?;

        Self::row_to_entity(&row)
    }

    async fn increment_access_count(&self, token: &str) -> Result<u64, DomainError> {
        // One atomic statement — the relative bump can't lose concurrent
        // increments and never rewrites unrelated columns (the legacy
        // read-modify-write wrote back item_name/password_hash wholesale,
        // silently clobbering concurrent owner edits). The expiry guard
        // mirrors find_share_by_token's MIN(expires_at) subquery: NULL =
        // never expires.
        let result = sqlx::query(
            r#"
            UPDATE storage.shares s
               SET access_count = s.access_count + 1
             WHERE s.token = $1
               AND COALESCE(
                     (SELECT MIN(ag.expires_at)
                        FROM storage.role_grants ag
                       WHERE ag.subject_type = 'token' AND ag.subject_id = s.id) > NOW(),
                     TRUE)
            "#,
        )
        .bind(token)
        .execute(&*self.db_pool)
        .await
        .map_err(|e| {
            tracing::error!("Database error incrementing share access count: {}", e);
            DomainError::internal_error("Share", format!("Failed to register access: {e}"))
        })?;
        Ok(result.rows_affected())
    }

    async fn consume_download(&self, share_id: Uuid) -> Result<bool, DomainError> {
        // Check and increment in one statement: two round-trips would let
        // concurrent visitors both read count = max - 1 and both proceed.
        let result = sqlx::query(
            r#"
            UPDATE storage.shares
               SET download_count = download_count + 1
             WHERE id = $1
               AND (max_downloads IS NULL OR download_count < max_downloads)
            "#,
        )
        .bind(share_id)
        .execute(&*self.db_pool)
        .await
        .map_err(|e| {
            tracing::error!("Database error consuming share download: {}", e);
            DomainError::internal_error("Share", format!("Failed to register download: {e}"))
        })?;
        Ok(result.rows_affected() > 0)
    }

    async fn find_share_by_token(&self, token: &str) -> Result<Share, DomainError> {
        let row = sqlx::query(
            r#"
            SELECT s.id, s.item_id, s.item_name, s.item_type, s.token, s.password_hash,
                (SELECT MIN(EXTRACT(EPOCH FROM ag.expires_at)::BIGINT)
                 FROM storage.role_grants ag
                 WHERE ag.subject_type = 'token' AND ag.subject_id = s.id) AS expires_at,
                s.created_at, s.created_by, s.access_count, s.max_downloads, s.download_count
            FROM storage.shares s
            WHERE s.token = $1
            "#,
        )
        .bind(token)
        .fetch_optional(&*self.db_pool)
        .await
        .map_err(|e| {
            tracing::error!("Database error finding share by token: {}", e);
            DomainError::internal_error("Share", format!("Failed to find share by token: {e}"))
        })?;

        match row {
            Some(r) => Self::row_to_entity(&r),
            None => Err(DomainError::not_found(
                "Share",
                format!("Share with token {token} not found"),
            )),
        }
    }

    async fn find_share_by_id_for_user(
        &self,
        id: Uuid,
        user_id: Uuid,
    ) -> Result<Share, DomainError> {
        let row = sqlx::query(
            r#"
            SELECT s.id, s.item_id, s.item_name, s.item_type, s.token, s.password_hash,
                (SELECT MIN(EXTRACT(EPOCH FROM ag.expires_at)::BIGINT)
                 FROM storage.role_grants ag
                 WHERE ag.subject_type = 'token' AND ag.subject_id = s.id) AS expires_at,
                s.created_at, s.created_by, s.access_count, s.max_downloads, s.download_count
            FROM storage.shares s
            WHERE s.id = $1 AND s.created_by = $2
            "#,
        )
        .bind(id)
        .bind(user_id)
        .fetch_optional(&*self.db_pool)
        .await
        .map_err(|e| {
            tracing::error!("Database error finding share by id for user: {}", e);
            DomainError::internal_error("Share", format!("Failed to find share: {e}"))
        })?;

        match row {
            Some(r) => Self::row_to_entity(&r),
            // SECURITY: return NotFound (not Forbidden) to prevent share-ID enumeration
            None => Err(DomainError::not_found(
                "Share",
                format!("Share with ID {id} not found"),
            )),
        }
    }

    async fn delete_share_for_user(&self, id: Uuid, user_id: Uuid) -> Result<(), DomainError> {
        let result = sqlx::query("DELETE FROM storage.shares WHERE id = $1 AND created_by = $2")
            .bind(id)
            .bind(user_id)
            .execute(&*self.db_pool)
            .await
            .map_err(|e| {
                tracing::error!("Database error deleting share for user: {}", e);
                DomainError::internal_error("Share", format!("Failed to delete share: {e}"))
            })?;

        if result.rows_affected() == 0 {
            // SECURITY: could be non-existent or owned by another user — same 404
            return Err(DomainError::not_found(
                "Share",
                format!("Share with ID {id} not found"),
            ));
        }

        Ok(())
    }

    async fn find_shares_by_item_for_user(
        &self,
        item_id: &str,
        item_type: &ShareItemType,
        user_id: Uuid,
    ) -> Result<Vec<Share>, DomainError> {
        let rows = sqlx::query(
            r#"
            SELECT s.id, s.item_id, s.item_name, s.item_type, s.token, s.password_hash,
                (SELECT MIN(EXTRACT(EPOCH FROM ag.expires_at)::BIGINT)
                 FROM storage.role_grants ag
                 WHERE ag.subject_type = 'token' AND ag.subject_id = s.id) AS expires_at,
                s.created_at, s.created_by, s.access_count, s.max_downloads, s.download_count
            FROM storage.shares s
            WHERE s.item_id = $1 AND s.item_type = $2 AND s.created_by = $3
            ORDER BY s.created_at DESC
            "#,
        )
        .bind(item_id)
        .bind(item_type.to_string())
        .bind(user_id)
        .fetch_all(&*self.db_pool)
        .await
        .map_err(|e| {
            tracing::error!("Database error finding shares by item for user: {}", e);
            DomainError::internal_error("Share", format!("Failed to find shares by item: {e}"))
        })?;

        rows.iter().map(Self::row_to_entity).collect()
    }

    async fn update_share(&self, share: &Share) -> Result<Share, DomainError> {
        let row = sqlx::query(
            r#"
            UPDATE storage.shares SET
                item_name     = $2,
                password_hash = $3,
                access_count  = $4,
                max_downloads = $5
            WHERE id = $1
            RETURNING
                id, item_id, item_name, item_type, token, password_hash,
                (SELECT MIN(EXTRACT(EPOCH FROM ag.expires_at)::BIGINT)
                 FROM storage.role_grants ag
                 WHERE ag.subject_type = 'token' AND ag.subject_id = storage.shares.id) AS expires_at,
                created_at, created_by, access_count, max_downloads, download_count
            "#,
        )
        .bind(share.id())
        .bind(share.item_name())
        .bind(share.password_hash())
        .bind(share.access_count() as i64)
        .bind(share.max_downloads().map(i64::from))
        .fetch_optional(&*self.db_pool)
        .await
        .map_err(|e| {
            tracing::error!("Database error updating share: {}", e);
            DomainError::internal_error("Share", format!("Failed to update share: {e}"))
        })?;

        match row {
            Some(r) => Self::row_to_entity(&r),
            None => Err(DomainError::not_found(
                "Share",
                format!("Share with ID {} not found for update", share.id()),
            )),
        }
    }

    async fn find_shares_by_user(
        &self,
        user_id: Uuid,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<Share>, usize), DomainError> {
        // Single query with window function — count + rows in one roundtrip
        let rows = sqlx::query(
            r#"
            SELECT s.id, s.item_id, s.item_name, s.item_type, s.token, s.password_hash,
                (SELECT MIN(EXTRACT(EPOCH FROM ag.expires_at)::BIGINT)
                 FROM storage.role_grants ag
                 WHERE ag.subject_type = 'token' AND ag.subject_id = s.id) AS expires_at,
                s.created_at, s.created_by, s.access_count, s.max_downloads, s.download_count,
                COUNT(*) OVER() AS total_count
            FROM storage.shares s
            WHERE s.created_by = $1
            ORDER BY s.created_at DESC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(user_id)
        .bind(limit as i64)
        .bind(offset as i64)
        .fetch_all(&*self.db_pool)
        .await
        .map_err(|e| {
            tracing::error!("Database error finding shares by user: {}", e);
            DomainError::internal_error("Share", format!("Failed to find shares by user: {e}"))
        })?;

        let total: usize = rows
            .first()
            .and_then(|r| r.try_get::<i64, _>("total_count").ok())
            .unwrap_or(0) as usize;

        let shares: Result<Vec<Share>, DomainError> =
            rows.iter().map(Self::row_to_entity).collect();

        Ok((shares?, total))
    }
}
