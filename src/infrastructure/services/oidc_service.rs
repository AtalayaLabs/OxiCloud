//! OpenID Connect (OIDC) service implementation.
//!
//! Handles OIDC discovery, authorization URL generation, code exchange,
//! ID token validation (RS256/ES256 via JWKS), and UserInfo fetching.
//! Supports both RSA (RS256, RS384, RS512) and EC (ES256, ES384) algorithms.
//! Compatible with Authentik, Keycloak, and any standard OIDC provider.

use serde::Deserialize;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use crate::application::ports::auth_ports::{
    OidcIdClaims, OidcLogoutClaims, OidcServicePort, OidcTokenSet,
};
use crate::common::config::OidcConfig;
use crate::common::errors::{DomainError, ErrorKind};

/// How long discovery/JWKS documents stay cached before re-fetching.
/// 1 hour balances freshness against unnecessary network requests.
const OIDC_CACHE_TTL: Duration = Duration::from_secs(3600);

// ============================================================================
// OIDC Discovery Document
// ============================================================================

#[derive(Debug, Clone, Deserialize)]
struct OidcDiscovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    userinfo_endpoint: Option<String>,
    jwks_uri: String,
    /// RP-initiated logout endpoint (OIDC Session Management 1.0).
    /// Optional — not every IdP advertises it. When missing, callers
    /// must fall back to local-only logout.
    end_session_endpoint: Option<String>,
}

// ============================================================================
// JWKS parsing using jsonwebtoken (enabled via rust_crypto feature)
// ============================================================================

#[derive(Debug, Clone, Deserialize)]
struct JwksDocument {
    keys: Vec<jsonwebtoken::jwk::Jwk>,
}

// ============================================================================
// Token exchange response
// ============================================================================

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    id_token: Option<String>,
    refresh_token: Option<String>,
    #[allow(dead_code)]
    token_type: Option<String>,
    #[allow(dead_code)]
    expires_in: Option<i64>,
}

// ============================================================================
// ID token claims (standard OIDC)
// ============================================================================

#[derive(Debug, Deserialize)]
struct IdTokenClaims {
    sub: String,
    email: Option<String>,
    email_verified: Option<bool>,
    preferred_username: Option<String>,
    name: Option<String>,
    given_name: Option<String>,
    family_name: Option<String>,
    groups: Option<Vec<String>>,
    nonce: Option<String>,
    picture: Option<String>,
    locale: Option<String>,
    /// OIDC session identifier — only set by IdPs configured to emit it
    /// (Keycloak: "Backchannel Logout Session Required"). When present,
    /// bind it to the OxiCloud session so BCL can revoke just that device.
    sid: Option<String>,
    // Standard JWT fields
    #[allow(dead_code)]
    iss: Option<String>,
    #[allow(dead_code)]
    aud: Option<serde_json::Value>,
    #[allow(dead_code)]
    exp: Option<i64>,
    #[allow(dead_code)]
    iat: Option<i64>,
}

/// OIDC Back-Channel Logout 1.0, §2.4 — the logout_token JWT.
///
/// Structural differences from an id_token:
/// - MUST have `sub` OR `sid` (or both).
/// - MUST have `events` claim containing the backchannel-logout URI.
/// - MUST NOT have `nonce`.
/// - `exp` is optional (unlike id_token where it's required); a missing
///   exp is fine, we clamp with our own iat-based freshness check.
#[derive(Debug, Deserialize)]
struct LogoutTokenClaims {
    iss: String,
    aud: serde_json::Value,
    iat: i64,
    jti: Option<String>,
    sub: Option<String>,
    sid: Option<String>,
    events: serde_json::Value,
    nonce: Option<String>,
}

const BACKCHANNEL_LOGOUT_EVENT: &str = "http://schemas.openid.net/event/backchannel-logout";

// ============================================================================
// UserInfo response
// ============================================================================

#[derive(Debug, Deserialize)]
struct UserInfoResponse {
    sub: String,
    email: Option<String>,
    email_verified: Option<bool>,
    preferred_username: Option<String>,
    name: Option<String>,
    given_name: Option<String>,
    family_name: Option<String>,
    groups: Option<Vec<String>>,
    picture: Option<String>,
    locale: Option<String>,
}

// ============================================================================
// OIDC Service
// ============================================================================

/// A cached value with a fetch timestamp for TTL-based expiry.
#[derive(Clone)]
struct Cached<T: Clone> {
    value: T,
    fetched_at: Instant,
}

impl<T: Clone> Cached<T> {
    fn new(value: T) -> Self {
        Self {
            value,
            fetched_at: Instant::now(),
        }
    }

    fn is_expired(&self) -> bool {
        self.fetched_at.elapsed() > OIDC_CACHE_TTL
    }
}

pub struct OidcService {
    config: OidcConfig,
    http_client: reqwest::Client,
    /// Cached discovery document (expires after OIDC_CACHE_TTL)
    discovery: RwLock<Option<Cached<OidcDiscovery>>>,
    /// Cached JWKS (expires after OIDC_CACHE_TTL)
    jwks: RwLock<Option<Cached<JwksDocument>>>,
}

impl OidcService {
    pub fn new(config: OidcConfig) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("Failed to build HTTP client for OIDC");

        Self {
            config,
            http_client,
            discovery: RwLock::new(None),
            jwks: RwLock::new(None),
        }
    }

    /// Authoritative issuer URL from the IdP's discovery document —
    /// **cache-only, non-async, non-blocking**. Returns `Some(issuer)`
    /// when discovery has been fetched successfully before AND is not
    /// expired; returns `None` otherwise (cold cache OR expired without
    /// re-fetch).
    ///
    /// Deliberately does NOT trigger a network fetch — this is the
    /// accessor that public endpoints (`/api/auth/oidc/providers`)
    /// use, and driving IdP HTTP off every unauthenticated request is
    /// a DoS amplifier. The cache gets warmed as a side-effect of
    /// every real OIDC flow (authorize / callback / login /
    /// validate_id_token all call `get_discovery`), so within seconds
    /// of the first legit login this returns `Some`.
    ///
    /// Callers that need the definitive answer (validate_id_token, JIT
    /// provisioning) should keep going through the async
    /// discovery-fetching path. Callers that need a display hint
    /// (providers endpoint) MUST use this non-async path and fall back
    /// to a config value when it returns `None`.
    pub fn cached_issuer(&self) -> Option<String> {
        // try_read is non-blocking; if the cache write lock is held
        // (extremely rare, only during a discovery refresh), we return
        // None rather than block on public traffic.
        let cache = self.discovery.try_read().ok()?;
        cache.as_ref().and_then(|cached| {
            if cached.is_expired() {
                None
            } else {
                Some(cached.value.issuer.clone())
            }
        })
    }

    /// Fetch and cache the OIDC discovery document (TTL: 1 hour)
    async fn get_discovery(&self) -> Result<OidcDiscovery, DomainError> {
        // Check cache first (return cached value only if not expired)
        {
            let cache = self.discovery.read().await;
            if let Some(ref cached) = *cache {
                if !cached.is_expired() {
                    return Ok(cached.value.clone());
                }
                tracing::debug!("OIDC discovery cache expired, re-fetching");
            }
        }

        // Fetch discovery document
        let issuer = self.config.issuer_url.trim_end_matches('/');
        let discovery_url = format!("{}/.well-known/openid-configuration", issuer);

        tracing::info!("Fetching OIDC discovery from: {}", discovery_url);

        let resp = self
            .http_client
            .get(&discovery_url)
            .send()
            .await
            .map_err(|e| {
                DomainError::new(
                    ErrorKind::InternalError,
                    "OIDC",
                    format!("Failed to fetch OIDC discovery: {}", e),
                )
            })?;

        if !resp.status().is_success() {
            return Err(DomainError::new(
                ErrorKind::InternalError,
                "OIDC",
                format!("OIDC discovery returned status {}", resp.status()),
            ));
        }

        let discovery: OidcDiscovery = resp.json().await.map_err(|e| {
            DomainError::new(
                ErrorKind::InternalError,
                "OIDC",
                format!("Failed to parse OIDC discovery: {}", e),
            )
        })?;

        // Cache it with timestamp
        {
            let mut cache = self.discovery.write().await;
            *cache = Some(Cached::new(discovery.clone()));
        }

        Ok(discovery)
    }

    /// Fetch and cache JWKS document for ID token validation (TTL: 1 hour)
    async fn get_jwks(&self) -> Result<JwksDocument, DomainError> {
        // Check cache first (return cached value only if not expired)
        {
            let cache = self.jwks.read().await;
            if let Some(ref cached) = *cache {
                if !cached.is_expired() {
                    return Ok(cached.value.clone());
                }
                tracing::debug!("OIDC JWKS cache expired, re-fetching");
            }
        }

        let discovery = self.get_discovery().await?;

        tracing::debug!("Fetching JWKS from: {}", discovery.jwks_uri);

        let resp = self
            .http_client
            .get(&discovery.jwks_uri)
            .send()
            .await
            .map_err(|e| {
                DomainError::new(
                    ErrorKind::InternalError,
                    "OIDC",
                    format!("Failed to fetch JWKS: {}", e),
                )
            })?;

        let jwks: JwksDocument = resp.json().await.map_err(|e| {
            DomainError::new(
                ErrorKind::InternalError,
                "OIDC",
                format!("Failed to parse JWKS: {}", e),
            )
        })?;

        // Cache it with timestamp
        {
            let mut cache = self.jwks.write().await;
            *cache = Some(Cached::new(jwks.clone()));
        }

        Ok(jwks)
    }

    /// Find a suitable key from JWKS by kid header (filters out encryption keys)
    fn find_key<'a>(
        jwks: &'a JwksDocument,
        kid: Option<&str>,
    ) -> Option<&'a jsonwebtoken::jwk::Jwk> {
        jwks.keys.iter().find(|k| {
            // Exclude encryption keys (only use signature keys)
            if k.common.public_key_use == Some(jsonwebtoken::jwk::PublicKeyUse::Encryption) {
                return false;
            }
            // Match kid if provided
            if let Some(target_kid) = kid {
                return k.common.key_id.as_deref() == Some(target_kid);
            }
            // No kid specified, include this key
            true
        })
    }

    /// Extract the `kid` from a JWT header without full validation
    fn extract_jwt_kid(token: &str) -> Option<String> {
        let parts: Vec<&str> = token.splitn(3, '.').collect();
        if parts.len() < 2 {
            return None;
        }
        use base64::Engine;
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header_bytes = engine.decode(parts[0]).ok()?;
        let header: serde_json::Value = serde_json::from_slice(&header_bytes).ok()?;
        header
            .get("kid")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }
}

impl OidcServicePort for OidcService {
    async fn get_authorize_url(
        &self,
        state: &str,
        nonce: &str,
        pkce_challenge: &str,
    ) -> Result<String, DomainError> {
        // Fetch or use cached discovery to get the correct authorization_endpoint
        let discovery = self.get_discovery().await?;
        let auth_endpoint = discovery.authorization_endpoint;

        let scopes = self.config.scopes.replace(',', " ");
        let url = format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&nonce={}&code_challenge={}&code_challenge_method=S256",
            auth_endpoint,
            urlencoding::encode(&self.config.client_id),
            urlencoding::encode(&self.config.redirect_uri),
            urlencoding::encode(&scopes),
            urlencoding::encode(state),
            urlencoding::encode(nonce),
            urlencoding::encode(pkce_challenge),
        );

        Ok(url)
    }

    async fn exchange_code(
        &self,
        code: &str,
        pkce_verifier: &str,
    ) -> Result<OidcTokenSet, DomainError> {
        let discovery = self.get_discovery().await?;

        tracing::debug!(
            "Exchanging authorization code at: {}",
            discovery.token_endpoint
        );

        let resp = self
            .http_client
            .post(&discovery.token_endpoint)
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", &self.config.redirect_uri),
                ("client_id", &self.config.client_id),
                ("client_secret", &self.config.client_secret),
                ("code_verifier", pkce_verifier),
            ])
            .send()
            .await
            .map_err(|e| {
                DomainError::new(
                    ErrorKind::InternalError,
                    "OIDC",
                    format!("Token exchange failed: {}", e),
                )
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::error!(
                "OIDC token exchange error: status={}, body={}",
                status,
                body
            );
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "OIDC",
                format!("Token exchange failed with status {}", status),
            ));
        }

        let token_resp: TokenResponse = resp.json().await.map_err(|e| {
            DomainError::new(
                ErrorKind::InternalError,
                "OIDC",
                format!("Failed to parse token response: {}", e),
            )
        })?;

        let id_token = token_resp.id_token.ok_or_else(|| {
            DomainError::new(
                ErrorKind::InternalError,
                "OIDC",
                "No id_token in token response",
            )
        })?;

        Ok(OidcTokenSet {
            access_token: token_resp.access_token,
            id_token,
            refresh_token: token_resp.refresh_token,
        })
    }

    async fn validate_id_token(
        &self,
        id_token: &str,
        expected_nonce: Option<&str>,
    ) -> Result<OidcIdClaims, DomainError> {
        let jwks = self.get_jwks().await?;
        let discovery = self.get_discovery().await?;

        // Extract kid from JWT header
        let kid = Self::extract_jwt_kid(id_token);

        // Find the matching key from JWKS (already typed as Jwk)
        let jwk = Self::find_key(&jwks, kid.as_deref()).ok_or_else(|| {
            DomainError::new(
                ErrorKind::AccessDenied,
                "OIDC",
                "No suitable key found in JWKS for ID token validation",
            )
        })?;

        let decoding_key = jsonwebtoken::DecodingKey::from_jwk(jwk).map_err(|e| {
            DomainError::new(
                ErrorKind::InternalError,
                "OIDC",
                format!("Failed to create decoding key from JWK: {}", e),
            )
        })?;

        // Get algorithm from JWK - convert KeyAlgorithm to Algorithm
        let alg = match jwk.common.key_algorithm {
            Some(jsonwebtoken::jwk::KeyAlgorithm::RS256) => jsonwebtoken::Algorithm::RS256,
            Some(jsonwebtoken::jwk::KeyAlgorithm::RS384) => jsonwebtoken::Algorithm::RS384,
            Some(jsonwebtoken::jwk::KeyAlgorithm::RS512) => jsonwebtoken::Algorithm::RS512,
            Some(jsonwebtoken::jwk::KeyAlgorithm::ES256) => jsonwebtoken::Algorithm::ES256,
            Some(jsonwebtoken::jwk::KeyAlgorithm::ES384) => jsonwebtoken::Algorithm::ES384,
            _ => jsonwebtoken::Algorithm::RS256, // default
        };

        // Build validation: check expiry and issuer
        let mut validation = jsonwebtoken::Validation::new(alg);
        validation.set_issuer(&[&discovery.issuer]);
        validation.set_audience(&[&self.config.client_id]);

        let token_data =
            jsonwebtoken::decode::<IdTokenClaims>(id_token, &decoding_key, &validation).map_err(
                |e| {
                    tracing::warn!("OIDC ID token validation failed: {}", e);
                    DomainError::new(
                        ErrorKind::AccessDenied,
                        "OIDC",
                        format!("ID token validation failed: {}", e),
                    )
                },
            )?;

        let claims = token_data.claims;

        // Verify nonce to prevent token replay attacks
        if let Some(expected) = expected_nonce {
            match &claims.nonce {
                Some(actual) if actual == expected => { /* OK */ }
                Some(actual) => {
                    tracing::warn!("OIDC nonce mismatch: expected={}, got={}", expected, actual);
                    return Err(DomainError::new(
                        ErrorKind::AccessDenied,
                        "OIDC",
                        "ID token nonce mismatch — possible replay attack",
                    ));
                }
                None => {
                    tracing::warn!("OIDC nonce missing from ID token (expected={})", expected);
                    // Some providers don't include nonce; log warning but don't fail
                }
            }
        }

        Ok(OidcIdClaims {
            sub: claims.sub,
            // Safe echo: jsonwebtoken::decode with
            // `validation.set_issuer(&[&discovery.issuer])` above already
            // enforced iss == discovery.issuer, so the discovery value
            // IS the validated iss claim. The caller (auth service uses
            // it for Phase B lazy-rebind) can trust this without a
            // second validation pass.
            iss: discovery.issuer.clone(),
            email: claims.email,
            email_verified: claims.email_verified,
            preferred_username: claims.preferred_username,
            name: claims.name,
            given_name: claims.given_name,
            family_name: claims.family_name,
            groups: claims.groups.unwrap_or_default(),
            picture: claims.picture,
            locale: claims.locale,
            sid: claims.sid,
        })
    }

    async fn fetch_user_info(&self, access_token: &str) -> Result<OidcIdClaims, DomainError> {
        let discovery = self.get_discovery().await?;
        // Capture issuer before we move `userinfo_endpoint` out below.
        // Same rationale as validate_id_token: discovery.issuer IS the
        // authoritative iss for this deployment; fetch_user_info is only
        // called after a successful token exchange with this same issuer.
        let iss = discovery.issuer.clone();

        let userinfo_url = discovery.userinfo_endpoint.ok_or_else(|| {
            DomainError::new(
                ErrorKind::InternalError,
                "OIDC",
                "No userinfo_endpoint in OIDC discovery",
            )
        })?;

        let resp = self
            .http_client
            .get(&userinfo_url)
            .header("Authorization", format!("Bearer {}", access_token))
            .send()
            .await
            .map_err(|e| {
                DomainError::new(
                    ErrorKind::InternalError,
                    "OIDC",
                    format!("UserInfo request failed: {}", e),
                )
            })?;

        if !resp.status().is_success() {
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "OIDC",
                format!("UserInfo returned status {}", resp.status()),
            ));
        }

        let info: UserInfoResponse = resp.json().await.map_err(|e| {
            DomainError::new(
                ErrorKind::InternalError,
                "OIDC",
                format!("Failed to parse UserInfo: {}", e),
            )
        })?;

        Ok(OidcIdClaims {
            sub: info.sub,
            iss,
            email: info.email,
            email_verified: info.email_verified,
            preferred_username: info.preferred_username,
            name: info.name,
            given_name: info.given_name,
            family_name: info.family_name,
            groups: info.groups.unwrap_or_default(),
            picture: info.picture,
            locale: info.locale,
            // UserInfo endpoint doesn't emit sid — it's an id_token-only
            // claim. Callers merging UserInfo into id_token claims must
            // preserve the id_token's sid.
            sid: None,
        })
    }

    fn provider_name(&self) -> &str {
        &self.config.provider_name
    }

    async fn build_end_session_url(
        &self,
        id_token_hint: &str,
        post_logout_redirect_uri: &str,
    ) -> Result<Option<String>, DomainError> {
        let discovery = self.get_discovery().await?;
        let Some(endpoint) = discovery.end_session_endpoint else {
            return Ok(None);
        };
        // client_id is also included: some IdPs (Keycloak in "legacy" mode)
        // use it to look up the registered post_logout_redirect_uri when
        // the id_token_hint is expired or missing.
        let url = format!(
            "{}?id_token_hint={}&post_logout_redirect_uri={}&client_id={}",
            endpoint,
            urlencoding::encode(id_token_hint),
            urlencoding::encode(post_logout_redirect_uri),
            urlencoding::encode(&self.config.client_id),
        );
        Ok(Some(url))
    }

    async fn validate_logout_token(
        &self,
        logout_token: &str,
    ) -> Result<OidcLogoutClaims, DomainError> {
        let jwks = self.get_jwks().await?;
        let discovery = self.get_discovery().await?;

        let kid = Self::extract_jwt_kid(logout_token);
        let jwk = Self::find_key(&jwks, kid.as_deref()).ok_or_else(|| {
            DomainError::new(
                ErrorKind::AccessDenied,
                "OIDC",
                "No suitable key found in JWKS for logout_token validation",
            )
        })?;

        let decoding_key = jsonwebtoken::DecodingKey::from_jwk(jwk).map_err(|e| {
            DomainError::new(
                ErrorKind::InternalError,
                "OIDC",
                format!("Failed to create decoding key from JWK: {}", e),
            )
        })?;

        let alg = match jwk.common.key_algorithm {
            Some(jsonwebtoken::jwk::KeyAlgorithm::RS256) => jsonwebtoken::Algorithm::RS256,
            Some(jsonwebtoken::jwk::KeyAlgorithm::RS384) => jsonwebtoken::Algorithm::RS384,
            Some(jsonwebtoken::jwk::KeyAlgorithm::RS512) => jsonwebtoken::Algorithm::RS512,
            Some(jsonwebtoken::jwk::KeyAlgorithm::ES256) => jsonwebtoken::Algorithm::ES256,
            Some(jsonwebtoken::jwk::KeyAlgorithm::ES384) => jsonwebtoken::Algorithm::ES384,
            _ => jsonwebtoken::Algorithm::RS256,
        };

        // Spec: iss + aud validated same as id_token. exp is OPTIONAL for
        // logout_tokens (unlike id_tokens where it's mandatory), so tell
        // jsonwebtoken not to require it; the iat-based freshness clamp
        // below enforces our own upper bound.
        let mut validation = jsonwebtoken::Validation::new(alg);
        validation.set_issuer(&[&discovery.issuer]);
        validation.set_audience(&[&self.config.client_id]);
        validation.required_spec_claims.remove("exp");

        let token_data =
            jsonwebtoken::decode::<LogoutTokenClaims>(logout_token, &decoding_key, &validation)
                .map_err(|e| {
                    tracing::warn!("OIDC logout_token validation failed: {}", e);
                    DomainError::new(
                        ErrorKind::AccessDenied,
                        "OIDC",
                        format!("logout_token validation failed: {}", e),
                    )
                })?;

        let claims = token_data.claims;

        // Spec §2.4: MUST NOT contain a nonce claim (that's an id_token thing).
        // If we see one, the IdP is confused or an attacker is replaying an
        // id_token as a logout_token; refuse.
        if claims.nonce.is_some() {
            tracing::warn!(
                "OIDC logout_token rejected: nonce claim present (spec §2.4 forbids it)"
            );
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "OIDC",
                "logout_token must not contain nonce",
            ));
        }

        // Spec §2.4: MUST have `events` claim as a JSON object with a
        // property whose name is the backchannel-logout URI. Value is
        // typically `{}` — we don't inspect it.
        let has_event = claims
            .events
            .as_object()
            .map(|o| o.contains_key(BACKCHANNEL_LOGOUT_EVENT))
            .unwrap_or(false);
        if !has_event {
            tracing::warn!(
                "OIDC logout_token rejected: missing events.'{}'",
                BACKCHANNEL_LOGOUT_EVENT
            );
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "OIDC",
                "logout_token missing required backchannel-logout event",
            ));
        }

        // Spec §2.4: MUST contain `sub` and/or `sid`. Without one, we have
        // nothing to key the revocation on.
        if claims.sub.is_none() && claims.sid.is_none() {
            tracing::warn!("OIDC logout_token rejected: neither sub nor sid present");
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "OIDC",
                "logout_token must contain sub or sid",
            ));
        }

        // Freshness clamp — iat within the last 5 minutes. Prevents
        // rogue replay of an old logout_token. Not spec-mandated but
        // recommended (BCL §2.6).
        let now = chrono::Utc::now().timestamp();
        const MAX_AGE_SECS: i64 = 300;
        if (now - claims.iat).abs() > MAX_AGE_SECS {
            tracing::warn!(
                "OIDC logout_token rejected: iat too old (age={}s, max={}s)",
                now - claims.iat,
                MAX_AGE_SECS
            );
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "OIDC",
                "logout_token iat outside freshness window",
            ));
        }

        // Belt-and-suspenders — the jsonwebtoken decode already enforced
        // iss+aud, but log if we get here somehow. Actively used only if
        // future changes to Validation config regress the check.
        if claims.iss != discovery.issuer {
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "OIDC",
                "logout_token iss mismatch",
            ));
        }
        // aud may be string or array — accept either shape carrying our client_id.
        let aud_ok = match &claims.aud {
            serde_json::Value::String(s) => s == &self.config.client_id,
            serde_json::Value::Array(a) => a
                .iter()
                .any(|v| v.as_str() == Some(self.config.client_id.as_str())),
            _ => false,
        };
        if !aud_ok {
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "OIDC",
                "logout_token aud mismatch",
            ));
        }

        Ok(OidcLogoutClaims {
            sub: claims.sub,
            sid: claims.sid,
            jti: claims.jti,
            iss: claims.iss,
        })
    }
}

// We need urlencoding — let's use a minimal inline implementation
mod urlencoding {
    pub fn encode(input: &str) -> String {
        let mut result = String::with_capacity(input.len() * 3);
        for byte in input.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    result.push(byte as char);
                }
                _ => {
                    result.push('%');
                    result.push_str(&format!("{:02X}", byte));
                }
            }
        }
        result
    }
}
