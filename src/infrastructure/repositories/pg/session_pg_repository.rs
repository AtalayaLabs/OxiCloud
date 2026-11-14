use chrono::{DateTime, Utc};
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

    /// Decode one `auth.sessions` row.
    ///
    /// Extracted while adding [`find_family_successor`]: the same
    /// fourteen-field `Session::from_raw` call was already written out at
    /// four callsites in this file, and a fifth copy is how a newly added
    /// column ends up read in three places and forgotten in two. The
    /// existing callsites are left alone in this change (a security fix is
    /// the wrong diff to bury a sweep in) but should migrate here.
    ///
    /// The SELECT list must name every column below, in any order.
    fn session_from_row(row: &sqlx::postgres::PgRow) -> Session {
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
            crate::domain::entities::session::SessionOrigin::from_wire(row.get("origin")),
            row.get("last_seen_at"),
        )
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
                            oidc_id_token, oidc_sid, dpop_jkt, origin, last_seen_at
                        ) VALUES (
                            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14
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
                .bind(session_clone.origin().as_str())
                // Bound explicitly, NOT left to the column's `DEFAULT NOW()`:
                // that default is the DB clock while `created_at` above is the
                // app clock, so a fresh row always came back with
                // `last_seen_at > created_at` and anything comparing the two
                // read every brand-new session as already active. The entity
                // sets both from one `Utc::now()`, so equality now means
                // "never served a request" — which is what the refresh grace
                // tests (`docs/plan/refresh-token-grace.md`).
                .bind(session_clone.last_seen_at())
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
                oidc_id_token, oidc_sid, dpop_jkt, origin, last_seen_at
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
            crate::domain::entities::session::SessionOrigin::from_wire(row.get("origin")),
            row.get("last_seen_at"),
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
                oidc_id_token, oidc_sid, dpop_jkt, origin, last_seen_at
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
            crate::domain::entities::session::SessionOrigin::from_wire(row.get("origin")),
            row.get("last_seen_at"),
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
                oidc_id_token, oidc_sid, dpop_jkt, origin, last_seen_at
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
                    crate::domain::entities::session::SessionOrigin::from_wire(row.get("origin")),
                    row.get("last_seen_at"),
                )
            })
            .collect();

        Ok(sessions)
    }

    async fn find_family_successor(
        &self,
        family_id: Uuid,
        after: DateTime<Utc>,
    ) -> SessionRepositoryResult<Option<Session>> {
        // `created_at > $2` ordered ASC LIMIT 1 is the child of the rotation
        // that revoked the row created at `$2`. Safe to read as "the child"
        // because `rotate_session` is the only INSERT into an existing family
        // and it inserts exactly one row — see the trait docs.
        let row = sqlx::query(
            r#"
            SELECT
                id, user_id, refresh_token, expires_at,
                ip_address, user_agent, created_at, revoked, family_id,
                oidc_id_token, oidc_sid, dpop_jkt, origin, last_seen_at
            FROM auth.sessions
            WHERE family_id = $1 AND created_at > $2
            ORDER BY created_at ASC
            LIMIT 1
            "#,
        )
        .bind(family_id)
        .bind(after)
        .fetch_optional(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        Ok(row.as_ref().map(Self::session_from_row))
    }

    async fn list_sessions_paginated(
        &self,
        user_id_filter: Option<Uuid>,
        include_revoked: bool,
        limit: i64,
        offset: i64,
    ) -> SessionRepositoryResult<Vec<Session>> {
        // Single SQL with nullable-user-id + include-revoked flag
        // baked in as parameters, rather than four hand-forked
        // queries. `$1::uuid IS NULL` short-circuits when no filter is
        // set; `$2 OR (revoked = false AND expires_at > NOW())` folds
        // the active-only rule into one predicate. Both branches use
        // the same index (`idx_sessions_user_id`) on the filtered
        // path, and a full table scan bounded by `LIMIT` on the
        // unfiltered path — acceptable for an admin-triggered view
        // that operators paginate through.
        let rows = sqlx::query(
            r#"
            SELECT
                id, user_id, refresh_token, expires_at,
                ip_address, user_agent, created_at, revoked, family_id,
                oidc_id_token, oidc_sid, dpop_jkt, origin, last_seen_at
            FROM auth.sessions
            WHERE ($1::uuid IS NULL OR user_id = $1)
              AND ($2 OR (revoked = false AND expires_at > NOW()))
            ORDER BY created_at DESC
            LIMIT $3 OFFSET $4
            "#,
        )
        .bind(user_id_filter)
        .bind(include_revoked)
        .bind(limit)
        .bind(offset)
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
                    crate::domain::entities::session::SessionOrigin::from_wire(row.get("origin")),
                    row.get("last_seen_at"),
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

    async fn delete_sessions_expired_before(
        &self,
        cutoff: chrono::DateTime<chrono::Utc>,
    ) -> SessionRepositoryResult<u64> {
        // Same shape as `delete_expired_sessions` but the caller picks the
        // cutoff instead of it being pinned to NOW(). Lets the janitor
        // keep a forensic window past the natural session expiry — see
        // `SessionCleanupService` and [[project_session_janitor_missing]].
        let result = sqlx::query(
            r#"
            DELETE FROM auth.sessions
            WHERE expires_at < $1
            "#,
        )
        .bind(cutoff)
        .execute(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        Ok(result.rows_affected())
    }

    async fn bind_dpop_jkt(&self, session_id: Uuid, dpop_jkt: &str) -> SessionRepositoryResult<()> {
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
                            oidc_id_token, oidc_sid, dpop_jkt, origin, last_seen_at
                        ) VALUES (
                            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14
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
                .bind(session_clone.origin().as_str())
                // Bound explicitly, NOT left to the column's `DEFAULT NOW()`:
                // that default is the DB clock while `created_at` above is the
                // app clock, so a fresh row always came back with
                // `last_seen_at > created_at` and anything comparing the two
                // read every brand-new session as already active. The entity
                // sets both from one `Utc::now()`, so equality now means
                // "never served a request" — which is what the refresh grace
                // tests (`docs/plan/refresh-token-grace.md`).
                .bind(session_clone.last_seen_at())
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

    async fn find_family_successor(
        &self,
        family_id: Uuid,
        after: DateTime<Utc>,
    ) -> Result<Option<Session>, DomainError> {
        SessionRepository::find_family_successor(self, family_id, after)
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

    async fn bind_dpop_jkt(&self, session_id: Uuid, dpop_jkt: &str) -> Result<(), DomainError> {
        SessionRepository::bind_dpop_jkt(self, session_id, dpop_jkt)
            .await
            .map_err(DomainError::from)
    }

    async fn get_session_by_id(&self, session_id: Uuid) -> Result<Session, DomainError> {
        SessionRepository::get_session_by_id(self, session_id)
            .await
            .map_err(DomainError::from)
    }

    async fn list_sessions_paginated(
        &self,
        user_id_filter: Option<Uuid>,
        include_revoked: bool,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Session>, DomainError> {
        SessionRepository::list_sessions_paginated(
            self,
            user_id_filter,
            include_revoked,
            limit,
            offset,
        )
        .await
        .map_err(DomainError::from)
    }
}

#[cfg(integration_tests)]
mod integration_tests {
    //! Coverage for [`SessionPgRepository::find_family_successor`], the read
    //! the refresh grace decides on (`docs/plan/refresh-token-grace.md`).
    //!
    //! Fixtures are scoped PER TEST — own user id, own family id, own cleanup
    //! — because these modules all run concurrently against one Postgres
    //! (AGENTS.md § DB integration tests share one database). Nothing here
    //! queries "all sessions" or counts rows it does not own.
    use super::*;
    use crate::domain::entities::session::SessionOrigin;
    use crate::integration_test_support::test_db_url;
    use sqlx::postgres::PgPoolOptions;

    async fn repo() -> SessionPgRepository {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&test_db_url())
            .await
            .expect("connect to integration-test PostgreSQL");
        SessionPgRepository::new(Arc::new(pool))
    }

    async fn seed_user(repo: &SessionPgRepository, id: Uuid) {
        sqlx::query(
            r#"
            INSERT INTO auth.users (
                id, username, email, password_hash, role,
                storage_quota_bytes, storage_used_bytes,
                created_at, updated_at, last_login_at, active,
                federation_issuer, is_external
            ) VALUES (
                $1, $2, $3, NULL, 'user'::auth.userrole,
                10737418240, 0,
                now(), now(), NULL, TRUE,
                NULL, FALSE
            )
            "#,
        )
        .bind(id)
        .bind(format!("grace-{id}"))
        .bind(format!("grace-{id}@example.test"))
        .execute(repo.pool.as_ref())
        .await
        .expect("seed user");
    }

    async fn insert_session(repo: &SessionPgRepository, user_id: Uuid, family_id: Uuid) -> Session {
        let session = Session::new(
            user_id,
            format!("rt-{}", Uuid::new_v4()),
            None,
            None,
            7,
            family_id,
            SessionOrigin::Password,
        );
        SessionRepository::create_session(repo, session)
            .await
            .expect("insert session")
    }

    async fn cleanup(repo: &SessionPgRepository, user_id: Uuid) {
        // Sessions cascade from the user row.
        let _ = sqlx::query("DELETE FROM auth.users WHERE id = $1")
            .bind(user_id)
            .execute(repo.pool.as_ref())
            .await;
    }

    /// The IMMEDIATE child, not the newest row in the family. That difference
    /// IS the successor-bound semantic: once a client has rotated again, the
    /// first parent must stay dead, and it does precisely because its
    /// immediate child is revoked — a query returning the family tip would
    /// hand back a live grandchild and resurrect it.
    #[tokio::test]
    async fn returns_the_immediate_child_not_the_family_tip() {
        let repo = repo().await;
        let user_id = Uuid::new_v4();
        let family_id = Uuid::new_v4();
        seed_user(&repo, user_id).await;

        let parent = insert_session(&repo, user_id, family_id).await;
        let child = insert_session(&repo, user_id, family_id).await;
        let grandchild = insert_session(&repo, user_id, family_id).await;

        let found = SessionRepository::find_family_successor(&repo, family_id, parent.created_at())
            .await
            .expect("query")
            .expect("a successor exists");

        assert_eq!(found.id(), child.id());
        assert_ne!(found.id(), grandchild.id());

        cleanup(&repo, user_id).await;
    }

    /// Nothing newer → `None`, which is what a logout-revoked session looks
    /// like (revoked WITHOUT a rotation inserting anything) and therefore
    /// what denies it the grace.
    #[tokio::test]
    async fn returns_none_for_the_newest_session_in_a_family() {
        let repo = repo().await;
        let user_id = Uuid::new_v4();
        let family_id = Uuid::new_v4();
        seed_user(&repo, user_id).await;

        let only = insert_session(&repo, user_id, family_id).await;

        let found = SessionRepository::find_family_successor(&repo, family_id, only.created_at())
            .await
            .expect("query");

        assert!(found.is_none(), "a family tip has no successor");

        cleanup(&repo, user_id).await;
    }

    /// Families are the blast-radius boundary, so the read must not look
    /// across them: another login's rows are never this parent's child, even
    /// for the same user and even when newer.
    #[tokio::test]
    async fn ignores_rows_from_another_family() {
        let repo = repo().await;
        let user_id = Uuid::new_v4();
        let mine = Uuid::new_v4();
        let other = Uuid::new_v4();
        seed_user(&repo, user_id).await;

        let parent = insert_session(&repo, user_id, mine).await;
        // Newer, same user, different login chain.
        let _unrelated = insert_session(&repo, user_id, other).await;

        let found = SessionRepository::find_family_successor(&repo, mine, parent.created_at())
            .await
            .expect("query");

        assert!(
            found.is_none(),
            "another family's session is not a successor"
        );

        cleanup(&repo, user_id).await;
    }
}
