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
use chrono::{DateTime, Utc};
use futures::Stream;
use serde::Serialize;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use crate::domain::errors::DomainError;

/// One row returned by [`BlobStorageBackend::list_blob_hashes`] — the
/// hash of a blob physically present on the backend, plus its
/// last-modified timestamp when the backend can supply one. `mtime`
/// is used by `backend_consistency` to skip freshly-created files
/// still within the write grace window (avoids false-positive
/// orphans during the durability-before-visibility window that
/// `dedup_service` opens).
#[derive(Debug, Clone)]
pub struct BackendBlobEntry {
    pub hash: String,
    /// `None` when the backend doesn't track mtime — the consistency
    /// scan then falls back to treating the entry as "old enough" and
    /// will emit an orphan finding without a grace check.
    pub mtime: Option<DateTime<Utc>>,
}

/// A file present in the blob-storage namespace but NOT matching the
/// canonical `<64-hex>.blob` shape. Sidecars (`.blob.orig`,
/// `.blob.lost`, `.blob.tmp`), wrong extensions, non-hex names —
/// anything the enumeration filter skips for the blob list. Surfaced
/// so `backend_consistency` can emit them as `anomaly` notices
/// (informational only — they don't hurt anything but the operator
/// should know they're there).
#[derive(Debug, Clone)]
pub struct BackendUnknownEntry {
    /// Backend-relative path (`04/04f48c...blob.orig` on local FS or
    /// as an S3 key). Included in the finding detail so operators
    /// can locate it.
    pub path: String,
    pub mtime: Option<DateTime<Utc>>,
}

/// Return type of [`BlobStorageBackend::list_blob_hashes`] — one
/// batch of the enumeration. Struct (not tuple) so adding future
/// per-batch metadata (e.g. `truncated: bool`) doesn't break every
/// backend impl. `next_cursor = None` signals end of enumeration.
///
/// Backends that don't track sidecar/unknown files leave `unknowns`
/// empty; the tenant just doesn't emit any `unknown_backend_file`
/// notices from that batch.
#[derive(Debug, Clone)]
pub struct BlobListPage {
    pub blobs: Vec<BackendBlobEntry>,
    pub unknowns: Vec<BackendUnknownEntry>,
    pub next_cursor: Option<String>,
}

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

    /// How many chunk fetches the CDC reader may run concurrently when
    /// reassembling a file (`read_blob_stream`'s `buffered(N)` read-ahead).
    ///
    /// The trait default is a conservative **1** (strictly sequential) — the
    /// safe fallback for any backend that doesn't know its own I/O profile.
    /// Concrete backends override it:
    ///   • Local disk → a small benchmarked depth (default `2`, env-tunable):
    ///     overlapping the next chunk's `File::open` with the current chunk's
    ///     drain measured +7–12% on disk-bound reads with no cold regression on
    ///     SSDs; deeper queues buy little (the data read, not the open, is then
    ///     the cost) and risk competing random I/O over scattered content-
    ///     addressed chunk files on seek-bound HDDs. See `LocalBlobBackend`.
    ///   • Remote (S3/Azure) → `8`: there the dominant cost is per-chunk request
    ///     latency, and overlapping fetches hides it (≈ N× faster reassembly).
    /// Wrapping backends delegate to the backend that actually serves the bytes.
    fn read_prefetch(&self) -> usize {
        1
    }

    /// Enumerate blob entries physically present on this backend, in
    /// implementation-defined order — cursor-based paging.
    ///
    /// * `cursor` — opaque continuation token from a prior call, or
    ///   `None` to start from the beginning. Format is per-backend
    ///   (local = last path visited; S3 = continuation token; Azure
    ///   = list marker); callers treat it as opaque.
    /// * `limit` — soft cap on batch size; backends may return
    ///   fewer (e.g. end of a shard directory).
    ///
    /// Returns `(entries, next_cursor)`. `next_cursor = None` means
    /// enumeration is complete. Each `BackendBlobEntry` carries the
    /// hash + optional mtime for grace-window filtering.
    ///
    /// The trait default returns
    /// [`DomainError::NotSupported`](DomainError::not_supported)
    /// — future backends that genuinely can't enumerate (some
    /// write-only queue, some read-only mirror) can inherit it. All
    /// currently-shipped backends (local, S3, Azure) override.
    ///
    /// Filtering out non-blob artifacts (temp files, `.corrupt` /
    /// `.lost` sidecars, encryption metadata) is the backend's
    /// responsibility — the tenant walks whatever this returns.
    fn list_blob_hashes(
        &self,
        _cursor: Option<String>,
        _limit: usize,
    ) -> BoxFut<'_, Result<BlobListPage, DomainError>> {
        Box::pin(async {
            Err(DomainError::operation_not_supported(
                "list_blob_hashes",
                "this backend does not implement enumeration",
            ))
        })
    }
}
