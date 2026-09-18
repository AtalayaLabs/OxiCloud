//! PostgreSQL repository for `collab.doc_sessions`.
//!
//! Concrete impl of [`DocSessionRepository`](crate::application::ports::collab_ports::DocSessionRepository).
//! The service (`CollabSessionService`) exercises this through the trait
//! for testability; unit tests use `MockDocSessionRepository` from the
//! `test_utils` feature.
//!
//! Migration: `migrations/20261027000000_collab_doc_sessions.sql`.

use async_trait::async_trait;
use sqlx::{PgPool, Row};
use std::sync::Arc;
use uuid::Uuid;

use crate::application::ports::collab_ports::{DocSessionRepository, StoredDocSession};
use crate::common::errors::DomainError;

pub struct CollabDocSessionPgRepository {
    pool: Arc<PgPool>,
}

impl CollabDocSessionPgRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    fn pool(&self) -> &PgPool {
        &self.pool
    }
}

#[async_trait]
impl DocSessionRepository for CollabDocSessionPgRepository {
    async fn load(&self, file_id: Uuid) -> Result<Option<StoredDocSession>, DomainError> {
        let row = sqlx::query(
            r#"
            SELECT file_id,
                   state,
                   state_vector,
                   updates_since_snapshot,
                   last_flushed_content_hash,
                   last_flushed_at,
                   last_activity_at,
                   created_at
              FROM collab.doc_sessions
             WHERE file_id = $1
            "#,
        )
        .bind(file_id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| DomainError::internal_error("CollabDocSessionPg", format!("load: {e}")))?;

        Ok(row.map(|r| StoredDocSession {
            file_id: r.get("file_id"),
            state: r.get("state"),
            state_vector: r.get("state_vector"),
            updates_since_snapshot: r.get("updates_since_snapshot"),
            last_flushed_content_hash: r.try_get("last_flushed_content_hash").ok(),
            last_flushed_at: r.try_get("last_flushed_at").ok(),
            last_activity_at: r.get("last_activity_at"),
            created_at: r.get("created_at"),
        }))
    }

    async fn save_snapshot(
        &self,
        file_id: Uuid,
        state: &[u8],
        state_vector: &[u8],
    ) -> Result<(), DomainError> {
        // Upsert: first-attach path inserts the row; every subsequent
        // compaction updates state+state_vector and resets the counter.
        // `last_activity_at` bumps to NOW() so the idle-GC scan starts
        // its clock from this moment. `created_at` uses DEFAULT NOW()
        // on insert; kept unchanged on update via the WHERE-key clause.
        sqlx::query(
            r#"
            INSERT INTO collab.doc_sessions (file_id, state, state_vector, updates_since_snapshot, last_activity_at)
                 VALUES ($1, $2, $3, 0, NOW())
            ON CONFLICT (file_id) DO UPDATE
                    SET state = EXCLUDED.state,
                        state_vector = EXCLUDED.state_vector,
                        updates_since_snapshot = 0,
                        last_activity_at = NOW()
            "#,
        )
        .bind(file_id)
        .bind(state)
        .bind(state_vector)
        .execute(self.pool())
        .await
        .map_err(|e| DomainError::internal_error("CollabDocSessionPg", format!("save_snapshot: {e}")))?;

        Ok(())
    }

    async fn touch_after_update(&self, file_id: Uuid) -> Result<(), DomainError> {
        // Hot-path write — fires per applied update between snapshots.
        // Kept minimal (just two counter/timestamp bumps) so it doesn't
        // dominate the actor's write budget under a busy doc.
        sqlx::query(
            r#"
            UPDATE collab.doc_sessions
               SET updates_since_snapshot = updates_since_snapshot + 1,
                   last_activity_at = NOW()
             WHERE file_id = $1
            "#,
        )
        .bind(file_id)
        .execute(self.pool())
        .await
        .map_err(|e| {
            DomainError::internal_error("CollabDocSessionPg", format!("touch_after_update: {e}"))
        })?;

        Ok(())
    }

    async fn record_flush(&self, file_id: Uuid, content_hash: &str) -> Result<(), DomainError> {
        sqlx::query(
            r#"
            UPDATE collab.doc_sessions
               SET last_flushed_content_hash = $2,
                   last_flushed_at = NOW()
             WHERE file_id = $1
            "#,
        )
        .bind(file_id)
        .bind(content_hash)
        .execute(self.pool())
        .await
        .map_err(|e| {
            DomainError::internal_error("CollabDocSessionPg", format!("record_flush: {e}"))
        })?;

        Ok(())
    }

    async fn delete(&self, file_id: Uuid) -> Result<(), DomainError> {
        sqlx::query("DELETE FROM collab.doc_sessions WHERE file_id = $1")
            .bind(file_id)
            .execute(self.pool())
            .await
            .map_err(|e| {
                DomainError::internal_error("CollabDocSessionPg", format!("delete: {e}"))
            })?;

        Ok(())
    }
}
