//! WOPI access token service.
//!
//! Generates and validates WOPI-scoped JWT tokens that are separate from
//! the regular authentication tokens. Uses the same `jsonwebtoken` crate
//! but with a distinct `scope: "wopi"` claim to prevent token confusion.

use chrono::Utc;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};

use crate::common::errors::{DomainError, ErrorKind};

/// JWT claims for WOPI access tokens.
#[derive(Debug, Serialize, Deserialize)]
pub struct WopiTokenClaims {
    /// User ID
    pub sub: String,
    /// File ID this token grants access to
    pub file_id: String,
    /// Whether the user can write (edit) the file
    pub can_write: bool,
    /// Token scope — always "wopi" to distinguish from auth tokens
    pub scope: String,
    /// Display name for the editor UI
    pub username: String,
    /// Optional WOPI action this token was minted for (`"edit"`, `"view"`, `"convert"`).
    /// `None` on legacy tokens falls back to `"edit"` when `can_write` else `"view"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// Optional target extension for `"convert"` tokens (e.g. `"docx"`, `"odt"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_ext: Option<String>,
    /// Expiration timestamp (seconds since Unix epoch)
    pub exp: i64,
    /// Issued at timestamp
    pub iat: i64,
}

/// Service for generating and validating WOPI access tokens.
pub struct WopiTokenService {
    /// Pre-built signing key — `EncodingKey::from_secret` copies the secret into
    /// a fresh `Vec` on each call, so build it once (mirrors `JwtTokenService`).
    encoding_key: EncodingKey,
    /// Pre-built verification key — same copy-per-call cost as `encoding_key`.
    decoding_key: DecodingKey,
    /// Pre-built HS256 validation config — `Validation::new` allocates a
    /// `required_spec_claims` HashSet + an `algorithms` Vec; Office/Collabora
    /// hosts poll `validate_token` continuously (benches/ROUND19.md §M2).
    validation: Validation,
    token_ttl_secs: i64,
}

impl WopiTokenService {
    pub fn new(secret: String, token_ttl_secs: i64) -> Self {
        Self {
            encoding_key: EncodingKey::from_secret(secret.as_bytes()),
            decoding_key: DecodingKey::from_secret(secret.as_bytes()),
            validation: Validation::new(Algorithm::HS256),
            token_ttl_secs,
        }
    }

    /// Generate a WOPI access token for a specific file and user.
    ///
    /// Returns `(token_string, expiration_unix_ms)`.
    pub fn generate_token(
        &self,
        file_id: &str,
        user_id: &str,
        username: &str,
        can_write: bool,
    ) -> Result<(String, i64), DomainError> {
        self.generate_token_with_action(file_id, user_id, username, can_write, None, None)
    }

    /// Generate a WOPI access token bound to a specific WOPI action and optional conversion target.
    ///
    /// Returns `(token_string, expiration_unix_ms)`.
    pub fn generate_token_with_action(
        &self,
        file_id: &str,
        user_id: &str,
        username: &str,
        can_write: bool,
        action: Option<&str>,
        target_ext: Option<&str>,
    ) -> Result<(String, i64), DomainError> {
        let now = Utc::now().timestamp();
        let claims = WopiTokenClaims {
            sub: user_id.to_string(),
            file_id: file_id.to_string(),
            can_write,
            scope: "wopi".to_string(),
            username: username.to_string(),
            action: action.map(str::to_string),
            target_ext: target_ext.map(str::to_string),
            exp: now + self.token_ttl_secs,
            iat: now,
        };

        let token = encode(&Header::default(), &claims, &self.encoding_key).map_err(|e| {
            DomainError::new(
                ErrorKind::InternalError,
                "WopiTokenService",
                format!("Failed to generate WOPI token: {}", e),
            )
        })?;

        let expires_at_unix_ms = claims.exp * 1000;
        Ok((token, expires_at_unix_ms))
    }

    /// Validate a WOPI access token and extract its claims.
    pub fn validate_token(&self, token: &str) -> Result<WopiTokenClaims, DomainError> {
        let token_data = decode::<WopiTokenClaims>(token, &self.decoding_key, &self.validation)
            .map_err(|e| match e.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => DomainError::new(
                    ErrorKind::AccessDenied,
                    "WopiTokenService",
                    "WOPI token expired",
                ),
                _ => DomainError::new(
                    ErrorKind::AccessDenied,
                    "WopiTokenService",
                    format!("Invalid WOPI token: {}", e),
                ),
            })?;

        let claims = token_data.claims;

        if claims.scope != "wopi" {
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "WopiTokenService",
                "Token is not a WOPI token",
            ));
        }

        Ok(claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> WopiTokenService {
        WopiTokenService::new("test_secret_at_least_32_bytes_long!!".to_string(), 3600)
    }

    #[test]
    fn test_generate_and_validate() {
        let svc = service();
        let (token, ttl_ms) = svc
            .generate_token("file-123", "user-456", "test_user", true)
            .expect("Should generate token");

        let claims = svc.validate_token(&token).expect("Should validate");
        assert_eq!(claims.file_id, "file-123");
        assert_eq!(claims.sub, "user-456");
        assert!(claims.can_write);
        assert_eq!(claims.scope, "wopi");
        assert_eq!(claims.username, "test_user");

        // access_token_ttl must be absolute UNIX time in milliseconds.
        assert_eq!(ttl_ms, claims.exp * 1000);
        assert!(ttl_ms > claims.iat * 1000);
    }

    #[test]
    fn test_reject_invalid_token() {
        let svc = service();
        let result = svc.validate_token("garbage");
        assert!(result.is_err());
    }

    #[test]
    fn test_reject_wrong_secret() {
        let svc1 = service();
        let svc2 = WopiTokenService::new("different_secret_also_32_bytes!!".to_string(), 3600);

        let (token, _) = svc1
            .generate_token("file-1", "user-1", "test_user", false)
            .expect("Should generate");
        let result = svc2.validate_token(&token);
        assert!(result.is_err());
    }

    #[test]
    fn test_read_only_token() {
        let svc = service();
        let (token, _) = svc
            .generate_token("file-1", "user-1", "test_user", false)
            .expect("Should generate");
        let claims = svc.validate_token(&token).expect("Should validate");
        assert!(!claims.can_write);
        assert_eq!(claims.action, None);
        assert_eq!(claims.target_ext, None);
    }

    #[test]
    fn test_generate_with_action_and_target_ext() {
        let svc = service();
        let (token, _) = svc
            .generate_token_with_action(
                "file-1",
                "user-1",
                "test_user",
                true,
                Some("convert"),
                Some("docx"),
            )
            .expect("Should generate");
        let claims = svc.validate_token(&token).expect("Should validate");
        assert!(claims.can_write);
        assert_eq!(claims.action.as_deref(), Some("convert"));
        assert_eq!(claims.target_ext.as_deref(), Some("docx"));
    }

    // Legacy tokens minted prior to the `action`/`target_ext` claims must
    // continue decoding cleanly during rolling deployments.
    #[test]
    fn test_validate_legacy_token_without_action_fields() {
        #[derive(Serialize)]
        struct LegacyClaims {
            sub: &'static str,
            file_id: &'static str,
            can_write: bool,
            scope: &'static str,
            username: &'static str,
            exp: i64,
            iat: i64,
        }

        let secret = "test_secret_at_least_32_bytes_long!!";
        let now = Utc::now().timestamp();
        let legacy = LegacyClaims {
            sub: "user-1",
            file_id: "file-1",
            can_write: true,
            scope: "wopi",
            username: "legacy_user",
            exp: now + 3600,
            iat: now,
        };
        let token = encode(
            &Header::default(),
            &legacy,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .expect("Should encode legacy token");

        let svc = WopiTokenService::new(secret.to_string(), 3600);
        let claims = svc.validate_token(&token).expect("Should decode legacy");
        assert!(claims.can_write);
        assert_eq!(claims.action, None);
        assert_eq!(claims.target_ext, None);
    }
}
