use std::sync::Arc;

use crate::domain::entities::file::File;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::display_helpers::{classify_display, format_file_size, intern_display, intern_mime};
use super::timestamp_helpers::{datetime_to_epoch, epoch_to_datetime, epoch_zero};

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

    /// Creation timestamp (RFC 3339 UTC).
    pub created_at: DateTime<Utc>,

    /// Last modification timestamp (RFC 3339 UTC).
    pub modified_at: DateTime<Utc>,

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
            // `File` keeps timestamps as epoch seconds internally
            // (ETag input, cheap compare); on the wire the DTO is
            // RFC 3339 UTC — convert once at the DTO boundary.
            created_at: epoch_to_datetime(parts.created_at),
            modified_at: epoch_to_datetime(parts.modified_at),
            icon_class,
            icon_special_class,
            category,
            size_formatted,
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
        // Domain entity keeps timestamps as epoch seconds; DTO holds
        // RFC 3339 UTC. See `timestamp_helpers` for the rationale.
        File::from_dto(
            dto.id,
            dto.name,
            dto.path,
            dto.size,
            dto.mime_type.to_string(),
            dto.folder_id,
            datetime_to_epoch(dto.created_at),
            datetime_to_epoch(dto.modified_at),
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
            created_at: epoch_zero(),
            modified_at: epoch_zero(),
            icon_class: intern_display("fas fa-file"),
            icon_special_class: intern_display(""),
            category: intern_display("Document"),
            size_formatted: "0 Bytes".to_string(),
            content_hash: String::new(),
            etag: String::new(),
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

#[cfg(test)]
mod wire_shape_tests {
    //! Byte-level snapshot of the FileDto JSON wire shape. Freezes the
    //! ISO-8601 timestamp contract: `created_at` / `modified_at` must emit
    //! as RFC 3339 UTC strings, never as integer epoch seconds. Trip-wire
    //! for any future accidental re-flip of the timestamp serializer.

    use super::*;

    fn fixture() -> FileDto {
        let mut dto = FileDto::empty();
        dto.id = "11111111-1111-1111-1111-111111111111".to_string();
        dto.name = "report.pdf".to_string();
        dto.path = "Documents/report.pdf".to_string();
        dto.size = 1024;
        dto.folder_id = Some("22222222-2222-2222-2222-222222222222".to_string());
        dto.created_at = epoch_to_datetime(1_700_000_000);
        dto.modified_at = epoch_to_datetime(1_700_086_400);
        dto.content_hash = "abcdef0123456789".to_string();
        dto.etag = "abcdef0123456789-1700086400".to_string();
        dto
    }

    #[test]
    fn timestamps_serialize_as_rfc3339_utc_strings() {
        let json = serde_json::to_value(fixture()).unwrap();
        // Byte-exact: no sub-second noise, trailing `Z`, 20-char RFC 3339.
        assert_eq!(json["created_at"], "2023-11-14T22:13:20Z");
        assert_eq!(json["modified_at"], "2023-11-15T22:13:20Z");
    }

    #[test]
    fn timestamps_round_trip_through_wire() {
        let before = fixture();
        let json = serde_json::to_string(&before).unwrap();
        let after: FileDto = serde_json::from_str(&json).unwrap();
        assert_eq!(before.created_at, after.created_at);
        assert_eq!(before.modified_at, after.modified_at);
    }

    #[test]
    fn empty_dto_timestamps_are_epoch_zero() {
        let json = serde_json::to_value(FileDto::empty()).unwrap();
        assert_eq!(json["created_at"], "1970-01-01T00:00:00Z");
        assert_eq!(json["modified_at"], "1970-01-01T00:00:00Z");
    }
}
