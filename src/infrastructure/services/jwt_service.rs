//! JWT-based token service implementation.
//!
//! This module provides JWT token generation and validation functionality,
//! implementing the TokenServicePort trait defined in the application layer.
//!
//! **Performance optimisation**: a per-token validation cache (moka, lock-free)
//! avoids repeating the HMAC-SHA256 verification on every request for the same
//! token.  Entries are keyed by a fast BLAKE3 hash of the raw token string and
//! auto-expire after a short TTL (30 s by default) so revoked tokens don't stay
//! valid for long.

use chrono::Utc;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use moka::sync::Cache;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use uuid::Uuid;

use crate::application::ports::auth_ports::{TokenClaims, TokenServicePort};
use crate::common::errors::{DomainError, ErrorKind};
use crate::domain::entities::user::User;

/// Internal JWT claims structure for serialization.
/// This is the actual JWT payload structure used by jsonwebtoken crate.
///
/// `username` / `email` deserialize straight into `Arc<str>` (serde `rc`,
/// one allocation — same count as `String`) so the `TokenClaims` conversion
/// below is a plain move and the port-level claims can hand refcount bumps
/// to every consumer.
#[derive(Debug, Serialize, Deserialize)]
struct JwtClaims {
    /// Subject identifier - contains the user ID
    pub sub: String,
    /// Expiration timestamp (seconds since Unix epoch)
    pub exp: i64,
    /// Issued at timestamp (seconds since Unix epoch)
    pub iat: i64,
    /// JWT unique ID for token tracking and revocation
    pub jti: String,
    /// Username for display and identification purposes
    pub username: Arc<str>,
    /// User email for communication and identification
    pub email: Arc<str>,
    /// User role for authorization checks
    pub role: String,
    /// RFC 9449 §5 confirmation-key claim: JWK thumbprint of the
    /// browser-held DPoP keypair the session was bound to at login.
    /// `None` for unbound sessions (app passwords, Nextcloud clients,
    /// pre-DPoP sessions). Populated at token mint time from
    /// `session.dpop_jkt`; the DPoP middleware reads it to enforce
    /// "bound session → proof required" without a DB round trip.
    ///
    /// Serialised as `{"cnf": {"jkt": "..."}}` to match RFC 9449.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cnf: Option<CnfClaim>,
}

/// RFC 9449 §5 confirmation-key wrapper. Only the `jkt` member is
/// used today; future extensions (`x5t#S256`, etc.) would live here.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CnfClaim {
    pub jkt: String,
}

impl From<JwtClaims> for TokenClaims {
    fn from(claims: JwtClaims) -> Self {
        // Pre-parse the subject once at decode time (amortized over the
        // validation-cache TTL) so the auth middleware reads a `Copy` instead
        // of re-parsing the 36-char string per request. A verified token we
        // signed always carries a UUID `sub`; nil is a safe sentinel the
        // middleware rejects. See benches/ROUND14.md §A3.
        let sub_id = uuid::Uuid::parse_str(&claims.sub).unwrap_or_else(|_| uuid::Uuid::nil());
        TokenClaims {
            sub_id,
            sub: claims.sub,
            exp: claims.exp,
            iat: claims.iat,
            jti: claims.jti,
            username: claims.username,
            email: claims.email,
            role: claims.role,
            dpop_jkt: claims.cnf.map(|c| c.jkt),
        }
    }
}

/// JWT-based implementation of the TokenServicePort.
///
/// This service handles JWT token generation and validation for user authentication.
/// It uses HS256 algorithm for signing tokens.
///
/// ## Validation cache
///
/// `jsonwebtoken::decode()` performs HMAC-SHA256 verification on every call.
/// While fast in absolute terms (~2-4 µs on modern hardware), at 10 k req/s
/// that is 20-40 ms of pure CPU per second — and it is synchronous, blocking
/// the Tokio worker thread.
///
/// The cache uses the **BLAKE3** hash of the raw token string as key (32-byte,
/// ~0.1 µs to compute — 20× cheaper than HMAC verification) and stores the
/// validated claims behind an `Arc`.  On a cache hit the HMAC step is
/// completely skipped and the lookup returns a refcount bump rather than a
/// deep clone of the (multi-`String`) `TokenClaims`.
///
/// **Security properties**:
/// - TTL of 30 s bounds the window in which a revoked token remains valid.
/// - Max 50 000 entries (≈ 4 MB RSS) with LRU eviction prevents DoS via
///   unique-token flooding.
/// - Expired tokens are never cached (decode itself rejects them first).
pub struct JwtTokenService {
    /// Pre-built signing key — `EncodingKey::from_secret` copies the secret
    /// into a fresh buffer, so building it per `generate_access_token` call
    /// paid an allocation per login/refresh for a process-invariant value.
    encoding_key: EncodingKey,
    /// Pre-built verification key (same rationale, on the validation-cache
    /// miss path — every new token and every token once per TTL window).
    decoding_key: DecodingKey,
    /// Pre-built HS256 validation config — `Validation::new` allocates a
    /// `HashSet{"exp"}` + algorithm `Vec` on every call otherwise.
    validation: Validation,
    /// Expiration time for access tokens in seconds
    access_token_expiry: i64,
    /// Expiration time for refresh tokens in seconds
    refresh_token_expiry: i64,
    /// Validation result cache: blake3(token) → Arc<TokenClaims>.
    /// `Arc` so a cache hit is a refcount bump, not a multi-`String` clone.
    validation_cache: Cache<[u8; 32], Arc<TokenClaims>>,
    /// Cache hit counter (for observability / metrics)
    cache_hits: AtomicU64,
    /// Cache miss counter
    cache_misses: AtomicU64,
}

/// Default TTL for cached validation results (seconds).
const VALIDATION_CACHE_TTL_SECS: u64 = 30;

/// Maximum number of cached token validations.
const VALIDATION_CACHE_MAX_ENTRIES: u64 = 50_000;

impl JwtTokenService {
    /// Create a new JwtTokenService with the specified configuration.
    ///
    /// # Arguments
    /// * `jwt_secret` - Secret key for signing tokens (should be at least 32 bytes)
    /// * `access_token_expiry_secs` - Lifetime of access tokens in seconds
    /// * `refresh_token_expiry_secs` - Lifetime of refresh tokens in seconds
    pub fn new(
        jwt_secret: String,
        access_token_expiry_secs: i64,
        refresh_token_expiry_secs: i64,
    ) -> Self {
        let validation_cache = Cache::builder()
            .max_capacity(VALIDATION_CACHE_MAX_ENTRIES)
            .time_to_live(Duration::from_secs(VALIDATION_CACHE_TTL_SECS))
            .build();

        tracing::info!(
            "JWT validation cache initialised: TTL={}s, max_entries={}",
            VALIDATION_CACHE_TTL_SECS,
            VALIDATION_CACHE_MAX_ENTRIES,
        );

        Self {
            encoding_key: EncodingKey::from_secret(jwt_secret.as_bytes()),
            decoding_key: DecodingKey::from_secret(jwt_secret.as_bytes()),
            validation: Validation::new(Algorithm::HS256),
            access_token_expiry: access_token_expiry_secs,
            refresh_token_expiry: refresh_token_expiry_secs,
            validation_cache,
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
        }
    }

    /// Compute a fast BLAKE3 hash of a token string, used as cache key.
    ///
    /// BLAKE3 is ~20× faster than SHA-256 and ~40× faster than HMAC-SHA256
    /// verification through `jsonwebtoken`, making it an ideal pre-filter.
    #[inline]
    fn token_hash(token: &str) -> [u8; 32] {
        blake3::hash(token.as_bytes()).into()
    }

    /// Return cache hit/miss statistics for monitoring.
    pub fn cache_stats(&self) -> (u64, u64) {
        (
            self.cache_hits.load(Ordering::Relaxed),
            self.cache_misses.load(Ordering::Relaxed),
        )
    }
}

impl TokenServicePort for JwtTokenService {
    fn generate_access_token(
        &self,
        user: &User,
        dpop_jkt: Option<&str>,
    ) -> Result<String, DomainError> {
        let now = Utc::now().timestamp();

        // Log information for debugging
        tracing::debug!(
            "Generating token for user: {}, id: {}, role: {}",
            user.display_for_audit(),
            user.id(),
            user.role()
        );

        let claims = JwtClaims {
            sub: user.id().to_string(),
            exp: now + self.access_token_expiry,
            iat: now,
            jti: Uuid::new_v4().to_string(),
            username: Arc::from(user.username().unwrap_or("")),
            email: Arc::from(user.email()),
            role: user.role().as_str().to_string(),
            cnf: dpop_jkt.map(|jkt| CnfClaim {
                jkt: jkt.to_string(),
            }),
        };

        // Log JWT claims for debugging
        tracing::debug!(
            "JWT claims: sub={}, exp={}, iat={}",
            claims.sub,
            claims.exp,
            claims.iat
        );

        encode(&Header::default(), &claims, &self.encoding_key).map_err(|e| {
            tracing::error!("Error generating token: {}", e);
            DomainError::new(
                ErrorKind::InternalError,
                "TokenService",
                format!("Error generating token: {}", e),
            )
        })
    }

    fn validate_token(&self, token: &str) -> Result<Arc<TokenClaims>, DomainError> {
        // ── 1. Fast-path: check the validation cache ─────────────
        let key = Self::token_hash(token);

        if let Some(cached_claims) = self.validation_cache.get(&key) {
            // Even on a cache hit we must verify the token hasn't expired
            // since it was cached (the cached exp is an absolute timestamp).
            let now = Utc::now().timestamp();
            if cached_claims.exp > now {
                self.cache_hits.fetch_add(1, Ordering::Relaxed);
                return Ok(cached_claims);
            }
            // Token expired while cached — evict and fall through to full
            // verification which will return the proper "Token expired" error.
            self.validation_cache.invalidate(&key);
        }

        // ── 2. Slow-path: full HMAC-SHA256 verification ─────────
        self.cache_misses.fetch_add(1, Ordering::Relaxed);

        let token_data = decode::<JwtClaims>(token, &self.decoding_key, &self.validation).map_err(
            |e| match e.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => {
                    DomainError::new(ErrorKind::AccessDenied, "TokenService", "Token expired")
                }
                _ => DomainError::new(
                    ErrorKind::AccessDenied,
                    "TokenService",
                    format!("Invalid token: {}", e),
                ),
            },
        )?;

        let claims = Arc::new(TokenClaims::from(token_data.claims));

        // ── 3. Store in cache for subsequent requests ────────────
        // Only cache tokens that won't expire within the cache TTL window,
        // avoiding stale positives right at the boundary.
        let remaining_secs = claims.exp - Utc::now().timestamp();
        if remaining_secs > VALIDATION_CACHE_TTL_SECS as i64 {
            // Refcount bump — the claims live once behind the `Arc`.
            self.validation_cache.insert(key, Arc::clone(&claims));
        }

        Ok(claims)
    }

    fn generate_refresh_token(&self) -> String {
        Uuid::new_v4().to_string()
    }

    fn refresh_token_expiry_secs(&self) -> i64 {
        self.refresh_token_expiry
    }

    fn refresh_token_expiry_days(&self) -> i64 {
        self.refresh_token_expiry / (24 * 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::entities::user::{User, UserRole};
    use uuid::Uuid;

    fn create_test_user() -> User {
        User::from_data(
            Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
            Some("testuser".to_string()),
            "test@example.com".to_string(),
            Some("hashed_password".to_string()),
            UserRole::User,
            1024 * 1024 * 1024, // 1GB
            0,
            chrono::Utc::now(),
            chrono::Utc::now(),
            None,
            true,
        )
    }

    #[test]
    fn test_generate_and_validate_token() {
        let service = JwtTokenService::new(
            "test_secret_key_at_least_32_bytes_long".to_string(),
            3600,  // 1 hour
            86400, // 1 day
        );

        let user = create_test_user();
        let token = service
            .generate_access_token(&user, None)
            .expect("Should generate token");

        let claims = service
            .validate_token(&token)
            .expect("Should validate token");
        assert_eq!(claims.sub, user.id().to_string());
        assert_eq!(Some(&*claims.username), user.username());
        assert_eq!(&*claims.email, user.email());
    }

    #[test]
    fn test_refresh_token_is_unique() {
        let service = JwtTokenService::new("secret".to_string(), 3600, 86400);

        let token1 = service.generate_refresh_token();
        let token2 = service.generate_refresh_token();

        assert_ne!(token1, token2);
    }

    #[test]
    fn test_invalid_token() {
        let service = JwtTokenService::new("secret".to_string(), 3600, 86400);

        let result = service.validate_token("invalid_token");
        assert!(result.is_err());
    }

    #[test]
    fn test_validation_cache_hit() {
        let service = JwtTokenService::new(
            "test_secret_key_at_least_32_bytes_long".to_string(),
            3600,
            86400,
        );

        let user = create_test_user();
        let token = service
            .generate_access_token(&user, None)
            .expect("Should generate token");

        // First call: cache miss — performs full HMAC verification
        let claims1 = service.validate_token(&token).expect("Should validate");

        // Second call: cache hit — skips HMAC, returns cloned claims
        let claims2 = service
            .validate_token(&token)
            .expect("Should validate from cache");

        assert_eq!(claims1.sub, claims2.sub);
        assert_eq!(claims1.username, claims2.username);

        let (hits, misses) = service.cache_stats();
        assert_eq!(hits, 1, "Expected 1 cache hit");
        assert_eq!(misses, 1, "Expected 1 cache miss");
    }

    #[test]
    fn test_cache_hit_returns_same_arc_not_a_clone() {
        let service = JwtTokenService::new(
            "test_secret_key_at_least_32_bytes_long".to_string(),
            3600,
            86400,
        );
        let token = service
            .generate_access_token(&create_test_user(), None)
            .expect("Should generate token");

        // Miss populates the cache; hit must hand back the very same
        // allocation (pointer-equal Arc), proving the hot path is a refcount
        // bump rather than a deep clone of the claims' Strings.
        let first = service.validate_token(&token).expect("miss");
        let second = service.validate_token(&token).expect("hit");

        assert!(
            Arc::ptr_eq(&first, &second),
            "cache hit must return the same Arc, not a fresh allocation"
        );
        let (hits, misses) = service.cache_stats();
        assert_eq!((hits, misses), (1, 1));
    }

    #[test]
    fn test_invalid_token_not_cached() {
        let service = JwtTokenService::new("secret".to_string(), 3600, 86400);

        // Invalid tokens should never be cached
        let _ = service.validate_token("bad_token");
        let _ = service.validate_token("bad_token");

        let (hits, _misses) = service.cache_stats();
        assert_eq!(hits, 0, "Invalid tokens should never produce cache hits");
    }
}
