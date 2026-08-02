use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct Session {
    id: Uuid,
    user_id: Uuid,
    refresh_token: String,
    expires_at: DateTime<Utc>,
    ip_address: Option<String>,
    user_agent: Option<String>,
    created_at: DateTime<Utc>,
    revoked: bool,
    /// Groups all tokens issued from the same original login.
    /// Replaying a revoked token from this family triggers full-family revocation.
    family_id: Uuid,
    /// ID token from the OIDC login exchange. Used as `id_token_hint` on the
    /// RP-initiated logout URL so the IdP can terminate its own SSO session.
    /// `None` for password / magic-link sessions.
    oidc_id_token: Option<String>,
}

impl Session {
    pub fn new(
        user_id: Uuid,
        refresh_token: String,
        ip_address: Option<String>,
        user_agent: Option<String>,
        expires_in_days: i64,
        family_id: Uuid,
    ) -> Self {
        if refresh_token.is_empty() {
            panic!("Session refresh_token cannot be empty");
        }

        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            user_id,
            refresh_token,
            expires_at: now + Duration::days(expires_in_days),
            ip_address,
            user_agent,
            created_at: now,
            revoked: false,
            family_id,
            oidc_id_token: None,
        }
    }

    /// Attach an OIDC ID token — call on sessions minted via the OIDC exchange.
    /// The token is persisted with the session and re-emitted at logout as
    /// `id_token_hint` so the IdP can end its own SSO session.
    pub fn with_oidc_id_token(mut self, id_token: String) -> Self {
        self.oidc_id_token = Some(id_token);
        self
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_raw(
        id: Uuid,
        user_id: Uuid,
        refresh_token: String,
        expires_at: DateTime<Utc>,
        ip_address: Option<String>,
        user_agent: Option<String>,
        created_at: DateTime<Utc>,
        revoked: bool,
        family_id: Uuid,
        oidc_id_token: Option<String>,
    ) -> Self {
        Self {
            id,
            user_id,
            refresh_token,
            expires_at,
            ip_address,
            user_agent,
            created_at,
            revoked,
            family_id,
            oidc_id_token,
        }
    }

    // Getters
    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn user_id(&self) -> Uuid {
        self.user_id
    }

    pub fn refresh_token(&self) -> &str {
        &self.refresh_token
    }

    pub fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }

    pub fn ip_address(&self) -> Option<&str> {
        self.ip_address.as_deref()
    }

    pub fn user_agent(&self) -> Option<&str> {
        self.user_agent.as_deref()
    }

    pub fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }

    pub fn is_expired(&self) -> bool {
        Utc::now() > self.expires_at
    }

    pub fn is_revoked(&self) -> bool {
        self.revoked
    }

    pub fn revoke(&mut self) {
        self.revoked = true;
    }

    pub fn family_id(&self) -> Uuid {
        self.family_id
    }

    pub fn oidc_id_token(&self) -> Option<&str> {
        self.oidc_id_token.as_deref()
    }
}
