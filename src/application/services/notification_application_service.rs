//! Orchestrates persistent notifications.
//!
//! `create()` is the single ingester entry point:
//!
//! 1. Insert the row via [`NotificationRepository::create`].
//! 2. Publish a thin `NotificationReceived` event on
//!    `user:{user_id}:notifications` so subscribed sessions refetch
//!    immediately.
//!
//! The DB row is the truth (see `docs/plan/message-bus.md § Slice E`).
//! The bus is best-effort — a subscriber offline at publish time
//! recovers on next `GET /api/notifications`. Publish happens AFTER
//! the DB write succeeds, never inside a transaction — the plan's
//! "publish after commit" invariant.
//!
//! Reads (`list_for_user`, `count_unread_for_user`) and state changes
//! (`mark_read`, `mark_all_read`, `delete`) back the REST endpoints in
//! `interfaces/api/handlers/notifications.rs`. Every mutating method
//! is scoped on `user_id` at the SQL layer; the service does not run
//! its own AuthZ check because the identity is by construction
//! (`caller_id == user_id`, extracted from the auth middleware).

use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use crate::application::ports::message_bus_ports::{MessageBus, MessageBusEvent, Topic};
use crate::common::errors::DomainError;
use crate::domain::entities::notification::{NewNotification, Notification};
use crate::domain::repositories::notification_repository::{
    NotificationListFilter, NotificationRepository,
};

pub struct NotificationApplicationService {
    repo: Arc<dyn NotificationRepository>,
    bus: Arc<dyn MessageBus>,
}

impl NotificationApplicationService {
    pub fn new(repo: Arc<dyn NotificationRepository>, bus: Arc<dyn MessageBus>) -> Self {
        Self { repo, bus }
    }

    /// Insert a row for `new_notif` and publish a thin bus event.
    /// Returns the persisted row. This is the ingester-facing method
    /// — called from `ShareService::create_grant`,
    /// `AuthApplicationService` (new-device login),
    /// `SchedulerEngine` (job completed for actor), and the quota
    /// threshold hook.
    pub async fn create(&self, new_notif: NewNotification) -> Result<Notification, DomainError> {
        let row = self.repo.create(&new_notif).await?;

        // Publish AFTER the row is durable. Silent no-op if the bus
        // is disabled at boot (`OXICLOUD_MESSAGEBUS_ENABLE=false`) —
        // the WS route is unmounted so the publish just hits a dead
        // sender. The FE bell still works: it reads from the DB on
        // mount. See plan § "Slice E".
        self.bus.publish(
            &Topic::UserNotifications(row.user_id),
            MessageBusEvent::NotificationReceived {
                notification_id: row.id,
                kind: row.kind.clone(),
                created_at: row.created_at,
            },
        );

        Ok(row)
    }

    /// List notifications for `user_id` newest-first. Default limit at
    /// this layer is 50 rows (the repo caps at 500 defensively).
    pub async fn list_for_user(
        &self,
        user_id: Uuid,
        filter: NotificationListFilter,
    ) -> Result<Vec<Notification>, DomainError> {
        self.repo.list_for_user(user_id, &filter).await
    }

    /// Unread badge count.
    pub async fn count_unread_for_user(&self, user_id: Uuid) -> Result<i64, DomainError> {
        self.repo.count_unread_for_user(user_id).await
    }

    /// Mark one notification as read. Returns `true` if the row
    /// transitioned unread → read (i.e. was owned by `caller_id` and
    /// was previously unread). Returns `false` for already-read,
    /// missing, or misowned rows — indistinguishable at the wire so
    /// enumeration doesn't leak.
    pub async fn mark_read(
        &self,
        notification_id: Uuid,
        caller_id: Uuid,
    ) -> Result<bool, DomainError> {
        self.repo
            .mark_read(notification_id, caller_id, Utc::now())
            .await
    }

    /// Bulk mark-all-read. Returns rows updated.
    pub async fn mark_all_read(&self, caller_id: Uuid) -> Result<u64, DomainError> {
        self.repo
            .mark_all_read_for_user(caller_id, Utc::now())
            .await
    }

    /// Hard-delete one row. Same anti-enumeration semantics as
    /// [`mark_read`] — returns `false` for missing / misowned.
    pub async fn delete(
        &self,
        notification_id: Uuid,
        caller_id: Uuid,
    ) -> Result<bool, DomainError> {
        self.repo.delete_by_id(notification_id, caller_id).await
    }

    /// Retention job entry point. Called by `notifications_cleanup`
    /// on its daily cadence — deletes read rows older than `cutoff`.
    /// Unread rows are always preserved.
    pub async fn purge_read_before_cutoff(
        &self,
        cutoff: chrono::DateTime<Utc>,
    ) -> Result<u64, DomainError> {
        self.repo.purge_read_before(cutoff).await
    }
}
