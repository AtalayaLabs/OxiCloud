//! Thumbnail Port - Application layer abstraction for thumbnail generation.
//!
//! This module defines the port (trait) for thumbnail operations,
//! keeping the application and interface layers independent of specific
//! image processing implementations.

use crate::common::errors::DomainError;
use bytes::Bytes;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Thumbnail sizes supported by the system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThumbnailSize {
    /// Small icon for file listings (150×150)
    Icon,
    /// Medium preview for gallery view (400×400)
    Preview,
    /// Large preview for detail view (800×800)
    Large,
}

impl ThumbnailSize {
    /// Stable name, byte-identical to the derived `Debug` output. Used by
    /// the thumbnail/preview ETags on the hottest revalidation path — a
    /// `&'static str` push beats routing through the `Debug` machinery
    /// (benches/ROUND11.md §7) while keeping every already-cached client
    /// ETag valid.
    pub fn as_str(self) -> &'static str {
        match self {
            ThumbnailSize::Icon => "Icon",
            ThumbnailSize::Preview => "Preview",
            ThumbnailSize::Large => "Large",
        }
    }

    /// Get the maximum dimension for this size.
    pub fn max_dimension(&self) -> u32 {
        match self {
            ThumbnailSize::Icon => 150,
            ThumbnailSize::Preview => 400,
            ThumbnailSize::Large => 800,
        }
    }

    /// Get the directory name for this size.
    pub fn dir_name(&self) -> &'static str {
        match self {
            ThumbnailSize::Icon => "icon",
            ThumbnailSize::Preview => "preview",
            ThumbnailSize::Large => "large",
        }
    }

    /// Get all thumbnail sizes.
    pub fn all() -> &'static [ThumbnailSize] {
        &[
            ThumbnailSize::Icon,
            ThumbnailSize::Preview,
            ThumbnailSize::Large,
        ]
    }
}

/// Output encoding of a generated thumbnail.
///
/// WebP (lossy) is the primary format — ~25-30% smaller than JPEG at equal
/// quality — generated eagerly on upload and served to the ~97% of clients that
/// advertise `Accept: image/webp`. JPEG is the fallback for older clients and
/// NextCloud, generated lazily on first request and then cached like WebP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThumbnailFormat {
    /// Lossy WebP — primary, eager.
    Webp,
    /// Baseline JPEG — fallback for non-WebP clients, lazy.
    Jpeg,
}

impl ThumbnailFormat {
    /// Stable name, byte-identical to the derived `Debug` output (see
    /// [`ThumbnailSize::as_str`] — same ETag-stability contract).
    pub fn as_str(self) -> &'static str {
        match self {
            ThumbnailFormat::Webp => "Webp",
            ThumbnailFormat::Jpeg => "Jpeg",
        }
    }

    /// On-disk file extension for this format (no dot).
    pub fn ext(self) -> &'static str {
        match self {
            ThumbnailFormat::Webp => "webp",
            ThumbnailFormat::Jpeg => "jpg",
        }
    }

    /// Media type, for `content_derived_blobs.content_type` and for any
    /// response serving these bytes.
    ///
    /// Beside `ext` deliberately: the two must agree, and an extension
    /// without a matching media type is how a WebP ends up labelled JPEG.
    pub fn mime(self) -> &'static str {
        match self {
            ThumbnailFormat::Webp => "image/webp",
            ThumbnailFormat::Jpeg => "image/jpeg",
        }
    }

    /// Pick the output format from a request `Accept` header: WebP when the
    /// client advertises `image/webp`, JPEG otherwise. A plain substring check
    /// is sufficient — no client sends `image/webp;q=0`, and every WebP-capable
    /// browser lists it explicitly.
    pub fn from_accept(accept: Option<&str>) -> Self {
        match accept {
            Some(a) if a.contains("image/webp") => ThumbnailFormat::Webp,
            _ => ThumbnailFormat::Jpeg,
        }
    }
}

/// Statistics about the thumbnail cache.
#[derive(Debug, Clone)]
pub struct ThumbnailStatsDto {
    pub cached_thumbnails: usize,
    pub cache_size_bytes: usize,
    pub max_cache_bytes: usize,
}

/// Port for thumbnail generation and retrieval.
///
/// Implementations handle the actual image processing, caching,
/// and storage of thumbnails, while the application layer only
/// interacts through this abstraction.
pub trait ThumbnailPort: Send + Sync + 'static {
    /// Check if a file is an image that can have thumbnails.
    fn is_supported_image(&self, mime_type: &str) -> bool;

    /// Get a thumbnail, generating it on-demand if needed.
    ///
    /// `blob_hash` is the content hash used as the disk storage key
    /// (dedup: identical blobs share one set of thumbnails).
    async fn get_thumbnail(
        &self,
        file_id: &str,
        blob_hash: &str,
        size: ThumbnailSize,
        original_path: &Path,
    ) -> Result<Bytes, DomainError>;

    /// Generate all thumbnail sizes for a file in the background.
    ///
    /// `blob_hash` is the content hash used as the disk storage key.
    /// If thumbnails already exist for this hash, only the moka cache
    /// is populated (zero CPU for image processing).
    fn generate_all_sizes_background(
        self: Arc<Self>,
        file_id: String,
        blob_hash: String,
        original_path: PathBuf,
    );

    /// Delete all thumbnails for a file.
    async fn delete_thumbnails(&self, file_id: &str) -> Result<(), DomainError>;

    /// Try to get a cached thumbnail without generating one.
    ///
    /// Returns `None` if no cached thumbnail exists on disk or in memory.
    /// `blob_hash` is used to locate the file on disk. If `None`, only
    /// the in-memory moka cache is checked.
    async fn get_cached_thumbnail(
        &self,
        file_id: &str,
        blob_hash: Option<&str>,
        size: ThumbnailSize,
    ) -> Option<Bytes>;

    /// Store an externally-generated thumbnail (e.g. client-side video frame).
    ///
    /// Validates the image and persists it as JPEG (external/video thumbnails
    /// are kept JPEG-only — a tiny, non-dedup-able slice not worth a second codec).
    async fn store_external_thumbnail(
        &self,
        file_id: &str,
        size: ThumbnailSize,
        data: Bytes,
    ) -> Result<Bytes, DomainError>;

    /// Get cache statistics.
    async fn get_stats(&self) -> ThumbnailStatsDto;
}
