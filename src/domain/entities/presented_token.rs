//! What the credential on a request says about itself.
//!
//! In the domain rather than beside the middleware that reads it,
//! because three layers need the same pair: the middleware computes it,
//! the realtime ticket store carries it across the gap between a
//! `POST /api/rt/ticket` and the WebSocket upgrade that follows, and the
//! live-role decision consumes it. `infrastructure` may not depend on
//! `interfaces`, so the shared vocabulary belongs here.

use uuid::Uuid;

/// The two claims the revocation checks need beyond a user id.
///
/// One struct rather than two positional `Option`s, because both are
/// optional and `resolve_live_role(a, b, c, None, None)` at a call site
/// says nothing about which `None` is which.
///
/// `PresentedToken::default()` — both absent — is the honest answer for
/// a credential that is not a JWT at all, and it is NOT a clean bill of
/// health. It means the revocation checks cannot be applied, so a caller
/// passing it is declaring a blind spot rather than asserting safety.
/// Today that is an app password over Basic auth, which carries neither
/// claim and is revoked by deleting it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PresentedToken {
    /// The JWT `iat`, for the user-wide revocation cutoff
    /// (`auth.users.sessions_valid_from`).
    pub issued_at: Option<i64>,
    /// The JWT `sid` (`auth.sessions.id`), for per-session revocation.
    pub session_id: Option<Uuid>,
}

impl PresentedToken {
    /// Build from a validated token's claims.
    ///
    /// A named constructor so the field order cannot be transposed at a
    /// call site — both are `Option`s of different types today, but
    /// `issued_at` is only an `i64` by accident of the JWT encoding.
    pub fn from_claims(issued_at: i64, session_id: Option<Uuid>) -> Self {
        Self {
            issued_at: Some(issued_at),
            session_id,
        }
    }
}
