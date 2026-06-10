//! Blob Storage Backend Port — abstracts raw byte I/O for content-addressable storage.
//!
//! This trait decouples `DedupService` from any specific storage medium.
//! Implementations include:
//! - `LocalBlobBackend`  — local filesystem (default)
//! - `S3BlobBackend`     — any S3-compatible service (AWS, Backblaze B2, MinIO, R2…)
//!
//! `DedupService` owns an `Arc<dyn BlobStorageBackend>` and delegates all
//! byte-level I/O through this trait, keeping BLAKE3 hashing, ref-counting
//! and PostgreSQL index logic in `DedupService` itself.

use bytes::Bytes;
use futures::Stream;
use serde::Serialize;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use crate::domain::errors::DomainError;

/// Boxed future alias used by [`BlobStorageBackend`] to keep the trait dyn-compatible.
type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Pinned boxed byte stream — the return type for blob reads.
pub type BlobStream = Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>;

/// Health-check result returned by [`BlobStorageBackend::health_check`].
#[derive(Debug, Clone, Serialize)]
pub struct StorageHealthStatus {
    /// Whether the backend is reachable and functional.
    pub connected: bool,
    /// Human-readable backend identifier (e.g. `"local"`, `"s3"`).
    pub backend_type: String,
    /// Descriptive status message.
    pub message: String,
    /// Available space in bytes, if the backend can report it.
    pub available_bytes: Option<u64>,
}

/// Minimal trait for blob byte I/O — decoupled from dedup logic.
///
/// Every method operates on a *hash key* that uniquely identifies a blob.
/// The backend is responsible for mapping the hash to its own addressing
/// scheme (filesystem path, S3 key, etc.).
///
/// Returns boxed futures so the trait is dyn-compatible (`Arc<dyn BlobStorageBackend>`).
pub trait BlobStorageBackend: Send + Sync + 'static {
    /// Perform any one-time setup (create directories, verify bucket, etc.).
    fn initialize(&self) -> BoxFut<'_, Result<(), DomainError>>;

    /// Store a blob from a local temporary file.
    ///
    /// Must be **idempotent**: if the blob already exists the call succeeds
    /// without overwriting.  Returns the number of bytes stored.
    fn put_blob(&self, hash: &str, source_path: &Path) -> BoxFut<'_, Result<u64, DomainError>>;

    /// Store a blob from in-memory bytes (used by CDC chunk storage).
    ///
    /// Must be **idempotent**: if the blob already exists the call succeeds
    /// without overwriting.  Returns the number of bytes stored.
    fn put_blob_from_bytes(&self, hash: &str, data: Bytes) -> BoxFut<'_, Result<u64, DomainError>>;

    /// Store a blob from in-memory bytes **without forcing durability**.
    ///
    /// Same idempotency contract as [`Self::put_blob_from_bytes`], but the
    /// bytes may still sit in volatile caches (e.g. the OS page cache) when
    /// the future resolves. Durability is only guaranteed after a subsequent
    /// [`Self::sync_blobs`] covering this hash returns `Ok`. Callers MUST NOT
    /// record a durable reference to the blob (e.g. a PostgreSQL row) before
    /// that sync completes.
    ///
    /// Default: delegates to `put_blob_from_bytes` (immediately durable),
    /// pairing with the no-op `sync_blobs` default so backends that don't
    /// opt in keep today's per-write durability semantics.
    fn put_blob_from_bytes_unsynced(
        &self,
        hash: &str,
        data: Bytes,
    ) -> BoxFut<'_, Result<u64, DomainError>> {
        self.put_blob_from_bytes(hash, data)
    }

    /// Make previously written blobs durable in one batched operation.
    ///
    /// Durability barrier for blobs written via `put_blob_from_bytes_unsynced`:
    /// when this returns `Ok`, every listed blob is crash-safe. Local
    /// filesystem backends fsync each listed blob file plus each distinct
    /// parent directory once — one sweep per upload instead of two fsyncs
    /// per chunk. Remote object stores are durable on PUT, so the default
    /// is a no-op.
    fn sync_blobs(&self, _hashes: &[String]) -> BoxFut<'_, Result<(), DomainError>> {
        Box::pin(async { Ok(()) })
    }

    /// Stream the full blob content in chunks.
    fn get_blob_stream(&self, hash: &str) -> BoxFut<'_, Result<BlobStream, DomainError>>;

    /// Stream the byte range `[start, end)` of the blob (for HTTP Range
    /// requests / video seek). `end` is **exclusive**; `None` means "to the
    /// end of the blob". Callers translating inclusive HTTP Range headers
    /// must pass `last_byte + 1`.
    fn get_blob_range_stream(
        &self,
        hash: &str,
        start: u64,
        end: Option<u64>,
    ) -> BoxFut<'_, Result<BlobStream, DomainError>>;

    /// Delete a blob by hash.  Must be **idempotent** (no error if already gone).
    fn delete_blob(&self, hash: &str) -> BoxFut<'_, Result<(), DomainError>>;

    /// Check if a blob exists in the backend.
    fn blob_exists(&self, hash: &str) -> BoxFut<'_, Result<bool, DomainError>>;

    /// Get blob size in bytes without downloading content.
    fn blob_size(&self, hash: &str) -> BoxFut<'_, Result<u64, DomainError>>;

    /// Verify connectivity and permissions (used by the admin "Test Connection" button).
    fn health_check(&self) -> BoxFut<'_, Result<StorageHealthStatus, DomainError>>;

    /// Return the backend type name for display (e.g. `"local"`, `"s3"`).
    fn backend_type(&self) -> &'static str;

    /// Return the local filesystem path for a blob, if available.
    ///
    /// Only meaningful for local-filesystem backends.  Remote backends
    /// return `None`; callers that need a local file must stream + spool.
    fn local_blob_path(&self, hash: &str) -> Option<PathBuf>;
}
