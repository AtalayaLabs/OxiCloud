//! Port for user-scoped lists that must be swept when a caller
//! loses an ACL path to one of their rows (issue #913).
//!
//! `FavoritesService` and `RecentService` are the day-one
//! implementations. Both store user-owned rows that reference a
//! `storage.files` or `storage.folders` id and JOIN back to live
//! metadata on the listing path — if the reader loses Read access
//! to the target, the row leaks name/size/content_hash/mime for a
//! resource they can no longer open (and the owner can observe
//! accesses by watching the row re-appear with updated fields).
//!
//! **How the drain uses this.** The `ListCleanupService` scheduled
//! job (`src/infrastructure/services/list_cleanup_service.rs`)
//! drains `auth.cleanup_queue` — populated by the
//! `role_grants_enqueue_cleanup` trigger (migration
//! 20261118000000) on every `storage.role_grants` DELETE. For each
//! queued user it iterates every registered `ListCleanup`
//! implementor and calls `cleanup_user_orphans`.
//!
//! **Extending.** A new user-scoped list (bookmarks, pinned items,
//! read-states, …) that could leak the same way plugs in by
//! implementing this trait and getting registered with the
//! `ListCleanupService` in `common/di.rs`. The drain job needs no
//! changes.
//!
//! The implementation is expected to be idempotent — re-running
//! against the current ACL state produces the same result — so
//! the drain can safely retry on next tick after a transient
//! infrastructure error.

use async_trait::async_trait;
use uuid::Uuid;

use crate::common::errors::Result;

#[async_trait]
pub trait ListCleanup: Send + Sync {
    /// A short, stable name used in audit logs and admin
    /// listings. Kebab-case, e.g. `"favorites"`, `"recent"`.
    fn list_name(&self) -> &'static str;

    /// Sweep `user_id`'s rows in this list and delete those whose
    /// target the user can no longer Read via SOME grant path.
    ///
    /// The implementation re-checks each row against the current
    /// ACL engine; a user with any alternate grant path
    /// (direct user, group membership, ancestor folder, drive
    /// membership) keeps the row. Only an actual readability
    /// loss removes it.
    ///
    /// Returns the number of rows deleted. Infrastructure errors
    /// propagate; "cannot read" is a sweep result, not an error.
    async fn cleanup_user_orphans(&self, user_id: Uuid) -> Result<u64>;

    /// Count `user_id`'s rows in this list whose target they can
    /// no longer Read. Read-only mirror of `cleanup_user_orphans`:
    /// same re-check logic, no DELETE.
    ///
    /// Used by the `list_consistency` detector job (discovery-only
    /// by default; `?repair=true` enqueues affected users for the
    /// drain). Lets the detector surface drift from paths the
    /// `role_grants` DELETE trigger cannot cover — direct-SQL
    /// revokes, rows that outlived the pre-trigger era, bugs in
    /// future code that deletes grants another way — without
    /// doing the repair itself.
    async fn count_user_orphans(&self, user_id: Uuid) -> Result<u64>;
}
