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
    /// OIDC session identifier (sid claim). Populated only when the IdP
    /// emits it. Enables per-device Back-Channel Logout — without it, a
    /// BCL notification would revoke all of the user's sessions rather
    /// than just the one that logged out on the far end.
    oidc_sid: Option<String>,
    /// DPoP JWK thumbprint (RFC 7638, base64url-encoded SHA-256) binding
    /// this session to a browser-held keypair. `None` for app-password
    /// / Nextcloud-client / pre-DPoP / unbound sessions — the DPoP
    /// middleware exempts them (see `docs/plan/dpop.md`).
    ///
    /// Immutable per-session: set at construction time, never updated.
    /// Downgrading a bound session by clearing the thumbprint would let
    /// a stolen cookie replay without the private key — the whole point
    /// of the binding is to prevent that.
    dpop_jkt: Option<String>,
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
            oidc_sid: None,
            dpop_jkt: None,
        }
    }

    /// Bind the session to a DPoP-Nonce browser keypair. Called by every
    /// login handler when the client presented a well-formed thumbprint
    /// in its login request. Absent → session stays unbound (fail-open).
    ///
    /// Immutable once set: this method panics if called on a session
    /// that already has a thumbprint, so a callsite mistake can't
    /// silently overwrite the binding.
    pub fn with_dpop_jkt(mut self, jkt: String) -> Self {
        if self.dpop_jkt.is_some() {
            panic!("Session.dpop_jkt is immutable — call at construction time only");
        }
        self.dpop_jkt = Some(jkt);
        self
    }

    /// Attach an OIDC ID token — call on sessions minted via the OIDC exchange.
    /// The token is persisted with the session and re-emitted at logout as
    /// `id_token_hint` so the IdP can end its own SSO session.
    pub fn with_oidc_id_token(mut self, id_token: String) -> Self {
        self.oidc_id_token = Some(id_token);
        self
    }

    /// Attach the OIDC session identifier from the id_token's `sid` claim.
    /// Optional even for OIDC sessions — only present when the IdP emits
    /// sid (Keycloak requires "Backchannel Logout Session Required" on the
    /// client). Without it, Back-Channel Logout falls back to sub-based
    /// revocation which is coarser (all of the user's OxiCloud sessions).
    pub fn with_oidc_sid(mut self, sid: String) -> Self {
        self.oidc_sid = Some(sid);
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
        oidc_sid: Option<String>,
        dpop_jkt: Option<String>,
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
            oidc_sid,
            dpop_jkt,
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

    pub fn oidc_sid(&self) -> Option<&str> {
        self.oidc_sid.as_deref()
    }

    pub fn dpop_jkt(&self) -> Option<&str> {
        self.dpop_jkt.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_session() -> Session {
        Session::new(
            Uuid::new_v4(),
            "refresh-token".to_string(),
            None,
            None,
            30,
            Uuid::new_v4(),
        )
    }

    #[test]
    fn new_session_has_no_dpop_binding() {
        assert_eq!(fresh_session().dpop_jkt(), None);
    }

    #[test]
    fn with_dpop_jkt_stores_thumbprint() {
        let s = fresh_session().with_dpop_jkt("abc123".to_string());
        assert_eq!(s.dpop_jkt(), Some("abc123"));
    }

    #[test]
    #[should_panic(expected = "Session.dpop_jkt is immutable")]
    fn with_dpop_jkt_rejects_double_bind() {
        fresh_session()
            .with_dpop_jkt("first".to_string())
            .with_dpop_jkt("second".to_string());
    }

    #[test]
    fn from_raw_round_trips_dpop_jkt() {
        let s = Session::from_raw(
            Uuid::new_v4(),
            Uuid::new_v4(),
            "token".to_string(),
            Utc::now(),
            None,
            None,
            Utc::now(),
            false,
            Uuid::new_v4(),
            None,
            None,
            Some("thumbprint-xyz".to_string()),
        );
        assert_eq!(s.dpop_jkt(), Some("thumbprint-xyz"));
    }
}
