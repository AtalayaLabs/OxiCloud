use bytes::Bytes;
use futures::Stream;
use serde_json::Value;
use std::path::PathBuf;
use std::pin::Pin;
use uuid::Uuid;

use crate::application::dtos::search_dto::SearchCriteriaDto;
use crate::common::errors::DomainError;
use crate::domain::entities::file::File;
use crate::domain::services::path_service::StoragePath;

// Re-export domain repository traits for backward compatibility.
// The canonical definitions now live in domain/repositories/.
pub use crate::domain::repositories::file_repository::{
    FileReadRepository, FileRepository, FileWriteRepository,
};
pub use crate::domain::repositories::folder_repository::FolderRepository;

// ─────────────────────────────────────────────────────
// FileReadPort — application-layer alias for FileReadRepository
// ─────────────────────────────────────────────────────

/// Secondary port for file **reading**.
///
/// Encapsulates every operation that queries state without modifying it:
/// get, list, content, stream, mmap, range, path resolution.
pub trait FileReadPort: Send + Sync + 'static {
    /// Gets a file by its ID.
    async fn get_file(&self, id: &str) -> Result<File, DomainError>;

    async fn get_file_or_trashed(&self, id: &str) -> Result<File, DomainError>;

    /// Gets a file by its ID, scoped to a specific owner.
    ///
    /// Returns `NotFound` if the file does not exist **or** belongs to a
    /// different user.  This is the primary IDOR-safe accessor — handlers
    /// serving end-user requests should always prefer this over `get_file`.
    async fn get_file_for_owner(&self, id: &str, owner_id: Uuid) -> Result<File, DomainError>;

    /// Verifies that the file identified by `id` belongs to `owner_id`.
    ///
    /// Returns `Ok(())` on success or `NotFound` when the file does not
    /// exist or belongs to another user.
    async fn verify_file_owner(&self, id: &str, owner_id: Uuid) -> Result<(), DomainError> {
        self.get_file_for_owner(id, owner_id).await.map(|_| ())
    }

    /// Lists files in a folder.
    async fn list_files(&self, folder_id: Option<&str>) -> Result<Vec<File>, DomainError>;

    /// Lists files in a folder scoped to a specific owner (SQL-level).
    ///
    /// Default falls back to `list_files` + in-memory filter.
    /// Repositories should override with a direct `AND user_id = $N` query.
    async fn list_files_for_owner(
        &self,
        folder_id: Option<&str>,
        owner_id: Uuid,
    ) -> Result<Vec<File>, DomainError> {
        let all = self.list_files(folder_id).await?;
        Ok(all
            .into_iter()
            .filter(|f| f.owner_id() == Some(owner_id))
            .collect())
    }

    /// Gets content as a stream (ideal for large files).
    async fn get_file_stream(
        &self,
        id: &str,
    ) -> Result<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>, DomainError>;

    /// Stream of a byte range (HTTP Range Requests, video seek).
    async fn get_file_range_stream(
        &self,
        id: &str,
        start: u64,
        end: Option<u64>,
    ) -> Result<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>, DomainError>;

    /// Gets the logical storage path of a file.
    async fn get_file_path(&self, id: &str) -> Result<StoragePath, DomainError>;

    /// Gets the parent folder ID from a path (WebDAV).
    async fn get_parent_folder_id(&self, path: &str) -> Result<String, DomainError>;

    /// Gets a folder ID by its path.
    async fn get_folder_id_by_path(&self, folder_path: &str) -> Result<String, DomainError>;

    /// Gets the content-addressable blob hash for a file (O(1) DB lookup).
    ///
    /// Returns the BLAKE3 hash stored in `storage.files.blob_hash`.
    /// Used for dedup reference tracking without loading file content.
    async fn get_blob_hash(&self, file_id: &str) -> Result<String, DomainError>;

    /// Find a file by its logical path (folder_name/.../file_name).
    ///
    /// The default implementation falls back to `list_files(None)` + linear
    /// scan (O(N)). Repositories should override with a direct SQL query.
    async fn find_file_by_path(&self, path: &str) -> Result<Option<File>, DomainError> {
        let path = path.trim_start_matches('/').trim_end_matches('/');
        let all_files = self.list_files(None).await?;
        for file in all_files {
            let file_path = file.path_string();
            let file_path = file_path.trim_start_matches('/').trim_end_matches('/');
            if file_path == path
                || file_path.ends_with(&format!("/{}", path))
                || path.ends_with(&format!("/{}", file_path))
            {
                return Ok(Some(file));
            }
        }
        Ok(None)
    }

    /// Lists files in a folder with LIMIT/OFFSET pagination.
    ///
    /// Used by streaming WebDAV PROPFIND to avoid loading all files at once.
    /// Default: falls back to `list_files` (loads all, then slices in memory).
    async fn list_files_batch(
        &self,
        folder_id: Option<&str>,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<File>, DomainError> {
        let all = self.list_files(folder_id).await?;
        let start = (offset as usize).min(all.len());
        let end = (start + limit as usize).min(all.len());
        Ok(all.into_iter().skip(start).take(end - start).collect())
    }

    /// Like [`list_files_batch`], but only returns files owned by `owner_id`.
    ///
    /// Used by streaming WebDAV PROPFIND to list files scoped to the
    /// authenticated user, preventing cross-user data leakage.
    async fn list_files_batch_for_owner(
        &self,
        folder_id: Option<&str>,
        owner_id: Uuid,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<File>, DomainError> {
        // Default: filter in-memory (repos should override with SQL)
        let all = self.list_files_batch(folder_id, offset, limit).await?;
        Ok(all
            .into_iter()
            .filter(|f| f.owner_id() == Some(owner_id))
            .collect())
    }

    /// Streams every file in the subtree rooted at `folder_id`.
    ///
    /// Uses an ltree `<@` join against `storage.folders` so the entire
    /// subtree is resolved in a single GiST-indexed query, but rows are
    /// delivered via a PostgreSQL cursor — RAM stays O(1) per row.
    ///
    /// Callers consume the stream incrementally (e.g. build a HashMap
    /// keyed by folder_id) without ever materializing the full Vec.
    async fn stream_files_in_subtree(
        &self,
        folder_id: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<File, DomainError>> + Send>>, DomainError>;

    /// Search files with pagination and filtering at database level.
    ///
    /// This is more efficient than loading all files and filtering in memory,
    /// especially for large datasets. The filtering is pushed to the SQL layer.
    ///
    /// # Arguments
    /// * `folder_id` - Optional folder ID to scope the search (for recursive search, pass None)
    /// * `criteria` - Search criteria including name_contains, file_types, date ranges, size ranges
    /// * `user_id` - User ID for ownership filtering
    ///
    /// # Returns
    /// A tuple of (files, total_count) where files are paginated and filtered
    async fn search_files_paginated(
        &self,
        folder_id: Option<&str>,
        criteria: &SearchCriteriaDto,
        user_id: Uuid,
    ) -> Result<(Vec<File>, usize), DomainError>;

    /// Search files recursively in a folder subtree using ltree.
    ///
    /// When `root_folder_id` is Some, uses ltree descendant queries to find
    /// all files within the subtree rooted at that folder. When None, searches
    /// all files for the user. This replaces the O(N) recursive spawn-per-folder
    /// approach with O(1) SQL queries.
    ///
    /// Returns a tuple of (matching files, total count for pagination).
    async fn search_files_in_subtree(
        &self,
        root_folder_id: Option<&str>,
        criteria: &SearchCriteriaDto,
        user_id: Uuid,
    ) -> Result<(Vec<File>, usize), DomainError> {
        // Default: delegate to paginated search (non-recursive fallback)
        self.search_files_paginated(root_folder_id, criteria, user_id)
            .await
    }

    /// Count files matching the search criteria (without loading them).
    ///
    /// Used for pagination metadata without fetching the actual files.
    async fn count_files(
        &self,
        folder_id: Option<&str>,
        criteria: &SearchCriteriaDto,
        user_id: Uuid,
    ) -> Result<usize, DomainError>;

    /// Return up to `limit` files whose name contains `query` (case-insensitive).
    ///
    /// Results are ordered by relevance (exact > starts-with > contains) so the
    /// caller can use them directly for autocomplete suggestions.
    ///
    /// The default implementation falls back to `list_files` + in-memory filter
    /// so that stubs and mocks compile without changes.
    async fn suggest_files_by_name(
        &self,
        folder_id: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<File>, DomainError> {
        let all = self.list_files(folder_id).await?;
        let q = query.to_lowercase();
        let mut matched: Vec<File> = all
            .into_iter()
            .filter(|f| f.name().to_lowercase().contains(&q))
            .collect();
        matched.truncate(limit);
        Ok(matched)
    }
}

// ─────────────────────────────────────────────────────
// FileWritePort — all write / mutate operations
// ─────────────────────────────────────────────────────

/// Result of an atomic recursive folder tree copy.
#[derive(Debug, Clone)]
pub struct CopyFolderTreeResult {
    /// UUID of the newly created root folder
    pub new_root_folder_id: String,
    /// Total folders created (including root)
    pub folders_copied: i64,
    /// Total files copied (zero-copy via dedup)
    pub files_copied: i64,
}

/// Secondary port for file **writing**.
///
/// Covers: upload (buffered + streaming), move, delete, update,
/// and deferred registration for the write-behind cache.
pub trait FileWritePort: Send + Sync + 'static {
    /// Streaming upload — saves a file from a temp file already on disk.
    ///
    /// When `pre_computed_hash` is provided, the dedup service skips the
    /// hash re-read — zero extra I/O beyond the initial spool.
    async fn save_file_from_temp(
        &self,
        name: String,
        folder_id: Option<String>,
        content_type: String,
        temp_path: &std::path::Path,
        size: u64,
        pre_computed_hash: Option<String>,
    ) -> Result<File, DomainError>;

    /// Moves a file to another folder.
    async fn move_file(
        &self,
        file_id: &str,
        target_folder_id: Option<String>,
    ) -> Result<File, DomainError>;

    /// Renames a file (same folder, different name).
    async fn rename_file(&self, file_id: &str, new_name: &str) -> Result<File, DomainError>;

    /// Deletes a file.
    async fn delete_file(&self, id: &str) -> Result<(), DomainError>;

    /// Streaming update — replaces file content from a temp file on disk.
    ///
    /// When `pre_computed_hash` is provided, the dedup service skips the
    /// hash re-read — zero extra I/O beyond the initial spool.
    /// Peak RAM: ~256 KB regardless of file size.
    ///
    /// Returns `(new_blob_hash, updated_at_epoch)` — everything a caller
    /// needs to rebuild the fresh entity/ETag from a `File` it already
    /// holds, without re-reading the row it just updated.
    async fn update_file_content_from_temp(
        &self,
        file_id: &str,
        temp_path: &std::path::Path,
        size: u64,
        content_type: Option<String>,
        pre_computed_hash: Option<String>,
        modified_at: Option<i64>,
    ) -> Result<(String, i64), DomainError>;

    /// Registers file metadata WITHOUT writing content to disk (write-behind).
    ///
    /// Returns `(File, PathBuf)` where `PathBuf` is the destination path for the
    /// deferred write that the `WriteBehindCache` will perform.
    async fn register_file_deferred(
        &self,
        name: String,
        folder_id: Option<String>,
        content_type: String,
        size: u64,
    ) -> Result<(File, PathBuf), DomainError>;

    /// Copies a file to a (possibly different) folder.
    ///
    /// With blob-dedup, this only creates a new metadata row and increments
    /// the blob reference count — zero disk I/O for the content.
    async fn copy_file(
        &self,
        file_id: &str,
        target_folder_id: Option<String>,
    ) -> Result<File, DomainError>;

    /// Copies an entire folder subtree atomically using ltree.
    ///
    /// Creates a copy of `source_folder_id` (with optional `dest_name`)
    /// under `target_parent_id`, including ALL sub-folders and files.
    /// Files are zero-copy (blob ref_counts are incremented in batch).
    ///
    /// Uses a PL/pgSQL function: O(depth) folder INSERTs + 1 file batch
    /// + 1 ref_count batch.  Replaces the N+1 sequential copy pattern.
    ///
    /// Default: returns error (only PostgreSQL backend implements this).
    async fn copy_folder_tree(
        &self,
        _source_folder_id: &str,
        _target_parent_id: Option<String>,
        _dest_name: Option<String>,
    ) -> Result<CopyFolderTreeResult, DomainError> {
        Err(DomainError::internal_error(
            "FileWritePort",
            "copy_folder_tree not implemented for this storage backend",
        ))
    }

    // ── Trash operations ──

    /// Moves a file to the trash
    async fn move_to_trash(&self, file_id: &str) -> Result<(), DomainError>;

    /// Restores a file from the trash to its original location
    async fn restore_from_trash(
        &self,
        file_id: &str,
        original_path: &str,
    ) -> Result<(), DomainError>;

    /// Permanently deletes a file (used by the trash)
    async fn delete_file_permanently(&self, file_id: &str) -> Result<(), DomainError>;
}

// ─────────────────────────────────────────────────────
// Auxiliary ports (unchanged)
// ─────────────────────────────────────────────────────

/// Secondary port for storage usage management
pub trait StorageUsagePort: Send + Sync + 'static {
    /// Updates storage usage statistics for a user
    async fn update_user_storage_usage(&self, user_id: Uuid) -> Result<i64, DomainError>;

    /// Updates storage usage statistics for a user, looked up by username
    async fn update_user_storage_usage_by_username(
        &self,
        username: &str,
    ) -> Result<i64, DomainError>;

    /// Updates storage usage statistics for all users
    async fn update_all_users_storage_usage(&self) -> Result<(), DomainError>;

    /// Checks if a user has enough quota for an additional upload.
    /// Returns Ok(()) if the upload is allowed, or Err(QuotaExceeded) with a
    /// descriptive message otherwise.
    async fn check_storage_quota(
        &self,
        user_id: Uuid,
        additional_bytes: u64,
    ) -> Result<(), DomainError>;

    /// Returns (used_bytes, quota_bytes) for a user.
    async fn get_user_storage_info(&self, user_id: Uuid) -> Result<(i64, i64), DomainError>;
}

/// Generic storage service interface for calendar and contact services
pub trait StorageUseCase: Send + Sync + 'static {
    /// Handle a request with the specified action and parameters
    async fn handle_request(&self, action: &str, params: Value) -> Result<Value, DomainError>;
}
