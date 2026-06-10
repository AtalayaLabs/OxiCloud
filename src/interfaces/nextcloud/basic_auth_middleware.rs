use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::Engine;
use std::sync::Arc;

use crate::common::di::AppState;
use crate::interfaces::middleware::auth::CurrentUser;

#[derive(Debug, thiserror::Error)]
pub enum NextcloudAuthError {
    #[error("Unauthorized")]
    Unauthorized,
    #[error("Nextcloud services unavailable")]
    ServiceUnavailable,
    #[error("Internal error: {0}")]
    Internal(String),
}

impl IntoResponse for NextcloudAuthError {
    fn into_response(self) -> Response {
        match self {
            NextcloudAuthError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Basic realm=\"OxiCloud\"")],
                "Unauthorized",
            )
                .into_response(),
            NextcloudAuthError::ServiceUnavailable => {
                (StatusCode::SERVICE_UNAVAILABLE, "Nextcloud unavailable").into_response()
            }
            NextcloudAuthError::Internal(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
            }
        }
    }
}

pub async fn basic_auth_middleware(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    mut request: Request,
    next: Next,
) -> Result<Response, NextcloudAuthError> {
    tracing::debug!("[NC] {} {}", request.method(), request.uri());

    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            tracing::warn!(
                "[NC] 401 no auth header: {} {}",
                request.method(),
                request.uri()
            );
            NextcloudAuthError::Unauthorized
        })?;

    let (username, password) =
        parse_basic_auth(auth_header).ok_or(NextcloudAuthError::Unauthorized)?;

    // Check account lockout before attempting password verification (saves CPU).
    // The lockout is per (account, IP), see #323 for rationale.
    let client_ip = crate::interfaces::middleware::rate_limit::extract_client_ip(&request);
    if let Some(auth_svc) = state.auth_service.as_ref()
        && let Err(secs) = auth_svc.login_lockout.check(&username, &client_ip)
    {
        tracing::warn!(
            username = %username,
            client_ip = %client_ip,
            lockout_remaining_secs = secs,
            "[NC] Account locked, too many failed attempts from this IP"
        );
        return Err(NextcloudAuthError::Unauthorized);
    }

    let nextcloud = state
        .nextcloud
        .as_ref()
        .ok_or(NextcloudAuthError::ServiceUnavailable)?;

    match nextcloud
        .app_passwords
        .verify_basic_auth(&username, &password)
        .await
    {
        Ok((user_id, uname, email, role)) => {
            // Reset lockout counter on success
            if let Some(auth_svc) = state.auth_service.as_ref() {
                auth_svc.login_lockout.record_success(&username, &client_ip);
            }
            // External users must never authenticate against the NC
            // surface — that whole subtree (WebDAV files, uploads,
            // trashbin, OCS user info, sharees autocomplete, etc.) has
            // no semantic meaning for a magic-link-only principal, and
            // an app password would be a persistent credential
            // bypassing the magic-link-eligibility rule. POST
            // /api/auth/app-passwords also gates externals upfront;
            // this is the belt-and-braces check in case one slipped
            // through (e.g. user later flipped to is_external).
            if let Some(auth_svc) = state.auth_service.as_ref()
                && let Ok(user) = auth_svc
                    .auth_application_service
                    .get_user_by_id(user_id)
                    .await
                && user.is_external
            {
                tracing::info!(
                    target: "audit",
                    event = "auth.nc_basic_rejected",
                    reason = "external_user",
                    user_id = %user_id,
                    "👮🏻‍♂️ External user attempted NC Basic auth — rejected"
                );
                return Err(NextcloudAuthError::Unauthorized);
            }
            // Populate the deferred `user_id` field on the request
            // tracing span (declared in `middleware/trace_span.rs::ClientIpMakeSpan`).
            // Mirrors what `interfaces/middleware/auth.rs` does for the
            // JWT path so the two auth surfaces produce log lines with
            // the same structured shape — without this, every NC
            // request would appear in the logs with `user_id=-`,
            // making it harder to correlate WebDAV / OCS activity to
            // a specific principal.
            tracing::Span::current().record("user_id", user_id.to_string());
            request.extensions_mut().insert(Arc::new(CurrentUser {
                id: user_id,
                username: uname,
                email,
                role,
            }));
            Ok(next.run(request).await)
        }
        Err(_) => {
            // Record failed attempt for lockout tracking
            if let Some(auth_svc) = state.auth_service.as_ref() {
                auth_svc.login_lockout.record_failure(&username, &client_ip);
            }
            Err(NextcloudAuthError::Unauthorized)
        }
    }
}

/// Parse a `Basic` Authorization header into `(username, password)`.
pub fn parse_basic_auth(header_value: &str) -> Option<(String, String)> {
    let mut parts = header_value.splitn(2, ' ');
    let scheme = parts.next()?.trim();
    let encoded = parts.next()?.trim();

    if !scheme.eq_ignore_ascii_case("Basic") {
        return None;
    }

    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (user, pass) = decoded.split_once(':')?;

    Some((user.to_string(), pass.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_valid_basic_auth() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("alice:secret123");
        let header = format!("Basic {}", encoded);
        let (user, pass) = parse_basic_auth(&header).expect("should parse");
        assert_eq!(user, "alice");
        assert_eq!(pass, "secret123");
    }

    #[test]
    fn test_parse_basic_auth_with_colon_in_password() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("user:pass:with:colons");
        let header = format!("Basic {}", encoded);
        let (user, pass) = parse_basic_auth(&header).expect("should parse");
        assert_eq!(user, "user");
        assert_eq!(pass, "pass:with:colons");
    }

    #[test]
    fn test_parse_basic_auth_bearer_scheme_rejected() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("user:pass");
        let header = format!("Bearer {}", encoded);
        assert!(parse_basic_auth(&header).is_none());
    }

    #[test]
    fn test_parse_basic_auth_missing_colon() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("nocolon");
        let header = format!("Basic {}", encoded);
        assert!(parse_basic_auth(&header).is_none());
    }

    #[test]
    fn test_parse_basic_auth_invalid_base64() {
        assert!(parse_basic_auth("Basic not-valid-base64!!!").is_none());
    }

    #[test]
    fn test_parse_basic_auth_case_insensitive_scheme() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("user:pass");
        let header = format!("BASIC {}", encoded);
        let result = parse_basic_auth(&header);
        assert!(result.is_some());
    }
}
