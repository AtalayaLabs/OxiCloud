//! PostgreSQL + Blob-backed file write repository.
//!
//! Implements `FileWritePort` using:
//! - `storage.files` table for metadata
//! - `DedupPort` for content-addressable blob storage on the filesystem
//!
//! File paths are resolved by querying the materialized `storage.folders.path`
//! column (O(1) per lookup), so no recursive CTEs are needed.

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
use crate::infrastructure::services::dedup_service::DedupService;

/// File write repository backed by PostgreSQL metadata + blob storage.
pub struct FileBlobWriteRepository {
    pool: Arc<PgPool>,
    dedup: Arc<DedupService>,
    folder_repo: Arc<FolderDbRepository>,
}

impl FileBlobWriteRepository {
    pub fn new(
        pool: Arc<PgPool>,
        dedup: Arc<DedupService>,
        folder_repo: Arc<FolderDbRepository>,
    ) -> Self {
        Self {
            pool,
            dedup,
            folder_repo,
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
        let (old_hash, updated_at) = match sqlx::query_as::<_, (String, i64)>(
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

    /// Like [`FileWritePort::save_file_from_temp`] but also returns whether the
    /// blob was genuinely new (`true`) or a dedup hit (`false`).
    /// Used by [`FileUploadService`] to pass `is_new_blob` to lifecycle hooks.
    pub async fn save_file_from_temp_with_dedup(
        &self,
        name: String,
        folder_id: Option<String>,
        content_type: String,
        temp_path: &std::path::Path,
        size: u64,
        pre_computed_hash: Option<String>,
    ) -> Result<(File, bool), DomainError> {
        let user_id = self.resolve_user_id(folder_id.as_deref()).await?;

        let dedup_result = self
            .dedup
            .store_from_file(temp_path, Some(content_type.clone()), pre_computed_hash)
            .await?;
        let is_new_blob = !dedup_result.was_deduplicated();
        let blob_hash = dedup_result.hash().to_string();

        let row = match sqlx::query_as::<_, (String, i64, i64)>(
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
        .bind(&blob_hash)
        .bind(size as i64)
        .bind(&content_type)
        .bind(category_order_for(&name, &content_type))
        .fetch_one(self.pool.as_ref())
        .await
        {
            Ok(row) => row,
            Err(e) => {
                if let Err(rollback_err) = self.dedup.remove_reference(&blob_hash).await {
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

        let folder_path = self.lookup_folder_path(folder_id.as_deref()).await?;
        let file = Self::row_to_file(
            row.0,
            name,
            folder_id,
            folder_path,
            size as i64,
            content_type,
            row.1,
            row.2,
            Some(user_id),
            blob_hash,
        )?;
        Ok((file, is_new_blob))
    }
}

impl FileWritePort for FileBlobWriteRepository {
    async fn save_file_from_temp(
        &self,
        name: String,
        folder_id: Option<String>,
        content_type: String,
        temp_path: &std::path::Path,
        size: u64,
        pre_computed_hash: Option<String>,
    ) -> Result<File, DomainError> {
        self.save_file_from_temp_with_dedup(
            name,
            folder_id,
            content_type,
            temp_path,
            size,
            pre_computed_hash,
        )
        .await
        .map(|(file, _)| file)
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

        let row = sqlx::query_as::<
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

        Ok(())
    }

    async fn update_file_content_from_temp(
        &self,
        file_id: &str,
        temp_path: &std::path::Path,
        size: u64,
        content_type: Option<String>,
        pre_computed_hash: Option<String>,
        modified_at: Option<i64>,
    ) -> Result<(String, i64), DomainError> {
        // Streaming: pass pre-computed hash so dedup skips re-reading the file.
        let dedup_result = self
            .dedup
            .store_from_file(temp_path, content_type, pre_computed_hash)
            .await?;
        let new_hash = dedup_result.hash().to_string();

        self.swap_blob_hash(file_id, &new_hash, size as i64, modified_at)
            .await
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

        let row = sqlx::query_as::<_, (String, i64, i64)>(
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
