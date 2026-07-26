use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::application::dtos::cursor::PageCursor;
use crate::application::dtos::grant_dto::{ResourceContentDto, ResourceTypeDto};

/**
 * Data Transfer Object for file search criteria.
 *
 * This structure represents all possible search parameters that can be used
 * to filter files and folders in the system. It supports various filter types
 * including name matching, file types, date ranges, and size constraints.
 */
#[derive(Debug, Clone, Hash, Serialize, Deserialize, ToSchema)]
pub struct SearchCriteriaDto {
    /// Optional text to search in file/folder names
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name_contains: Option<String>,

    /// Optional list of file extensions to include (e.g., "pdf", "jpg")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_types: Option<Vec<String>>,

    /// Optional minimum creation date (seconds since epoch)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_after: Option<u64>,

    /// Optional maximum creation date (seconds since epoch)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_before: Option<u64>,

    /// Optional minimum modification date (seconds since epoch)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_after: Option<u64>,

    /// Optional maximum modification date (seconds since epoch)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_before: Option<u64>,

    /// Optional minimum file size in bytes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_size: Option<u64>,

    /// Optional maximum file size in bytes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_size: Option<u64>,

    /// Optional folder ID to limit search scope
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder_id: Option<String>,

    /// Whether to search recursively within subfolders (default: true)
    #[serde(default = "default_recursive")]
    pub recursive: bool,

    /// Maximum number of results to return
    #[serde(default = "default_limit")]
    pub limit: usize,

    /// Offset for pagination
    #[serde(default)]
    pub offset: usize,

    /// Sort order for results: "relevance", "name", "name_desc", "date", "date_desc", "size", "size_desc"
    #[serde(default = "default_sort_by")]
    pub sort_by: String,
}

/// Default value for recursive search (true)
fn default_recursive() -> bool {
    true
}

/// Default limit for search results (100)
fn default_limit() -> usize {
    100
}

/// Default sort_by value
fn default_sort_by() -> String {
    "relevance".to_string()
}

impl Default for SearchCriteriaDto {
    fn default() -> Self {
        Self {
            name_contains: None,
            file_types: None,
            created_after: None,
            created_before: None,
            modified_after: None,
            modified_before: None,
            min_size: None,
            max_size: None,
            folder_id: None,
            recursive: default_recursive(),
            limit: default_limit(),
            offset: 0,
            sort_by: default_sort_by(),
        }
    }
}

/// A file search result enriched with server-computed metadata.
///
/// Phase 1-plus extension (AuthZ-adjacent audit follow-up, 2026-07-26):
/// carries `etag`, `created_by`, `updated_by`, `is_favorite`, `is_shared`
/// through from `FileDto`. Pre-fix these fields were dropped at `enrich_file`
/// time, so the wire-normalised `SearchResourcesDto` handler couldn't
/// reconstruct a full `FileDto` for its `resource` slot — every result
/// looked unfavorited / unshared, and provenance was blank. The extra
/// columns come from `file_blob_read_repository.rs::search_files_paginated`
/// (SELECT'd inline, EXISTS subqueries for the caller-scoped booleans).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SearchFileResultDto {
    /// File ID
    pub id: String,
    /// File name
    pub name: String,
    /// Path to the file (relative)
    pub path: String,
    /// Size in bytes
    pub size: u64,
    /// MIME type — `Arc<str>` so enrichment reuses `FileDto`'s interned
    /// value (an atomic increment) instead of allocating per result row.
    #[schema(value_type = String)]
    pub mime_type: Arc<str>,
    /// Parent folder ID
    pub folder_id: Option<String>,
    /// Creation timestamp
    pub created_at: u64,
    /// Last modification timestamp
    pub modified_at: u64,
    /// Relevance score (0-100) computed server-side
    pub relevance_score: u32,
    /// Human-readable file size (e.g., "2.5 MB")
    pub size_formatted: String,
    /// CSS icon class for the file type (e.g., "fas fa-file-pdf")
    #[schema(value_type = String)]
    pub icon_class: Arc<str>,
    /// Extra CSS class for icon styling (e.g., "pdf-icon", "code-icon js-icon")
    #[schema(value_type = String)]
    pub icon_special_class: Arc<str>,
    /// Content category: "document", "image", "video", "audio", "archive", "code", "other"
    #[schema(value_type = String)]
    pub category: Arc<str>,
    /// Raw BLAKE3 content hash. Feeds `FileDto::content_hash` and
    /// `File::compute_etag` when search results are converted to
    /// `FileDto` (NC REPORT/SEARCH response). Defaults to `String::new()`
    /// for backward-compatible deserialisation of cached results
    /// that pre-date the column.
    #[serde(default)]
    pub blob_hash: String,
    /// Plain-text fragment around the first content match, present only for
    /// hits discovered through the full-text content index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// Where the match came from: "name" (filename matched the query) or
    /// "content" (discovered via the full-text content index).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_source: Option<String>,
    /// HTTP ETag — derived from `blob_hash + modified_at`. Duplicates
    /// `FileDto::etag` so the wire handler can hand a client the same
    /// token for `If-Match` / `If-None-Match` conditional requests on
    /// search results as it would on a folder listing.
    #[serde(default)]
    pub etag: String,
    /// §14 provenance — user that originally created this file. `None`
    /// when the referenced user has been deleted or for legacy rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<Uuid>,
    /// §14 provenance — user that performed the most recent mutation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<Uuid>,
    /// Caller-scoped: `true` when the requesting user has favorited
    /// this file. Populated by an EXISTS subquery in the search SQL —
    /// the search repo carries it back as a per-row bool that the
    /// service plumbs into this DTO.
    #[serde(default)]
    pub is_favorite: bool,
    /// Resource-scoped: `true` when the file has ANY explicit role-grant.
    /// Populated by the sibling EXISTS on `storage.role_grants`.
    #[serde(default)]
    pub is_shared: bool,
}

/// A folder search result enriched with server-computed metadata.
///
/// See `SearchFileResultDto` for the Phase 1-plus rationale — same
/// story: the six caller/provenance fields are carried through so the
/// wire-normalised handler can hand the frontend a complete `FolderDto`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SearchFolderResultDto {
    /// Folder ID
    pub id: String,
    /// Folder name
    pub name: String,
    /// Path to the folder (relative)
    pub path: String,
    /// Parent folder ID
    pub parent_id: Option<String>,
    /// Drive that owns this folder. Same column as `storage.folders.drive_id`,
    /// carried through so downstream callers (e.g. the NC search REPORT
    /// handler) can populate `FolderDto::drive_id` without a fallback sentinel.
    pub drive_id: Uuid,
    /// Creation timestamp
    pub created_at: u64,
    /// Last modification timestamp
    pub modified_at: u64,
    /// Whether it is a root folder
    pub is_root: bool,
    /// Relevance score (0-100) computed server-side
    pub relevance_score: u32,
    /// HTTP ETag — folders derive theirs from the tree-etag propagator.
    /// Duplicating it here keeps the wire handler's `FolderDto`
    /// reconstruction complete.
    #[serde(default)]
    pub etag: String,
    /// §14 provenance — creator user id. `None` for legacy folders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<Uuid>,
    /// §14 provenance — last-mutator user id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<Uuid>,
    /// Caller-scoped: `true` when the requesting user has favorited
    /// this folder. EXISTS on `auth.user_favorites`.
    #[serde(default)]
    pub is_favorite: bool,
    /// Resource-scoped: `true` when the folder has any explicit
    /// role-grant. EXISTS on `storage.role_grants`.
    #[serde(default)]
    pub is_shared: bool,
}

/**
 * Data Transfer Object for search results.
 *
 * This structure encapsulates the results of a search operation, including
 * both files and folders that match the search criteria, along with pagination
 * information and server-computed metadata.
 */
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SearchResultsDto {
    /// Files matching the search criteria (enriched with metadata)
    pub files: Vec<SearchFileResultDto>,

    /// Folders matching the search criteria (enriched with metadata)
    pub folders: Vec<SearchFolderResultDto>,

    /// Total count of matching items (for pagination)
    pub total_count: Option<usize>,

    /// Limit used in the search
    pub limit: usize,

    /// Offset used in the search
    pub offset: usize,

    /// Whether there are more results available
    pub has_more: bool,

    /// Query execution time in milliseconds (server-side)
    pub query_time_ms: u64,

    /// Sort order used
    pub sort_by: String,
}

impl SearchResultsDto {
    /// Creates a new empty search results object
    pub fn empty() -> Self {
        Self {
            files: Vec::new(),
            folders: Vec::new(),
            total_count: None,
            limit: 0,
            offset: 0,
            has_more: false,
            query_time_ms: 0,
            sort_by: "relevance".to_string(),
        }
    }

    /// Creates a new search results object from files and folders
    pub fn new(
        files: Vec<SearchFileResultDto>,
        folders: Vec<SearchFolderResultDto>,
        limit: usize,
        offset: usize,
        total_count: Option<usize>,
        query_time_ms: u64,
        sort_by: String,
    ) -> Self {
        let has_more = match total_count {
            Some(total) => (offset + files.len() + folders.len()) < total,
            None => false,
        };

        Self {
            files,
            folders,
            total_count,
            limit,
            offset,
            has_more,
            query_time_ms,
            sort_by,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// New wire shape — normalised to the `/*/resources` envelope
// (`items[] { resource_type, resource, meta }` + `next_cursor` + optional
// `total`/`query_time_ms`). Phase 1-plus: internal service still speaks
// `SearchCriteriaDto`/`SearchResultsDto`; the REST handler translates.
// ═══════════════════════════════════════════════════════════════════════════

/// Query parameters for `GET /api/search`.
///
/// Mirrors `FolderResourcesQuery` for the shared axes (`limit`, `cursor`,
/// `order_by`, `resource_types`, `reverse`) then adds search-specific
/// filters (`query`, `folder_id`, `recursive`, `file_types`,
/// `created_after`/`before`, `modified_after`/`before`, `min_size`/
/// `max_size`). `serde_urlencoded` doesn't support `#[serde(flatten)]`,
/// so the paging fields are inlined rather than composed from
/// `CursorQuery`.
#[derive(Debug, Deserialize, IntoParams)]
pub struct SearchResourcesQuery {
    /// Search phrase (matched against name; optionally content when the
    /// full-text index is enabled). Absent = "match everything," so
    /// callers can page through with just a folder scope + filters.
    pub query: Option<String>,

    /// Maximum items per page (1–200, default 50).
    #[serde(default = "SearchResourcesQuery::default_limit")]
    pub limit: u32,

    /// Opaque cursor from a previous response. Absent = first page.
    /// Encodes the current offset — Phase 1-plus still uses the
    /// existing offset-based service internals under the hood.
    pub cursor: Option<String>,

    /// Sort dimension. Supported: `"relevance"` (default), `"name"`,
    /// `"name_desc"`, `"date"` (= `modified_at`), `"date_desc"`,
    /// `"size"`, `"size_desc"`. Names match the pre-normalisation
    /// values `SearchCriteriaDto.sort_by` accepted so cached results
    /// remain reachable.
    pub order_by: Option<String>,

    /// Comma-separated resource types to include, e.g. `"file,folder"`.
    /// Absent = both. Matches the `FolderResourcesQuery` idiom.
    pub resource_types: Option<String>,

    /// Reverse the sort order. Default `false`.
    #[serde(default)]
    pub reverse: bool,

    /// Comma-separated file extensions filter, e.g. `"pdf,docx"`.
    #[serde(rename = "type")]
    pub type_filter: Option<String>,

    /// Restrict search to this folder.
    pub folder_id: Option<String>,

    /// Recursive traversal below `folder_id`. Default `true`.
    #[serde(default = "SearchResourcesQuery::default_recursive")]
    pub recursive: bool,

    /// Minimum creation timestamp (seconds since epoch).
    pub created_after: Option<u64>,
    /// Maximum creation timestamp (seconds since epoch).
    pub created_before: Option<u64>,
    /// Minimum modification timestamp (seconds since epoch).
    pub modified_after: Option<u64>,
    /// Maximum modification timestamp (seconds since epoch).
    pub modified_before: Option<u64>,
    /// Minimum file size in bytes.
    pub min_size: Option<u64>,
    /// Maximum file size in bytes.
    pub max_size: Option<u64>,
}

impl SearchResourcesQuery {
    pub fn default_limit() -> u32 {
        50
    }
    pub fn default_recursive() -> bool {
        true
    }
    pub fn limit_clamped(&self) -> usize {
        self.limit.clamp(1, 200) as usize
    }
    pub fn decode_cursor(&self) -> Option<SearchResourceCursor> {
        self.cursor
            .as_deref()
            .and_then(SearchResourceCursor::decode)
    }
    /// Convert to the internal `SearchCriteriaDto` the service still
    /// consumes. `limit` / `offset` come from the decoded cursor (or the
    /// query's `limit` on the first page). Sort names pass through
    /// verbatim — the service's `sort_by` matcher accepts the same set.
    pub fn to_criteria(&self) -> SearchCriteriaDto {
        let offset = self.decode_cursor().map(|c| c.offset).unwrap_or(0);
        let file_types = self.type_filter.as_deref().map(|s| {
            s.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect()
        });
        SearchCriteriaDto {
            name_contains: self.query.clone(),
            file_types,
            created_after: self.created_after,
            created_before: self.created_before,
            modified_after: self.modified_after,
            modified_before: self.modified_before,
            min_size: self.min_size,
            max_size: self.max_size,
            folder_id: self.folder_id.clone(),
            recursive: self.recursive,
            limit: self.limit_clamped(),
            offset,
            sort_by: self.order_by.clone().unwrap_or_else(default_sort_by),
        }
    }
    /// Which resource kinds to include. `None` = both. Anything else
    /// selects the intersection.
    pub fn include_files(&self) -> bool {
        match self.resource_types.as_deref() {
            None => true,
            Some(s) => s.split(',').any(|t| t.trim() == "file"),
        }
    }
    pub fn include_folders(&self) -> bool {
        match self.resource_types.as_deref() {
            None => true,
            Some(s) => s.split(',').any(|t| t.trim() == "folder"),
        }
    }
}

/// Opaque cursor for `/api/search`. Encodes the offset the underlying
/// service still uses, plus the sort dimension so a page fetched with
/// a different `order_by` than the previous one cannot silently drift
/// into a broken keyset. Phase 2 (service rewrite) would replace this
/// with a true keyset cursor over `(sort_key, id)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResourceCursor {
    pub offset: usize,
    pub order_by: String,
}
impl PageCursor for SearchResourceCursor {}

/// Search-specific per-item metadata (relevance score, snippet, hit
/// source). Sits inline on each `SearchResourceItem` so consumers get
/// data locality — no keyed lookup. `ResourceList` ignores the field.
///
/// Wire keys are shortened (`meta.score`, `meta.via`) vs the internal
/// `SearchFileResultDto` field names (`relevance_score`, `match_source`)
/// to keep the envelope compact on large result pages.
#[derive(Debug, Serialize, ToSchema)]
pub struct SearchMeta {
    /// Relevance 0-100. Higher = better match.
    pub score: u32,
    /// Plain-text fragment around the first content-index hit. Absent
    /// for name-only matches and for folder results.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// Where the hit came from: `"name"` or `"content"`. Absent when the
    /// origin is ambiguous (empty query → everything matches).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
}

/// One search result — same `resource_type + resource` shape as the
/// `/*/resources` envelopes so `ResourceList` consumes it as-is, plus
/// the inline `meta` for search-specific enrichment.
#[derive(Debug, Serialize, ToSchema)]
pub struct SearchResourceItem {
    pub resource_type: ResourceTypeDto,
    /// Full resource details (untagged: `FileDto | FolderDto | DriveDto`).
    /// Shape determined by `resource_type`.
    pub resource: ResourceContentDto,
    /// Search-specific metadata for this row.
    pub meta: SearchMeta,
}

/// Response envelope for `GET /api/search` — cursor-paginated + search
/// metadata. `total` is an approximate caller-visible count (permission-
/// filtered) when the service can compute it cheaply, absent otherwise —
/// matches sibling envelope endpoints, which all serialise counts as
/// integers (see `app_password_dto`, `plugin_dto`, `pagination`).
#[derive(Debug, Serialize, ToSchema)]
pub struct SearchResourcesDto {
    pub items: Vec<SearchResourceItem>,
    /// Opaque cursor for the next page. Absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Server-side query time. UI shows "Found N in Xms" and admins use
    /// it as a health signal.
    pub query_time_ms: u64,
    /// Approximate caller-visible total match count. Never leaks a count
    /// for rows the caller cannot see. Omitted when unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
}

impl SearchResourcesDto {
    /// Build the envelope from the service's existing offset-paginated
    /// result plus the request's cursor position. The service returns
    /// `SearchResultsDto` with `total_count` + `has_more` derived from
    /// COUNT(*) OVER(); we translate:
    /// - `has_more` → derive `next_cursor` (encoding `offset + returned`).
    /// - `total_count` → pass through as `Some(42)` when known, else `None`.
    ///
    /// Ownership: consumes the service result so the enriched DTOs move
    /// into the `resource` slot without cloning.
    pub fn from_service_result(
        results: crate::application::dtos::search_dto::SearchResultsDto,
        query: &SearchResourcesQuery,
    ) -> Self {
        let order_by = query.order_by.clone().unwrap_or_else(default_sort_by);
        let current_offset = query.decode_cursor().map(|c| c.offset).unwrap_or(0);
        let returned = results.files.len() + results.folders.len();

        let next_cursor = if results.has_more {
            Some(
                SearchResourceCursor {
                    offset: current_offset + returned,
                    order_by: order_by.clone(),
                }
                .encode(),
            )
        } else {
            None
        };

        let total = results.total_count;

        // Build items in an order the UI expects: folders first (like the
        // legacy split-shape) unless a specific sort is requested. When
        // ordering by relevance / date / size the caller almost always
        // wants interleaved output; when ordering by name the folders-
        // first convention matches file managers. Splitting the choice
        // by sort dimension keeps folder browsing intuitive.
        let mut items: Vec<SearchResourceItem> = Vec::with_capacity(returned);
        let query_lower = query
            .query
            .as_deref()
            .map(|s| s.to_lowercase())
            .unwrap_or_default();
        let folders_first = matches!(order_by.as_str(), "name" | "name_desc");

        if folders_first {
            append_folders(&mut items, results.folders);
            append_files(&mut items, results.files, &query_lower);
        } else {
            // Interleave by relevance_score (or the natural service order for
            // date/size — the service already returns rows in the requested
            // dimension, but folders and files come as two separate arrays
            // that we merge here by score for `relevance`, or just append
            // for size/date since the two arrays are individually ordered.
            append_folders(&mut items, results.folders);
            append_files(&mut items, results.files, &query_lower);
            if order_by == "relevance" {
                items.sort_by_key(|item| std::cmp::Reverse(item.meta.score));
            }
        }

        Self {
            items,
            next_cursor,
            query_time_ms: results.query_time_ms,
            total,
        }
    }
}

fn append_files(
    items: &mut Vec<SearchResourceItem>,
    files: Vec<SearchFileResultDto>,
    _query_lower: &str,
) {
    for f in files {
        let meta = SearchMeta {
            score: f.relevance_score,
            snippet: f.snippet.clone(),
            via: f.match_source.clone(),
        };
        // Reconstruct FileDto from the enriched search result. `size_formatted`
        // and display fields were already computed by `enrich_file`; the
        // Phase 1-plus extensions (etag / created_by / updated_by /
        // is_favorite / is_shared) carry through so the DTO is complete.
        let file_dto = crate::application::dtos::file_dto::FileDto {
            id: f.id,
            name: f.name,
            path: f.path,
            size: f.size,
            mime_type: f.mime_type,
            folder_id: f.folder_id,
            created_at: f.created_at,
            modified_at: f.modified_at,
            icon_class: f.icon_class,
            icon_special_class: f.icon_special_class,
            category: f.category,
            size_formatted: f.size_formatted,
            content_hash: f.blob_hash,
            etag: f.etag,
            created_by: f.created_by,
            updated_by: f.updated_by,
            is_favorite: f.is_favorite,
            is_shared: f.is_shared,
            sort_date: None,
        };
        items.push(SearchResourceItem {
            resource_type: ResourceTypeDto::File,
            resource: ResourceContentDto::File(file_dto),
            meta,
        });
    }
}

fn append_folders(items: &mut Vec<SearchResourceItem>, folders: Vec<SearchFolderResultDto>) {
    for f in folders {
        let meta = SearchMeta {
            score: f.relevance_score,
            snippet: None,
            via: None,
        };
        let folder_dto = crate::application::dtos::folder_dto::FolderDto {
            id: f.id.clone(),
            name: f.name,
            path: f.path,
            parent_id: f.parent_id,
            drive_id: f.drive_id,
            created_at: f.created_at,
            modified_at: f.modified_at,
            is_root: f.is_root,
            // Folders carry closed-set display fields — always the
            // same three static strings. Cheap to build via `Arc::from`
            // (interning-worthy but not on the search hot path).
            icon_class: Arc::from("fas fa-folder"),
            icon_special_class: Arc::from("folder-icon"),
            category: Arc::from("Folder"),
            etag: f.etag,
            created_by: f.created_by,
            updated_by: f.updated_by,
            is_favorite: f.is_favorite,
            is_shared: f.is_shared,
        };
        items.push(SearchResourceItem {
            resource_type: ResourceTypeDto::Folder,
            resource: ResourceContentDto::Folder(folder_dto),
            meta,
        });
    }
}

// `search_meta` map form was explored earlier and rejected in favour of
// inline `meta` per item (Ed 2026-07-26): data locality wins, no
// keyed-lookup step for consumers, matches the extensibility other
// `/*/resources` endpoints will want later.
#[allow(dead_code)]
fn _keep_hashmap_import_alive_for_future(_: HashMap<String, SearchMeta>) {}

/// DTO for search suggestion results (quick prefix search)
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SearchSuggestionsDto {
    /// Suggested file/folder names matching the query prefix
    pub suggestions: Vec<SearchSuggestionItem>,
    /// Query execution time in milliseconds
    pub query_time_ms: u64,
}

/// Individual search suggestion item
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SearchSuggestionItem {
    /// The suggested name
    pub name: String,
    /// Type: "file" or "folder"
    pub item_type: String,
    /// Item ID for navigation
    pub id: String,
    /// Path for context
    pub path: String,
    /// CSS icon class
    #[schema(value_type = String)]
    pub icon_class: Arc<str>,
    /// Extra CSS class for icon styling
    #[schema(value_type = String)]
    pub icon_special_class: Arc<str>,
    /// Relevance score
    pub relevance_score: u32,
}
