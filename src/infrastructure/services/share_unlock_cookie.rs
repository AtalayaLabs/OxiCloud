//! Signed cookie issued after `/verify` so subsequent share requests bypass
//! the password gate. JWT carries `sub` (share token), `exp`, `iat`.

use chrono::Utc;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};

use crate::common::errors::DomainError;

pub const DEFAULT_TTL_SECS: i64 = 3600;

#[derive(Debug, Serialize, Deserialize)]
struct UnlockClaims {
    sub: String,
    exp: i64,
    iat: i64,
}

pub fn issue_jwt(secret: &str, share_token: &str, ttl_secs: i64) -> Result<String, DomainError> {
    if secret.is_empty() {
        return Err(DomainError::internal_error(
            "ShareUnlockCookie",
            "JWT secret is empty",
        ));
    }
    let now = Utc::now().timestamp();
    let claims = UnlockClaims {
        sub: share_token.to_string(),
        exp: now + ttl_secs,
        iat: now,
    };
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| DomainError::internal_error("ShareUnlockCookie", format!("sign: {}", e)))
}

/// `true` iff the JWT is well-formed, signed by `secret`, unexpired, and its
/// `sub` matches `share_token`. Any failure returns `false`.
pub fn verify_jwt(secret: &str, share_token: &str, jwt: &str) -> bool {
    if secret.is_empty() || jwt.is_empty() {
        return false;
    }
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = true;
    validation.leeway = 0;
    validation.required_spec_claims.clear();
    validation.required_spec_claims.insert("exp".to_string());
    validation.required_spec_claims.insert("sub".to_string());

    match decode::<UnlockClaims>(
        jwt,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    ) {
        Ok(data) => data.claims.sub == share_token,
        Err(_) => false,
    }
}

pub fn extract_from_cookie_header(cookie_header: &str, share_token: &str) -> Option<String> {
    let target_name = format!("oxi_share_unlock_{}", share_token);
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some(eq_idx) = part.find('=') {
            let (name, value) = part.split_at(eq_idx);
            if name == target_name {
                return Some(value[1..].to_string());
            }
        }
    }
    None
}

pub fn build_set_cookie(share_token: &str, jwt: &str, ttl_secs: i64) -> String {
    format!(
        "oxi_share_unlock_{}={}; HttpOnly; SameSite=Lax; Path={}; Max-Age={}",
        share_token,
        jwt,
        unlock_cookie_path(),
        ttl_secs
    )
}

/// Cookie `Path` for the unlock cookie — the deployment base path
/// (`OXICLOUD_BASE_PATH`) or `/` at the root. Browser-facing, so unlike
/// nest-stripped route matching it must carry the prefix.
fn unlock_cookie_path() -> &'static str {
    let base = crate::common::config::server_base_path();
    if base.is_empty() { "/" } else { base }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const TOKEN: &str = "49dc31a5-62c8-4ce5-b82c-16092b513805";

    #[test]
    fn issue_then_verify_succeeds() {
        let jwt = issue_jwt(TEST_SECRET, TOKEN, 60).expect("issue");
        assert!(verify_jwt(TEST_SECRET, TOKEN, &jwt));
    }

    #[test]
    fn verify_rejects_different_token() {
        let jwt = issue_jwt(TEST_SECRET, TOKEN, 60).expect("issue");
        assert!(!verify_jwt(TEST_SECRET, "other-token", &jwt));
    }

    #[test]
    fn verify_rejects_different_secret() {
        let jwt = issue_jwt(TEST_SECRET, TOKEN, 60).expect("issue");
        let other_secret = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        assert!(!verify_jwt(other_secret, TOKEN, &jwt));
    }

    #[test]
    fn verify_rejects_expired_token() {
        let jwt = issue_jwt(TEST_SECRET, TOKEN, -10).expect("issue");
        assert!(!verify_jwt(TEST_SECRET, TOKEN, &jwt));
    }

    #[test]
    fn verify_rejects_garbage() {
        assert!(!verify_jwt(TEST_SECRET, TOKEN, "not.a.jwt"));
        assert!(!verify_jwt(TEST_SECRET, TOKEN, ""));
    }

    #[test]
    fn issue_rejects_empty_secret() {
        assert!(issue_jwt("", TOKEN, 60).is_err());
    }

    #[test]
    fn verify_rejects_empty_secret_or_jwt() {
        let jwt = issue_jwt(TEST_SECRET, TOKEN, 60).expect("issue");
        assert!(!verify_jwt("", TOKEN, &jwt));
        assert!(!verify_jwt(TEST_SECRET, TOKEN, ""));
    }

    #[test]
    fn extract_from_cookie_header_finds_target() {
        let jwt = "abc.def.ghi";
        let header = format!(
            "session=xyz; oxi_share_unlock_{}={}; theme=dark",
            TOKEN, jwt
        );
        assert_eq!(
            extract_from_cookie_header(&header, TOKEN),
            Some(jwt.to_string())
        );
    }

    #[test]
    fn extract_from_cookie_header_returns_none_for_other_token() {
        let header = format!("oxi_share_unlock_{}=xxx", TOKEN);
        assert_eq!(extract_from_cookie_header(&header, "different-token"), None);
    }

    #[test]
    fn extract_from_cookie_header_handles_empty() {
        assert_eq!(extract_from_cookie_header("", TOKEN), None);
        assert_eq!(extract_from_cookie_header("malformed", TOKEN), None);
    }

    #[test]
    fn build_set_cookie_has_required_attributes() {
        let s = build_set_cookie(TOKEN, "jwt.value.here", 3600);
        assert!(s.contains(&format!("oxi_share_unlock_{}=jwt.value.here", TOKEN)));
        assert!(s.contains("HttpOnly"));
        assert!(s.contains("SameSite=Lax"));
        assert!(s.contains("Path=/"));
        assert!(s.contains("Max-Age=3600"));
    }
}
