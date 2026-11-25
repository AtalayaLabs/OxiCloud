//! DTOs for pre-upload subtree comparison (`POST /api/folders/{id}/upload-preflight`).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::application::dtos::favorites_dto::ItemKind;

/// Maximum number of relative paths accepted in one preflight call.
///
/// The frontend splits larger folder drops into bounded chunks of 1 000 paths.
pub const MAX_PREFLIGHT_PATHS: usize = 2000;

/// Maximum directory nesting depth accepted in a preflight relative path.
pub const MAX_PREFLIGHT_DEPTH: usize = 64;

/// Maximum byte length of a single relative path string.
pub const MAX_PREFLIGHT_PATH_LEN: usize = 4096;

/// Request body of `POST /api/folders/{id}/upload-preflight`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UploadPreflightRequest {
    /// Relative paths of files to upload (e.g. `"report.pdf"`, `"Photos/2026/img.jpg"`).
    /// Leading/trailing slashes, backslashes, `.` / `..` segments and null bytes are rejected.
    pub paths: Vec<String>,
}

/// An existing folder already present under the target directory.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PreflightExistingFolderDto {
    /// Relative path matching the incoming directory prefix (NFC-normalized).
    pub relative_path: String,
    /// Existing folder ID — reused directly instead of calling `POST /api/folders`.
    pub id: String,
}

/// Metadata for an existing live file colliding with an incoming upload path.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PreflightExistingFileDto {
    /// Relative path of the incoming file (NFC-normalized).
    pub relative_path: String,
    /// Existing file ID (used as the target for `Replace` / `PUT /api/files/{id}/content`).
    pub id: String,
    /// Parent folder ID holding the existing file.
    pub folder_id: String,
    /// Basename of the existing file (NFC-normalized).
    pub name: String,
    /// Stored content size in bytes.
    pub size: u64,
    /// Content modification timestamp (`content_modified_at`, RFC 3339 UTC).
    pub modified_at: DateTime<Utc>,
    /// BLAKE3 whole-file content hash (64 hex chars).
    pub content_hash: String,
    /// Whether the caller holds `Permission::Update` on this file (false for
    /// e.g. `Contributor` roles or read-only drives).
    pub can_update: bool,
}

/// A cross-kind collision at an incoming path: either an incoming file wants
/// the name of an existing folder, or an incoming directory segment wants the
/// name of an existing file.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PreflightKindConflictDto {
    /// Relative path where the kind mismatch occurs (NFC-normalized).
    pub relative_path: String,
    /// Kind of the existing server item blocking that path (`file` or `folder`).
    pub existing_kind: ItemKind,
    /// ID of the existing server item.
    pub existing_id: String,
}

/// Response body of `POST /api/folders/{id}/upload-preflight`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UploadPreflightResponse {
    /// `true` for native PostgreSQL folders; `false` for external mounts
    /// (`ext:` / mount root) where subtree preflight is unsupported and the
    /// client falls back gracefully.
    pub supported: bool,
    /// Existing subfolders matching directory prefixes of `paths`.
    pub existing_folders: Vec<PreflightExistingFolderDto>,
    /// Existing live files colliding with incoming file paths.
    pub existing_files: Vec<PreflightExistingFileDto>,
    /// File↔folder kind mismatches preventing direct creation or overwrite.
    pub kind_conflicts: Vec<PreflightKindConflictDto>,
}
