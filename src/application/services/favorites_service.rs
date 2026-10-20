use std::collections::HashSet;
use std::sync::Arc;

use tracing::info;
use uuid::Uuid;

use crate::application::dtos::cursor::PageCursor;
use crate::application::dtos::favorites_dto::{
    BatchFavoritesResult, BatchFavoritesStats, FavoriteItemDto, FavoriteResourceRow,
    FavoritesCursor, FavoritesOrderBy,
};
use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::favorites_ports::{FavoritesRepositoryPort, FavoritesUseCase};
use crate::application::services::search_service::SearchService;
use crate::common::errors::Result;
use crate::domain::services::authorization::{Permission, Resource, ResourceKind, Subject};
use crate::infrastructure::repositories::pg::FavoritesPgRepository;
use crate::infrastructure::services::pg_acl_engine::PgAclEngine;

/// Implementation of the FavoritesUseCase for managing user favorites.
///
/// Depends on `FavoritesRepositoryPort` (outbound port) instead of
/// accessing the database directly, following hexagonal architecture.
pub struct FavoritesService {
    repo: Arc<FavoritesPgRepository>,
    /// ReBAC engine — enforces `Permission::Read` on the referenced
    /// file/folder before enrolling it into a user's favorites.
    /// Without this gate the write path is an information oracle:
    /// listing endpoints JOIN back to `storage.files/folders` and
    /// return name/mime/size/drive_id for any UUID the caller was
    /// able to enroll. See `docs/plan/authz_audit/rest_storage.md`.
    authorization: Arc<PgAclEngine>,
    /// Optional search-cache invalidator. Every favorite mutation
    /// changes what `is_favorite` returns on the caller's cached
    /// search result pages; without this hook the user sees a stale
    /// star badge for up to the search cache's 5-minute TTL (Ed's
    /// 2026-07-26 UX report). `None` when search is disabled
    /// (`OXICLOUD_ENABLE_SEARCH=false`).
    search: Option<Arc<SearchService>>,
}

impl FavoritesService {
    /// Create a new FavoritesService with the given repository port.
    /// `search` is `None` when search is disabled — the favorites path
    /// still works, just without the cache-invalidation callback.
    pub fn new(
        repo: Arc<FavoritesPgRepository>,
        authorization: Arc<PgAclEngine>,
        search: Option<Arc<SearchService>>,
    ) -> Self {
        Self {
            repo,
            authorization,
            search,
        }
    }

    /// Subset of `(item_id, item_type)` pairs the user has favorited — used to
    /// stamp star badges onto a folder listing in one batched query (no N+1, no
    /// global page fetch).
    pub async fn favorited_ids(
        &self,
        user_id: Uuid,
        items: &[(&str, &str)],
    ) -> Result<HashSet<String>> {
        self.repo.batch_check_favorites(user_id, items).await
    }

    /// Shared enrichment helper — computes `is_favorite` + `is_shared`
    /// for a single resource so single-item handlers (get / rename /
    /// move / upload / delta upload / photos / bulk get by ids) can
    /// populate the two wire-contract flags on FileDto / FolderDto
    /// before Json emission. Delegates straight to the repository
    /// port; kept on `FavoritesService` because the port already
    /// lives on that service and callers already hold it in DI.
    ///
    /// `resource_type` MUST be `"file"` or `"folder"`.
    pub async fn caller_flags(
        &self,
        caller_id: Uuid,
        resource_type: &str,
        resource_id: Uuid,
    ) -> Result<(bool, bool)> {
        self.repo
            .caller_flags(caller_id, resource_type, resource_id)
            .await
    }
}

impl FavoritesUseCase for FavoritesService {
    /// Get all favorites for a user
    async fn get_favorites(&self, user_id: Uuid) -> Result<Vec<FavoriteItemDto>> {
        info!("Getting favorites for user: {}", user_id);
        let favorites = self.repo.get_favorites(user_id).await?;
        info!(
            "Retrieved {} favorites for user {}",
            favorites.len(),
            user_id
        );
        Ok(favorites)
    }

    /// Add an item to user's favorites
    async fn add_to_favorites(&self, user_id: Uuid, item_id: &str, item_type: &str) -> Result<()> {
        info!(
            "Adding {} '{}' to favorites for user {}",
            item_type, item_id, user_id
        );

        // AuthZ pre-write: caller must have Read on the referenced
        // resource. Denial routes through `require` → NotFound
        // (anti-enum, matches the listing shape) + `authz.denied`
        // audit line. Without this gate the write path was an
        // information oracle over the whole tenant.
        let resource = Resource::parse(item_type, item_id)?;
        self.authorization
            .require(Subject::User(user_id), Permission::Read, resource)
            .await?;

        self.repo.add_favorite(user_id, item_id, item_type).await?;
        // Drop this user's cached search pages so a subsequent search
        // reflects the new star. Scoped to the caller — other tenants'
        // caches are untouched.
        if let Some(search) = &self.search {
            search.invalidate_for_user(user_id).await;
        }
        info!(
            "Successfully added {} '{}' to favorites for user {}",
            item_type, item_id, user_id
        );
        Ok(())
    }

    /// Remove an item from user's favorites
    async fn remove_from_favorites(
        &self,
        user_id: Uuid,
        item_id: &str,
        item_type: &str,
    ) -> Result<bool> {
        info!(
            "Removing {} '{}' from favorites for user {}",
            item_type, item_id, user_id
        );
        let removed = self
            .repo
            .remove_favorite(user_id, item_id, item_type)
            .await?;
        // Only invalidate when a row was actually removed — a no-op
        // remove (item wasn't favorited) doesn't need to cold-start the
        // cache. Keeps the "toggle a non-favorite" no-op cheap.
        if removed && let Some(search) = &self.search {
            search.invalidate_for_user(user_id).await;
        }
        info!(
            "{} {} '{}' from favorites for user {}",
            if removed {
                "Successfully removed"
            } else {
                "Did not find"
            },
            item_type,
            item_id,
            user_id
        );
        Ok(removed)
    }

    /// Check if an item is in user's favorites
    async fn is_favorite(&self, user_id: Uuid, item_id: &str, item_type: &str) -> Result<bool> {
        info!(
            "Checking if {} '{}' is favorite for user {}",
            item_type, item_id, user_id
        );
        self.repo.is_favorite(user_id, item_id, item_type).await
    }

    async fn batch_add_to_favorites(
        &self,
        user_id: Uuid,
        items: &[(String, String)],
    ) -> Result<BatchFavoritesResult> {
        info!(
            "Batch adding {} items to favorites for user {}",
            items.len(),
            user_id
        );

        // AuthZ pre-write: caller must have Read on every referenced
        // resource. Fail the whole batch on the first denial so the
        // response shape doesn't tell an attacker which items were
        // valid (partial success would leak the same oracle we
        // closed on the single-item path). See
        // `docs/plan/authz_audit/rest_storage.md`.
        //
        // Deliberately serial: a `try_join_all` fan-out measured WORSE
        // on both the cold (drive_of point-SELECTs) and warm (all-moka)
        // paths — future orchestration + pool-acquire contention cost
        // more than the local round trips they overlap. Rejected by
        // `bench_favorites_authz`; numbers in benches/ROUND6.md.
        for (item_id, item_type) in items {
            let resource = Resource::parse(item_type, item_id)?;
            self.authorization
                .require(Subject::User(user_id), Permission::Read, resource)
                .await?;
        }

        let requested = items.len();
        let inserted = self.repo.add_favorites_batch(user_id, items).await?;
        let already_existed = requested as u64 - inserted;
        // Any actual insert flips is_favorite for at least one row —
        // invalidate. Skip when the batch was fully idempotent (every
        // item was already favorited); no user-visible change.
        if inserted > 0
            && let Some(search) = &self.search
        {
            search.invalidate_for_user(user_id).await;
        }

        info!(
            "Batch favorites for user {}: {} requested, {} inserted, {} already existed",
            user_id, requested, inserted, already_existed
        );

        // Return the full enriched list so the client can replace its cache
        let favorites = self.repo.get_favorites(user_id).await?;

        Ok(BatchFavoritesResult {
            stats: BatchFavoritesStats {
                requested,
                inserted,
                already_existed,
            },
            favorites,
        })
    }

    async fn batch_check_favorites(
        &self,
        user_id: Uuid,
        item_ids: &[(&str, &str)],
    ) -> Result<HashSet<String>> {
        self.repo.batch_check_favorites(user_id, item_ids).await
    }
}

impl FavoritesService {
    /// Cursor-paginated list of the user's favorited resources.
    ///
    /// No authz needed — favorites are strictly user-scoped; the repository
    /// enforces `WHERE user_id = $1` so users can only see their own entries.
    ///
    /// Returns `(rows, next_cursor_encoded)`.
    pub async fn list_resources_paged(
        &self,
        user_id: Uuid,
        limit: usize,
        cursor: Option<FavoritesCursor>,
        order_by: FavoritesOrderBy,
        kinds: Option<&[ResourceKind]>,
        reverse: bool,
    ) -> Result<(Vec<FavoriteResourceRow>, Option<String>)> {
        // Fetch one extra row to detect whether a next page exists.
        let mut rows = self
            .repo
            .list_resources_paged(
                user_id,
                limit + 1,
                cursor.as_ref(),
                order_by,
                kinds,
                reverse,
            )
            .await?;

        let next_cursor = if rows.len() > limit {
            let last = &rows[limit - 1];
            let c = build_favorites_cursor(last, order_by, reverse);
            rows.truncate(limit);
            Some(c.encode())
        } else {
            None
        };

        Ok((rows, next_cursor))
    }
}

/// Build the next-page cursor from the last row of the current page.
/// `reverse` is stored in the cursor so subsequent pages use the same direction.
/// §7: exhaustive on `FavoritesOrderBy`, with no catch-all. The `_` arm
/// this replaced doubled as both the `name` case and the
/// unknown-axis case, so an invalid axis silently produced a `name`
/// cursor — and that cursor then disagreed with whatever ORDER BY the
/// repository chose. Adding a variant is now a compile error here.
///
/// Only the three sort-field slots vary per axis; `order_by` itself is
/// just the (Copy) parameter, where each arm previously re-spelled it as
/// a string literal and could typo it independently of its own match arm.
fn build_favorites_cursor(
    row: &FavoriteResourceRow,
    order_by: FavoritesOrderBy,
    reverse: bool,
) -> FavoritesCursor {
    let (sort_str, sort_int, sort_ts) = match order_by {
        // sort_str = LOWER(name), sort_int = folder_first (0 = folder, 1 = file)
        FavoritesOrderBy::Name => (row.sort_str.clone(), row.sort_int, None),
        // sort_str = LOWER(name), sort_int = type_order
        FavoritesOrderBy::Type => (row.sort_str.clone(), row.sort_int, None),
        FavoritesOrderBy::FavoritedAt => (None, None, row.sort_ts),
        FavoritesOrderBy::ModifiedAt => (None, None, row.sort_ts),
        // file size in bytes
        FavoritesOrderBy::Size => (None, row.sort_int, None),
        // sort_str = LOWER(username), favorited_at as the secondary sort
        FavoritesOrderBy::Owner => (row.sort_str.clone(), None, row.sort_ts),
    };
    FavoritesCursor {
        order_by,
        resource_id: row.resource_id,
        sort_str,
        sort_int,
        sort_ts,
        reverse,
    }
}
