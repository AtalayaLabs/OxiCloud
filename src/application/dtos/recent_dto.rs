use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::cursor::{CursorListResponse, CursorQuery, PageCursor};
use super::display_helpers::{classify_display, format_file_size};
use super::favorites_dto::ItemKind;
use super::grant_dto::{ResourceContentDto, ResourceTypeDto};
use crate::domain::services::authorization::ResourceKind;

/// DTO for recent items, enriched with item metadata via SQL JOIN
/// so the frontend does not need N+1 requests to resolve names/sizes.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RecentItemDto {
    /// Unique identifier for the recent item
    pub id: String,

    /// Owner user ID
    pub user_id: String,

    /// Item ID (file or folder)
    pub item_id: String,

    /// Item type. Shared enum with `FavoriteItemDto.item_type` — both
    /// collections record a reference to one of the two leaf resource
    /// kinds, nothing else.
    pub item_type: ItemKind,

    /// When the item was accessed
    pub accessed_at: DateTime<Utc>,

    // ── Enriched metadata (resolved via JOIN) ──
    /// Display name of the file or folder
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_name: Option<String>,

    /// Size in bytes (files only; folders → None)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_size: Option<i64>,

    /// MIME type (files only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_mime_type: Option<String>,

    /// Parent folder ID (folder_id for files, parent_id for folders)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,

    /// Full human-readable path (e.g. "Documents/Work" for a folder,
    /// "Documents/Work/report.pdf" for a file)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_path: Option<String>,

    // ── Pre-computed display fields ──
    /// FontAwesome icon CSS class (e.g. "fas fa-file-image", "fas fa-folder")
    pub icon_class: String,

    /// Extra CSS class for icon styling (e.g. "image-icon", "folder-icon")
    pub icon_special_class: String,

    /// Human-readable category (e.g. "Image", "Folder")
    pub category: String,

    /// Formatted file size (e.g. "3.27 MB"); "--" for folders
    pub size_formatted: String,
}

impl RecentItemDto {
    /// Populate display fields from the enriched metadata.
    /// Call this after constructing from the SQL row.
    pub fn with_display_fields(mut self) -> Self {
        if matches!(self.item_type, ItemKind::Folder) {
            self.icon_class = "fas fa-folder".to_string();
            self.icon_special_class = "folder-icon".to_string();
            self.category = "Folder".to_string();
            self.size_formatted = "--".to_string();
        } else {
            let name = self.item_name.as_deref().unwrap_or("");
            let mime = self
                .item_mime_type
                .as_deref()
                .unwrap_or("application/octet-stream");
            let classes = classify_display(name, mime);
            self.icon_class = classes.icon_class.to_string();
            self.icon_special_class = classes.icon_special_class.to_string();
            self.category = classes.category.to_string();
            self.size_formatted = format_file_size(self.item_size.unwrap_or(0) as u64);
        }
        self
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Cursor-paginated recent resources  (GET /api/recent/resources)
// ════════════════════════════════════════════════════════════════════════════

/// Raw row returned by the UNION ALL query for `/api/recent/resources`.
pub struct RecentResourceRow {
    pub resource_type: String, // "file" | "folder"
    pub resource_id: Uuid,
    pub name: String,
    pub parent_id: Option<Uuid>,
    pub mime_type: Option<String>,
    /// `-1` for folders, actual byte-count for files.
    pub size: i64,
    pub resource_created_at: DateTime<Utc>,
    pub modified_at: DateTime<Utc>,
    /// Drive that owns this row. Surfaced on the recent listing
    /// so a UI can tell when a recently-accessed item lives in a
    /// different drive than the user's home (post-D6 cross-drive
    /// moves + copies make this reachable).
    pub drive_id: Uuid,
    /// Raw BLAKE3 content hash. `Some(_)` for file rows, `None` for
    /// folder rows. Feeds `File::compute_etag` so this listing's
    /// `etag` matches GET/HEAD/PROPFIND for the same file.
    pub blob_hash: Option<String>,
    /// §14 provenance — who created the row. `None` when the creator
    /// was deleted (FK `ON DELETE SET NULL`). Powers the owner column
    /// on the `/recent` UI (aligned with `/files` and `/favorites`
    /// for cross-surface consistency, rather than the finer-grained
    /// but noisier "who touched this last" signal).
    pub created_by: Option<Uuid>,
    /// §14 provenance — who last touched the row. Not currently
    /// consumed by the UI but surfaced for API parity with the other
    /// listing endpoints.
    pub updated_by: Option<Uuid>,
    /// Caller-scoped: `true` when the requesting user has favorited
    /// this row. Populates `FileDto::is_favorite` /
    /// `FolderDto::is_favorite` on this listing via a per-row
    /// `EXISTS` in the SQL.
    pub is_favorite: bool,
    /// Resource-scoped: `true` when the row has any
    /// `storage.role_grants` entry.
    pub is_shared: bool,
    /// `true` when `owner_id == requesting user_id`.
    pub is_owner: bool,
    pub accessed_at: DateTime<Utc>,
    /// Human-readable path. Always populated in the row; the handler clears it
    /// to `""` when `is_owner` is false.
    pub path: Option<String>,
    // Pre-computed sort fields for cursor construction.
    pub sort_str: Option<String>,
    pub sort_int: Option<i64>,
    pub sort_ts: Option<DateTime<Utc>>,
}

/// Opaque keyset-pagination cursor for `GET /api/recent/resources`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentCursor {
    /// Sort dimension active when this cursor was produced.
    /// Values: `"accessed_at"` (default), `"name"`, `"type"`, `"modified_at"`, `"size"`, `"owner"`.
    #[serde(default = "RecentCursor::default_order")]
    pub order_by: String,
    /// UUID of the last item on the previous page (tie-breaker).
    pub resource_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_str: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_int: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_ts: Option<DateTime<Utc>>,
    #[serde(default)]
    pub reverse: bool,
}

impl RecentCursor {
    fn default_order() -> String {
        "accessed_at".to_owned()
    }
}

impl PageCursor for RecentCursor {}

/// Query parameters for `GET /api/recent/resources`.
#[derive(Debug, Deserialize, IntoParams)]
pub struct RecentResourcesQuery {
    /// Maximum items per page (1–200, default 50).
    #[serde(default = "CursorQuery::default_limit")]
    pub limit: u32,
    /// Opaque cursor from a previous response. Omit to start from the first page.
    pub cursor: Option<String>,
    /// Sort / group-by dimension. Supported: `"accessed_at"` (default), `"name"`,
    /// `"type"`, `"modified_at"`, `"size"`, `"owner"`.
    pub order_by: Option<String>,
    /// Comma-separated resource types to include, e.g. `"file,folder"`.
    /// Omit to include both.
    pub resource_types: Option<String>,
    /// Reverse the sort order. Default `false`.
    #[serde(default)]
    pub reverse: bool,
}

impl RecentResourcesQuery {
    pub fn limit_clamped(&self) -> usize {
        self.limit.clamp(1, 200) as usize
    }

    pub fn decode_cursor(&self) -> Option<RecentCursor> {
        self.cursor.as_deref().and_then(RecentCursor::decode)
    }

    /// Returns `None` when `resource_types` is absent (= include all).
    pub fn resource_kinds(&self) -> Option<Vec<ResourceKind>> {
        self.resource_types.as_deref().map(|s| {
            s.split(',')
                .filter_map(|t| ResourceKind::parse(t.trim()))
                .collect()
        })
    }
}

/// One item in a `GET /api/recent/resources` page.
#[derive(Debug, Serialize, ToSchema)]
pub struct RecentResourceItemDto {
    pub resource_type: ResourceTypeDto,
    /// When the resource was last accessed.
    pub accessed_at: DateTime<Utc>,
    /// Full resource details — shape determined by `resource_type`.
    pub resource: ResourceContentDto,
}

/// Response envelope for `GET /api/recent/resources`.
pub type RecentResourcesDto = CursorListResponse<RecentResourceItemDto>;
