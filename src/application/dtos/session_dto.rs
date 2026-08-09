//! DTOs for the admin sessions panel.
//!
//! [`SessionSummaryDto`] is the wire shape returned by
//! `GET /api/admin/sessions`. It's deliberately narrower than the
//! `Session` domain entity — the `refresh_token` and any OIDC
//! ID-token payload are **never** serialized; the raw DPoP thumbprint
//! is truncated to an 8-char prefix so an admin viewing another
//! user's sessions cannot exfiltrate the full binding fingerprint.
//!
//! Enrichment (username/email lookup for each `user_id`) is
//! intentionally deferred to the SPA — it already caches the admin
//! user list, and doing the JOIN server-side would either force a
//! per-request JOIN (extra work most operators don't need) or a
//! separate batch fetch (extra round-trip). Frontend cross-references
//! `user_id` against its cached user list.

use chrono::{DateTime, Utc};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::domain::entities::session::{Session, SessionOrigin};

/// Authenticated-caller context — the caller's identity + session-
/// bound signals a service method might key off. Constructed at the
/// handler boundary from `AuthUser` and passed through unchanged;
/// keeps service signatures flat instead of accumulating parallel
/// `caller_id`, `caller_jkt`, `caller_ip` parameters. Every
/// caller-context field lives here, one place to extend.
///
/// Not admin-specific — any handler that needs caller context can
/// build one from `AuthUser`. Admin methods just happen to be the
/// first callers (sessions panel's `is_current` comparison and
/// audit lines).
#[derive(Debug, Clone)]
pub struct SessionCaller<'a> {
    /// AuthZ subject — used by `require_admin_caller` and audit lines.
    pub id: Uuid,
    /// Caller's own DPoP thumbprint from the JWT `cnf.jkt` claim.
    /// Enables the sessions panel's "you are here" highlight
    /// ([`SessionSummaryDto::is_current`]) — `None` when the caller
    /// logged in via an unbound path (legacy password without DPoP,
    /// pre-bind OIDC redirect, etc.).
    pub dpop_jkt: Option<&'a str>,
}

/// Wire shape for `GET /api/admin/sessions`. Contains everything the
/// admin table renders and **nothing the raw session entity would
/// leak** (refresh token, OIDC ID-token, full DPoP thumbprint).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SessionSummaryDto {
    pub id: Uuid,
    pub user_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
    /// `true` iff the session is DPoP-bound. Rendered as a lock icon
    /// in the admin table. Complements the auth-badges surface.
    pub is_bound: bool,
    /// First 8 chars of the DPoP thumbprint when bound, `None` otherwise.
    /// Enough to distinguish two bindings of the same user across
    /// devices at a glance; not enough to leak the full jkt.
    pub dpop_jkt_prefix: Option<String>,
    /// `true` when the row is revoked. Present because the panel has an
    /// opt-in "include revoked" checkbox — active-only listings will
    /// always show `false` here, but forensics listings need the flag.
    pub is_revoked: bool,
    /// Whether this row is currently usable — `!revoked && expires_at > now()`.
    /// Kept server-side so the SPA doesn't drift if the browser clock is off.
    pub is_active: bool,
    // NOTE: no `oidc_sid` / `oidc_sid_prefix` field. The IdP-emitted
    // sid identifies the row's upstream session and stays server-side
    // (used by Back-Channel Logout matching). Exposing even a prefix
    // earns no operator utility over what `id` / `created_at` /
    // `ip_address` / `user_agent` already give. `origin` below answers
    // "how did this session start?" cleanly.
    /// How the session was minted — see [`SessionOrigin`]. Set at
    /// INSERT time by the login handler and carried over on refresh
    /// (rotation doesn't change how the user first authenticated).
    /// Drives the admin panel's origin column + filter.
    pub origin: SessionOrigin,
    /// `true` when this row IS the caller's currently-active session —
    /// set by the service layer by comparing the row's `dpop_jkt` with
    /// the caller's own bound thumbprint. Lets the admin panel flag
    /// "revoking this cuts your own branch" so an admin doesn't
    /// accidentally log themselves out. `false` when either side is
    /// unbound (can't correlate) or when the jkts don't match.
    pub is_current: bool,
}

impl From<Session> for SessionSummaryDto {
    fn from(s: Session) -> Self {
        Self::from_session(s, None)
    }
}

impl SessionSummaryDto {
    /// Build the DTO with an optional `caller_jkt` used to compute
    /// `is_current`. Pass the admin caller's DPoP thumbprint to have
    /// the panel highlight the caller's own row; pass `None` when
    /// the caller is unbound (no jkt = no correlation) or from
    /// non-admin contexts.
    pub fn from_session(s: Session, caller_jkt: Option<&str>) -> Self {
        let is_revoked = s.is_revoked();
        let is_expired = s.is_expired();
        let jkt = s.dpop_jkt().map(|s| s.to_owned());
        let dpop_jkt_prefix = jkt.as_ref().map(|t| t.chars().take(8).collect::<String>());
        let is_current = match (jkt.as_deref(), caller_jkt) {
            (Some(row), Some(caller)) => row == caller,
            _ => false,
        };
        Self {
            id: s.id(),
            user_id: s.user_id(),
            created_at: s.created_at(),
            expires_at: s.expires_at(),
            ip_address: s.ip_address().map(str::to_owned),
            user_agent: s.user_agent().map(str::to_owned),
            is_bound: jkt.is_some(),
            dpop_jkt_prefix,
            is_revoked,
            is_active: !is_revoked && !is_expired,
            origin: s.origin(),
            is_current,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn base(revoked: bool, jkt: Option<&str>) -> Session {
        let mut s = Session::new(
            Uuid::new_v4(),
            "refresh-token".to_string(),
            Some("192.0.2.1".to_string()),
            Some("Mozilla/5.0".to_string()),
            30,
            Uuid::new_v4(),
            crate::domain::entities::session::SessionOrigin::Password,
        );
        if revoked {
            s.revoke();
        }
        if let Some(k) = jkt {
            s = s.with_dpop_jkt(k.to_string());
        }
        s
    }

    fn oidc_session(sid: &str) -> Session {
        Session::from_raw(
            Uuid::new_v4(),
            Uuid::new_v4(),
            "rt".to_string(),
            Utc::now() + Duration::days(30),
            None,
            None,
            Utc::now(),
            false,
            Uuid::new_v4(),
            Some("dummy.id.token".to_string()),
            Some(sid.to_string()),
            None,
            crate::domain::entities::session::SessionOrigin::Oidc,
        )
    }

    #[test]
    fn dto_never_leaks_oidc_sid() {
        // The IdP-emitted `sid` uniquely correlates the row to a real
        // user's live IdP session and MUST stay server-side (used by
        // Back-Channel Logout matching, never useful to an admin
        // viewing sessions). Not even a prefix — see the DTO comment.
        let full_sid = "8aa711b3-7438-cb35-4089-71a202e12285";
        let dto = SessionSummaryDto::from(oidc_session(full_sid));
        let json = serde_json::to_string(&dto).unwrap();
        assert!(
            !json.contains(full_sid),
            "oidc_sid must never appear in the wire shape (not even a prefix): {json}"
        );
        assert!(
            !json.contains("oidc_sid"),
            "the `oidc_sid` key MUST NOT appear in the DTO shape: {json}"
        );
    }

    #[test]
    fn dto_never_leaks_oidc_id_token() {
        // The id_token itself must NEVER surface — it's a JWT carrying
        // user claims + a valid `id_token_hint` for RP-initiated logout.
        let dto = SessionSummaryDto::from(oidc_session("sid-1"));
        let json = serde_json::to_string(&dto).unwrap();
        assert!(
            !json.contains("dummy.id.token"),
            "oidc_id_token must never appear in the wire shape: {json}"
        );
    }

    #[test]
    fn dto_never_leaks_refresh_token() {
        let s = base(false, None);
        let dto = SessionSummaryDto::from(s);
        let json = serde_json::to_string(&dto).unwrap();
        assert!(
            !json.contains("refresh-token"),
            "refresh_token must never appear in the wire shape"
        );
    }

    #[test]
    fn dto_truncates_dpop_jkt_to_8_chars() {
        // 44-char base64url thumbprint (SHA-256 → 32 bytes → ceil(32/3)*4 = 44)
        let full = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGH";
        let dto = SessionSummaryDto::from(base(false, Some(full)));
        assert_eq!(dto.dpop_jkt_prefix.as_deref(), Some("abcdefgh"));
        assert!(dto.is_bound);
    }

    #[test]
    fn dto_unbound_session_has_no_prefix() {
        let dto = SessionSummaryDto::from(base(false, None));
        assert_eq!(dto.dpop_jkt_prefix, None);
        assert!(!dto.is_bound);
    }

    #[test]
    fn is_active_false_when_revoked() {
        let dto = SessionSummaryDto::from(base(true, None));
        assert!(dto.is_revoked);
        assert!(!dto.is_active);
    }

    #[test]
    fn is_active_true_for_fresh_unrevoked_session() {
        let dto = SessionSummaryDto::from(base(false, Some("jkt-abc")));
        assert!(!dto.is_revoked);
        assert!(dto.is_active);
    }

    #[test]
    fn from_raw_expired_session_is_not_active() {
        let past = Utc::now() - Duration::days(1);
        let s = Session::from_raw(
            Uuid::new_v4(),
            Uuid::new_v4(),
            "rt".to_string(),
            past,
            None,
            None,
            past,
            false,
            Uuid::new_v4(),
            None,
            None,
            None,
            crate::domain::entities::session::SessionOrigin::Unknown,
        );
        let dto = SessionSummaryDto::from(s);
        assert!(!dto.is_active);
        assert!(!dto.is_revoked); // exp-but-unrevoked distinct from revoked
    }
}
