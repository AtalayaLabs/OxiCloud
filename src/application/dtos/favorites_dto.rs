use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::cursor::{CursorListResponse, CursorQuery, PageCursor};
use super::display_helpers::{classify_display, format_file_size};
use super::grant_dto::{ResourceContentDto, ResourceTypeDto};
use crate::domain::services::authorization::ResourceKind;

/// What a favorite points at. The server emits exactly these two values
/// today; the dedicated enum (rather than the 6-value `ResourceTypeDto`)
/// matches reality — favorites hold a file or a folder, nothing else —
/// so clients don't have to carry impossible branches for drives,
/// calendars, address books or playlists. If a future product decision
/// permits favoriting one of those, add the variant here and the TS SDK
/// surfaces it on regen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    File,
    Folder,
}

impl std::str::FromStr for ItemKind {
    type Err = String;

    /// Parse the DB / wire spelling. Matches the DB CHECK constraint on
    /// `favorites.item_type` and the handwritten FE `ItemType` union.
    /// Unknown values are a bug — the column has a CHECK and the enum
    /// variants are the source of truth.
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "file" => Ok(ItemKind::File),
            "folder" => Ok(ItemKind::Folder),
            other => Err(format!(
                "unexpected favorite item_type: {other} (expected 'file' or 'folder')"
            )),
        }
    }
}

/// DTO for favorites item, enriched with item metadata via SQL JOIN
/// so the frontend does not need N+1 requests to resolve names/sizes.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FavoriteItemDto {
    /// Unique identifier for the favorite entry
    pub id: String,

    /// User ID who owns this favorite
    pub user_id: String,

    /// ID of the favorited item (file or folder)
    pub item_id: String,

    /// Type of the item ('file' or 'folder'). Typed enum rather than a
    /// raw string — the CHECK constraint on `favorites.item_type` enforces
    /// the same invariant, and the TS SDK gets exhaustive narrowing on
    /// the two values.
    pub item_type: ItemKind,

    /// When the item was added to favorites
    pub created_at: DateTime<Utc>,

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

    /// Last modification timestamp of the item
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<DateTime<Utc>>,

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

impl FavoriteItemDto {
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

/// Result DTO for batch add-to-favorites.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BatchFavoritesResult {
    /// Statistics about the batch operation
    pub stats: BatchFavoritesStats,
    /// Full list of the user's favourites (enriched), so the client can
    /// replace its local cache in a single round-trip.
    pub favorites: Vec<FavoriteItemDto>,
}

// ════════════════════════════════════════════════════════════════════════════
// Cursor-paginated favorites resources  (GET /api/favorites/resources)
// ════════════════════════════════════════════════════════════════════════════

/// Raw row returned by the UNION ALL query that joins `auth.user_favorites`
/// with `storage.files` / `storage.folders`.  Never serialised directly.
pub struct FavoriteResourceRow {
    pub resource_type: String, // "file" | "folder"
    pub resource_id: Uuid,
    pub name: String,
    pub parent_id: Option<Uuid>,
    pub mime_type: Option<String>,
    /// `-1` for folders, actual byte-count for files.
    pub size: i64,
    pub resource_created_at: DateTime<Utc>,
    pub modified_at: DateTime<Utc>,
    /// Drive that owns this row. Surfaced on the favorites listing
    /// so a UI can tell when a favorited item lives in a different
    /// drive than the user's home (post-D6 cross-drive moves +
    /// copies make this reachable).
    pub drive_id: Uuid,
    /// Raw BLAKE3 content hash. `Some(_)` for file rows, `None` for
    /// folder rows. Routes into `FileDto::content_hash` and feeds
    /// `File::compute_etag` to populate `FileDto::etag`.
    pub blob_hash: Option<String>,
    /// §14 provenance — who created the row. `None` when the creator
    /// was deleted (FK `ON DELETE SET NULL`).
    pub created_by: Option<Uuid>,
    /// §14 provenance — who last touched the row.
    pub updated_by: Option<Uuid>,
    /// Caller-scoped favorite state — `TRUE` by construction on this
    /// listing (every row IS a favorite). The listing SQL hardcodes
    /// `TRUE AS is_favorite`; this field stays here so the DTO builder
    /// signature stays symmetric with the other listings.
    pub is_favorite: bool,
    /// Resource-scoped: `true` when the row has any `storage.role_grants`
    /// entry. Computed by a per-row `EXISTS` in the listing SQL.
    pub is_shared: bool,
    /// `true` when `owner_id == requesting user_id`.
    pub is_owner: bool,
    pub favorited_at: DateTime<Utc>,
    /// Human-readable path (e.g. `Documents/Work` for a folder,
    /// `Documents/Work/report.pdf` for a file).  Always populated; the
    /// handler clears it to `""` when `is_owner` is false.
    pub path: Option<String>,
    // Pre-computed sort fields for cursor construction.
    pub sort_str: Option<String>,
    pub sort_int: Option<i64>,
    pub sort_ts: Option<DateTime<Utc>>,
}

/// Sort axis for `GET /api/favorites/resources` (§7).
///
/// Typed rather than `Option<String>` so an unknown axis is refused at
/// wire-decode time instead of silently falling back to the default. See
/// [`crate::application::dtos::trash_dto::TrashOrderBy`] for why these are
/// per-endpoint enums rather than one shared union: the date axis is the
/// part that differs, and favorites' is `favorited_at`, which is
/// meaningless on every other endpoint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FavoritesOrderBy {
    /// Default — alphabetical, folders before files (`folder_first`).
    #[default]
    Name,
    /// Grouped by kind, then alphabetical within a kind.
    Type,
    /// When the caller starred it, most recent first.
    FavoritedAt,
    /// The item's content clock (`content_modified_at`) — a rename or
    /// move does not resurface a row, only a real content change.
    ModifiedAt,
    /// By byte size. Folders carry a sentinel so they cluster rather
    /// than interleaving at zero.
    Size,
    /// By owner display name — favorites span drives, so this groups a
    /// mixed list by who owns each item.
    Owner,
}

/// Opaque keyset-pagination cursor for `GET /api/favorites/resources`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FavoritesCursor {
    /// Sort dimension active when this cursor was produced. Typed so the
    /// cursor's axis and the query's axis are one type and cannot drift —
    /// the keyset predicate and the ORDER BY must name the same column or
    /// pagination skips or repeats rows.
    ///
    /// Wire-compatible with the previous `String` form: snake_case
    /// serialises to the identical token, so cursors minted before this
    /// change still decode.
    #[serde(default)]
    pub order_by: FavoritesOrderBy,
    /// UUID of the last item on the previous page (tie-breaker).
    pub resource_id: Uuid,
    /// `LOWER(name)` for `name`/`type` sorts; `LOWER(username)` for `owner`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_str: Option<String>,
    /// Multipurpose integer: `folder_first` for `name`, `type_order` for `type`,
    /// size in bytes for `size`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_int: Option<i64>,
    /// Timestamp for `favorited_at` and `modified_at` sorts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_ts: Option<DateTime<Utc>>,
    /// Whether the result set was reversed — must match on every page.
    #[serde(default)]
    pub reverse: bool,
}

impl PageCursor for FavoritesCursor {}

/// Query parameters for `GET /api/favorites/resources`.
#[derive(Debug, Deserialize, IntoParams)]
pub struct FavoritesResourcesQuery {
    /// Maximum items per page (1–200, default 50).
    #[serde(default = "CursorQuery::default_limit")]
    pub limit: u32,
    /// Opaque cursor from a previous response. Omit to start from the first page.
    pub cursor: Option<String>,
    /// Sort / group-by dimension — see [`FavoritesOrderBy`]. Omit for
    /// `name`. An unknown value is a 400 naming the accepted set, not a
    /// silent fall back to the default.
    pub order_by: Option<FavoritesOrderBy>,
    /// Comma-separated resource types to include, e.g. `"file,folder"`.
    /// Omit to include both.
    pub resource_types: Option<String>,
    /// Reverse the sort order. Default `false`.
    #[serde(default)]
    pub reverse: bool,
}

impl FavoritesResourcesQuery {
    pub fn limit_clamped(&self) -> usize {
        self.limit.clamp(1, 200) as usize
    }

    pub fn decode_cursor(&self) -> Option<FavoritesCursor> {
        self.cursor.as_deref().and_then(FavoritesCursor::decode)
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

/// One item in a `GET /api/favorites/resources` page.
#[derive(Debug, Serialize, ToSchema)]
pub struct FavoritesResourceItemDto {
    pub resource_type: ResourceTypeDto,
    /// When the resource was added to the user's favorites.
    pub favorited_at: DateTime<Utc>,
    /// Full resource details — shape determined by `resource_type`.
    pub resource: ResourceContentDto,
}

/// Response envelope for `GET /api/favorites/resources`.
pub type FavoritesResourcesDto = CursorListResponse<FavoritesResourceItemDto>;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BatchFavoritesStats {
    /// How many items were requested
    pub requested: usize,
    /// How many were actually inserted (new)
    pub inserted: u64,
    /// How many were already favourites (skipped)
    pub already_existed: u64,
}
