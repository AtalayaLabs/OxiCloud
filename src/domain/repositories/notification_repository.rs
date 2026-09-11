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
/// fields are additive — `None` means "no restriction on this axis".
#[derive(Debug, Clone, Default)]
pub struct NotificationListFilter {
    /// Cap on rows returned. Default at the service layer is 50; the
    /// repo does not impose one so a full-export use case remains
    /// possible.
    pub limit: Option<u32>,
    /// When `Some(true)`, return only rows with `read_at IS NULL`.
    /// When `Some(false)`, return only rows with `read_at IS NOT NULL`.
    /// `None` returns both.
    pub unread_only: Option<bool>,
    /// When `Some(t)`, return only rows created strictly before `t`.
    /// Cursor-style pagination: caller passes the oldest `created_at`
    /// from the previous page.
    pub before: Option<DateTime<Utc>>,
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
