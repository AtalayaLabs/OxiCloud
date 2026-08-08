use crate::common::errors::DomainError;
use crate::domain::entities::session::Session;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum SessionRepositoryError {
    #[error("Session not found: {0}")]
    NotFound(String),

    #[error("Database error: {0}")]
    DatabaseError(String),

    #[error("Timeout error: {0}")]
    Timeout(String),

    /// Attempted to bind a DPoP thumbprint to a session that already
    /// carries one. Immutable-per-session invariant (see
    /// `docs/plan/dpop.md` — mutable bind would let an attacker
    /// downgrade a bound session by binding to their own key).
    #[error("Session already has a DPoP thumbprint")]
    DpopAlreadyBound,
}

pub type SessionRepositoryResult<T> = Result<T, SessionRepositoryError>;

// Conversion from SessionRepositoryError to DomainError
impl From<SessionRepositoryError> for DomainError {
    fn from(err: SessionRepositoryError) -> Self {
        match err {
            SessionRepositoryError::NotFound(msg) => DomainError::not_found("Session", msg),
            SessionRepositoryError::DatabaseError(msg) => {
                DomainError::internal_error("Database", msg)
            }
            SessionRepositoryError::Timeout(msg) => DomainError::timeout("Database", msg),
            SessionRepositoryError::DpopAlreadyBound => DomainError::new(
                crate::common::errors::ErrorKind::AlreadyExists,
                "Session",
                "This session already has a DPoP thumbprint and cannot be re-bound",
            ),
        }
    }
}

pub trait SessionRepository: Send + Sync + 'static {
    /// Creates a new session
    async fn create_session(&self, session: Session) -> SessionRepositoryResult<Session>;

    /// Gets a session by ID
    async fn get_session_by_id(&self, id: Uuid) -> SessionRepositoryResult<Session>;

    /// Gets a session by refresh token
    async fn get_session_by_refresh_token(
        &self,
        refresh_token: &str,
    ) -> SessionRepositoryResult<Session>;

    /// Gets all sessions for a user
    async fn get_sessions_by_user_id(&self, user_id: Uuid)
    -> SessionRepositoryResult<Vec<Session>>;

    /// Revokes a specific session
    async fn revoke_session(&self, session_id: Uuid) -> SessionRepositoryResult<()>;

    /// Revokes all sessions for a user
    async fn revoke_all_user_sessions(&self, user_id: Uuid) -> SessionRepositoryResult<u64>;

    /// Revokes every session for `user_id` EXCEPT the one identified by
    /// `keep_session_id`. Classic "password change" pattern: log the
    /// user out from every OTHER device, but keep the current device's
    /// session alive so the SPA can complete follow-up work (e.g. OPAQUE
    /// envelope re-registration) without a session-death race.
    ///
    /// Returns the count of revoked rows (excluding the kept one).
    /// If `keep_session_id` doesn't belong to `user_id` (defensive),
    /// the WHERE clause still matches nothing to revoke on that row —
    /// no cross-user side effect.
    async fn revoke_other_user_sessions(
        &self,
        user_id: Uuid,
        keep_session_id: Uuid,
    ) -> SessionRepositoryResult<u64>;

    /// Revokes all sessions in a token family (theft response)
    async fn revoke_session_family(&self, family_id: Uuid) -> SessionRepositoryResult<u64>;

    /// Revokes every OxiCloud session whose OIDC sid claim matches.
    ///
    /// Used by the Back-Channel Logout handler when the IdP sends a
    /// logout_token with a `sid` — this is the per-device path and
    /// matches (in the typical case) exactly one session row. Returns
    /// user IDs of every affected session so the caller can dispatch
    /// per-user lifecycle hooks.
    async fn revoke_sessions_by_oidc_sid(&self, sid: &str) -> SessionRepositoryResult<Vec<Uuid>>;

    /// Revokes every session belonging to the user identified by
    /// `(federation_issuer, federation_subject)`.
    ///
    /// Fallback path for the Back-Channel Logout handler when the IdP
    /// omits `sid` from the logout_token — coarser than sid-based
    /// revocation (kills the user's other devices too). Returns the
    /// user id of the affected account, or `None` if no matching user.
    async fn revoke_user_sessions_by_federation_subject(
        &self,
        issuer: &str,
        subject: &str,
    ) -> SessionRepositoryResult<Option<Uuid>>;

    /// Deletes expired sessions
    async fn delete_expired_sessions(&self) -> SessionRepositoryResult<u64>;

    /// One-shot bind a DPoP JWK thumbprint (RFC 7638) to a session that
    /// was created without one. Used by the post-redirect bind endpoint
    /// (`POST /api/auth/dpop/bind`) for the OIDC and magic-link flows,
    /// where the redemption is a GET and can't carry the thumbprint in
    /// its request body.
    ///
    /// Enforces the immutability invariant at the SQL level with a
    /// `WHERE dpop_jkt IS NULL` guard: if the row already carries a
    /// thumbprint the UPDATE affects zero rows and we return
    /// [`SessionRepositoryError::DpopAlreadyBound`]. That's the anti-
    /// downgrade guard from `docs/plan/dpop.md` — an attacker who has
    /// stolen the cookie of a bound session cannot re-bind to their
    /// own key.
    async fn bind_dpop_jkt(&self, session_id: Uuid, dpop_jkt: &str) -> SessionRepositoryResult<()>;
}
