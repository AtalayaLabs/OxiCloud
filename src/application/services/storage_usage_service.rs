use crate::application::ports::auth_ports::UserStoragePort;
use crate::application::ports::storage_ports::StorageUsagePort;
use crate::common::errors::DomainError;
use crate::infrastructure::repositories::pg::UserPgRepository;
use sqlx::PgPool;
use std::sync::Arc;
use tokio::task;
use tracing::{debug, error, info};
use uuid::Uuid;

/**
 * Service for managing and updating user storage usage statistics.
 *
 * This service is responsible for calculating how much storage each user
 * is using and updating this information in the user records.
 *
 * Storage usage is calculated directly from the `storage.files` table
 * by summing file sizes for each user (using the `user_id` column).
 */
pub struct StorageUsageService {
    pool: Arc<PgPool>,
    user_repository: Arc<UserPgRepository>,
}

impl StorageUsageService {
    /// Creates a new storage usage service
    pub fn new(pool: Arc<PgPool>, user_repository: Arc<UserPgRepository>) -> Self {
        Self {
            pool,
            user_repository,
        }
    }

    /// Recalculates and stores one user's usage in a single statement.
    ///
    /// The correlated `SUM(size)` over the user's non-trashed files is
    /// O(number of files) but runs as an index-only scan on the
    /// `idx_files_user_size_active` covering partial index. One round-trip
    /// (was three: user lookup + SUM + UPDATE). NOT called on the request
    /// path — only by the per-upload background update and the sweep.
    pub async fn update_user_storage_usage(&self, user_id: Uuid) -> Result<i64, DomainError> {
        let total_usage: Option<i64> = sqlx::query_scalar(
            r#"
            UPDATE auth.users u
               SET storage_used_bytes = COALESCE((
                       SELECT SUM(f.size)::bigint
                         FROM storage.files f
                        WHERE f.user_id = u.id AND NOT f.is_trashed), 0)
             WHERE u.id = $1
            RETURNING u.storage_used_bytes
            "#,
        )
        .bind(user_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| {
            DomainError::internal_error("StorageUsage", format!("Failed to update usage: {e}"))
        })?;

        let total_usage = total_usage
            .ok_or_else(|| DomainError::not_found("User", format!("User ID: {user_id}")))?;

        debug!(
            "Updated storage usage for user {} to {} bytes",
            user_id, total_usage
        );

        Ok(total_usage)
    }

    /// Same as [`Self::update_user_storage_usage`], keyed by username.
    pub async fn update_user_storage_usage_by_username(
        &self,
        username: &str,
    ) -> Result<i64, DomainError> {
        let total_usage: Option<i64> = sqlx::query_scalar(
            r#"
            UPDATE auth.users u
               SET storage_used_bytes = COALESCE((
                       SELECT SUM(f.size)::bigint
                         FROM storage.files f
                        WHERE f.user_id = u.id AND NOT f.is_trashed), 0)
             WHERE u.username = $1
            RETURNING u.storage_used_bytes
            "#,
        )
        .bind(username)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| {
            DomainError::internal_error("StorageUsage", format!("Failed to update usage: {e}"))
        })?;

        let total_usage =
            total_usage.ok_or_else(|| DomainError::not_found("User", username.to_string()))?;

        debug!(
            "Updated storage usage for username {} to {} bytes",
            username, total_usage
        );

        Ok(total_usage)
    }

    /// Incrementally adjust one user's cached usage by `delta` bytes — O(1),
    /// the per-upload counterpart to the O(N) full recompute above. An upload
    /// adds `+size` (was a full `SUM(size)` over every file the user owns, i.e.
    /// O(N) per upload and O(N²) for a bulk upload). Deletes/trash do not
    /// decrement here (they never did); the periodic reconciliation sweep
    /// ([`StorageUsagePort::update_all_users_storage_usage`]) remains the
    /// correctness backstop for every mutation. Clamped at 0 so a late or
    /// duplicate adjustment can never drive the counter negative.
    pub async fn add_user_storage_usage_delta(
        &self,
        user_id: Uuid,
        delta: i64,
    ) -> Result<(), DomainError> {
        sqlx::query(
            "UPDATE auth.users
                SET storage_used_bytes = GREATEST(0, storage_used_bytes + $2)
              WHERE id = $1",
        )
        .bind(user_id)
        .bind(delta)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("StorageUsage", format!("usage delta: {e}")))?;
        Ok(())
    }

    /// Spawn a background task that periodically reconciles every user's cached
    /// `storage_used_bytes` against the actual sum of their files.
    ///
    /// `GET /api/auth/me` no longer recomputes usage on the request path; this
    /// sweep (plus the per-upload update) keeps the cached value current for
    /// all mutations — including deletes and trash — without any O(N) work on a
    /// hot endpoint. Runs on the maintenance pool. The first sweep is deferred
    /// by one interval so it never adds load at boot.
    pub fn start_reconciliation_job(&self, interval_secs: u64) {
        // Floor the interval so a misconfiguration can't busy-loop the sweep.
        let interval_secs = interval_secs.max(30);
        let service = self.clone();
        info!(
            "Starting storage-usage reconciliation job (every {}s)",
            interval_secs
        );
        task::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
            // tokio's first `tick()` fires immediately — consume it so the
            // first real sweep happens one interval after startup.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                debug!("Running scheduled storage-usage reconciliation");
                if let Err(e) = service.update_all_users_storage_usage().await {
                    error!("Scheduled storage-usage reconciliation failed: {}", e);
                }
            }
        });
    }
}

/**
 * Implementation of the StorageUsagePort trait to expose storage usage services
 * to the application layer.
 */
impl StorageUsagePort for StorageUsageService {
    async fn update_user_storage_usage(&self, user_id: Uuid) -> Result<i64, DomainError> {
        StorageUsageService::update_user_storage_usage(self, user_id).await
    }

    async fn update_user_storage_usage_by_username(
        &self,
        username: &str,
    ) -> Result<i64, DomainError> {
        StorageUsageService::update_user_storage_usage_by_username(self, username).await
    }

    /// Reconcile every internal user's cached usage in ONE set-based UPDATE.
    ///
    /// Replaces the previous shape (paginated user list + one spawned task
    /// per user, each issuing SUM + UPDATE — up to 2N queries and N
    /// concurrent tasks fighting for pool connections). A single GROUP BY
    /// over the covering index feeds all users at once, and the
    /// `IS DISTINCT FROM` guard skips rewriting rows whose value didn't
    /// change (no dead-tuple churn for idle users). This also removes the
    /// old `LIMIT 1000` page cap, which silently left users beyond the
    /// first thousand unreconciled.
    ///
    /// External users are excluded — they carry no storage by construction
    /// (DB CHECK `users_external_no_storage`).
    async fn update_all_users_storage_usage(&self) -> Result<(), DomainError> {
        debug!("Starting storage-usage reconciliation sweep");

        let result = sqlx::query(
            r#"
            UPDATE auth.users u
               SET storage_used_bytes = COALESCE(t.total, 0)
              FROM auth.users u2
              LEFT JOIN (
                    SELECT user_id, SUM(size)::bigint AS total
                      FROM storage.files
                     WHERE NOT is_trashed
                     GROUP BY user_id
                   ) t ON t.user_id = u2.id
             WHERE u.id = u2.id
               AND NOT u2.is_external
               AND u.storage_used_bytes IS DISTINCT FROM COALESCE(t.total, 0)
            "#,
        )
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| {
            error!("Storage-usage reconciliation sweep failed: {}", e);
            DomainError::internal_error("StorageUsage", format!("reconciliation sweep: {e}"))
        })?;

        info!(
            "Storage-usage reconciliation corrected {} user(s)",
            result.rows_affected()
        );
        Ok(())
    }

    async fn check_storage_quota(
        &self,
        user_id: Uuid,
        additional_bytes: u64,
    ) -> Result<(), DomainError> {
        let user = self.user_repository.get_user_by_id(user_id).await?;
        let quota = user.storage_quota_bytes();
        let used = user.storage_used_bytes();

        // Quota of 0 means unlimited
        if quota <= 0 {
            return Ok(());
        }

        let additional = additional_bytes as i64;

        // Case 1: the single file alone exceeds the entire quota
        if additional > quota {
            let quota_fmt = format_bytes(quota);
            let file_fmt = format_bytes(additional);
            return Err(DomainError::quota_exceeded(format!(
                "File size ({}) exceeds your total storage quota ({})",
                file_fmt, quota_fmt
            )));
        }

        // Case 2: the upload would push usage over the quota
        if used + additional > quota {
            let available = (quota - used).max(0);
            let avail_fmt = format_bytes(available);
            let file_fmt = format_bytes(additional);
            return Err(DomainError::quota_exceeded(format!(
                "Not enough storage space. File size: {}, available: {}",
                file_fmt, avail_fmt
            )));
        }

        Ok(())
    }

    async fn get_user_storage_info(&self, user_id: Uuid) -> Result<(i64, i64), DomainError> {
        let user = self.user_repository.get_user_by_id(user_id).await?;
        Ok((user.storage_used_bytes(), user.storage_quota_bytes()))
    }
}

// Make StorageUsageService cloneable to support spawning concurrent tasks
impl Clone for StorageUsageService {
    fn clone(&self) -> Self {
        Self {
            pool: Arc::clone(&self.pool),
            user_repository: Arc::clone(&self.user_repository),
        }
    }
}

/// Format bytes into human-readable units for error messages.
fn format_bytes(bytes: i64) -> String {
    const KB: i64 = 1024;
    const MB: i64 = KB * 1024;
    const GB: i64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}
