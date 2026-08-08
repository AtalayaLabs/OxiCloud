use chrono::Utc;
use futures::future::BoxFuture;
use sqlx::{PgPool, Row};
use std::sync::Arc;
use uuid::Uuid;

use crate::application::ports::auth_ports::SessionStoragePort;
use crate::common::errors::DomainError;
use crate::domain::entities::session::Session;
use crate::domain::repositories::session_repository::{
    SessionRepository, SessionRepositoryError, SessionRepositoryResult,
};
use crate::infrastructure::repositories::pg::transaction_utils::with_transaction;

// Implement From<sqlx::Error> for SessionRepositoryError to allow automatic conversions
impl From<sqlx::Error> for SessionRepositoryError {
    fn from(err: sqlx::Error) -> Self {
        SessionPgRepository::map_sqlx_error(err)
    }
}

pub struct SessionPgRepository {
    pool: Arc<PgPool>,
}

impl SessionPgRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    // Helper method to map SQL errors to domain errors
    pub fn map_sqlx_error(err: sqlx::Error) -> SessionRepositoryError {
        match err {
            sqlx::Error::RowNotFound => {
                SessionRepositoryError::NotFound("Session not found".to_string())
            }
            _ => SessionRepositoryError::DatabaseError(format!("Database error: {}", err)),
        }
    }
}

impl SessionRepository for SessionPgRepository {
    /// Creates a new session using a transaction
    async fn create_session(&self, session: Session) -> SessionRepositoryResult<Session> {
        // Create a copy of the session for the closure
        let session_clone = session.clone();

        with_transaction(&self.pool, "create_session", |tx| {
            Box::pin(async move {
                // Insert the session
                sqlx::query(
                    r#"
                        INSERT INTO auth.sessions (
                            id, user_id, refresh_token, expires_at,
                            ip_address, user_agent, created_at, revoked, family_id,
                            oidc_id_token, oidc_sid, dpop_jkt
                        ) VALUES (
                            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12
                        )
                        "#,
                )
                .bind(session_clone.id())
                .bind(session_clone.user_id())
                .bind(session_clone.refresh_token())
                .bind(session_clone.expires_at())
                .bind(session_clone.ip_address())
                .bind(session_clone.user_agent())
                .bind(session_clone.created_at())
                .bind(session_clone.is_revoked())
                .bind(session_clone.family_id())
                .bind(session_clone.oidc_id_token())
                .bind(session_clone.oidc_sid())
                .bind(session_clone.dpop_jkt())
                .execute(&mut **tx)
                .await
                .map_err(Self::map_sqlx_error)?;

                // Optionally, update the user's last login
                // within the same transaction
                sqlx::query(
                    r#"
                        UPDATE auth.users
                        SET last_login_at = NOW(), updated_at = NOW()
                        WHERE id = $1
                        "#,
                )
                .bind(session_clone.user_id())
                .execute(&mut **tx)
                .await
                .map_err(|e| {
                    // Convert the error but without interrupting session
                    // creation if the update fails
                    tracing::warn!(
                        "Could not update last_login_at for user {}: {}",
                        session_clone.user_id(),
                        e
                    );
                    SessionRepositoryError::DatabaseError(format!(
                        "Session created but could not update last_login_at: {}",
                        e
                    ))
                })?;

                Ok(session_clone)
            }) as BoxFuture<'_, SessionRepositoryResult<Session>>
        })
        .await?;

        Ok(session)
    }

    /// Gets a session by ID
    async fn get_session_by_id(&self, id: Uuid) -> SessionRepositoryResult<Session> {
        let row = sqlx::query(
            r#"
            SELECT
                id, user_id, refresh_token, expires_at,
                ip_address, user_agent, created_at, revoked, family_id,
                oidc_id_token, oidc_sid, dpop_jkt
            FROM auth.sessions
            WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_one(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        Ok(Session::from_raw(
            row.get("id"),
            row.get("user_id"),
            row.get("refresh_token"),
            row.get("expires_at"),
            row.get("ip_address"),
            row.get("user_agent"),
            row.get("created_at"),
            row.get("revoked"),
            row.get("family_id"),
            row.get("oidc_id_token"),
            row.get("oidc_sid"),
            row.get("dpop_jkt"),
        ))
    }

    /// Gets a session by refresh token — returns revoked sessions too so the
    /// application layer can distinguish "not found" from "replayed revoked token".
    async fn get_session_by_refresh_token(
        &self,
        refresh_token: &str,
    ) -> SessionRepositoryResult<Session> {
        let row = sqlx::query(
            r#"
            SELECT
                id, user_id, refresh_token, expires_at,
                ip_address, user_agent, created_at, revoked, family_id,
                oidc_id_token, oidc_sid, dpop_jkt
            FROM auth.sessions
            WHERE refresh_token = $1
            "#,
        )
        .bind(refresh_token)
        .fetch_one(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        Ok(Session::from_raw(
            row.get("id"),
            row.get("user_id"),
            row.get("refresh_token"),
            row.get("expires_at"),
            row.get("ip_address"),
            row.get("user_agent"),
            row.get("created_at"),
            row.get("revoked"),
            row.get("family_id"),
            row.get("oidc_id_token"),
            row.get("oidc_sid"),
            row.get("dpop_jkt"),
        ))
    }

    /// Gets all sessions for a user
    async fn get_sessions_by_user_id(
        &self,
        user_id: Uuid,
    ) -> SessionRepositoryResult<Vec<Session>> {
        let rows = sqlx::query(
            r#"
            SELECT
                id, user_id, refresh_token, expires_at,
                ip_address, user_agent, created_at, revoked, family_id,
                oidc_id_token, oidc_sid, dpop_jkt
            FROM auth.sessions
            WHERE user_id = $1
            ORDER BY created_at DESC
            "#,
        )
        .bind(user_id)
        .fetch_all(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        let sessions = rows
            .into_iter()
            .map(|row| {
                Session::from_raw(
                    row.get("id"),
                    row.get("user_id"),
                    row.get("refresh_token"),
                    row.get("expires_at"),
                    row.get("ip_address"),
                    row.get("user_agent"),
                    row.get("created_at"),
                    row.get("revoked"),
                    row.get("family_id"),
                    row.get("oidc_id_token"),
                    row.get("oidc_sid"),
                    row.get("dpop_jkt"),
                )
            })
            .collect();

        Ok(sessions)
    }

    /// Revokes a specific session using a transaction
    async fn revoke_session(&self, session_id: Uuid) -> SessionRepositoryResult<()> {
        let id = session_id; // Copy for use in closure

        with_transaction(&self.pool, "revoke_session", |tx| {
            Box::pin(async move {
                // Revoke the session
                let result = sqlx::query(
                    r#"
                        UPDATE auth.sessions
                        SET revoked = true
                        WHERE id = $1
                        RETURNING user_id
                        "#,
                )
                .bind(id)
                .fetch_optional(&mut **tx)
                .await
                .map_err(Self::map_sqlx_error)?;

                // If we found the session, we can log a security event
                if let Some(row) = result {
                    let user_id: Uuid = row.try_get("user_id").unwrap_or_default();

                    // Log security event (in a security table)
                    // This is optional but shows how additional operations
                    // can be performed in the same transaction
                    tracing::info!("Session with ID {} for user {} revoked", id, user_id);
                }

                Ok(())
            }) as BoxFuture<'_, SessionRepositoryResult<()>>
        })
        .await
    }

    /// Revokes all sessions for a user using a transaction
    async fn revoke_all_user_sessions(&self, user_id: Uuid) -> SessionRepositoryResult<u64> {
        let user_id_copy = user_id; // Copy for use in closure

        with_transaction(&self.pool, "revoke_all_user_sessions", |tx| {
            Box::pin(async move {
                // Revoke all sessions for the user
                let result = sqlx::query(
                    r#"
                        UPDATE auth.sessions
                        SET revoked = true
                        WHERE user_id = $1 AND revoked = false
                        "#,
                )
                .bind(user_id_copy)
                .execute(&mut **tx)
                .await
                .map_err(Self::map_sqlx_error)?;

                let affected = result.rows_affected();

                // Log security event
                if affected > 0 {
                    tracing::info!("Revoked {} sessions for user {}", affected, user_id_copy);
                }

                Ok(affected)
            }) as BoxFuture<'_, SessionRepositoryResult<u64>>
        })
        .await
    }

    async fn revoke_other_user_sessions(
        &self,
        user_id: Uuid,
        keep_session_id: Uuid,
    ) -> SessionRepositoryResult<u64> {
        // Classic "password change" revocation: kill every OTHER
        // session for this user so a stolen credential elsewhere is
        // invalidated, but leave the caller's own session alive so
        // the SPA can complete follow-up work (envelope re-register,
        // etc.) without racing a session-death 401.
        let user_id_copy = user_id;
        let keep = keep_session_id;
        with_transaction(&self.pool, "revoke_other_user_sessions", |tx| {
            Box::pin(async move {
                let result = sqlx::query(
                    r#"
                    UPDATE auth.sessions
                       SET revoked = true
                     WHERE user_id  = $1
                       AND id      != $2
                       AND revoked  = false
                    "#,
                )
                .bind(user_id_copy)
                .bind(keep)
                .execute(&mut **tx)
                .await
                .map_err(Self::map_sqlx_error)?;

                let affected = result.rows_affected();
                if affected > 0 {
                    tracing::info!(
                        "Revoked {} other sessions for user {} (kept {})",
                        affected,
                        user_id_copy,
                        keep
                    );
                }
                Ok(affected)
            }) as BoxFuture<'_, SessionRepositoryResult<u64>>
        })
        .await
    }

    /// Revokes all sessions in a token family (theft response)
    async fn revoke_session_family(&self, family_id: Uuid) -> SessionRepositoryResult<u64> {
        let result = sqlx::query(
            r#"
            UPDATE auth.sessions
            SET revoked = true
            WHERE family_id = $1 AND revoked = false
            "#,
        )
        .bind(family_id)
        .execute(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        let affected = result.rows_affected();
        if affected > 0 {
            tracing::warn!(
                "Token reuse detected: revoked {} session(s) in family {}",
                affected,
                family_id
            );
        }
        Ok(affected)
    }

    /// Back-Channel Logout — revoke sessions matched by the IdP-supplied
    /// `sid`. Filters `NOT revoked` so double-notifications are idempotent
    /// (returning empty second time). Only session rows with a non-null
    /// oidc_sid ever match, so this is safe against sid values happening
    /// to collide with anything else.
    async fn revoke_sessions_by_oidc_sid(&self, sid: &str) -> SessionRepositoryResult<Vec<Uuid>> {
        let rows = sqlx::query(
            r#"
            UPDATE auth.sessions
            SET revoked = true
            WHERE oidc_sid = $1 AND NOT revoked
            RETURNING user_id
            "#,
        )
        .bind(sid)
        .fetch_all(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        let user_ids: Vec<Uuid> = rows.iter().map(|r| r.get("user_id")).collect();
        if !user_ids.is_empty() {
            tracing::info!(
                target: "audit",
                event = "oidc.backchannel_logout_by_sid",
                sid = %sid,
                revoked_count = user_ids.len(),
                "👮🏻‍♂️ OIDC backchannel-logout revoked sessions by sid"
            );
        }
        Ok(user_ids)
    }

    /// Back-Channel Logout fallback — the IdP omitted `sid` in the
    /// logout_token, so we revoke every session belonging to the user
    /// identified by (federation_issuer, federation_subject). Users are
    /// looked up through auth.users; the query is federation-kind-agnostic
    /// today (matches any user regardless of federation_kind) — an OCM
    /// notification arriving here would find only OIDC users because that's
    /// the only path that writes federation_issuer today, but the schema
    /// admits OCM rows too, so this method may see multi-kind matches once
    /// OCM lands. Restrict by federation_kind then if that becomes ambiguous.
    async fn revoke_user_sessions_by_federation_subject(
        &self,
        issuer: &str,
        subject: &str,
    ) -> SessionRepositoryResult<Option<Uuid>> {
        // Two-step: look up the user first (deterministic error class if
        // the user is unknown), then revoke. Combining into a single
        // UPDATE-FROM would work but the audit log wants the user_id
        // separately from the revocation count.
        let user_row = sqlx::query(
            r#"
            SELECT id FROM auth.users
            WHERE federation_issuer = $1 AND federation_subject = $2
            "#,
        )
        .bind(issuer)
        .bind(subject)
        .fetch_optional(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        let Some(row) = user_row else {
            return Ok(None);
        };
        let user_id: Uuid = row.get("id");

        let result = sqlx::query(
            r#"
            UPDATE auth.sessions
            SET revoked = true
            WHERE user_id = $1 AND NOT revoked
            "#,
        )
        .bind(user_id)
        .execute(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        tracing::info!(
            target: "audit",
            event = "federation.backchannel_logout_by_sub",
            federation_issuer = %issuer,
            federation_subject = %subject,
            user_id = %user_id,
            revoked_count = result.rows_affected(),
            "👮🏻‍♂️ OIDC backchannel-logout revoked all user sessions by sub"
        );
        Ok(Some(user_id))
    }

    /// Deletes expired sessions
    async fn delete_expired_sessions(&self) -> SessionRepositoryResult<u64> {
        let now = Utc::now();

        let result = sqlx::query(
            r#"
            DELETE FROM auth.sessions
            WHERE expires_at < $1
            "#,
        )
        .bind(now)
        .execute(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        Ok(result.rows_affected())
    }

    async fn bind_dpop_jkt(
        &self,
        session_id: Uuid,
        dpop_jkt: &str,
    ) -> SessionRepositoryResult<()> {
        // `WHERE dpop_jkt IS NULL` enforces the immutability invariant
        // at the SQL level — a bound session's UPDATE affects 0 rows
        // and we surface `DpopAlreadyBound`. Also guards against a
        // stolen cookie replaying the bind endpoint with the
        // attacker's own thumbprint on an already-bound session.
        let result = sqlx::query(
            r#"
            UPDATE auth.sessions
            SET dpop_jkt = $2
            WHERE id = $1 AND dpop_jkt IS NULL
            "#,
        )
        .bind(session_id)
        .bind(dpop_jkt)
        .execute(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        if result.rows_affected() == 0 {
            // Distinguish "session gone" from "already bound" — the
            // caller (bind endpoint) returns different HTTP shapes.
            // A tiny extra SELECT here is worth the disambiguation
            // because both cases are rare.
            let row = sqlx::query("SELECT dpop_jkt FROM auth.sessions WHERE id = $1")
                .bind(session_id)
                .fetch_optional(&*self.pool)
                .await
                .map_err(Self::map_sqlx_error)?;
            return match row {
                None => Err(SessionRepositoryError::NotFound(session_id.to_string())),
                Some(_) => Err(SessionRepositoryError::DpopAlreadyBound),
            };
        }

        Ok(())
    }
}

// Implementation of the storage port for the application layer
impl SessionStoragePort for SessionPgRepository {
    async fn create_session(&self, session: Session) -> Result<Session, DomainError> {
        SessionRepository::create_session(self, session)
            .await
            .map_err(DomainError::from)
    }

    /// Revoke + insert + last-login stamp in ONE transaction — the refresh
    /// rotation used to pay two full BEGIN/COMMIT round-trip pairs
    /// (`revoke_session` then `create_session`) per token refresh.
    async fn rotate_session(
        &self,
        old_session_id: Uuid,
        new_session: Session,
    ) -> Result<Session, DomainError> {
        let session_clone = new_session.clone();
        with_transaction(&self.pool, "rotate_session", |tx| {
            Box::pin(async move {
                sqlx::query("UPDATE auth.sessions SET revoked = true WHERE id = $1")
                    .bind(old_session_id)
                    .execute(&mut **tx)
                    .await
                    .map_err(Self::map_sqlx_error)?;

                sqlx::query(
                    r#"
                        INSERT INTO auth.sessions (
                            id, user_id, refresh_token, expires_at,
                            ip_address, user_agent, created_at, revoked, family_id,
                            oidc_id_token, oidc_sid, dpop_jkt
                        ) VALUES (
                            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12
                        )
                        "#,
                )
                .bind(session_clone.id())
                .bind(session_clone.user_id())
                .bind(session_clone.refresh_token())
                .bind(session_clone.expires_at())
                .bind(session_clone.ip_address())
                .bind(session_clone.user_agent())
                .bind(session_clone.created_at())
                .bind(session_clone.is_revoked())
                .bind(session_clone.family_id())
                .bind(session_clone.oidc_id_token())
                .bind(session_clone.oidc_sid())
                .bind(session_clone.dpop_jkt())
                .execute(&mut **tx)
                .await
                .map_err(Self::map_sqlx_error)?;

                sqlx::query(
                    r#"
                        UPDATE auth.users
                        SET last_login_at = NOW(), updated_at = NOW()
                        WHERE id = $1
                        "#,
                )
                .bind(session_clone.user_id())
                .execute(&mut **tx)
                .await
                .map_err(|e| {
                    tracing::warn!(
                        "Could not update last_login_at for user {}: {}",
                        session_clone.user_id(),
                        e
                    );
                    SessionRepositoryError::DatabaseError(format!(
                        "Session rotated but could not update last_login_at: {}",
                        e
                    ))
                })?;

                Ok(session_clone)
            }) as BoxFuture<'_, SessionRepositoryResult<Session>>
        })
        .await
        .map_err(DomainError::from)?;

        Ok(new_session)
    }

    async fn get_session_by_refresh_token(
        &self,
        refresh_token: &str,
    ) -> Result<Session, DomainError> {
        SessionRepository::get_session_by_refresh_token(self, refresh_token)
            .await
            .map_err(DomainError::from)
    }

    async fn revoke_session(&self, session_id: Uuid) -> Result<(), DomainError> {
        SessionRepository::revoke_session(self, session_id)
            .await
            .map_err(DomainError::from)
    }

    async fn revoke_all_user_sessions(&self, user_id: Uuid) -> Result<u64, DomainError> {
        SessionRepository::revoke_all_user_sessions(self, user_id)
            .await
            .map_err(DomainError::from)
    }

    async fn revoke_other_user_sessions(
        &self,
        user_id: Uuid,
        keep_session_id: Uuid,
    ) -> Result<u64, DomainError> {
        SessionRepository::revoke_other_user_sessions(self, user_id, keep_session_id)
            .await
            .map_err(DomainError::from)
    }

    async fn revoke_session_family(&self, family_id: Uuid) -> Result<u64, DomainError> {
        SessionRepository::revoke_session_family(self, family_id)
            .await
            .map_err(DomainError::from)
    }

    async fn revoke_sessions_by_oidc_sid(&self, sid: &str) -> Result<Vec<Uuid>, DomainError> {
        SessionRepository::revoke_sessions_by_oidc_sid(self, sid)
            .await
            .map_err(DomainError::from)
    }

    async fn revoke_user_sessions_by_federation_subject(
        &self,
        issuer: &str,
        subject: &str,
    ) -> Result<Option<Uuid>, DomainError> {
        SessionRepository::revoke_user_sessions_by_federation_subject(self, issuer, subject)
            .await
            .map_err(DomainError::from)
    }

    async fn bind_dpop_jkt(
        &self,
        session_id: Uuid,
        dpop_jkt: &str,
    ) -> Result<(), DomainError> {
        SessionRepository::bind_dpop_jkt(self, session_id, dpop_jkt)
            .await
            .map_err(DomainError::from)
    }
}
