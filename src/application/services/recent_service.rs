use crate::application::dtos::cursor::PageCursor;
use crate::application::dtos::recent_dto::{
    RecentCursor, RecentItemDto, RecentOrderBy, RecentResourceRow,
};
use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::recent_ports::{RecentItemsRepositoryPort, RecentItemsUseCase};
use crate::application::ports::resource_access_hook::ResourceAccessHook;
use crate::common::errors::{DomainError, Result};
use crate::domain::services::authorization::{Permission, Resource, ResourceKind, Subject};
use crate::infrastructure::repositories::pg::RecentItemsPgRepository;
use crate::infrastructure::services::pg_acl_engine::PgAclEngine;
use std::sync::{Arc, OnceLock};
use tracing::info;
use uuid::Uuid;

/// Implementation of the use case for managing recent items.
///
/// Depends on `RecentItemsRepositoryPort` (outbound port) instead
/// of accessing `PgPool` directly, following the hexagonal architecture.
pub struct RecentService {
    repo: Arc<RecentItemsPgRepository>,
    max_recent_items: i32,
    /// ReBAC engine — enforces `Permission::Read` on the referenced
    /// file/folder before enrolling it into a user's Recent list.
    /// The listing side JOINs back to `storage.files/folders` and
    /// returns name/mime/size/drive_id for any enrolled UUID, so
    /// the write path is an information oracle without this gate.
    /// See `docs/plan/authz_audit/rest_storage.md`.
    authorization: Arc<PgAclEngine>,
    /// Set after construction via [`Self::set_resource_access_hook`].
    /// The hook is built FROM this service (it wraps an `Arc<Self>`), so
    /// we can't take it as a constructor arg without circular ownership;
    /// the OnceLock holds the back-edge so this service can notify the
    /// hook when the user clears or removes Recent rows. The notification
    /// lets the hook drop its in-memory throttle entries — otherwise a
    /// freshly-cleared Recent refuses to re-record until the TTL expires.
    resource_access_hook: OnceLock<Arc<dyn ResourceAccessHook>>,
}

impl RecentService {
    /// Create a new recent items service
    pub fn new(
        repo: Arc<RecentItemsPgRepository>,
        authorization: Arc<PgAclEngine>,
        max_recent_items: i32,
    ) -> Self {
        Self {
            repo,
            max_recent_items: max_recent_items.clamp(1, 100),
            authorization,
            resource_access_hook: OnceLock::new(),
        }
    }

    /// Wire the access hook in after construction. Idempotent: a second
    /// `set` is a no-op (returns the existing value as `Err`). Called
    /// from DI once `RecentRecordingHook::new(Arc<Self>)` has produced
    /// the back-edge that closes the loop.
    pub fn set_resource_access_hook(&self, hook: Arc<dyn ResourceAccessHook>) {
        let _ = self.resource_access_hook.set(hook);
    }

    /// Internal helper: notify the hook (if registered) that `user_id`
    /// has emptied their Recent list — wholly or by removing a single
    /// row. The hook drops its in-memory throttle entries so the very
    /// next access re-records into the freshly-empty table.
    fn notify_recents_cleared(&self, user_id: Uuid) {
        if let Some(hook) = self.resource_access_hook.get() {
            hook.on_recents_cleared(user_id);
        }
    }

    /// Record access to an item WITHOUT the pre-write `authz.require`
    /// gate. Callers must have gated the caller's Read upstream — this
    /// method exists for the `RecentRecordingHook` fast path: writes
    /// that reach the hook have already passed a `_with_perms` service
    /// method (uploads, streams, GETs, etc.), so re-checking here
    /// would be pure duplicate work AND widen the race window between
    /// the POST response and the `tokio::spawn`ed upsert (
    /// `tests/api/recent.hurl` step 7 hits this — the extra SQL
    /// round-trip pushes the upsert past the client's immediate
    /// `GET /api/recent/resources`).
    ///
    /// **Do NOT call this from an externally-reachable handler.** The
    /// REST endpoint goes through the trait method `record_item_access`
    /// below, which enforces the Read gate per AGENTS.md convention.
    pub async fn record_item_access_internal(
        &self,
        user_id: Uuid,
        item_id: &str,
        item_type: &str,
    ) -> Result<()> {
        // Type validation only — no authz, no resource parse for the
        // engine (the hook path is already resource-typed by construction).
        if item_type != "file" && item_type != "folder" {
            return Err(DomainError::new(
                crate::common::errors::ErrorKind::InvalidInput,
                "RecentItems",
                "Item type must be 'file' or 'folder'",
            ));
        }

        // Prune only when the upsert actually inserted a NEW row — a
        // re-access refreshes an existing row's timestamp and can never
        // grow the set past the cap, so the prune (a DELETE over an
        // OFFSET self-subquery) is a wasted round-trip on that common path
        // (benches/ROUND13.md §Q3).
        let inserted = self.repo.upsert_access(user_id, item_id, item_type).await?;
        if inserted {
            self.repo.prune(user_id, self.max_recent_items).await?;
        }
        Ok(())
    }
}

impl RecentItemsUseCase for RecentService {
    /// Get recent items for a user
    async fn get_recent_items(
        &self,
        user_id: Uuid,
        limit: Option<i32>,
    ) -> Result<Vec<RecentItemDto>> {
        info!("Getting recent items for user: {}", user_id);
        let limit_value = limit
            .unwrap_or(self.max_recent_items)
            .min(self.max_recent_items);
        let items = self.repo.get_recent_items(user_id, limit_value).await?;
        info!(
            "Retrieved {} recent items for user {}",
            items.len(),
            user_id
        );
        Ok(items)
    }

    /// Record access to an item
    async fn record_item_access(
        &self,
        user_id: Uuid,
        item_id: &str,
        item_type: &str,
    ) -> Result<()> {
        info!(
            "Recording access to {} '{}' for user {}",
            item_type, item_id, user_id
        );

        // AuthZ pre-write: caller must have Read on the referenced
        // resource. Denial routes through `require` → NotFound
        // (anti-enum) + `authz.denied` audit line. Without this
        // gate the write path was an information oracle over the
        // whole tenant via the listing endpoint's JOIN back to
        // storage.files/folders.
        //
        // Internal hook callers (RecentRecordingHook) bypass the
        // trait entry point and call `record_item_access_internal`
        // directly — Read has already been enforced upstream on
        // whatever `_with_perms` service produced the access event.
        let resource = Resource::parse(item_type, item_id)?;
        self.authorization
            .require(Subject::User(user_id), Permission::Read, resource)
            .await?;

        self.record_item_access_internal(user_id, item_id, item_type)
            .await?;

        info!(
            "Successfully recorded access to {} '{}' for user {}",
            item_type, item_id, user_id
        );
        Ok(())
    }

    /// Remove an item from recent
    async fn remove_from_recent(
        &self,
        user_id: Uuid,
        item_id: &str,
        item_type: &str,
    ) -> Result<bool> {
        info!(
            "Removing {} '{}' from recent for user {}",
            item_type, item_id, user_id
        );
        let removed = self.repo.remove_item(user_id, item_id, item_type).await?;
        info!(
            "{} {} '{}' from recent items for user {}",
            if removed {
                "Successfully removed"
            } else {
                "Not found"
            },
            item_type,
            item_id,
            user_id
        );
        // Drop the throttle entries so the next access re-records. We
        // notify on every call (even when `removed == false`) so the
        // semantics are "the user expressed intent to forget this" —
        // the hook owns the per-(user, item) cache anyway, dropping a
        // miss is a no-op.
        self.notify_recents_cleared(user_id);
        Ok(removed)
    }

    /// Clear all recent items
    async fn clear_recent_items(&self, user_id: Uuid) -> Result<()> {
        info!("Clearing all recent items for user {}", user_id);
        self.repo.clear_all(user_id).await?;
        info!("Cleared all recent items for user {}", user_id);
        self.notify_recents_cleared(user_id);
        Ok(())
    }
}

impl RecentService {
    /// No authz needed — recent items are strictly user-scoped; the repository
    /// enforces `WHERE user_id = $1` so users can only see their own entries.
    ///
    /// Returns `(rows, next_cursor_encoded)`.
    pub async fn list_resources_paged(
        &self,
        user_id: Uuid,
        limit: usize,
        cursor: Option<RecentCursor>,
        order_by: RecentOrderBy,
        kinds: Option<&[ResourceKind]>,
        reverse: bool,
    ) -> Result<(Vec<RecentResourceRow>, Option<String>)> {
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
            let c = build_recent_cursor(last, order_by, reverse);
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
fn build_recent_cursor(
    row: &RecentResourceRow,
    order_by: RecentOrderBy,
    reverse: bool,
) -> RecentCursor {
    // §7: exhaustive on `RecentOrderBy`, no catch-all. The `_` arm this
    // replaced doubled as the `accessed_at` default AND the unknown-axis
    // case, so a typo minted an accessed_at cursor that then disagreed
    // with whatever ORDER BY the repository picked.
    //
    // Only the three sort-field slots vary; the axis is just the (Copy)
    // parameter, where each arm used to re-spell it as a literal that
    // could drift from the arm matching it.
    let (sort_str, sort_int, sort_ts) = match order_by {
        // accessed_at timestamp
        RecentOrderBy::AccessedAt => (None, None, row.sort_ts),
        // sort_str = LOWER(name), sort_int = folder_first
        RecentOrderBy::Name => (row.sort_str.clone(), row.sort_int, None),
        // sort_str = LOWER(name), sort_int = type_order
        RecentOrderBy::Type => (row.sort_str.clone(), row.sort_int, None),
        RecentOrderBy::ModifiedAt => (None, None, row.sort_ts),
        // size in bytes
        RecentOrderBy::Size => (None, row.sort_int, None),
        // sort_str = LOWER(username), accessed_at as the secondary sort
        RecentOrderBy::Owner => (row.sort_str.clone(), None, row.sort_ts),
    };
    {
        RecentCursor {
            order_by,
            resource_id: row.resource_id,
            sort_str,
            sort_int,
            sort_ts,
            reverse,
        }
    }
}
