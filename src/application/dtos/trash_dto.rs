use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::cursor::{CursorListResponse, CursorQuery, PageCursor};
use super::grant_dto::{ResourceContentDto, ResourceTypeDto};
use crate::domain::services::authorization::ResourceKind;

/// DTO representing an item in the trash
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct TrashedItemDto {
    pub id: String,
    pub original_id: String,
    pub item_type: String, // "file" o "folder"
    pub name: String,
    pub original_path: String,
    pub trashed_at: DateTime<Utc>,
    pub days_until_deletion: i64,
    /// Content category — a stable machine key, not display text (e.g.
    /// "Image", "Folder", "Document"). See `FileDto::category`.
    pub category: String,
}

/// Request to move an item to trash
#[derive(Debug, Deserialize, ToSchema)]
pub struct MoveToTrashRequest {
    pub item_id: String,
    pub item_type: String, // "file" o "folder"
}

/// Request to restore an item from trash
#[derive(Debug, Deserialize, ToSchema)]
pub struct RestoreFromTrashRequest {
    pub trash_id: String,
}

/// Request to permanently delete an item from trash
#[derive(Debug, Deserialize, ToSchema)]
pub struct DeletePermanentlyRequest {
    pub trash_id: String,
}

// ════════════════════════════════════════════════════════════════════════════
// Cursor-paginated trash resources  (GET /api/trash/resources)
// ════════════════════════════════════════════════════════════════════════════

/// Raw row returned by the UNION-ALL query over `storage.files`/`storage.folders`
/// where `is_trashed = TRUE`. Never serialised directly.
pub struct TrashResourceRow {
    pub resource_type: String, // "file" | "folder"
    pub resource_id: Uuid,
    pub name: String,
    pub parent_id: Option<Uuid>,
    pub mime_type: Option<String>,
    /// `-1` for folders (sentinel), actual byte-count for files.
    pub size: i64,
    pub resource_created_at: DateTime<Utc>,
    pub modified_at: DateTime<Utc>,
    /// Drive the trashed item belongs to. Surfaced verbatim on the wire
    /// (`TrashResourceItemDto.drive_id`) so the `/trash` UI can group by
    /// drive without an extra lookup per row. D2b: filtering by drive is
    /// done in SQL via `WHERE drive_id = ANY($accessible_drive_ids)`.
    pub drive_id: Uuid,
    /// Raw BLAKE3 content hash. `Some(_)` for file rows, `None` for
    /// folder rows. Feeds `File::compute_etag` so the trash listing's
    /// `etag` matches what GET/HEAD/PROPFIND would return for the
    /// same file (restorable trash items are conditional-request
    /// targets too).
    pub blob_hash: Option<String>,
    /// §14 provenance — who created the row. `None` when the creator
    /// was deleted (FK `ON DELETE SET NULL`).
    pub created_by: Option<Uuid>,
    /// §14 provenance — who last touched the row (includes the trash
    /// action itself, which stamps `updated_by = caller_id`).
    pub updated_by: Option<Uuid>,
    /// Caller-scoped: `true` when the caller has favorited this
    /// trashed item.
    pub is_favorite: bool,
    /// Resource-scoped: `true` when the row has any
    /// `storage.role_grants` entry (surviving trash — grants are
    /// GC'd by `purge_expired_grants` on the 15-day grace).
    pub is_shared: bool,
    pub trashed_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// Original location path (for folders: `path`; for files: `parent.path || '/' || name`).
    pub path: Option<String>,
    // Pre-computed sort fields for cursor construction.
    pub sort_str: Option<String>,
    pub sort_int: Option<i64>,
    pub sort_ts: Option<DateTime<Utc>>,
}

/// Sort axis for `GET /api/trash/resources` (§7).
///
/// Typed rather than `Option<String>` so an unknown axis is refused at
/// wire-decode time instead of silently falling back to the default —
/// which is what a bare string did, turning a client typo into "sorted by
/// something else" with a 200. serde's derived error names the accepted
/// variants, so the 400 tells the caller what to send; no custom extractor
/// is involved. `PhotoOrderBy` set this precedent in §6.
///
/// **Per-endpoint by design, not one shared union.** `Name`/`Type`/`Size`
/// recur across endpoints but the date axes do not — trash has
/// `expires_at`/`trashed_at`, favorites has `favorited_at`, recent has
/// `accessed_at`. A shared enum would accept nonsense on every endpoint
/// but its own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TrashOrderBy {
    /// Default — soonest expiry first, so the items about to be purged
    /// are the ones a user sees without scrolling. Orders by the computed
    /// `expires_at` (`trashed_at` + retention), renamed from
    /// `deletion_date` in §7 part 2.
    #[default]
    ExpiresAt,
    /// Most recently trashed first — "what did I just delete".
    TrashedAt,
    /// Alphabetical, folders before files (the `folder_first` axis).
    Name,
    /// Grouped by kind, then alphabetical within a kind.
    Type,
    /// By byte size. Folders carry a `-1` sentinel so they cluster
    /// together rather than interleaving at zero.
    Size,
}

/// Opaque keyset-pagination cursor for `GET /api/trash/resources`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrashCursor {
    /// Sort dimension active when this cursor was produced. Typed, so the
    /// axis in a cursor and the axis in a query are the same type and
    /// cannot drift — the keyset predicate and the ORDER BY must name the
    /// same column or pagination silently skips or repeats rows.
    ///
    /// Wire-compatible with the previous `String` form: `rename_all =
    /// "snake_case"` serialises to the identical token, so cursors minted
    /// before this change still decode. The one exception is a cursor
    /// minted before part 2, carrying `"deletion_date"` — that now fails
    /// to decode and returns 400 instead of silently re-sorting the
    /// remaining pages, which is the better of the two.
    #[serde(default)]
    pub order_by: TrashOrderBy,
    /// UUID of the last item on the previous page (tie-breaker).
    pub resource_id: Uuid,
    /// `LOWER(name)` for `name`/`type` sorts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_str: Option<String>,
    /// Multipurpose integer: `folder_first` for `name`, `type_order` for `type`,
    /// size in bytes for `size`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_int: Option<i64>,
    /// Timestamp for `expires_at` and `trashed_at` sorts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_ts: Option<DateTime<Utc>>,
    /// Whether the result set was reversed — must match on every page.
    #[serde(default)]
    pub reverse: bool,
}

impl PageCursor for TrashCursor {}

/// Query parameters for `GET /api/trash/resources`.
#[derive(Debug, Deserialize, IntoParams)]
pub struct TrashResourcesQuery {
    /// Maximum items per page (1–200, default 50).
    #[serde(default = "CursorQuery::default_limit")]
    pub limit: u32,
    /// Opaque cursor from a previous response. Omit to start from the first page.
    pub cursor: Option<String>,
    /// Sort / group-by dimension — see [`TrashOrderBy`]. Omit for
    /// `expires_at` (soonest expiry first). An unknown value is a 400
    /// naming the accepted set, not a silent fall back to the default.
    pub order_by: Option<TrashOrderBy>,
    /// Comma-separated resource types to include, e.g. `"file,folder"`.
    /// Omit to include both.
    pub resource_types: Option<String>,
    /// Reverse the sort order. Default `false`.
    #[serde(default)]
    pub reverse: bool,
}

impl TrashResourcesQuery {
    pub fn limit_clamped(&self) -> usize {
        self.limit.clamp(1, 200) as usize
    }

    pub fn decode_cursor(&self) -> Option<TrashCursor> {
        self.cursor.as_deref().and_then(TrashCursor::decode)
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

/// One item in a `GET /api/trash/resources` page.
///
/// `expires_at` is the real timestamp at which the item will be permanently
/// deleted (= `trashed_at + retention_days`). The client derives "days until
/// deletion" itself from this + the current clock — the wire format does not
/// duplicate that derivation. `resource.path` carries the original location
/// (soft-delete preserves the row's `path` column).
#[derive(Debug, Serialize, ToSchema)]
pub struct TrashResourceItemDto {
    pub resource_type: ResourceTypeDto,
    /// When the user moved the item to trash.
    pub trashed_at: DateTime<Utc>,
    /// When the item will be permanently deleted by the retention sweeper.
    pub expires_at: DateTime<Utc>,
    /// The drive the trashed item belongs to. Enables client-side
    /// group-by-drive in the `/trash` UI (D2b spec — see
    /// `project_trash_groupbys_d2b` memory). The drive's display name
    /// resolves through the `/api/drives` listing the client already
    /// holds in `drives.svelte` — no extra round-trip needed.
    pub drive_id: Uuid,
    /// Full resource details — shape determined by `resource_type`.
    pub resource: ResourceContentDto,
}

/// Response envelope for `GET /api/trash/resources`.
pub type TrashResourcesDto = CursorListResponse<TrashResourceItemDto>;
