use std::collections::HashSet;

use uuid::Uuid;

use crate::application::dtos::favorites_dto::{
    BatchFavoritesResult, FavoriteItemDto, FavoriteResourceRow, FavoritesCursor, FavoritesOrderBy,
};
use crate::common::errors::Result;
use crate::domain::services::authorization::ResourceKind;

/// Defines operations for managing user favorites
pub trait FavoritesUseCase: Send + Sync {
    /// Get all favorites for a user
    async fn get_favorites(&self, user_id: Uuid) -> Result<Vec<FavoriteItemDto>>;

    /// Add an item to user's favorites
    async fn add_to_favorites(&self, user_id: Uuid, item_id: &str, item_type: &str) -> Result<()>;

    /// Remove an item from user's favorites
    async fn remove_from_favorites(
        &self,
        user_id: Uuid,
        item_id: &str,
        item_type: &str,
    ) -> Result<bool>;

    /// Check if an item is in user's favorites
    async fn is_favorite(&self, user_id: Uuid, item_id: &str, item_type: &str) -> Result<bool>;

    /// Add multiple items to favorites in a single transaction.
    /// Returns enriched favourites list so the client can replace its cache.
    async fn batch_add_to_favorites(
        &self,
        user_id: Uuid,
        items: &[(String, String)],
    ) -> Result<BatchFavoritesResult>;

    /// Check which of the given item IDs are favorites for this user.
    /// Returns the set of item_ids that are favorites.
    async fn batch_check_favorites(
        &self,
        user_id: Uuid,
        item_ids: &[(&str, &str)], // (item_id, item_type) pairs
    ) -> Result<HashSet<String>>;
}

// ─────────────────────────────────────────────────────
// Outbound port — persistence abstraction
// ─────────────────────────────────────────────────────

/// Secondary (outbound) port for favorites persistence.
///
/// Application services depend on this trait instead of
/// accessing `PgPool` directly. The concrete implementation
/// lives in `infrastructure::repositories::pg`.
pub trait FavoritesRepositoryPort: Send + Sync + 'static {
    /// Gets all favorites for a user.
    async fn get_favorites(&self, user_id: Uuid) -> Result<Vec<FavoriteItemDto>>;

    /// Adds an item to favorites. Returns `Ok(())` if it already existed (idempotent).
    async fn add_favorite(&self, user_id: Uuid, item_id: &str, item_type: &str) -> Result<()>;

    /// Removes an item from favorites. Returns `true` if it existed.
    async fn remove_favorite(&self, user_id: Uuid, item_id: &str, item_type: &str) -> Result<bool>;

    /// Checks if an item is in favorites.
    async fn is_favorite(&self, user_id: Uuid, item_id: &str, item_type: &str) -> Result<bool>;

    /// Insert multiple items in a single transaction.
    /// Returns the number of rows actually inserted (ignoring duplicates).
    async fn add_favorites_batch(&self, user_id: Uuid, items: &[(String, String)]) -> Result<u64>;

    /// Check which of the given item IDs are favorites for this user.
    /// Returns the set of item_ids that are favorites.
    async fn batch_check_favorites(
        &self,
        user_id: Uuid,
        item_ids: &[(&str, &str)], // (item_id, item_type) pairs
    ) -> Result<HashSet<String>>;

    /// Cursor-paginated list of a user's favorited resources.
    /// Items that no longer exist (deleted/trashed) are silently excluded.
    /// `kinds = None` → both files and folders.
    async fn list_resources_paged(
        &self,
        user_id: Uuid,
        limit: usize,
        cursor: Option<&FavoritesCursor>,
        order_by: FavoritesOrderBy,
        kinds: Option<&[ResourceKind]>,
        reverse: bool,
    ) -> Result<Vec<FavoriteResourceRow>>;

    /// Caller-scoped inline state flags for a single resource — the
    /// shared enrichment helper called by every single-item handler
    /// that emits `FileDto` / `FolderDto` to the SPA (get / rename /
    /// move / upload / delta upload / photos / bulk get by ids).
    ///
    /// Runs one SQL round trip with two `EXISTS` in the SELECT:
    ///   * `is_favorite` — `EXISTS on auth.user_favorites` for the
    ///     `(caller_id, resource_id, resource_type)` triple.
    ///   * `is_shared`   — `EXISTS on storage.role_grants` for the
    ///     `(resource_id, resource_type)` pair, regardless of role /
    ///     subject_type / granter. Link shares live in `role_grants`
    ///     as `subject_type = 'token'` under the unified model, so
    ///     one EXISTS covers link shares + user grants + group grants.
    ///
    /// `resource_type` MUST be `"file"` or `"folder"`.
    async fn caller_flags(
        &self,
        caller_id: Uuid,
        resource_type: &str,
        resource_id: Uuid,
    ) -> Result<(bool, bool)>;
}
