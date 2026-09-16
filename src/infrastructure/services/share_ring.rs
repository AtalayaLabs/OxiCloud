//! The **share ring** — one signed, capped list of the public-share links a
//! browser has unlocked.
//!
//! ## Why a ring rather than one cookie per share
//!
//! A share visitor's credential has to be a cookie: `<img src="/api/files/
//! {id}/thumbnail/preview">` cannot send a header, so the browser must carry
//! it ambiently. The naive shape is one cookie per unlocked share, and it has
//! a nasty property — **the number of credentials becomes client-controlled**.
//! An attacker attaches a hundred forged cookies and the server pays a
//! hundred HMAC verifications to reject them, from a single cheap request.
//!
//! A ring inverts that. The list lives inside one JWT **we** signed, so it can
//! only grow by passing `/s/{token}/verify` against a real share. One
//! signature verification regardless of how many shares it holds, and the
//! multiplier is ours rather than the caller's.
//!
//! ## Why it is separate from the access token
//!
//! Folding the list into the user's access token would entangle it with three
//! things it has nothing to do with: the RFC 9449 `cnf.jkt` DPoP binding
//! (re-minting must not silently unbind the session), refresh rotation (every
//! refresh would have to carry the list forward), and the `&User`-shaped mint
//! path (the share handler has no `User`). The ring is orthogonal to identity
//! — it says what you have unlocked, not who you are — so it gets its own
//! cookie and its own TTL.
//!
//! ```text
//! oxicloud_access  → who you are        (untouched by unlocking a share)
//! oxi_shares       → what you unlocked  (this module)
//! ```
//!
//! A logged-in user carries both, and authorization tries their own grants
//! first and the ring second. An anonymous visitor carries only the ring.
//!
//! ## The cap
//!
//! [`MAX_RING_SIZE`] bounds the list, evicting oldest-first. Not for DoS —
//! the server controls growth now — but because these bytes ride on **every**
//! request to the origin, image loads included. Someone who opens fifty share
//! links should not pay for it on every thumbnail fetch forever.

use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::errors::DomainError;

/// Cookie name. Deliberately NOT `oxicloud_access`: that cookie is `Path=/`,
/// so reusing it would mean a logged-in user who clicks a share link has
/// their real session overwritten and every subsequent request becomes
/// anonymous.
pub const SHARE_RING_COOKIE: &str = "oxi_shares";

/// Maximum shares held at once. Oldest are evicted first.
pub const MAX_RING_SIZE: usize = 10;

/// Ring lifetime, deliberately independent of `access_token_expiry_secs`.
///
/// An access token is short because it is a bearer credential for a real
/// account, refreshable in the background by a running SPA. A ring is neither:
/// it grants only what the share owner already published, and expiring it
/// mid-visit means a gallery that silently stops loading thumbnails with no
/// session for the visitor to refresh. Eight hours covers a working day of
/// browsing; the share's own `expires_at` is still checked per request, so a
/// long ring cannot outlive the share it names.
pub const DEFAULT_TTL_SECS: i64 = 8 * 3600;

/// Claim type discriminator.
///
/// The unlock cookie predating this module has no `typ` and is signed with
/// the SAME secret as access tokens — it is safe only because `JwtClaims`
/// happens to require `username`/`email`/`role`/`jti`, so an unlock JWT fails
/// to deserialise as one. That is an accident of struct shape, not a design.
/// This module states its type explicitly and refuses anything else.
const RING_TYP: &str = "share-ring";

#[derive(Debug, Serialize, Deserialize)]
struct RingClaims {
    typ: String,
    /// Visitor id — stable for the life of this ring.
    ///
    /// The ring IS the anonymous session, so this is the only identifier a
    /// public-share visitor has. It is not a user id and matches no row; it
    /// exists so one visitor's requests can be correlated in logs, and so
    /// `CurrentUser.id` has something honest to hold.
    vid: Uuid,
    /// `storage.shares.id` values, oldest first.
    shares: Vec<Uuid>,
    exp: i64,
    iat: i64,
}

/// What a valid ring carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ring {
    /// Stable per-visitor id — see [`RingClaims::vid`].
    pub visitor_id: Uuid,
    /// Unlocked shares, oldest first.
    pub shares: Vec<Uuid>,
}

/// Mint a ring holding `shares` (already capped by the caller via [`append`]).
pub fn issue(
    secret: &str,
    visitor_id: Uuid,
    shares: &[Uuid],
    ttl_secs: i64,
) -> Result<String, DomainError> {
    if secret.is_empty() {
        return Err(DomainError::internal_error(
            "ShareRing",
            "JWT secret is not configured",
        ));
    }
    let now = chrono::Utc::now().timestamp();
    let claims = RingClaims {
        typ: RING_TYP.to_string(),
        vid: visitor_id,
        shares: shares.to_vec(),
        exp: now + ttl_secs,
        iat: now,
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| DomainError::internal_error("ShareRing", format!("failed to sign ring: {e}")))
}

/// Read the shares out of a ring, or `None` if it is absent, expired,
/// wrongly typed or not ours.
///
/// Every failure collapses to `None` on purpose: a caller cannot act on the
/// difference between "tampered", "expired" and "absent" — in all three the
/// bearer has unlocked nothing — and distinguishing them in a response would
/// tell an attacker which of their guesses was closer.
pub fn verify(secret: &str, jwt: &str) -> Option<Ring> {
    if secret.is_empty() {
        return None;
    }
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = true;
    validation.leeway = 0;
    validation.required_spec_claims.insert("exp".to_string());

    let data = decode::<RingClaims>(
        jwt,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .ok()?;

    if data.claims.typ != RING_TYP {
        return None;
    }
    Some(Ring {
        visitor_id: data.claims.vid,
        shares: data.claims.shares,
    })
}

/// Add `share_id` to the ring carried in `existing`, returning a fresh token.
///
/// An unreadable `existing` is treated as an empty ring rather than an error:
/// an expired or tampered cookie should not block a visitor from unlocking a
/// share they legitimately hold the link for.
///
/// Already-present ids move to the newest position instead of duplicating, so
/// re-opening a link refreshes its place in the eviction order rather than
/// consuming another slot.
pub fn append(
    secret: &str,
    existing: Option<&str>,
    share_id: Uuid,
    ttl_secs: i64,
) -> Result<String, DomainError> {
    let held = existing.and_then(|jwt| verify(secret, jwt));

    // Keep the visitor id across appends so one person's requests stay
    // correlatable in logs as they unlock more links. A ring that could not
    // be read starts a new visitor rather than failing — see the doc above.
    let visitor_id = held.as_ref().map_or_else(Uuid::new_v4, |r| r.visitor_id);
    let mut shares = held.map(|r| r.shares).unwrap_or_default();

    shares.retain(|&s| s != share_id);
    shares.push(share_id);

    // Oldest-first eviction. `saturating_sub` keeps this correct if
    // MAX_RING_SIZE is ever lowered below the length of a ring already in the
    // wild — the excess is dropped rather than panicking on a bad range.
    let overflow = shares.len().saturating_sub(MAX_RING_SIZE);
    shares.drain(..overflow);

    issue(secret, visitor_id, &shares, ttl_secs)
}

/// Find the ring token in a `Cookie:` header value.
pub fn extract_from_cookie_header(cookie_header: &str) -> Option<&str> {
    cookie_header.split(';').find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        (name == SHARE_RING_COOKIE).then_some(value)
    })
}

/// `Set-Cookie` value for the ring.
///
/// `Path=/` because the ring must accompany `/api/files/...` requests issued
/// by `<img>` and `<video>` tags, which cannot set headers.
///
/// `Secure` is conditional on the same helper the session cookies use, so a
/// plain-HTTP dev instance still works while production never sends the ring
/// in the clear. The older `oxi_share_unlock_*` cookie omits `Secure`
/// entirely — that is a pre-existing gap, not a precedent to copy.
pub fn build_set_cookie(jwt: &str, ttl_secs: i64) -> String {
    let secure = if crate::interfaces::api::cookie_auth::is_cookie_secure() {
        "; Secure"
    } else {
        ""
    };
    format!("{SHARE_RING_COOKIE}={jwt}; HttpOnly; SameSite=Lax; Path=/; Max-Age={ttl_secs}{secure}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "test-secret-do-not-use-in-prod-minimum-32-chars";
    const TTL: i64 = 3600;

    /// Most assertions care only about the share list.
    fn shares_of(jwt: &str) -> Option<Vec<Uuid>> {
        verify(SECRET, jwt).map(|r| r.shares)
    }

    #[test]
    fn a_ring_round_trips() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let jwt = issue(SECRET, Uuid::new_v4(), &[a, b], TTL).unwrap();
        assert_eq!(shares_of(&jwt), Some(vec![a, b]));
    }

    #[test]
    fn append_accumulates_and_preserves_order() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();

        let first = append(SECRET, None, a, TTL).unwrap();
        assert_eq!(shares_of(&first), Some(vec![a]));

        let second = append(SECRET, Some(&first), b, TTL).unwrap();
        assert_eq!(shares_of(&second), Some(vec![a, b]));
    }

    /// One visitor stays one visitor as they unlock more links — otherwise
    /// their requests could not be correlated in logs across a visit.
    #[test]
    fn the_visitor_id_survives_appends() {
        let first = append(SECRET, None, Uuid::new_v4(), TTL).unwrap();
        let vid = verify(SECRET, &first).unwrap().visitor_id;

        let second = append(SECRET, Some(&first), Uuid::new_v4(), TTL).unwrap();
        assert_eq!(verify(SECRET, &second).unwrap().visitor_id, vid);
    }

    /// Re-opening a link must not consume a second slot — it refreshes the
    /// existing entry's position in the eviction order.
    #[test]
    fn re_unlocking_moves_to_newest_rather_than_duplicating() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();

        let r = append(SECRET, None, a, TTL).unwrap();
        let r = append(SECRET, Some(&r), b, TTL).unwrap();
        let r = append(SECRET, Some(&r), a, TTL).unwrap();

        assert_eq!(shares_of(&r), Some(vec![b, a]));
    }

    /// The cap is what stops the ring growing without bound on every request
    /// to the origin, image loads included.
    #[test]
    fn the_ring_is_capped_evicting_oldest() {
        let ids: Vec<Uuid> = (0..MAX_RING_SIZE + 3).map(|_| Uuid::new_v4()).collect();

        let mut ring = None;
        for id in &ids {
            ring = Some(append(SECRET, ring.as_deref(), *id, TTL).unwrap());
        }

        let held = shares_of(ring.as_deref().unwrap()).unwrap();
        assert_eq!(held.len(), MAX_RING_SIZE);
        // The three oldest are gone, the newest survive in order.
        assert_eq!(held, ids[3..], "eviction must be oldest-first");
    }

    /// A ring signed with another secret is not ours. This is the property
    /// that makes the list server-controlled — the whole reason a ring beats
    /// one cookie per share.
    #[test]
    fn a_foreign_signature_is_rejected() {
        let jwt = issue(
            "some-other-secret-at-least-32-bytes-long!!",
            Uuid::new_v4(),
            &[Uuid::new_v4()],
            TTL,
        )
        .unwrap();
        assert_eq!(verify(SECRET, &jwt), None);
    }

    /// An access token must never be readable as a ring, and vice versa. The
    /// pre-existing unlock cookie is safe from this only by accident of
    /// struct shape; here it is explicit.
    #[test]
    fn a_wrongly_typed_token_is_rejected() {
        #[derive(Serialize)]
        struct NotARing {
            typ: String,
            shares: Vec<Uuid>,
            exp: i64,
            iat: i64,
        }
        let now = chrono::Utc::now().timestamp();
        let forged = encode(
            &Header::default(),
            &NotARing {
                typ: "access".to_string(),
                shares: vec![Uuid::new_v4()],
                exp: now + TTL,
                iat: now,
            },
            &EncodingKey::from_secret(SECRET.as_bytes()),
        )
        .unwrap();

        assert_eq!(verify(SECRET, &forged), None);
    }

    #[test]
    fn an_expired_ring_is_rejected() {
        let jwt = issue(SECRET, Uuid::new_v4(), &[Uuid::new_v4()], -10).unwrap();
        assert_eq!(verify(SECRET, &jwt), None);
    }

    /// An unreadable existing cookie must not block a legitimate unlock —
    /// the visitor holds the link, which is the thing that matters.
    #[test]
    fn append_treats_an_unreadable_ring_as_empty() {
        let a = Uuid::new_v4();
        let fresh = append(SECRET, Some("not.a.jwt"), a, TTL).unwrap();
        assert_eq!(shares_of(&fresh), Some(vec![a]));
    }

    #[test]
    fn extract_finds_the_ring_among_other_cookies() {
        let header = format!("oxicloud_access=abc; {SHARE_RING_COOKIE}=ring.value; other=x");
        assert_eq!(extract_from_cookie_header(&header), Some("ring.value"));
        assert_eq!(extract_from_cookie_header("oxicloud_access=abc"), None);
    }

    #[test]
    fn cookie_carries_the_required_attributes() {
        let s = build_set_cookie("ring.value", 3600);
        assert!(s.contains(&format!("{SHARE_RING_COOKIE}=ring.value")));
        assert!(s.contains("HttpOnly"));
        assert!(s.contains("SameSite=Lax"));
        // Must reach `/api/files/...` for `<img>`-driven thumbnail loads.
        assert!(s.contains("Path=/"));
        assert!(s.contains("Max-Age=3600"));
    }
}
