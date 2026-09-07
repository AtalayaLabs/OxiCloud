//! Image Transcode Port - Application layer abstraction for image transcoding.
//!
//! This module defines the port (trait) for on-demand image format conversion
//! (e.g., JPEG/PNG → WebP), keeping the application and interface layers
//! independent of specific image processing implementations.

use crate::common::errors::DomainError;
use bytes::Bytes;

/// Supported output formats for image transcoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputFormat {
    /// WebP format — best current browser support with good compression.
    WebP,
    // Future: Avif, JpegXl
}

impl OutputFormat {
    /// Get the file extension for this format.
    pub fn extension(&self) -> &'static str {
        match self {
            OutputFormat::WebP => "webp",
        }
    }

    /// Get the MIME type for this format.
    pub fn mime_type(&self) -> &'static str {
        match self {
            OutputFormat::WebP => "image/webp",
        }
    }
}

/// Browser image format capabilities detected from the Accept header.
#[derive(Debug)]
pub struct BrowserCapabilities {
    pub supports_webp: bool,
    pub supports_avif: bool,
}

impl BrowserCapabilities {
    /// Parse the HTTP Accept header to determine browser image format support.
    pub fn from_accept_header(accept: Option<&str>) -> Self {
        let accept = accept.unwrap_or("");
        Self {
            supports_webp: accept.contains("image/webp"),
            supports_avif: accept.contains("image/avif"),
        }
    }

    /// Get the best output format supported by the browser.
    pub fn best_format(&self) -> Option<OutputFormat> {
        if self.supports_webp {
            Some(OutputFormat::WebP)
        } else {
            None
        }
    }
}

/// Statistics about transcoding operations.
#[derive(Debug, Default, Clone)]
pub struct TranscodeStatsDto {
    pub cache_hits: u64,
    pub disk_hits: u64,
    pub transcodes: u64,
    pub bytes_saved: u64,
    pub transcode_errors: u64,
}

/// Port for image transcoding operations.
///
/// Implementations handle the actual image conversion, caching,
/// and format detection, while the application layer only interacts
/// through this abstraction.
pub trait ImageTranscodePort: Send + Sync + 'static {
    /// Check if a MIME type can be transcoded.
    fn can_transcode(&self, mime_type: &str) -> bool;

    /// Check if transcoding should be attempted based on file size and type.
    fn should_transcode(&self, mime_type: &str, file_size: u64) -> bool;

    /// Get a transcoded version of an image.
    ///
    /// Returns `(content, mime_type, was_transcoded)`.
    /// If transcoding is not beneficial (output larger than input), returns the
    /// original content with `was_transcoded = false`.
    ///
    /// `source_hash` is the BLAKE3 of the original content, which is how the
    /// durable derived tier is keyed. `None` restricts the implementation to
    /// its local cache — correct for callers with no hash (external mounts),
    /// and the behaviour of every caller before that tier existed.
    async fn get_transcoded(
        &self,
        file_id: &str,
        source_hash: Option<&str>,
        original_content: Bytes,
        original_mime: &str,
        target_format: OutputFormat,
    ) -> Result<(Bytes, String, bool), DomainError>;

    /// Invalidate cached transcodes for a file.
    async fn invalidate(&self, file_id: &str);

    /// Get transcoding statistics.
    async fn get_stats(&self) -> TranscodeStatsDto;

    /// Clear all caches.
    async fn clear_cache(&self) -> Result<(), DomainError>;
}
