use axum::{
    extract::{FromRequestParts, Request, State},
    http::{StatusCode, header, request::Parts},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::convert::Infallible;
use std::sync::Arc;
use uuid::Uuid;

use crate::common::di::AppState;

// Re-export CurrentUser from application layer for use in handlers
pub use crate::application::dtos::user_dto::CurrentUser;
use crate::application::ports::auth_ports::TokenServicePort;
use crate::interfaces::middleware::user::{LiveRole, resolve_live_role};

/// Marker inserted into request extensions when the user was authenticated
/// via the `oxicloud_access` HttpOnly cookie rather than a Bearer/Basic header.
/// The CSRF middleware uses this to decide whether CSRF validation is required.
#[derive(Clone, Copy, Debug)]
pub struct CookieAuthenticated;

// Newtype over Arc<CurrentUser> for zero-allocation extraction.
// `Deref<Target = CurrentUser>` lets handlers access `.id`, `.username`,
// `.email`, `.role` transparently — no signature changes needed.
#[derive(Clone, Debug)]
pub struct AuthUser(pub Arc<CurrentUser>);

impl std::ops::Deref for AuthUser {
    type Target = CurrentUser;
    #[inline]
    fn deref(&self) -> &CurrentUser {
        &self.0
    }
}

/// Reusable extractor that gets the user_id of the authenticated user.
/// Automatically extracted from the `CurrentUser` inserted by the auth middleware.
///
/// Usage in handlers:
/// ```ignore
/// async fn my_handler(CurrentUserId(user_id): CurrentUserId) -> impl IntoResponse { ... }
/// ```
#[derive(Clone, Debug)]
pub struct CurrentUserId(pub Uuid);

// Implement FromRequestParts for AuthUser — allows using `auth_user: AuthUser` in handlers.
// Cost: 1 atomic increment (~1 ns) instead of 3 String clones (~100 ns + 3 mallocs).
impl<S> FromRequestParts<S> for AuthUser
where
    S: Send + Sync,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Arc<CurrentUser>>()
            .cloned()
            .map(AuthUser)
            .ok_or(AuthError::UserNotFound)
    }
}

// Implement FromRequestParts for CurrentUserId — lightweight extractor for user_id only
impl<S> FromRequestParts<S> for CurrentUserId
where
    S: Send + Sync,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Arc<CurrentUser>>()
            .map(|cu| CurrentUserId(cu.id))
            .ok_or(AuthError::UserNotFound)
    }
}

/// Optional user ID extractor – never fails.
/// Yields `Some(id)` when auth middleware ran, `None` otherwise.
#[derive(Clone, Debug)]
pub struct OptionalUserId(pub Option<Uuid>);

impl<S> FromRequestParts<S> for OptionalUserId
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(OptionalUserId(
            parts.extensions.get::<Arc<CurrentUser>>().map(|cu| cu.id),
        ))
    }
}

// Error for authentication operations
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("Token not provided")]
    TokenNotProvided,

    #[error("Invalid token: {0}")]
    InvalidToken(String),

    #[error("Token expired")]
    TokenExpired,

    #[error("User not found")]
    UserNotFound,

    #[error("Account is no longer active")]
    AccountInactive,

    #[error("Access denied: {0}")]
    AccessDenied(String),

    #[error("Authentication service unavailable")]
    AuthServiceUnavailable,
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let (status, error_message) = match self {
            AuthError::TokenNotProvided => {
                (StatusCode::UNAUTHORIZED, "Token not provided".to_string())
            }
            AuthError::InvalidToken(msg) => (StatusCode::UNAUTHORIZED, msg),
            AuthError::TokenExpired => (StatusCode::UNAUTHORIZED, "Token expired".to_string()),
            AuthError::UserNotFound => (StatusCode::UNAUTHORIZED, "User not found".to_string()),
            AuthError::AccountInactive => (
                StatusCode::UNAUTHORIZED,
                "Account is no longer active".to_string(),
            ),
            AuthError::AccessDenied(msg) => (StatusCode::FORBIDDEN, msg),
            AuthError::AuthServiceUnavailable => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Authentication service unavailable".to_string(),
            ),
        };

        let body = axum::Json(serde_json::json!({
            "error": error_message
        }));

        (status, body).into_response()
    }
}

/// Secure authentication middleware.
///
/// Supports three authentication methods (tried in order):
/// 1. **Bearer JWT** — standard token in `Authorization: Bearer <token>`
/// 2. **Basic Auth with App Passwords** — for DAV clients (DAVx⁵, Thunderbird, rclone)
///    that send `Authorization: Basic base64(username:app_password)`
/// 3. **HttpOnly Cookie** — `oxicloud_access` cookie set by the login endpoint;
///    used by browser-based sessions so tokens are never exposed to JS.
///
/// Bearer is tried first; if no Bearer header is found, Basic is attempted,
/// then the cookie fallback.
pub async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Result<Response, AuthError> {
    // Borrow the Authorization header straight from the request instead of
    // taking axum's `HeaderMap` extractor, which clones the whole map (~2
    // allocs) on every authenticated request purely to read it
    // (benches/ROUND14.md §A4). The borrow is dead by the time each arm
    // reaches `request.extensions_mut()` / `next.run(request)` (NLL), so no
    // owned copy is needed.
    let auth_header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());

    // ── 1. Try Bearer JWT ────────────────────────────────────────
    if let Some(header_value) = auth_header {
        if let Some(token_str) = header_value.strip_prefix("Bearer ") {
            let token_str = token_str.trim();
            if !token_str.is_empty() {
                tracing::debug!("Processing Bearer authentication token");

                if let Some(auth_service) = state.auth_service.as_ref() {
                    let token_service = &auth_service.token_service;
                    match token_service.validate_token(token_str) {
                        Ok(claims) => {
                            tracing::debug!(
                                "Token validated successfully for user: {}",
                                claims.username
                            );
                            // Pre-parsed at decode time (benches/ROUND14.md §A3);
                            // nil only for a malformed sub, which we reject as before.
                            let user_id = claims.sub_id;
                            if user_id.is_nil() {
                                return Err(AuthError::InvalidToken(
                                    "Invalid user ID in token".to_string(),
                                ));
                            }
                            // A cryptographically valid token must not outlive the
                            // account: re-check the live record so deactivation,
                            // deletion and demotion take effect within the flags-cache
                            // TTL instead of waiting for token expiry. The returned
                            // role is authoritative — never the frozen JWT claim.
                            let role = match resolve_live_role(
                                auth_service.auth_application_service.as_ref(),
                                user_id,
                                &claims.role,
                            )
                            .await
                            {
                                LiveRole::Active(role) => role,
                                LiveRole::Revoked => return Err(AuthError::AccountInactive),
                            };
                            // `username`/`email` are `Arc<str>` refcount
                            // bumps out of the cached claims; `role` is an
                            // inline SmolStr — the whole build is 1 alloc
                            // (the `Arc::new`) instead of 4.
                            let current_user = Arc::new(CurrentUser {
                                id: user_id,
                                username: Arc::clone(&claims.username),
                                email: Arc::clone(&claims.email),
                                role,
                                dpop_jkt: claims.dpop_jkt.clone(),
                            });
                            request.extensions_mut().insert(current_user);
                            tracing::Span::current()
                                .record("user_id", tracing::field::display(user_id));
                            // Bump per-session liveness for the
                            // Prometheus gauges. O(1) DashMap upsert
                            // — no I/O on this hot path. The `sid`
                            // claim is `None` on tokens minted by
                            // pre-`sid` builds, in which case the
                            // stamp is skipped entirely — no
                            // fallback lookup, no round-trip.
                            if let (Some(sid), Some(tracker)) =
                                (claims.sid, state.last_seen_tracker.as_ref())
                            {
                                tracker.stamp(sid);
                            }
                            return Ok(next.run(request).await);
                        }
                        Err(e) => {
                            tracing::warn!("Bearer token validation failed: {}", e);
                            return Err(AuthError::InvalidToken(format!("Invalid token: {}", e)));
                        }
                    }
                }
            }
        }

        // ── 2. Try Basic Auth with App Passwords ─────────────────
        if let Some(basic_encoded) = header_value.strip_prefix("Basic ") {
            let basic_encoded = basic_encoded.trim();
            if !basic_encoded.is_empty() {
                tracing::debug!("Processing Basic authentication (app password)");

                // Decode base64(username:password)
                use base64::Engine;
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(basic_encoded)
                    .map_err(|_| {
                        AuthError::InvalidToken("Invalid Basic auth encoding".to_string())
                    })?;
                let credentials = String::from_utf8(decoded).map_err(|_| {
                    AuthError::InvalidToken("Invalid Basic auth encoding".to_string())
                })?;

                let (username, password) = credentials.split_once(':').ok_or_else(|| {
                    AuthError::InvalidToken("Invalid Basic auth format".to_string())
                })?;

                if let Some(app_pw_service) = state.app_password_service.as_ref() {
                    match app_pw_service.verify_basic_auth(username, password).await {
                        Ok((user_id, uname, email, role)) => {
                            tracing::debug!(
                                "App password authentication successful for user: {}",
                                uname
                            );
                            // App-password sessions are always unbound —
                            // they belong to NC clients / CLI / mobile
                            // tools without WebCrypto. DPoP middleware
                            // exempts them.
                            let current_user = Arc::new(CurrentUser {
                                id: user_id,
                                username: uname,
                                email,
                                role,
                                dpop_jkt: None,
                            });
                            request.extensions_mut().insert(current_user);
                            tracing::Span::current()
                                .record("user_id", tracing::field::display(user_id));
                            return Ok(next.run(request).await);
                        }
                        Err(e) => {
                            tracing::warn!("App password verification failed: {}", e);
                            // For DAV clients: include WWW-Authenticate so the client
                            // re-prompts for credentials rather than failing silently.
                            if is_dav_path(request.uri().path()) {
                                return Ok(dav_basic_auth_challenge(
                                    "Invalid username or app password",
                                ));
                            }
                            return Err(AuthError::InvalidToken(
                                "Invalid username or app password".to_string(),
                            ));
                        }
                    }
                } else {
                    tracing::warn!("Basic auth attempted but app password service not configured");
                    return Err(AuthError::InvalidToken(
                        "App passwords are not enabled".to_string(),
                    ));
                }
            }
        }
    }

    // ── 3. Try HttpOnly cookie (browser sessions) ────────────────
    {
        use crate::interfaces::api::cookie_auth;

        if let Some(token_str) =
            cookie_auth::extract_cookie_str(request.headers(), cookie_auth::ACCESS_COOKIE)
            && !token_str.is_empty()
        {
            tracing::debug!("Processing cookie-based authentication");

            if let Some(auth_service) = state.auth_service.as_ref() {
                let token_service = &auth_service.token_service;
                match token_service.validate_token(token_str) {
                    Ok(claims) => {
                        tracing::debug!("Cookie token validated for user: {}", claims.username);
                        // Pre-parsed at decode time (benches/ROUND14.md §A3).
                        let user_id = claims.sub_id;
                        if user_id.is_nil() {
                            return Err(AuthError::InvalidToken(
                                "Invalid user ID in token".to_string(),
                            ));
                        }
                        // Same live-account re-check as the Bearer path. On
                        // revocation we fall through (rather than erroring) so the
                        // browser receives the standard 401 and redirects to
                        // /login, exactly like an invalid or expired cookie.
                        match resolve_live_role(
                            auth_service.auth_application_service.as_ref(),
                            user_id,
                            &claims.role,
                        )
                        .await
                        {
                            LiveRole::Active(role) => {
                                let current_user = Arc::new(CurrentUser {
                                    id: user_id,
                                    username: Arc::clone(&claims.username),
                                    email: Arc::clone(&claims.email),
                                    role,
                                    dpop_jkt: claims.dpop_jkt.clone(),
                                });
                                request.extensions_mut().insert(current_user);
                                request.extensions_mut().insert(CookieAuthenticated);
                                tracing::Span::current()
                                    .record("user_id", tracing::field::display(user_id));
                                // Cookie-auth branch stamps the same
                                // way as the Bearer branch above —
                                // see that site for the O(1) /
                                // no-DB rationale.
                                if let (Some(sid), Some(tracker)) =
                                    (claims.sid, state.last_seen_tracker.as_ref())
                                {
                                    tracker.stamp(sid);
                                }
                                return Ok(next.run(request).await);
                            }
                            LiveRole::Revoked => {
                                // Fall through to the unauthenticated 401 / login redirect.
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!("Cookie token validation failed: {}", e);
                        // Don't return error — fall through to "no token" so
                        // the browser gets a 401 and can redirect to /login.
                    }
                }
            }
        }
    }

    // No valid credentials found via any method.
    if state.auth_service.is_none() {
        tracing::error!("Auth middleware invoked but auth service is not configured");
        return Err(AuthError::AuthServiceUnavailable);
    }

    // For DAV requests with no credentials at all: return 401 with
    // WWW-Authenticate so that spec-compliant clients (Thunderbird, DAVx5,
    // Apple Calendar/Contacts, Nautilus, Cyberduck, Windows Explorer, macOS
    // Finder) know to prompt for credentials and retry. Unlike `curl -u`, these
    // clients do NOT send Basic credentials preemptively — without the
    // challenge they never authenticate and fail with "discovery failed" / 401.
    // Non-DAV routes return the standard AuthError which renders without this
    // header — keeping browser sessions redirecting to /login as before.
    if is_dav_path(request.uri().path()) {
        return Ok(dav_basic_auth_challenge("Authentication required"));
    }

    Err(AuthError::TokenNotProvided)
}

/// DAV protocol surfaces (WebDAV, CalDAV, CardDAV) authenticate over HTTP Basic.
/// Spec-compliant clients (Thunderbird, DAVx5, Apple Calendar/Contacts, file
/// managers) only send credentials after receiving a `401` carrying a
/// `WWW-Authenticate: Basic` challenge, so these paths must emit it. Browser and
/// JSON-API routes deliberately do not, so they keep redirecting to `/login`.
fn is_dav_path(path: &str) -> bool {
    path.starts_with("/webdav") || path.starts_with("/caldav") || path.starts_with("/carddav")
}

/// Build the `401 Unauthorized` Basic-auth challenge shared by every DAV
/// surface, so clients re-prompt for credentials instead of failing silently.
fn dav_basic_auth_challenge(message: &'static str) -> Response {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(header::WWW_AUTHENTICATE, r#"Basic realm="OxiCloud""#)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(axum::body::Body::from(message))
        .unwrap()
}

/// Middleware to verify that the authenticated user has an admin role.
///
/// Must be applied AFTER auth_middleware, as it depends on `CurrentUser`
/// being present in the request extensions. The role carried by
/// `CurrentUser` is the *live* role resolved by `auth_middleware` (see
/// [`resolve_live_role`]), not the JWT claim, so a demotion is honoured
/// here within the flags-cache TTL.
///
/// Denial shapes distinguish authn from authz:
///   - `CurrentUser` present, role != "admin" → 403 Forbidden.
///   - `CurrentUser` absent → 401 Unauthorized. Should not happen in
///     practice (auth_middleware guards against it), but the
///     defensive fallback returns the honest shape: "we don't know
///     who you are" is 401, not "we know you and refuse" (403).
pub async fn require_admin(request: Request, next: Next) -> Response {
    // Get the CurrentUser inserted by auth_middleware
    if let Some(current_user) = request.extensions().get::<Arc<CurrentUser>>() {
        if current_user.role == "admin" {
            tracing::debug!("Admin access granted for user: {}", current_user.username);
            return next.run(request).await;
        }
        tracing::info!(
            target: "audit",
            event = "authz.admin_denied",
            reason = "not_admin",
            caller_id = %current_user.id,
            role = %current_user.role,
            "👮🏻‍♂️ admin-only route denied for non-admin caller"
        );
        return AuthError::AccessDenied("Admin role required".to_string()).into_response();
    }

    tracing::info!(
        target: "audit",
        event = "authz.admin_denied",
        reason = "unauthenticated",
        "👮🏻‍♂️ admin-only route reached with no authenticated user"
    );
    AuthError::TokenNotProvided.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dav_paths_receive_basic_auth_challenge() {
        // Regression for #480: CalDAV/CardDAV clients (Thunderbird, DAVx5) only
        // send credentials after a 401 carrying WWW-Authenticate. All three DAV
        // surfaces must qualify so the challenge is emitted.
        for path in [
            "/webdav/",
            "/webdav/admin/file.txt",
            "/caldav/",
            "/caldav/admin/cal/",
            "/carddav/",
            "/carddav/principals/admin/",
        ] {
            assert!(is_dav_path(path), "{path} should be treated as a DAV path");
        }
    }

    #[test]
    fn non_dav_paths_do_not_receive_basic_auth_challenge() {
        for path in [
            "/",
            "/api/files",
            "/login",
            "/index.html",
            "/.well-known/caldav",
        ] {
            assert!(
                !is_dav_path(path),
                "{path} must not get a Basic-auth challenge (browser/API surface)"
            );
        }
    }

    #[test]
    fn challenge_sets_www_authenticate_header() {
        let resp = dav_basic_auth_challenge("Authentication required");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            resp.headers()
                .get(header::WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok()),
            Some(r#"Basic realm="OxiCloud""#),
        );
    }

    #[test]
    fn account_inactive_maps_to_401() {
        // A token that is still cryptographically valid but whose account was
        // deactivated/deleted must be rejected with 401 (credentials no longer
        // valid), so browsers redirect to /login rather than seeing a 403.
        let resp = AuthError::AccountInactive.into_response();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
