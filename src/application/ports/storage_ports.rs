use bytes::Bytes;
use futures::Stream;
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

    /// Lists files in a folder.
    async fn list_files(&self, folder_id: Option<&str>) -> Result<Vec<File>, DomainError>;

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

    /// Gets the parent folder ID from a path (WebDAV), scoped to a drive.
    ///
    /// Post-D0, `storage.folders.path` is unique only within a single
    /// drive. The `drive_id` filter scopes the lookup to a specific
    /// drive (caller derives it from its protocol context: NC chroot,
    /// native default-drive lookup, WOPI default-drive lookup).
    async fn get_parent_folder_id(&self, path: &str, drive_id: Uuid)
    -> Result<String, DomainError>;

    /// Gets a folder ID by its path, scoped to a drive.
    ///
    /// Post-D0 same scoping rule as `get_parent_folder_id` — names like
    /// `"Personal"` repeat across drives, so the `drive_id` filter is
    /// required to disambiguate.
    async fn get_folder_id_by_path(
        &self,
        folder_path: &str,
        drive_id: Uuid,
    ) -> Result<String, DomainError>;

    /// Gets the content-addressable blob hash for a file (O(1) DB lookup).
    ///
    /// Returns the BLAKE3 hash stored in `storage.files.blob_hash`.
    /// Used for dedup reference tracking without loading file content.
    async fn get_blob_hash(&self, file_id: &str) -> Result<String, DomainError>;

    /// Find a file by its logical path (folder_name/.../file_name),
    /// scoped to a drive.
    ///
    /// Post-D0 `storage.files.path` is unique only within a single
    /// drive. The `drive_id` filter prevents non-deterministic
    /// resolution when the same path exists in multiple drives.
    ///
    /// The default implementation falls back to `list_files(None)` + linear
    /// scan (O(N)) and ignores the drive filter — only used by stubs.
    /// Repositories should override with a direct SQL query that applies
    /// the filter.
    async fn find_file_by_path(
        &self,
        path: &str,
        _drive_id: Uuid,
    ) -> Result<Option<File>, DomainError> {
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

    /// Lists files in a folder in name order, keyset-paginated.
    ///
    /// Used by streaming WebDAV PROPFIND to avoid loading all files at once.
    /// `after_name` is the last name of the previous page (`None` = first
    /// page); names are unique within a folder (unique index on
    /// `(drive_id, folder_id, name)`), so `name > after_name` is a total,
    /// stable cursor. Unlike LIMIT/OFFSET, every page is O(page) — the old
    /// offset shape re-scanned and re-sorted the whole folder per page
    /// (benches/PROPFIND-PAGING.md).
    ///
    /// Default: falls back to `list_files` (loads all, then slices in memory).
    async fn list_files_batch(
        &self,
        folder_id: Option<&str>,
        after_name: Option<&str>,
        limit: i64,
    ) -> Result<Vec<File>, DomainError> {
        let mut all = self.list_files(folder_id).await?;
        all.sort_by(|a, b| a.name().cmp(b.name()));
        Ok(all
            .into_iter()
            .filter(|f| after_name.is_none_or(|a| f.name() > a))
            .take(limit as usize)
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
    /// * `caller_id` - Caller user id — scoped by drive-membership grants
    ///   (`role_grants` on `resource_type='drive'`) rather than the legacy
    ///   `files.user_id` column. Group memberships (direct + transitive)
    ///   are expanded inline via `storage.caller_group_ids($caller)`.
    ///
    /// # Returns
    /// A tuple `(files, caller_flags, total_count)`:
    /// - `files`: the paginated + filtered file rows.
    /// - `caller_flags`: parallel `Vec<(is_favorite, is_shared)>` aligned
    ///   1:1 with `files` by index. Populated in-SQL via per-row EXISTS
    ///   subqueries on `auth.user_favorites` and `storage.role_grants` so
    ///   the caller's SPA can render badges without a follow-up round-trip
    ///   (same pattern the photos-timeline listing uses). Kept as a
    ///   parallel vec rather than folded into `File` so the domain
    ///   entity stays caller-agnostic.
    /// - `total_count`: `COUNT(*) OVER()` total for pagination.
    async fn search_files_paginated(
        &self,
        folder_id: Option<&str>,
        criteria: &SearchCriteriaDto,
        caller_id: Uuid,
    ) -> Result<(Vec<File>, Vec<(bool, bool)>, usize), DomainError>;

    /// Search files recursively in a folder subtree using ltree.
    ///
    /// When `root_folder_id` is Some, uses ltree descendant queries to find
    /// all files within the subtree rooted at that folder. When None,
    /// delegates to `search_files_paginated`.
    ///
    /// Post-PR-B: scoped by drive-membership grants (same semantics as
    /// `search_files_paginated`), not by `files.user_id`.
    ///
    /// Returns the same shape as [`Self::search_files_paginated`]:
    /// `(files, caller_flags, total_count)`.
    async fn search_files_in_subtree(
        &self,
        root_folder_id: Option<&str>,
        criteria: &SearchCriteriaDto,
        caller_id: Uuid,
    ) -> Result<(Vec<File>, Vec<(bool, bool)>, usize), DomainError> {
        // Default: delegate to paginated search (non-recursive fallback)
        self.search_files_paginated(root_folder_id, criteria, caller_id)
            .await
    }

    /// Return up to `limit` files whose name contains `query` (case-insensitive).
    ///
    /// Results are ordered by relevance (exact > starts-with > contains) so the
    /// caller can use them directly for autocomplete suggestions.
    ///
    /// `caller_id` scopes results to files whose owning drive the caller can
    /// Read (direct or group-mediated `role_grants`). Without it the endpoint
    /// leaks names + paths across every tenant on the instance — closed as
    /// AuthZ audit finding #1 (2026-07-12).
    ///
    /// The default implementation falls back to `list_files` + in-memory
    /// filter so that stubs and mocks compile without changes. Stub-mode
    /// callers already operate against a single tenant's data, so ignoring
    /// `caller_id` here is safe; the PG impl enforces the real scope.
    async fn suggest_files_by_name(
        &self,
        folder_id: Option<&str>,
        query: &str,
        limit: usize,
        _caller_id: Uuid,
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
/// Covers: upload registration, move, delete, update, and deferred
/// registration for the write-behind cache.
pub trait FileWritePort: Send + Sync + 'static {
    /// Register a file row pointing at a blob already stored in the
    /// content-addressable chunk store.
    ///
    /// Takes ownership of one blob reference: on any failure the reference
    /// is released before the error is returned.
    ///
    /// `caller_id` is stamped into both `created_by` and `updated_by`
    /// (§14 provenance — authorship belongs to the caller, not to the
    /// parent folder's owner). In D2 shared drives, a non-owner member
    /// can upload into a folder owned by someone else; the previous
    /// `created_by = parent.user_id` would have silently recorded the
    /// wrong principal.
    async fn save_file_with_blob(
        &self,
        name: String,
        folder_id: Option<String>,
        content_type: String,
        blob_hash: &str,
        size: u64,
        caller_id: Uuid,
    ) -> Result<File, DomainError>;

    /// Moves a file to another folder. `caller_id` is stamped into
    /// `updated_by` alongside the `updated_at = NOW()` bump
    /// (§14 provenance — authorship belongs to the caller, not to
    /// the destination folder's owner).
    async fn move_file(
        &self,
        file_id: &str,
        target_folder_id: Option<String>,
        caller_id: Uuid,
    ) -> Result<File, DomainError>;

    /// Renames a file (same folder, different name). `caller_id` is
    /// stamped into `updated_by` alongside the `updated_at = NOW()`
    /// bump (§14 provenance).
    async fn rename_file(
        &self,
        file_id: &str,
        new_name: &str,
        caller_id: Uuid,
    ) -> Result<File, DomainError>;

    /// Deletes a file.
    async fn delete_file(&self, id: &str) -> Result<(), DomainError>;

    /// Atomically swap a file's content to a blob already stored in the
    /// content-addressable chunk store.
    ///
    /// Takes ownership of one blob reference (released on failure); the
    /// previous content's reference is dropped after the swap.
    ///
    /// Returns `(new_blob_hash, updated_at_epoch)` — everything a caller
    /// needs to rebuild the fresh entity/ETag from a `File` it already
    /// holds, without re-reading the row it just updated.
    ///
    /// `caller_id` is stamped into `updated_by` alongside the
    /// `updated_at` bump (§14 provenance).
    ///
    /// `expected_hash`: when `Some`, makes this a true compare-and-swap —
    /// the write only takes effect if the row's current `blob_hash`
    /// still equals it, checked and applied atomically under the same
    /// row lock (no gap between check and write for a concurrent writer
    /// to land in). A mismatch returns `ErrorKind::PreconditionFailed`
    /// and leaves the row untouched. `None` keeps the previous
    /// blind-overwrite behaviour (PUT/WOPI/chunked-upload finalize).
    async fn update_file_content_with_blob(
        &self,
        file_id: &str,
        blob_hash: &str,
        size: u64,
        modified_at: Option<i64>,
        caller_id: Uuid,
        expected_hash: Option<&str>,
    ) -> Result<(String, i64), DomainError>;

    /// Registers file metadata WITHOUT writing content to disk (write-behind).
    ///
    /// Returns `(File, PathBuf)` where `PathBuf` is the destination path for the
    /// deferred write that the `WriteBehindCache` will perform.
    ///
    /// `caller_id` is stamped into both `created_by` and `updated_by`
    /// (§14 provenance — see `save_file_with_blob`).
    async fn register_file_deferred(
        &self,
        name: String,
        folder_id: Option<String>,
        content_type: String,
        size: u64,
        caller_id: Uuid,
    ) -> Result<(File, PathBuf), DomainError>;

    /// Copies a file to a (possibly different) folder.
    ///
    /// With blob-dedup, this only creates a new metadata row and increments
    /// the blob reference count — zero disk I/O for the content.
    ///
    /// `new_name` is honored when `Some(_)` — without it, copying a file to
    /// the same folder always collides on the source's filename. WebDAV
    /// COPY uses this for the "same folder, different name" case (the
    /// classic `COPY /a.txt → /b.txt` pattern).
    ///
    /// `caller_id` is stamped into both `created_by` and `updated_by`
    /// on the new row (§14 provenance — the caller authored this copy,
    /// not the destination folder's owner).
    async fn copy_file(
        &self,
        file_id: &str,
        target_folder_id: Option<String>,
        new_name: Option<&str>,
        caller_id: Uuid,
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

    /// Moves a file to the trash. `caller_id` is stamped into
    /// `updated_by` (§14 provenance).
    async fn move_to_trash(&self, file_id: &str, caller_id: Uuid) -> Result<(), DomainError>;

    /// Restores a file from the trash to its original location.
    /// `caller_id` is stamped into `updated_by` (§14 provenance).
    async fn restore_from_trash(
        &self,
        file_id: &str,
        original_path: &str,
        caller_id: Uuid,
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

    /// Reconcile every user's cached `storage_used_bytes` against the
    /// authoritative `SUM(drives.used_bytes)` over their personal
    /// drives. Returns the number of user rows the sweep actually
    /// corrected (`rows_affected` from the `UPDATE … IS DISTINCT
    /// FROM …`) — used by the periodic-scheduler `JobHandler` to
    /// populate `JobOutcome::Ok.count`.
    async fn update_all_users_storage_usage(&self) -> Result<u64, DomainError>;

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

    /// Incrementally adjust one drive's cached `storage.drives.used_bytes`
    /// by `delta` bytes — O(1), the per-upload counterpart to the
    /// O(N) full recompute below. Mirrors `add_user_storage_usage_delta`
    /// in shape: single statement, `GREATEST(0, …)` clamp so a late or
    /// duplicate adjustment can never drive the counter negative.
    /// Deletes/trash do not decrement here (mirroring user-quota
    /// design); the periodic reconciliation sweep is the correctness
    /// backstop.
    async fn add_drive_storage_usage_delta(
        &self,
        drive_id: Uuid,
        delta: i64,
    ) -> Result<(), DomainError>;

    /// Reconcile every drive's cached `used_bytes` against the actual
    /// sum of its non-trashed files in one set-based UPDATE. Same
    /// shape as `update_all_users_storage_usage`: `LEFT JOIN` over a
    /// `GROUP BY drive_id` aggregate, with an `IS DISTINCT FROM`
    /// guard so idle drives don't churn dead tuples. Runs from the
    /// same reconciliation ticker.
    ///
    /// Returns the number of drive rows actually corrected — used by
    /// the periodic-scheduler `JobHandler` to populate `JobOutcome::Ok.count`.
    async fn update_all_drives_storage_usage(&self) -> Result<u64, DomainError>;

    /// Pre-upload quota check on a single drive.
    ///
    /// Returns `Ok(())` when `used_bytes + additional_bytes` fits under
    /// `quota_bytes`, or `Err(QuotaExceeded)` otherwise.
    /// `quota_bytes IS NULL` short-circuits to `Ok(())` — unlimited
    /// drive. Single read-only `SELECT` on `storage.drives`; the
    /// check/write window is a soft cap by design (same semantics as
    /// the user-quota path), bounded by the sweep interval.
    async fn check_drive_quota(
        &self,
        drive_id: Uuid,
        additional_bytes: u64,
    ) -> Result<(), DomainError>;
}
