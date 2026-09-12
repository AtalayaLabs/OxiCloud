//! Storage port for [`Notification`].
//!
//! Backs the bell UI. `create` is the only ingester-facing method;
//! `list_for_user` / `mark_read` / `mark_all_read` / `delete_by_id` /
//! `purge_read_before` back the REST endpoints and the retention job.
//!
//! Every method takes `user_id` where relevant so the SQL includes the
//! caller-scope in its WHERE clause — the application service double-
//! checks the requested notification's owner matches the caller, but
//! the repo scoping is defense in depth (a bug that misroutes an id
//! still can't leak another user's row through `mark_read`).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::common::errors::DomainError;
use crate::domain::entities::notification::{NewNotification, Notification};

/// Optional filter for [`NotificationRepository::list_for_user`]. All
/// fields are additive — the default (Default::default) applies no
/// restriction on any axis.
#[derive(Debug, Clone, Default)]
pub struct NotificationListFilter {
    /// Cap on rows returned. Default at the service layer is 50; the
    /// repo caps defensively at 500 so a runaway caller can't drag
    /// the DB.
    pub limit: Option<u32>,
    /// `true` → return only rows with `read_at IS NULL`. `false`
    /// (default) returns both read and unread. There is no
    /// "read-only" filter — no consumer needed it, and adding one
    /// bloats the query surface.
    pub unread_only: bool,
    /// When `Some(t)`, return only rows created strictly BEFORE `t`.
    /// Cursor-style pagination for the "load older page" flow: caller
    /// passes the oldest `created_at` from the previous page.
    pub before: Option<DateTime<Utc>>,
    /// When `Some(t)`, return only rows created strictly AFTER `t`.
    /// Delta-catch-up cursor for the "since last seen" flow — used by
    /// the FE bell on WS reconnect / tab reactivation to fetch rows
    /// that arrived during a disconnect window. Combines with
    /// `before` (both applied); combining them semantically bounds
    /// the returned range on both sides.
    pub after: Option<DateTime<Utc>>,
}

#[async_trait]
pub trait NotificationRepository: Send + Sync + 'static {
    /// Insert a new notification. Returns the persisted row (id +
    /// created_at populated). The application service publishes the
    /// bus event AFTER this returns Ok — see plan's "publish after
    /// commit" invariant.
    async fn create(&self, new_notif: &NewNotification) -> Result<Notification, DomainError>;

    /// List notifications for `user_id` newest-first, honouring
    /// `filter`. Returns an empty Vec (not an error) when the user
    /// has none.
    async fn list_for_user(
        &self,
        user_id: Uuid,
        filter: &NotificationListFilter,
    ) -> Result<Vec<Notification>, DomainError>;

    /// Count unread rows for `user_id`. Backs the bell's unread badge.
    /// Separate from `list_for_user` so the badge can render without
    /// fetching payloads.
    async fn count_unread_for_user(&self, user_id: Uuid) -> Result<i64, DomainError>;

    /// Mark one notification as read. Returns `Ok(true)` if a row
    /// transitioned from unread → read (i.e. was owned by `user_id`
    /// AND had `read_at IS NULL`); `Ok(false)` if the row didn't
    /// exist, was owned by someone else, or was already read.
    /// Idempotent from the caller's perspective; the `bool` is for
    /// logs / audit only.
    async fn mark_read(
        &self,
        notification_id: Uuid,
        user_id: Uuid,
        at: DateTime<Utc>,
    ) -> Result<bool, DomainError>;

    /// Bulk mark-all-read. Returns the number of rows updated.
    async fn mark_all_read_for_user(
        &self,
        user_id: Uuid,
        at: DateTime<Utc>,
    ) -> Result<u64, DomainError>;

    /// Hard-delete a single row. Same ownership scoping as
    /// [`mark_read`]. Returns `Ok(true)` iff a row was deleted.
    async fn delete_by_id(&self, notification_id: Uuid, user_id: Uuid)
    -> Result<bool, DomainError>;

    /// Retention job: delete every read row whose `read_at` is older
    /// than `cutoff`. Returns the number of rows removed.
    /// Unread rows are preserved unconditionally — that's the whole
    /// point of the durable table.
    async fn purge_read_before(&self, cutoff: DateTime<Utc>) -> Result<u64, DomainError>;
}
