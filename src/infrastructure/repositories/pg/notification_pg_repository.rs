//! PostgreSQL implementation of [`NotificationRepository`].
//!
//! Backs the bell UI plus the daily retention job. All queries scope on
//! `user_id` at the SQL layer so a row misroute in the caller can't
//! leak another user's data through mark_read / delete. Schema lives
//! in `migrations/20261026000000_notifications.sql`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};
use std::sync::Arc;
use uuid::Uuid;

use crate::common::errors::{DomainError, ErrorKind};
use crate::domain::entities::notification::{NewNotification, Notification};
use crate::domain::repositories::notification_repository::{
    NotificationListFilter, NotificationRepository,
};

pub struct NotificationPgRepository {
    pool: Arc<PgPool>,
}

impl NotificationPgRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    fn map_row(row: &sqlx::postgres::PgRow) -> Result<Notification, DomainError> {
        let map_err = |field: &str, e: sqlx::Error| {
            DomainError::new(
                ErrorKind::DatabaseError,
                "Notification",
                format!("read {field}: {e}"),
            )
        };
        Ok(Notification {
            id: row.try_get("id").map_err(|e| map_err("id", e))?,
            user_id: row.try_get("user_id").map_err(|e| map_err("user_id", e))?,
            kind: row.try_get("kind").map_err(|e| map_err("kind", e))?,
            payload: row.try_get("payload").map_err(|e| map_err("payload", e))?,
            created_at: row
                .try_get("created_at")
                .map_err(|e| map_err("created_at", e))?,
            read_at: row.try_get("read_at").ok(),
        })
    }
}

fn db_err(op: &'static str, e: sqlx::Error) -> DomainError {
    DomainError::new(
        ErrorKind::DatabaseError,
        "Notification",
        format!("{op}: {e}"),
    )
}

#[async_trait]
impl NotificationRepository for NotificationPgRepository {
    async fn create(&self, new_notif: &NewNotification) -> Result<Notification, DomainError> {
        let row = sqlx::query(
            r#"
            INSERT INTO notif.notifications (user_id, kind, payload)
            VALUES ($1::uuid, $2, $3)
            RETURNING id, user_id, kind, payload, created_at, read_at
            "#,
        )
        .bind(new_notif.user_id)
        .bind(&new_notif.kind)
        .bind(&new_notif.payload)
        .fetch_one(self.pool.as_ref())
        .await
        .map_err(|e| db_err("create", e))?;
        Self::map_row(&row)
    }

    async fn list_for_user(
        &self,
        user_id: Uuid,
        filter: &NotificationListFilter,
    ) -> Result<Vec<Notification>, DomainError> {
        // Dynamic-shape query built to still hit the
        // notifications_user_created_read index — every branch keys
        // on (user_id, created_at DESC).
        let limit: i64 = filter.limit.unwrap_or(50).min(500) as i64;
        let rows = match (filter.unread_only, filter.before) {
            (None, None) => {
                sqlx::query(
                    r#"
                    SELECT id, user_id, kind, payload, created_at, read_at
                      FROM notif.notifications
                     WHERE user_id = $1::uuid
                     ORDER BY created_at DESC
                     LIMIT $2
                    "#,
                )
                .bind(user_id)
                .bind(limit)
                .fetch_all(self.pool.as_ref())
                .await
            }
            (Some(true), None) => {
                sqlx::query(
                    r#"
                    SELECT id, user_id, kind, payload, created_at, read_at
                      FROM notif.notifications
                     WHERE user_id = $1::uuid AND read_at IS NULL
                     ORDER BY created_at DESC
                     LIMIT $2
                    "#,
                )
                .bind(user_id)
                .bind(limit)
                .fetch_all(self.pool.as_ref())
                .await
            }
            (Some(false), None) => {
                sqlx::query(
                    r#"
                    SELECT id, user_id, kind, payload, created_at, read_at
                      FROM notif.notifications
                     WHERE user_id = $1::uuid AND read_at IS NOT NULL
                     ORDER BY created_at DESC
                     LIMIT $2
                    "#,
                )
                .bind(user_id)
                .bind(limit)
                .fetch_all(self.pool.as_ref())
                .await
            }
            (None, Some(before)) => {
                sqlx::query(
                    r#"
                    SELECT id, user_id, kind, payload, created_at, read_at
                      FROM notif.notifications
                     WHERE user_id = $1::uuid AND created_at < $2
                     ORDER BY created_at DESC
                     LIMIT $3
                    "#,
                )
                .bind(user_id)
                .bind(before)
                .bind(limit)
                .fetch_all(self.pool.as_ref())
                .await
            }
            (Some(true), Some(before)) => {
                sqlx::query(
                    r#"
                    SELECT id, user_id, kind, payload, created_at, read_at
                      FROM notif.notifications
                     WHERE user_id = $1::uuid AND read_at IS NULL AND created_at < $2
                     ORDER BY created_at DESC
                     LIMIT $3
                    "#,
                )
                .bind(user_id)
                .bind(before)
                .bind(limit)
                .fetch_all(self.pool.as_ref())
                .await
            }
            (Some(false), Some(before)) => {
                sqlx::query(
                    r#"
                    SELECT id, user_id, kind, payload, created_at, read_at
                      FROM notif.notifications
                     WHERE user_id = $1::uuid AND read_at IS NOT NULL AND created_at < $2
                     ORDER BY created_at DESC
                     LIMIT $3
                    "#,
                )
                .bind(user_id)
                .bind(before)
                .bind(limit)
                .fetch_all(self.pool.as_ref())
                .await
            }
        }
        .map_err(|e| db_err("list_for_user", e))?;

        rows.iter().map(Self::map_row).collect()
    }

    async fn count_unread_for_user(&self, user_id: Uuid) -> Result<i64, DomainError> {
        let row = sqlx::query(
            r#"
            SELECT COUNT(*)::bigint AS c
              FROM notif.notifications
             WHERE user_id = $1::uuid AND read_at IS NULL
            "#,
        )
        .bind(user_id)
        .fetch_one(self.pool.as_ref())
        .await
        .map_err(|e| db_err("count_unread_for_user", e))?;
        row.try_get::<i64, _>("c")
            .map_err(|e| db_err("count_unread_for_user.map", e))
    }

    async fn mark_read(
        &self,
        notification_id: Uuid,
        user_id: Uuid,
        at: DateTime<Utc>,
    ) -> Result<bool, DomainError> {
        // Guard on read_at IS NULL so a re-issued call from a client
        // that's already ack'd the row is a no-op instead of stamping
        // a later timestamp over the earlier one.
        let res = sqlx::query(
            r#"
            UPDATE notif.notifications
               SET read_at = $3
             WHERE id = $1::uuid
               AND user_id = $2::uuid
               AND read_at IS NULL
            "#,
        )
        .bind(notification_id)
        .bind(user_id)
        .bind(at)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| db_err("mark_read", e))?;
        Ok(res.rows_affected() == 1)
    }

    async fn mark_all_read_for_user(
        &self,
        user_id: Uuid,
        at: DateTime<Utc>,
    ) -> Result<u64, DomainError> {
        let res = sqlx::query(
            r#"
            UPDATE notif.notifications
               SET read_at = $2
             WHERE user_id = $1::uuid AND read_at IS NULL
            "#,
        )
        .bind(user_id)
        .bind(at)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| db_err("mark_all_read_for_user", e))?;
        Ok(res.rows_affected())
    }

    async fn delete_by_id(
        &self,
        notification_id: Uuid,
        user_id: Uuid,
    ) -> Result<bool, DomainError> {
        let res = sqlx::query(
            r#"
            DELETE FROM notif.notifications
             WHERE id = $1::uuid AND user_id = $2::uuid
            "#,
        )
        .bind(notification_id)
        .bind(user_id)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| db_err("delete_by_id", e))?;
        Ok(res.rows_affected() == 1)
    }

    async fn purge_read_before(&self, cutoff: DateTime<Utc>) -> Result<u64, DomainError> {
        let res = sqlx::query(
            r#"
            DELETE FROM notif.notifications
             WHERE read_at IS NOT NULL AND read_at < $1
            "#,
        )
        .bind(cutoff)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| db_err("purge_read_before", e))?;
        Ok(res.rows_affected())
    }
}
