use std::sync::Arc;

use crate::domain::entities::file::File;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::display_helpers::{classify_display, format_file_size, intern_display, intern_mime};

/// DTO for file responses
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FileDto {
    /// File ID
    pub id: String,

    /// File name
    pub name: String,

    /// Path to the file (relative)
    pub path: String,

    /// Size in bytes
    pub size: u64,

    /// MIME type — `Arc<str>` because MIME values repeat across files
    /// and DTOs are cloned on every request (clone is O(1) atomic increment).
    #[schema(value_type = String)]
    pub mime_type: Arc<str>,

    /// Parent folder ID
    pub folder_id: Option<String>,

    /// Creation timestamp
    pub created_at: u64,

    /// Last modification timestamp
    pub modified_at: u64,

    // ── Pre-computed display fields (Arc<str>: values come from static tables) ──
    /// FontAwesome icon CSS class (e.g. "fas fa-file-image")
    #[schema(value_type = String)]
    pub icon_class: Arc<str>,

    /// Extra CSS class for icon styling (e.g. "image-icon", "" when default)
    #[schema(value_type = String)]
    pub icon_special_class: Arc<str>,

    /// Human-readable file category (e.g. "Image", "Document")
    #[schema(value_type = String)]
    pub category: Arc<str>,

    /// Human-readable formatted size (e.g. "3.27 MB")
    pub size_formatted: String,

    /// Sort date for Photos timeline — COALESCE(EXIF captured_at, created_at).
    /// Only populated by the /api/photos endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_date: Option<u64>,

    /// Raw BLAKE3 content hash. Populated from `File::content_hash()`.
    /// Exposed in REST JSON so API consumers can use it for
    /// content-addressable URLs, dedup verification, and integrity
    /// audits. Distinct from `etag` (which is an HTTP-only cache
    /// token whose formula may grow to include `modified_at` etc.).
    pub content_hash: String,

    /// Opaque HTTP ETag. Populated from `File::etag()`. Used by
    /// WebDAV/NextCloud handlers when emitting `ETag` headers and
    /// also exposed in REST JSON so frontends can pass it back
    /// through `If-Match` / `If-None-Match` on download / mutation
    /// endpoints without a separate HEAD round-trip.
    pub etag: String,

    /// §14 provenance: user that originally created this file.
    /// `None` when the referenced user has been deleted (FK is
    /// `ON DELETE SET NULL`) or for stub/legacy files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<Uuid>,

    /// §14 provenance: user that performed the most recent mutation
    /// that bumped `updated_at`. Authorship signal — distinct from
    /// `owner_id`. `None` when the referenced user is deleted or for
    /// stub/legacy files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<Uuid>,

    /// Caller-scoped: `true` when the requesting user has favorited
    /// this file. **Wire contract: always present**, never null and
    /// never absent — the SPA reads it as a required `boolean` with no
    /// nullish branch. Every emission path (listing endpoints inline
    /// via a per-row `EXISTS` in the listing SQL; single-item endpoints
    /// via the shared `caller_flags` helper on the favorites port) is
    /// responsible for populating this before the DTO reaches the
    /// wire. WebDAV/CalDAV/CardDAV DTOs default to `false` — the XML
    /// property serializer drops the field entirely, so a stale
    /// default is never observable on those surfaces.
    pub is_favorite: bool,

    /// Resource-scoped: `true` when the file has ANY explicit
    /// role-grant on it (link share via `subject_type = 'token'`,
    /// user/group grant, any role). "Someone was given access to
    /// this beyond drive membership." Same wire contract as
    /// `is_favorite` — always present.
    pub is_shared: bool,
}

impl From<File> for FileDto {
    fn from(file: File) -> Self {
        // Compute the HTTP ETag BEFORE consuming the entity —
        // `File::etag()` derives from `blob_hash` + `modified_at`,
        // so it must run against the live entity, not against
        // already-extracted parts. `content_hash` is just the raw
        // blob hash; `etag` is the cache token derived from it.
        let etag = file.etag();

        // Consume the entity by moving all fields — zero heap allocations
        // for id, name, path, folder_id (previously 4× .to_string()), and now
        // for `content_hash` too: `into_parts()` moves `blob_hash` out, so it is
        // reused verbatim below instead of cloning it through the
        // `content_hash()` getter. The moved `parts.blob_hash` was previously
        // dropped unused while the getter clone paid 1 alloc/row on every file
        // listing (folder browse, streaming PROPFIND, search/favorites/recent
        // hydration). `etag` is still computed first from the live entity.
        let parts = file.into_parts();

        // Display fields come from closed static tables and MIME values
        // repeat massively across rows — intern instead of allocating a
        // fresh Arc<str> per row (`Arc::from(&str)` always allocs+copies).
        let classes = classify_display(&parts.name, &parts.mime_type);
        let icon_class = intern_display(classes.icon_class);
        let icon_special_class = intern_display(classes.icon_special_class);
        let category = intern_display(classes.category);
        let size_formatted = format_file_size(parts.size);
        let mime_type = intern_mime(&parts.mime_type);

        Self {
            id: parts.id,
            name: parts.name,
            path: parts.storage_path.into_joined(),
            size: parts.size,
            mime_type,
            folder_id: parts.folder_id,
            created_at: parts.created_at,
            modified_at: parts.modified_at,
            icon_class,
            icon_special_class,
            category,
            size_formatted,
            sort_date: None,
            content_hash: parts.blob_hash,
            etag,
            created_by: parts.created_by,
            updated_by: parts.updated_by,
            // `From<File>` has no caller context. Callers that will
            // emit the DTO to the SPA MUST override these before
            // Json emission via the `caller_flags` helper.
            is_favorite: false,
            is_shared: false,
        }
    }
}

// To convert from FileDto to File for batch handlers
impl From<FileDto> for File {
    fn from(dto: FileDto) -> Self {
        // Display fields (icon_class, icon_special_class, category, size_formatted)
        // are not part of the domain entity and are ignored.
        File::from_dto(
            dto.id,
            dto.name,
            dto.path,
            dto.size,
            dto.mime_type.to_string(),
            dto.folder_id,
            dto.created_at,
            dto.modified_at,
        )
    }
}

impl FileDto {
    /// Returns a copy of this DTO with the `path` field cleared.
    ///
    /// Used when a file is returned to a share recipient: `path` reveals the
    /// full folder hierarchy above the file which the recipient may not have
    /// access to.  `folder_id` is intentionally kept — it's needed for
    /// sub-folder navigation (covered by the cascade grant).
    #[must_use]
    pub fn without_hierarchy_info(self) -> Self {
        Self {
            path: String::new(),
            ..self
        }
    }

    /// Everything withheld from a caller holding only a share token — the
    /// file counterpart of `FolderDto::redacted_for_token`. See that doc for
    /// why the owner's stable identifiers come off for an anonymous visitor
    /// but not for a signed-in share recipient.
    #[must_use]
    pub fn redacted_for_token(self) -> Self {
        Self {
            created_by: None,
            updated_by: None,
            ..self.without_hierarchy_info()
        }
    }

    /// Creates an empty file DTO for stub implementations
    pub fn empty() -> Self {
        Self {
            id: "stub-id".to_string(),
            name: "stub-file".to_string(),
            path: "/stub/path".to_string(),
            size: 0,
            mime_type: intern_mime("application/octet-stream"),
            folder_id: None,
            created_at: 0,
            modified_at: 0,
            icon_class: intern_display("fas fa-file"),
            icon_special_class: intern_display(""),
            category: intern_display("Document"),
            size_formatted: "0 Bytes".to_string(),
            content_hash: String::new(),
            etag: String::new(),
            sort_date: None,
            created_by: None,
            updated_by: None,
            is_favorite: false,
            is_shared: false,
        }
    }
}

impl Default for FileDto {
    fn default() -> Self {
        Self::empty()
    }
}
