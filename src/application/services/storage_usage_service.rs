use crate::application::ports::storage_ports::StorageUsagePort;
use crate::common::errors::DomainError;
use crate::infrastructure::repositories::pg::UserPgRepository;
use sqlx::PgPool;
use std::sync::Arc;
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
/// Fused quota-gate row: `(user_used, user_quota, drive_used, drive_quota,
/// drive_found)` — see [`StorageUsageService::check_upload_quotas`].
type QuotaPairRow = (i64, i64, Option<i64>, Option<i64>, bool);

pub struct StorageUsageService {
    pool: Arc<PgPool>,
    user_repository: Arc<UserPgRepository>,
    /// Optional so DI can wire it lazily and older test constructors
    /// keep compiling. When `Some`, every write path that mutates
    /// `drives.used_bytes` or `users.storage_used_bytes` invalidates
    /// the drive lookup caches so `GET /api/drives` reflects the new
    /// usage on the next call (see the invalidation calls in the
    /// delta / sweep methods below).
    drive_repo: Option<Arc<dyn crate::domain::repositories::drive_repository::DriveRepository>>,
}

impl StorageUsageService {
    /// Creates a new storage usage service
    pub fn new(pool: Arc<PgPool>, user_repository: Arc<UserPgRepository>) -> Self {
        Self {
            pool,
            user_repository,
            drive_repo: None,
        }
    }

    /// Wires the drive repository used for cache-invalidation-on-write.
    /// Production DI calls this in `common::di`; tests without a real
    /// drive repo leave it `None` and the invalidation calls no-op.
    pub fn with_drive_repo(
        mut self,
        drive_repo: Arc<dyn crate::domain::repositories::drive_repository::DriveRepository>,
    ) -> Self {
        self.drive_repo = Some(drive_repo);
        self
    }

    /// Drop the per-caller readable-drive listing cache and the
    /// per-user default-drive cache so `GET /api/drives` and the
    /// WebDAV / NextCloud / WOPI drive-lookup paths re-read fresh
    /// values.
    ///
    /// **Called only from the reconciliation sweep**, not from the
    /// hot-path `add_drive_storage_usage_delta*` methods. The design
    /// (Ed's call, 2026-07-17): keep the cache useful under active
    /// upload load — per-mutation invalidation would nuke the cache
    /// on every file upload, defeating the point. `used_bytes` on
    /// `GET /api/drives` therefore lags by up to the cache TTL (30 s),
    /// which matches the sibling caches' accepted UX phantom for
    /// drive-name staleness. Tests / operators that need immediate
    /// freshness call `POST /api/admin/internal/trigger-sweep`, which
    /// runs `update_all_drives_storage_usage` → this method.
    ///
    /// Security posture unaffected: `check_drive_quota` reads
    /// directly from SQL, bypassing the cache entirely, so quota
    /// enforcement is honest regardless of listing staleness.
    fn invalidate_drive_lookup_caches(&self) {
        if let Some(repo) = &self.drive_repo {
            repo.invalidate_readable_all();
            repo.invalidate_default_drive_all();
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
        // User envelope = SUM of `drives.used_bytes` across personal
        // drives owned by the user (see `docs/plan/drive.md` §7). Shared
        // drives don't count. Ownership is canonical via `role_grants`.
        let total_usage: Option<i64> = sqlx::query_scalar(
            r#"
            UPDATE auth.users u
               SET storage_used_bytes = COALESCE((
                       SELECT SUM(d.used_bytes)::bigint
                         FROM storage.drives      d
                         JOIN storage.role_grants g
                           ON g.resource_type = 'drive'
                          AND g.resource_id   = d.id
                          AND g.role          = 'owner'
                          AND g.subject_type  = 'user'
                          AND g.subject_id    = u.id
                        WHERE d.kind = 'personal'), 0)
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
                       SELECT SUM(d.used_bytes)::bigint
                         FROM storage.drives      d
                         JOIN storage.role_grants g
                           ON g.resource_type = 'drive'
                          AND g.resource_id   = d.id
                          AND g.role          = 'owner'
                          AND g.subject_type  = 'user'
                          AND g.subject_id    = u.id
                        WHERE d.kind = 'personal'), 0)
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

    /// Conditional user-side delta: only fires when the target folder's
    /// drive is `kind='personal'`. See `docs/plan/drive.md` §7.
    ///
    /// The new quota model: `auth.users.storage_quota_bytes` is the cap on
    /// the SUM of `used_bytes` across the user's personal drives. Shared
    /// drives never count against any user envelope. The upload hot path
    /// reads `drives.kind` from the same JOIN that already runs for the
    /// drive cap check; firing this conditional delta instead of the
    /// unconditional [`Self::add_user_storage_usage_delta`] keeps the
    /// counter aligned with that envelope semantics. Idempotent + clamped
    /// at zero, same as the unconditional sibling.
    ///
    /// Implementation note: the EXISTS subquery is two indexed PK probes
    /// (folder by id, drive by id) so the personal/shared discrimination
    /// adds no real cost vs. the unconditional update.
    pub async fn add_user_storage_usage_delta_if_personal(
        &self,
        user_id: Uuid,
        folder_id: Uuid,
        delta: i64,
    ) -> Result<(), DomainError> {
        sqlx::query(
            "UPDATE auth.users u
                SET storage_used_bytes = GREATEST(0, u.storage_used_bytes + $2)
              WHERE u.id = $1
                AND EXISTS (
                    SELECT 1
                      FROM storage.folders f
                      JOIN storage.drives  d ON d.id = f.drive_id
                     WHERE f.id = $3
                       AND d.kind = 'personal'
                )",
        )
        .bind(user_id)
        .bind(delta)
        .bind(folder_id)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| {
            DomainError::internal_error("StorageUsage", format!("usage delta if personal: {e}"))
        })?;
        Ok(())
    }

    /// Incrementally adjust one drive's cached `storage.drives.used_bytes`
    /// by `delta` bytes — same shape as
    /// [`Self::add_user_storage_usage_delta`]: single statement, no
    /// read-then-write window, `GREATEST(0, …)` clamp so a late or
    /// duplicate adjustment can never drive the counter negative.
    /// Deletes / trash do not decrement here; the periodic reconciliation
    /// sweep ([`Self::update_all_drives_storage_usage`]) remains the
    /// correctness backstop.
    pub async fn add_drive_storage_usage_delta(
        &self,
        drive_id: Uuid,
        delta: i64,
    ) -> Result<(), DomainError> {
        sqlx::query(
            "UPDATE storage.drives
                SET used_bytes = GREATEST(0, used_bytes + $2)
              WHERE id = $1",
        )
        .bind(drive_id)
        .bind(delta)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("StorageUsage", format!("drive delta: {e}")))?;
        // Deliberate no-invalidate here — see the class doc on
        // `invalidate_drive_lookup_caches`. Delta writes lag the
        // cache by up to the TTL; the sweep is the escape hatch.
        Ok(())
    }

    /// Return the size in bytes of a single non-trashed file. `None`
    /// if the file is trashed or absent. Used by cross-drive MOVE to
    /// know how many bytes will land on the destination drive so the
    /// pre-move `check_drive_quota` call can fire.
    pub async fn file_bytes(&self, file_id: Uuid) -> Result<Option<i64>, DomainError> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT size::bigint FROM storage.files WHERE id = $1 AND NOT is_trashed",
        )
        .bind(file_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("StorageUsage", format!("file_bytes: {e}")))?;
        Ok(row.map(|(s,)| s))
    }

    /// Sum the sizes of every non-trashed file whose parent folder is
    /// `folder_id` itself or a descendant of it via the `lpath` ltree.
    /// Used by cross-drive MOVE to know how many bytes would land on
    /// the destination drive — necessary for the pre-move
    /// `check_drive_quota` call.
    ///
    /// Returns 0 for an empty subtree AND for a non-existent
    /// `folder_id` (the JOIN silently drops); callers that need to
    /// distinguish those two cases must probe the folder separately.
    pub async fn folder_subtree_bytes(&self, folder_id: Uuid) -> Result<i64, DomainError> {
        let (bytes,): (Option<i64>,) = sqlx::query_as(
            "SELECT COALESCE(SUM(f.size), 0)::bigint
               FROM storage.files f
               JOIN storage.folders fo ON fo.id = f.folder_id
              WHERE fo.lpath <@ (SELECT lpath FROM storage.folders WHERE id = $1)
                AND NOT f.is_trashed",
        )
        .bind(folder_id)
        .fetch_one(self.pool.as_ref())
        .await
        .map_err(|e| {
            DomainError::internal_error("StorageUsage", format!("folder_subtree_bytes: {e}"))
        })?;
        Ok(bytes.unwrap_or(0))
    }

    /// Same as [`Self::add_drive_storage_usage_delta`] but resolves
    /// the drive id from a parent folder id in a single statement.
    /// Avoids a separate `SELECT drive_id FROM storage.folders` round
    /// trip at the upload hook site (where the folder id is what's
    /// naturally on the FileDto). The nested SELECT is point-lookup
    /// on the folder PK; clamp + idempotency properties are
    /// unchanged.
    pub async fn add_drive_storage_usage_delta_by_folder(
        &self,
        folder_id: Uuid,
        delta: i64,
    ) -> Result<(), DomainError> {
        // FROM-form UPDATE keeps the same join shape as
        // `check_drive_quota_by_folder` so both methods agree on
        // how a folder maps to its drive. A subquery form would
        // silently `UPDATE … WHERE id = NULL` (matching zero rows)
        // if the lookup misses; the FROM-form simply doesn't match
        // — same outcome, more conventional SQL.
        sqlx::query(
            "UPDATE storage.drives d
                SET used_bytes = GREATEST(0, d.used_bytes + $2)
               FROM storage.folders f
              WHERE f.drive_id = d.id
                AND f.id = $1",
        )
        .bind(folder_id)
        .bind(delta)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| {
            DomainError::internal_error("StorageUsage", format!("drive delta by folder: {e}"))
        })?;
        // See `add_drive_storage_usage_delta` — deliberate no-invalidate.
        Ok(())
    }

    /// Pre-upload quota check on a single drive.
    ///
    /// Read-only `SELECT (used_bytes, quota_bytes) FROM storage.drives`;
    /// returns `QuotaExceeded` when the projected `used_bytes +
    /// additional_bytes` would breach `quota_bytes`. A `NULL`
    /// `quota_bytes` short-circuits to `Ok(())` (unlimited drive —
    /// admin override / future system drives).
    ///
    /// Soft cap by design: the check/write window matches the
    /// user-quota path, bounded by the sweep interval. The clamp on
    /// `add_drive_storage_usage_delta` and the set-based reconciliation
    /// keep the counter honest; small over-quota slippage during the
    /// window is acceptable.
    pub async fn check_drive_quota(
        &self,
        drive_id: Uuid,
        additional_bytes: u64,
    ) -> Result<(), DomainError> {
        let row: Option<(i64, Option<i64>)> =
            sqlx::query_as("SELECT used_bytes, quota_bytes FROM storage.drives WHERE id = $1")
                .bind(drive_id)
                .fetch_optional(self.pool.as_ref())
                .await
                .map_err(|e| {
                    DomainError::internal_error("StorageUsage", format!("drive quota lookup: {e}"))
                })?;

        let Some((used, quota)) = row else {
            // Anti-enum at the upload edge would normally map to 404,
            // but at this layer we surface the typed not-found and let
            // the caller decide how to react. In practice the upload
            // path resolves the drive id from a folder/file lookup
            // first, so this branch fires only on a deleted-drive race.
            return Err(DomainError::not_found("Drive", drive_id.to_string()));
        };
        Self::eval_drive_cap(used, quota, additional_bytes)
    }

    /// Drive-cap verdict over already-fetched counters. Shared by
    /// [`Self::check_drive_quota`] and the fused
    /// [`Self::check_upload_quotas`] pair so both produce byte-identical
    /// errors.
    fn eval_drive_cap(
        used: i64,
        quota: Option<i64>,
        additional_bytes: u64,
    ) -> Result<(), DomainError> {
        let Some(quota) = quota else {
            return Ok(()); // unlimited
        };
        // Saturate on the i64 + u64 sum so a hostile / corrupt counter
        // can't silently overflow into a negative comparison.
        let projected = (used as i128) + (additional_bytes as i128);
        if projected > quota as i128 {
            return Err(DomainError::new(
                crate::common::errors::ErrorKind::QuotaExceeded,
                "Drive",
                format!(
                    "Drive quota exceeded: {} + {} > {} bytes",
                    used, additional_bytes, quota
                ),
            ));
        }
        Ok(())
    }

    /// User-envelope verdict over already-fetched counters. Shared by
    /// `check_storage_quota` and the fused [`Self::check_upload_quotas`]
    /// pair so both produce byte-identical errors.
    fn eval_user_envelope(used: i64, quota: i64, additional_bytes: u64) -> Result<(), DomainError> {
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

    /// Fused pre-upload gate: user envelope + drive cap in ONE round-trip.
    ///
    /// Upload entry points used to run `check_storage_quota` then
    /// `check_drive_quota` as two serial point reads — and the NC chunked
    /// PUT pays that pair on EVERY chunk. One `LEFT JOIN` row carries both
    /// counter pairs; verdict precedence (user envelope first, then drive
    /// existence, then drive cap) and every error shape are identical to
    /// the two-call sequence (benches/ROUND12.md §6, 1.81x).
    ///
    /// Row shape shared with [`Self::check_upload_quotas_by_folder`]:
    /// `(user_used, user_quota, drive_used, drive_quota, drive_found)`.
    pub async fn check_upload_quotas(
        &self,
        user_id: Uuid,
        drive_id: Uuid,
        additional_bytes: u64,
    ) -> Result<(), DomainError> {
        let row: Option<QuotaPairRow> = sqlx::query_as(
            r#"
            SELECT u.storage_used_bytes, u.storage_quota_bytes,
                   d.used_bytes, d.quota_bytes, (d.id IS NOT NULL)
            FROM auth.users u
            LEFT JOIN storage.drives d ON d.id = $2
            WHERE u.id = $1
            "#,
        )
        .bind(user_id)
        .bind(drive_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| {
            DomainError::internal_error("StorageUsage", format!("upload quota lookup: {e}"))
        })?;

        let Some((uused, uquota, dused, dquota, drive_found)) = row else {
            return Err(DomainError::not_found("User", user_id.to_string()));
        };
        Self::eval_user_envelope(uused, uquota, additional_bytes)?;
        if !drive_found {
            return Err(DomainError::not_found("Drive", drive_id.to_string()));
        }
        Self::eval_drive_cap(dused.unwrap_or(0), dquota, additional_bytes)
    }

    /// [`Self::check_upload_quotas`] with the drive resolved from a parent
    /// folder id — for the REST upload paths, which hold `folder_id`.
    /// A missing folder (or a folder whose drive vanished mid-race) maps to
    /// `not_found("Folder")`, exactly like `check_drive_quota_by_folder`.
    pub async fn check_upload_quotas_by_folder(
        &self,
        user_id: Uuid,
        folder_id: Uuid,
        additional_bytes: u64,
    ) -> Result<(), DomainError> {
        let row: Option<QuotaPairRow> = sqlx::query_as(
            r#"
            SELECT u.storage_used_bytes, u.storage_quota_bytes,
                   d.used_bytes, d.quota_bytes, (d.id IS NOT NULL)
            FROM auth.users u
            LEFT JOIN storage.folders f ON f.id = $2
            LEFT JOIN storage.drives d ON d.id = f.drive_id
            WHERE u.id = $1
            "#,
        )
        .bind(user_id)
        .bind(folder_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| {
            DomainError::internal_error("StorageUsage", format!("upload quota lookup: {e}"))
        })?;

        let Some((uused, uquota, dused, dquota, drive_found)) = row else {
            return Err(DomainError::not_found("User", user_id.to_string()));
        };
        Self::eval_user_envelope(uused, uquota, additional_bytes)?;
        if !drive_found {
            return Err(DomainError::not_found("Folder", folder_id.to_string()));
        }
        Self::eval_drive_cap(dused.unwrap_or(0), dquota, additional_bytes)
    }

    /// Same as [`Self::check_drive_quota`] but resolves the drive id
    /// from a parent folder id. Mirrors
    /// [`Self::add_drive_storage_usage_delta_by_folder`] so the upload
    /// handler (which holds `folder_id` from the multipart form) can
    /// gate the write in one round trip. Returns
    /// `DomainError::not_found("Folder", …)` if the folder id doesn't
    /// resolve — the upload pipeline would 404 on that anyway.
    pub async fn check_drive_quota_by_folder(
        &self,
        folder_id: Uuid,
        additional_bytes: u64,
    ) -> Result<(), DomainError> {
        let row: Option<(i64, Option<i64>)> = sqlx::query_as(
            "SELECT d.used_bytes, d.quota_bytes
               FROM storage.drives d
               JOIN storage.folders f ON f.drive_id = d.id
              WHERE f.id = $1",
        )
        .bind(folder_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| {
            DomainError::internal_error("StorageUsage", format!("drive quota by folder: {e}"))
        })?;

        let Some((used, quota)) = row else {
            return Err(DomainError::not_found("Folder", folder_id.to_string()));
        };
        let Some(quota) = quota else {
            return Ok(()); // unlimited
        };
        let projected = (used as i128) + (additional_bytes as i128);
        if projected > quota as i128 {
            return Err(DomainError::new(
                crate::common::errors::ErrorKind::QuotaExceeded,
                "Drive",
                format!(
                    "Drive quota exceeded: {} + {} > {} bytes",
                    used, additional_bytes, quota
                ),
            ));
        }
        Ok(())
    }

    /// Interval helper — same clamping (min 30 s) the retired
    /// `start_reconciliation_job` applied, exposed so DI can pass a
    /// sanitised `Duration` to `JobRegistry::register`.
    pub fn reconciliation_interval(interval_secs: u64) -> std::time::Duration {
        std::time::Duration::from_secs(interval_secs.max(30))
    }
}

pub const STORAGE_RECONCILE_JOB_NAME: &str = "storage_reconcile";

use crate::infrastructure::scheduler::{JobHandler, JobOutcome};
use async_trait::async_trait;

#[async_trait]
impl JobHandler for StorageUsageService {
    fn name(&self) -> &str {
        STORAGE_RECONCILE_JOB_NAME
    }

    /// Runs both reconciliation sweeps — drives first, then users —
    /// and reports the total number of rows corrected.
    ///
    /// **Sweep order matters.** The user envelope is computed from
    /// `SUM(drives.used_bytes)` over the caller's personal drives
    /// (`docs/plan/drive.md` §7). Running the user sweep before the
    /// drive sweep would read a stale per-drive counter and freeze
    /// the user counter on the previous tick's numbers — invisible in
    /// steady state, breaks any Hurl that trashes + sweeps within one
    /// call. See memory note `bug_storage_sweep_order_drive_first`.
    ///
    /// Failure of one sub-sweep short-circuits the tick to `Err`;
    /// operators see `outcome=err, cause=handler` in the scheduler
    /// log and the individual sweep's own `error!` line above it.
    async fn run(&self) -> JobOutcome {
        let drives = match self.update_all_drives_storage_usage().await {
            Ok(n) => n,
            Err(e) => return JobOutcome::Err(format!("drive reconciliation failed: {e}")),
        };
        let users = match self.update_all_users_storage_usage().await {
            Ok(n) => n,
            Err(e) => return JobOutcome::Err(format!("user reconciliation failed: {e}")),
        };
        JobOutcome::ok_with(
            drives + users,
            serde_json::json!({
                "drives_corrected": drives,
                "users_corrected": users,
            }),
        )
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
    async fn update_all_users_storage_usage(&self) -> Result<u64, DomainError> {
        debug!("Starting storage-usage reconciliation sweep");

        // User envelope = SUM of `drives.used_bytes` across the user's
        // personal drives. Shared drives don't count against any user
        // (`docs/plan/drive.md` §7). The drive-side sweep runs FIRST
        // (`start_reconciliation_job`) so `drives.used_bytes` is
        // already honest by the time we read it here.
        //
        // Ownership lookup uses `role_grants` (canonical per §1) so
        // both the user's default personal AND any secondary
        // personals owned via Owner grants are summed. Secondaries
        // aren't user-creatable today, but a backfill or admin path
        // can produce them — covering that surface from day one.
        let result = sqlx::query(
            r#"
            UPDATE auth.users u
               SET storage_used_bytes = COALESCE(t.total, 0)
              FROM auth.users u2
              LEFT JOIN (
                    SELECT g.subject_id      AS user_id,
                           SUM(d.used_bytes)::bigint AS total
                      FROM storage.drives      d
                      JOIN storage.role_grants g
                        ON g.resource_type = 'drive'
                       AND g.resource_id   = d.id
                       AND g.role          = 'owner'
                       AND g.subject_type  = 'user'
                     WHERE d.kind = 'personal'
                     GROUP BY g.subject_id
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

        let corrected = result.rows_affected();
        info!("Storage-usage reconciliation corrected {} user(s)", corrected);
        Ok(corrected)
    }

    async fn check_storage_quota(
        &self,
        user_id: Uuid,
        additional_bytes: u64,
    ) -> Result<(), DomainError> {
        // Narrow 2-column read — the full user row carries the up-to-512 KiB
        // avatar `image` column, paid on every upload quota check otherwise.
        let (used, quota) = self.user_repository.get_storage_usage(user_id).await?;
        Self::eval_user_envelope(used, quota, additional_bytes)
    }

    async fn get_user_storage_info(&self, user_id: Uuid) -> Result<(i64, i64), DomainError> {
        // Narrow 2-column read (avatar-free) — runs on every folder PROPFIND
        // that reports quota. See benches/QUOTA-PATH.md.
        Ok(self.user_repository.get_storage_usage(user_id).await?)
    }

    async fn add_drive_storage_usage_delta(
        &self,
        drive_id: Uuid,
        delta: i64,
    ) -> Result<(), DomainError> {
        StorageUsageService::add_drive_storage_usage_delta(self, drive_id, delta).await
    }

    /// Reconcile every drive's cached `used_bytes` in ONE set-based UPDATE.
    ///
    /// Same shape as the per-user sweep above: `LEFT JOIN` over the
    /// `storage.files` aggregate keyed on `drive_id`, `IS DISTINCT
    /// FROM` guard to skip no-op rewrites so idle drives don't churn
    /// dead tuples. Runs from the same reconciliation ticker as the
    /// user sweep; failure is logged but doesn't stop the next tick.
    async fn update_all_drives_storage_usage(&self) -> Result<u64, DomainError> {
        debug!("Starting drive storage-usage reconciliation sweep");
        let result = sqlx::query(
            r#"
            UPDATE storage.drives d
               SET used_bytes = COALESCE(t.total, 0)
              FROM storage.drives d2
              LEFT JOIN (
                    SELECT drive_id, SUM(size)::bigint AS total
                      FROM storage.files
                     WHERE NOT is_trashed
                     GROUP BY drive_id
                   ) t ON t.drive_id = d2.id
             WHERE d.id = d2.id
               AND d.used_bytes IS DISTINCT FROM COALESCE(t.total, 0)
            "#,
        )
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| {
            error!("Drive storage-usage reconciliation sweep failed: {}", e);
            DomainError::internal_error("StorageUsage", format!("drive reconciliation sweep: {e}"))
        })?;

        let corrected = result.rows_affected();
        info!(
            "Drive storage-usage reconciliation corrected {} drive(s)",
            corrected
        );
        // Unconditional invalidation — do NOT gate on
        // `rows_affected() > 0`. When a fire-and-forget delta has
        // already made SQL correct BEFORE the sweep runs, the sweep
        // touches zero rows but the cache may still hold the
        // pre-delta value from an earlier `GET /api/drives`. Gating
        // means the cache stays stale in exactly the case
        // `trigger-sweep` is called to fix. The invalidation cost is
        // small (moka `invalidate_all` on both caches); the
        // correctness guarantee matters. Regression avoidance:
        // drive_quota.hurl Step 6 exercises this race — 2nd upload's
        // delta lands during the 200 ms delay, sweep sees SQL is
        // already right → zero rows → without unconditional
        // invalidation, cache stays at the previous step's value.
        self.invalidate_drive_lookup_caches();
        Ok(corrected)
    }

    async fn check_drive_quota(
        &self,
        drive_id: Uuid,
        additional_bytes: u64,
    ) -> Result<(), DomainError> {
        StorageUsageService::check_drive_quota(self, drive_id, additional_bytes).await
    }
}

// Make StorageUsageService cloneable to support spawning concurrent tasks
impl Clone for StorageUsageService {
    fn clone(&self) -> Self {
        Self {
            pool: Arc::clone(&self.pool),
            user_repository: Arc::clone(&self.user_repository),
            drive_repo: self.drive_repo.clone(),
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
