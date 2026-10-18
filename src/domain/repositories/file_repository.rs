//! Domain persistence port for the File entity.
//!
//! Defines the contract that any file storage implementation must fulfill.
//! This trait lives in the domain because File is a core entity of the system
//! and its persistence contracts belong to the domain layer, following
//! Clean/Hexagonal Architecture principles.
//!
//! Concrete implementations (filesystem, PostgreSQL, S3, etc.) live in
//! the infrastructure layer.

use std::path::PathBuf;

use bytes::Bytes;
use futures::Stream;
use uuid::Uuid;

use crate::common::errors::DomainError;
use crate::domain::entities::file::File;
use crate::domain::services::path_service::StoragePath;

// ─────────────────────────────────────────────────────
// FileReadRepository — read/query operations
// ─────────────────────────────────────────────────────

/// Domain port for file **reading**.
///
/// Encapsulates every operation that queries state without modifying it:
/// get, list, content, stream, mmap, range, path resolution.
pub trait FileReadRepository: Send + Sync + 'static {
    /// Gets a file by its ID.
    async fn get_file(&self, id: &str) -> Result<File, DomainError>;

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
    /// drive — the `drive_id` filter scopes the lookup.
    async fn get_parent_folder_id(&self, path: &str, drive_id: Uuid)
    -> Result<String, DomainError>;
}

// ─────────────────────────────────────────────────────
// FileWriteRepository — write/mutation operations
// ─────────────────────────────────────────────────────

/// Domain port for file **writing**.
///
/// Covers: upload (buffered + streaming), move, delete, update,
/// and deferred registration for write-behind cache.
pub trait FileWriteRepository: Send + Sync + 'static {
    /// Saves a new file from bytes.
    async fn save_file(
        &self,
        name: String,
        folder_id: Option<String>,
        content_type: String,
        content: Vec<u8>,
    ) -> Result<File, DomainError>;

    /// Registers a file row pointing at a blob already stored in the
    /// content-addressable chunk store (one blob reference is consumed).
    ///
    /// `caller_id` stamps both `created_by` and `updated_by`
    /// (§14 provenance).
    ///
    /// `content_modified_at` is the client-asserted source mtime in epoch
    /// seconds (`X-OC-Mtime`), already validated by
    /// `timestamp_helpers::validate_client_mtime`; `None` means "stamp NOW()".
    /// Threading it through the CREATE path — not just the overwrite path —
    /// is what makes an upload of a pre-existing file keep its mtime, which
    /// is the common NextCloud-desktop case.
    // Flat column values for one INSERT — see the mirror declaration in
    // `application/ports/storage_ports.rs`.
    #[allow(clippy::too_many_arguments)]
    async fn save_file_with_blob(
        &self,
        name: String,
        folder_id: Option<String>,
        content_type: String,
        blob_hash: &str,
        size: u64,
        caller_id: Uuid,
        content_modified_at: Option<i64>,
    ) -> Result<File, DomainError>;

    /// Moves a file to another folder. `caller_id` stamps `updated_by`
    /// in the same UPDATE that bumps `updated_at` (§14 provenance).
    async fn move_file(
        &self,
        file_id: &str,
        target_folder_id: Option<String>,
        caller_id: Uuid,
    ) -> Result<File, DomainError>;

    /// Renames a file (same folder, different name). `caller_id`
    /// stamps `updated_by` in the same UPDATE (§14 provenance).
    async fn rename_file(
        &self,
        file_id: &str,
        new_name: &str,
        caller_id: Uuid,
    ) -> Result<File, DomainError>;

    /// Deletes a file.
    async fn delete_file(&self, id: &str) -> Result<(), DomainError>;

    /// Updates the content of an existing file.
    async fn update_file_content(&self, file_id: &str, content: Vec<u8>)
    -> Result<(), DomainError>;

    /// Registers file metadata WITHOUT writing content to disk (write-behind).
    ///
    /// Returns `(File, PathBuf)` where `PathBuf` is the destination path for
    /// the deferred write that the `WriteBehindCache` will perform.
    ///
    /// `caller_id` stamps both `created_by` and `updated_by`
    /// (§14 provenance).
    async fn register_file_deferred(
        &self,
        name: String,
        folder_id: Option<String>,
        content_type: String,
        size: u64,
        caller_id: Uuid,
    ) -> Result<(File, PathBuf), DomainError>;

    // ── Trash operations ──

    /// Moves a file to the trash. `caller_id` stamps `updated_by`
    /// (§14 provenance).
    async fn move_to_trash(&self, file_id: &str, caller_id: Uuid) -> Result<(), DomainError>;

    /// Restores a file from the trash to its original location.
    /// `caller_id` stamps `updated_by` (§14 provenance).
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
// FileRepository — unified supertrait
// ─────────────────────────────────────────────────────

/// Unified port for file persistence.
///
/// It is a supertrait of `FileReadRepository + FileWriteRepository`.
/// Any type that implements both ports gets `FileRepository`
/// automatically via blanket impl.
pub trait FileRepository: FileReadRepository + FileWriteRepository {}

/// Blanket implementation: any type that implements both ports
/// is automatically a FileRepository.
impl<T: FileReadRepository + FileWriteRepository> FileRepository for T {}
