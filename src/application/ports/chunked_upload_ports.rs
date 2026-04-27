//! Chunked Upload Port - Application layer abstraction for resumable chunked uploads.
//!
//! This module defines the port (trait) and DTOs for chunked/resumable upload
//! operations, keeping the application and interface layers independent of
//! the specific upload implementation (TUS-like protocol, S3 multipart, etc.).

use crate::common::errors::DomainError;
use bytes::Bytes;
use serde::Serialize;
use std::path::PathBuf;
use utoipa::ToSchema;
use uuid::Uuid;

/// Default chunk size (5 MB) — optimised for parallel transfers.
pub const DEFAULT_CHUNK_SIZE: usize = 5 * 1024 * 1024;

/// Minimum file size to use chunked upload (10 MB).
pub const CHUNKED_UPLOAD_THRESHOLD: usize = 10 * 1024 * 1024;

/// Response returned when a new upload session is created.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CreateUploadResponseDto {
    pub upload_id: String,
    pub chunk_size: usize,
    pub total_chunks: usize,
    pub expires_at: u64,
}

/// Response returned after a single chunk is uploaded.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ChunkUploadResponseDto {
    pub chunk_index: usize,
    pub bytes_received: u64,
    pub progress: f64,
    pub is_complete: bool,
}

/// Response for querying upload session status.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct UploadStatusResponseDto {
    pub upload_id: String,
    pub filename: String,
    pub total_size: u64,
    pub bytes_received: u64,
    pub progress: f64,
    pub total_chunks: usize,
    pub completed_chunks: usize,
    pub pending_chunks: Vec<usize>,
    pub is_complete: bool,
}

/// Port for chunked/resumable upload operations.
///
/// Implementations manage upload sessions, chunk storage, reassembly,
/// and cleanup, while the application layer only interacts through
/// this abstraction.
pub trait ChunkedUploadPort: Send + Sync + 'static {
    /// Create a new upload session.
    ///
    /// Returns session metadata including the upload ID, chunk size,
    /// total number of chunks, and expiration timestamp.
    async fn create_session(
        &self,
        user_id: Uuid,
        filename: String,
        folder_id: Option<String>,
        content_type: String,
        total_size: u64,
        chunk_size: Option<usize>,
    ) -> Result<CreateUploadResponseDto, DomainError>;

    /// Upload a single chunk.
    ///
    /// `checksum` is an optional MD5 hex string for integrity verification.
    async fn upload_chunk(
        &self,
        upload_id: &str,
        user_id: Uuid,
        chunk_index: usize,
        data: Bytes,
        checksum: Option<String>,
    ) -> Result<ChunkUploadResponseDto, DomainError>;

    /// Get the current status of an upload session.
    async fn get_status(
        &self,
        upload_id: &str,
        user_id: Uuid,
    ) -> Result<UploadStatusResponseDto, DomainError>;

    /// Assemble all chunks into the final file.
    ///
    /// Returns `(assembled_file_path, filename, folder_id, content_type, total_size, blake3_hash)`.
    /// The hash is computed during assembly (hash-on-write), eliminating a
    /// second sequential read of the assembled file.
    async fn complete_upload(
        &self,
        upload_id: &str,
        user_id: Uuid,
    ) -> Result<(PathBuf, String, Option<String>, String, u64, String), DomainError>;

    /// Finalize upload: clean up the session and temporary files.
    async fn finalize_upload(&self, upload_id: &str, user_id: Uuid) -> Result<(), DomainError>;

    /// Cancel an upload and clean up all temporary data.
    async fn cancel_upload(&self, upload_id: &str, user_id: Uuid) -> Result<(), DomainError>;

    /// Check if a file size qualifies for chunked upload.
    fn should_use_chunked(&self, size: u64) -> bool;
}
