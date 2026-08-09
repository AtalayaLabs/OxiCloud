//! DPoP-Nonce service (RFC 9449 §8) — issues and validates the
//! server-generated nonces that eliminate reliance on the client
//! clock for freshness.
//!
//! Model — a **pool of currently-valid nonces**, not a single "last"
//! value. Every nonce we hand out sits in the pool for its full
//! lifetime; multiple can be simultaneously valid (rotation overlap,
//! multi-tab). A proof's `nonce` claim is valid iff the pool still
//! remembers it.
//!
//! Rotation — every request response can carry a `DPoP-Nonce`
//! header pointing at the "current" nonce. When the current nonce
//! is older than [`ROTATION_INTERVAL`], `current_or_rotate` mints
//! a fresh one and returns it (the outgoing one keeps living in
//! the pool until its TTL expires — the overlap window). Clients
//! opportunistically pick up the fresh header and start using it;
//! in-flight requests carrying the previous nonce remain valid
//! throughout the overlap.
//!
//! Storage — in-memory `moka` LRU, no PG persistence. On server
//! restart the pool is empty → every next client request gets a
//! `use_dpop_nonce` challenge (middleware handles this) which
//! transparently rotates the client onto a fresh nonce. That's
//! why the SPA fetch interceptor (Gate 4) has a mandatory
//! challenge-retry loop.
//!
//! Scale — a hard cap on cache size bounds memory under attack.
//! At ~64 bytes/entry and a 100k cap, worst-case ~6 MB. Under
//! normal traffic the pool is far below the cap.
//!
//! **Multi-instance caveat**: each OxiCloud replica has its own
//! pool. A nonce issued by node A + validated by node B will 401 →
//! challenge → retry → one extra round trip, no security impact.
//! Elevate to a shared Redis if the operational impact ever
//! matters; for the common single-instance self-hosted deployment
//! in-memory is correct.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64_URL_NO_PAD;
use moka::sync::Cache;
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// How long a nonce is valid after issuance. Rejected outright once
/// past this window (moka TTL enforces it — no manual sweep needed).
pub const NONCE_LIFETIME: Duration = Duration::from_secs(300); // 5 min

/// Once the "current" nonce is older than this, `current_or_rotate`
/// mints a fresh one on next call. The outgoing nonce stays valid
/// in the pool until its own TTL expires, giving a 3-minute overlap
/// window during which both work. Clients pick up the fresh header
/// in the next response and switch over lazily.
pub const ROTATION_INTERVAL: Duration = Duration::from_secs(120); // 2 min

/// Max nonce entries — bounds memory under attack. LRU eviction
/// past this cap.
const MAX_POOL_SIZE: u64 = 100_000;

/// Byte length of a nonce before base64url encoding. 32 bytes →
/// 43-char b64url string, matching JWK-thumbprint dimensions so
/// operator eyes calibrate the same way for both fields.
const NONCE_BYTES: usize = 32;

/// Currently-active pool of nonces + the freshest one served in
/// `DPoP-Nonce` response headers.
pub struct DpopNonceService {
    /// Pool of live nonces. Value is `()` — presence == validity;
    /// TTL enforced by moka's `time_to_live`.
    pool: Cache<String, ()>,
    /// Freshest nonce we've issued + when — used to decide when to
    /// rotate. `None` at boot, populated on first `current_or_rotate`.
    current: RwLock<Option<CurrentNonce>>,
}

struct CurrentNonce {
    value: String,
    issued_at: Instant,
}

impl Default for DpopNonceService {
    fn default() -> Self {
        Self::new()
    }
}

impl DpopNonceService {
    pub fn new() -> Self {
        Self {
            pool: Cache::builder()
                .max_capacity(MAX_POOL_SIZE)
                .time_to_live(NONCE_LIFETIME)
                .build(),
            current: RwLock::new(None),
        }
    }

    /// Return the current nonce, minting a fresh one when the last
    /// mint is older than [`ROTATION_INTERVAL`] (or on cold start).
    /// The returned value is what the middleware stamps into
    /// outgoing `DPoP-Nonce` response headers.
    pub fn current_or_rotate(&self) -> String {
        // Fast path: read lock, current is still fresh → clone the string.
        if let Some(cur) = self.current.read().unwrap().as_ref()
            && cur.issued_at.elapsed() < ROTATION_INTERVAL
        {
            return cur.value.clone();
        }
        // Slow path: write lock, re-check (someone else may have
        // rotated between drop-read and acquire-write), otherwise
        // mint fresh.
        let mut guard = self.current.write().unwrap();
        if let Some(cur) = guard.as_ref()
            && cur.issued_at.elapsed() < ROTATION_INTERVAL
        {
            return cur.value.clone();
        }
        let fresh = mint_nonce();
        self.pool.insert(fresh.clone(), ());
        *guard = Some(CurrentNonce {
            value: fresh.clone(),
            issued_at: Instant::now(),
        });
        fresh
    }

    /// Check whether a nonce presented by a client is still valid.
    /// Returns `false` for absent-from-pool AND for
    /// past-TTL-eviction; both are indistinguishable from the
    /// caller's perspective.
    pub fn is_valid(&self, nonce: &str) -> bool {
        self.pool.contains_key(nonce)
    }
}

fn mint_nonce() -> String {
    // Re-use `p256`'s already-transitive `rand_core::OsRng` — no new
    // dep, no version alignment risk. `OsRng` reads from the OS
    // entropy source; `fill_bytes` panics on RNG failure (unreachable
    // outside catastrophic OS state, and safer to crash than mint a
    // guessable nonce).
    use p256::elliptic_curve::rand_core::{OsRng, RngCore};
    let mut buf = [0u8; NONCE_BYTES];
    OsRng.fill_bytes(&mut buf);
    B64_URL_NO_PAD.encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issues_a_nonce_of_expected_shape() {
        let svc = DpopNonceService::new();
        let n = svc.current_or_rotate();
        // 43 chars = base64url(SHA-256-equivalent length)
        assert_eq!(n.len(), 43);
        assert!(
            n.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "nonce contains non-base64url chars: {n}"
        );
    }

    #[test]
    fn issued_nonce_validates_immediately() {
        let svc = DpopNonceService::new();
        let n = svc.current_or_rotate();
        assert!(svc.is_valid(&n));
    }

    #[test]
    fn returns_same_nonce_within_rotation_window() {
        let svc = DpopNonceService::new();
        let a = svc.current_or_rotate();
        let b = svc.current_or_rotate();
        assert_eq!(a, b);
    }

    #[test]
    fn unknown_nonce_is_rejected() {
        let svc = DpopNonceService::new();
        assert!(!svc.is_valid("not-a-real-nonce-value-1234567890abcde"));
    }
}
