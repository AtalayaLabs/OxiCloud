use std::cmp::Reverse;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::application::dtos::display_helpers::intern_display;
use crate::application::dtos::file_dto::FileDto;
use crate::application::dtos::folder_dto::FolderDto;
use crate::application::dtos::search_dto::{
    SearchCriteriaDto, SearchFileResultDto, SearchFolderResultDto, SearchResultsDto,
    SearchSuggestionItem, SearchSuggestionsDto,
};
use crate::application::ports::content_index_ports::{ContentHitDto, ContentIndexPort};
use crate::application::ports::inbound::SearchUseCase;
use crate::application::ports::storage_ports::FileReadPort;
use crate::common::errors::Result;
use crate::common::text::ascii_ci_contains;
use crate::domain::entities::folder::Folder;
use crate::domain::repositories::folder_repository::FolderRepository;
use crate::infrastructure::repositories::pg::file_blob_read_repository::FileBlobReadRepository;
use crate::infrastructure::repositories::pg::folder_db_repository::FolderDbRepository;
use std::hash::{Hash, Hasher};
use uuid::Uuid;

/**
 * High-performance search service implementation for files and folders.
 *
 * All search processing (filtering, scoring, sorting, categorization,
 * formatting) is performed server-side in Rust for maximum efficiency.
 * The frontend acts as a thin rendering client only.
 *
 * Features:
 * - Single-query recursive subtree search via PostgreSQL ltree
 * - Relevance scoring (exact match > starts-with > contains)
 * - Content categorization and icon mapping
 * - Multiple sort options (relevance, name, date, size)
 * - Server-side formatted file sizes
 * - Quick suggestions endpoint for autocomplete
 * - TTL-based result caching
 */
pub struct SearchService {
    /// Repository for file operations
    file_repository: Arc<FileBlobReadRepository>,

    /// Repository for folder operations
    folder_repository: Arc<FolderDbRepository>,

    /// Optional full-text content index (embedded Tantivy). When present,
    /// query-bearing searches additionally surface files whose CONTENT
    /// matches; hits are hydrated and re-filtered through SQL before use.
    content_index: Option<Arc<dyn ContentIndexPort>>,

    /// Optional authorization engine — needed to resolve the caller's
    /// accessible drive set before querying the content index, and to
    /// re-verify each Tantivy hit against `engine.check(Read, File(id))`
    /// as a defense-in-depth measure (catches index staleness and
    /// per-file grants that the drive-only Tantivy filter misses; see
    /// `docs/plan/drive.md` §11). `None` short-circuits the content
    /// index (the cheapest safe degradation).
    authorization: Option<Arc<crate::infrastructure::services::pg_acl_engine::PgAclEngine>>,

    /// Optional drive repository — used in tandem with the authorization
    /// engine to resolve the caller's accessible drives for the Tantivy
    /// filter. `None` short-circuits the content index.
    drive_repo: Option<Arc<dyn crate::domain::repositories::drive_repository::DriveRepository>>,

    /// Lock-free concurrent cache with automatic TTL and LRU eviction (moka).
    /// Values are `Arc<SearchResultsDto>` so cache insert/hit is a single
    /// atomic ref-count increment (~1 ns) instead of cloning thousands of Strings.
    ///
    /// **Byte-bounded**, not entry-bounded: entries are weighed by
    /// [`search_results_entry_weight`] and `max_capacity` is a byte budget.
    /// Keys span user × query × offset × limit, and each page holds up to 500
    /// enriched rows (~500–900 B of owned Strings each) — an entry-count bound
    /// let hundreds of MB of result pages accumulate invisibly.
    ///
    /// Key is `(user_id, criteria_hash)` (not a single fused `u64`) so
    /// `invalidate_for_user` can predicate on `k.0` — a per-user flush
    /// runs when the user favorites/shares a file so their next search
    /// sees the fresh `is_favorite` / `is_shared` flags instead of a
    /// cache entry that hardened at compute-time (was up to 5 min stale
    /// before 2026-07-26 — Ed reported the mismatch).
    search_cache: moka::future::Cache<(Uuid, u64), Arc<SearchResultsDto>>,
}

// ─── Search-results cache (byte-bounded) ─────────────────────────────────

/// Approximate heap bytes retained by one cached search page.
///
/// With a `weigher` installed, moka's `max_capacity` is the sum of entry
/// *weights*, so this converts the cache bound from "number of entries" to
/// real bytes: the length of every owned `String` in each file/folder row,
/// plus a fixed per-row and per-entry overhead for struct fields, the 24-B
/// `String` headers, `Vec` slots and allocator slop. Same pattern as the
/// file-content cache and the dedup manifest cache.
///
/// `pub` so `examples/bench_search_cache_mem.rs` can recompute retained
/// bytes with the exact production formula.
pub fn search_results_entry_weight(_key: &(Uuid, u64), value: &Arc<SearchResultsDto>) -> u32 {
    /// Fixed per-row overhead: struct scalars + one 24-B header per `String`
    /// field (12 on a file row, 4 on a folder row) + `Vec` slot + allocator
    /// slop. Deliberately a round upper-ish estimate — under-weighing is the
    /// failure mode that re-opens the memory hole.
    const ROW_OVERHEAD: usize = 200;
    /// Fixed per-entry overhead: `Arc` + `SearchResultsDto` scalars + `Vec`
    /// headers + moka's own bookkeeping per entry.
    const ENTRY_OVERHEAD: usize = 256;

    fn opt_len(s: &Option<String>) -> usize {
        s.as_deref().map_or(0, str::len)
    }

    let mut bytes = ENTRY_OVERHEAD + value.sort_by.len();
    for f in &value.files {
        bytes += ROW_OVERHEAD
            + f.id.len()
            + f.name.len()
            + f.path.len()
            + f.mime_type.len()
            + opt_len(&f.folder_id)
            + f.size_formatted.len()
            + f.icon_class.len()
            + f.icon_special_class.len()
            + f.category.len()
            + f.blob_hash.len()
            + opt_len(&f.snippet)
            + opt_len(&f.match_source);
    }
    for d in &value.folders {
        bytes += ROW_OVERHEAD + d.id.len() + d.name.len() + d.path.len() + opt_len(&d.parent_id);
    }
    bytes.min(u32::MAX as usize) as u32
}

/// Build the search-results cache exactly as production wires it: a byte
/// budget enforced through [`search_results_entry_weight`], plus TTL.
///
/// Shared with `examples/bench_search_cache_mem.rs` so the benchmark
/// measures the identical cache configuration that serves requests.
pub fn build_search_results_cache(
    cache_ttl_secs: u64,
    max_bytes: u64,
) -> moka::future::Cache<(Uuid, u64), Arc<SearchResultsDto>> {
    moka::future::Cache::builder()
        .max_capacity(max_bytes)
        .weigher(search_results_entry_weight)
        .time_to_live(Duration::from_secs(cache_ttl_secs))
        // Required for `invalidate_entries_if` to actually match anything
        // — without this the closure silently no-ops (per the
        // `bug_moka_invalidate_entries_if_needs_opt_in` memo).
        .support_invalidation_closures()
        .build()
}

// ─── Utility functions (pure, no self — computed on the server) ─────────

/// Compute relevance score (0–100) for a name against a query.
/// Exact match = 100, starts-with = 80, contains = 50, no match = 0.
///
/// `query_lower` **must** already be lowercased by the caller so that the
/// allocation happens once per search, not once per result.
///
/// The overwhelmingly common all-ASCII filename takes an allocation-free
/// ASCII case-fold fast path — `name.to_lowercase()` (full Unicode) is pure
/// waste there, and it ran once *per result row* (and per keystroke on the
/// suggest path). Non-ASCII names fall back to the exact Unicode-lowercase
/// comparison, so behavior is unchanged (for ASCII, lowercasing preserves
/// length, so the `contains` length ratio is identical). See benches/ROUND14.md §A2.
fn compute_relevance(name: &str, query_lower: &str) -> u32 {
    if name.is_ascii() {
        let (nb, qb) = (name.as_bytes(), query_lower.as_bytes());
        if nb.eq_ignore_ascii_case(qb) {
            100
        } else if nb.len() >= qb.len() && nb[..qb.len()].eq_ignore_ascii_case(qb) {
            80
        } else if ascii_ci_contains(nb, qb) {
            // Bonus for shorter names (more specific match). ASCII lowercase
            // preserves length, so `name.len()` == the old `name_lower.len()`.
            let ratio = query_lower.len() as f64 / name.len() as f64;
            50 + (ratio * 20.0) as u32
        } else {
            0
        }
    } else {
        let name_lower = name.to_lowercase();
        if name_lower == query_lower {
            100
        } else if name_lower.starts_with(query_lower) {
            80
        } else if name_lower.contains(query_lower) {
            let ratio = query_lower.len() as f64 / name_lower.len() as f64;
            50 + (ratio * 20.0) as u32
        } else {
            0
        }
    }
}

/// Max content-index candidates fetched per search. Hydration re-filters
/// them in ONE SQL round-trip, so this bounds both index and DB work.
const CONTENT_HITS_LIMIT: usize = 200;

/// Map a BM25 score into the 10–45 relevance band, normalized against the
/// best score of the result set. Deliberately below the weakest name match
/// (contains = 50): a filename hit is more specific than a body mention.
fn content_relevance(score: f32, max_score: f32) -> u32 {
    if !score.is_finite() || max_score <= 0.0 {
        return 10;
    }
    let ratio = (score / max_score).clamp(0.0, 1.0);
    10 + (ratio * 35.0).round() as u32
}

/// Re-sort the merged file list with the same semantics the folder list
/// uses. Only invoked when content hits were merged into a SQL-ordered page.
///
/// Sort dimension + direction are orthogonal (matches the wire
/// `SearchResourcesQuery` / internal `SearchCriteriaDto` split): 5
/// canonical `sort_by` values (`relevance | name | size | updated_at |
/// created_at`) × the boolean `reverse`.
fn sort_enriched_files(files: &mut [SearchFileResultDto], sort_by: &str, reverse: bool) {
    match sort_by {
        "name" if reverse => files.sort_by_cached_key(|f| Reverse(f.name.to_lowercase())),
        "name" => files.sort_by_cached_key(|f| f.name.to_lowercase()),
        "updated_at" if reverse => files.sort_by_key(|f| Reverse(f.modified_at)),
        "updated_at" => files.sort_by_key(|f| f.modified_at),
        "created_at" if reverse => files.sort_by_key(|f| Reverse(f.created_at)),
        "created_at" => files.sort_by_key(|f| f.created_at),
        "size" if reverse => files.sort_by_key(|f| Reverse(f.size)),
        "size" => files.sort_by_key(|f| f.size),
        // `relevance` (default) — reverse is a no-op; descending
        // relevance would be "least-relevant first," meaningless.
        _ => files.sort_by_key(|f| Reverse(f.relevance_score)),
    }
}

/// Format bytes into a human-readable string (e.g. "2.5 MB").
fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    if bytes == 0 {
        return "0 B".to_string();
    }
    let exp = (bytes as f64).log(1024.0).floor() as usize;
    let exp = exp.min(UNITS.len() - 1);
    let value = bytes as f64 / 1024_f64.powi(exp as i32);
    if exp == 0 {
        format!("{} B", bytes)
    } else {
        format!("{:.1} {}", value, UNITS[exp])
    }
}

// ─── SearchService implementation ───────────────────────────────────────

impl SearchService {
    /**
     * Creates a new instance of the search service.
     *
     * `max_cache_bytes` is the byte budget for the results cache (weigher-
     * bounded, see [`search_results_entry_weight`]) — it replaced the old
     * entry-count capacity, which was blind to how big each cached page is.
     */
    pub fn new(
        file_repository: Arc<FileBlobReadRepository>,
        folder_repository: Arc<FolderDbRepository>,
        content_index: Option<Arc<dyn ContentIndexPort>>,
        authorization: Option<Arc<crate::infrastructure::services::pg_acl_engine::PgAclEngine>>,
        drive_repo: Option<Arc<dyn crate::domain::repositories::drive_repository::DriveRepository>>,
        cache_ttl: u64,
        max_cache_bytes: u64,
    ) -> Self {
        let search_cache = build_search_results_cache(cache_ttl, max_cache_bytes);

        Self {
            file_repository,
            folder_repository,
            content_index,
            authorization,
            drive_repo,
            search_cache,
        }
    }

    /// Creates a cache key from the search criteria using zero-allocation hashing.
    /// Hash just the criteria — the caller pairs the returned `u64` with
    /// the `Uuid` user_id to form the composite cache key `(Uuid, u64)`.
    /// Split from the fused hash so `invalidate_for_user` can predicate
    /// on the user side of the tuple without decoding the criteria.
    fn create_cache_key(criteria: &SearchCriteriaDto) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        criteria.hash(&mut hasher);
        hasher.finish()
    }

    /// Enrich a FileDto → SearchFileResultDto with server-computed metadata.
    ///
    /// Consumes the DTO: every `String` moves and the interned display
    /// fields (`mime_type`/`icon_class`/`icon_special_class`/`category`,
    /// already computed once in `FileDto::from`) transfer as refcount
    /// bumps — the old borrow-based version cloned all of them AND re-ran
    /// the three display classifiers per result row.
    ///
    /// `query_lower` must already be lowercased (empty string when no query).
    fn enrich_file(file: FileDto, query_lower: &str) -> SearchFileResultDto {
        let relevance = if query_lower.is_empty() {
            50
        } else {
            compute_relevance(&file.name, query_lower)
        };

        SearchFileResultDto {
            id: file.id,
            name: file.name,
            path: file.path,
            size: file.size,
            mime_type: file.mime_type,
            folder_id: file.folder_id,
            created_at: file.created_at,
            modified_at: file.modified_at,
            relevance_score: relevance,
            size_formatted: format_bytes(file.size),
            icon_class: file.icon_class,
            icon_special_class: file.icon_special_class,
            category: file.category,
            // Carry the content hash through so REPORT/SEARCH
            // responses on the NC surface can emit the same ETag
            // (`File::compute_etag`) as PROPFIND/GET would.
            blob_hash: file.content_hash,
            snippet: None,
            match_source: (!query_lower.is_empty() && relevance > 0).then(|| "name".to_string()),
            // Phase 1-plus: carry the FileDto fields the old enrich
            // shape dropped. Populates the normalised
            // `SearchResourcesDto` items with a complete `FileDto` so
            // the frontend `ResourceList` renders favorites / share
            // badges / provenance consistently with other listings.
            etag: file.etag,
            created_by: file.created_by,
            updated_by: file.updated_by,
            is_favorite: file.is_favorite,
            is_shared: file.is_shared,
        }
    }

    /// Enrich a FolderDto → SearchFolderResultDto with server-computed metadata.
    ///
    /// Consumes the DTO so the owned strings move instead of cloning.
    ///
    /// `query_lower` must already be lowercased (empty string when no query).
    fn enrich_folder(folder: FolderDto, query_lower: &str) -> SearchFolderResultDto {
        let relevance = if query_lower.is_empty() {
            50
        } else {
            compute_relevance(&folder.name, query_lower)
        };

        SearchFolderResultDto {
            id: folder.id,
            name: folder.name,
            path: folder.path,
            parent_id: folder.parent_id,
            drive_id: folder.drive_id,
            created_at: folder.created_at,
            modified_at: folder.modified_at,
            is_root: folder.is_root,
            relevance_score: relevance,
            // Phase 1-plus (see sibling `enrich_file`): carry the
            // FolderDto fields the old enrich shape dropped.
            etag: folder.etag,
            created_by: folder.created_by,
            updated_by: folder.updated_by,
            is_favorite: folder.is_favorite,
            is_shared: folder.is_shared,
        }
    }

    /// Query the content index for files matching by CONTENT (when the index
    /// is enabled). First page only — content hits have no stable
    /// interleaving with SQL pagination beyond it, and page one is where
    /// search UX lives. Index failures degrade to name-only results, never
    /// to a failed search.
    async fn lookup_content_hits(
        &self,
        criteria: &SearchCriteriaDto,
        user_id: Uuid,
    ) -> Vec<ContentHitDto> {
        use crate::application::ports::authorization_ports::AuthorizationEngine;
        use crate::domain::services::authorization::Subject;

        let Some(index) = &self.content_index else {
            return Vec::new();
        };
        let Some(authz) = &self.authorization else {
            return Vec::new();
        };
        let Some(drive_repo) = &self.drive_repo else {
            return Vec::new();
        };
        if criteria.offset != 0 {
            return Vec::new();
        }
        let Some(query) = criteria
            .name_contains
            .as_deref()
            .map(str::trim)
            .filter(|q| q.len() >= 2)
        else {
            return Vec::new();
        };

        // Resolve the caller's accessible drive set. Group-mediated
        // grants are honoured inline by `storage.caller_group_ids` on
        // the SQL side, so no Rust-side subject expansion here.
        let accessible_drives: Vec<Uuid> = match drive_repo.list_readable_by(user_id).await {
            Ok(drives) => drives.iter().map(|d| d.drive.id).collect(),
            Err(e) => {
                tracing::warn!("Content-index: drive lookup failed — degrading to empty: {e}");
                return Vec::new();
            }
        };

        // Tantivy filter (Must drive_id ∈ accessible_drives) handles
        // the cross-drive isolation. Empty drive list short-circuits
        // inside `search_content`.
        let hits = match index
            .search_content(&accessible_drives, query, CONTENT_HITS_LIMIT)
            .await
        {
            Ok(hits) => hits,
            Err(e) => {
                tracing::warn!("Content-index lookup failed — returning name-only results: {e}");
                return Vec::new();
            }
        };

        // Defense in depth: re-verify each hit through the engine.
        // Catches two cases the drive_id filter can't:
        //   * Index staleness — the file just moved drives and the
        //     worker hasn't caught up.
        //   * Per-file grants — ReBAC can grant a single file inside a
        //     drive the caller doesn't otherwise have. The Tantivy
        //     filter is drive-only; this re-check restores per-file
        //     resolution.
        // Failures degrade conservatively (drop the hit / the page,
        // log it) — never leak. Batched: one drive-resolution query for
        // the whole page instead of up to CONTENT_HITS_LIMIT sequential
        // point SELECTs (benches/SEARCH-REBAC.md).
        // Parse each hit id ONCE and carry the pair through the verify loop
        // — the old shape re-parsed every `file_id` a second time below
        // (benches/ROUND11.md §12: 1.6x on a 100-hit page).
        let mut pairs = Vec::with_capacity(hits.len());
        for hit in hits {
            match Uuid::parse_str(&hit.file_id) {
                Ok(u) => pairs.push((hit, u)),
                Err(_) => {
                    tracing::warn!("Content-index hit had non-UUID file_id: {}", hit.file_id);
                }
            }
        }
        let hit_ids: Vec<Uuid> = pairs.iter().map(|(_, u)| *u).collect();
        let allowed = match authz
            .check_files_read_batch(Subject::User(user_id), &hit_ids)
            .await
        {
            Ok(set) => set,
            Err(e) => {
                tracing::warn!("ReBAC re-check failed for content hits: {e}");
                return Vec::new();
            }
        };
        let mut verified = Vec::with_capacity(pairs.len());
        for (hit, file_uuid) in pairs {
            if allowed.contains(&file_uuid) {
                verified.push(hit);
            } else {
                tracing::debug!(
                    target: "oxicloud::search",
                    file_id = %file_uuid,
                    "dropping content-index hit: ReBAC denies Read after Tantivy filter",
                );
            }
        }
        verified
    }

    /// Merge content-index hits into the name-search result page:
    /// * files the name search already found just gain their `snippet`;
    /// * content-only candidates are hydrated through SQL in one round-trip
    ///   (re-applying user scope, trash state and every active filter — a
    ///   stale index id silently drops out), enriched, scored into the
    ///   content relevance band and appended;
    /// * the merged page is re-sorted with the caller's `sort_by`.
    ///
    /// Returns how many files were added (callers bump their totals by it).
    async fn merge_content_hits(
        &self,
        hits: Vec<ContentHitDto>,
        enriched_files: &mut Vec<SearchFileResultDto>,
        criteria: &SearchCriteriaDto,
        user_id: Uuid,
    ) -> Result<usize> {
        if hits.is_empty() {
            return Ok(0);
        }

        let mut by_id: std::collections::HashMap<&str, &ContentHitDto> =
            hits.iter().map(|h| (h.file_id.as_str(), h)).collect();
        for file in enriched_files.iter_mut() {
            if let Some(hit) = by_id.remove(file.id.as_str()) {
                file.snippet = hit.snippet.clone();
            }
        }
        if by_id.is_empty() {
            return Ok(0);
        }

        // Preserve the index's score order when collecting the leftovers.
        let candidate_ids: Vec<String> = hits
            .iter()
            .filter(|h| by_id.contains_key(h.file_id.as_str()))
            .map(|h| h.file_id.clone())
            .collect();
        let files = self
            .file_repository
            .fetch_files_by_ids_filtered(&candidate_ids, criteria, user_id)
            .await?;
        if files.is_empty() {
            return Ok(0);
        }

        let max_score = hits.iter().map(|h| h.score).fold(0.0_f32, f32::max);
        let mut added = 0usize;
        for file in files {
            let dto = FileDto::from(file);
            let Some(hit) = by_id.get(dto.id.as_str()) else {
                continue;
            };
            let (score, snippet) = (hit.score, hit.snippet.clone());
            let mut enriched = Self::enrich_file(dto, "");
            enriched.relevance_score = content_relevance(score, max_score);
            enriched.snippet = snippet;
            enriched.match_source = Some("content".to_string());
            enriched_files.push(enriched);
            added += 1;
        }
        if added > 0 {
            sort_enriched_files(enriched_files, &criteria.sort_by, criteria.reverse);
        }
        Ok(added)
    }

    /// Quick suggestions search — returns up to `limit` name suggestions
    /// matching the query. Pushes filtering, relevance sort and LIMIT to SQL
    /// so only a handful of rows cross the DB→app boundary.
    ///
    /// `caller_id` scopes the underlying repo queries to drives the caller
    /// can Read. Without it (the pre-fix shape) any authenticated user —
    /// including external magic-link recipients — could autocomplete both
    /// names and full paths across every tenant on the instance (AuthZ
    /// audit finding #1, 2026-07-12). Named `_with_perms` per the
    /// AGENTS.md AuthZ convention.
    pub async fn suggest_with_perms(
        &self,
        query: &str,
        folder_id: Option<&str>,
        limit: usize,
        caller_id: Uuid,
    ) -> Result<SearchSuggestionsDto> {
        let start = Instant::now();

        // Ask SQL for at most `limit` best-matching files and folders
        let (files, folders) = tokio::join!(
            self.file_repository
                .suggest_files_by_name(folder_id, query, limit, caller_id),
            self.folder_repository
                .suggest_folders_by_name(folder_id, query, limit, caller_id),
        );
        let files = files?;
        let folders = folders?;

        let mut suggestions: Vec<SearchSuggestionItem> =
            Vec::with_capacity(files.len() + folders.len());

        // Pre-compute once — avoids N heap allocations inside the loops.
        let query_lower = query.to_lowercase();

        // Consume the entities: the old loop deep-cloned every File into
        // the DTO conversion and then cloned name/id/path AGAIN into the
        // suggestion — 3 field clones + a full entity clone per row on
        // an every-keystroke path.
        for file in files {
            let file_dto = FileDto::from(file);
            let score = compute_relevance(&file_dto.name, &query_lower);
            suggestions.push(SearchSuggestionItem {
                name: file_dto.name,
                item_type: "file".to_string(),
                id: file_dto.id,
                path: file_dto.path,
                // Interned in `FileDto::from` — reuse instead of re-running
                // the display classifiers per keystroke suggestion.
                icon_class: file_dto.icon_class,
                icon_special_class: file_dto.icon_special_class,
                relevance_score: score,
            });
        }

        for folder in folders {
            let folder_dto = FolderDto::from(folder);
            let score = compute_relevance(&folder_dto.name, &query_lower);
            suggestions.push(SearchSuggestionItem {
                name: folder_dto.name,
                item_type: "folder".to_string(),
                id: folder_dto.id,
                path: folder_dto.path,
                icon_class: intern_display("fas fa-folder"),
                icon_special_class: intern_display("folder-icon"),
                relevance_score: score,
            });
        }

        // Merge files + folders by relevance and truncate to the final limit
        suggestions.sort_by_key(|f| Reverse(f.relevance_score));
        suggestions.truncate(limit);

        let elapsed = start.elapsed().as_millis() as u64;
        Ok(SearchSuggestionsDto {
            suggestions,
            query_time_ms: elapsed,
        })
    }
}

// ─── Bench-only public wrappers (feature = "bench") ──────────────────────

#[cfg(feature = "bench")]
impl SearchService {
    /// Public wrapper over the private `enrich_file` so
    /// `examples/bench_search_enrich.rs` can measure it.
    pub fn enrich_file_for_bench(file: FileDto, query_lower: &str) -> SearchFileResultDto {
        Self::enrich_file(file, query_lower)
    }

    /// Public wrapper over the private `enrich_folder` for the same bench.
    pub fn enrich_folder_for_bench(folder: FolderDto, query_lower: &str) -> SearchFolderResultDto {
        Self::enrich_folder(folder, query_lower)
    }
}

// ─── SearchUseCase trait implementation ──────────────────────────────────

impl SearchUseCase for SearchService {
    /**
     * Performs a search based on the specified criteria.
     *
     * Optimization: For non-recursive searches, uses database-level pagination
     * for better performance. For recursive searches, uses the parallel approach.
     *
     * All processing happens server-side:
     * - Database-level pagination for non-recursive searches
     * - Parallel recursive traversal for recursive searches
     * - Filtering by name, type, dates, size
     * - Relevance scoring
     * - Sorting (relevance, name, date, size)
     * - Content categorization & icon mapping
     * - Human-readable size formatting
     * - Pagination
     */
    async fn search(
        &self,
        criteria: SearchCriteriaDto,
        user_id: Uuid,
    ) -> Result<Arc<SearchResultsDto>> {
        // Composite key: `(user_id, criteria_hash)`. Pairs the identity of
        // the caller with the hash of the request so `invalidate_for_user`
        // can drop just this user's entries when their favorites / shares
        // change (see the `search_cache` field doc for the "why").
        let cache_key = (user_id, Self::create_cache_key(&criteria));

        // Single-flight: collapse N identical concurrent searches into ONE
        // execution. `try_get_with` serves the cached result on a hit and, on a
        // miss, runs the closure exactly once while the other callers await it
        // — so a burst of identical queries no longer floods Postgres or drains
        // the connection pool (the old get-from-cache fast path is subsumed).
        self.search_cache
            .try_get_with(cache_key, async move {
                let start = Instant::now();
                let query = criteria.name_contains.as_deref().unwrap_or("");
                // Pre-compute once — avoids N heap allocations inside enrich_file/enrich_folder.
                let query_lower = query.to_lowercase();

                // For non-recursive searches, use efficient database-level pagination
                // This avoids loading all files into memory
                if !criteria.recursive {
                    // Same folders-first sequencing as the recursive branch
                    // below (see the block comment there for the rationale
                    // — SQL applies file offset+limit, so folder_count has
                    // to be known before the file query is issued).
                    let (content_hits, folders_res) = tokio::join!(
                        self.lookup_content_hits(&criteria, user_id),
                        self.folder_repository.search_folders(
                            criteria.folder_id.as_deref(),
                            criteria.name_contains.as_deref(),
                            user_id,
                            false,
                        ),
                    );
                    let (folders, folder_flags) = folders_res?;

                    let folder_count = folders.len();
                    let folders_before_page = criteria.offset.min(folder_count);
                    let folders_on_page = (folder_count - folders_before_page).min(criteria.limit);
                    let file_offset = criteria.offset - folders_before_page;
                    let file_limit_needed = criteria.limit - folders_on_page;
                    let file_limit_probe = file_limit_needed.max(1);

                    let mut file_criteria = criteria.clone();
                    file_criteria.offset = file_offset;
                    file_criteria.limit = file_limit_probe;

                    let (files, file_flags, total_file_count) = self
                        .file_repository
                        .search_files_paginated(
                            criteria.folder_id.as_deref(),
                            &file_criteria,
                            user_id,
                        )
                        .await?;

                    // Convert to DTOs and enrich with metadata — one fused
                    // pass, no intermediate Vec<FileDto> materialization.
                    // `file_flags` is aligned 1:1 with `files` (in-SQL
                    // per-row EXISTS on favorites + role_grants) so a
                    // simple parallel zip plumbs the caller-scoped
                    // booleans onto the FileDto before enrichment.
                    let mut enriched_files: Vec<SearchFileResultDto> = files
                        .into_iter()
                        .zip(file_flags)
                        .map(|(f, (is_fav, is_shr))| {
                            let mut dto = FileDto::from(f);
                            dto.is_favorite = is_fav;
                            dto.is_shared = is_shr;
                            Self::enrich_file(dto, &query_lower)
                        })
                        .collect();

                    // For folders, apply sorting and pagination in memory (usually fewer folders).
                    // Same parallel-zip shape as the file branch above —
                    // `folder_flags` is aligned 1:1 by index.
                    let mut enriched_folders: Vec<SearchFolderResultDto> = folders
                        .into_iter()
                        .zip(folder_flags)
                        .map(|(f, (is_fav, is_shr))| {
                            let mut dto = FolderDto::from(f);
                            dto.is_favorite = is_fav;
                            dto.is_shared = is_shr;
                            Self::enrich_folder(dto, &query_lower)
                        })
                        .collect();

                    // Sort folders (cached_key avoids O(N log N) temporary String allocations).
                    // 5-dimension model (`relevance | name | size | updated_at | created_at`)
                    // × the boolean `reverse` — matches the file-side `sort_enriched_files`
                    // + the SQL match in `file_blob_read_repository`. `size` isn't
                    // meaningful for folders (no size column), so it falls through
                    // to the relevance default.
                    match (criteria.sort_by.as_str(), criteria.reverse) {
                        ("name", false) => {
                            enriched_folders.sort_by_cached_key(|f| f.name.to_lowercase());
                        }
                        ("name", true) => {
                            enriched_folders.sort_by_cached_key(|f| Reverse(f.name.to_lowercase()));
                        }
                        ("updated_at", false) => {
                            enriched_folders.sort_by_key(|f| f.modified_at);
                        }
                        ("updated_at", true) => {
                            enriched_folders.sort_by_key(|f| Reverse(f.modified_at));
                        }
                        ("created_at", false) => {
                            enriched_folders.sort_by_key(|f| f.created_at);
                        }
                        ("created_at", true) => {
                            enriched_folders.sort_by_key(|f| Reverse(f.created_at));
                        }
                        // `relevance` (default) + `size` (N/A for folders) +
                        // anything unrecognised — all land on relevance-desc.
                        _ => {
                            enriched_folders.sort_by_key(|f| Reverse(f.relevance_score));
                        }
                    }

                    // Blend in content-discovered files, then truncate to
                    // the exact page size — see the recursive branch's
                    // block comment for why the truncate is required.
                    //
                    // Note: `total_count` is intentionally the pure SQL
                    // name-match count (plus folders) — NOT inflated by
                    // `added` content-hit rows. `added` counts hits that
                    // aren't already in the *current* SQL slice, which is
                    // per-page (different SQL rows on each page produce
                    // different dedup outcomes and a different `added`).
                    // Including it made the client-visible `total`
                    // flicker as the user paginated (2026-07-26 report:
                    // 4184 → 4186 across scope + page toggles for a
                    // stable dataset). Content-hits still bubble into
                    // each page's `items`; they just don't move the
                    // grand total.
                    let _added = self
                        .merge_content_hits(content_hits, &mut enriched_files, &criteria, user_id)
                        .await?;
                    enriched_files.truncate(file_limit_needed);

                    let total_count = total_file_count + folder_count;

                    let paginated_folders: Vec<_> = enriched_folders
                        .into_iter()
                        .skip(folders_before_page)
                        .take(folders_on_page)
                        .collect();
                    let paginated_files = enriched_files;

                    let elapsed_ms = start.elapsed().as_millis() as u64;

                    let search_results = Arc::new(SearchResultsDto::new(
                        paginated_files,
                        paginated_folders,
                        criteria.limit,
                        criteria.offset,
                        Some(total_count),
                        elapsed_ms,
                        criteria.sort_by.clone(),
                    ));

                    return Ok(search_results);
                }

                // ── Recursive search via ltree (single SQL query per entity type) ──
                // Uses PostgreSQL ltree GiST index to find all files and folders
                // in the subtree in O(1) queries, replacing the O(N) spawn-per-folder
                // approach that could saturate the connection pool.
                //
                // ── Correct pagination across a folders-then-files list ──
                // Pre-fix the service ran (content, file-page, folder-page)
                // in one `tokio::join!` with the SAME `criteria.offset/limit`
                // going to the file SQL, then re-paginated in memory using
                // ABSOLUTE offsets. That was doubly wrong: SQL already
                // applied `[offset, offset+limit)` and the in-memory slice
                // then tried to skip `offset` MORE rows — for any query
                // with few folders this dropped whole pages (2026-07-26:
                // 4002-file query returned 50 rows on page 1 then `items:[]`
                // on page 2 with a valid `next_cursor`).
                //
                // Fix: fold folders + content lookup first (they're both
                // cheap and folder_count is what tells us how many files
                // to skip). Then run the file SQL with `offset` shifted by
                // `folder_count` and `limit` reduced by whatever folders
                // fit on the current page — so SQL returns EXACTLY the
                // file slice that belongs here, no in-memory re-slicing.
                // The min-1 probe below preserves `COUNT(*) OVER()` even
                // when folders fill the whole page (LIMIT 0 → 0 rows →
                // total_count column projects nowhere → false zero).
                let (content_hits, folders_res) = tokio::join!(
                    self.lookup_content_hits(&criteria, user_id),
                    self.folder_repository.search_folders(
                        criteria.folder_id.as_deref(),
                        criteria.name_contains.as_deref(),
                        user_id,
                        true,
                    ),
                );
                let (found_folders, folder_flags): (Vec<Folder>, Vec<(bool, bool)>) = folders_res?;

                let folder_count = found_folders.len();
                let folders_before_page = criteria.offset.min(folder_count);
                let folders_on_page = (folder_count - folders_before_page).min(criteria.limit);
                let file_offset = criteria.offset - folders_before_page;
                let file_limit_needed = criteria.limit - folders_on_page;
                // Probe with LIMIT ≥ 1 so `COUNT(*) OVER()` has a row to
                // project onto; the extra row (if any) is truncated below.
                let file_limit_probe = file_limit_needed.max(1);

                let mut file_criteria = criteria.clone();
                file_criteria.offset = file_offset;
                file_criteria.limit = file_limit_probe;

                let (found_files, file_flags, total_file_count) = self
                    .file_repository
                    .search_files_in_subtree(criteria.folder_id.as_deref(), &file_criteria, user_id)
                    .await?;

                // ── Convert to DTOs and enrich with server-computed metadata ──
                // Fused single pass: no intermediate DTO Vec materialization.
                // Same shape as the non-recursive branch above — parallel
                // zip of `found_files` with the in-SQL caller_flags.
                let mut enriched_files: Vec<SearchFileResultDto> = found_files
                    .into_iter()
                    .zip(file_flags)
                    .map(|(f, (is_fav, is_shr))| {
                        let mut dto = FileDto::from(f);
                        dto.is_favorite = is_fav;
                        dto.is_shared = is_shr;
                        Self::enrich_file(dto, &query_lower)
                    })
                    .collect();

                let mut enriched_folders: Vec<SearchFolderResultDto> = found_folders
                    .into_iter()
                    .zip(folder_flags)
                    .map(|(f, (is_fav, is_shr))| {
                        let mut dto = FolderDto::from(f);
                        dto.is_favorite = is_fav;
                        dto.is_shared = is_shr;
                        Self::enrich_folder(dto, &query_lower)
                    })
                    .collect();

                // ── Sort folders (cached_key avoids O(N log N) temporary String allocations) ──
                match criteria.sort_by.as_str() {
                    "name" => {
                        enriched_folders.sort_by_cached_key(|f| f.name.to_lowercase());
                    }
                    "name_desc" => {
                        enriched_folders.sort_by_cached_key(|f| Reverse(f.name.to_lowercase()));
                    }
                    "date" => {
                        enriched_folders.sort_by_key(|f| f.modified_at);
                    }
                    "date_desc" => {
                        enriched_folders.sort_by_key(|f| Reverse(f.modified_at));
                    }
                    _ => {
                        enriched_folders.sort_by_key(|f| Reverse(f.relevance_score));
                    }
                }

                // Blend in content-discovered files. `merge_content_hits`
                // pushes candidates from the Tantivy index onto the tail
                // and re-sorts by the criteria; the SQL limit above only
                // bounded the name-match set, so truncate after merging
                // to the exact page size we intended to return.
                //
                // `total_count` is the pure SQL name-match count + folder
                // count — see the non-recursive branch's block comment
                // for why `added` is deliberately excluded (per-page
                // dedup outcome, would flicker the client-visible
                // total across pages).
                let _added = self
                    .merge_content_hits(content_hits, &mut enriched_files, &criteria, user_id)
                    .await?;
                enriched_files.truncate(file_limit_needed);

                let total_count = total_file_count + folder_count;

                // Fetches were pre-sliced: enriched_files is already the
                // exact file page (SQL applied file_offset + file_limit),
                // and folders_before_page / folders_on_page tell us which
                // slice of `enriched_folders` belongs here. No in-memory
                // absolute-offset math — see the block comment above.
                let paginated_folders: Vec<_> = enriched_folders
                    .into_iter()
                    .skip(folders_before_page)
                    .take(folders_on_page)
                    .collect();
                let paginated_files = enriched_files;

                let elapsed_ms = start.elapsed().as_millis() as u64;

                let search_results = Arc::new(SearchResultsDto::new(
                    paginated_files,
                    paginated_folders,
                    criteria.limit,
                    criteria.offset,
                    Some(total_count),
                    elapsed_ms,
                    criteria.sort_by.clone(),
                ));

                Ok(search_results)
            })
            .await
            .map_err(|shared: Arc<crate::common::errors::DomainError>| {
                crate::common::errors::DomainError::new(
                    shared.kind,
                    shared.entity_type,
                    shared.message.clone(),
                )
            })
    }

    /// Returns quick suggestions for autocomplete. Delegates to the
    /// inherent `suggest_with_perms` — the trait method is preserved as
    /// the polymorphic entry point (e.g. for `StubSearchUseCase` in
    /// tests); production callers can equivalently call the inherent
    /// method directly.
    async fn suggest(
        &self,
        query: &str,
        folder_id: Option<&str>,
        limit: usize,
        caller_id: Uuid,
    ) -> Result<SearchSuggestionsDto> {
        self.suggest_with_perms(query, folder_id, limit, caller_id)
            .await
    }

    /// Clears the search results cache.
    async fn clear_search_cache(&self) -> Result<()> {
        self.search_cache.invalidate_all();
        self.search_cache.run_pending_tasks().await;
        Ok(())
    }
}

impl SearchService {
    /// Drop every cached search page for a single user. Called by the
    /// favorites / share services after a mutation that changes what
    /// `is_favorite` / `is_shared` would return for one of the caller's
    /// files — without this the caller would see a stale flag for up
    /// to `cache_ttl_secs` (Ed's 2026-07-26 report).
    ///
    /// `invalidate_entries_if` needs `.support_invalidation_closures()`
    /// on the cache builder — set in `build_search_results_cache`. This
    /// is scoped (predicate matches `k.0 == user_id` on the composite
    /// `(Uuid, u64)` key), so a per-user favorite toggle does NOT
    /// cold-start every other tenant's cache the way `invalidate_all`
    /// does on the admin cache-flush endpoint.
    pub async fn invalidate_for_user(&self, user_id: Uuid) {
        // moka registers the predicate and returns a `PredicateId` — we
        // don't need the id (we're not planning to unregister). Errors
        // here are non-critical: worst case the caller sees stale
        // is_favorite / is_shared for TTL seconds, exactly the state
        // before this fix. Log-and-swallow keeps the mutation path
        // reliable even under moka pressure.
        if let Err(e) = self
            .search_cache
            .invalidate_entries_if(move |k, _| k.0 == user_id)
        {
            tracing::warn!(
                target: "oxicloud::search",
                error = %e,
                %user_id,
                "search cache invalidate_entries_if failed — user will see \
                 stale is_favorite / is_shared until TTL expires",
            );
        }
    }
}

// ─── Stub for testing ────────────────────────────────────────────────────

impl SearchService {
    /// Creates a stub version of the service for testing
    pub fn new_stub() -> impl SearchUseCase {
        struct SearchServiceStub;

        impl SearchUseCase for SearchServiceStub {
            async fn search(
                &self,
                _criteria: SearchCriteriaDto,
                _user_id: Uuid,
            ) -> Result<Arc<SearchResultsDto>> {
                Ok(Arc::new(SearchResultsDto::empty()))
            }

            async fn suggest(
                &self,
                _query: &str,
                _folder_id: Option<&str>,
                _limit: usize,
                _caller_id: Uuid,
            ) -> Result<SearchSuggestionsDto> {
                Ok(SearchSuggestionsDto {
                    suggestions: Vec::new(),
                    query_time_ms: 0,
                })
            }

            async fn clear_search_cache(&self) -> Result<()> {
                Ok(())
            }
        }

        SearchServiceStub
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_relevance_stays_below_name_contains_band() {
        // Best hit of the set caps at 45 — always under contains (50).
        assert_eq!(content_relevance(8.0, 8.0), 45);
        assert_eq!(content_relevance(4.0, 8.0), 28);
        // Degenerate inputs fall to the floor instead of panicking.
        assert_eq!(content_relevance(1.0, 0.0), 10);
        assert_eq!(content_relevance(f32::NAN, 8.0), 10);
        assert!(content_relevance(0.0, 8.0) >= 10);
    }

    fn dto(name: &str, relevance: u32, size: u64, modified_at: u64) -> SearchFileResultDto {
        SearchFileResultDto {
            id: name.to_string(),
            name: name.to_string(),
            path: format!("/{name}"),
            size,
            mime_type: "text/plain".into(),
            folder_id: None,
            created_at: 0,
            modified_at,
            relevance_score: relevance,
            size_formatted: String::new(),
            icon_class: "".into(),
            icon_special_class: "".into(),
            category: "".into(),
            blob_hash: String::new(),
            snippet: None,
            match_source: None,
            etag: String::new(),
            created_by: None,
            updated_by: None,
            is_favorite: false,
            is_shared: false,
        }
    }

    #[test]
    fn entry_weight_counts_every_owned_string_plus_overheads() {
        // Empty page: entry overhead + sort_by ("relevance" = 9 bytes).
        let empty = Arc::new(SearchResultsDto::empty());
        let base = search_results_entry_weight(&(Uuid::nil(), 0), &empty) as usize;
        assert_eq!(base, 256 + 9);

        // One file row: base + row overhead + its owned string bytes
        // (id 7 + name 7 + path 8 + mime 10; the rest are empty/None).
        let one_file = Arc::new(SearchResultsDto::new(
            vec![dto("abc.txt", 50, 10, 1)],
            Vec::new(),
            100,
            0,
            Some(1),
            0,
            "relevance".to_string(),
        ));
        let w = search_results_entry_weight(&(Uuid::nil(), 0), &one_file) as usize;
        assert_eq!(w, base + 200 + 7 + 7 + 8 + 10);

        // Folder rows weigh too (id 2 + name 4 + path 5 + parent 6 = 17).
        let one_folder = Arc::new(SearchResultsDto::new(
            Vec::new(),
            vec![SearchFolderResultDto {
                id: "f1".to_string(),
                name: "docs".to_string(),
                path: "/docs".to_string(),
                parent_id: Some("parent".to_string()),
                drive_id: Uuid::nil(),
                created_at: 0,
                modified_at: 0,
                is_root: false,
                relevance_score: 50,
                etag: String::new(),
                created_by: None,
                updated_by: None,
                is_favorite: false,
                is_shared: false,
            }],
            100,
            0,
            Some(1),
            0,
            "relevance".to_string(),
        ));
        let w = search_results_entry_weight(&(Uuid::nil(), 0), &one_folder) as usize;
        assert_eq!(w, base + 200 + 2 + 4 + 5 + 6);
    }

    #[tokio::test]
    async fn cache_evicts_down_to_the_byte_budget() {
        // Budget fits ~2 of these entries; inserting 20 must never let the
        // weighted size settle above the budget.
        let entry = |i: usize| {
            Arc::new(SearchResultsDto::new(
                (0..50)
                    .map(|r| dto(&format!("file_{i}_{r}_{}", "x".repeat(100)), 50, 1, 1))
                    .collect(),
                Vec::new(),
                50,
                0,
                Some(50),
                0,
                "relevance".to_string(),
            ))
        };
        let per_entry = search_results_entry_weight(&(Uuid::nil(), 0), &entry(0)) as u64;
        let budget = per_entry * 2 + per_entry / 2;

        let cache = build_search_results_cache(300, budget);
        for i in 0..20u64 {
            cache.insert((Uuid::nil(), i), entry(i as usize)).await;
        }
        cache.run_pending_tasks().await;

        let retained: u64 = cache
            .iter()
            .map(|(k, v)| search_results_entry_weight(&k, &v) as u64)
            .sum();
        assert!(
            retained <= budget,
            "retained {retained} B exceeds budget {budget} B"
        );
        assert!(cache.entry_count() <= 2);
    }

    #[test]
    fn merged_files_resort_by_relevance_and_by_column() {
        let mut files = vec![
            dto("b-content.txt", 30, 10, 200),
            dto("a-name.txt", 80, 99, 100),
        ];
        sort_enriched_files(&mut files, "relevance", false);
        assert_eq!(
            files[0].name, "a-name.txt",
            "name match must outrank content match"
        );

        // 5-dimension sort model (2026-07-26): direction is a separate
        // boolean, `_desc` suffixes retired. `size + reverse=true` = old
        // `size_desc`, `updated_at + reverse=false` = old `date`, etc.
        sort_enriched_files(&mut files, "size", true);
        assert_eq!(files[0].name, "a-name.txt");
        sort_enriched_files(&mut files, "updated_at", false);
        assert_eq!(files[0].name, "a-name.txt");
        sort_enriched_files(&mut files, "name", true);
        assert_eq!(files[0].name, "b-content.txt");
    }
}
