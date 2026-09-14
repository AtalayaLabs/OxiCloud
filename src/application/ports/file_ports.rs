use bytes::Bytes;
use futures::Stream;
use std::pin::Pin;
use std::sync::Arc;
use uuid::Uuid;

use crate::application::dtos::file_dto::FileDto;
use crate::application::ports::storage_ports::CopyFolderTreeResult;
use crate::application::services::file_management_service::FileManagementService;
use crate::application::services::file_retrieval_service::FileRetrievalService;
use crate::application::services::file_upload_service::FileUploadService;
use crate::common::errors::DomainError;
use crate::domain::services::authorization::{Permission, Subject};

// ─────────────────────────────────────────────────────
// Upload port
// ─────────────────────────────────────────────────────

/// A blob already stored in the content-addressable chunk store, carrying
/// exactly ONE reference that the receiving method takes ownership of.
///
/// Produced by the upload-ingest layer (`interfaces::upload_ingest`), which
/// streams the request body straight into the CDC dedup store. Methods that
/// accept a `StoredBlob` either attach the reference to a file row or
/// release it on failure — callers never need to compensate themselves.
#[derive(Debug, Clone)]
pub struct StoredBlob {
    /// BLAKE3 of the full content (manifest / blob key).
    pub hash: String,
    /// Content size in bytes.
    pub size: u64,
    /// `false` when the content already existed (dedup hit) — forwarded to
    /// lifecycle hooks so e.g. thumbnails aren't regenerated for known blobs.
    pub is_new_blob: bool,
}

/// Primary port for file upload operations.
///
/// **All upload paths converge on streaming-into-the-chunk-store** — content
/// never passes through this port; it is ingested by the interface layer
/// (CDC chunking + hashing while the body arrives, no spool file) and only
/// the resulting [`StoredBlob`] reference travels through here.
pub trait FileUploadUseCase: Send + Sync + 'static {
    /// Register a new file row pointing at an already-ingested blob.
    ///
    /// Takes ownership of the blob's reference (released on failure).
    ///
    /// `caller_id` is plumbed down into
    /// `FileWritePort::save_file_with_blob` so the §14 `created_by` /
    /// `updated_by` columns record the principal performing the upload —
    /// not the parent folder's owner. D2 shared drives surface this
    /// most clearly: Adam upload into Alice's folder must record
    /// `created_by = adam.id`.
    async fn upload_file_streaming(
        &self,
        name: String,
        folder_id: Option<String>,
        content_type: String,
        blob: StoredBlob,
        caller_id: Uuid,
    ) -> Result<FileDto, DomainError>;

    /// `_with_perms` variant of `upload_file_streaming` — enforces
    /// `Create` on the target folder before registering the row.
    ///
    /// AuthZ audit #17 (2026-07-12): the chunked-upload `complete`
    /// path called plain `upload_file_streaming` at finalize; a grant
    /// revoked between session open and finalize stayed effective
    /// until the caller landed the final chunk (up to 24h JWT TTL,
    /// forever with app-passwords). Handlers now call this variant
    /// so the engine re-checks at finalize regardless of how long
    /// the session was open.
    async fn upload_file_streaming_with_perms(
        &self,
        name: String,
        folder_id: Option<String>,
        content_type: String,
        blob: StoredBlob,
        caller_id: Uuid,
    ) -> Result<FileDto, DomainError>;

    /// Replace the content of the file at `path` with an already-ingested
    /// blob, or create the file when it doesn't exist (WebDAV/WOPI PUT).
    ///
    /// Takes ownership of the blob's reference (released on failure).
    ///
    /// `drive_id` scopes both the existence probe (`find_file_by_path`)
    /// and the parent-folder resolution (`get_parent_folder_id`) — the
    /// handler is responsible for deriving it from its protocol context
    /// (NC chroot, native default-drive lookup, WOPI default-drive).
    ///
    /// `caller_id` is plumbed down into
    /// `FileWritePort::update_file_content_with_blob` so the §14
    /// `updated_by` column reflects the principal that performed the
    /// PUT — not the file's existing owner (D2 shared drives let
    /// non-owners overwrite content).
    /// `_with_perms` suffix (AGENTS.md AuthZ convention): the
    /// implementation calls `authz.require(caller, Update, File(id))`
    /// on the overwrite branch and `authz.require(caller, Create,
    /// Folder|Drive(id))` on the new-file branch. Handlers just plumb
    /// `caller_id` through — no protocol-layer authz.
    ///
    /// `expected_hash`: forwarded to
    /// `FileWritePort::update_file_content_with_blob` on the overwrite
    /// branch for compare-and-swap; ignored on the new-file branch
    /// (nothing to compare against). Pass `None` for plain PUT/WOPI/
    /// chunked-upload last-write-wins semantics; pass the pre-write
    /// snapshot's content hash for PATCH, where a concurrent write
    /// during the (potentially slow) splice must be rejected rather
    /// than silently clobbered.
    #[allow(clippy::too_many_arguments)]
    async fn update_file_streaming_with_perms(
        &self,
        path: &str,
        drive_id: Uuid,
        blob: StoredBlob,
        content_type: &str,
        modified_at: Option<i64>,
        caller_id: Uuid,
        expected_hash: Option<&str>,
    ) -> Result<FileDto, DomainError>;
}

// ─────────────────────────────────────────────────────
// Retrieval / download port
// ─────────────────────────────────────────────────────

/// Optimized file content returned by the retrieval service.
///
/// The handler only needs to map each variant to the appropriate HTTP
/// response; all caching / transcoding decisions happen in the
/// application layer.
pub enum OptimizedFileContent {
    /// Small-file content (possibly transcoded / compressed) already in RAM.
    Bytes {
        data: Bytes,
        mime_type: Arc<str>,
        was_transcoded: bool,
    },
    /// Streaming download for everything above the in-RAM cache threshold.
    Stream(Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>),
}

/// Result of a cache-aware HTTP-Range read
/// (`FileRetrievalService::get_file_range_preloaded`). Same split as
/// [`OptimizedFileContent`]: handlers map each variant onto a response body.
pub enum RangeContent {
    /// Zero-copy slice out of the RAM content cache (a `Bytes::slice` is a
    /// refcount bump — no allocation, no I/O, no DB).
    Bytes(Bytes),
    /// Streaming range read from the blob store (cache miss / large file).
    Stream(Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>),
}

/// Primary port for file retrieval operations
pub trait FileRetrievalUseCase: Send + Sync + 'static {
    /// Gets a file by its ID (system/internal — no ownership check).
    async fn get_file(&self, id: &str) -> Result<FileDto, DomainError>;

    // ── Read path: `Subject`, not `caller_id: Uuid` ──────────────────
    //
    // The five methods below take a `Subject` because a public-share
    // visitor must be able to reach them as `Subject::Token(share_id)`.
    // Every other method on this trait keeps `caller_id: Uuid` — and that
    // asymmetry is deliberate documentation, not an oversight: a method
    // taking a `Uuid` *cannot* be called with a token principal, so the
    // signature itself records which operations a non-user caller may
    // perform. Widening one to `Subject` is a decision to expose it, and
    // should be made on purpose.

    /// Gets a file by its ID, enforcing that `caller` may read it.
    ///
    /// Returns `NotFound` when the file does not exist **or** the caller
    /// has no grant reaching it. All user-facing handlers should use this
    /// method.
    async fn get_file_with_perms(&self, id: &str, caller: Subject) -> Result<FileDto, DomainError>;

    async fn get_file_or_trashed_with_perms(
        &self,
        id: &str,
        caller: Subject,
    ) -> Result<FileDto, DomainError>;

    /// Gets a file by its path (for WebDAV), scoped to a drive.
    ///
    /// Post-D0, `storage.files.path` is unique only within a single
    /// drive. The `drive_id` filter scopes the lookup to a specific
    /// drive (caller derives it from its protocol context: NC chroot,
    /// native default-drive lookup, WOPI default-drive lookup).
    async fn get_file_by_path(&self, path: &str, drive_id: Uuid) -> Result<FileDto, DomainError>;

    /// Lists files in a folder
    async fn list_files(&self, folder_id: Option<&str>) -> Result<Vec<FileDto>, DomainError>;

    /// Lists files in a folder, scoped to the authenticated user.
    ///
    /// Uses SQL-level `AND user_id` filtering — no in-memory post-filter.
    /// All user-facing list handlers should use this method.
    async fn list_files_with_perms(
        &self,
        folder_id: Option<&str>,
        owner_id: Uuid,
    ) -> Result<Vec<FileDto>, DomainError>;

    /// Gets file content as a stream (for large files)
    async fn get_file_stream(
        &self,
        id: &str,
    ) -> Result<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>, DomainError>;

    /// Gets file content as a stream, enforcing that `caller_id` is the owner.
    async fn get_file_stream_with_perms(
        &self,
        id: &str,
        caller: Subject,
    ) -> Result<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>, DomainError>;

    /// Optimized multi-tier download.
    ///
    /// Internalises: write-behind lookup → content-cache → WebP transcode →
    /// mmap → streaming, returning an `OptimizedFileContent` variant so the
    /// handler only builds the HTTP response.
    async fn get_file_optimized(
        &self,
        id: &str,
        accept_webp: bool,
        prefer_original: bool,
    ) -> Result<(FileDto, OptimizedFileContent), DomainError>;

    /// Ownership-scoped optimized download.
    ///
    /// Verifies `caller_id` owns the file before returning content.
    /// All user-facing download handlers should use this.
    async fn get_file_optimized_with_perms(
        &self,
        id: &str,
        caller: Subject,
        accept_webp: bool,
        prefer_original: bool,
    ) -> Result<(FileDto, OptimizedFileContent), DomainError>;

    /// Like `get_file_optimized` but accepts an already-fetched `FileDto`,
    /// avoiding a redundant metadata query when the handler already has it.
    async fn get_file_optimized_preloaded(
        &self,
        id: &str,
        file_dto: FileDto,
        accept_webp: bool,
        prefer_original: bool,
    ) -> Result<(FileDto, OptimizedFileContent), DomainError> {
        // Default: ignore pre-fetched meta, re-fetch everything.
        let _ = file_dto;
        self.get_file_optimized(id, accept_webp, prefer_original)
            .await
    }

    /// Range-based streaming for HTTP Range Requests (video seek, resumable DL).
    async fn get_file_range_stream(
        &self,
        id: &str,
        start: u64,
        end: Option<u64>,
    ) -> Result<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>, DomainError>;

    /// Ownership-scoped range stream — verifies caller owns the file first.
    async fn get_file_range_stream_with_perms(
        &self,
        id: &str,
        caller: Subject,
        start: u64,
        end: Option<u64>,
    ) -> Result<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>, DomainError>;

    /// Streams every file in the subtree rooted at `folder_id`.
    ///
    /// Returns a streaming cursor — RAM stays O(1) per row.  Callers
    /// consume incrementally (e.g. group into a HashMap by folder_id)
    /// without materializing the full result set.
    async fn stream_files_in_subtree(
        &self,
        folder_id: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<FileDto, DomainError>> + Send>>, DomainError>;

    /// Lists files in a folder with LIMIT/OFFSET pagination.
    ///
    /// Used by streaming WebDAV PROPFIND to avoid loading all files at once.
    /// Default: falls back to `list_files` (loads all, then slices in memory).
    async fn list_files_batch(
        &self,
        folder_id: Option<&str>,
        after_name: Option<&str>,
        limit: i64,
    ) -> Result<Vec<FileDto>, DomainError> {
        let mut all = self.list_files(folder_id).await?;
        all.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(all
            .into_iter()
            .filter(|f| after_name.is_none_or(|a| f.name.as_str() > a))
            .take(limit as usize)
            .collect())
    }

    /// Like [`list_files_batch`], but scoped to a specific caller.
    ///
    /// Used by streaming WebDAV PROPFIND. Post-D7 the concrete
    /// implementation in `FileRetrievalService` uses drive-membership
    /// grants; this default falls back to the unscoped listing (the
    /// caller passes through `owner_id` for interface parity but the
    /// stub can't apply a real filter without a repo lookup).
    async fn list_files_batch_with_perms(
        &self,
        folder_id: Option<&str>,
        _owner_id: Uuid,
        after_name: Option<&str>,
        limit: i64,
    ) -> Result<Vec<FileDto>, DomainError> {
        self.list_files_batch(folder_id, after_name, limit).await
    }
}

/// Primary port for file management operations
pub trait FileManagementUseCase: Send + Sync + 'static {
    /// Takes a **set** of [`Subject`]s rather than a bare `caller_id: Uuid`
    /// because one HTTP caller can hold several credentials at once: a
    /// logged-in Alice who has also opened a public share carries
    /// `Subject::User(alice)` *and* one `Subject::Token(share_id)` per share
    /// in her ring. Passing the whole set lets the engine grant on any of
    /// them (see `AuthorizationEngine::require_any`), which is the only shape
    /// under which "click a colleague's share link" neither downgrades her
    /// session nor 404s the shared file.
    ///
    /// Wrapping a `Uuid` in `Subject::User` at the call site keeps the
    /// decision about *what kind of principal this is* with the caller,
    /// where it is known, instead of assuming it here. A one-element slice is
    /// the normal case and behaves exactly as a single-subject check.
    ///
    /// Returns the credential that granted, so a handler making a further
    /// single-subject call for the same file passes back the one that worked
    /// rather than guessing.
    async fn require_permission(
        &self,
        callers: &[Subject],
        permission: Permission,
        file_id: &str,
    ) -> Result<Subject, DomainError>;

    /// Moves a file, enforcing that `caller_id` is the owner.
    async fn move_file_with_perms(
        &self,
        file_id: &str,
        caller_id: Uuid,
        folder_id: Option<String>,
    ) -> Result<FileDto, DomainError>;

    /// Copies a file, enforcing that `caller_id` is the owner.
    ///
    /// `new_name`, when `Some(_)`, becomes the copy's filename — without it
    /// the copy keeps the source's name, which makes "same folder, different
    /// name" copies (classic WebDAV `COPY /a.txt → /b.txt`) collide on the
    /// `(folder, name, user)` unique index.
    async fn copy_file_with_perms(
        &self,
        file_id: &str,
        caller_id: Uuid,
        target_folder_id: Option<String>,
        new_name: Option<String>,
    ) -> Result<FileDto, DomainError>;

    /// Renames a file, enforcing that `caller_id` is the owner.
    async fn rename_file_with_perms(
        &self,
        file_id: &str,
        caller_id: Uuid,
        new_name: &str,
    ) -> Result<FileDto, DomainError>;

    /// Deletes a file, enforcing that `caller_id` is the owner.
    async fn delete_file_with_perms(&self, id: &str, caller_id: Uuid) -> Result<(), DomainError>;

    /// Smart delete: trash-first with dedup reference cleanup.
    ///
    /// 1. Tries to move to trash (soft delete).
    /// 2. Falls back to permanent delete if trash unavailable/failed.
    /// 3. Decrements the dedup reference count for the content hash.
    ///
    /// Returns `Ok(true)` when trashed, `Ok(false)` when permanently deleted.
    async fn delete_and_cleanup_with_perms(
        &self,
        id: &str,
        user_id: Uuid,
    ) -> Result<bool, DomainError>;

    /// Copies an entire folder subtree atomically (WebDAV COPY Depth: infinity).
    /// enforcing that `caller_id` owns both the source folder
    /// and the target parent folder.
    ///
    /// Creates a copy of `source_folder_id` (with optional name override) under
    /// `target_parent_id`, including ALL sub-folders and files. Files are
    /// zero-copy (blob ref_counts incremented in batch).
    ///
    /// Default: returns error (only available with PostgreSQL backend).
    async fn copy_folder_tree_with_perms(
        &self,
        source_folder_id: &str,
        caller_id: Uuid,
        target_parent_id: Option<String>,
        dest_name: Option<String>,
    ) -> Result<CopyFolderTreeResult, DomainError>;
}

/// Factory for creating file use case implementations
pub trait FileUseCaseFactory: Send + Sync + 'static {
    fn create_file_upload_use_case(&self) -> Arc<FileUploadService>;
    fn create_file_retrieval_use_case(&self) -> Arc<FileRetrievalService>;
    fn create_file_management_use_case(&self) -> Arc<FileManagementService>;
}
