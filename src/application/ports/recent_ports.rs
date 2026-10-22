use uuid::Uuid;

use crate::application::dtos::recent_dto::{RecentItemDto, RecentOrderBy};
use crate::common::errors::Result;

/// Defines operations for managing user recent items
pub trait RecentItemsUseCase: Send + Sync {
    /// Get all recent items for a user
    async fn get_recent_items(
        &self,
        user_id: Uuid,
        limit: Option<i32>,
    ) -> Result<Vec<RecentItemDto>>;

    /// Record access to an item
    async fn record_item_access(&self, user_id: Uuid, item_id: &str, item_type: &str)
    -> Result<()>;

    /// Remove an item from recents
    async fn remove_from_recent(
        &self,
        user_id: Uuid,
        item_id: &str,
        item_type: &str,
    ) -> Result<bool>;

    /// Clear the entire recent items list
    async fn clear_recent_items(&self, user_id: Uuid) -> Result<()>;
}

// ─────────────────────────────────────────────────────
// Outbound port — persistence abstraction
// ─────────────────────────────────────────────────────

/// Secondary (outbound) port for recent items persistence.
///
/// Abstracts access to the `auth.user_recent_files` table so that
/// `RecentService` does not depend directly on `PgPool`.
pub trait RecentItemsRepositoryPort: Send + Sync + 'static {
    /// Gets the latest recent items for a user (ordered by date desc).
    async fn get_recent_items(&self, user_id: Uuid, limit: i32) -> Result<Vec<RecentItemDto>>;

    /// Records/updates access to an item (upsert by user+item+type).
    /// Returns `true` when a NEW row was inserted (the recent set grew) and
    /// `false` when an existing row's timestamp was merely refreshed — the
    /// caller prunes only in the former case, since a re-access can never
    /// push the user over the cap (benches/ROUND13.md §Q3).
    async fn upsert_access(&self, user_id: Uuid, item_id: &str, item_type: &str) -> Result<bool>;

    /// Removes an item from recents. Returns `true` if it existed.
    async fn remove_item(&self, user_id: Uuid, item_id: &str, item_type: &str) -> Result<bool>;

    /// Removes all recent items for a user.
    async fn clear_all(&self, user_id: Uuid) -> Result<()>;

    /// Removes items exceeding `max_items` (the oldest ones).
    async fn prune(&self, user_id: Uuid, max_items: i32) -> Result<()>;

    /// List recent items with cursor pagination, sorting, and optional type filter.
    async fn list_resources_paged(
        &self,
        user_id: Uuid,
        limit: usize,
        cursor: Option<&crate::application::dtos::recent_dto::RecentCursor>,
        order_by: RecentOrderBy,
        kinds: Option<&[crate::domain::services::authorization::ResourceKind]>,
        reverse: bool,
    ) -> Result<Vec<crate::application::dtos::recent_dto::RecentResourceRow>>;
}
