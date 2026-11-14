//! `storage.link_events`: (subject, resource) rows queued by triggers for the `links_settle` job.

use crate::common::errors::DomainError;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct LinkEvent {
    pub id: i64,
    pub subject_type: String,
    pub subject_id: Uuid,
    pub resource_type: String,
    pub resource_id: Uuid,
    pub cursor: Option<Uuid>,
}

pub struct LinkEventsPgRepository {
    pool: Arc<PgPool>,
}

impl LinkEventsPgRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    fn err(op: &str, e: sqlx::Error) -> DomainError {
        DomainError::internal_error("LinkEventsDb", format!("{op}: {e}"))
    }

    pub async fn next(&self) -> Result<Option<LinkEvent>, DomainError> {
        self.next_after(0).await
    }

    pub async fn next_after(&self, id: i64) -> Result<Option<LinkEvent>, DomainError> {
        sqlx::query_as::<_, LinkEvent>(
            "SELECT id, subject_type, subject_id, resource_type, resource_id, cursor \
               FROM storage.link_events WHERE id > $1 ORDER BY id LIMIT 1",
        )
        .bind(id)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| Self::err("next", e))
    }

    pub async fn checkpoint(&self, id: i64, cursor: Uuid) -> Result<(), DomainError> {
        sqlx::query("UPDATE storage.link_events SET cursor = $2 WHERE id = $1")
            .bind(id)
            .bind(cursor)
            .execute(&*self.pool)
            .await
            .map(|_| ())
            .map_err(|e| Self::err("checkpoint", e))
    }

    pub async fn delete(&self, id: i64) -> Result<(), DomainError> {
        sqlx::query("DELETE FROM storage.link_events WHERE id = $1")
            .bind(id)
            .execute(&*self.pool)
            .await
            .map(|_| ())
            .map_err(|e| Self::err("delete", e))
    }

    pub async fn pending(&self) -> Result<i64, DomainError> {
        sqlx::query_scalar("SELECT COUNT(*) FROM storage.link_events")
            .fetch_one(&*self.pool)
            .await
            .map_err(|e| Self::err("pending", e))
    }

    pub async fn enqueue(
        &self,
        subject_type: &str,
        subject_id: Uuid,
        resource_type: &str,
        resource_id: Uuid,
    ) -> Result<(), DomainError> {
        sqlx::query(
            "INSERT INTO storage.link_events (subject_type, subject_id, resource_type, resource_id) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(subject_type)
        .bind(subject_id)
        .bind(resource_type)
        .bind(resource_id)
        .execute(&*self.pool)
        .await
        .map(|_| ())
        .map_err(|e| Self::err("enqueue", e))
    }

    /// One event per role_grants row (user/group, folder/drive) with `from < expires_at <= now()`, where
    /// `from` defaults to 30 days before the database clock; returns (rows enqueued, the database `now()` used).
    pub async fn enqueue_expired_since(
        &self,
        from: Option<DateTime<Utc>>,
    ) -> Result<(u64, DateTime<Utc>), DomainError> {
        sqlx::query_as::<_, (i64, DateTime<Utc>)>(
            "WITH t AS (SELECT now() AS t), \
                  ins AS ( \
                    INSERT INTO storage.link_events (subject_type, subject_id, resource_type, resource_id) \
                    SELECT g.subject_type, g.subject_id, g.resource_type, g.resource_id \
                      FROM storage.role_grants g, t \
                     WHERE g.expires_at IS NOT NULL \
                       AND g.expires_at > COALESCE($1, t.t - interval '30 days') \
                       AND g.expires_at <= t.t \
                       AND g.subject_type IN ('user', 'group') AND g.resource_type IN ('folder', 'drive') \
                    RETURNING 1) \
             SELECT (SELECT COUNT(*) FROM ins), (SELECT t FROM t)",
        )
        .bind(from)
        .fetch_one(&*self.pool)
        .await
        .map(|(n, to)| (n as u64, to))
        .map_err(|e| Self::err("enqueue_expired_since", e))
    }

    /// One event per non-expired role_grants row (user/group, folder/drive), in one statement.
    pub async fn enqueue_all_current_grants(&self) -> Result<u64, DomainError> {
        sqlx::query_scalar::<_, i64>(
            "WITH ins AS ( \
               INSERT INTO storage.link_events (subject_type, subject_id, resource_type, resource_id) \
               SELECT g.subject_type, g.subject_id, g.resource_type, g.resource_id \
                 FROM storage.role_grants g \
                WHERE (g.expires_at IS NULL OR g.expires_at > now()) \
                  AND g.subject_type IN ('user', 'group') AND g.resource_type IN ('folder', 'drive') \
               RETURNING 1) \
             SELECT COUNT(*) FROM ins",
        )
        .fetch_one(&*self.pool)
        .await
        .map(|n| n as u64)
        .map_err(|e| Self::err("enqueue_all_current_grants", e))
    }
}

#[cfg(all(test, integration_tests))]
mod it {
    use super::*;
    use crate::mount_it_support::fresh_db;

    #[tokio::test]
    async fn link_events_next_checkpoint_delete_pending() {
        let (_c, pool) = fresh_db().await;
        let repo = LinkEventsPgRepository::new(pool.clone());
        sqlx::query("DELETE FROM storage.link_events")
            .execute(&*pool)
            .await
            .unwrap();
        assert_eq!(repo.next().await.unwrap(), None);
        let (u, f, g) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        repo.enqueue("user", u, "folder", f).await.unwrap();
        repo.enqueue("group", g, "group", g).await.unwrap();
        assert_eq!(repo.pending().await.unwrap(), 2);
        let first = repo.next().await.unwrap().unwrap();
        assert_eq!(
            (
                first.subject_type.as_str(),
                first.subject_id,
                first.resource_type.as_str()
            ),
            ("user", u, "folder")
        );
        assert_eq!(first.cursor, None);
        let c = Uuid::new_v4();
        repo.checkpoint(first.id, c).await.unwrap();
        assert_eq!(repo.next().await.unwrap().unwrap().cursor, Some(c));
        repo.delete(first.id).await.unwrap();
        let second = repo.next().await.unwrap().unwrap();
        assert_eq!(second.subject_id, g);
        assert_eq!(repo.pending().await.unwrap(), 1);
    }
}
