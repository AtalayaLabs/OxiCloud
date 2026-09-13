use axum::{
    extract::{FromRequestParts, Request, State},
    http::{StatusCode, header, request::Parts},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::Arc;
use uuid::Uuid;

use crate::common::di::AppState;

// Re-export CurrentUser from application layer for use in handlers
pub use crate::application::dtos::user_dto::CurrentUser;
use crate::application::ports::auth_ports::TokenServicePort;
use crate::domain::entities::user::UserRole;
use crate::domain::services::authorization::Subject;
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

/// Record the principal on the request span — `share_id` for a public-share
/// session, `user_id` otherwise. **Never both.**
///
/// An anonymous session's `sub` is a fresh per-visit uuid matching no
/// `auth.users` row. Recording it as `user_id` would hand operators an id
/// that looks lookupable and is not, and would make share traffic
/// indistinguishable from user traffic in every log query. A line carrying
/// `share_id` says what the caller actually is.
fn record_principal_on_span(user_id: Uuid, share_id: Option<Uuid>) {
    let span = tracing::Span::current();
    match share_id {
        Some(sid) => span.record("share_id", tracing::field::display(sid)),
        None => span.record("user_id", tracing::field::display(user_id)),
    };
}

/// Reject a token whose `role` and `share_id` claims disagree, before any
/// `CurrentUser` is built from them.
///
/// `CallerSubject` refuses the same shapes at *use*; this refuses them at
/// *construction*, so an incoherent principal never enters a request at all.
/// That matters because `CurrentUser.id` is read at ~200 sites which are
/// safe today only because `AuthUser` rejects anonymous first — a second
/// control doing the work, rather than the invariant holding on its own.
///
/// The dangerous shape is `anonymous` with no `share_id`: anything falling
/// back to `Subject::User(cu.id)` would match a *session* id against user
/// grants — a valid UUID belonging to no user, so the query runs and
/// silently returns nothing.
fn validate_principal_shape(role: &str, share_id: Option<Uuid>) -> Result<(), AuthError> {
    let anonymous = UserRole::from_session(role).is_some_and(|r| r.is_anonymous());
    match (anonymous, share_id) {
        (true, Some(_)) | (false, None) => Ok(()),
        (true, None) | (false, Some(_)) => {
            tracing::error!(
                target: "audit",
                event = "auth.rejected",
                reason = "incoherent_principal",
                role = %role,
                has_share = share_id.is_some(),
                "👮🏻‍♂️ token role and share_id disagree — refusing to build a principal",
            );
            Err(AuthError::InvalidToken(
                "Malformed token principal".to_string(),
            ))
        }
    }
}

/// Assert that `cu` meets a minimum role, or deny.
///
/// The single place a role requirement is expressed. Returns a `Result`
/// rather than a bool on purpose: with `?` the outcome cannot be ignored,
/// which an `is_anonymous()` bool invites.
///
/// **This is not the only enforcement point.** Four paths authenticate
/// without ever reaching an extractor — `middleware/admin.rs`'s
/// `require_authenticated`, the three DAV handlers' hand-rolled
/// `extract_user`, `POST /api/auth/refresh` (mounted outside
/// `auth_middleware`), and `GET /api/rt/ws` (self-auths from a raw Bearer).
/// Each must call this too; see `src/AGENTS.md` § AuthZ enforcement points.
pub fn require_role(cu: &CurrentUser, min: UserRole) -> Result<(), AuthError> {
    if cu.role_enum().at_least(min) {
        return Ok(());
    }
    tracing::info!(
        target: "audit",
        event = "authz.denied",
        reason = "insufficient_role",
        caller_id = %cu.id,
        role = %cu.role,
        required = min.as_str(),
        "👮🏻‍♂️ principal role is below the minimum this endpoint requires",
    );
    Err(AuthError::AccessDenied(format!(
        "This endpoint requires role `{}`",
        min.as_str()
    )))
}

/// A caller expressed as an authorization [`Subject`] — the only extractor
/// that accepts a public-share visitor.
///
/// `AuthUser` means "a real user" and refuses `anonymous`. A route that
/// should be reachable through a share link opts in by taking this instead.
/// The swap is one line, visible in the signature, and reviewable per route
/// — which is the point: widening a route is a decision, not a default.
///
/// This is the single place a *session* concept (`role`) is translated into
/// an *authorization* concept (`Subject`). Handlers never see the role and
/// never branch on it; services take the `Subject` and the engine matches
/// grants against it. Keeping the two vocabularies separated here is what
/// lets the engine stay ignorant of HTTP sessions entirely.
pub struct CallerSubject(pub Subject);

impl<S> FromRequestParts<S> for CallerSubject
where
    S: Send + Sync,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let cu = parts
            .extensions
            .get::<Arc<CurrentUser>>()
            .ok_or(AuthError::UserNotFound)?;

        match (cu.is_anonymous(), cu.share_id) {
            // A share visitor authorises as the share's token grant — the
            // row `share_service` already writes on every share creation.
            (true, Some(share_id)) => Ok(CallerSubject(Subject::Token(share_id))),
            (false, None) => Ok(CallerSubject(Subject::User(cu.id))),

            // The two fields disagree. Refuse rather than pick one: an
            // anonymous principal with no share would fall back to
            // `Subject::User(cu.id)` — and `cu.id` for an anonymous session
            // is a SESSION id, which would be matched against user grants.
            // A non-anonymous principal carrying a share id is equally
            // incoherent. Neither should be reachable; both are denied
            // loudly if they ever are.
            (true, None) | (false, Some(_)) => {
                tracing::error!(
                    target: "audit",
                    event = "authz.denied",
                    reason = "incoherent_principal",
                    caller_id = %cu.id,
                    role = %cu.role,
                    has_share = cu.share_id.is_some(),
                    "👮🏻‍♂️ principal role and share_id disagree — refusing to guess",
                );
                Err(AuthError::AccessDenied(
                    "Malformed session principal".to_string(),
                ))
            }
        }
    }
}

/// Build an [`AuthUser`] from request extensions, for handlers that receive a
/// raw `Request` and therefore cannot use `FromRequestParts`.
///
/// The three DAV surfaces (`/webdav`, `/caldav`, `/carddav`) each hand-rolled
/// this, which meant the entire DAV surface was invisible to any rule added to
/// the `AuthUser` extractor — three copies, three chances to forget. One
/// implementation instead, so the guard cannot drift between them.
pub fn auth_user_from_extensions(
    ext: &axum::http::Extensions,
) -> Result<AuthUser, crate::interfaces::errors::AppError> {
    use crate::interfaces::errors::AppError;
    let cu = ext
        .get::<Arc<CurrentUser>>()
        .cloned()
        .ok_or_else(|| AppError::unauthorized("Authentication required"))?;
    require_role(&cu, UserRole::User)
        .map_err(|_| AppError::forbidden("This surface requires a user account"))?;
    Ok(AuthUser(cu))
}

// Implement FromRequestParts for AuthUser — allows using `auth_user: AuthUser` in handlers.
// Cost: 1 atomic increment (~1 ns) instead of 3 String clones (~100 ns + 3 mallocs).
//
// `AuthUser` means "a real user principal". It REJECTS `role = anonymous`,
// which is what keeps the ~200 handlers taking it fail-closed against
// public-share sessions with no edit to any of them. A route that should be
// reachable by a share visitor opts in explicitly with a different extractor
// rather than this one relaxing.
impl<S> FromRequestParts<S> for AuthUser
where
    S: Send + Sync,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let cu = parts
            .extensions
            .get::<Arc<CurrentUser>>()
            .cloned()
            .ok_or(AuthError::UserNotFound)?;
        require_role(&cu, UserRole::User)?;
        Ok(AuthUser(cu))
    }
}

// Implement FromRequestParts for CurrentUserId — lightweight extractor for user_id only
impl<S> FromRequestParts<S> for CurrentUserId
where
    S: Send + Sync,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let cu = parts
            .extensions
            .get::<Arc<CurrentUser>>()
            .ok_or(AuthError::UserNotFound)?;
        // Same contract as `AuthUser` — this yields a `user_id`, and an
        // anonymous principal has no `auth.users` row for that id to mean.
        // It also closes the message-bus door for free: `POST /api/rt/ticket`
        // takes this extractor, so no ticket is minted and the WS upgrade has
        // no credential to present.
        require_role(cu, UserRole::User)?;
        Ok(CurrentUserId(cu.id))
    }
}

// `OptionalUserId` used to live here — an infallible extractor yielding
// `Option<Uuid>`. It was deleted rather than taught about anonymous roles:
// it had ZERO call sites in `src/` and `tests/`, so "it returns None for
// anonymous" would have been a protection that guarded nothing. An
// unused permissive extractor is a trap for the next person who reaches
// for it; if an optional principal is ever genuinely needed, reintroduce
// it deliberately with a role decision baked in.

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
                            // Refuse an incoherent token before a principal
                            // exists to be misread downstream.
                            validate_principal_shape(&role, claims.share_id)?;
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
                                share_id: claims.share_id,
                            });
                            request.extensions_mut().insert(current_user);
                            record_principal_on_span(user_id, claims.share_id);
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
                                // Basic auth is app-password only.
                                share_id: None,
                            });
                            request.extensions_mut().insert(current_user);
                            // Basic auth is app-password only — never a share.
                            record_principal_on_span(user_id, None);
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
                                    // The cookie arm — the one a share
                                    // visitor actually arrives through.
                                    share_id: claims.share_id,
                                });
                                request.extensions_mut().insert(current_user);
                                request.extensions_mut().insert(CookieAuthenticated);
                                record_principal_on_span(user_id, claims.share_id);
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
    use smol_str::SmolStr;

    fn principal(role: &str) -> CurrentUser {
        CurrentUser {
            id: Uuid::new_v4(),
            username: "visitor".into(),
            email: "".into(),
            role: SmolStr::new(role),
            dpop_jkt: None,
            share_id: None,
        }
    }

    /// Construction-time refusal — the incoherent token never becomes a
    /// principal at all, so nothing downstream can misread it.
    #[test]
    fn incoherent_token_shapes_are_refused_before_a_principal_exists() {
        let share = Uuid::new_v4();

        // The coherent pair.
        assert!(validate_principal_shape("anonymous", Some(share)).is_ok());
        assert!(validate_principal_shape("user", None).is_ok());
        assert!(validate_principal_shape("admin", None).is_ok());

        // The dangerous one: anything falling back to `Subject::User(cu.id)`
        // here would match a SESSION id against user grants.
        assert!(validate_principal_shape("anonymous", None).is_err());
        // And its mirror — a real role has no business carrying a share.
        assert!(validate_principal_shape("user", Some(share)).is_err());
        assert!(validate_principal_shape("admin", Some(share)).is_err());

        // An unparseable role is not anonymous, so it must not carry a share.
        assert!(validate_principal_shape("bogus", None).is_ok());
        assert!(validate_principal_shape("bogus", Some(share)).is_err());
    }

    /// `role` and `share_id` must agree. The pairing is what stops a
    /// half-built principal from authorising as the wrong subject.
    #[test]
    fn role_and_share_id_must_agree() {
        let anon_no_share = principal("anonymous");
        assert!(anon_no_share.is_anonymous() && anon_no_share.share_id.is_none());

        let mut user_with_share = principal("user");
        user_with_share.share_id = Some(Uuid::new_v4());
        assert!(!user_with_share.is_anonymous() && user_with_share.share_id.is_some());

        // Both shapes above are incoherent and `CallerSubject` refuses them.
        // The dangerous one is the first: falling back to
        // `Subject::User(cu.id)` there would match a SESSION id against user
        // grants — an id that belongs to no user but is a valid UUID, so the
        // query would run and quietly return nothing (or, worse, something).
    }

    /// The property the whole public-share design rests on: `AuthUser` —
    /// the extractor ~200 handlers take — refuses an anonymous principal.
    /// If this ever passes for `anonymous`, every one of those handlers is
    /// reachable by a share-link visitor.
    #[test]
    fn anonymous_is_refused_a_user_role() {
        let err = require_role(&principal("anonymous"), UserRole::User)
            .expect_err("anonymous must not satisfy a user requirement");
        assert!(matches!(err, AuthError::AccessDenied(_)));

        require_role(&principal("user"), UserRole::User).expect("a user satisfies user");
        require_role(&principal("admin"), UserRole::User).expect("an admin satisfies user");
    }

    #[test]
    fn only_admin_satisfies_admin() {
        require_role(&principal("admin"), UserRole::Admin).expect("admin satisfies admin");
        assert!(require_role(&principal("user"), UserRole::Admin).is_err());
        assert!(require_role(&principal("anonymous"), UserRole::Admin).is_err());
    }

    /// An unrecognised role resolves to the LEAST privileged answer, not the
    /// most. Every other parse site in the tree uses `_ => UserRole::User`,
    /// which is fail-open: a corrupt or future value silently becomes a real
    /// user. Here a garbage role can reach nothing.
    #[test]
    fn unknown_roles_fail_closed_not_open() {
        for role in ["", "superuser", "Admin", "ANONYMOUS", "root", "user "] {
            let cu = principal(role);
            assert!(
                cu.is_anonymous(),
                "unrecognised role {role:?} must degrade to anonymous, not user",
            );
            assert!(require_role(&cu, UserRole::User).is_err());
        }
    }

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
