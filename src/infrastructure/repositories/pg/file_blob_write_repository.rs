//! PostgreSQL + Blob-backed file write repository.
//!
//! Implements `FileWritePort` using:
//! - `storage.files` table for metadata
//! - `DedupPort` for content-addressable blob storage on the filesystem
//!
//! File paths are resolved by querying the materialized `storage.folders.path`
//! column (O(1) per lookup), so no recursive CTEs are needed.

use moka::sync::Cache;
use sqlx::PgPool;
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

use crate::application::dtos::display_helpers::category_order_for;
use crate::application::ports::storage_ports::{CopyFolderTreeResult, FileWritePort};
use crate::common::errors::DomainError;
use crate::domain::entities::file::File;
use crate::domain::services::path_service::StoragePath;

use super::folder_db_repository::FolderDbRepository;
use super::transaction_utils::retry_on_deadlock;
use crate::infrastructure::services::dedup_service::DedupService;

/// File write repository backed by PostgreSQL metadata + blob storage.
pub struct FileBlobWriteRepository {
    pool: Arc<PgPool>,
    dedup: Arc<DedupService>,
    folder_repo: Arc<FolderDbRepository>,
    /// Shared handle to `FileBlobReadRepository`'s file_id → blob_hash
    /// cache. Content swaps and hard deletes invalidate the mapping here
    /// so the read side can never serve a stale blob after a PUT update.
    hash_cache: Cache<String, String>,
}

impl FileBlobWriteRepository {
    pub fn new(
        pool: Arc<PgPool>,
        dedup: Arc<DedupService>,
        folder_repo: Arc<FolderDbRepository>,
        hash_cache: Cache<String, String>,
    ) -> Self {
        Self {
            pool,
            dedup,
            folder_repo,
            hash_cache,
        }
    }

    /// Creates a stub instance for testing — never hits PG.
    #[cfg(test)]
    pub fn new_stub() -> Self {
        use crate::infrastructure::services::dedup_service::DedupService;
        Self {
            pool: Arc::new(
                sqlx::pool::PoolOptions::<sqlx::Postgres>::new()
                    .max_connections(1)
                    .connect_lazy("postgres://invalid:5432/none")
                    .unwrap(),
            ),
            dedup: Arc::new(DedupService::new_stub()),
            folder_repo: Arc::new(super::folder_db_repository::FolderDbRepository::new_stub()),
            hash_cache: Cache::builder().max_capacity(10_000).build(),
        }
    }

    /// Build a `StoragePath` from the materialized folder path + file name.
    fn make_file_path(folder_path: Option<&str>, file_name: &str) -> StoragePath {
        match folder_path {
            Some(fp) if !fp.is_empty() => StoragePath::from_string(&format!("{fp}/{file_name}")),
            _ => StoragePath::from_string(file_name),
        }
    }

    /// Look up the materialized folder path. O(1) — no recursive CTE.
    async fn lookup_folder_path(
        &self,
        folder_id: Option<&str>,
    ) -> Result<Option<String>, DomainError> {
        match folder_id {
            Some(fid) => {
                let path: String =
                    sqlx::query_scalar("SELECT path FROM storage.folders WHERE id = $1::uuid")
                        .bind(fid)
                        .fetch_optional(self.pool.as_ref())
                        .await
                        .map_err(|e| {
                            DomainError::internal_error(
                                "FileBlobWrite",
                                format!("folder path: {e}"),
                            )
                        })?
                        .ok_or_else(|| DomainError::not_found("Folder", fid))?;
                Ok(Some(path))
            }
            None => Ok(None),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn row_to_file(
        id: String,
        name: String,
        folder_id: Option<String>,
        folder_path: Option<String>,
        size: i64,
        mime_type: String,
        created_at: i64,
        modified_at: i64,
        owner_id: Option<Uuid>,
        blob_hash: String,
    ) -> Result<File, DomainError> {
        let storage_path = Self::make_file_path(folder_path.as_deref(), &name);
        File::with_timestamps_and_blob_hash(
            id,
            name,
            storage_path,
            size as u64,
            mime_type,
            folder_id,
            created_at as u64,
            modified_at as u64,
            owner_id,
            blob_hash,
        )
        .map_err(|e| DomainError::internal_error("FileBlobWrite", format!("entity: {e}")))
    }

    /// Derive user_id from the parent folder, or error if folder_id is None.
    async fn resolve_user_id(&self, folder_id: Option<&str>) -> Result<Uuid, DomainError> {
        match folder_id {
            Some(fid) => self.folder_repo.get_folder_user_id(fid).await,
            None => Err(DomainError::internal_error(
                "FileBlobWrite",
                "folder_id is required to determine file owner",
            )),
        }
    }

    /// Atomically swap the blob hash of a file.
    ///
    /// Uses a CTE to capture the old hash before updating so the old blob
    /// reference can be decremented afterwards. Compensates on failure by
    /// removing the new blob reference.
    ///
    /// `modified_at`: if `Some`, sets `updated_at` to that Unix timestamp;
    /// if `None`, uses `NOW()` (server time). Returns
    /// `(new_hash, updated_at_epoch)` on success — the effective timestamp
    /// is returned so callers can rebuild the fresh entity without
    /// re-reading the row.
    async fn swap_blob_hash(
        &self,
        file_id: &str,
        new_hash: &str,
        new_size: i64,
        modified_at: Option<i64>,
    ) -> Result<(String, i64), DomainError> {
        // Atomic CTE: capture old hash then update in one round-trip, no TOCTOU.
        // Deadlock victims (40P01) retry before the compensation below runs —
        // a successful retry must keep the new blob reference alive.
        let (old_hash, updated_at) = match retry_on_deadlock("files.swap_blob_hash", || {
            sqlx::query_as::<_, (String, i64)>(
                r#"
                WITH old AS (
                    SELECT id, blob_hash FROM storage.files WHERE id = $3::uuid FOR UPDATE
                )
                UPDATE storage.files f
                   SET blob_hash = $1, size = $2,
                       updated_at = COALESCE(to_timestamp($4), NOW())
                  FROM old
                 WHERE f.id = old.id
                RETURNING old.blob_hash, EXTRACT(EPOCH FROM f.updated_at)::bigint
                "#,
            )
            .bind(new_hash)
            .bind(new_size)
            .bind(file_id)
            .bind(modified_at.map(|t| t as f64))
            .fetch_optional(self.pool.as_ref())
        })
        .await
        {
            Ok(Some(row)) => row,
            Ok(None) => {
                // File not found — compensate: remove the new blob ref
                if let Err(e) = self.dedup.remove_reference(new_hash).await {
                    tracing::error!("Blob orphaned after missing file: {}", e);
                }
                return Err(DomainError::not_found("File", file_id));
            }
            Err(e) => {
                // UPDATE failed — compensate: remove the new blob ref
                if let Err(rollback_err) = self.dedup.remove_reference(new_hash).await {
                    tracing::error!(
                        "Blob orphaned after failed UPDATE — hash: {}, err: {}",
                        &new_hash[..12],
                        rollback_err
                    );
                }
                return Err(DomainError::internal_error(
                    "FileBlobWrite",
                    format!("update: {e}"),
                ));
            }
        };

        // Decrement old blob ref (only if hash changed, best-effort)
        if old_hash != new_hash
            && let Err(e) = self.dedup.remove_reference(&old_hash).await
        {
            tracing::warn!(
                "Failed to decrement old blob ref {}: {}",
                &old_hash[..12],
                e
            );
        }

        Ok((new_hash.to_string(), updated_at))
    }

    /// Register a file row pointing at a blob already stored in the chunk
    /// store (the upload-ingest layer streamed the content in). Consumes the
    /// caller's blob reference: any failure releases it before returning.
    async fn save_file_with_blob_impl(
        &self,
        name: String,
        folder_id: Option<String>,
        content_type: String,
        blob_hash: &str,
        size: u64,
    ) -> Result<File, DomainError> {
        // Root files have no parent folder to derive an owner from — keep the
        // previous resolve_user_id(None) contract (release the ref, error out).
        let Some(fid) = folder_id.as_deref() else {
            if let Err(rollback_err) = self.dedup.remove_reference(blob_hash).await {
                tracing::error!(
                    "Blob orphaned after missing folder_id — hash: {}, err: {}",
                    &blob_hash[..12],
                    rollback_err
                );
            }
            return Err(DomainError::internal_error(
                "FileBlobWrite",
                "folder_id is required to determine file owner",
            ));
        };

        // ONE round-trip: derive owner + materialized path from the parent
        // folder and insert in a single statement. (Was resolve_user_id +
        // INSERT + lookup_folder_path = 3 trips, two of them re-reading the
        // same folders row.) An empty `parent` CTE — the folder vanished
        // between ingest and insert — inserts zero rows, which surfaces as a
        // clean NotFound instead of a generic owner-resolution error.
        //
        // Deadlock victims (40P01) retry before the compensation below runs —
        // a successful retry must keep the blob reference alive. The final
        // attempt's error falls through untouched so the 23505 mapping holds
        // (a retried INSERT can legitimately lose to a concurrent identical
        // upload).
        let result = retry_on_deadlock("files.insert", || {
            sqlx::query_as::<_, (String, Uuid, String, i64, i64)>(
                r#"
                WITH parent AS (
                    SELECT id, user_id, path FROM storage.folders WHERE id = $2::uuid
                )
                INSERT INTO storage.files
                    (name, folder_id, user_id, blob_hash, size, mime_type, category_order)
                SELECT $1, parent.id, parent.user_id, $3, $4, $5, $6 FROM parent
                RETURNING id::text,
                          user_id,
                          (SELECT path FROM parent),
                          EXTRACT(EPOCH FROM created_at)::bigint,
                          EXTRACT(EPOCH FROM updated_at)::bigint
                "#,
            )
            .bind(&name)
            .bind(fid)
            .bind(blob_hash)
            .bind(size as i64)
            .bind(&content_type)
            .bind(category_order_for(&name, &content_type))
            .fetch_optional(self.pool.as_ref())
        })
        .await;

        let (id, user_id, folder_path, created_at, updated_at) = match result {
            Ok(Some(row)) => row,
            Ok(None) => {
                if let Err(rollback_err) = self.dedup.remove_reference(blob_hash).await {
                    tracing::error!(
                        "Blob orphaned after missing parent folder — hash: {}, err: {}",
                        &blob_hash[..12],
                        rollback_err
                    );
                }
                return Err(DomainError::not_found("Folder", fid));
            }
            Err(e) => {
                if let Err(rollback_err) = self.dedup.remove_reference(blob_hash).await {
                    tracing::error!(
                        "Blob orphaned after failed INSERT — hash: {}, err: {}",
                        &blob_hash[..12],
                        rollback_err
                    );
                }
                if let sqlx::Error::Database(ref db_err) = e
                    && db_err.code().as_deref() == Some("23505")
                {
                    return Err(DomainError::already_exists(
                        "File",
                        format!("'{name}' already exists in this folder"),
                    ));
                }
                return Err(DomainError::internal_error(
                    "FileBlobWrite",
                    format!("insert: {e}"),
                ));
            }
        };

        tracing::info!(
            "📡 STREAMING WRITE: {} ({} bytes, hash: {})",
            name,
            size,
            &blob_hash[..12]
        );

        Self::row_to_file(
            id,
            name,
            folder_id,
            Some(folder_path),
            size as i64,
            content_type,
            created_at,
            updated_at,
            Some(user_id),
            blob_hash.to_string(),
        )
    }
}

impl FileWritePort for FileBlobWriteRepository {
    async fn save_file_with_blob(
        &self,
        name: String,
        folder_id: Option<String>,
        content_type: String,
        blob_hash: &str,
        size: u64,
    ) -> Result<File, DomainError> {
        self.save_file_with_blob_impl(name, folder_id, content_type, blob_hash, size)
            .await
    }

    async fn move_file(
        &self,
        file_id: &str,
        target_folder_id: Option<String>,
    ) -> Result<File, DomainError> {
        // If moving to a different folder, get the new user_id (must be same user)
        let row = sqlx::query_as::<_, (String, String, Option<String>, i64, String, i64, i64)>(
            r#"
            UPDATE storage.files
               SET folder_id = $1::uuid, updated_at = NOW()
             WHERE id = $2::uuid AND NOT is_trashed
            RETURNING id::text, name, folder_id::text, size, mime_type,
                      EXTRACT(EPOCH FROM created_at)::bigint,
                      EXTRACT(EPOCH FROM updated_at)::bigint
            "#,
        )
        .bind(&target_folder_id)
        .bind(file_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("FileBlobWrite", format!("move: {e}")))?
        .ok_or_else(|| DomainError::not_found("File", file_id))?;

        let folder_path = self.lookup_folder_path(row.2.as_deref()).await?;
        Self::row_to_file(
            row.0,
            row.1,
            row.2,
            folder_path,
            row.3,
            row.4,
            row.5,
            row.6,
            None,
            String::new(),
        )
    }

    async fn copy_file(
        &self,
        file_id: &str,
        target_folder_id: Option<String>,
    ) -> Result<File, DomainError> {
        // Atomic CTE: read source file → insert new row with same blob_hash → increment ref_count.
        // Single round-trip; blob content is NOT copied (dedup makes this zero-copy).
        let target_fid = target_folder_id.clone();

        let row = retry_on_deadlock("files.copy", || {
            sqlx::query_as::<
                _,
                (
                    String,
                    String,
                    Option<String>,
                    i64,
                    String,
                    i64,
                    i64,
                    String,
                ),
            >(
                r#"
                WITH src AS (
                    SELECT name, folder_id, user_id, blob_hash, size, mime_type, category_order
                      FROM storage.files
                     WHERE id = $1::uuid AND NOT is_trashed
                ),
                new_file AS (
                    INSERT INTO storage.files (name, folder_id, user_id, blob_hash, size, mime_type, category_order)
                    SELECT name,
                           COALESCE($2::uuid, folder_id),
                           user_id,
                           blob_hash,
                           size,
                           mime_type,
                           category_order
                      FROM src
                    RETURNING id::text, name, folder_id::text, size, mime_type,
                              EXTRACT(EPOCH FROM created_at)::bigint,
                              EXTRACT(EPOCH FROM updated_at)::bigint,
                              blob_hash
                )
                SELECT * FROM new_file
                "#,
            )
            .bind(file_id)
            .bind(&target_fid)
            .fetch_optional(self.pool.as_ref())
        })
        .await
        .map_err(|e| {
            if let sqlx::Error::Database(ref db_err) = e
                && db_err.code().as_deref() == Some("23505")
            {
                return DomainError::already_exists(
                    "File",
                    "a file with this name already exists in the target folder",
                );
            }
            DomainError::internal_error("FileBlobWrite", format!("copy: {e}"))
        })?
        .ok_or_else(|| DomainError::not_found("File", file_id))?;

        let blob_hash = &row.7;

        // Increment blob reference count (best-effort; INSERT already succeeded)
        if let Err(e) = self.dedup.add_reference(blob_hash).await {
            tracing::warn!(
                "Failed to increment blob ref for copy {}: {}",
                &blob_hash[..12],
                e
            );
        }

        tracing::info!(
            "📋 BLOB COPY: {} (hash: {}, zero-copy via dedup)",
            row.1,
            &blob_hash[..12]
        );

        let folder_path = self.lookup_folder_path(row.2.as_deref()).await?;
        Self::row_to_file(
            row.0,
            row.1,
            row.2,
            folder_path,
            row.3,
            row.4,
            row.5,
            row.6,
            None,
            row.7,
        )
    }

    async fn rename_file(&self, file_id: &str, new_name: &str) -> Result<File, DomainError> {
        let row = sqlx::query_as::<_, (String, String, Option<String>, i64, String, i64, i64)>(
            r#"
            UPDATE storage.files
               SET name = $1, updated_at = NOW()
             WHERE id = $2::uuid AND NOT is_trashed
            RETURNING id::text, name, folder_id::text, size, mime_type,
                      EXTRACT(EPOCH FROM created_at)::bigint,
                      EXTRACT(EPOCH FROM updated_at)::bigint
            "#,
        )
        .bind(new_name)
        .bind(file_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| {
            if let sqlx::Error::Database(ref db_err) = e
                && db_err.code().as_deref() == Some("23505")
            {
                return DomainError::already_exists("File", format!("'{new_name}' already exists"));
            }
            DomainError::internal_error("FileBlobWrite", format!("rename: {e}"))
        })?
        .ok_or_else(|| DomainError::not_found("File", file_id))?;

        let folder_path = self.lookup_folder_path(row.2.as_deref()).await?;
        Self::row_to_file(
            row.0,
            row.1,
            row.2,
            folder_path,
            row.3,
            row.4,
            row.5,
            row.6,
            None,
            String::new(),
        )
    }

    async fn delete_file(&self, id: &str) -> Result<(), DomainError> {
        // The PG trigger `trg_files_decrement_blob_ref` automatically
        // decrements storage.blobs.ref_count for the deleted row's blob_hash.
        // Disk cleanup of orphaned blobs (ref_count = 0) is handled by
        // garbage_collect().
        let result = sqlx::query("DELETE FROM storage.files WHERE id = $1::uuid")
            .bind(id)
            .execute(self.pool.as_ref())
            .await
            .map_err(|e| DomainError::internal_error("FileBlobWrite", format!("delete: {e}")))?;

        if result.rows_affected() == 0 {
            return Err(DomainError::not_found("File", id));
        }

        // Drop the read-side file_id → blob_hash mapping for the dead row.
        self.hash_cache.invalidate(id);
        Ok(())
    }

    async fn update_file_content_with_blob(
        &self,
        file_id: &str,
        blob_hash: &str,
        size: u64,
        modified_at: Option<i64>,
    ) -> Result<(String, i64), DomainError> {
        // The content was already ingested into the chunk store by the
        // upload-ingest layer; swap_blob_hash consumes its reference and
        // releases it on failure.
        let swapped = self
            .swap_blob_hash(file_id, blob_hash, size as i64, modified_at)
            .await?;
        // The file now maps to a different blob — drop the read-side cache
        // entry so streaming downloads cannot serve the previous content
        // for the rest of its TTI window.
        self.hash_cache.invalidate(file_id);
        Ok(swapped)
    }

    async fn register_file_deferred(
        &self,
        name: String,
        folder_id: Option<String>,
        content_type: String,
        size: u64,
    ) -> Result<(File, PathBuf), DomainError> {
        let user_id = self.resolve_user_id(folder_id.as_deref()).await?;

        // For deferred registration we use a placeholder hash.
        // The write-behind cache will call update_file_content later.
        let placeholder_hash = "0000000000000000000000000000000000000000000000000000000000000000";

        let row = retry_on_deadlock("files.insert_deferred", || {
            sqlx::query_as::<_, (String, i64, i64)>(
                r#"
                INSERT INTO storage.files (name, folder_id, user_id, blob_hash, size, mime_type, category_order)
                VALUES ($1, $2::uuid, $3, $4, $5, $6, $7)
                RETURNING id::text,
                          EXTRACT(EPOCH FROM created_at)::bigint,
                          EXTRACT(EPOCH FROM updated_at)::bigint
                "#,
            )
            .bind(&name)
            .bind(&folder_id)
            .bind(user_id)
            .bind(placeholder_hash)
            .bind(size as i64)
            .bind(&content_type)
            .bind(category_order_for(&name, &content_type))
            .fetch_one(self.pool.as_ref())
        })
        .await
        .map_err(|e| DomainError::internal_error("FileBlobWrite", format!("deferred: {e}")))?;

        let folder_path = self.lookup_folder_path(folder_id.as_deref()).await?;
        let file = Self::row_to_file(
            row.0.clone(),
            name,
            folder_id,
            folder_path,
            size as i64,
            content_type,
            row.1,
            row.2,
            Some(user_id),
            String::new(),
        )?;

        // The target_path is not meaningful for blob storage (content goes to .blobs/)
        // but the WriteBehindCache API requires it. We return a synthetic path.
        let target_path = PathBuf::from(format!(".pending/{}", row.0));

        Ok((file, target_path))
    }

    // ── Trash operations ──

    async fn move_to_trash(&self, file_id: &str) -> Result<(), DomainError> {
        let result = sqlx::query(
            r#"
            UPDATE storage.files
               SET is_trashed = TRUE,
                   trashed_at = NOW(),
                   original_folder_id = folder_id,
                   updated_at = NOW()
             WHERE id = $1::uuid AND NOT is_trashed
            "#,
        )
        .bind(file_id)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("FileBlobWrite", format!("trash: {e}")))?;

        if result.rows_affected() == 0 {
            return Err(DomainError::not_found("File", file_id));
        }
        Ok(())
    }

    async fn restore_from_trash(
        &self,
        file_id: &str,
        _original_path: &str,
    ) -> Result<(), DomainError> {
        let result = sqlx::query(
            r#"
            UPDATE storage.files
               SET is_trashed = FALSE,
                   trashed_at = NULL,
                   folder_id = COALESCE(original_folder_id, folder_id),
                   original_folder_id = NULL,
                   updated_at = NOW()
             WHERE id = $1::uuid AND is_trashed
            "#,
        )
        .bind(file_id)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("FileBlobWrite", format!("restore: {e}")))?;

        if result.rows_affected() == 0 {
            return Err(DomainError::not_found("File", file_id));
        }
        Ok(())
    }

    async fn delete_file_permanently(&self, file_id: &str) -> Result<(), DomainError> {
        // Read blob_hash before deletion so we can clean up disk after the
        // PG trigger has decremented the ref_count.
        let blob_hash: Option<String> =
            sqlx::query_scalar("SELECT blob_hash FROM storage.files WHERE id = $1::uuid")
                .bind(file_id)
                .fetch_optional(self.pool.as_ref())
                .await
                .map_err(|e| {
                    DomainError::internal_error("FileBlobWrite", format!("fetch blob_hash: {e}"))
                })?;

        // DELETE fires trg_files_decrement_blob_ref → storage.blobs.ref_count--
        self.delete_file(file_id).await?;

        // If the blob is now unreferenced, remove disk file + thumbnails.
        if let Some(hash) = blob_hash {
            self.dedup.cleanup_if_orphaned(&hash).await;
        }

        Ok(())
    }

    async fn copy_folder_tree(
        &self,
        source_folder_id: &str,
        target_parent_id: Option<String>,
        dest_name: Option<String>,
    ) -> Result<CopyFolderTreeResult, DomainError> {
        let row = sqlx::query_as::<_, (String, i64, i64)>(
            "SELECT new_root_id, folders_copied, files_copied \
               FROM storage.copy_folder_tree($1::uuid, $2::uuid, $3)",
        )
        .bind(source_folder_id)
        .bind(&target_parent_id)
        .bind(&dest_name)
        .fetch_one(self.pool.as_ref())
        .await
        .map_err(|e| {
            // Map PG P0002 (no_data_found) to NotFound
            if let sqlx::Error::Database(ref db_err) = e {
                if db_err.code().as_deref() == Some("P0002") {
                    return DomainError::not_found("Folder", source_folder_id);
                }
                if db_err.code().as_deref() == Some("23505") {
                    return DomainError::already_exists(
                        "Folder",
                        "a folder with this name already exists in the target location",
                    );
                }
            }
            DomainError::internal_error("FileBlobWrite", format!("copy_folder_tree: {e}"))
        })?;

        tracing::info!(
            "📂 TREE COPY: {} folders + {} files (root: {}, zero-copy via dedup)",
            row.1,
            row.2,
            &row.0[..8]
        );

        Ok(CopyFolderTreeResult {
            new_root_folder_id: row.0,
            folders_copied: row.1,
            files_copied: row.2,
        })
    }
}
