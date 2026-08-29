//! Deduplication Port - Application layer abstraction for content-addressable storage.
//!
//! This module defines the port (trait) and DTOs for deduplication operations,
//! keeping the application and interface layers independent of the specific
//! content-addressable storage implementation.

use crate::common::errors::DomainError;
use bytes::Bytes;
use futures::Stream;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::pin::Pin;

/// Metadata of a stored blob in the dedup system.
#[derive(Debug, Clone, Serialize)]
pub struct BlobMetadataDto {
    /// BLAKE3 hash of the content.
    pub hash: String,
    /// Size in bytes.
    pub size: u64,
    /// Number of references to this blob.
    pub ref_count: u32,
    /// Original content type (for serving).
    pub content_type: Option<String>,
}

/// A stored server-derived artifact: which blob holds it, and what it is.
///
/// `content_type` is carried so the read path can set the response header
/// without byte-sniffing the payload, which is what it does today.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedBlobRef {
    pub blob_hash: String,
    pub content_type: String,
}

/// What the derived tier knows about one `(source_hash, kind, variant)`.
///
/// Three answers, not two. `Option<DerivedBlobRef>` could only say
/// "have it" or "don't", which collapses the two cases that matter most
/// to a caller deciding whether to spend a decode:
///
/// * [`Missing`](Self::Missing) — never attempted. Derive it.
/// * [`NotDerivable`](Self::NotDerivable) — attempted, and the attempt
///   is known to be a waste for this content: the transcode came out
///   larger than the original, the source cannot be decoded, the source
///   is over the decode ceiling. Serve the original and do not retry.
/// * [`Found`](Self::Found) — here are the bytes.
///
/// Only failures that are deterministic in the CONTENT may be recorded
/// as `NotDerivable`. A timeout or an I/O error is a property of the
/// moment; persisting one would mark a good image underivable forever.
#[derive(Debug, Clone)]
pub enum DerivedLookup {
    Missing,
    NotDerivable,
    Found(DerivedBlobRef),
}

/// Result of a deduplication store operation.
#[derive(Debug, Clone)]
pub enum DedupResultDto {
    /// New content was stored (first occurrence).
    NewBlob { hash: String, size: u64 },
    /// Content already existed; a reference was added instead.
    ExistingBlob {
        hash: String,
        size: u64,
        saved_bytes: u64,
    },
}

impl DedupResultDto {
    pub fn hash(&self) -> &str {
        match self {
            DedupResultDto::NewBlob { hash, .. } => hash,
            DedupResultDto::ExistingBlob { hash, .. } => hash,
        }
    }

    pub fn size(&self) -> u64 {
        match self {
            DedupResultDto::NewBlob { size, .. } => *size,
            DedupResultDto::ExistingBlob { size, .. } => *size,
        }
    }

    pub fn was_deduplicated(&self) -> bool {
        matches!(self, DedupResultDto::ExistingBlob { .. })
    }
}

/// Statistics for the deduplication service.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DedupStatsDto {
    /// Total number of unique blobs.
    pub total_blobs: u64,
    /// Total bytes stored (actual disk usage).
    pub total_bytes_stored: u64,
    /// Total bytes referenced (logical size).
    pub total_bytes_referenced: u64,
    /// Bytes saved through deduplication.
    pub bytes_saved: u64,
    /// Number of deduplication hits.
    pub dedup_hits: u64,
    /// Deduplication ratio (referenced / stored).
    pub dedup_ratio: f64,
}

/// Port for content-addressable deduplication operations.
///
/// Implementations store files by their content hash, eliminating
/// duplicate storage automatically. Multiple file references can
/// point to the same physical blob.
pub trait DedupPort: Send + Sync + 'static {
    /// Check if a blob with the given hash exists.
    async fn blob_exists(&self, hash: &str) -> bool;

    /// Look up a server-derived artifact by the content it was derived from.
    ///
    /// The read counterpart of `store_derived_blob`. Returns `None` when no
    /// such variant has been derived yet — the caller then renders it.
    async fn find_derived_blob(
        &self,
        source_hash: &str,
        kind: &str,
        variant: &str,
    ) -> Option<DerivedBlobRef>;

    /// Get metadata for a blob.
    async fn get_blob_metadata(&self, hash: &str) -> Option<BlobMetadataDto>;

    /// Stream blob content in chunks (64 KB default) — constant memory usage.
    async fn read_blob_stream(
        &self,
        hash: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>, DomainError>;

    /// Stream a byte range of a blob — only reads the requested portion.
    ///
    /// Uses seek + take so a 1 MB range on a 1 GB file only reads 1 MB from disk.
    async fn read_blob_range_stream(
        &self,
        hash: &str,
        start: u64,
        end: Option<u64>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>, DomainError>;

    /// Get the size of a blob without reading its content.
    ///
    /// Used by HEAD requests to return Content-Length without loading the file.
    async fn blob_size(&self, hash: &str) -> Result<u64, DomainError>;

    /// Add a reference to a blob (increment ref_count).
    async fn add_reference(&self, hash: &str) -> Result<(), DomainError>;

    /// Remove a reference from a blob.
    ///
    /// Returns `true` if the last reference was removed (the content is now
    /// unreferenced). For CDC content the now-orphaned chunks are reclaimed
    /// later by garbage collection rather than unlinked inline; legacy
    /// whole-file blobs are still freed eagerly.
    async fn remove_reference(&self, hash: &str) -> Result<bool, DomainError>;

    /// Calculate BLAKE3 hash of a file (streaming).
    async fn hash_file(&self, path: &Path) -> Result<String, DomainError>;

    /// Get the physical filesystem path for a blob by its hash.
    ///
    /// Returns the path where the blob is stored on disk.
    /// Used by services that need direct filesystem access (e.g., thumbnail generation).
    fn blob_path(&self, hash: &str) -> PathBuf;

    /// Get deduplication statistics.
    async fn get_stats(&self) -> DedupStatsDto;

    /// Flush the index to persistent storage.
    async fn flush(&self) -> Result<(), DomainError>;

    /// Verify integrity of all stored blobs.
    ///
    /// Returns a list of issues found (empty if everything is OK).
    async fn verify_integrity(&self) -> Result<Vec<String>, DomainError>;
}
